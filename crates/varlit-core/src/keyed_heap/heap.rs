//! The single writer of keyed storage: puts, deletes and finds records, and
//! keeps the heap within the declared free-space ratio.
//!
//! Memory is fixed when the writer is opened: the ring's buffers, the hole
//! cache, 64 bucket page buffers and a small table of sparse regions. Every
//! chunk is read and written through stack buffers, so puts, deletes, splits
//! and compaction allocate nothing.
//!
//! **New values go to new space.** A put never writes over the value it
//! replaces: it takes space no published generation since the last
//! checkpoint points at (a hole freed by a generation in place, the current
//! region's tail, a free region or a new one), writes the chunk there
//! straight to the file when nothing staged covers it, and changes only the
//! slot through the ring. The old chunk becomes a hole, retaken after the
//! next checkpoint, which runs once more freed space waits than half the
//! free-space allowance.
//!
//! **Staging units.** A reader's overlay keeps one range per start offset
//! and lays overlapping ranges in offset order, so every byte this writer
//! stages is staged in the same units each time: a bucket page whole (from
//! the page buffers, once per publication), a chunk as its header, its key
//! and its payload, a region as its header. Where the units themselves
//! change (a hole or a free region taken for chunks of other sizes), the
//! space is taken only once the generation that freed it is in place, so no
//! overlay still holds a range in its old units.

use super::lookup::{self, Located, slot_at};
use super::pages::{HeapView, Pages};
use super::sip::Sip;
use super::{
    BUCKET_HEADER, BUCKET_PAGE, CHUNK_HEADER, Chunk, FREE_BLOCK, Fixed, META_FIXED_LEN, META_LEN,
    META_OVERFLOW_HEAD_AT, META_STATE_AT, REGION_BYTES, REGION_HEADER, Region, SLOT_LEN,
    SLOTS_PER_PAGE, Slot, State, corrupt, heap_chunk_len, page_header, set_page_header,
};
use crate::matrix_tiles::ring_store::RingStore;
use crate::matrix_tiles::store::{TileSource, TileStore};
use crate::matrix_tiles::view::directory_offset;
use crate::matrix_tiles::{DIR_FANOUT, DIR_LEVEL_BITS, DIR_PAGE_BYTES, RecordKind};
use crate::{Error, Result};

/// Holes the writer remembers. Fixed when the writer is opened; when it is
/// full the smallest hole is forgotten (compaction reclaims it later).
const HOLE_CACHE: usize = 4096;
/// Sparse regions remembered as compaction candidates.
const SPARSE: usize = 32;
/// Bytes copied per step when a chunk moves or a key is compared.
const STEP: usize = 4096;
const HEADER_BYTES: usize = crate::width::small(BUCKET_HEADER);
const SLOT_BYTES: usize = crate::width::small(SLOT_LEN);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeapStats {
    pub splits: u64,
    pub regions_appended: u64,
    pub regions_reused: u64,
    pub holes_reused: u64,
    pub chunks_moved: u64,
    pub regions_compacted: u64,
    /// Checkpoints run so freed space could be retaken.
    pub checkpoints: u64,
    /// Chunk units written straight to the file rather than through the
    /// ring.
    pub direct_writes: u64,
}

/// Region headers changed since the last publication, staged once each
/// when the writer publishes (or when the table needs room), instead of
/// once per change.
const DIRTY_REGIONS: usize = 64;

#[derive(Clone, Copy, Debug, Default)]
struct DirtyRegion {
    /// The region's offset; `0` for an empty entry.
    at: u64,
    header: Region,
    used: u64,
}

struct DirtyRegions {
    entries: [DirtyRegion; DIRTY_REGIONS],
    clock: u64,
}

impl DirtyRegions {
    const fn new() -> Self {
        Self {
            entries: [DirtyRegion {
                at: 0,
                header: Region {
                    len: 0,
                    live: 0,
                    next_all: 0,
                    next_free: 0,
                    freed: 0,
                },
                used: 0,
            }; DIRTY_REGIONS],
            clock: 0,
        }
    }

    fn get(&self, at: u64) -> Option<Region> {
        self.entries
            .iter()
            .find(|entry| entry.at == at && at != 0)
            .map(|entry| entry.header)
    }

    /// Records `header` for the region at `at`; returns an entry it pushed
    /// out to make room, to be staged.
    fn set(&mut self, at: u64, header: Region) -> Option<(u64, Region)> {
        self.clock += 1;
        let index = match self.entries.iter().position(|entry| entry.at == at) {
            Some(index) => index,
            None => (0..DIRTY_REGIONS)
                .min_by_key(|&index| (self.entries[index].at != 0, self.entries[index].used))
                .expect("entries"),
        };
        let old = self.entries[index];
        self.entries[index] = DirtyRegion {
            at,
            header,
            used: self.clock,
        };
        (old.at != 0 && old.at != at).then_some((old.at, old.header))
    }

    fn take(&mut self) -> Option<(u64, Region)> {
        let entry = self.entries.iter_mut().find(|entry| entry.at != 0)?;
        let taken = (entry.at, entry.header);
        entry.at = 0;
        Some(taken)
    }

    fn is_empty(&self) -> bool {
        self.entries.iter().all(|entry| entry.at == 0)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Hole {
    region: u64,
    at: u64,
    len: u64,
    /// The generation that freed it: reusable once that is in place.
    generation: u64,
}

struct Holes {
    entries: Box<[Hole]>,
    len: usize,
}

impl Holes {
    fn new() -> Result<Self> {
        Self::with_capacity(HOLE_CACHE)
    }

    fn with_capacity(capacity: usize) -> Result<Self> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| Error::AllocationFailed {
                resource: "keyed heap hole cache",
                requested: (capacity * std::mem::size_of::<Hole>()) as u64,
            })?;
        entries.resize(capacity, Hole::default());
        Ok(Self {
            entries: entries.into_boxed_slice(),
            len: 0,
        })
    }

    fn add(&mut self, hole: Hole) {
        if self.len < self.entries.len() {
            self.entries[self.len] = hole;
            self.len += 1;
            return;
        }
        let (index, smallest) = self.entries[..self.len]
            .iter()
            .enumerate()
            .min_by_key(|(_, held)| held.len)
            .map(|(index, held)| (index, held.len))
            .expect("the cache is full");
        if hole.len > smallest {
            self.entries[index] = hole;
        }
    }

    /// The smallest hole of at least `len` bytes outside `avoid` that a
    /// generation up to `in_place` freed.
    fn take_fitting(&mut self, len: u64, avoid: u64, in_place: u64) -> Option<Hole> {
        let index = self.entries[..self.len]
            .iter()
            .enumerate()
            .filter(|(_, hole)| {
                hole.len >= len && hole.region != avoid && hole.generation <= in_place
            })
            .min_by_key(|(_, hole)| hole.len)
            .map(|(index, _)| index)?;
        let hole = self.entries[index];
        self.len -= 1;
        self.entries[index] = self.entries[self.len];
        Some(hole)
    }

    fn forget_region(&mut self, region: u64) {
        let mut index = 0;
        while index < self.len {
            if self.entries[index].region == region {
                self.len -= 1;
                self.entries[index] = self.entries[self.len];
            } else {
                index += 1;
            }
        }
    }
}

/// The sparsest regions seen: `(region, live, len)`.
struct Sparse {
    entries: [(u64, u64, u64); SPARSE],
    len: usize,
}

impl Sparse {
    const fn new() -> Self {
        Self {
            entries: [(0, 0, 0); SPARSE],
            len: 0,
        }
    }

    fn forget(&mut self, region: u64) {
        if let Some(index) = self.entries[..self.len]
            .iter()
            .position(|entry| entry.0 == region)
        {
            self.len -= 1;
            self.entries[index] = self.entries[self.len];
        }
    }

    /// Notes a region's fill; keeps the [`SPARSE`] emptiest.
    fn note(&mut self, region: u64, live: u64, len: u64) {
        self.forget(region);
        if live >= len {
            return;
        }
        let entry = (region, live, len);
        if self.len < SPARSE {
            self.entries[self.len] = entry;
            self.len += 1;
            return;
        }
        let fullest = (0..self.len)
            .max_by(|&a, &b| fill(self.entries[a]).total_cmp(&fill(self.entries[b])))
            .expect("full");
        if fill(entry) < fill(self.entries[fullest]) {
            self.entries[fullest] = entry;
        }
    }

    fn take(&mut self) -> Option<u64> {
        let index = (0..self.len)
            .min_by(|&a, &b| fill(self.entries[a]).total_cmp(&fill(self.entries[b])))?;
        let region = self.entries[index].0;
        self.len -= 1;
        self.entries[index] = self.entries[self.len];
        Some(region)
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "a ranking of fill ratios, not an exact quantity"
)]
fn fill(entry: (u64, u64, u64)) -> f64 {
    entry.1 as f64 / entry.2.max(1) as f64
}

#[cfg(kani)]
#[path = "../verification/keyed_heap_holes.rs"]
mod hole_proofs;

pub(crate) struct KeyedHeap {
    pub(crate) meta: u64,
    fixed: Fixed,
    pub(crate) ring: RingStore,
    state: State,
    /// The state as the last publication staged it.
    staged: State,
    /// The current region's payload length (`0`: none).
    cur_len: u64,
    holes: Holes,
    sparse: Sparse,
    pages: Pages,
    regions: DirtyRegions,
    /// Bytes freed by the open generation, and by published generations
    /// not yet in place: space that cannot be retaken before a checkpoint.
    freed_open: u64,
    freed_waiting: u64,
    /// Bytes freed by the last published generation: the puts after it
    /// take them first.
    freed_last: u64,
    pub(crate) stats: HeapStats,
}

impl KeyedHeap {
    /// Lays out empty keyed storage: the metadata record, a ring of
    /// `ring_len` bytes, one empty bucket and its directory, published.
    pub(crate) fn create(
        store: &mut dyn TileStore,
        ring_len: u64,
        ratio_permille: u16,
        seed: [u64; 2],
    ) -> Result<Self> {
        if !(10..=50).contains(&ratio_permille) {
            return Err(Error::InvalidFormatSpec(
                "keyed storage free-space ratio must be 1 to 5 %",
            ));
        }
        let mut fixed = Fixed {
            ratio_permille,
            seed,
            ring: (0, 0),
        };
        let meta = store.append_zeroed(RecordKind::KeyedMeta, &fixed.encode(), META_LEN)?;
        let ring = RingStore::create(store, ring_len, meta + META_OVERFLOW_HEAD_AT)?;
        fixed.ring = (ring.ring.base, ring.ring.len);
        store.write_at(meta, &fixed.encode())?;
        let mut heap = Self::with_parts(meta, fixed, ring, State::default())?;
        let root = heap.new_dir_page(store)?;
        let bucket = heap.alloc_page(store)?;
        let page = heap.pages.page_new(&mut heap.ring, store, bucket)?;
        set_page_header(page, 0, 0);
        heap.ring.stage(store, root, &bucket.to_le_bytes())?;
        heap.state.dir = (root, 1);
        heap.publish(store)?;
        Ok(heap)
    }

    /// Opens keyed storage whose metadata payload is at `meta`.
    pub(crate) fn open(store: &mut dyn TileStore, meta: u64) -> Result<Self> {
        let mut bytes = [0u8; crate::width::small(META_FIXED_LEN)];
        store.read_at(meta, &mut bytes)?;
        let fixed = Fixed::decode(&bytes)?;
        let ring = RingStore::open(store, fixed.ring, meta + META_OVERFLOW_HEAD_AT)?;
        let mut raw = [0u8; State::FIELDS * 8];
        ring.read(store, meta + META_STATE_AT, &mut raw)?;
        let state = State::decode(&raw)?;
        let mut heap = Self::with_parts(meta, fixed, ring, state)?;
        heap.staged = state;
        if state.cur != 0 {
            heap.cur_len = heap.region(store, state.cur)?.len;
        }
        Ok(heap)
    }

    fn with_parts(meta: u64, fixed: Fixed, ring: RingStore, state: State) -> Result<Self> {
        Ok(Self {
            meta,
            fixed,
            ring,
            state,
            staged: State::default(),
            cur_len: 0,
            holes: Holes::new()?,
            sparse: Sparse::new(),
            pages: Pages::new()?,
            regions: DirtyRegions::new(),
            freed_open: 0,
            freed_waiting: 0,
            freed_last: 0,
            stats: HeapStats::default(),
        })
    }

    pub(crate) const fn state(&self) -> &State {
        &self.state
    }

    /// Bytes of holes in the heap (see [`State::holes`]).
    /// The holes the free-space ratio allows: a file whose holes are
    /// smaller than one region is not worth a move.
    fn allowance(&self) -> u64 {
        (self.state.live / 1000 * u64::from(self.fixed.ratio_permille)).max(REGION_BYTES)
    }

    /// Holes apart from those the last two generations freed: those are
    /// what the next puts take (after a checkpoint), not fragmentation a
    /// move should fix.
    pub(crate) fn settled_holes(&self) -> u64 {
        self.holes()
            .saturating_sub(self.freed_open + self.freed_last)
    }

    pub(crate) const fn holes(&self) -> u64 {
        self.state.holes(self.cur_tail())
    }

    const fn cur_tail(&self) -> u64 {
        if self.state.cur == 0 {
            0
        } else {
            self.cur_len - self.state.cur_used
        }
    }

    // ---- publication ----

    /// Publishes everything since the last publication as one generation.
    /// Whether anything changed since the last publication.
    pub(crate) fn is_dirty(&self) -> bool {
        self.ring.is_dirty()
            || self.pages.is_dirty()
            || !self.regions.is_empty()
            || self.state != self.staged
    }

    pub(crate) fn publish(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        self.pages.flush(&mut self.ring, store)?;
        while let Some((at, header)) = self.regions.take() {
            self.ring.stage(store, at, &header.encode())?;
        }
        if self.state != self.staged {
            self.ring
                .stage(store, self.meta + META_STATE_AT, &self.state.encode())?;
            self.staged = self.state;
        }
        let generation = self.ring.publish(store)?;
        self.freed_waiting += self.freed_open;
        self.freed_last = std::mem::take(&mut self.freed_open);
        Ok(generation)
    }

    // ---- lookups ----

    fn route(&self, block_id: u32, key: &[u8]) -> u32 {
        lookup::route(self.fixed.seed, block_id, key)
    }

    /// Finds `key` of `block_id` as this writer sees it.
    pub(crate) fn find(
        &self,
        store: &dyn TileSource,
        block_id: u32,
        key: &[u8],
    ) -> Result<Option<Located>> {
        lookup::find(
            &mut self.view(),
            store,
            self.fixed.seed,
            &self.state,
            block_id,
            key,
        )
    }

    /// Reads a located record's payload into `out`; see [`lookup::read_payload`].
    pub(crate) fn read_payload(
        &self,
        store: &dyn TileSource,
        found: &Located,
        out: &mut [u8],
    ) -> Result<()> {
        lookup::read_payload(&mut self.view(), store, found, out)
    }

    fn chunk(&self, store: &dyn TileSource, at: u64) -> Result<Chunk> {
        lookup::chunk(&mut self.view(), store, at)
    }

    fn region(&self, store: &dyn TileSource, at: u64) -> Result<Region> {
        if let Some(header) = self.regions.get(at) {
            return Ok(header);
        }
        let mut header = [0u8; crate::width::small(REGION_HEADER)];
        self.read(store, at, &mut header)?;
        Region::decode(&header)
    }

    // ---- puts and deletes ----

    /// Stores `payload` as the record of `key` of `block_id`, replacing the
    /// one there is.
    pub(crate) fn put(
        &mut self,
        store: &mut dyn TileStore,
        block_id: u32,
        key: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        if block_id == FREE_BLOCK {
            return Err(corrupt("reserved keyed block id"));
        }
        let header = Chunk {
            payload_len: field32("keyed record payload", payload.len())?,
            block_id,
            key_len: field32("keyed record key", key.len())?,
            crc: 0,
        };
        let len = heap_chunk_len(key.len() as u64, payload.len() as u64);
        // Space first: taking it may compact, which moves chunks, the old
        // value of this key among them.
        let (region, at) = self.alloc(store, len, 0)?;
        let found = self.find(store, block_id, key)?;
        match found {
            // A new value goes to new space, never over the old one: space
            // that can be reused is written straight to the file, and only
            // the slot that points at it goes through the ring.
            Some(found) => {
                self.write_chunk(store, at, header, key, payload)?;
                self.add_live(store, region, len)?;
                let slot = Slot {
                    route: found.slot.route,
                    offset: offset_in(region, at)?,
                    region,
                };
                self.set_slot(store, found.page, found.index, slot)?;
                self.release(store, found.slot.region, found.slot.at(), found.chunk.len())?;
            }
            None => {
                self.write_chunk(store, at, header, key, payload)?;
                self.add_live(store, region, len)?;
                let slot = Slot {
                    route: self.route(block_id, key),
                    offset: offset_in(region, at)?,
                    region,
                };
                self.insert_slot(store, self.state.bucket_of(slot.route), slot)?;
                self.state.keys += 1;
                while self.state.keys * 4 > self.state.buckets() * SLOTS_PER_PAGE as u64 * 3 {
                    self.split(store)?;
                }
            }
        }
        self.compact_if_needed(store)
    }

    /// Removes `key` of `block_id`; `false` when there is none.
    pub(crate) fn delete(
        &mut self,
        store: &mut dyn TileStore,
        block_id: u32,
        key: &[u8],
    ) -> Result<bool> {
        let Some(found) = self.find(store, block_id, key)? else {
            return Ok(false);
        };
        self.remove_slot(store, found.page, found.index)?;
        self.state.keys -= 1;
        self.release(store, found.slot.region, found.slot.at(), found.chunk.len())?;
        self.compact_if_needed(store)?;
        Ok(true)
    }

    fn write_chunk(
        &mut self,
        store: &mut dyn TileStore,
        at: u64,
        mut header: Chunk,
        key: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        let mut crc = crc32fast::Hasher::new();
        crc.update(&header.encode()[..12]);
        crc.update(key);
        crc.update(payload);
        header.crc = crc.finalize();
        // Each unit goes straight to the file when nothing staged covers it,
        // else through the ring.
        for (unit_at, bytes) in [
            (at, &header.encode()[..]),
            (at + CHUNK_HEADER, key),
            (at + CHUNK_HEADER + key.len() as u64, payload),
        ] {
            if bytes.is_empty() {
                continue;
            }
            if self.ring.write_unreferenced(store, unit_at, bytes)? {
                self.stats.direct_writes += 1;
            } else {
                self.ring.stage(store, unit_at, bytes)?;
            }
        }
        Ok(())
    }

    /// Marks `len` bytes at `at` of `region` free: no longer live, a hole.
    /// The chunk there keeps its header: no slot points at it any more,
    /// which is what a walk of the region checks.
    fn release(&mut self, store: &mut dyn TileStore, region: u64, at: u64, len: u64) -> Result<()> {
        self.sub_live(store, region, len)?;
        self.freed_open += len;
        let generation = self.ring.published() + 1;
        self.holes.add(Hole {
            region,
            at,
            len,
            generation,
        });
        Ok(())
    }

    /// Changes the header of the region at `at`: staged when the writer
    /// publishes.
    fn set_region(&mut self, store: &mut dyn TileStore, at: u64, header: Region) -> Result<()> {
        if let Some((out, out_header)) = self.regions.set(at, header) {
            self.ring.stage(store, out, &out_header.encode())?;
        }
        Ok(())
    }

    fn add_live(&mut self, store: &mut dyn TileStore, region: u64, len: u64) -> Result<()> {
        let mut header = self.region(store, region)?;
        header.live += len;
        self.state.live += len;
        self.set_region(store, region, header)?;
        Ok(())
    }

    fn sub_live(&mut self, store: &mut dyn TileStore, region: u64, len: u64) -> Result<()> {
        let mut header = self.region(store, region)?;
        header.live = header
            .live
            .checked_sub(len)
            .ok_or(corrupt("keyed heap region accounting"))?;
        self.state.live = self
            .state
            .live
            .checked_sub(len)
            .ok_or(corrupt("keyed heap accounting"))?;
        self.set_region(store, region, header)?;
        if region != self.state.cur {
            self.sparse.note(region, header.live, header.len);
        }
        Ok(())
    }

    // ---- space ----

    /// Finds `len` bytes for a chunk outside region `avoid`: the smallest
    /// cached hole that holds it, the current region's tail, a free region
    /// or a new one. Returns `(region, offset)`.
    fn alloc(&mut self, store: &mut dyn TileStore, len: u64, avoid: u64) -> Result<(u64, u64)> {
        if let Some(found) = self.reuse(store, len, avoid)? {
            return Ok(found);
        }
        // Space freed by generations not yet in place may not be retaken
        // (see the module notes). Once more of it waits than half the
        // free-space allowance, put those generations in place and look
        // again rather than grow the file; below that, growing is cheaper
        // than a checkpoint (two barriers) for every few records.
        if self.ring.checkpointed() == self.ring.published() {
            self.freed_waiting = 0;
        }
        if self.ring.published() > self.ring.checkpointed()
            && (self.state.free >= len || self.freed_waiting > self.allowance() / 2)
        {
            self.ring.checkpoint(store, u64::MAX)?;
            self.freed_waiting = 0;
            self.stats.checkpoints += 1;
            if let Some(found) = self.reuse(store, len, avoid)? {
                return Ok(found);
            }
        }
        if len > REGION_BYTES / 2 {
            let region = self.new_region(store, len)?;
            return Ok((region, region + REGION_HEADER));
        }
        self.close_cur(store)?;
        let region = match self.pop_free_region(store, len, avoid)? {
            Some(region) => region,
            None => self.new_region(store, REGION_BYTES)?,
        };
        self.state.cur = region;
        self.cur_len = self.region(store, region)?.len;
        self.state.cur_used = len;
        self.sparse.forget(region);
        Ok((region, region + REGION_HEADER))
    }

    /// Space that exists already: a cached hole, the current region's tail,
    /// or a free region as the new current one.
    fn reuse(
        &mut self,
        store: &mut dyn TileStore,
        len: u64,
        avoid: u64,
    ) -> Result<Option<(u64, u64)>> {
        if let Some(hole) = self
            .holes
            .take_fitting(len, avoid, self.ring.checkpointed())
        {
            self.stats.holes_reused += 1;
            if hole.len > len {
                // The rest was in place too: never staged in other units.
                self.ring
                    .stage(store, hole.at + len, &Chunk::free(hole.len - len)?.encode())?;
                self.holes.add(Hole {
                    region: hole.region,
                    at: hole.at + len,
                    len: hole.len - len,
                    generation: hole.generation,
                });
            }
            return Ok(Some((hole.region, hole.at)));
        }
        let cur = self.state.cur;
        if cur != 0 && cur != avoid && self.state.cur_used + len <= self.cur_len {
            let at = cur + REGION_HEADER + self.state.cur_used;
            self.state.cur_used += len;
            return Ok(Some((cur, at)));
        }
        if len > REGION_BYTES / 2 {
            return Ok(None);
        }
        let Some(region) = self.pop_free_region(store, len, avoid)? else {
            return Ok(None);
        };
        self.close_cur(store)?;
        self.state.cur = region;
        self.cur_len = self.region(store, region)?.len;
        self.state.cur_used = len;
        self.sparse.forget(region);
        Ok(Some((region, region + REGION_HEADER)))
    }

    /// Ends the current region: its unused tail becomes a free chunk.
    fn close_cur(&mut self, store: &mut dyn TileStore) -> Result<()> {
        let cur = self.state.cur;
        if cur == 0 {
            return Ok(());
        }
        let tail = self.cur_len - self.state.cur_used;
        if tail > 0 {
            let at = cur + REGION_HEADER + self.state.cur_used;
            self.ring.stage(store, at, &Chunk::free(tail)?.encode())?;
            // Nothing was ever staged in the tail but its free header, in
            // the unit a chunk header there uses: reusable at once.
            self.holes.add(Hole {
                region: cur,
                at,
                len: tail,
                generation: 0,
            });
        }
        self.state.cur = 0;
        self.state.cur_used = 0;
        self.cur_len = 0;
        let header = self.region(store, cur)?;
        self.sparse.note(cur, header.live, header.len);
        Ok(())
    }

    fn new_region(&mut self, store: &mut dyn TileStore, len: u64) -> Result<u64> {
        let header = Region {
            len,
            live: 0,
            next_all: self.state.regions,
            next_free: 0,
            freed: 0,
        };
        let at = self.ring.place_record(
            store,
            RecordKind::Heap,
            &header.encode(),
            REGION_HEADER + len,
        )?;
        self.state.regions = at;
        self.state.heap += len;
        self.stats.regions_appended += 1;
        Ok(at)
    }

    /// Takes a free region of at least `len` bytes from the head of the free
    /// list. Only the head is looked at: free regions are ordinary ones, and
    /// a larger record gets its own region anyway.
    fn pop_free_region(
        &mut self,
        store: &mut dyn TileStore,
        len: u64,
        avoid: u64,
    ) -> Result<Option<u64>> {
        let head = self.state.free_regions;
        if head == 0 || head == avoid {
            return Ok(None);
        }
        let mut header = self.region(store, head)?;
        if header.len < len || header.freed > self.ring.checkpointed() {
            return Ok(None);
        }
        // The last free region links to itself.
        if header.next_free == head {
            self.state.free_regions = 0;
            self.state.free_tail = 0;
        } else {
            self.state.free_regions = header.next_free;
        }
        self.state.free -= header.len;
        header.next_free = 0;
        header.live = 0;
        header.freed = 0;
        self.set_region(store, head, header)?;
        self.stats.regions_reused += 1;
        Ok(Some(head))
    }

    // ---- compaction ----

    /// Empties the sparsest regions when the holes the next puts will not
    /// take exceed the allowance.
    fn compact_if_needed(&mut self, store: &mut dyn TileStore) -> Result<()> {
        let excess = self.settled_holes().saturating_sub(self.allowance());
        if excess == 0 {
            return Ok(());
        }
        self.compact(store, excess)
    }

    /// Empties the sparsest regions until about `excess` bytes of holes
    /// are in free regions, four at most per call.
    fn compact(&mut self, store: &mut dyn TileStore, excess: u64) -> Result<()> {
        let Some(mut region) = self.sparsest(store)? else {
            return Ok(());
        };
        // Put what is published in place first: the holes and free regions
        // earlier generations left become reusable, so the moves below fill
        // them instead of growing the file.
        if self.ring.published() > self.ring.checkpointed() {
            self.ring.checkpoint(store, u64::MAX)?;
            self.freed_waiting = 0;
        }
        // Each pass empties one region; a few bound one call's work.
        let mut reclaimed = 0u64;
        for _ in 0..4 {
            let header = self.region(store, region)?;
            self.empty_region(store, region)?;
            reclaimed += header.len - header.live;
            if reclaimed >= excess {
                break;
            }
            match self.sparsest(store)? {
                Some(next) => region = next,
                None => break,
            }
        }
        Ok(())
    }

    fn sparsest(&mut self, store: &mut dyn TileStore) -> Result<Option<u64>> {
        if self.sparse.len == 0 {
            // Reopened, or every candidate was used: look at every region.
            let mut at = self.state.regions;
            while at != 0 {
                let header = self.region(store, at)?;
                if at != self.state.cur && header.next_free == 0 && at != self.state.free_regions {
                    self.sparse.note(at, header.live, header.len);
                }
                at = header.next_all;
            }
        }
        while let Some(region) = self.sparse.take() {
            if region != self.state.cur && !self.is_free_region(store, region)? {
                return Ok(Some(region));
            }
        }
        Ok(None)
    }

    fn is_free_region(&self, store: &dyn TileSource, region: u64) -> Result<bool> {
        Ok(region == self.state.free_regions || self.region(store, region)?.next_free != 0)
    }

    /// Moves every live chunk out of `region` and puts it on the free list.
    fn empty_region(&mut self, store: &mut dyn TileStore, region: u64) -> Result<()> {
        let header = self.region(store, region)?;
        let end = region + REGION_HEADER + header.len;
        let mut at = region + REGION_HEADER;
        while at < end {
            let chunk = self.chunk(store, at)?;
            let len = chunk.len();
            if len < CHUNK_HEADER || len > end - at {
                return Err(corrupt("keyed heap chunk length"));
            }
            if chunk.block_id != FREE_BLOCK
                && let Some((page, index, slot)) =
                    self.slot_pointing_at(store, &chunk, region, at)?
            {
                let (to_region, to) = self.alloc(store, len, region)?;
                self.move_chunk(store, &chunk, at, to)?;
                self.add_live(store, to_region, len)?;
                let moved = Slot {
                    route: slot.route,
                    offset: offset_in(to_region, to)?,
                    region: to_region,
                };
                self.set_slot(store, page, index, moved)?;
                self.stats.chunks_moved += 1;
            }
            at += len;
        }
        let mut header = self.region(store, region)?;
        self.state.live = self
            .state
            .live
            .checked_sub(header.live)
            .ok_or(corrupt("keyed heap accounting"))?;
        header.live = 0;
        header.freed = self.ring.published() + 1;
        // Free regions queue oldest first, so the one taken next is the one
        // most likely in place already. A free region always has a nonzero
        // link: the last links to itself.
        header.next_free = region;
        self.set_region(store, region, header)?;
        let tail = self.state.free_tail;
        if tail == 0 {
            self.state.free_regions = region;
        } else {
            let mut last = self.region(store, tail)?;
            last.next_free = region;
            self.set_region(store, tail, last)?;
        }
        self.state.free_tail = region;
        self.state.free += header.len;
        self.holes.forget_region(region);
        self.sparse.forget(region);
        self.stats.regions_compacted += 1;
        Ok(())
    }

    /// The slot that makes the chunk at `at` live: `(page, index, slot)`.
    fn slot_pointing_at(
        &self,
        store: &dyn TileSource,
        chunk: &Chunk,
        region: u64,
        at: u64,
    ) -> Result<Option<(u64, usize, Slot)>> {
        let mut hasher = Sip::new(self.fixed.seed);
        hasher.write(&chunk.block_id.to_le_bytes());
        let mut step = [0u8; STEP];
        let mut done = 0u64;
        let key_len = u64::from(chunk.key_len);
        while done < key_len {
            let n = crate::width::bounded(key_len - done, STEP);
            self.read(store, at + CHUNK_HEADER + done, &mut step[..n])?;
            hasher.write(&step[..n]);
            done += n as u64;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the low 32 bits route and fingerprint"
        )]
        let route = hasher.finish() as u32;
        let offset = offset_in(region, at)?;
        let mut page_at = self.bucket(store, self.state.bucket_of(route))?;
        let mut page = [0u8; crate::width::small(BUCKET_PAGE)];
        while page_at != 0 {
            self.read(store, page_at, &mut page)?;
            let (count, next) = page_header(&page)?;
            for index in 0..count {
                let slot = slot_at(&page, index);
                if slot.route == route && slot.region == region && slot.offset == offset {
                    return Ok(Some((page_at, index, slot)));
                }
            }
            page_at = next;
        }
        Ok(None)
    }

    /// Copies a chunk to `to` in its three units, as a put would write it.
    fn move_chunk(
        &mut self,
        store: &mut dyn TileStore,
        chunk: &Chunk,
        from: u64,
        to: u64,
    ) -> Result<()> {
        self.ring.stage(store, to, &chunk.encode())?;
        let key = u64::from(chunk.key_len);
        let payload = u64::from(chunk.payload_len);
        if key > 0 {
            self.ring
                .stage_copy(store, from + CHUNK_HEADER, to + CHUNK_HEADER, key)?;
        }
        if payload > 0 {
            self.ring.stage_copy(
                store,
                from + CHUNK_HEADER + key,
                to + CHUNK_HEADER + key,
                payload,
            )?;
        }
        Ok(())
    }

    /// Reads as this writer sees the file: its bucket pages over its ring.
    fn read(&self, store: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        self.pages.read(&self.ring, store, at, buf)
    }

    fn view(&self) -> HeapView<'_> {
        HeapView {
            ring: &self.ring,
            pages: &self.pages,
        }
    }

    /// Replaces the bucket page at `at` with `page`.
    fn put_page(
        &mut self,
        store: &mut dyn TileStore,
        at: u64,
        page: &[u8; crate::width::small(BUCKET_PAGE)],
    ) -> Result<()> {
        self.pages
            .page_new(&mut self.ring, store, at)?
            .copy_from_slice(page);
        Ok(())
    }

    fn set_slot(
        &mut self,
        store: &mut dyn TileStore,
        page: u64,
        index: usize,
        slot: Slot,
    ) -> Result<()> {
        let bytes = self.pages.page_mut(&mut self.ring, store, page)?;
        put_slot(bytes, index, &slot.encode());
        Ok(())
    }

    // ---- buckets ----

    fn insert_slot(&mut self, store: &mut dyn TileStore, bucket: u64, slot: Slot) -> Result<()> {
        let mut at = self.bucket(store, bucket)?;
        loop {
            let mut header = [0u8; crate::width::small(BUCKET_HEADER)];
            self.read(store, at, &mut header)?;
            let (count, next) = page_header(&header)?;
            if count < SLOTS_PER_PAGE {
                let page = self.pages.page_mut(&mut self.ring, store, at)?;
                put_slot(page, count, &slot.encode());
                set_page_header(page, count + 1, next);
                return Ok(());
            }
            if next == 0 {
                let page_at = self.alloc_page(store)?;
                let page = self.pages.page_new(&mut self.ring, store, page_at)?;
                set_page_header(page, 1, 0);
                put_slot(page, 0, &slot.encode());
                let page = self.pages.page_mut(&mut self.ring, store, at)?;
                set_page_header(page, count, page_at);
                return Ok(());
            }
            at = next;
        }
    }

    fn remove_slot(&mut self, store: &mut dyn TileStore, page: u64, index: usize) -> Result<()> {
        let bytes = self.pages.page_mut(&mut self.ring, store, page)?;
        let (count, next) = page_header(bytes)?;
        let last = count.checked_sub(1).ok_or(corrupt("keyed bucket page"))?;
        if index != last {
            let from = HEADER_BYTES + last * SLOT_BYTES;
            let to = HEADER_BYTES + index * SLOT_BYTES;
            bytes.copy_within(from..from + SLOT_BYTES, to);
        }
        set_page_header(bytes, last, next);
        Ok(())
    }

    /// Splits the bucket at the split pointer: slots whose next routing bit
    /// is set move to a new bucket, the rest are packed back into the old
    /// bucket's pages, and pages left over go to the free list.
    fn split(&mut self, store: &mut dyn TileStore) -> Result<()> {
        let from = self.state.split;
        let bit = 1u64 << self.state.level;
        let to = from + bit;
        let head = self.bucket(store, from)?;
        let to_head = self.alloc_page(store)?;
        self.set_bucket(store, to, to_head)?;

        let mut read = [0u8; crate::width::small(BUCKET_PAGE)];
        let mut stay = [0u8; crate::width::small(BUCKET_PAGE)];
        let mut moved = [0u8; crate::width::small(BUCKET_PAGE)];
        let (mut stay_count, mut moved_count) = (0usize, 0usize);
        // Stay pages overwrite the old chain from its head: never ahead of
        // the page being read, since they hold no more slots than were read.
        let mut stay_at = head;
        let mut moved_at = to_head;
        let mut at = head;
        while at != 0 {
            self.read(store, at, &mut read)?;
            let (count, next) = page_header(&read)?;
            for index in 0..count {
                let start = HEADER_BYTES + index * SLOT_BYTES;
                let bytes = &read[start..start + crate::width::small(SLOT_LEN)];
                if u64::from(Slot::decode(bytes).route) & bit != 0 {
                    if moved_count == SLOTS_PER_PAGE {
                        let next_page = self.alloc_page(store)?;
                        set_page_header(&mut moved, moved_count, next_page);
                        self.put_page(store, moved_at, &moved)?;
                        moved_at = next_page;
                        moved_count = 0;
                    }
                    put_slot(&mut moved, moved_count, bytes);
                    moved_count += 1;
                } else {
                    if stay_count == SLOTS_PER_PAGE {
                        let next_page = self.next_page(store, stay_at)?;
                        set_page_header(&mut stay, stay_count, next_page);
                        self.put_page(store, stay_at, &stay)?;
                        stay_at = next_page;
                        stay_count = 0;
                    }
                    put_slot(&mut stay, stay_count, bytes);
                    stay_count += 1;
                }
            }
            at = next;
        }
        let mut spare = self.next_page(store, stay_at)?;
        set_page_header(&mut stay, stay_count, 0);
        zero_slots(&mut stay, stay_count);
        self.put_page(store, stay_at, &stay)?;
        set_page_header(&mut moved, moved_count, 0);
        zero_slots(&mut moved, moved_count);
        self.put_page(store, moved_at, &moved)?;
        while spare != 0 {
            let next = self.next_page(store, spare)?;
            self.free_page(store, spare)?;
            spare = next;
        }
        self.state = self.state.after_split();
        self.stats.splits += 1;
        Ok(())
    }

    fn next_page(&self, store: &dyn TileSource, page: u64) -> Result<u64> {
        let mut header = [0u8; crate::width::small(BUCKET_HEADER)];
        self.read(store, page, &mut header)?;
        Ok(page_header(&header)?.1)
    }

    fn alloc_page(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        let head = self.state.free_pages;
        if head != 0 {
            let mut link = [0u8; 8];
            self.read(store, head + 8, &mut link)?;
            self.state.free_pages = u64::from_le_bytes(link);
            return Ok(head);
        }
        let mut header = [0u8; crate::width::small(BUCKET_HEADER)];
        set_page_header(&mut header, 0, 0);
        self.ring
            .place_record(store, RecordKind::Bucket, &header, BUCKET_PAGE)
    }

    fn free_page(&mut self, store: &mut dyn TileStore, page: u64) -> Result<()> {
        let free_pages = self.state.free_pages;
        let bytes = self.pages.page_new(&mut self.ring, store, page)?;
        set_page_header(bytes, 0, free_pages);
        self.state.free_pages = page;
        Ok(())
    }

    // ---- bucket directory ----

    /// The first page of bucket `bucket`.
    fn bucket(&self, store: &dyn TileSource, bucket: u64) -> Result<u64> {
        lookup::bucket(&mut self.view(), store, &self.state, bucket)
    }

    fn set_bucket(&mut self, store: &mut dyn TileStore, bucket: u64, first: u64) -> Result<()> {
        let (mut root, mut height) = self.state.dir;
        let bits = 64 - bucket.leading_zeros();
        let needed = u64::from(bits.div_ceil(DIR_LEVEL_BITS).max(1));
        while height < needed {
            let taller = self.new_dir_page(store)?;
            self.ring.stage(store, taller, &root.to_le_bytes())?;
            root = taller;
            height += 1;
        }
        self.state.dir = (root, height);
        let mut page = root;
        for level in (1..height).rev() {
            let index = (bucket >> (u64::from(DIR_LEVEL_BITS) * level)) & (DIR_FANOUT - 1);
            let at = page + index * 8;
            let mut entry = [0u8; 8];
            self.read(store, at, &mut entry)?;
            let mut child = directory_offset(u64::from_le_bytes(entry))?;
            if child == 0 {
                child = self.new_dir_page(store)?;
                self.ring.stage(store, at, &child.to_le_bytes())?;
            }
            page = child;
        }
        self.ring.stage(
            store,
            page + (bucket & (DIR_FANOUT - 1)) * 8,
            &first.to_le_bytes(),
        )
    }

    fn new_dir_page(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        self.ring
            .place_record(store, RecordKind::Directory, &[], DIR_PAGE_BYTES)
    }
}

fn put_slot(page: &mut [u8], index: usize, bytes: &[u8]) {
    let start = HEADER_BYTES + index * SLOT_BYTES;
    page[start..start + crate::width::small(SLOT_LEN)].copy_from_slice(bytes);
}

fn zero_slots(page: &mut [u8], from: usize) {
    let start = HEADER_BYTES + from * SLOT_BYTES;
    page[start..].fill(0);
}

fn offset_in(region: u64, at: u64) -> Result<u32> {
    u32::try_from(at - region).map_err(|_| corrupt("keyed heap chunk offset"))
}

fn field32(resource: &'static str, len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| Error::LimitExceeded {
        resource,
        actual: len as u64,
        limit: u64::from(u32::MAX),
    })
}
