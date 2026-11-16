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

/// What [`scan`] reports, in ring order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanEvent {
    /// An entry of a committed generation.
    Entry(u64, ScannedEntry),
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

/// Reads committed frames from ring offset `from`, whose first frame has
/// generation `first_gen`, reporting every entry of every **committed**
/// generation and each commit, in ring order. Stops at the first frame
/// that is torn, foreign, out of sequence, or at `stop` (exclusive ring
/// offset) when given. Frames of a generation that never committed are not
/// reported.
///
/// `max_gen` bounds what is reported (a reader stops at its snapshot).
pub(crate) fn scan(
    source: &dyn TileSource,
    ring: Ring,
    from: u64,
    first_gen: u64,
    max_gen: u64,
    visit: &mut dyn FnMut(ScanEvent) -> Result<()>,
) -> Result<ScanEnd> {
    let mut at = from;
    let mut published = first_gen.saturating_sub(1);
    let mut head = from;
    let mut current: Option<u64> = None;
    // Entries of the generation being read, reported once it commits.
    let mut held: Vec<ScannedEntry> = Vec::new();
    let mut walked = 0u64;
    let mut header = [0u8; crate::width::small(FRAME_HEADER)];
    let available = source.len()?;
    // One frame buffer for the whole scan, grown to the largest frame seen;
    // a read fills what it uses, so it is zeroed only when it grows.
    let mut frame_buf: Vec<u8> = Vec::new();
    loop {
        if walked >= ring.area() {
            break;
        }
        if ring.area() - at < FRAME_MIN {
            walked += ring.area() - at;
            at = 0;
            continue;
        }
        if source.read_at(ring.at(at), &mut header).is_err() || &header[0..4] != FRAME_MAGIC {
            break;
        }
        let flags = u32::from_le_bytes(header[4..8].try_into().expect("four"));
        let generation = u64::from_le_bytes(header[8..16].try_into().expect("eight"));
        let len = u64::from(u32::from_le_bytes(header[16..20].try_into().expect("four")));
        let count = u32::from_le_bytes(header[20..24].try_into().expect("four"));
        let requires = u64::from_le_bytes(header[24..32].try_into().expect("eight"));
        // A frame pointing past the end of the file depends on records that
        // are not there: a recovery cut them off (a primary is cut back to
        // its last acknowledged sync), so the frame never counted.
        if requires > available {
            break;
        }
        let Some(next) = ring.after_frame(at, len) else {
            break;
        };
        let expected = current.unwrap_or(published + 1);
        if generation != expected || generation > max_gen {
            break;
        }
        // The whole frame, checksummed before any of it is believed.
        if frame_buf.len() < crate::width::index(len)? {
            frame_buf
                .try_reserve_exact(crate::width::index(len)? - frame_buf.len())
                .map_err(|_| Error::AllocationFailed {
                    resource: "matrix ring frame",
                    requested: len,
                })?;
            frame_buf.resize(crate::width::index(len)?, 0);
        }
        let frame = &mut frame_buf[..crate::width::index(len)?];
        if source.read_at(ring.at(at), frame).is_err() {
            break;
        }
        let body_end = frame.len() - 4;
        let stored = u32::from_le_bytes(frame[body_end..].try_into().expect("four"));
        if crc32fast::hash(&frame[..body_end]) != stored {
            break;
        }
        if flags & FRAME_WRAP != 0 {
            if count != 0 {
                break;
            }
            walked += ring.area() - at;
            at = 0;
            continue;
        }
        let mut cursor = crate::width::small(FRAME_HEADER);
        let mut ok = true;
        for _ in 0..count {
            let Some(entry) = frame_entry(&frame[..body_end], cursor) else {
                ok = false;
                break;
            };
            held.try_reserve(1).map_err(|_| Error::AllocationFailed {
                resource: "matrix ring scan",
                requested: (held.len() as u64 + 1) * 24,
            })?;
            held.push(ScannedEntry {
                target: entry.target,
                at: ring.at(at) + entry.bytes_at as u64,
                len: entry.len,
            });
            cursor = entry.next;
        }
        if !ok {
            break;
        }
        walked += len;
        at = next;
        if flags & FRAME_COMMIT != 0 {
            for scanned in held.drain(..) {
                visit(ScanEvent::Entry(generation, scanned))?;
            }
            visit(ScanEvent::Committed(generation, at))?;
            published = generation;
            head = at;
            current = None;
        } else {
            current = Some(generation);
        }
        if at == from {
            break;
        }
    }
    Ok(ScanEnd { published, head })
}

/// One entry of a frame body: where it goes, where its bytes are in the body
/// and where the next entry starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameEntry {
    pub target: u64,
    pub len: u32,
    pub bytes_at: usize,
    pub next: usize,
}

/// The entry at `cursor` of a frame `body` (a frame without its trailing
/// checksum), if its header and all its bytes lie inside the body.
pub(crate) fn frame_entry(body: &[u8], cursor: usize) -> Option<FrameEntry> {
    let bytes_at = cursor.checked_add(crate::width::small(ENTRY_HEADER))?;
    let header = body.get(cursor..bytes_at)?;
    let target = u64::from_le_bytes(header[..8].try_into().expect("eight"));
    let len = u32::from_le_bytes(header[8..12].try_into().expect("four"));
    let next = bytes_at.checked_add(usize::try_from(len).ok()?)?;
    if next > body.len() {
        return None;
    }
    Some(FrameEntry {
        target,
        len,
        bytes_at,
        next,
    })
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

#[cfg(kani)]
#[path = "../verification/matrix_ring.rs"]
mod ring_proofs;
