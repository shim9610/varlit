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
/// ranges become one positional write. Between checkpoints the same buffer
/// holds the open frame's bytes not yet written, so a frame goes out in as
/// few writes as fit.
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

/// A ring frame being filled: its entries are buffered as they are staged
/// (written whenever the buffer fills), its header and checksum added when it
/// is closed, and what is still buffered written then.
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

    /// Writes `bytes` to `target`, after what is buffered when it follows on.
    fn write(&mut self, store: &mut dyn TileStore, target: u64, bytes: &[u8]) -> Result<()> {
        let mut done = 0usize;
        while done < bytes.len() {
            let at = target + done as u64;
            if self.len > 0 && self.at + self.len as u64 != at {
                self.flush(store)?;
            }
            if self.len == 0 {
                self.at = at;
            }
            let n = (bytes.len() - done).min(self.buf.len() - self.len);
            self.buf[self.len..self.len + n].copy_from_slice(&bytes[done..done + n]);
            self.len += n;
            done += n;
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

/// A source that shows the file with the open frame's buffered bytes over it:
/// the writer reading what it staged sees them before they are written.
struct Buffered<'a> {
    file: &'a dyn TileSource,
    at: u64,
    bytes: &'a [u8],
}

impl TileSource for Buffered<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.file.read_at(offset, buf)?;
        if let Some((dst, src, n)) = wal::overlap(offset, buf.len(), self.at, self.bytes.len()) {
            buf[dst..dst + n].copy_from_slice(&self.bytes[src..src + n]);
        }
        Ok(())
    }
    fn len(&self) -> Result<u64> {
        self.file.len()
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
    /// The checkpoint's coalescing buffer, and between checkpoints the open
    /// frame's bytes not yet written: `copy[..held]` belongs at `held_at`.
    copy: Box<[u8]>,
    held_at: u64,
    held: usize,
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
            held_at: 0,
            held: 0,
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
        let buffered = Buffered {
            file: store,
            at: self.held_at,
            bytes: &self.copy[..self.held],
        };
        self.layers
            .fill(
                &mut cache,
                &buffered,
                offset,
                buf,
                self.header.checkpointed,
                open,
            )
            .map(drop)
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
}

/// Where the first header goes in a new ring payload: the slot for sequence 1.
const HEADER_SLOT_FOR_SEQ1: usize = crate::width::small(wal::HEADER_SLOT);
#[allow(dead_code)]
const _: () = assert!(FRAMES_AT >= 2 * wal::HEADER_SLOT);
#[allow(dead_code)]
const _: () = assert!(PAGE.is_multiple_of(8));

// The implementation is split by concern across these files, included here
// so they share this module.
include!("ring_store/checkpoint.rs");
include!("ring_store/staging.rs");
