//! The single writer: stages byte ranges, publishes them through the ring,
//! and checkpoints them into place when the ring needs room.

use super::ranges::Ranges;

use super::geometry::{Geometry, META_FIXED_LEN, TileCache, TileCoord};
use super::store::{TileSource, TileStore};
use super::view::{
    Overlay, OverlayEntry, directory_offset, directory_root, find_tile, for_each_tile, lay_over,
};
use super::wal::{
    self, ENTRY_HEADER, EXTERNAL_ENTRY, EXTERNAL_FRAME_LEN, ExternalDescriptor, FRAME_COMMIT,
    FRAME_HEADER, FRAME_MIN, FRAME_WRAP, FRAMES_AT, FramePlan, MIN_RING, Ring, RingHeader,
    ScanEvent, frame_header,
};
use super::{DIR_FANOUT, DIR_LEVEL_BITS, DIR_PAGE_BYTES, RecordKind};
use crate::{Error, Result};

/// Writes of ranges into place go through one buffer of this size: adjacent
/// ranges become one positional write.
pub(crate) const COALESCE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WriterStats {
    pub barriers: u64,
    pub checkpoints: u64,
    pub frames: u64,
    pub in_place_writes: u64,
    /// Overflow regions written: reused ones and appended ones.
    pub overflow_reused: u64,
    pub overflow_appended: u64,
}

/// An overflow record this writer appended: where its payload is, how long
/// it is, and the newest generation whose ranges it holds. Once that
/// generation is checkpointed and reclaimed no reader can need its bytes, and
/// the next large publication writes over it instead of growing the file.
#[derive(Clone, Copy, Debug)]
struct OverflowRegion {
    at: u64,
    len: u64,
    generation: u64,
}

pub(crate) struct TileWriter {
    pub(crate) geom: Geometry,
    pub(crate) meta: u64,
    pub(crate) ring: Ring,
    header: RingHeader,
    /// Ring offset where the next frame goes (past any frames of the open
    /// generation already written).
    cursor: u64,
    /// Published ranges not yet in place, and spilled ranges of the open
    /// generation (whose `generation` is `header.published + 1`).
    overlay: Overlay,
    /// Ranges staged since the last frame.
    pending: Ranges<Vec<u8>>,
    pending_bytes: u64,
    /// Whether the open generation has frames in the ring already.
    spilled: bool,
    /// Records appended, or written directly, since the last barrier: they
    /// must be durable before a frame that points at them.
    appended: bool,
    /// Records appended since the last publication, `start -> end`. Nothing
    /// published points at them, so no reader can reach them: bytes staged
    /// into them are written in place at once instead of through the ring,
    /// which is what lets a publication of any size, such as an initial
    /// load, never fill the ring.
    fresh: Vec<(u64, u64)>,
    /// The end of the furthest matrix record this writer appended: what a
    /// frame it writes may point into, so what the frame `requires`.
    requires: u64,
    tiles: TileCache,
    /// Ring bytes the open generation's frames take: past half the ring the
    /// rest of the publication streams into overflow regions.
    open_ring_bytes: u64,
    /// Overflow records appended by this writer, reusable once reclaimed.
    overflow: Vec<OverflowRegion>,
    pub(crate) stats: WriterStats,
}

impl TileWriter {
    /// Lays out a new matrix: the metadata record, then a ring of `ring_len`
    /// bytes. Returns the writer; the metadata payload offset is `meta`.
    pub(crate) fn create(
        store: &mut dyn TileStore,
        geom: Geometry,
        ring_len: u64,
        creation_id: [u8; 16],
    ) -> Result<Self> {
        let ring_len = ring_len.max(MIN_RING).next_multiple_of(8);
        let meta_len = geom.meta_len();
        let mut meta_bytes = zeroed(meta_len, "matrix metadata")?;
        // The ring record follows the metadata record; its payload offset is
        // only known once appended, so the metadata is appended with a
        // placeholder and its fixed part rewritten.
        geom.encode_meta((0, ring_len), creation_id, &mut meta_bytes);
        let meta = store.append(RecordKind::Meta, &meta_bytes)?;
        let mut ring_bytes = zeroed(ring_len, "matrix ring")?;
        let header = RingHeader {
            seq: 1,
            ..RingHeader::default()
        };
        ring_bytes
            [HEADER_SLOT_FOR_SEQ1..HEADER_SLOT_FOR_SEQ1 + crate::width::small(wal::HEADER_SLOT)]
            .copy_from_slice(&header.encode());
        let base = store.append(RecordKind::Ring, &ring_bytes)?;
        geom.encode_meta((base, ring_len), creation_id, &mut meta_bytes);
        store.write_at(meta, &meta_bytes[..crate::width::small(META_FIXED_LEN)])?;
        store.sync()?;
        Self::with_parts(
            geom,
            meta,
            Ring {
                base,
                len: ring_len,
            },
            header,
        )
    }

    /// Opens an existing matrix for writing. Publications the ring still
    /// holds are adopted as an overlay over the tiles, not written in place;
    /// a publication whose frames are torn or uncommitted never happened. The
    /// ring header is rewritten only when the frames show a newer publication
    /// than the header records.
    pub(crate) fn open(store: &mut dyn TileStore, geom: Geometry, meta: u64) -> Result<Self> {
        let mut fixed = [0u8; crate::width::small(META_FIXED_LEN)];
        store.read_at(meta, &mut fixed)?;
        let (base, len) = geom.decode_meta(&fixed)?;
        let ring = Ring::within(base, len, store)?;
        let header = ring.read_header(store)?;
        let mut writer = Self::with_parts(geom, meta, ring, header)?;
        writer.recover(store)?;
        Ok(writer)
    }

    fn with_parts(geom: Geometry, meta: u64, ring: Ring, header: RingHeader) -> Result<Self> {
        let tiles = TileCache::for_geometry(&geom)?;
        Ok(Self {
            geom,
            meta,
            ring,
            cursor: header.head,
            header,
            overlay: Overlay::default(),
            pending: Ranges::new("matrix staged ranges"),
            pending_bytes: 0,
            spilled: false,
            appended: false,
            fresh: Vec::new(),
            requires: 0,
            tiles,
            open_ring_bytes: 0,
            overflow: Vec::new(),
            stats: WriterStats::default(),
        })
    }

    /// The newest published generation.
    pub(crate) const fn published(&self) -> u64 {
        self.header.published
    }

    /// Whether anything is staged or spilled and not yet published.
    pub(crate) fn is_dirty(&self) -> bool {
        !self.pending.is_empty() || self.spilled
    }

    // ---- reading this writer's own state ----

    /// Reads `buf.len()` bytes at `offset` as this writer sees them: staged
    /// ranges over published ones over the file.
    pub(crate) fn read(&self, store: &dyn TileSource, offset: u64, buf: &mut [u8]) -> Result<()> {
        store.read_at(offset, buf)?;
        self.overlay.lay(store, offset, buf, &|_| false)?;
        self.pending
            .for_each_overlapping(offset, buf.len() as u64, &mut |target, bytes| {
                lay_over(offset, buf, target, bytes);
                Ok(())
            })
    }

    // ---- staging ----

    /// Stages `bytes` to be written at `target` by the next publication. A
    /// later stage of the same range replaces it.
    pub(crate) fn stage(
        &mut self,
        store: &mut dyn TileStore,
        target: u64,
        bytes: &[u8],
    ) -> Result<()> {
        if self.is_fresh(target, bytes.len() as u64) {
            store.write_at(target, bytes)?;
            self.appended = true;
            return Ok(());
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(bytes.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix staged range",
                requested: bytes.len() as u64,
            })?;
        owned.extend_from_slice(bytes);
        if let Some(old) = self.pending.insert(target, owned)? {
            self.pending_bytes -= old.len() as u64;
        }
        self.pending_bytes += bytes.len() as u64;
        // A publication bigger than this goes to the ring in several frames
        // instead of growing in memory.
        if self.pending_bytes > self.spill_threshold() {
            self.spill(store, false)?;
        }
        Ok(())
    }

    /// Whether `[target, target + len)` lies inside a record appended since
    /// the last publication.
    fn is_fresh(&self, target: u64, len: u64) -> bool {
        // Appended in order, so sorted by start.
        let at = self.fresh.partition_point(|(start, _)| *start <= target);
        at > 0 && target + len <= self.fresh[at - 1].1
    }

    fn note_fresh(&mut self, start: u64, end: u64) -> Result<()> {
        self.fresh
            .try_reserve(1)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix fresh records",
                requested: 16,
            })?;
        self.fresh.push((start, end));
        self.requires = self.requires.max(end);
        Ok(())
    }

    fn spill_threshold(&self) -> u64 {
        (self.ring.area() / 8).clamp(FRAME_MIN, 4 << 20)
    }

    /// Forgets everything staged or spilled since the last publication.
    #[cfg(test)]
    pub(crate) fn discard(&mut self) {
        self.pending.clear();
        self.pending_bytes = 0;
        if self.spilled {
            let open = self.header.published + 1;
            self.overlay
                .ranges
                .retain(|_, entry| entry.generation != open);
            self.spilled = false;
        }
        self.cursor = self.header.head;
        self.open_ring_bytes = 0;
        self.tiles.clear();
        self.fresh.clear();
    }

    // ---- tiles ----

    /// The payload offset of `block`'s tile `coord`, appending it (and the
    /// directory pages it needs) when `create` and it does not exist yet.
    pub(crate) fn tile(
        &mut self,
        store: &mut dyn TileStore,
        block: usize,
        coord: TileCoord,
        create: bool,
    ) -> Result<Option<u64>> {
        let key = coord.morton()?;
        if let Some(found) = self.tiles.get(block, key) {
            return Ok(Some(found));
        }
        let root_at = self.meta + self.geom.root_at(block);
        let found = find_tile(root_at, coord, &mut |at, buf| self.read(store, at, buf))?;
        let found = match found {
            Some(found) => found,
            None if !create => return Ok(None),
            None => self.allocate_tile(store, block, coord, key)?,
        };
        self.tiles.insert(block, key, found);
        Ok(Some(found))
    }

    fn allocate_tile(
        &mut self,
        store: &mut dyn TileStore,
        block: usize,
        coord: TileCoord,
        key: u64,
    ) -> Result<u64> {
        let root_at = self.meta + self.geom.root_at(block);
        let mut root = [0u8; 16];
        self.read(store, root_at, &mut root)?;
        let (mut page, mut height) = directory_root(&root)?;
        let needed = levels_for(key);
        if page == 0 {
            page = self.append_page(store)?;
            height = needed;
        }
        while height < needed {
            // A taller root whose first entry is the old one: every existing
            // key keeps its path.
            let taller = self.append_page(store)?;
            self.stage(store, taller, &page.to_le_bytes())?;
            page = taller;
            height += 1;
        }
        let mut new_root = [0u8; 16];
        new_root[..8].copy_from_slice(&page.to_le_bytes());
        new_root[8..].copy_from_slice(&height.to_le_bytes());
        if new_root != root {
            self.stage(store, root_at, &new_root)?;
        }
        for level in (1..height).rev() {
            let index = (key >> (u64::from(DIR_LEVEL_BITS) * level)) & (DIR_FANOUT - 1);
            let at = page + index * 8;
            let mut entry = [0u8; 8];
            self.read(store, at, &mut entry)?;
            let mut child = directory_offset(u64::from_le_bytes(entry))?;
            if child == 0 {
                child = self.append_page(store)?;
                self.stage(store, at, &child.to_le_bytes())?;
            }
            page = child;
        }
        let geom = self.geom.blocks[block];
        let mut tile = zeroed(geom.tile_bytes(), "matrix tile")?;
        geom.encode_empty_tile(coord, &mut tile);
        let tile_at = store.append(RecordKind::Tile, &tile)?;
        self.appended = true;
        self.note_fresh(tile_at, tile_at + geom.tile_bytes())?;
        let at = page + (key & (DIR_FANOUT - 1)) * 8;
        self.stage(store, at, &tile_at.to_le_bytes())?;
        Ok(tile_at)
    }

    fn append_page(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        let page = zeroed(DIR_PAGE_BYTES, "matrix directory page")?;
        self.appended = true;
        let at = store.append(RecordKind::Directory, &page)?;
        self.note_fresh(at, at + DIR_PAGE_BYTES)?;
        Ok(at)
    }

    /// The payload offset of `block`'s tile `coord` if it exists, without
    /// caching the answer (for `&self` reads).
    pub(crate) fn find(
        &self,
        store: &dyn TileSource,
        block: usize,
        coord: TileCoord,
    ) -> Result<Option<u64>> {
        if let Some(found) = self.tiles.get(block, coord.morton()?) {
            return Ok(Some(found));
        }
        find_tile(
            self.meta + self.geom.root_at(block),
            coord,
            &mut |at, buf| self.read(store, at, buf),
        )
    }

    /// [`Self::for_each_tile`] for `&self` reads.
    pub(crate) fn for_each_tile_ref(
        &self,
        store: &dyn TileSource,
        block: usize,
        visit: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<()> {
        for_each_tile(
            self.meta + self.geom.root_at(block),
            &mut |at, buf| self.read(store, at, buf),
            visit,
        )
    }

    /// Calls `visit` with every allocated tile of `block`, as this writer
    /// sees the directory.
    pub(crate) fn for_each_tile(
        &mut self,
        store: &dyn TileSource,
        block: usize,
        visit: &mut dyn FnMut(u64) -> Result<()>,
    ) -> Result<()> {
        let root_at = self.meta + self.geom.root_at(block);
        for_each_tile(root_at, &mut |at, buf| self.read(store, at, buf), visit)
    }

    /// Calls `visit` with every published range not yet in place.
    #[cfg(feature = "mmap")]
    pub(crate) fn for_each_overlay(
        &self,
        visit: &mut dyn FnMut(u64, OverlayEntry) -> Result<()>,
    ) -> Result<()> {
        let open = self.header.published + 1;
        for (target, entry) in self.overlay.ranges.iter() {
            if entry.generation != open {
                visit(target, *entry)?;
            }
        }
        Ok(())
    }

    // ---- publication ----

    /// Publishes everything staged as the next generation and returns it.
    /// One barrier, two when this generation appended records (which must be
    /// durable before a frame that points at them).
    pub(crate) fn publish(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        if !self.is_dirty() {
            return Ok(self.header.published);
        }
        self.spill(store, true)?;
        store.sync()?;
        self.stats.barriers += 1;
        let generation = self.header.published + 1;
        let mut header = self.header;
        header.published = generation;
        header.head = self.cursor;
        self.write_header(store, header)?;
        self.spilled = false;
        self.open_ring_bytes = 0;
        // Published now: later changes to them must not reach a reader early.
        self.fresh.clear();
        Ok(generation)
    }

    /// Writes the staged ranges to the ring as a frame of the open
    /// generation, the commit frame when `commit`.
    fn spill(&mut self, store: &mut dyn TileStore, commit: bool) -> Result<()> {
        // A publication that would take more than half the ring, or a range
        // no frame can hold, streams the rest of its ranges outside the ring.
        let largest = self
            .pending
            .iter()
            .map(|(_, bytes)| bytes.len() as u64)
            .max();
        let frame_room =
            (self.ring.area() / 4).max(FRAME_MIN * 2) - FRAME_HEADER - ENTRY_HEADER - 12;
        let ring_need =
            self.pending_bytes + self.pending.len() as u64 * ENTRY_HEADER + FRAME_HEADER;
        if self.open_ring_bytes + ring_need > self.ring.area() / 2
            || largest.is_some_and(|len| len > frame_room)
        {
            return self.spill_external(store, commit);
        }
        // Everything a commit frame points at, appended records and overflow
        // regions alike, is durable before the commit frame is written. A
        // frame that is not the commit needs no barrier: if a crash leaves
        // it without its commit, the generation never happened.
        if self.appended && commit {
            store.sync()?;
            self.stats.barriers += 1;
            self.appended = false;
        }
        let generation = self.header.published + 1;
        // The matrix records these frames may point into: a recovery that
        // cuts the file short of them drops the generation. Records that are
        // not the matrix's own (a user record left uncommitted) do not count.
        let requires = self.requires;
        let refused = |requested: u64| Error::AllocationFailed {
            resource: "matrix ring frame",
            requested,
        };
        // The staged ranges stay staged until every frame holding them is
        // written: a refusal part way leaves them all still staged, and the
        // frames already written belong to a generation nothing committed.
        let mut targets: Vec<u64> = Vec::new();
        targets
            .try_reserve_exact(self.pending.len())
            .map_err(|_| refused(self.pending.len() as u64 * 8))?;
        targets.extend(self.pending.iter().map(|(target, _)| target));
        targets.sort_unstable();
        // Entries go out in frames no larger than a quarter of the ring, so
        // a checkpoint can always make room for the next one.
        let limit = (self.ring.area() / 4).max(FRAME_MIN * 2);
        let mut frame: Vec<u8> = Vec::new();
        frame
            .try_reserve(crate::width::small(FRAME_HEADER) + 12)
            .map_err(|_| refused(FRAME_HEADER + 12))?;
        let mut entries: Vec<(u64, u64, u32)> = Vec::new();
        let mut next = 0usize;
        loop {
            frame.clear();
            entries.clear();
            frame.extend_from_slice(&[0u8; crate::width::small(FRAME_HEADER)]);
            while let Some(target) = targets.get(next).copied() {
                let bytes = self.pending.get(target).expect("listed above");
                let need = ENTRY_HEADER + bytes.len() as u64;
                if frame.len() as u64 + need + 4 + 8 > limit && !entries.is_empty() {
                    break;
                }
                if FRAME_HEADER + need + 4 + 8 > limit {
                    return Err(Error::LimitExceeded {
                        resource: "matrix ring frame",
                        actual: need,
                        limit,
                    });
                }
                // Room for this entry, and for the padding and checksum
                // that close the frame.
                frame
                    .try_reserve(crate::width::index(need)? + 12)
                    .map_err(|_| refused(need + 12))?;
                entries.try_reserve(1).map_err(|_| refused(24))?;
                let bytes_at = frame.len() as u64 + ENTRY_HEADER;
                frame.extend_from_slice(&target.to_le_bytes());
                let bytes_len = crate::width::field32(bytes.len())?;
                frame.extend_from_slice(&bytes_len.to_le_bytes());
                frame.extend_from_slice(bytes);
                entries.push((target, bytes_at, bytes_len));
                next += 1;
            }
            let last = next == targets.len();
            if entries.is_empty() && !(last && commit) {
                break;
            }
            let len = (frame.len() as u64 + 4).next_multiple_of(8);
            frame.resize(crate::width::index(len)? - 4, 0);
            let flags = if last && commit { FRAME_COMMIT } else { 0 };
            frame[..crate::width::small(FRAME_HEADER)].copy_from_slice(&frame_header(
                flags,
                generation,
                len,
                crate::width::field32(entries.len())?,
                requires,
            ));
            let crc = crc32fast::hash(&frame);
            frame.extend_from_slice(&crc.to_le_bytes());
            let at = self.place(store, len)?;
            store.write_at(self.ring.at(at), &frame)?;
            self.stats.frames += 1;
            self.open_ring_bytes += len;
            for (target, bytes_at, n) in entries.drain(..) {
                self.overlay.insert(
                    target,
                    OverlayEntry {
                        generation,
                        at: self.ring.at(at) + bytes_at,
                        len: n,
                    },
                )?;
            }
            self.cursor = at + len;
            if self.cursor == self.ring.area() {
                self.cursor = 0;
            }
            self.spilled = true;
            if last {
                break;
            }
        }
        self.pending.clear();
        self.pending_bytes = 0;
        Ok(())
    }

    /// Writes every staged range to an overflow region, the bytes followed
    /// by a table of them, and one external frame pointing at it: the commit
    /// frame when `commit`. The ring takes one small frame however much is
    /// staged, so a publication of any size streams through it.
    fn spill_external(&mut self, store: &mut dyn TileStore, commit: bool) -> Result<()> {
        let generation = self.header.published + 1;
        let refused = |requested: u64| Error::AllocationFailed {
            resource: "matrix overflow region",
            requested,
        };
        let mut targets: Vec<u64> = Vec::new();
        targets
            .try_reserve_exact(self.pending.len())
            .map_err(|_| refused(self.pending.len() as u64 * 8))?;
        targets.extend(self.pending.iter().map(|(target, _)| target));
        targets.sort_unstable();
        let count = crate::width::field32(targets.len())?;
        let data_len = self.pending_bytes;
        let table_len = u64::from(count) * EXTERNAL_ENTRY;
        let table_len_field = u32::try_from(table_len).map_err(|_| Error::LimitExceeded {
            resource: "matrix overflow table",
            actual: table_len,
            limit: u64::from(u32::MAX),
        })?;
        let total = data_len + table_len;
        let mut region: Vec<u8> = Vec::new();
        region
            .try_reserve_exact(crate::width::index(total)?)
            .map_err(|_| refused(total))?;
        let mut table: Vec<u8> = Vec::new();
        table
            .try_reserve_exact(crate::width::index(table_len)?)
            .map_err(|_| refused(table_len))?;
        let mut entries: Vec<(u64, u64, u32)> = Vec::new();
        entries
            .try_reserve_exact(targets.len())
            .map_err(|_| refused(targets.len() as u64 * 24))?;
        for target in targets {
            let bytes = self.pending.get(target).expect("listed above");
            let len = crate::width::field32(bytes.len())?;
            let offset = region.len() as u64;
            table.extend_from_slice(&wal::encode_external_entry(target, len, offset));
            region.extend_from_slice(bytes);
            entries.push((target, offset, len));
        }
        region.extend_from_slice(&table);
        let table_crc = crc32fast::hash(&table);
        let base = self.overflow_write(store, &mut region, generation)?;
        self.appended = true;
        // The region is complete on disk before a commit frame names it.
        if commit {
            store.sync()?;
            self.stats.barriers += 1;
            self.appended = false;
        }
        let descriptor = ExternalDescriptor {
            base,
            data_len,
            table_crc,
            table_len: table_len_field,
        };
        let flags = if commit { FRAME_COMMIT } else { 0 };
        let frame = wal::external_frame(flags, generation, count, self.requires, &descriptor);
        let at = self.place(store, EXTERNAL_FRAME_LEN)?;
        store.write_at(self.ring.at(at), &frame)?;
        self.stats.frames += 1;
        self.open_ring_bytes += EXTERNAL_FRAME_LEN;
        for (target, offset, len) in entries {
            self.overlay.insert(
                target,
                OverlayEntry {
                    generation,
                    at: base + offset,
                    len,
                },
            )?;
        }
        self.cursor = at + EXTERNAL_FRAME_LEN;
        if self.cursor == self.ring.area() {
            self.cursor = 0;
        }
        self.spilled = true;
        self.pending.clear();
        self.pending_bytes = 0;
        Ok(())
    }

    /// Writes `bytes` as the overflow region of `generation` and returns its
    /// logical offset: over a region this writer appended before, when one
    /// is large enough and every generation in it is reclaimed (no reader can
    /// still read it), else as a new overflow record. A region waiting only
    /// for a checkpoint is freed by one when it would avoid growing the file.
    fn overflow_write(
        &mut self,
        store: &mut dyn TileStore,
        bytes: &mut Vec<u8>,
        generation: u64,
    ) -> Result<u64> {
        let len = bytes.len() as u64;
        let mut reusable = self.reusable_overflow(len);
        if reusable.is_none()
            && self.overflow.iter().any(|region| {
                region.len >= len
                    && region.generation <= self.header.published
                    && region.generation > self.header.reclaimed
            })
        {
            self.checkpoint(store, u64::MAX)?;
            reusable = self.reusable_overflow(len);
        }
        if let Some(index) = reusable {
            let region = &mut self.overflow[index];
            region.generation = generation;
            let at = region.at;
            store.write_at(at, bytes)?;
            self.stats.overflow_reused += 1;
            return Ok(at);
        }
        self.overflow
            .try_reserve(1)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix overflow regions",
                requested: (self.overflow.len() as u64 + 1) * 24,
            })?;
        // Whole mebibytes, so the slightly different sizes of later spills
        // fit the same regions.
        let capacity = len.next_multiple_of(1 << 20);
        bytes
            .try_reserve_exact(crate::width::index(capacity - len)?)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix overflow region",
                requested: capacity,
            })?;
        bytes.resize(crate::width::index(capacity)?, 0);
        let at = store.append(RecordKind::Overflow, bytes)?;
        self.requires = self.requires.max(at + capacity);
        self.overflow.push(OverflowRegion {
            at,
            len: capacity,
            generation,
        });
        self.stats.overflow_appended += 1;
        Ok(at)
    }

    /// The smallest reclaimed overflow region that holds `len` bytes.
    fn reusable_overflow(&self, len: u64) -> Option<usize> {
        self.overflow
            .iter()
            .enumerate()
            .filter(|(_, region)| region.len >= len && region.generation <= self.header.reclaimed)
            .min_by_key(|(_, region)| region.len)
            .map(|(index, _)| index)
    }

    /// Finds ring room for a frame of `len` bytes at the cursor, writing a
    /// wrap marker or checkpointing as needed. Returns its ring offset.
    fn place(&mut self, store: &mut dyn TileStore, len: u64) -> Result<u64> {
        for _ in 0..4 {
            match self.ring.plan_frame(self.header.tail, self.cursor, len) {
                FramePlan::AtCursor => return Ok(self.cursor),
                FramePlan::AtStart => {
                    self.wrap(store)?;
                    return Ok(0);
                }
                FramePlan::Restart => {
                    let mut header = self.header;
                    header.tail = 0;
                    header.head = 0;
                    self.write_header(store, header)?;
                    self.cursor = 0;
                }
                FramePlan::Checkpoint => self.checkpoint(store, len)?,
                FramePlan::TooLarge => break,
            }
        }
        Err(Error::LimitExceeded {
            resource: "matrix ring",
            actual: len,
            limit: self.ring.area(),
        })
    }

    fn wrap(&mut self, store: &mut dyn TileStore) -> Result<()> {
        let remaining = self.ring.area() - self.cursor;
        if remaining >= FRAME_MIN {
            let generation = self.header.published + 1;
            let mut marker = [0u8; crate::width::small(FRAME_MIN)];
            marker[..crate::width::small(FRAME_HEADER)]
                .copy_from_slice(&frame_header(FRAME_WRAP, generation, FRAME_MIN, 0, 0));
            let crc = crc32fast::hash(&marker[..crate::width::small(FRAME_MIN) - 4]);
            marker[crate::width::small(FRAME_MIN) - 4..].copy_from_slice(&crc.to_le_bytes());
            store.write_at(self.ring.at(self.cursor), &marker)?;
        }
        self.cursor = 0;
        Ok(())
    }

    // ---- checkpoint ----

    /// Puts the oldest published generations in place until at least `need`
    /// contiguous ring bytes and half the ring are free, or all of them when
    /// `need` is `u64::MAX`.
    pub(crate) fn checkpoint(&mut self, store: &mut dyn TileStore, need: u64) -> Result<()> {
        let header = self.header;
        if header.published == header.checkpointed {
            return Ok(());
        }
        // Which generations to put in place: the fewest, oldest ones that
        // leave room. Readers whose snapshots are newer keep reading them.
        let mut ends: Vec<(u64, u64)> = Vec::new();
        wal::scan(
            store,
            self.ring,
            header.tail,
            header.checkpointed + 1,
            header.published,
            &mut |event| {
                if let ScanEvent::Committed(generation, end) = event {
                    ends.try_reserve(1).map_err(|_| Error::AllocationFailed {
                        resource: "matrix checkpoint plan",
                        requested: (ends.len() as u64 + 1) * 16,
                    })?;
                    ends.push((generation, end));
                }
                Ok(())
            },
        )?;
        let Some(&(last_gen, _)) = ends.last() else {
            return Err(Error::InvalidMatrixLayout);
        };
        if last_gen != header.published {
            return Err(Error::InvalidMatrixLayout);
        }
        let area = self.ring.area();
        let target = if need == u64::MAX {
            header.published
        } else {
            // The largest frame that fits once frames before `tail` are gone,
            // by the same rule `place` then applies.
            let after_cursor = |tail: u64| -> u64 {
                let (at_cursor, at_start) = self.ring.room(tail, self.cursor);
                at_cursor.max(at_start)
            };
            // Free half the ring at least, not just this frame's room: a
            // ring kept just full would checkpoint (two more syncs) on
            // every publish. Half spreads that cost over many publishes
            // while keeping the newer half for readers behind.
            let free_total = |tail: u64| area - self.ring.used(tail, self.cursor);
            ends.iter()
                .find(|(_, end)| {
                    (after_cursor(*end) >= need && free_total(*end) >= area / 2)
                        || *end == self.cursor
                })
                .map_or(header.published, |(generation, _)| *generation)
        };
        let new_tail = ends
            .iter()
            .find(|(generation, _)| *generation == target)
            .map(|(_, end)| *end)
            .expect("target is a scanned generation");

        // The newest bytes of each range among the generations being put in
        // place.
        // `(target, order, ring offset, len)`; the newest of each target wins.
        let mut latest: Vec<(u64, u64, u64, u32)> = Vec::new();
        wal::scan(
            store,
            self.ring,
            header.tail,
            header.checkpointed + 1,
            target,
            &mut |event| {
                if let ScanEvent::Entry(_, entry) = event {
                    latest.try_reserve(1).map_err(|_| Error::AllocationFailed {
                        resource: "matrix checkpoint ranges",
                        requested: (latest.len() as u64 + 1) * 32,
                    })?;
                    let order = latest.len() as u64;
                    latest.push((entry.target, order, entry.at, entry.len));
                }
                Ok(())
            },
        )?;

        // Readers must see the mark before anything in place changes.
        let mut marked = self.header;
        marked.mark = target;
        self.write_header(store, marked)?;
        latest.sort_unstable_by_key(|(target, order, ..)| (*target, *order));
        // Keep the last of each target's run.
        let mut kept = 0;
        for index in 0..latest.len() {
            let is_last = latest
                .get(index + 1)
                .is_none_or(|next| next.0 != latest[index].0);
            if is_last {
                latest[kept] = latest[index];
                kept += 1;
            }
        }
        latest.truncate(kept);
        self.apply(store, &latest)?;
        store.sync()?;
        self.stats.barriers += 1;

        let mut done = self.header;
        done.checkpointed = target;
        done.reclaimed = target;
        done.tail = new_tail;
        done.tail_gen = target + 1;
        self.write_header(store, done)?;
        // The new tail must be durable before its old space is reused, or a
        // crash could leave a header pointing at overwritten frames.
        store.sync()?;
        self.stats.barriers += 1;
        self.stats.checkpoints += 1;
        self.overlay
            .ranges
            .retain(|_, entry| entry.generation > target);
        Ok(())
    }

    /// Writes ranges into place through the coalescing buffer.
    fn apply(&mut self, store: &mut dyn TileStore, ranges: &[(u64, u64, u64, u32)]) -> Result<()> {
        let mut buffer: Vec<u8> = Vec::new();
        buffer
            .try_reserve_exact(COALESCE_BYTES)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix checkpoint buffer",
                requested: COALESCE_BYTES as u64,
            })?;
        let mut buffer_at = 0u64;
        let mut chunk = [0u8; 4096];
        for (target, _, at, len) in ranges {
            let len = u64::from(*len);
            if !buffer.is_empty() && buffer_at + buffer.len() as u64 != *target {
                store.write_at(buffer_at, &buffer)?;
                self.stats.in_place_writes += 1;
                buffer.clear();
            }
            if buffer.is_empty() {
                buffer_at = *target;
            }
            let mut done = 0u64;
            while done < len {
                let n = crate::width::index((len - done).min(chunk.len() as u64))?;
                store.read_at(at + done, &mut chunk[..n])?;
                let mut piece = &chunk[..n];
                while !piece.is_empty() {
                    let room = COALESCE_BYTES - buffer.len();
                    let take = room.min(piece.len());
                    buffer.extend_from_slice(&piece[..take]);
                    piece = &piece[take..];
                    if buffer.len() == COALESCE_BYTES {
                        store.write_at(buffer_at, &buffer)?;
                        self.stats.in_place_writes += 1;
                        buffer_at += buffer.len() as u64;
                        buffer.clear();
                    }
                }
                done += n as u64;
            }
        }
        if !buffer.is_empty() {
            store.write_at(buffer_at, &buffer)?;
            self.stats.in_place_writes += 1;
        }
        Ok(())
    }

    /// Puts every published generation in place and empties the ring.
    #[cfg(test)]
    pub(crate) fn checkpoint_all(&mut self, store: &mut dyn TileStore) -> Result<()> {
        self.checkpoint(store, u64::MAX)
    }

    /// Finds what the ring holds: every committed publication after the
    /// last checkpoint becomes this writer's overlay, as a reader's would be.
    /// Nothing is written in place, so a reader's snapshot stays readable
    /// across a writer reopen; frames of a publication that never committed
    /// are simply overwritten by the next one.
    fn recover(&mut self, store: &mut dyn TileStore) -> Result<()> {
        let header = self.header;
        let mut overlay = Overlay::default();
        let end = wal::scan(
            store,
            self.ring,
            header.tail,
            header.checkpointed + 1,
            u64::MAX,
            &mut |event| match event {
                ScanEvent::Entry(generation, entry) => overlay.insert(
                    entry.target,
                    OverlayEntry {
                        generation,
                        at: entry.at,
                        len: entry.len,
                    },
                ),
                ScanEvent::Committed(..) => Ok(()),
            },
        )?;
        self.overlay = overlay;
        // The frames are the authority on what was published; a header
        // written after them may not have reached the disk.
        let mut found = header;
        found.published = end.published.max(header.checkpointed);
        found.head = if end.published > header.checkpointed {
            end.head
        } else {
            header.tail
        };
        self.cursor = found.head;
        if found != header {
            self.write_header(store, found)?;
        }
        Ok(())
    }

    fn write_header(&mut self, store: &mut dyn TileStore, mut header: RingHeader) -> Result<()> {
        header.seq = self.header.seq + 1;
        store.write_at(self.ring.slot_for(header.seq), &header.encode())?;
        self.header = header;
        Ok(())
    }
}

/// Where the first header goes in a new ring payload: the slot for sequence 1.
const HEADER_SLOT_FOR_SEQ1: usize = crate::width::small(wal::HEADER_SLOT);

/// Directory levels a Morton key needs.
fn levels_for(key: u64) -> u64 {
    let bits = 64 - key.leading_zeros();
    u64::from(bits.div_ceil(DIR_LEVEL_BITS).max(1))
}

fn zeroed(len: u64, resource: &'static str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(crate::width::index(len)?)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested: len,
        })?;
    bytes.resize(crate::width::index(len)?, 0);
    Ok(bytes)
}

#[allow(dead_code)]
const _: () = assert!(FRAMES_AT >= 2 * wal::HEADER_SLOT);
