//! Finding and reading records, the same for the writer and for a reader:
//! only the [`View`] bytes come through, and where class entries come from,
//! differ.

use super::{
    BUCKET_HEADER, BUCKET_PAGE, ClassEntry, ITEM_HEADER, ITEM_SPACE, Item, NO_ITEM, PAGE_BYTES,
    PAGE_LINK, SLOT_LEN, Slot, State, checked_ref, class_entry_at, class_size, corrupt,
    page_header, radix, ref_class, ref_index, sip,
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

/// Where a key's record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Located {
    pub slot: Slot,
    /// The bucket page holding the slot, and its index there.
    pub page: u64,
    pub index: usize,
    /// The head item's header.
    pub head: Item,
}

impl Located {
    /// The payload's length.
    pub(crate) const fn payload_len(&self) -> u64 {
        self.head.total_or_prev - self.head.key_len as u64
    }
}

/// The low 32 bits of a key's seeded hash: its route and fingerprint.
pub(crate) fn route(seed: [u64; 2], block_id: u32, key: &[u8]) -> u32 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the low 32 bits route and fingerprint"
    )]
    let route = sip::hash(seed, &[&block_id.to_le_bytes(), key]) as u32;
    route
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

/// Finds `key` of `block_id` in the table `state`.
pub(crate) fn find(
    ctx: &mut Ctx<'_>,
    seed: [u64; 2],
    state: &State,
    block_id: u32,
    key: &[u8],
) -> Result<Option<Located>> {
    let route = route(seed, block_id, key);
    let mut at = bucket(ctx, state, state.bucket_of(route))?;
    let mut page = [0u8; crate::width::small(BUCKET_PAGE)];
    while at != 0 {
        ctx.read(at, &mut page)?;
        let (count, next) = page_header(&page)?;
        for index in 0..count {
            let slot = slot_at(&page, index);
            if slot.route != route {
                continue;
            }
            // The header and the key in one read when the key fits.
            let size = class_size(checked_ref(slot.item).map(ref_class)?);
            let header_len = crate::width::small(ITEM_HEADER);
            let mut prefix = [0u8; STEP];
            let want = crate::width::bounded(size, STEP).min(header_len + key.len());
            ctx.read_item(slot.item, 0, &mut prefix[..want])?;
            let head = checked_item(slot.item, &prefix[..want])?;
            let matches = head.head
                && head.block_or_route == block_id
                && head.key_len as usize == key.len()
                && head.total_or_prev >= key.len() as u64
                && if want == header_len + key.len() && head.len as usize >= key.len() {
                    prefix[header_len..want] == *key
                } else {
                    key_equals(ctx, slot.item, key)?
                };
            if matches {
                return Ok(Some(Located {
                    slot,
                    page: at,
                    index,
                    head,
                }));
            }
        }
        at = next;
    }
    Ok(None)
}

/// Whether the record headed by `head` starts with `key`.
fn key_equals(ctx: &mut Ctx<'_>, head: u64, key: &[u8]) -> Result<bool> {
    let mut matched = 0usize;
    let mut equal = true;
    let mut item = head;
    while matched < key.len() {
        let header = ctx.item(item)?;
        let len = crate::width::index(u64::from(header.len))?;
        let n = len.min(key.len() - matched);
        let mut step = [0u8; STEP];
        let mut done = 0usize;
        while done < n {
            let m = (n - done).min(STEP);
            ctx.read_item(item, ITEM_HEADER + done as u64, &mut step[..m])?;
            if step[..m] != key[matched + done..matched + done + m] {
                equal = false;
                break;
            }
            done += m;
        }
        if !equal {
            return Ok(false);
        }
        matched += n;
        if matched < key.len() {
            if header.next == NO_ITEM {
                return Ok(false);
            }
            item = header.next;
        }
    }
    Ok(true)
}

/// Reads a located record's payload into `out`, which must be its length,
/// checking every item.
pub(crate) fn read_payload(ctx: &mut Ctx<'_>, found: &Located, out: &mut [u8]) -> Result<()> {
    let payload_len = found.payload_len();
    if out.len() as u64 != payload_len {
        return Err(Error::BufferTooSmall {
            resource: "keyed record payload",
            required: crate::width::index(payload_len)?,
            provided: out.len(),
        });
    }
    let mut skip = u64::from(found.head.key_len);
    let mut written = 0usize;
    stream(ctx, found.slot.item, &mut |bytes| {
        let mut bytes = bytes;
        if skip > 0 {
            let n = crate::width::bounded(skip, bytes.len());
            skip -= n as u64;
            bytes = &bytes[n..];
        }
        let end = written + bytes.len();
        let target = out
            .get_mut(written..end)
            .ok_or(corrupt("keyed record length"))?;
        target.copy_from_slice(bytes);
        written = end;
        Ok(true)
    })?;
    if written != out.len() {
        return Err(corrupt("keyed record length"));
    }
    Ok(())
}

/// The first page of bucket `bucket`.
pub(crate) fn bucket(ctx: &mut Ctx<'_>, state: &State, bucket: u64) -> Result<u64> {
    if !radix::reaches(state.dir.1, bucket) {
        return Err(corrupt("keyed bucket directory"));
    }
    let page = radix::get(ctx.view, ctx.source, state.dir, bucket)?;
    if page == 0 {
        return Err(corrupt("keyed bucket directory"));
    }
    Ok(page)
}

pub(crate) fn slot_at(page: &[u8], index: usize) -> Slot {
    let start = crate::width::small(BUCKET_HEADER) + index * crate::width::small(SLOT_LEN);
    Slot::decode(&page[start..start + crate::width::small(SLOT_LEN)])
}
