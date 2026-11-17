//! Reading items and record chains, the same for the writer and for a
//! reader: only the [`View`] bytes come through, and where class entries
//! come from, differ.

use super::{
    ClassEntry, ITEM_HEADER, ITEM_SPACE, Item, NO_ITEM, PAGE_BYTES, PAGE_LINK, checked_ref,
    class_entry_at, class_size, corrupt, radix, ref_class, ref_index,
};
use crate::matrix_tiles::store::TileSource;
use crate::matrix_tiles::view::RingReader;
use crate::{Error, Result};

/// Bytes read per step when a key is compared or a record is checked.
pub(crate) const STEP: usize = 4096;

/// Reads the file as one snapshot shows it.
pub(crate) trait View {
    fn read(&mut self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()>;
}

/// A reader's view: its snapshot, checked on every read against the ring
/// moving past it.
impl View for RingReader {
    fn read(&mut self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        Self::read(self, source, at, buf)
    }
}

/// What a lookup reads through: a view, the file, the metadata record, and
/// the class entries when the caller holds them (the writer does; a reader
/// reads them through its view).
pub(crate) struct Ctx<'a> {
    pub view: &'a mut dyn View,
    pub source: &'a dyn TileSource,
    pub meta: u64,
    pub classes: Option<&'a [ClassEntry]>,
    /// The last page a class's directory resolved: `(class, page, offset)`.
    /// One lookup reads the same item, and so the same page, several
    /// times; within it the snapshot does not change.
    pub memo: Option<(usize, u64, u64)>,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(
        view: &'a mut dyn View,
        source: &'a dyn TileSource,
        meta: u64,
        classes: Option<&'a [ClassEntry]>,
    ) -> Self {
        Self {
            view,
            source,
            meta,
            classes,
            memo: None,
        }
    }

    fn page(&mut self, class: usize, entry: &ClassEntry, page: u64) -> Result<u64> {
        if let Some((memo_class, memo_page, at)) = self.memo
            && memo_class == class
            && memo_page == page
        {
            return Ok(at);
        }
        let at = radix::get(self.view, self.source, entry.dir, page)?;
        if at == 0 {
            return Err(corrupt("keyed item page"));
        }
        self.memo = Some((class, page, at));
        Ok(at)
    }

    pub(crate) fn read(&mut self, at: u64, buf: &mut [u8]) -> Result<()> {
        self.view.read(self.source, at, buf)
    }

    pub(crate) fn class(&mut self, class: usize) -> Result<ClassEntry> {
        if let Some(classes) = self.classes {
            return classes
                .get(class)
                .copied()
                .ok_or(corrupt("keyed item reference"));
        }
        let mut raw = [0u8; crate::width::small(super::CLASS_ENTRY_LEN)];
        let at = class_entry_at(self.meta, class);
        self.read(at, &mut raw)?;
        ClassEntry::decode(&raw)
    }

    /// Reads `buf.len()` bytes of item `item` from byte `from` of it. An
    /// item may continue on the class's next page, past its link.
    pub(crate) fn read_item(&mut self, item: u64, from: u64, buf: &mut [u8]) -> Result<()> {
        let item = checked_ref(item)?;
        let class = ref_class(item);
        let entry = self.class(class)?;
        let size = class_size(class);
        if ref_index(item) >= entry.end || from + buf.len() as u64 > size {
            return Err(corrupt("keyed item reference"));
        }
        let mut at = ref_index(item) * size + from;
        let mut done = 0usize;
        while done < buf.len() {
            let page = self.page(class, &entry, at / ITEM_SPACE)?;
            let within = PAGE_LINK + at % ITEM_SPACE;
            let n = crate::width::bounded(PAGE_BYTES - within, buf.len() - done);
            self.read(page + within, &mut buf[done..done + n])?;
            done += n;
            at += n as u64;
        }
        Ok(())
    }

    pub(crate) fn item(&mut self, item: u64) -> Result<Item> {
        let mut header = [0u8; crate::width::small(ITEM_HEADER)];
        self.read_item(item, 0, &mut header)?;
        checked_item(item, &header)
    }
}

/// Decodes and checks the header of item `item`.
fn checked_item(item: u64, header: &[u8]) -> Result<Item> {
    let item_header = Item::decode(
        header[..crate::width::small(ITEM_HEADER)]
            .try_into()
            .expect("header"),
    );
    {
        if u64::from(item_header.len) + ITEM_HEADER > class_size(ref_class(item))
            || (item_header.next != NO_ITEM && checked_ref(item_header.next).is_err())
        {
            return Err(corrupt("keyed item header"));
        }
        Ok(item_header)
    }
}

/// Calls `visit(bytes)` with a record's bytes in order, from byte `from`,
/// item by item through `buf`. Stops early when `visit` returns `false`.
/// Checks every item's checksum when `from` is 0 and the whole record is
/// visited.
pub(crate) fn stream(
    ctx: &mut Ctx<'_>,
    head: u64,
    visit: &mut dyn FnMut(&[u8]) -> Result<bool>,
) -> Result<()> {
    let mut step = [0u8; STEP];
    let mut item = head;
    let mut first = true;
    let mut total = 0u64;
    let mut expected = 0u64;
    // A chain is no longer than its record allows: bounded by the record.
    loop {
        // The header and the first bytes in one read.
        let size = class_size(checked_ref(item).map(ref_class)?);
        let head_read = crate::width::bounded(size, STEP);
        ctx.read_item(item, 0, &mut step[..head_read])?;
        let header = checked_item(item, &step[..head_read])?;
        if first {
            if !header.head {
                return Err(corrupt("keyed record head"));
            }
            expected = header.total_or_prev;
            first = false;
        } else if header.head {
            return Err(corrupt("keyed record chain"));
        }
        let mut crc = crc32fast::Hasher::new();
        crc.update(&header.crc_prefix());
        let len = u64::from(header.len);
        let header_len = crate::width::small(ITEM_HEADER);
        let first_part = crate::width::bounded(len, head_read - header_len);
        let mut stop = false;
        {
            let bytes = &step[header_len..header_len + first_part];
            crc.update(bytes);
            if !visit(bytes)? {
                stop = true;
            }
        }
        let mut done = first_part as u64;
        while done < len {
            let n = crate::width::bounded(len - done, STEP);
            ctx.read_item(item, ITEM_HEADER + done, &mut step[..n])?;
            crc.update(&step[..n]);
            if !stop && !visit(&step[..n])? {
                stop = true;
            }
            done += n as u64;
        }
        if crc.finalize() != header.crc {
            return Err(Error::ChecksumMismatch { offset: item });
        }
        total += len;
        if stop || header.next == NO_ITEM {
            break;
        }
        if total >= expected {
            return Err(corrupt("keyed record chain"));
        }
        item = header.next;
    }
    Ok(())
}

