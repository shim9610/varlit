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
    pub(crate) fn encode(&self) -> [u8; HEADER_SLOT as usize] {
        let mut out = [0u8; HEADER_SLOT as usize];
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
        let header = Self {
            seq: field(0),
            checkpointed: field(1),
            mark: field(2),
            reclaimed: field(3),
            tail: field(4),
            tail_gen: field(5),
            head: field(6),
            published: field(7),
        };
        (header.reclaimed <= header.checkpointed
            && header.checkpointed <= header.mark.max(header.checkpointed)
            && header.checkpointed <= header.published)
            .then_some(header)
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
        let mut bytes = [0u8; 2 * HEADER_SLOT as usize];
        for _ in 0..8 {
            source.read_at(self.base, &mut bytes)?;
            let first = RingHeader::decode(&bytes[..HEADER_SLOT as usize]);
            let second = RingHeader::decode(&bytes[HEADER_SLOT as usize..]);
            match (first, second) {
                (Some(a), Some(b)) => return Ok(if a.seq >= b.seq { a } else { b }),
                (Some(a), None) | (None, Some(a)) => return Ok(a),
                (None, None) => std::hint::spin_loop(),
            }
        }
        Err(Error::InvalidMatrixLayout)
    }

    /// The logical offset of the slot a header of sequence `seq` goes to.
    pub(crate) const fn slot_for(&self, seq: u64) -> u64 {
        self.base + (seq % 2) * HEADER_SLOT
    }
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
    let mut header = [0u8; FRAME_HEADER as usize];
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
        if len < FRAME_HEADER + 4 || len > ring.area() - at || len % 8 != 0 {
            break;
        }
        let expected = current.unwrap_or(published + 1);
        if generation != expected || generation > max_gen {
            break;
        }
        // The whole frame, checksummed before any of it is believed.
        if frame_buf.len() < len as usize {
            frame_buf
                .try_reserve_exact(len as usize - frame_buf.len())
                .map_err(|_| Error::AllocationFailed {
                    resource: "matrix ring frame",
                    requested: len,
                })?;
            frame_buf.resize(len as usize, 0);
        }
        let frame = &mut frame_buf[..len as usize];
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
        let mut cursor = FRAME_HEADER as usize;
        let mut ok = true;
        for _ in 0..count {
            if cursor + ENTRY_HEADER as usize > body_end {
                ok = false;
                break;
            }
            let target = u64::from_le_bytes(frame[cursor..cursor + 8].try_into().expect("eight"));
            let n = u32::from_le_bytes(frame[cursor + 8..cursor + 12].try_into().expect("four"));
            let bytes_at = cursor + ENTRY_HEADER as usize;
            if bytes_at + n as usize > body_end {
                ok = false;
                break;
            }
            held.try_reserve(1).map_err(|_| Error::AllocationFailed {
                resource: "matrix ring scan",
                requested: (held.len() as u64 + 1) * 24,
            })?;
            held.push(ScannedEntry {
                target,
                at: ring.at(at) + bytes_at as u64,
                len: n,
            });
            cursor = bytes_at + n as usize;
        }
        if !ok {
            break;
        }
        walked += len;
        at += len;
        if at == ring.area() {
            at = 0;
        }
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
) -> [u8; FRAME_HEADER as usize] {
    let mut out = [0u8; FRAME_HEADER as usize];
    out[0..4].copy_from_slice(FRAME_MAGIC);
    out[4..8].copy_from_slice(&flags.to_le_bytes());
    out[8..16].copy_from_slice(&generation.to_le_bytes());
    out[16..20].copy_from_slice(&(len as u32).to_le_bytes());
    out[20..24].copy_from_slice(&count.to_le_bytes());
    out[24..32].copy_from_slice(&requires.to_le_bytes());
    out
}
