//! The in-place storage engine of the single writer: stages byte ranges,
//! publishes them through the write-ahead ring, checkpoints them into place
//! when the ring needs room, and places records in overflow space. It knows
//! nothing of what the bytes mean: a matrix ([`super::writer::TileWriter`])
//! and keyed storage both own one.
//!
//! Every byte staged goes from the caller's buffer straight to the file: into
//! a record nothing published points at yet, into a ring frame, or, for a
//! publication too large for the ring, into overflow space recorded by an
//! on-disk index. The writer's memory is allocated when it is opened (the
//! overlay, the index page cache, two scan and copy buffers) and never grows:
//! staging, publishing and checkpointing allocate nothing.

use std::sync::Mutex;

use super::RecordKind;
use super::index::{self, Building, Io, PAGE, PageCache};
use super::store::{TileSource, TileStore};
use super::view::{Layers, MAX_INDEXES, OverlayEntry, scan_buffer};
use super::wal::{
    self, ENTRY_HEADER, FRAME_COMMIT, FRAME_HEADER, FRAME_MIN, FRAME_WRAP, FRAMES_AT, FramePlan,
    INDEX_FRAME_LEN, MIN_RING, Ring, RingHeader, ScanEvent, frame_header,
};
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
    /// Overflow regions taken: reused ones and appended ones.
    pub overflow_reused: u64,
    pub overflow_appended: u64,
    /// Tiles and directory pages placed in free overflow space instead of
    /// appended.
    pub carved: u64,
    /// Publications that streamed past the ring into an index.
    pub indexed: u64,
}

/// The header at the start of every overflow region's payload.
///
/// | Bytes | Field |
/// | --- | --- |
/// | 0..4 | magic `b"VOVF"` |
/// | 4..8 | zero |
/// | 8..16 | payload offset of the region appended before this one, `0` for none |
/// | 16..24 | payload length of this region, header included |
/// | 24..32 | bytes at the end of the region given to tiles and directory pages |
/// | 32..36 | CRC32 of bytes 0..32 |
/// | 36..40 | zero |
///
/// The first two fields never change. The carved count changes through the
/// ring, with the publication whose directory first points into the space.
pub(crate) const OVERFLOW_HEADER: u64 = 40;
const OVERFLOW_MAGIC: &[u8; 4] = b"VOVF";
/// Overflow regions a writer keeps track of; regions past this are left
/// alone (they cost space, never correctness).
const MAX_REGIONS: usize = 64;
/// Regions are whole multiples of this, and at least this long.
const REGION_UNIT: u64 = 1 << 20;

fn encode_overflow_header(next: u64, len: u64, carved: u64) -> [u8; 40] {
    let mut out = [0u8; 40];
    out[..4].copy_from_slice(OVERFLOW_MAGIC);
    out[8..16].copy_from_slice(&next.to_le_bytes());
    out[16..24].copy_from_slice(&len.to_le_bytes());
    out[24..32].copy_from_slice(&carved.to_le_bytes());
    let crc = crc32fast::hash(&out[..32]);
    out[32..36].copy_from_slice(&crc.to_le_bytes());
    out
}

/// `(next, len, carved)` of a well-formed header.
fn decode_overflow_header(bytes: &[u8; 40]) -> Option<(u64, u64, u64)> {
    let field = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight"));
    let crc = u32::from_le_bytes(bytes[32..36].try_into().expect("four"));
    if &bytes[..4] != OVERFLOW_MAGIC || crc32fast::hash(&bytes[..32]) != crc {
        return None;
    }
    let (next, len, carved) = (field(8), field(16), field(24));
    (len >= OVERFLOW_HEADER && carved <= len - OVERFLOW_HEADER && carved % 8 == 0)
        .then_some((next, len, carved))
}

/// Zeros, written from here instead of from a buffer.
pub(crate) static ZEROS: [u8; 64 * 1024] = [0; 64 * 1024];

/// An overflow record: where its payload is, how long it is, the newest
/// generation whose index data it holds, and how many bytes at its end hold
/// tiles and directory pages. Once that generation is checkpointed and
/// reclaimed no reader can need its data: a later large publication writes
/// over it, and a new tile or directory page that fits is placed at the end
/// of the free space instead of being appended.
#[derive(Clone, Copy, Debug, Default)]
struct OverflowRegion {
    at: u64,
    len: u64,
    generation: u64,
    carved: u64,
    /// `carved` as of the last publication: space carved since is fresh.
    carved_published: u64,
    /// The region appended before this one, as its header records.
    next: u64,
}

impl OverflowRegion {
    /// Bytes between the header and the carved space.
    const fn free_room(&self) -> u64 {
        self.len - OVERFLOW_HEADER - self.carved
    }
}

/// Overflow space: the regions, newest first, and where the open
/// generation writes next.
struct Space {
    regions: [OverflowRegion; MAX_REGIONS],
    count: usize,
    /// The newest region as this writer sees it: the head of the list the
    /// metadata record points at, staged or published.
    head: u64,
    /// A region was appended whose place at the head is not staged yet.
    head_moved: bool,
    /// The region the open generation fills and the bytes it has used.
    filling: Option<(u64, u64)>,
    /// Bytes the open generation has put in overflow space.
    streamed: u64,
    /// The end of the furthest matrix record appended: what a frame the
    /// writer writes may point into.
    requires: u64,
    reused: u64,
    appended: u64,
}

impl Space {
    fn new() -> Self {
        Self {
            regions: [OverflowRegion::default(); MAX_REGIONS],
            count: 0,
            head: 0,
            head_moved: false,
            filling: None,
            streamed: 0,
            requires: 0,
            reused: 0,
            appended: 0,
        }
    }

    fn regions(&self) -> &[OverflowRegion] {
        &self.regions[..self.count]
    }

    fn find(&self, at: u64) -> Option<usize> {
        self.regions().iter().position(|region| region.at == at)
    }

    /// Keeps track of `region` as the newest; past [`MAX_REGIONS`] the oldest
    /// is forgotten.
    fn track_newest(&mut self, region: OverflowRegion) {
        let keep = self.count.min(MAX_REGIONS - 1);
        self.regions.copy_within(..keep, 1);
        self.regions[0] = region;
        self.count = keep + 1;
    }

    /// Keeps track of `region` as older than every region tracked so far.
    fn track_oldest(&mut self, region: OverflowRegion) -> bool {
        if self.count == MAX_REGIONS {
            return false;
        }
        self.regions[self.count] = region;
        self.count += 1;
        true
    }

    /// `len` bytes of overflow space for `generation`'s index: next in the
    /// region it is filling, else in a reclaimed region with room, else in a
    /// new region appended at least as long as what the generation has
    /// streamed so far (regions of a long publication double), within the
    /// store's record limit.
    fn reserve(
        &mut self,
        store: &mut dyn TileStore,
        len: u64,
        generation: u64,
        reclaimed: u64,
    ) -> Result<u64> {
        let taken = len.next_multiple_of(8);
        if let Some((region_at, used)) = self.filling
            && let Some(index) = self.find(region_at)
            && self.regions[index].free_room() - used >= taken
        {
            self.filling = Some((region_at, used + taken));
            self.streamed += taken;
            return Ok(region_at + OVERFLOW_HEADER + used);
        }
        let reusable = self
            .regions()
            .iter()
            .enumerate()
            .filter(|(_, region)| region.generation <= reclaimed && region.free_room() >= taken)
            .min_by_key(|(_, region)| region.free_room())
            .map(|(index, _)| index);
        if let Some(index) = reusable {
            let region = &mut self.regions[index];
            region.generation = generation;
            self.filling = Some((region.at, taken));
            self.streamed += taken;
            self.reused += 1;
            return Ok(region.at + OVERFLOW_HEADER);
        }
        let least = OVERFLOW_HEADER + taken;
        let limit = store.record_limit() / REGION_UNIT * REGION_UNIT;
        let capacity = least
            .max(self.streamed)
            .next_multiple_of(REGION_UNIT)
            .min(limit)
            .max(least);
        let next = self.head;
        let at = store.append_zeroed(
            RecordKind::Overflow,
            &encode_overflow_header(next, capacity, 0),
            capacity,
        )?;
        self.requires = self.requires.max(at + capacity);
        self.track_newest(OverflowRegion {
            at,
            len: capacity,
            generation,
            carved: 0,
            carved_published: 0,
            next,
        });
        self.head = at;
        self.head_moved = true;
        self.filling = Some((at, taken));
        self.streamed += taken;
        self.appended += 1;
        Ok(at + OVERFLOW_HEADER)
    }

    /// Whether `[target, target + len)` lies in space carved since the last
    /// publication.
    fn is_carved_fresh(&self, target: u64, len: u64) -> bool {
        self.regions().iter().any(|region| {
            let end = region.at + region.len;
            target >= end - region.carved && target + len <= end - region.carved_published
        })
    }

    fn published(&mut self) {
        for region in &mut self.regions[..self.count] {
            region.carved_published = region.carved;
        }
        self.filling = None;
        self.streamed = 0;
    }
}

/// A ring frame being filled: its entries are written as they are staged,
/// its header and checksum when it is closed.
struct OpenFrame {
    /// Ring offset of the frame.
    at: u64,
    /// Bytes so far, header included.
    len: u64,
    /// Contiguous ring bytes it may take.
    reserved: u64,
    count: u32,
    /// Checksum of everything after the header so far.
    hasher: crc32fast::Hasher,
}

/// Ranges written in place go out through one buffer: adjacent ones become
/// one write.
struct Coalescer<'b> {
    buf: &'b mut [u8],
    at: u64,
    len: usize,
    writes: u64,
}

impl Coalescer<'_> {
    /// Copies `len` bytes at `from` to `target`.
    fn copy(&mut self, store: &mut dyn TileStore, target: u64, from: u64, len: u64) -> Result<()> {
        let mut done = 0u64;
        while done < len {
            if self.len > 0 && self.at + self.len as u64 != target + done {
                self.flush(store)?;
            }
            if self.len == 0 {
                self.at = target + done;
            }
            let room = self.buf.len() - self.len;
            let n = crate::width::bounded(len - done, room);
            store.read_at(from + done, &mut self.buf[self.len..self.len + n])?;
            self.len += n;
            done += n as u64;
            if self.len == self.buf.len() {
                self.flush(store)?;
            }
        }
        Ok(())
    }

    fn flush(&mut self, store: &mut dyn TileStore) -> Result<()> {
        if self.len > 0 {
            store.write_at(self.at, &self.buf[..self.len])?;
            self.writes += 1;
            self.at += self.len as u64;
            self.len = 0;
        }
        Ok(())
    }
}

/// What a staged range holds: bytes from the caller.
#[derive(Clone, Copy)]
enum Src<'a> {
    Bytes(&'a [u8]),
}

impl Src<'_> {
    const fn len(&self) -> u64 {
        match self {
            Self::Bytes(bytes) => bytes.len() as u64,
        }
    }
}

pub(crate) struct RingStore {
    /// Where the newest overflow region's payload offset is kept (`0`: none),
    /// a field of the owner's metadata changed through the ring.
    head_at: u64,
    pub(crate) ring: Ring,
    header: RingHeader,
    /// Ring offset where the next frame goes (past any frames of the open
    /// generation already written).
    cursor: u64,
    /// Ranges in the ring not yet in place (published and open), and the
    /// published indexes.
    layers: Layers,
    /// Index pages; behind a lock so `&self` reads can use it.
    cache: Mutex<PageCache>,
    scratch: Box<[u8]>,
    copy: Box<[u8]>,
    frame: Option<OpenFrame>,
    /// The open generation's index, once it streams past the ring.
    building: Option<Building>,
    /// Whether the open generation staged anything through the ring or an
    /// index.
    spilled: bool,
    /// Records appended, or written directly, since the last barrier: they
    /// must be durable before a frame that points at them.
    appended: bool,
    /// The file's logical length at the last publication: records appended
    /// since lie at or past it. Nothing published points at them, so no
    /// reader can reach them, and bytes staged into them are written in place
    /// at once instead of through the ring.
    fresh_floor: u64,
    /// Ring bytes the open generation's frames take: past half the ring the
    /// rest of the publication streams into an index.
    open_ring_bytes: u64,
    space: Space,
    pub(crate) stats: WriterStats,
}
impl RingStore {
    /// Appends a ring of `ring_len` bytes (at least [`MIN_RING`]) and returns
    /// the engine over it. The owner records the ring's place and keeps the
    /// overflow list head at `head_at`, zero for now.
    pub(crate) fn create(store: &mut dyn TileStore, ring_len: u64, head_at: u64) -> Result<Self> {
        let ring_len = ring_len.max(MIN_RING).next_multiple_of(8);
        let header = RingHeader {
            seq: 1,
            ..RingHeader::default()
        };
        let mut slots = [0u8; 2 * crate::width::small(wal::HEADER_SLOT)];
        slots[HEADER_SLOT_FOR_SEQ1..HEADER_SLOT_FOR_SEQ1 + crate::width::small(wal::HEADER_SLOT)]
            .copy_from_slice(&header.encode());
        let base = store.append_zeroed(RecordKind::Ring, &slots, ring_len)?;
        let ring = Ring {
            base,
            len: ring_len,
        };
        Self::with_parts(store, head_at, ring, header)
    }
    /// Opens the engine over the ring at `ring` (payload offset, length).
    /// Publications the ring still holds are adopted as an overlay, not
    /// written in place; a publication whose frames are torn or uncommitted
    /// never happened. The ring header is rewritten only when the frames show
    /// a newer publication than the header records.
    pub(crate) fn open(store: &mut dyn TileStore, ring: (u64, u64), head_at: u64) -> Result<Self> {
        let ring = Ring::within(ring.0, ring.1, store)?;
        let header = ring.read_header(store)?;
        let mut engine = Self::with_parts(store, head_at, ring, header)?;
        engine.recover(store)?;
        engine.adopt_overflow(store)?;
        Ok(engine)
    }

    fn with_parts(
        store: &dyn TileSource,
        head_at: u64,
        ring: Ring,
        header: RingHeader,
    ) -> Result<Self> {
        let mut copy = Vec::new();
        copy.try_reserve_exact(COALESCE_BYTES)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix copy buffer",
                requested: COALESCE_BYTES as u64,
            })?;
        copy.resize(COALESCE_BYTES, 0);
        Ok(Self {
            head_at,
            ring,
            cursor: header.head,
            header,
            layers: Layers::new(ring)?,
            cache: Mutex::new(PageCache::with_pages(index::DEFAULT_CACHE_PAGES)?),
            scratch: scan_buffer()?,
            copy: copy.into_boxed_slice(),
            frame: None,
            building: None,
            spilled: false,
            appended: false,
            fresh_floor: store.len()?,
            open_ring_bytes: 0,
            space: Space::new(),
            stats: WriterStats::default(),
        })
    }

    /// The overflow regions this writer knows, newest first:
    /// `(payload offset, length, carved bytes)`.
    #[cfg(test)]
    pub(crate) fn overflow_regions(&self) -> Vec<(u64, u64, u64)> {
        self.space
            .regions()
            .iter()
            .map(|region| (region.at, region.len, region.carved))
            .collect()
    }

    /// The newest published generation.
    pub(crate) const fn published(&self) -> u64 {
        self.header.published
    }

    /// Whether anything is staged and not yet published.
    pub(crate) fn is_dirty(&self) -> bool {
        self.spilled
    }

    fn cache(&self) -> Result<std::sync::MutexGuard<'_, PageCache>> {
        self.cache.lock().map_err(|_| Error::InvalidMatrixLayout)
    }

    // ---- reading this writer's own state ----

    /// Reads `buf.len()` bytes at `offset` as this writer sees them: what it
    /// staged over what it published over the file.
    pub(crate) fn read(&self, store: &dyn TileSource, offset: u64, buf: &mut [u8]) -> Result<()> {
        let open = self.building.map(|b| (b.generation, b.root));
        let mut cache = self.cache()?;
        self.layers
            .fill(
                &mut cache,
                store,
                offset,
                buf,
                self.header.checkpointed,
                open,
            )
            .map(drop)
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
        self.stage_src(store, target, Src::Bytes(bytes))
    }

    fn stage_src(&mut self, store: &mut dyn TileStore, target: u64, src: Src<'_>) -> Result<()> {
        if self.is_fresh(target, src.len()) {
            self.write_src(store, target, src, false)?;
            self.appended = true;
            return Ok(());
        }
        self.spilled = true;
        if self.building.is_none() && !self.stage_in_ring(store, target, src)? {
            self.start_index(store)?;
        }
        if self.building.is_some() {
            self.stage_in_index(store, target, src)?;
        }
        Ok(())
    }

    /// Writes `src` at `at`, feeding the open frame's checksum when `hash`.
    fn write_src(
        &mut self,
        store: &mut dyn TileStore,
        at: u64,
        src: Src<'_>,
        hash: bool,
    ) -> Result<()> {
        match src {
            Src::Bytes(bytes) => {
                store.write_at(at, bytes)?;
                if hash && let Some(frame) = self.frame.as_mut() {
                    frame.hasher.update(bytes);
                }
            }
        }
        Ok(())
    }

    /// Ring bytes the open generation may still take before the rest of it
    /// streams into an index appended to the file.
    pub(crate) fn ring_room(&self) -> u64 {
        (self.ring.area() / 2).saturating_sub(self.open_ring_bytes)
    }

    /// The bytes of the ring's frame area.
    pub(crate) const fn ring_area(&self) -> u64 {
        self.ring.area()
    }

    /// The newest generation in place: ranges staged in it or before are no
    /// longer in any overlay.
    pub(crate) const fn checkpointed(&self) -> u64 {
        self.header.checkpointed
    }

    /// The oldest generation a reader may still show: a reader whose
    /// snapshot is older expires. A checkpoint raises it to the generation it
    /// put in place; [`Self::raise_mark`] raises it further.
    pub(crate) const fn mark(&self) -> u64 {
        self.header.mark
    }

    /// Expires every reader older than the last publication. Afterwards no
    /// reader and no recovery shows a generation older than it, so space
    /// that generation no longer points at may be written in place.
    pub(crate) fn raise_mark(&mut self, store: &mut dyn TileStore) -> Result<()> {
        if self.header.mark < self.header.published {
            let mut marked = self.header;
            marked.mark = marked.published;
            self.write_header(store, marked)?;
        }
        Ok(())
    }

    /// Writes `bytes` at `target` straight to the file instead of through
    /// the ring, when nothing the ring holds covers any of them: no range
    /// staged since the last checkpoint (published or not) and no index.
    /// Returns `false`, writing nothing, otherwise.
    ///
    /// Only for space the caller knows no generation from the mark on points
    /// at, such as space freed by a generation the mark has reached. A
    /// reader whose snapshot is older than the mark expires, and newer
    /// snapshots do not point at the space, so no reader can see these bytes
    /// before the publication that points at them; that publication makes
    /// them durable before its commit frame, as it does appended records. A
    /// recovery shows the last publication, which does not point at them
    /// either.
    pub(crate) fn write_unreferenced(
        &mut self,
        store: &mut dyn TileStore,
        target: u64,
        bytes: &[u8],
    ) -> Result<bool> {
        let len = bytes.len() as u64;
        if !self.is_fresh(target, len) && self.covers(&*store, target, len)? {
            return Ok(false);
        }
        store.write_at(target, bytes)?;
        self.appended = true;
        Ok(true)
    }

    /// Whether anything staged since the last checkpoint, in the ring or in
    /// an index, published or not, covers any of `[target, target + len)`.
    fn covers(&self, store: &dyn TileSource, target: u64, len: u64) -> Result<bool> {
        let mut covered = false;
        self.layers
            .overlay
            .for_each_overlapping(target, len, &mut |_, _| {
                covered = true;
                Ok(())
            })?;
        if covered {
            return Ok(true);
        }
        let checkpointed = self.header.checkpointed;
        let open = self.building.map(|b| (b.generation, b.root));
        let indexes = self.layers.indexes().iter().copied();
        let mut cache = self.cache()?;
        for (generation, root) in indexes.filter(|(g, _)| *g > checkpointed).chain(open) {
            index::for_each_overlapping(
                &mut cache,
                Io::Read(store),
                generation,
                root,
                target,
                len,
                &mut |_, _, _| {
                    covered = true;
                    Ok(())
                },
            )?;
            if covered {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether `[target, target + len)` lies inside a record appended, or
    /// space carved, since the last publication.
    fn is_fresh(&self, target: u64, len: u64) -> bool {
        target >= self.fresh_floor || self.space.is_carved_fresh(target, len)
    }

    /// The largest frame: a quarter of the ring, so a checkpoint can always
    /// make room for the next one.
    fn frame_limit(&self) -> u64 {
        (self.ring.area() / 4).max(FRAME_MIN * 2)
    }

    /// Writes the range into the open ring frame. `false` when it belongs in
    /// an index instead: it does not fit a frame, the open generation would
    /// take more than half the ring, or the overlay has no room for it.
    fn stage_in_ring(
        &mut self,
        store: &mut dyn TileStore,
        target: u64,
        src: Src<'_>,
    ) -> Result<bool> {
        let len = src.len();
        let need = ENTRY_HEADER + len;
        // Room for the entry, a header, padding and the checksum.
        let whole = FRAME_HEADER + need + 12;
        if whole > self.frame_limit() || self.open_ring_bytes + whole > self.ring.area() / 2 {
            return Ok(false);
        }
        let overlay = &mut self.layers.overlay;
        if overlay.len() == overlay.capacity() && overlay.get(target).is_none() {
            let in_place = self.header.checkpointed;
            overlay.retain(|_, entry| entry.generation > in_place);
            if overlay.len() == overlay.capacity() {
                return Ok(false);
            }
        }
        if let Some(frame) = &self.frame
            && frame.len + need + 12 > frame.reserved
        {
            self.close_frame(store, false)?;
        }
        if self.frame.is_none() {
            self.open_frame(store, whole)?;
        }
        let frame = self.frame.as_mut().expect("opened above");
        let at = self.ring.at(frame.at + frame.len);
        let mut entry = [0u8; crate::width::small(ENTRY_HEADER)];
        entry[..8].copy_from_slice(&target.to_le_bytes());
        let len32 = crate::width::field32(crate::width::small(len))?;
        entry[8..].copy_from_slice(&len32.to_le_bytes());
        let header_len = entry.len();
        frame.hasher.update(&entry);
        frame.len += need;
        frame.count += 1;
        match src {
            Src::Bytes(bytes) if header_len + bytes.len() <= self.copy.len() => {
                self.copy[..header_len].copy_from_slice(&entry);
                self.copy[header_len..header_len + bytes.len()].copy_from_slice(bytes);
                store.write_at(at, &self.copy[..header_len + bytes.len()])?;
                if let Some(frame) = self.frame.as_mut() {
                    frame.hasher.update(bytes);
                }
            }
            _ => {
                store.write_at(at, &entry)?;
                self.write_src(store, at + ENTRY_HEADER, src, true)?;
            }
        }
        self.open_ring_bytes += need;
        let inserted = self.layers.overlay.insert(
            target,
            OverlayEntry {
                generation: self.header.published + 1,
                at: at + ENTRY_HEADER,
                len: len32,
            },
        );
        debug_assert!(inserted, "room was made above");
        Ok(true)
    }

    /// Opens a frame of at least `least` bytes at the next ring room.
    fn open_frame(&mut self, store: &mut dyn TileStore, least: u64) -> Result<()> {
        let at = self.place(store, least)?;
        let (room, _) = self.ring.room(self.header.tail, at);
        let reserved = room.min(self.frame_limit()).max(least);
        self.frame = Some(OpenFrame {
            at,
            len: FRAME_HEADER,
            reserved,
            count: 0,
            hasher: crc32fast::Hasher::new(),
        });
        self.open_ring_bytes += FRAME_HEADER;
        Ok(())
    }

    /// Seals the open frame: padding, header and checksum. The commit frame
    /// when `commit`.
    fn close_frame(&mut self, store: &mut dyn TileStore, commit: bool) -> Result<()> {
        let Some(mut frame) = self.frame.take() else {
            return Ok(());
        };
        let len = (frame.len + 4).next_multiple_of(8);
        let pad = crate::width::small(len - 4 - frame.len);
        frame.hasher.update(&ZEROS[..pad]);
        let header = frame_header(
            if commit { FRAME_COMMIT } else { 0 },
            self.header.published + 1,
            len,
            frame.count,
            self.space.requires,
        );
        let mut crc = crc32fast::Hasher::new();
        crc.update(&header);
        crc.combine(&frame.hasher);
        let mut tail = [0u8; 12];
        tail[pad..pad + 4].copy_from_slice(&crc.finalize().to_le_bytes());
        store.write_at(self.ring.at(frame.at + frame.len), &tail[..pad + 4])?;
        store.write_at(self.ring.at(frame.at), &header)?;
        self.stats.frames += 1;
        self.open_ring_bytes += len - frame.len;
        self.cursor = frame.at + len;
        if self.cursor == self.ring.area() {
            self.cursor = 0;
        }
        Ok(())
    }

    /// Streams the rest of the open generation into an index. Everything
    /// published is put in place first: the open generation's index is then
    /// the only one a writer adds, and the regions earlier ones used are free.
    fn start_index(&mut self, store: &mut dyn TileStore) -> Result<()> {
        self.close_frame(store, false)?;
        if self.header.published > self.header.checkpointed {
            self.checkpoint(store, u64::MAX)?;
        }
        debug_assert!(self.layers.indexes().len() < MAX_INDEXES);
        let generation = self.header.published + 1;
        let reclaimed = self.header.reclaimed;
        let space = &mut self.space;
        let cache = self
            .cache
            .get_mut()
            .map_err(|_| Error::InvalidMatrixLayout)?;
        let building = Building::new(cache, store, generation, &mut |store, len| {
            space.reserve(store, len, generation, reclaimed)
        })?;
        self.building = Some(building);
        self.stats.indexed += 1;
        self.note_space();
        Ok(())
    }

    /// Writes the range to overflow space and records it in the open index.
    fn stage_in_index(
        &mut self,
        store: &mut dyn TileStore,
        target: u64,
        src: Src<'_>,
    ) -> Result<()> {
        let generation = self.header.published + 1;
        let reclaimed = self.header.reclaimed;
        let mut pending = Some((target, src));
        let mut head: [u8; 8];
        while let Some((target, src)) = pending.take() {
            let len = src.len();
            let at = self.space.reserve(store, len, generation, reclaimed)?;
            self.write_src(store, at, src, false)?;
            self.appended = true;
            let space = &mut self.space;
            let cache = self
                .cache
                .get_mut()
                .map_err(|_| Error::InvalidMatrixLayout)?;
            let building = self.building.as_mut().ok_or(Error::InvalidMatrixLayout)?;
            building.insert(
                cache,
                store,
                &mut |store, len| space.reserve(store, len, generation, reclaimed),
                target,
                at,
                crate::width::field32(crate::width::small(len))?,
            )?;
            // Pages written back now keep room for clean pages a read can
            // evict.
            if cache.dirty() > cache.pages() / 2 {
                cache.flush(store)?;
            }
            // A new region becomes the head of the list in the same
            // publication.
            if self.space.head_moved {
                self.space.head_moved = false;
                head = self.space.head.to_le_bytes();
                pending = Some((self.head_at, Src::Bytes(&head)));
            }
        }
        self.note_space();
        Ok(())
    }

    fn note_space(&mut self) {
        self.stats.overflow_appended = self.space.appended;
        self.stats.overflow_reused = self.space.reused;
    }
    /// Writes a new tile or directory page of `len` bytes, `prefix` and then
    /// zeros: into free overflow space when a region has room, else as a new
    /// record. Either way nothing published points at it yet, so it is
    /// fresh until the next publication.
    pub(crate) fn place_record(
        &mut self,
        store: &mut dyn TileStore,
        kind: RecordKind,
        prefix: &[u8],
        len: u64,
    ) -> Result<u64> {
        let at = match self.carve(store, len)? {
            Some(at) => {
                // The space held older data: zero all of it first.
                let mut done = 0u64;
                while done < len {
                    let n = crate::width::bounded(len - done, ZEROS.len());
                    store.write_at(at + done, &ZEROS[..n])?;
                    done += n as u64;
                }
                store.write_at(at, prefix)?;
                self.stats.carved += 1;
                at
            }
            None => {
                let at = store.append_zeroed(kind, prefix, len)?;
                self.space.requires = self.space.requires.max(at + len);
                at
            }
        };
        self.appended = true;
        Ok(at)
    }
    /// Calls `visit` with every published range not yet in place, in the
    /// order they are laid.
    #[cfg(feature = "mmap")]
    pub(crate) fn for_each_overlay(
        &self,
        store: &dyn TileSource,
        visit: &mut dyn FnMut(u64, OverlayEntry) -> Result<()>,
    ) -> Result<()> {
        let mut cache = self.cache()?;
        super::view::for_each_layered(
            &self.layers,
            &mut cache,
            store,
            self.header.checkpointed,
            self.header.published,
            None,
            visit,
        )
    }

    /// Takes `len` bytes (rounded up to 8) from the end of the free space of
    /// the reclaimed overflow region with the least room that holds them, and
    /// stages the region's new carved count. `None` when no region can.
    fn carve(&mut self, store: &mut dyn TileStore, len: u64) -> Result<Option<u64>> {
        let len = len.next_multiple_of(8);
        let reclaimed = self.header.reclaimed;
        let Some(index) = self
            .space
            .regions()
            .iter()
            .enumerate()
            .filter(|(_, region)| region.generation <= reclaimed && region.free_room() >= len)
            .min_by_key(|(_, region)| region.free_room())
            .map(|(index, _)| index)
        else {
            return Ok(None);
        };
        let region = &mut self.space.regions[index];
        region.carved += len;
        let (region_at, next, region_len, carved) =
            (region.at, region.next, region.len, region.carved);
        self.stage(
            store,
            region_at,
            &encode_overflow_header(next, region_len, carved),
        )?;
        Ok(Some(region_at + region_len - carved))
    }

    /// Forgets everything staged since the last publication.
    #[cfg(test)]
    pub(crate) fn discard(&mut self) {
        let open = self.header.published + 1;
        self.layers
            .overlay
            .retain(|_, entry| entry.generation != open);
        if let Ok(cache) = self.cache.get_mut() {
            cache.forget(open);
        }
        self.frame = None;
        self.building = None;
        self.spilled = false;
        self.cursor = self.header.head;
        self.open_ring_bytes = 0;
        self.space.filling = None;
        self.space.streamed = 0;
    }

    // ---- publication ----

    /// Publishes everything staged as the next generation and returns it.
    /// One barrier, two when this generation appended records (which must be
    /// durable before a frame that points at them).
    pub(crate) fn publish(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        if !self.is_dirty() {
            return Ok(self.header.published);
        }
        let generation = self.header.published + 1;
        if self.building.is_some() {
            self.cache
                .get_mut()
                .map_err(|_| Error::InvalidMatrixLayout)?
                .flush(store)?;
        }
        // Everything a commit frame points at, appended records, overflow
        // data and index pages alike, is durable before it is written.
        if self.appended {
            store.sync()?;
            self.stats.barriers += 1;
            self.appended = false;
        }
        if let Some(building) = self.building.take() {
            let frame = wal::index_frame(
                FRAME_COMMIT,
                generation,
                self.space.requires,
                &building.root,
            );
            let at = self.place(store, INDEX_FRAME_LEN)?;
            store.write_at(self.ring.at(at), &frame)?;
            self.stats.frames += 1;
            self.cursor = at + INDEX_FRAME_LEN;
            if self.cursor == self.ring.area() {
                self.cursor = 0;
            }
            self.layers.push_index(generation, building.root)?;
        } else {
            if self.frame.is_none() {
                self.open_frame(store, FRAME_MIN)?;
            }
            self.close_frame(store, true)?;
        }
        store.sync()?;
        self.stats.barriers += 1;
        let mut header = self.header;
        header.published = generation;
        header.head = self.cursor;
        self.write_header(store, header)?;
        self.spilled = false;
        self.open_ring_bytes = 0;
        // Published now: later changes to them must not reach a reader early.
        self.fresh_floor = store.len()?;
        self.space.published();
        Ok(generation)
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
    /// `need` is `u64::MAX`. Ranges stream from the ring and from indexes
    /// through the copy buffer.
    pub(crate) fn checkpoint(&mut self, store: &mut dyn TileStore, need: u64) -> Result<()> {
        let header = self.header;
        if header.published == header.checkpointed {
            return Ok(());
        }
        // Which generations to put in place: the fewest, oldest ones that
        // leave room. Readers whose snapshots are newer keep reading them.
        let area = self.ring.area();
        let ring = self.ring;
        let cursor = self.cursor;
        let mut chosen: Option<(u64, u64)> = None;
        let mut last: Option<(u64, u64)> = None;
        wal::scan(
            store,
            ring,
            header.tail,
            header.checkpointed + 1,
            header.published,
            &mut self.scratch,
            &mut |_, event| {
                if let ScanEvent::Committed(generation, end) = event {
                    last = Some((generation, end));
                    if need != u64::MAX && chosen.is_none() {
                        // The largest frame that fits once frames before
                        // `end` are gone, by the rule `place` then applies.
                        // Free half the ring at least, not just this frame's
                        // room: a ring kept just full would checkpoint (two
                        // more syncs) on every publish.
                        let (at_cursor, at_start) = ring.room(end, cursor);
                        let free_total = area - ring.used(end, cursor);
                        if (at_cursor.max(at_start) >= need && free_total >= area / 2)
                            || end == cursor
                        {
                            chosen = Some((generation, end));
                        }
                    }
                }
                Ok(())
            },
        )?;
        let Some(last) = last else {
            return Err(Error::InvalidMatrixLayout);
        };
        if last.0 != header.published {
            return Err(Error::InvalidMatrixLayout);
        }
        let (target, new_tail) = chosen.unwrap_or(last);

        // Readers must see the mark before anything in place changes.
        let mut marked = self.header;
        marked.mark = marked.mark.max(target);
        self.write_header(store, marked)?;
        {
            let cache = self
                .cache
                .get_mut()
                .map_err(|_| Error::InvalidMatrixLayout)?;
            let mut out = Coalescer {
                buf: &mut self.copy,
                at: 0,
                len: 0,
                writes: 0,
            };
            // In ring order: generation after generation, and within one its
            // ring ranges before its index, so the newest bytes land last.
            wal::scan(
                store,
                ring,
                header.tail,
                header.checkpointed + 1,
                target,
                &mut self.scratch,
                &mut |store, event| match event {
                    ScanEvent::Entry(_, entry) => {
                        out.copy(store, entry.target, entry.at, u64::from(entry.len))
                    }
                    ScanEvent::Index(generation, root) => index::for_each(
                        cache,
                        Io::Write(store),
                        generation,
                        root,
                        &mut |io, target, at, len| match io {
                            Io::Write(store) => out.copy(&mut **store, target, at, u64::from(len)),
                            Io::Read(_) => Err(Error::InvalidMatrixLayout),
                        },
                    ),
                    ScanEvent::Committed(..) => Ok(()),
                },
            )?;
            out.flush(store)?;
            self.stats.in_place_writes += out.writes;
        }
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
        let cache = self
            .cache
            .get_mut()
            .map_err(|_| Error::InvalidMatrixLayout)?;
        self.layers.drop_in_place(target, cache);
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
        self.layers.clear();
        let layers = &mut self.layers;
        let end = wal::scan(
            store,
            self.ring,
            header.tail,
            header.checkpointed + 1,
            u64::MAX,
            &mut self.scratch,
            &mut |_, event| match event {
                ScanEvent::Entry(generation, entry) => {
                    let kept = layers.insert(
                        entry.target,
                        OverlayEntry {
                            generation,
                            at: entry.at,
                            len: entry.len,
                        },
                        header.checkpointed,
                    );
                    if kept {
                        Ok(())
                    } else {
                        Err(Error::InvalidMatrixLayout)
                    }
                }
                ScanEvent::Index(generation, root) => layers.push_index(generation, root),
                ScanEvent::Committed(..) => Ok(()),
            },
        )?;
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

    /// Finds the overflow regions a reopened matrix already has, walking the
    /// list from the head the metadata record points at. Their data may
    /// belong to a publication not yet in place, so each is in use until
    /// everything published now is reclaimed. A header that does not check
    /// out ends the walk: the regions past it are left alone, never reused.
    fn adopt_overflow(&mut self, store: &mut dyn TileStore) -> Result<()> {
        let mut head = [0u8; 8];
        self.read(store, self.head_at, &mut head)?;
        self.space.head = u64::from_le_bytes(head);
        let end = store.len()?;
        let mut at = self.space.head;
        // Each region points at an older, earlier one, so the walk ends.
        let mut before = u64::MAX;
        while at != 0 && at < before && at.checked_add(OVERFLOW_HEADER).is_some_and(|e| e <= end) {
            let mut bytes = [0u8; crate::width::small(OVERFLOW_HEADER)];
            self.read(store, at, &mut bytes)?;
            let Some((next, len, carved)) = decode_overflow_header(&bytes) else {
                break;
            };
            if at.checked_add(len).is_none_or(|e| e > end) {
                break;
            }
            let tracked = self.space.track_oldest(OverflowRegion {
                at,
                len,
                generation: self.header.published,
                carved,
                carved_published: carved,
                next,
            });
            if !tracked {
                break;
            }
            before = at;
            at = next;
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
#[allow(dead_code)]
const _: () = assert!(FRAMES_AT >= 2 * wal::HEADER_SLOT);
#[allow(dead_code)]
const _: () = assert!(PAGE.is_multiple_of(8));
