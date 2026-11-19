//! The write-ahead ring: a fixed-size internal record holding the byte ranges
//! of every publication not yet checkpointed into place.
//!
//! Layout of the ring record's payload:
//! - two header slots of [`HEADER_SLOT`] bytes at 0 and [`HEADER_SLOT`]; the
//!   valid one with the higher sequence is current, so a torn header write
//!   leaves the previous one in force;
//! - frames from [`FRAMES_AT`] to the end of the record, written circularly.
//!
//! A frame is a header, `count` entries of `(target, len, bytes)`, and a
//! CRC32 of all of it. A publication is one or more frames of one generation,
//! the last carrying [`FRAME_COMMIT`]. A frame that does not fit before the
//! end of the ring is preceded by a [`FRAME_WRAP`] marker, or, when not even
//! a marker fits, the scan wraps by itself.
//!
//! A publication too large for the ring streams the rest of its ranges
//! outside it: their bytes go to overflow records and an on-disk index (a
//! B+tree keyed by target) records where each is. The publication's commit
//! frame carries [`FRAME_INDEX`] and the index root instead of entries, so
//! the ring holds one small frame for it whatever its size, and it commits
//! atomically with the same barrier.

use super::store::TileSource;
use crate::{Error, Result};

pub(crate) const RING_MAGIC: &[u8; 4] = b"VRNG";
pub(crate) const FRAME_MAGIC: &[u8; 4] = b"VWFR";
pub(crate) const HEADER_SLOT: u64 = 128;
pub(crate) const FRAMES_AT: u64 = 4096;
/// Magic, flags, generation, length, entry count, and the logical file
/// length the frame depends on (`requires`).
pub(crate) const FRAME_HEADER: u64 = 32;
/// The smallest frame (a wrap marker): header and CRC, 8-aligned. Fewer
/// bytes than this before the end of the ring wrap without a marker.
pub(crate) const FRAME_MIN: u64 = 40;
pub(crate) const ENTRY_HEADER: u64 = 12;
pub(crate) const FRAME_COMMIT: u32 = 1;
pub(crate) const FRAME_WRAP: u32 = 2;
/// The frame names the root of the generation's on-disk index instead of
/// holding entries; its body is an [`IndexRoot`].
pub(crate) const FRAME_INDEX: u32 = 4;
/// An index frame: header, descriptor, four bytes of padding, CRC.
pub(crate) const INDEX_FRAME_LEN: u64 = FRAME_HEADER + INDEX_DESCRIPTOR + 8;
/// Root page (u64), height (u32), zero (u32), range count (u64).
pub(crate) const INDEX_DESCRIPTOR: u64 = 24;
/// The tallest index a frame may name: far more levels than any file needs.
pub(crate) const MAX_INDEX_HEIGHT: u32 = 16;
/// The smallest ring: its header page and room for a few frames.
pub(crate) const MIN_RING: u64 = FRAMES_AT + 64 * 1024;
/// The largest ring a matrix may have: a format constant, so a ring length
/// read from a file is bounded without any limit the user declares.
pub(crate) const MAX_RING_BYTES: u64 = 64 << 20;

/// The ring's state. Every generation number is a publication count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RingHeader {
    pub seq: u64,
    /// Every generation up to this is in place in the file and durable there.
    pub checkpointed: u64,
    /// The file may hold values of generations up to this: raised *before*
    /// a checkpoint writes in place, so a reader that read the file and then
    /// sees this at or below its snapshot read nothing newer than it.
    pub mark: u64,
    /// Frames of generations up to this may have been overwritten: raised
    /// before the ring reuses their space.
    pub reclaimed: u64,
    /// Ring offset (relative to the frames start) of the oldest frame kept.
    pub tail: u64,
    /// Generation of the frame at `tail` (when the ring is not empty).
    pub tail_gen: u64,
    /// Ring offset just past the newest committed frame.
    pub head: u64,
    /// The newest committed generation.
    pub published: u64,
}

impl RingHeader {
    pub(crate) fn encode(&self) -> [u8; crate::width::small(HEADER_SLOT)] {
        let mut out = [0u8; crate::width::small(HEADER_SLOT)];
        out[0..4].copy_from_slice(RING_MAGIC);
        out[4..6].copy_from_slice(&1u16.to_le_bytes());
        let fields = [
            self.seq,
            self.checkpointed,
            self.mark,
            self.reclaimed,
            self.tail,
            self.tail_gen,
            self.head,
            self.published,
        ];
        for (index, value) in fields.iter().enumerate() {
            out[8 + index * 8..16 + index * 8].copy_from_slice(&value.to_le_bytes());
        }
        let crc = crc32fast::hash(&out[..72]);
        out[72..76].copy_from_slice(&crc.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        if &bytes[0..4] != RING_MAGIC
            || u16::from_le_bytes([bytes[4], bytes[5]]) != 1
            || crc32fast::hash(&bytes[..72])
                != u32::from_le_bytes(bytes[72..76].try_into().expect("four"))
        {
            return None;
        }
        let field = |index: usize| {
            u64::from_le_bytes(
                bytes[8 + index * 8..16 + index * 8]
                    .try_into()
                    .expect("eight"),
            )
        };
        Self::from_fields([
            field(0),
            field(1),
            field(2),
            field(3),
            field(4),
            field(5),
            field(6),
            field(7),
        ])
    }

    /// The header these stored fields describe, if a writer could have
    /// written them. A checksum proves the fields were written together, not
    /// that they are sane: the generations must be ordered as every writer
    /// keeps them, and leave room for the next one.
    pub(crate) const fn from_fields(fields: [u64; 8]) -> Option<Self> {
        let header = Self {
            seq: fields[0],
            checkpointed: fields[1],
            mark: fields[2],
            reclaimed: fields[3],
            tail: fields[4],
            tail_gen: fields[5],
            head: fields[6],
            published: fields[7],
        };
        if header.reclaimed <= header.checkpointed
            && header.checkpointed <= header.mark
            && header.mark <= header.published
            && header.published < u64::MAX
            && header.seq < u64::MAX
        {
            Some(header)
        } else {
            None
        }
    }

    /// Whether the ring offsets lie inside a frame area of `area` bytes, on
    /// the 8-byte boundaries frames start on.
    pub(crate) const fn fits(&self, area: u64) -> bool {
        self.tail < area
            && self.head < area
            && self.tail.is_multiple_of(8)
            && self.head.is_multiple_of(8)
    }
}

/// Where the ring is and how big its frame area is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ring {
    /// Logical offset of the ring record's payload.
    pub base: u64,
    /// Payload length.
    pub len: u64,
}

impl Ring {
    /// The ring a metadata record names, once it is known to lie inside the
    /// file: every offset computed from it afterwards stays in range.
    pub(crate) fn within(base: u64, len: u64, source: &dyn TileSource) -> Result<Self> {
        let end = base.checked_add(len).ok_or(Error::InvalidMatrixLayout)?;
        if !(MIN_RING..=MAX_RING_BYTES).contains(&len) || end > source.len()? {
            return Err(Error::InvalidMatrixLayout);
        }
        Ok(Self { base, len })
    }

    pub(crate) const fn area(&self) -> u64 {
        self.len - FRAMES_AT
    }
    /// Logical offset of ring offset `at`.
    pub(crate) const fn at(&self, at: u64) -> u64 {
        self.base + FRAMES_AT + at
    }
    /// Bytes the frames between `tail` and `head` occupy. `full` tells an
    /// empty ring (tail == head) from a full one, which the writer never
    /// lets happen: it always keeps one byte free.
    pub(crate) const fn used(&self, tail: u64, head: u64) -> u64 {
        if head >= tail {
            head - tail
        } else {
            self.area() - tail + head
        }
    }

    /// The current header: the valid slot with the higher sequence. Retries a
    /// few times when both slots read torn (a writer in the middle of one).
    pub(crate) fn read_header(&self, source: &dyn TileSource) -> Result<RingHeader> {
        let mut bytes = [0u8; 2 * crate::width::small(HEADER_SLOT)];
        for _ in 0..8 {
            source.read_at(self.base, &mut bytes)?;
            let area = self.area();
            let valid = |slot: &[u8]| RingHeader::decode(slot).filter(|header| header.fits(area));
            let first = valid(&bytes[..crate::width::small(HEADER_SLOT)]);
            let second = valid(&bytes[crate::width::small(HEADER_SLOT)..]);
            match (first, second) {
                (Some(a), Some(b)) => return Ok(if a.seq >= b.seq { a } else { b }),
                (Some(a), None) | (None, Some(a)) => return Ok(a),
                (None, None) => std::hint::spin_loop(),
            }
        }
        Err(Error::InvalidMatrixLayout)
    }

    /// Where the frame after one of `len` bytes at ring offset `at` starts,
    /// if such a frame fits there: a whole number of 8-byte words, at least
    /// a header and a checksum, ending inside the frame area. A frame ending
    /// exactly at the end of the area is followed by offset 0.
    pub(crate) const fn after_frame(&self, at: u64, len: u64) -> Option<u64> {
        let area = self.area();
        if at >= area || len < FRAME_HEADER + 4 || len > area - at || !len.is_multiple_of(8) {
            return None;
        }
        let next = at + len;
        Some(if next == area { 0 } else { next })
    }

    /// Contiguous room for one frame while the frames from `tail` up to
    /// `cursor` are kept: at the cursor, and at the start of the ring after
    /// wrapping. One byte always stays free, so `tail == cursor` only ever
    /// means an empty ring.
    pub(crate) const fn room(&self, tail: u64, cursor: u64) -> (u64, u64) {
        let area = self.area();
        if tail > cursor {
            (tail - cursor - 1, 0)
        } else if tail == 0 {
            (area - cursor - 1, 0)
        } else {
            (area - cursor, tail - 1)
        }
    }

    /// Where a frame of `len` bytes goes, given the frames kept.
    pub(crate) const fn plan_frame(&self, tail: u64, cursor: u64, len: u64) -> FramePlan {
        let (at_cursor, at_start) = self.room(tail, cursor);
        if len <= at_cursor {
            FramePlan::AtCursor
        } else if tail == cursor {
            // Nothing is kept: the whole ring is available from its start.
            if len > self.area() - 1 {
                FramePlan::TooLarge
            } else {
                FramePlan::Restart
            }
        } else if len <= at_start {
            FramePlan::AtStart
        } else {
            FramePlan::Checkpoint
        }
    }

    /// The logical offset of the slot a header of sequence `seq` goes to.
    pub(crate) const fn slot_for(&self, seq: u64) -> u64 {
        self.base + (seq % 2) * HEADER_SLOT
    }
}

/// What a writer does to find room for its next frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FramePlan {
    /// The frame fits at the cursor.
    AtCursor,
    /// The frame fits at the start of the ring, before the oldest kept
    /// frame: wrap, then write it at offset 0.
    AtStart,
    /// The ring is empty: move tail and cursor to offset 0 and plan again.
    Restart,
    /// Kept frames are in the way: checkpoint the oldest and plan again.
    Checkpoint,
    /// The frame is larger than the ring can ever hold.
    TooLarge,
}

/// The scratch a scan streams frames through: allocated once by its owner.
pub(crate) const SCAN_BUFFER: usize = 64 * 1024;

/// The root of a generation's on-disk index: a B+tree of the ranges it
/// staged outside the ring (see `index.rs`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct IndexRoot {
    /// Logical offset of the root page.
    pub root: u64,
    /// Levels below the root; `0` when the root is a leaf.
    pub height: u32,
    /// Ranges the index holds.
    pub count: u64,
}

impl IndexRoot {
    fn encode(&self) -> [u8; crate::width::small(INDEX_DESCRIPTOR)] {
        let mut out = [0u8; crate::width::small(INDEX_DESCRIPTOR)];
        out[0..8].copy_from_slice(&self.root.to_le_bytes());
        out[8..12].copy_from_slice(&self.height.to_le_bytes());
        out[16..24].copy_from_slice(&self.count.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8; crate::width::small(INDEX_DESCRIPTOR)]) -> Option<Self> {
        let root = u64::from_le_bytes(bytes[0..8].try_into().expect("eight"));
        let height = u32::from_le_bytes(bytes[8..12].try_into().expect("four"));
        let count = u64::from_le_bytes(bytes[16..24].try_into().expect("eight"));
        (bytes[12..16] == [0; 4] && height <= MAX_INDEX_HEIGHT && root <= i64::MAX as u64)
            .then_some(Self {
                root,
                height,
                count,
            })
    }
}

/// What [`scan`] reports, in ring order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanEvent {
    /// An entry of a committed generation.
    Entry(u64, ScannedEntry),
    /// The on-disk index of a committed generation: its ranges staged after
    /// the generation's ring entries, so they win over them.
    Index(u64, IndexRoot),
    /// Generation `.0` committed; its last frame ends at ring offset `.1`.
    Committed(u64, u64),
}

/// One entry of a scanned frame: where its bytes go and where they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScannedEntry {
    pub target: u64,
    /// Logical offset of the bytes inside the ring.
    pub at: u64,
    pub len: u32,
}

/// What a scan found: committed generations and where the ring ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScanEnd {
    /// The newest committed generation seen (or the start generation - 1).
    pub published: u64,
    /// Ring offset just past its commit frame.
    pub head: u64,
}

/// Reads `[pos, end)` front to back through a caller's buffer, hashing every
/// byte it passes. A read error ends the stream like its end does: a scan
/// treats both as the end of what the ring proves.
struct Stream<'b> {
    pos: u64,
    end: u64,
    buf: &'b mut [u8],
    /// Buffered bytes `[lo, hi)` start at file offset `buf_at`.
    lo: usize,
    hi: usize,
    hasher: crc32fast::Hasher,
}

impl<'b> Stream<'b> {
    fn new(start: u64, end: u64, buf: &'b mut [u8]) -> Self {
        Self {
            pos: start,
            end,
            buf,
            lo: 0,
            hi: 0,
            hasher: crc32fast::Hasher::new(),
        }
    }

    fn fill<S: TileSource + ?Sized>(&mut self, source: &S) -> bool {
        let n = crate::width::bounded(self.end - self.pos, self.buf.len());
        if n == 0 || source.read_at(self.pos, &mut self.buf[..n]).is_err() {
            return false;
        }
        self.lo = 0;
        self.hi = n;
        true
    }

    /// Passes `n` bytes, copying them into `out` when given.
    fn advance<S: TileSource + ?Sized>(
        &mut self,
        source: &S,
        mut n: u64,
        mut out: Option<&mut [u8]>,
    ) -> bool {
        if self.end - self.pos < n {
            return false;
        }
        let mut written = 0usize;
        while n > 0 {
            if self.lo == self.hi && !self.fill(source) {
                return false;
            }
            let take = crate::width::bounded(n, self.hi - self.lo);
            let bytes = &self.buf[self.lo..self.lo + take];
            self.hasher.update(bytes);
            if let Some(out) = out.as_deref_mut() {
                out[written..written + take].copy_from_slice(bytes);
            }
            written += take;
            self.lo += take;
            self.pos += take as u64;
            n -= take as u64;
        }
        true
    }

    fn take<S: TileSource + ?Sized>(&mut self, source: &S, out: &mut [u8]) -> bool {
        self.advance(source, out.len() as u64, Some(out))
    }
}

/// An entry header taken from a frame with `remaining` body bytes after it:
/// `(target, len)` when all its bytes lie inside the body.
pub(crate) fn stream_entry(
    header: &[u8; crate::width::small(ENTRY_HEADER)],
    remaining: u64,
) -> Option<(u64, u32)> {
    let target = u64::from_le_bytes(header[..8].try_into().expect("eight"));
    let len = u32::from_le_bytes(header[8..12].try_into().expect("four"));
    (u64::from(len) <= remaining).then_some((target, len))
}

/// What a scan calls for each event, with the source it reads.
type ScanVisit<'v, S> = dyn FnMut(&mut S, ScanEvent) -> Result<()> + 'v;

/// What one checked frame is.
enum Checked {
    /// A wrap marker: the scan continues at ring offset 0.
    Wrap,
    /// A frame of `generation`, ending where the next one starts.
    Frame {
        generation: u64,
        commit: bool,
        next: u64,
        len: u64,
    },
}

/// What every frame check of one scan shares: the ring, and the logical
/// length of the file the frames' entries may point into.
#[derive(Clone, Copy)]
struct Frames {
    ring: Ring,
    available: u64,
}

/// Checks the frame at ring offset `at`: header, sequence, bounds, entries
/// and checksum, streamed through `scratch`. With `report`, every entry and
/// the index of the frame are passed to it as they are read; the scan only
/// reports frames it already checked once. `None` ends the scan.
fn check_frame<S: TileSource + ?Sized>(
    source: &mut S,
    frames: Frames,
    at: u64,
    expected: u64,
    max_gen: u64,
    scratch: &mut [u8],
    mut report: Option<&mut ScanVisit<'_, S>>,
) -> Result<Option<Checked>> {
    let Frames { ring, available } = frames;
    let mut header = [0u8; crate::width::small(FRAME_HEADER)];
    if source.read_at(ring.at(at), &mut header).is_err() || &header[0..4] != FRAME_MAGIC {
        return Ok(None);
    }
    let flags = u32::from_le_bytes(header[4..8].try_into().expect("four"));
    let generation = u64::from_le_bytes(header[8..16].try_into().expect("eight"));
    let len = u64::from(u32::from_le_bytes(header[16..20].try_into().expect("four")));
    let count = u32::from_le_bytes(header[20..24].try_into().expect("four"));
    let requires = u64::from_le_bytes(header[24..32].try_into().expect("eight"));
    // A frame pointing past the end of the file depends on records that are
    // not there: a recovery cut them off (a primary is cut back to its last
    // acknowledged sync), so the frame never counted.
    if requires > available || flags & !(FRAME_COMMIT | FRAME_WRAP | FRAME_INDEX) != 0 {
        return Ok(None);
    }
    let Some(next) = ring.after_frame(at, len) else {
        return Ok(None);
    };
    if generation != expected || generation > max_gen {
        return Ok(None);
    }
    let start = ring.at(at);
    let body_end = start + len - 4;
    let mut stream = Stream::new(start, body_end, scratch);
    if !stream.advance(&*source, FRAME_HEADER, None) {
        return Ok(None);
    }
    let mut index = None;
    if flags & FRAME_WRAP != 0 {
        if count != 0 || flags != FRAME_WRAP {
            return Ok(None);
        }
    } else if flags & FRAME_INDEX != 0 {
        let mut descriptor = [0u8; crate::width::small(INDEX_DESCRIPTOR)];
        if len != INDEX_FRAME_LEN || count != 0 || !stream.take(&*source, &mut descriptor) {
            return Ok(None);
        }
        let Some(root) = IndexRoot::decode(&descriptor) else {
            return Ok(None);
        };
        index = Some(root);
    } else {
        for _ in 0..count {
            let mut entry = [0u8; crate::width::small(ENTRY_HEADER)];
            if !stream.take(&*source, &mut entry) {
                return Ok(None);
            }
            let Some((target, n)) = stream_entry(&entry, stream.end - stream.pos) else {
                return Ok(None);
            };
            let bytes_at = stream.pos;
            if !stream.advance(&*source, u64::from(n), None) {
                return Ok(None);
            }
            if let Some(report) = report.as_deref_mut() {
                report(
                    source,
                    ScanEvent::Entry(
                        generation,
                        ScannedEntry {
                            target,
                            at: bytes_at,
                            len: n,
                        },
                    ),
                )?;
            }
        }
    }
    // Padding up to the checksum.
    let rest = stream.end - stream.pos;
    if !stream.advance(&*source, rest, None) {
        return Ok(None);
    }
    let mut stored = [0u8; 4];
    if source.read_at(body_end, &mut stored).is_err()
        || stream.hasher.finalize() != u32::from_le_bytes(stored)
    {
        return Ok(None);
    }
    if let (Some(root), Some(report)) = (index, report) {
        report(source, ScanEvent::Index(generation, root))?;
    }
    Ok(Some(if flags & FRAME_WRAP != 0 {
        Checked::Wrap
    } else {
        Checked::Frame {
            generation,
            commit: flags & FRAME_COMMIT != 0,
            next,
            len,
        }
    }))
}

/// Reads committed frames from ring offset `from`, whose first frame has
/// generation `first_gen`, reporting every entry and index of every
/// **committed** generation and each commit, in ring order. Stops at the
/// first frame that is torn, foreign or out of sequence. Frames of a
/// generation that never committed are not reported.
///
/// `max_gen` bounds what is reported (a reader stops at its snapshot).
/// Frames stream through `scratch`: a generation's frames are checked as
/// they are read and read again, to report them, once its commit frame
/// proves it whole. Nothing is allocated.
pub(crate) fn scan<S: TileSource + ?Sized>(
    source: &mut S,
    ring: Ring,
    from: u64,
    first_gen: u64,
    max_gen: u64,
    scratch: &mut [u8],
    visit: &mut dyn FnMut(&mut S, ScanEvent) -> Result<()>,
) -> Result<ScanEnd> {
    let frames = Frames {
        ring,
        available: source.len()?,
    };
    let mut at = from;
    let mut published = first_gen.saturating_sub(1);
    let mut head = from;
    let mut current: Option<u64> = None;
    // Where the generation being read started, and how far the scan had
    // walked then.
    let mut gen_start = from;
    let mut walked = 0u64;
    loop {
        if walked >= ring.area() {
            break;
        }
        if ring.area() - at < FRAME_MIN {
            walked += ring.area() - at;
            at = 0;
            continue;
        }
        let expected = current.unwrap_or(published + 1);
        let Some(checked) = check_frame(source, frames, at, expected, max_gen, scratch, None)?
        else {
            break;
        };
        match checked {
            Checked::Wrap => {
                walked += ring.area() - at;
                at = 0;
                continue;
            }
            Checked::Frame {
                generation,
                commit,
                next,
                len,
            } => {
                if current.is_none() {
                    gen_start = at;
                }
                walked += len;
                at = next;
                if commit {
                    if !replay(source, frames, gen_start, at, generation, scratch, visit)? {
                        break;
                    }
                    visit(source, ScanEvent::Committed(generation, at))?;
                    published = generation;
                    head = at;
                    current = None;
                } else {
                    current = Some(generation);
                }
            }
        }
        if at == from {
            break;
        }
    }
    Ok(ScanEnd { published, head })
}

/// Reads the frames of `generation` from `start` up to `end` again,
/// reporting their entries. `false` when a frame no longer checks out: the
/// ring changed under a reader since the first read, which its header shows.
fn replay<S: TileSource + ?Sized>(
    source: &mut S,
    frames: Frames,
    start: u64,
    end: u64,
    generation: u64,
    scratch: &mut [u8],
    visit: &mut dyn FnMut(&mut S, ScanEvent) -> Result<()>,
) -> Result<bool> {
    let ring = frames.ring;
    let mut at = start;
    for _ in 0..=ring.area() / FRAME_MIN {
        if at == end {
            return Ok(true);
        }
        if ring.area() - at < FRAME_MIN {
            at = 0;
            continue;
        }
        let report: &mut ScanVisit<'_, S> = &mut *visit;
        match check_frame(
            source,
            frames,
            at,
            generation,
            generation,
            scratch,
            Some(report),
        )? {
            None => return Ok(false),
            Some(Checked::Wrap) => at = 0,
            Some(Checked::Frame { next, .. }) => at = next,
        }
    }
    Ok(false)
}

/// Encodes a frame header in place; the caller appends entries and the CRC.
/// A frame's header. `requires` is the logical length the file had when the
/// frame was written: everything the frame's entries point into (tiles,
/// directory pages appended for it) lies before it.
pub(crate) fn frame_header(
    flags: u32,
    generation: u64,
    len: u64,
    count: u32,
    requires: u64,
) -> [u8; crate::width::small(FRAME_HEADER)] {
    let mut out = [0u8; crate::width::small(FRAME_HEADER)];
    out[0..4].copy_from_slice(FRAME_MAGIC);
    out[4..8].copy_from_slice(&flags.to_le_bytes());
    out[8..16].copy_from_slice(&generation.to_le_bytes());
    // A frame is at most a quarter of the ring, and a ring at most
    // MAX_RING_BYTES (64 MiB).
    #[allow(
        clippy::cast_possible_truncation,
        reason = "bounded by the ring, see above"
    )]
    let len = len as u32;
    out[16..20].copy_from_slice(&len.to_le_bytes());
    out[20..24].copy_from_slice(&count.to_le_bytes());
    out[24..32].copy_from_slice(&requires.to_le_bytes());
    out
}

/// A whole index frame naming `root`, CRC included.
pub(crate) fn index_frame(
    flags: u32,
    generation: u64,
    requires: u64,
    root: &IndexRoot,
) -> [u8; crate::width::small(INDEX_FRAME_LEN)] {
    let mut out = [0u8; crate::width::small(INDEX_FRAME_LEN)];
    let header = crate::width::small(FRAME_HEADER);
    out[..header].copy_from_slice(&frame_header(
        flags | FRAME_INDEX,
        generation,
        INDEX_FRAME_LEN,
        0,
        requires,
    ));
    out[header..header + crate::width::small(INDEX_DESCRIPTOR)].copy_from_slice(&root.encode());
    let body = out.len() - 4;
    let crc = crc32fast::hash(&out[..body]);
    out[body..].copy_from_slice(&crc.to_le_bytes());
    out
}

/// Where a read of `len` bytes at `offset` overlaps `held` bytes buffered
/// for `held_at`: `(at in the read, at in the buffer, length)`, or `None`
/// when they do not overlap (or a range's end would not fit a `u64`). The
/// writer serves its own reads of an open frame's buffered bytes with it.
pub(crate) fn overlap(
    offset: u64,
    len: usize,
    held_at: u64,
    held: usize,
) -> Option<(usize, usize, usize)> {
    let end = offset.checked_add(len as u64)?;
    let held_end = held_at.checked_add(held as u64)?;
    let from = offset.max(held_at);
    let to = end.min(held_end);
    if from >= to {
        return None;
    }
    // Each difference is at most `len` or `held`, both `usize`.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "each difference is at most `len` or `held`, which are `usize`"
    )]
    Some((
        (from - offset) as usize,
        (from - held_at) as usize,
        (to - from) as usize,
    ))
}

#[cfg(kani)]
#[path = "../verification/matrix_ring.rs"]
mod ring_proofs;

/// What [`copy_committed`] passes each piece of an entry to: the source, the
/// piece's target offset and its bytes.
pub(crate) type CopyPiece<'c, S> = dyn FnMut(&mut S, u64, &[u8]) -> Result<()> + 'c;

/// Reads the frames from ring offset `from` up to `end` once, in ring
/// order, and passes every entry's bytes to `copy` in pieces as they stream
/// through `scratch` (`copy(source, target, bytes)`), unless `skip(
/// generation, target, len)` says another entry rewrites them.
///
/// Nothing is checked: only the writer that published these frames, or
/// checked them when it opened, reads them this way, and nothing else
/// writes the ring between its tail and head. Index frames are refused:
/// their entries are not in the ring.
pub(crate) fn copy_committed<S: TileSource + ?Sized>(
    source: &mut S,
    ring: Ring,
    from: u64,
    end: u64,
    scratch: &mut [u8],
    skip: &mut dyn FnMut(u64, u64, u32) -> bool,
    copy: &mut CopyPiece<'_, S>,
) -> Result<()> {
    let bad = || crate::Error::InvalidMatrixLayout;
    let mut at = from;
    for _ in 0..=ring.area() / FRAME_MIN {
        if at == end {
            return Ok(());
        }
        if ring.area() - at < FRAME_MIN {
            at = 0;
            continue;
        }
        let mut header = [0u8; crate::width::small(FRAME_HEADER)];
        source.read_at(ring.at(at), &mut header)?;
        if &header[0..4] != FRAME_MAGIC {
            return Err(bad());
        }
        let flags = u32::from_le_bytes(header[4..8].try_into().expect("four"));
        let generation = u64::from_le_bytes(header[8..16].try_into().expect("eight"));
        let len = u64::from(u32::from_le_bytes(header[16..20].try_into().expect("four")));
        let count = u32::from_le_bytes(header[20..24].try_into().expect("four"));
        if flags & FRAME_WRAP != 0 {
            at = 0;
            continue;
        }
        if flags & FRAME_INDEX != 0 {
            return Err(bad());
        }
        let next = ring.after_frame(at, len).ok_or_else(bad)?;
        let start = ring.at(at);
        let mut stream = Stream::new(start + FRAME_HEADER, start + len - 4, scratch);
        for _ in 0..count {
            let mut entry = [0u8; crate::width::small(ENTRY_HEADER)];
            if !stream.take(&*source, &mut entry) {
                return Err(bad());
            }
            let (target, n) = stream_entry(&entry, stream.end - stream.pos).ok_or_else(bad)?;
            if skip(generation, target, n) {
                if !stream.advance(&*source, u64::from(n), None) {
                    return Err(bad());
                }
                continue;
            }
            let mut done = 0u64;
            while done < u64::from(n) {
                if stream.lo == stream.hi && !stream.fill(&*source) {
                    return Err(bad());
                }
                let take = crate::width::bounded(u64::from(n) - done, stream.hi - stream.lo);
                let piece = &stream.buf[stream.lo..stream.lo + take];
                copy(source, target + done, piece)?;
                stream.lo += take;
                stream.pos += take as u64;
                done += take as u64;
            }
        }
        at = next;
    }
    Err(bad())
}
