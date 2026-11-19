//! The on-disk index of a publication too large for the ring.
//!
//! A B+tree of 4 KiB pages keyed by target offset; a leaf entry says where
//! the newest staged bytes of one range are (`at`) and how many there are.
//! The writer builds the tree of the generation it is publishing in
//! overflow space and names its root in the generation's commit frame; after
//! that the tree never changes. Readers and the writer reach pages through a
//! [`PageCache`] whose memory is taken once at open, so building, looking up
//! and walking an index of any size allocate nothing.
//!
//! Page layout (all little-endian):
//!
//! | Bytes | Field |
//! | --- | --- |
//! | 0..4 | magic `b"VIDX"` |
//! | 4..6 | kind: `1` leaf, `2` inner |
//! | 6..8 | entry count |
//! | 8..16 | leaf: the next leaf's offset, `0` for the last; inner: zero |
//! | 16..20 | CRC32 of the page with these four bytes zero |
//! | 20..24 | zero |
//! | 24.. | entries |
//!
//! A leaf entry is target `u64`, offset of the bytes `u64`, length `u32`,
//! zero `u32`. An inner entry is key `u64`, child offset `u64`; child `i`
//! holds the keys from key `i` up to key `i + 1`, and key 0 is zero.
//! Ranges never overlap partially and a range is always restaged whole, so
//! an entry ending past a separator never exists: a lookup only needs the
//! leaf its start routes to and the leaves after it.

use super::store::{TileSource, TileStore};
use super::wal::{IndexRoot, MAX_INDEX_HEIGHT};
use crate::{Error, Result};

pub(crate) const PAGE: u64 = 4096;
const PAGE_LEN: usize = 4096;
const MAGIC: &[u8; 4] = b"VIDX";
const HEAD: usize = 24;
const LEAF: u16 = 1;
const INNER: u16 = 2;
const LEAF_ENTRY: usize = 24;
const INNER_ENTRY: usize = 16;
pub(crate) const LEAF_MAX: usize = (PAGE_LEN - HEAD) / LEAF_ENTRY;
pub(crate) const INNER_MAX: usize = (PAGE_LEN - HEAD) / INNER_ENTRY;

/// Pages the default cache holds (1 MiB).
pub(crate) const DEFAULT_CACHE_PAGES: usize = 256;

#[derive(Clone, Copy, Default)]
struct Tag {
    /// Page offset; `0` marks an empty slot (no page lives at offset 0).
    at: u64,
    /// The generation whose index the page belongs to: an offset reused by
    /// a later index is a different page.
    generation: u64,
    stamp: u64,
    dirty: bool,
}

/// How a cache reaches the file: a reader only reads; the writer also
/// writes back the pages it changed.
pub(crate) enum Io<'a> {
    /// Reading only.
    Read(&'a dyn TileSource),
    Write(&'a mut dyn TileStore),
}

impl Io<'_> {
    pub(crate) fn reborrow(&mut self) -> Io<'_> {
        match self {
            Io::Read(source) => Io::Read(*source),
            Io::Write(store) => Io::Write(&mut **store),
        }
    }

    pub(crate) fn len(&self) -> Result<u64> {
        match self {
            Io::Read(source) => source.len(),
            Io::Write(store) => store.len(),
        }
    }
}

/// A fixed set of page buffers, least recently used out first.
pub(crate) struct PageCache {
    bytes: Box<[u8]>,
    tags: Box<[Tag]>,
    clock: u64,
}

fn u16_at(page: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(page[at..at + 2].try_into().expect("two"))
}
fn u32_at(page: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(page[at..at + 4].try_into().expect("four"))
}
fn u64_at(page: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(page[at..at + 8].try_into().expect("eight"))
}
fn put_u16(page: &mut [u8], at: usize, value: u16) {
    page[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(page: &mut [u8], at: usize, value: u32) {
    page[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(page: &mut [u8], at: usize, value: u64) {
    page[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn page_crc(page: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&page[..16]);
    hasher.update(&[0; 4]);
    hasher.update(&page[20..]);
    hasher.finalize()
}

fn seal(page: &mut [u8]) {
    let crc = page_crc(page);
    put_u32(page, 16, crc);
}

/// Whether `page` is a well-formed index page.
fn check(page: &[u8]) -> bool {
    let kind = u16_at(page, 4);
    let count = usize::from(u16_at(page, 6));
    &page[..4] == MAGIC
        && u32_at(page, 16) == page_crc(page)
        && match kind {
            LEAF => count <= LEAF_MAX,
            INNER => (1..=INNER_MAX).contains(&count) && u64_at(page, 8) == 0,
            _ => false,
        }
}

fn kind(page: &[u8]) -> u16 {
    u16_at(page, 4)
}
fn count(page: &[u8]) -> usize {
    usize::from(u16_at(page, 6))
}
fn set_count(page: &mut [u8], n: usize) {
    put_u16(
        page,
        6,
        u16::try_from(n).expect("a page holds fewer than 2^16 entries"),
    );
}
fn next_leaf(page: &[u8]) -> u64 {
    u64_at(page, 8)
}
fn leaf_entry(page: &[u8], i: usize) -> (u64, u64, u32) {
    let at = HEAD + i * LEAF_ENTRY;
    (
        u64_at(page, at),
        u64_at(page, at + 8),
        u32_at(page, at + 16),
    )
}
fn put_leaf_entry(page: &mut [u8], i: usize, (key, at, len): (u64, u64, u32)) {
    let o = HEAD + i * LEAF_ENTRY;
    put_u64(page, o, key);
    put_u64(page, o + 8, at);
    put_u32(page, o + 16, len);
    put_u32(page, o + 20, 0);
}
fn inner_entry(page: &[u8], i: usize) -> (u64, u64) {
    let at = HEAD + i * INNER_ENTRY;
    (u64_at(page, at), u64_at(page, at + 8))
}
fn put_inner_entry(page: &mut [u8], i: usize, (key, child): (u64, u64)) {
    let o = HEAD + i * INNER_ENTRY;
    put_u64(page, o, key);
    put_u64(page, o + 8, child);
}

fn init(page: &mut [u8], kind: u16) {
    page.fill(0);
    page[..4].copy_from_slice(MAGIC);
    put_u16(page, 4, kind);
}

/// The last entry index whose key is at most `key` among the first `n`
/// keys read by `key_of`; `None` when every key is larger.
fn floor(n: usize, key: u64, key_of: impl Fn(usize) -> u64) -> Option<usize> {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if key_of(mid) <= key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo.checked_sub(1)
}

/// A page offset read from an index page, believed only as far as it can be
/// a page inside the file.
fn page_offset(raw: u64, available: u64) -> Result<u64> {
    if raw == 0 || raw.checked_add(PAGE).is_none_or(|end| end > available) {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(raw)
}

impl PageCache {
    /// A cache of `pages` pages, its memory taken now.
    pub(crate) fn with_pages(pages: usize) -> Result<Self> {
        let pages = pages.max(8);
        let len = pages * PAGE_LEN;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix index page cache",
                requested: len as u64,
            })?;
        bytes.resize(len, 0);
        let mut tags = Vec::new();
        tags.try_reserve_exact(pages)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix index page cache",
                requested: (pages * std::mem::size_of::<Tag>()) as u64,
            })?;
        tags.resize(pages, Tag::default());
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            tags: tags.into_boxed_slice(),
            clock: 0,
        })
    }

    fn page(&self, slot: usize) -> &[u8] {
        &self.bytes[slot * PAGE_LEN..(slot + 1) * PAGE_LEN]
    }

    fn page_mut(&mut self, slot: usize) -> &mut [u8] {
        self.tags[slot].dirty = true;
        &mut self.bytes[slot * PAGE_LEN..(slot + 1) * PAGE_LEN]
    }

    fn touch(&mut self, slot: usize) {
        self.clock += 1;
        self.tags[slot].stamp = self.clock;
    }

    fn find(&self, at: u64, generation: u64) -> Option<usize> {
        self.tags
            .iter()
            .position(|tag| tag.at == at && tag.generation == generation)
    }

    /// The least recently used slot, written back first when dirty.
    fn victim(&mut self, io: &mut Io<'_>) -> Result<usize> {
        // Reading only, a changed page cannot be written back: it stays.
        let writable = matches!(io, Io::Write(_));
        let slot = self
            .tags
            .iter()
            .enumerate()
            .filter(|(_, tag)| writable || !tag.dirty)
            .min_by_key(|(_, tag)| if tag.at == 0 { 0 } else { tag.stamp })
            .map(|(slot, _)| slot)
            .ok_or(Error::InvalidMatrixLayout)?;
        if self.tags[slot].dirty {
            let Io::Write(store) = io else {
                return Err(Error::InvalidMatrixLayout);
            };
            let at = self.tags[slot].at;
            let page = &mut self.bytes[slot * PAGE_LEN..(slot + 1) * PAGE_LEN];
            seal(page);
            store.write_at(at, page)?;
            self.tags[slot].dirty = false;
        }
        self.tags[slot] = Tag::default();
        Ok(slot)
    }

    /// The slot holding page `at` of `generation`'s index, read and checked
    /// when not cached. A page that does not check out is
    /// `InvalidMatrixLayout`.
    fn load(&mut self, mut io: Io<'_>, at: u64, generation: u64) -> Result<usize> {
        if let Some(slot) = self.find(at, generation) {
            self.touch(slot);
            return Ok(slot);
        }
        let slot = self.victim(&mut io)?;
        let page = &mut self.bytes[slot * PAGE_LEN..(slot + 1) * PAGE_LEN];
        match io {
            Io::Read(source) => source.read_at(at, page)?,
            Io::Write(store) => store.read_at(at, page)?,
        }
        if !check(page) {
            return Err(Error::InvalidMatrixLayout);
        }
        self.tags[slot] = Tag {
            at,
            generation,
            stamp: 0,
            dirty: false,
        };
        self.touch(slot);
        Ok(slot)
    }

    /// A new page at `at`, initialized, not read.
    fn create(
        &mut self,
        store: &mut dyn TileStore,
        at: u64,
        generation: u64,
        kind: u16,
    ) -> Result<usize> {
        let slot = self.victim(&mut Io::Write(store))?;
        self.tags[slot] = Tag {
            at,
            generation,
            stamp: 0,
            dirty: true,
        };
        init(self.page_mut(slot), kind);
        self.touch(slot);
        Ok(slot)
    }

    /// Writes every changed page to the file.
    pub(crate) fn flush(&mut self, store: &mut dyn TileStore) -> Result<()> {
        for slot in 0..self.tags.len() {
            if self.tags[slot].dirty {
                let at = self.tags[slot].at;
                let page = &mut self.bytes[slot * PAGE_LEN..(slot + 1) * PAGE_LEN];
                seal(page);
                store.write_at(at, page)?;
                self.tags[slot].dirty = false;
            }
        }
        Ok(())
    }

    /// Pages changed and not yet written back.
    pub(crate) fn dirty(&self) -> usize {
        self.tags.iter().filter(|tag| tag.dirty).count()
    }

    pub(crate) fn pages(&self) -> usize {
        self.tags.len()
    }

    /// Forgets every cached page of `generation`'s index without writing it.
    pub(crate) fn forget(&mut self, generation: u64) {
        for tag in self.tags.iter_mut() {
            if tag.generation == generation {
                *tag = Tag::default();
            }
        }
    }
}

/// An index being built: the open generation's.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Building {
    pub generation: u64,
    pub root: IndexRoot,
}

impl Building {
    pub(crate) fn new(
        cache: &mut PageCache,
        store: &mut dyn TileStore,
        generation: u64,
        alloc: &mut dyn FnMut(&mut dyn TileStore, u64) -> Result<u64>,
    ) -> Result<Self> {
        let root = alloc(store, PAGE)?;
        cache.create(store, root, generation, LEAF)?;
        Ok(Self {
            generation,
            root: IndexRoot {
                root,
                height: 0,
                count: 0,
            },
        })
    }

    /// Records that the newest bytes of the range at `key` are `len` bytes
    /// at `at`, replacing what the index said of it.
    pub(crate) fn insert(
        &mut self,
        cache: &mut PageCache,
        store: &mut dyn TileStore,
        alloc: &mut dyn FnMut(&mut dyn TileStore, u64) -> Result<u64>,
        key: u64,
        at: u64,
        len: u32,
    ) -> Result<()> {
        let generation = self.generation;
        // Inner pages from the root down: (page, child index taken).
        let mut path = [(0u64, 0usize); MAX_INDEX_HEIGHT as usize + 1];
        let mut page = self.root.root;
        for step in path.iter_mut().take(self.root.height as usize) {
            let slot = cache.load(Io::Write(&mut *store), page, generation)?;
            let node = cache.page(slot);
            let i = floor(count(node), key, |i| inner_entry(node, i).0).unwrap_or(0);
            *step = (page, i);
            page = inner_entry(node, i).1;
        }
        let slot = cache.load(Io::Write(&mut *store), page, generation)?;
        let node = cache.page(slot);
        let n = count(node);
        let found = floor(n, key, |i| leaf_entry(node, i).0);
        if let Some(i) = found
            && leaf_entry(node, i).0 == key
        {
            put_leaf_entry(cache.page_mut(slot), i, (key, at, len));
            return Ok(());
        }
        let position = found.map_or(0, |i| i + 1);
        self.root.count += 1;
        if n < LEAF_MAX {
            let node = cache.page_mut(slot);
            shift_up(node, position, n, LEAF_ENTRY);
            put_leaf_entry(node, position, (key, at, len));
            set_count(node, n + 1);
            return Ok(());
        }
        // Split the full leaf: the upper half moves to a new leaf after it.
        let right = alloc(store, PAGE)?;
        let mut moved = [0u8; PAGE_LEN];
        let half = n / 2;
        let old_next = {
            let slot = cache.load(Io::Write(&mut *store), page, generation)?;
            let node = cache.page(slot);
            moved[..(n - half) * LEAF_ENTRY]
                .copy_from_slice(&node[HEAD + half * LEAF_ENTRY..HEAD + n * LEAF_ENTRY]);
            next_leaf(node)
        };
        let rslot = cache.create(store, right, generation, LEAF)?;
        {
            let rnode = cache.page_mut(rslot);
            rnode[HEAD..HEAD + (n - half) * LEAF_ENTRY]
                .copy_from_slice(&moved[..(n - half) * LEAF_ENTRY]);
            set_count(rnode, n - half);
            put_u64(rnode, 8, old_next);
        }
        let lslot = cache.load(Io::Write(&mut *store), page, generation)?;
        {
            let lnode = cache.page_mut(lslot);
            set_count(lnode, half);
            put_u64(lnode, 8, right);
        }
        let rslot = cache.load(Io::Write(&mut *store), right, generation)?;
        let separator = leaf_entry(cache.page(rslot), 0).0;
        // Put the new key in the half that now holds its place.
        let (target_page, index, len_now) = if position <= half {
            (page, position, half)
        } else {
            (right, position - half, n - half)
        };
        let tslot = cache.load(Io::Write(&mut *store), target_page, generation)?;
        let node = cache.page_mut(tslot);
        shift_up(node, index, len_now, LEAF_ENTRY);
        put_leaf_entry(node, index, (key, at, len));
        set_count(node, len_now + 1);
        let separator = if target_page == right && index == 0 {
            key
        } else {
            separator
        };
        self.push_up(cache, store, alloc, &path, separator, right)
    }

    /// Inserts `(separator, child)` into the inner pages on `path`, splitting
    /// them as needed, and grows a new root when the old one splits.
    fn push_up(
        &mut self,
        cache: &mut PageCache,
        store: &mut dyn TileStore,
        alloc: &mut dyn FnMut(&mut dyn TileStore, u64) -> Result<u64>,
        path: &[(u64, usize)],
        mut separator: u64,
        mut child: u64,
    ) -> Result<()> {
        let generation = self.generation;
        for level in (0..self.root.height as usize).rev() {
            let (page, taken) = path[level];
            let slot = cache.load(Io::Write(&mut *store), page, generation)?;
            let n = count(cache.page(slot));
            let position = taken + 1;
            if n < INNER_MAX {
                let node = cache.page_mut(slot);
                shift_up(node, position, n, INNER_ENTRY);
                put_inner_entry(node, position, (separator, child));
                set_count(node, n + 1);
                return Ok(());
            }
            let right = alloc(store, PAGE)?;
            let half = n / 2;
            let mut moved = [0u8; PAGE_LEN];
            {
                let slot = cache.load(Io::Write(&mut *store), page, generation)?;
                let node = cache.page(slot);
                moved[..(n - half) * INNER_ENTRY]
                    .copy_from_slice(&node[HEAD + half * INNER_ENTRY..HEAD + n * INNER_ENTRY]);
            }
            let rslot = cache.create(store, right, generation, INNER)?;
            {
                let rnode = cache.page_mut(rslot);
                rnode[HEAD..HEAD + (n - half) * INNER_ENTRY]
                    .copy_from_slice(&moved[..(n - half) * INNER_ENTRY]);
                set_count(rnode, n - half);
            }
            let lslot = cache.load(Io::Write(&mut *store), page, generation)?;
            set_count(cache.page_mut(lslot), half);
            // `moved` holds entries from its first byte, not after a header.
            let up = u64::from_le_bytes(moved[..8].try_into().expect("eight"));
            let (target_page, index, len_now) = if position <= half {
                (page, position, half)
            } else {
                (right, position - half, n - half)
            };
            let tslot = cache.load(Io::Write(&mut *store), target_page, generation)?;
            let node = cache.page_mut(tslot);
            shift_up(node, index, len_now, INNER_ENTRY);
            put_inner_entry(node, index, (separator, child));
            set_count(node, len_now + 1);
            let up = if target_page == right && index == 0 {
                separator
            } else {
                up
            };
            separator = up;
            child = right;
        }
        // The root split: a new root over both halves.
        if self.root.height >= MAX_INDEX_HEIGHT {
            return Err(Error::InvalidMatrixLayout);
        }
        let root = alloc(store, PAGE)?;
        let slot = cache.create(store, root, generation, INNER)?;
        let node = cache.page_mut(slot);
        put_inner_entry(node, 0, (0, self.root.root));
        put_inner_entry(node, 1, (separator, child));
        set_count(node, 2);
        self.root.root = root;
        self.root.height += 1;
        Ok(())
    }
}

/// Moves entries `[from, n)` of a page one place up.
fn shift_up(page: &mut [u8], from: usize, n: usize, entry: usize) {
    if from < n {
        page.copy_within(
            HEAD + from * entry..HEAD + n * entry,
            HEAD + (from + 1) * entry,
        );
    }
}

/// Calls `visit(target, at, len)` for every entry of `generation`'s index
/// rooted at `root` whose range overlaps `[at, at + len)`, in key order.
pub(crate) fn for_each_overlapping(
    cache: &mut PageCache,
    mut io: Io<'_>,
    generation: u64,
    root: IndexRoot,
    at: u64,
    len: u64,
    visit: &mut dyn FnMut(u64, u64, u32) -> Result<()>,
) -> Result<()> {
    let available = io.len()?;
    let end = at.saturating_add(len);
    let mut page = root.root;
    for _ in 0..root.height {
        let slot = cache.load(io.reborrow(), page, generation)?;
        let node = cache.page(slot);
        if kind(node) != INNER {
            return Err(Error::InvalidMatrixLayout);
        }
        let i = floor(count(node), at, |i| inner_entry(node, i).0).unwrap_or(0);
        page = page_offset(inner_entry(node, i).1, available)?;
    }
    let slot = cache.load(io.reborrow(), page, generation)?;
    let node = cache.page(slot);
    if kind(node) != LEAF {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut i = floor(count(node), at, |i| leaf_entry(node, i).0).unwrap_or(0);
    // Leaves are visited at most once each: the chain cannot loop longer
    // than the index has ranges.
    for _ in 0..=root.count {
        let slot = cache.load(io.reborrow(), page, generation)?;
        let node = cache.page(slot);
        let n = count(node);
        while i < n {
            let (key, bytes_at, bytes_len) = leaf_entry(node, i);
            if key >= end {
                return Ok(());
            }
            if key.saturating_add(u64::from(bytes_len)) > at {
                visit(key, bytes_at, bytes_len)?;
            }
            i += 1;
        }
        let next = next_leaf(node);
        if next == 0 {
            return Ok(());
        }
        page = page_offset(next, available)?;
        i = 0;
    }
    Err(Error::InvalidMatrixLayout)
}

/// Calls `visit(io, target, at, len)` for every entry of the index, in key
/// order; the visitor reaches the file through the same `io`.
pub(crate) fn for_each(
    cache: &mut PageCache,
    mut io: Io<'_>,
    generation: u64,
    root: IndexRoot,
    visit: &mut dyn FnMut(&mut Io<'_>, u64, u64, u32) -> Result<()>,
) -> Result<()> {
    let available = io.len()?;
    let mut page = root.root;
    for _ in 0..root.height {
        let slot = cache.load(io.reborrow(), page, generation)?;
        let node = cache.page(slot);
        if kind(node) != INNER {
            return Err(Error::InvalidMatrixLayout);
        }
        page = page_offset(inner_entry(node, 0).1, available)?;
    }
    let mut seen = 0u64;
    loop {
        let slot = cache.load(io.reborrow(), page, generation)?;
        let node = cache.page(slot);
        if kind(node) != LEAF {
            return Err(Error::InvalidMatrixLayout);
        }
        let n = count(node);
        let next = next_leaf(node);
        for i in 0..n {
            let (key, bytes_at, bytes_len) = leaf_entry(cache.page(slot), i);
            visit(&mut io, key, bytes_at, bytes_len)?;
        }
        seen += n as u64;
        if next == 0 {
            return Ok(());
        }
        if seen > root.count {
            return Err(Error::InvalidMatrixLayout);
        }
        page = page_offset(next, available)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix_tiles::store::memory::MemStore;
    use std::collections::BTreeMap;

    /// Random inserts and replacements, against a `BTreeMap`, through a cache
    /// far smaller than the tree; lookups and a walk from a fresh cache.
    #[test]
    fn an_index_built_through_a_small_cache_matches_an_ordered_model() {
        let store = MemStore::new();
        let mut writer = store.clone();
        let mut cache = PageCache::with_pages(8).unwrap();
        // Pages go into one record appended up front.
        let base = writer
            .append_zeroed(crate::matrix_tiles::RecordKind::Overflow, &[], 64 << 20)
            .unwrap();
        let mut next_page = base;
        let mut alloc = |_: &mut dyn TileStore, len: u64| -> Result<u64> {
            let at = next_page;
            next_page += len;
            Ok(at)
        };
        let mut index = Building::new(&mut cache, &mut writer, 7, &mut alloc).unwrap();
        let mut model = BTreeMap::new();
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for round in 0..60_000u64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let key = (state % 40_000) * 32;
            index
                .insert(&mut cache, &mut writer, &mut alloc, key, round, 32)
                .unwrap();
            model.insert(key, round);
        }
        assert!(index.root.height >= 2, "{:?}", index.root);
        assert_eq!(index.root.count, model.len() as u64);
        cache.flush(&mut writer).unwrap();
        let mut reader = PageCache::with_pages(8).unwrap();
        let mut all = Vec::new();
        for_each(
            &mut reader,
            Io::Read(&store),
            7,
            index.root,
            &mut |_, key, at, _| {
                all.push((key, at));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(all, model.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>());
        for probe in [0u64, 31, 32, 1000, 639_999, 1_279_990, 1_280_000] {
            let mut seen = Vec::new();
            for_each_overlapping(
                &mut reader,
                Io::Read(&store),
                7,
                index.root,
                probe,
                100,
                &mut |key, at, _| {
                    seen.push((key, at));
                    Ok(())
                },
            )
            .unwrap();
            let want: Vec<_> = model
                .range(..probe + 100)
                .filter(|(key, _)| **key + 32 > probe)
                .map(|(k, v)| (*k, *v))
                .collect();
            assert_eq!(seen, want, "probe {probe}");
        }
        // Another generation's pages at the same offsets are other pages.
        assert!(
            for_each(
                &mut reader,
                Io::Read(&store),
                8,
                index.root,
                &mut |_, _, _, _| Ok(())
            )
            .is_ok()
        );
    }
}
