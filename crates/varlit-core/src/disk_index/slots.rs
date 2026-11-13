//! Direct finite-domain tables. Array position is the enum code; no stored keys
//! or comparison tree. Extents are published only at a confirmation.
//!
//! An append-format table (`VIXSLOT1`) is written at the end of the file and
//! never rewritten. A reuse-format table (`VIXSLOT2`) is written into pages a
//! [`tree::PageAlloc`] hands out: every chunk carries its block, position and
//! birth generation, the directory names each chunk's birth, and the root names
//! the directory's, so a reader holding an old root that reaches a rewritten
//! page sees [`DiskIndexError::PageReclaimed`] rather than another chunk's rows.
use super::*;
use crate::fallible::Arc;
use std::{cell::RefCell, collections::VecDeque, fs::File};
use tree::{PAGE, PageAlloc, PageFormat, read_at, write_at};
const MAGIC: &[u8; 8] = b"VIXSLOT1";
const MAGIC_REUSE: &[u8; 8] = b"VIXSLOT2";
/// A reuse-format chunk's trailer before its CRC: block id, chunk position
/// and birth.
const CHUNK_TRAILER: usize = 16;
const WIDTH: usize = 64;
// Offset, sequence, extent and verification flags/CRC. Block IDs come from the
// schema directory, enum codes from array positions; neither is stored per row.
const ROW: usize = 40;
fn oom(_: std::collections::TryReserveError) -> DiskIndexError {
    std::io::Error::from(std::io::ErrorKind::OutOfMemory).into()
}
fn buffer(len: usize) -> DiskIndexResult<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(oom)?;
    bytes.resize(len, 0);
    Ok(bytes)
}
fn pack(value: Option<[u8; LATEST_LEN]>) -> [u8; ROW] {
    let mut row = [0; ROW];
    if let Some(v) = value {
        row[..6].copy_from_slice(&v[..6]);
        row[6..30].copy_from_slice(&v[8..32]);
        row[30..34].copy_from_slice(&v[40..44]);
        row[34..38].copy_from_slice(&v[48..52]);
    }
    row
}
fn unpack(row: &[u8], block: u32) -> DiskIndexResult<Option<[u8; LATEST_LEN]>> {
    if row == [0; ROW] {
        return Ok(None);
    }
    if row[38..] != [0; 2] {
        return Err(bad());
    }
    let mut value = [0; LATEST_LEN];
    value[..6].copy_from_slice(&row[..6]);
    value[8..32].copy_from_slice(&row[6..30]);
    value[32..36].copy_from_slice(&block.to_le_bytes());
    if value[1] & LATEST_HAS_PHYSICAL != 0 {
        let id = if value[0] == 1 {
            TOMBSTONE_BLOCK_ID
        } else {
            block
        };
        value[36..40].copy_from_slice(&id.to_le_bytes());
    }
    value[40..44].copy_from_slice(&row[30..34]);
    value[48..52].copy_from_slice(&row[34..38]);
    decode_latest(&value)?;
    Ok(Some(value))
}
/// Writes a table extent: into a page from `alloc` when it fits one, else at
/// the end of the file. A reuse-format extent appended at the end is padded
/// to a whole page, so the page it occupies can be reused later.
fn place(file: &File, bytes: &[u8], alloc: Option<&mut PageAlloc>) -> DiskIndexResult<u64> {
    let Some(alloc) = alloc else {
        return append(file, bytes);
    };
    if bytes.len() > PAGE {
        return append(file, bytes);
    }
    if let Some(offset) = alloc.take() {
        crate::scalable_fault_point("checkpoint.record_write");
        write_at(file, offset, bytes)?;
        crate::scalable_fault_point("checkpoint.record_write");
        return Ok(offset);
    }
    let mut page = [0u8; PAGE];
    page[..bytes.len()].copy_from_slice(bytes);
    append(file, &page)
}
fn directory_fits_page(directory: &Directory) -> bool {
    let fixed = header_len(directory.format);
    let entry = chunk_entry(directory.format);
    directory
        .blocks
        .iter()
        .try_fold(fixed + 4, |n, b| n.checked_add(16 + b.chunks.len() * entry))
        .is_some_and(|len| len <= PAGE)
}
fn append(file: &File, bytes: &[u8]) -> DiskIndexResult<u64> {
    let offset = file.metadata()?.len();
    #[cfg(feature = "scalable-fault-injection")]
    if crate::scalable_fault::is_armed() {
        crate::scalable_fault_point("checkpoint.record_prefix");
        write_at(file, offset, &bytes[..bytes.len() / 2])?;
        crate::scalable_fault_point("checkpoint.record_prefix");
    }
    crate::scalable_fault_point("checkpoint.record_write");
    write_at(file, offset, bytes)?;
    crate::scalable_fault_point("checkpoint.record_write");
    Ok(offset)
}
fn bad() -> DiskIndexError {
    DiskIndexError::CheckpointMismatch("invalid finite slot table")
}
#[derive(Clone, Copy, Default)]
pub(super) struct Root {
    pub offset: u64,
    pub count: u32,
    /// The directory's birth; 0 for an append-format table.
    pub birth: u64,
}
#[derive(Clone)]
struct Block {
    id: u32,
    len: usize,
    used: u32,
    chunks: Vec<u64>,
    /// Each chunk's birth; all 0 in an append-format table.
    births: Vec<u64>,
}
#[derive(Clone)]
struct Directory {
    format: PageFormat,
    blocks: Vec<Block>,
}
fn header_len(format: PageFormat) -> usize {
    match format {
        PageFormat::Append => 24,
        PageFormat::Reuse => 32,
    }
}
fn chunk_entry(format: PageFormat) -> usize {
    match format {
        PageFormat::Append => 8,
        PageFormat::Reuse => 16,
    }
}
fn chunk_len(format: PageFormat, rows: usize) -> usize {
    rows * ROW
        + 4
        + match format {
            PageFormat::Append => 0,
            PageFormat::Reuse => CHUNK_TRAILER,
        }
}
fn reclaimed(format: PageFormat) -> DiskIndexError {
    match format {
        PageFormat::Append => bad(),
        PageFormat::Reuse => DiskIndexError::PageReclaimed,
    }
}
impl Directory {
    fn empty(format: PageFormat) -> Self {
        Self {
            format,
            blocks: Vec::new(),
        }
    }
    fn read(
        file: &File,
        end: u64,
        root: Root,
        budget: usize,
        format: PageFormat,
    ) -> DiskIndexResult<Self> {
        if root.offset == 0 {
            return if root.count == 0 {
                Ok(Self::empty(format))
            } else {
                Err(bad())
            };
        }
        let fixed = header_len(format);
        let entry = chunk_entry(format);
        let mut head = [0; 32];
        if root.offset < 24576
            || root
                .offset
                .checked_add(fixed as u64 + 4)
                .is_none_or(|n| n > end)
        {
            return Err(bad());
        }
        read_at(file, root.offset, &mut head[..fixed])?;
        let len = read_u32(&head, 8) as usize;
        let blocks = read_u32(&head, 12) as usize;
        let magic = match format {
            PageFormat::Append => MAGIC,
            PageFormat::Reuse => MAGIC_REUSE,
        };
        if &head[..8] != magic || (format == PageFormat::Reuse && read_u64(&head, 24) != root.birth)
        {
            return Err(reclaimed(format));
        }
        if read_u64(&head, 16) != u64::from(root.count)
            || len < fixed + 4
            || len > budget / 2
            || blocks > (len - fixed - 4) / (16 + entry)
            || root.offset.checked_add(len as u64).is_none_or(|n| n > end)
        {
            return Err(reclaimed(format));
        }
        let mut bytes = buffer(len)?;
        read_at(file, root.offset, &mut bytes)?;
        if crc32fast::hash(&bytes[..len - 4]) != read_u32(&bytes, len - 4) {
            return Err(reclaimed(format));
        }
        let mut result = Self::empty(format);
        result.blocks.try_reserve_exact(blocks).map_err(oom)?;
        let mut pos = fixed;
        let mut count = 0u64;
        for _ in 0..blocks {
            if pos + 16 > len - 4 {
                return Err(bad());
            }
            let id = read_u32(&bytes, pos);
            let slots = read_u32(&bytes, pos + 4) as usize;
            let used = read_u32(&bytes, pos + 8);
            let chunks = read_u32(&bytes, pos + 12) as usize;
            pos += 16;
            if slots == 0
                || slots > 4096
                || used as usize > slots
                || chunks != slots.div_ceil(WIDTH)
                || result.blocks.last().is_some_and(|b| b.id >= id)
                || pos + chunks * entry > len - 4
            {
                return Err(bad());
            }
            let mut offsets = Vec::new();
            offsets.try_reserve_exact(chunks).map_err(oom)?;
            let mut births = Vec::new();
            births.try_reserve_exact(chunks).map_err(oom)?;
            // An append-format chunk precedes the directory written after it;
            // a reuse-format chunk may sit anywhere in the file.
            let bound = match format {
                PageFormat::Append => root.offset,
                PageFormat::Reuse => end,
            };
            for i in 0..chunks {
                let offset = read_u64(&bytes, pos);
                let birth = match format {
                    PageFormat::Append => 0,
                    PageFormat::Reuse => read_u64(&bytes, pos + 8),
                };
                pos += entry;
                let bytes = chunk_len(format, (slots - i * WIDTH).min(WIDTH)) as u64;
                if offset != 0
                    && (offset < 24576
                        || offset.checked_add(bytes).is_none_or(|n| n > bound)
                        || (format == PageFormat::Reuse && (birth == 0 || birth > root.birth)))
                {
                    return Err(bad());
                }
                offsets.push(offset);
                births.push(birth);
            }
            count += u64::from(used);
            result.blocks.push(Block {
                id,
                len: slots,
                used,
                chunks: offsets,
                births,
            });
        }
        if pos != len - 4 || count != u64::from(root.count) {
            return Err(bad());
        }
        Ok(result)
    }
    fn try_clone(&self) -> DiskIndexResult<Self> {
        let mut blocks = Vec::new();
        blocks.try_reserve_exact(self.blocks.len()).map_err(oom)?;
        for b in &self.blocks {
            let mut chunks = Vec::new();
            chunks.try_reserve_exact(b.chunks.len()).map_err(oom)?;
            chunks.extend_from_slice(&b.chunks);
            let mut births = Vec::new();
            births.try_reserve_exact(b.births.len()).map_err(oom)?;
            births.extend_from_slice(&b.births);
            blocks.push(Block {
                id: b.id,
                len: b.len,
                used: b.used,
                chunks,
                births,
            });
        }
        Ok(Self {
            format: self.format,
            blocks,
        })
    }
    fn find(&self, key: &[u8]) -> DiskIndexResult<Option<(usize, usize)>> {
        if key.len() < 5 || key[0] != 1 {
            return Err(bad());
        }
        let id = u32::from_be_bytes(key[1..5].try_into().unwrap());
        let Ok(block) = self.blocks.binary_search_by_key(&id, |b| b.id) else {
            return Ok(None);
        };
        if key.len() != 7 {
            return Err(bad());
        }
        let code = u16::from_le_bytes(key[5..7].try_into().unwrap()) as usize;
        if code >= self.blocks[block].len {
            return Err(bad());
        }
        Ok(Some((block, code)))
    }
}
#[derive(Default)]
struct Cache {
    bytes: usize,
    values: BTreeMap<u64, Vec<u8>>,
    order: VecDeque<u64>,
}
pub(super) struct Reader {
    file: Arc<File>,
    end: u64,
    root: Root,
    budget: usize,
    format: PageFormat,
    directory: RefCell<Option<Directory>>,
    cache: RefCell<Cache>,
}
impl Reader {
    pub(super) fn new(
        file: Arc<File>,
        end: u64,
        root: Root,
        budget: usize,
        format: PageFormat,
    ) -> Self {
        Self {
            file,
            end,
            root,
            budget,
            format,
            directory: RefCell::new(None),
            cache: RefCell::new(Cache::default()),
        }
    }
    fn ensure_directory(&self) -> DiskIndexResult<()> {
        if self.directory.borrow().is_none() {
            *self.directory.borrow_mut() = Some(Directory::read(
                &self.file,
                self.end,
                self.root,
                self.budget,
                self.format,
            )?);
        }
        Ok(())
    }
    pub(super) fn validate_layout(&self, plan: DiskIndexPlan) -> DiskIndexResult<()> {
        // An unpublished/empty table has no rows capable of shadowing a tree.
        if self.root.offset == 0 {
            return Ok(());
        }
        self.ensure_directory()?;
        let directory = self.directory.borrow();
        if directory
            .as_ref()
            .unwrap()
            .blocks
            .iter()
            .map(|b| (b.id, b.len as u32))
            .ne(plan
                .descriptors()
                .iter()
                .filter(|d| d.finite_key_count != 0)
                .map(|d| (d.block_id, d.finite_key_count)))
        {
            return Err(bad());
        }
        Ok(())
    }
    fn directory(&self) -> DiskIndexResult<Directory> {
        self.ensure_directory()?;
        self.directory.borrow().as_ref().unwrap().try_clone()
    }
    pub fn get(&self, key: &[u8]) -> DiskIndexResult<Option<Option<[u8; LATEST_LEN]>>> {
        if self.root.offset == 0 {
            return Ok(None);
        }
        // Metadata is small and fixed by the schema. Slot access itself is direct.
        if self.directory.borrow().is_none() {
            self.ensure_directory()?;
        }
        let directory = self.directory.borrow();
        let directory = directory.as_ref().unwrap();
        let Some((block, code)) = directory.find(key)? else {
            return Ok(None);
        };
        let b = &directory.blocks[block];
        let chunk = code / WIDTH;
        let offset = b.chunks[chunk];
        if offset == 0 {
            return Ok(Some(None));
        }
        let format = directory.format;
        let len = chunk_len(format, (b.len - chunk * WIDTH).min(WIDTH));
        let start = (code % WIDTH) * ROW;
        let decode = |bytes: &[u8]| -> DiskIndexResult<Option<Option<[u8; LATEST_LEN]>>> {
            if bytes.len() != len {
                return Err(bad());
            }
            // Rows precede the trailer, so a row position never reaches it.
            Ok(Some(unpack(
                bytes.get(start..start + ROW).ok_or_else(bad)?,
                b.id,
            )?))
        };
        let mut cache = self.cache.borrow_mut();
        // One lookup on a warm cache. The offset alone cannot authenticate a
        // differently shaped reference in a damaged directory.
        if let Some(bytes) = cache.values.get(&offset) {
            return decode(bytes);
        }
        let mut bytes = buffer(len)?;
        read_at(&self.file, offset, &mut bytes)?;
        if crc32fast::hash(&bytes[..len - 4]) != read_u32(&bytes, len - 4) {
            return Err(reclaimed(format));
        }
        if format == PageFormat::Reuse {
            let trailer = len - 4 - CHUNK_TRAILER;
            if read_u32(&bytes, trailer) != b.id
                || read_u32(&bytes, trailer + 4) as usize != chunk
                || read_u64(&bytes, trailer + 8) != b.births[chunk]
            {
                return Err(DiskIndexError::PageReclaimed);
            }
        }
        let result = decode(&bytes)?;
        cache.order.try_reserve(1).map_err(oom)?;
        // Reserve half the budget for directory and cache bookkeeping.
        while cache.bytes + len + 128 > self.budget / 2 {
            let old = cache.order.pop_front().ok_or_else(bad)?;
            if let Some(bytes) = cache.values.remove(&old) {
                cache.bytes -= bytes.len() + 128;
            }
        }
        // BTreeMap node allocation remains the explicitly deferred collection.
        cache.values.insert(offset, bytes);
        cache.order.push_back(offset);
        cache.bytes += len + 128;
        Ok(result)
    }
}
pub(super) struct Working {
    directory: Directory,
    values: Vec<Vec<Option<[u8; LATEST_LEN]>>>,
    dirty: Vec<Vec<bool>>,
}
impl Working {
    pub fn load(reader: &Reader, layout: &[(u32, u32)], budget: usize) -> DiskIndexResult<Self> {
        let mut directory = reader.directory()?;
        if directory.blocks.is_empty() {
            directory
                .blocks
                .try_reserve_exact(layout.len())
                .map_err(oom)?;
            for &(id, len) in layout {
                let mut chunks = Vec::new();
                chunks
                    .try_reserve_exact((len as usize).div_ceil(WIDTH))
                    .map_err(oom)?;
                chunks.resize((len as usize).div_ceil(WIDTH), 0);
                let mut births = Vec::new();
                births.try_reserve_exact(chunks.len()).map_err(oom)?;
                births.resize(chunks.len(), 0);
                directory.blocks.push(Block {
                    id,
                    len: len as usize,
                    used: 0,
                    chunks,
                    births,
                });
            }
        }
        if directory
            .blocks
            .iter()
            .map(|b| (b.id, b.len as u32))
            .ne(layout.iter().copied())
        {
            return Err(bad());
        }
        let slots = layout
            .iter()
            .try_fold(0usize, |sum, (_, n)| sum.checked_add(*n as usize))
            .ok_or_else(bad)?;
        if slots.checked_mul(64).is_none_or(|n| n > budget) {
            return Err(bad());
        }
        let mut values = Vec::new();
        let mut dirty = Vec::new();
        values
            .try_reserve_exact(directory.blocks.len())
            .map_err(oom)?;
        dirty
            .try_reserve_exact(directory.blocks.len())
            .map_err(oom)?;
        for b in &directory.blocks {
            let mut rows = Vec::new();
            rows.try_reserve_exact(b.len).map_err(oom)?;
            for code in 0..b.len {
                let mut key = [0; 7];
                key[0] = 1;
                key[1..5].copy_from_slice(&b.id.to_be_bytes());
                key[5..].copy_from_slice(&(code as u16).to_le_bytes());
                rows.push(reader.get(&key)?.flatten());
            }
            if rows.iter().filter(|row| row.is_some()).count() != b.used as usize {
                return Err(bad());
            }
            values.push(rows);
            let mut changed = Vec::new();
            changed.try_reserve_exact(b.chunks.len()).map_err(oom)?;
            changed.resize(b.chunks.len(), false);
            dirty.push(changed);
        }
        Ok(Self {
            directory,
            values,
            dirty,
        })
    }
    pub fn count(&self) -> u64 {
        self.directory
            .blocks
            .iter()
            .map(|b| u64::from(b.used))
            .sum()
    }
    pub fn get(&self, key: &[u8]) -> DiskIndexResult<Option<Option<[u8; LATEST_LEN]>>> {
        Ok(self.directory.find(key)?.map(|(b, k)| self.values[b][k]))
    }
    pub fn put(&mut self, key: &[u8], value: [u8; LATEST_LEN]) -> DiskIndexResult<bool> {
        let Some((b, k)) = self.directory.find(key)? else {
            return Ok(false);
        };
        if self.values[b][k].is_none() {
            self.directory.blocks[b].used += 1;
        }
        self.values[b][k] = Some(value);
        self.dirty[b][k / WIDTH] = true;
        Ok(true)
    }
    pub fn compact(&mut self) {
        for (b, dirty) in self.directory.blocks.iter_mut().zip(&mut self.dirty) {
            b.chunks.fill(0);
            b.births.fill(0);
            dirty.fill(true);
        }
    }
    /// Writes every changed chunk and a new directory. With `alloc` the
    /// table is written in the reuse format into pages it hands out, and the
    /// pages of the chunks and directory it replaces are freed to it;
    /// `previous` is the directory being replaced.
    pub(super) fn publish(
        &mut self,
        file: &File,
        force: bool,
        mut alloc: Option<&mut PageAlloc>,
        previous: Root,
    ) -> DiskIndexResult<Root> {
        if self.directory.blocks.is_empty() {
            return Ok(Root::default());
        }
        let format = if alloc.is_some() {
            PageFormat::Reuse
        } else {
            PageFormat::Append
        };
        if format == PageFormat::Reuse && self.directory.format != PageFormat::Reuse {
            // A table is upgraded by compaction, which rewrites every chunk.
            if !force {
                return Err(bad());
            }
            self.directory.format = PageFormat::Reuse;
        }
        let birth = alloc.as_ref().map_or(0, |alloc| alloc.building);
        for ((b, rows), dirty) in self
            .directory
            .blocks
            .iter_mut()
            .zip(&self.values)
            .zip(&mut self.dirty)
        {
            for (chunk, changed) in dirty.iter_mut().enumerate() {
                if !*changed && !force {
                    continue;
                }
                if let Some(alloc) = alloc.as_deref_mut()
                    && b.chunks[chunk] != 0
                {
                    alloc.free(b.chunks[chunk], b.births[chunk])?;
                }
                let range = chunk * WIDTH..((chunk + 1) * WIDTH).min(b.len);
                if rows[range.clone()].iter().all(Option::is_none) {
                    b.chunks[chunk] = 0;
                    b.births[chunk] = 0;
                    *changed = false;
                    continue;
                }
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(chunk_len(format, range.len()))
                    .map_err(oom)?;
                for row in &rows[range] {
                    bytes.extend_from_slice(&pack(*row));
                }
                if format == PageFormat::Reuse {
                    bytes.extend_from_slice(&b.id.to_le_bytes());
                    bytes.extend_from_slice(&(chunk as u32).to_le_bytes());
                    bytes.extend_from_slice(&birth.to_le_bytes());
                }
                bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
                let offset = place(file, &bytes, alloc.as_deref_mut())?;
                b.chunks[chunk] = offset;
                b.births[chunk] = birth;
                *changed = false;
            }
        }
        let count = u32::try_from(self.count()).map_err(|_| bad())?;
        let fixed = header_len(format);
        let entry = chunk_entry(format);
        let len = self
            .directory
            .blocks
            .iter()
            .try_fold(fixed + 4, |n, b| {
                b.chunks
                    .len()
                    .checked_mul(entry)
                    .and_then(|m| m.checked_add(16))
                    .and_then(|m| n.checked_add(m))
            })
            .ok_or_else(bad)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len).map_err(oom)?;
        bytes.resize(fixed, 0);
        bytes[..8].copy_from_slice(match format {
            PageFormat::Append => MAGIC,
            PageFormat::Reuse => MAGIC_REUSE,
        });
        bytes[12..16].copy_from_slice(&(self.directory.blocks.len() as u32).to_le_bytes());
        bytes[16..24].copy_from_slice(&u64::from(count).to_le_bytes());
        if format == PageFormat::Reuse {
            bytes[24..32].copy_from_slice(&birth.to_le_bytes());
        }
        for b in &self.directory.blocks {
            bytes.extend_from_slice(&b.id.to_le_bytes());
            bytes.extend_from_slice(&(b.len as u32).to_le_bytes());
            bytes.extend_from_slice(&b.used.to_le_bytes());
            bytes.extend_from_slice(&(b.chunks.len() as u32).to_le_bytes());
            for (offset, chunk_birth) in b.chunks.iter().zip(&b.births) {
                bytes.extend_from_slice(&offset.to_le_bytes());
                if format == PageFormat::Reuse {
                    bytes.extend_from_slice(&chunk_birth.to_le_bytes());
                }
            }
        }
        let len = u32::try_from(bytes.len() + 4).map_err(|_| bad())?;
        bytes[8..12].copy_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
        if let Some(alloc) = alloc.as_deref_mut()
            && previous.offset != 0
            && previous.birth != 0
        {
            // Only a page-sized directory occupies a page of its own.
            if directory_fits_page(&self.directory) {
                alloc.free(previous.offset, previous.birth)?;
            }
        }
        let offset = place(file, &bytes, alloc)?;
        Ok(Root {
            offset,
            count,
            birth,
        })
    }
    pub fn layout(reader: &Reader) -> DiskIndexResult<Vec<(u32, u32)>> {
        reader.ensure_directory()?;
        let directory = reader.directory.borrow();
        let blocks = &directory.as_ref().unwrap().blocks;
        let mut layout = Vec::new();
        layout.try_reserve_exact(blocks.len()).map_err(oom)?;
        layout.extend(blocks.iter().map(|b| (b.id, b.len as u32)));
        Ok(layout)
    }
}
