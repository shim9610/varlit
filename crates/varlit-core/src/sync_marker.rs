//! Sync markers: the durable commit boundary of a checkpointed stream primary.
//!
//! A checkpointed writer (indexed, or stream with a state sidecar) publishes a
//! sync with one storage barrier, on the primary. Its sidecar is confirmed —
//! written and synced — only every so often, so after a crash the sidecar can
//! cover less than the primary acknowledged. What the sidecar does not cover is
//! recovered from the primary, and the primary has to say how far its
//! acknowledged prefix reaches without trusting anything the barrier did not
//! cover. That is this record.
//!
//! Each sync appends one marker after the records it commits. The marker names
//! where its chain link starts and carries the CRC-32 of every byte from there
//! to its own first byte. A link starts where the previous marker ended — or at
//! the sidecar's confirmed EOF, which is always such a boundary — so a walk from
//! a confirmed EOF accepts a prefix only up to the last marker whose bytes it
//! has verified. Bytes after that marker were never acknowledged, whatever they
//! look like: a crash can persist a record header whose payload pages never
//! reached storage, and without a checksum policy nothing else could tell.
//!
//! The marker is independent of the format's integrity policy: it checksums the
//! physical bytes, header and footer included, so it works for formats with no
//! record checksum at all.
//!
//! One exception: the payload of a matrix tile record
//! ([`crate::file::MATRIX_TILE_BLOCK_ID`]) is rewritten in place after it is
//! appended, so a link leaves it out — the record's header and footer stay
//! in, so the walk still verifies where every record starts. The matrix keeps
//! its own freshness (its ring) and its own checksums (per cell, per frame).

use crate::{Error, Result, snapshot::SnapshotFile};

/// Internal block id of a sync marker record.
pub const SYNC_BLOCK_ID: u32 = 0xFFFF_FFF4;

pub(crate) const SYNC_MAGIC: &[u8; 4] = b"VSYN";
pub(crate) const SYNC_PAYLOAD_LEN: usize = 32;
const SYNC_VERSION: u16 = 1;

/// The running link a writer extends with every byte it appends.
#[derive(Clone, Debug)]
pub(crate) struct SyncChain {
    start: u64,
    hasher: crc32fast::Hasher,
}

impl SyncChain {
    pub(crate) fn new(start: u64) -> Self {
        Self {
            start,
            hasher: crc32fast::Hasher::new(),
        }
    }

    pub(crate) const fn start(&self) -> u64 {
        self.start
    }

    #[cfg(test)]
    pub(crate) fn extend(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    /// Extends with a chunk appended at `chunk_at` whose records are
    /// `records`, leaving out the payload of each in-place record.
    pub(crate) fn extend_chunk(
        &mut self,
        chunk_at: u64,
        bytes: &[u8],
        records: &[(u32, crate::AppendInfo)],
    ) {
        let mut at = 0usize;
        for (block_id, info) in records {
            if !rewritten_in_place(*block_id) {
                continue;
            }
            let start = (info.payload_offset - chunk_at) as usize;
            let end = start + info.payload_len as usize;
            self.hasher.update(&bytes[at..start]);
            at = end;
        }
        self.hasher.update(&bytes[at..]);
    }

    /// The payload of a marker written at `marker_offset`, which must be the
    /// end of every byte this link has seen.
    pub(crate) fn payload(&self, marker_offset: u64) -> Result<[u8; SYNC_PAYLOAD_LEN]> {
        let len = marker_offset
            .checked_sub(self.start)
            .ok_or(Error::InvalidCanonicalEncoding(
                "sync marker precedes its chain link",
            ))?;
        Ok(encode(self.start, len, self.hasher.clone().finalize()))
    }
}

fn encode(start: u64, len: u64, crc: u32) -> [u8; SYNC_PAYLOAD_LEN] {
    let mut bytes = [0u8; SYNC_PAYLOAD_LEN];
    bytes[0..4].copy_from_slice(SYNC_MAGIC);
    bytes[4..6].copy_from_slice(&SYNC_VERSION.to_le_bytes());
    bytes[8..16].copy_from_slice(&start.to_le_bytes());
    bytes[16..24].copy_from_slice(&len.to_le_bytes());
    bytes[24..28].copy_from_slice(&crc.to_le_bytes());
    let check = crc32fast::hash(&bytes[..28]);
    bytes[28..32].copy_from_slice(&check.to_le_bytes());
    bytes
}

/// A decoded marker payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SyncMarker {
    pub(crate) start: u64,
    pub(crate) len: u64,
    pub(crate) crc: u32,
}

pub(crate) fn decode(bytes: &[u8]) -> Option<SyncMarker> {
    if bytes.len() != SYNC_PAYLOAD_LEN
        || &bytes[0..4] != SYNC_MAGIC
        || u16::from_le_bytes([bytes[4], bytes[5]]) != SYNC_VERSION
        || bytes[6..8] != [0; 2]
    {
        return None;
    }
    let check = u32::from_le_bytes(bytes[28..32].try_into().expect("fixed slice"));
    if crc32fast::hash(&bytes[..28]) != check {
        return None;
    }
    Some(SyncMarker {
        start: u64::from_le_bytes(bytes[8..16].try_into().expect("fixed slice")),
        len: u64::from_le_bytes(bytes[16..24].try_into().expect("fixed slice")),
        crc: u32::from_le_bytes(bytes[24..28].try_into().expect("fixed slice")),
    })
}

/// Whether records of `block_id` have their payload rewritten in place, and
/// so left out of a link's checksum.
pub(crate) fn rewritten_in_place(block_id: u32) -> bool {
    block_id == crate::file::MATRIX_TILE_BLOCK_ID
}

/// Checks that the marker record at `marker_offset` closes the link starting
/// at `start`: it must name that start, span exactly up to itself, and the
/// bytes it spans must hash to its CRC. Reads the link in bounded chunks.
///
/// With `in_place` the link holds records rewritten in place: it is framed
/// again from `start` (headers and footers only) to find their payloads,
/// which the checksum leaves out.
pub(crate) fn verify_link(
    spec: crate::FormatSpec,
    snapshot: &SnapshotFile,
    start: u64,
    marker_offset: u64,
    marker: SyncMarker,
    in_place: bool,
) -> Result<bool> {
    if marker.start != start || marker_offset.checked_sub(start) != Some(marker.len) {
        return Ok(false);
    }
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(LINK_READ)
        .map_err(|_| Error::AllocationFailed {
            resource: "sync marker link read",
            requested: LINK_READ as u64,
        })?;
    buffer.resize(LINK_READ, 0);
    let mut hash = |from: u64, to: u64| -> Result<()> {
        let mut at = from;
        while at < to {
            let take = usize::try_from((to - at).min(buffer.len() as u64))
                .expect("bounded by buffer length");
            snapshot.read_exact_at(at, &mut buffer[..take])?;
            hasher.update(&buffer[..take]);
            at += take as u64;
        }
        Ok(())
    };
    if !in_place {
        hash(start, marker_offset)?;
        return Ok(hasher.finalize() == marker.crc);
    }
    let mut scanner =
        crate::file::NativeStreamScanner::from_offset_framing_only(spec, snapshot.clone(), start)?;
    let mut at = start;
    loop {
        let entry = match scanner.next_entry() {
            Ok(Some(entry)) if entry.record_offset < marker_offset => entry,
            Ok(_) => break,
            Err(Error::Io(error)) => return Err(Error::Io(error)),
            // A link that no longer frames is not one this marker closes.
            Err(_) => return Ok(false),
        };
        if rewritten_in_place(entry.block_id) {
            let payload_end = entry
                .payload_offset
                .checked_add(entry.payload_len)
                .filter(|end| *end <= marker_offset && entry.payload_offset >= at)
                .ok_or(Error::InvalidCanonicalEncoding(
                    "in-place record payload outside its link",
                ))?;
            hash(at, entry.payload_offset)?;
            at = payload_end;
        }
    }
    hash(at, marker_offset)?;
    Ok(hasher.finalize() == marker.crc)
}

/// Bytes of a link read and hashed at a time.
const LINK_READ: usize = 256 * 1024;

/// A forward walk over a checkpointed primary's unconfirmed tail that accepts
/// records only up to the last verified sync marker.
///
/// `committed` is always a chain boundary: the confirmed EOF the walk started
/// from, or the end of the last marker it verified. `frontier` is how far
/// framing has reached past it. Framing reads headers and footers only, so a
/// record still being written is either framed whole or stops the walk; its
/// payload is hashed only once a marker after it exists, and a marker is
/// written after every byte it covers. Each call therefore frames new bytes
/// once and hashes each committed byte once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MarkedTail {
    committed: u64,
    frontier: u64,
    /// Whether framing met a record rewritten in place since `committed`:
    /// the next marker's link must then be framed again to leave its
    /// payload out.
    in_place: bool,
}

impl MarkedTail {
    pub(crate) const fn new(confirmed_eof: u64) -> Self {
        Self {
            committed: confirmed_eof,
            frontier: confirmed_eof,
            in_place: false,
        }
    }

    pub(crate) const fn committed(&self) -> u64 {
        self.committed
    }

    /// Frames forward from the frontier through `snapshot` and moves
    /// `committed` to the end of the last marker whose link verifies.
    /// Returns whether `committed` moved. A record that does not frame, or a
    /// marker that does not verify, ends the walk without error: neither is
    /// part of an acknowledged prefix. Storage errors are returned.
    pub(crate) fn advance(
        &mut self,
        spec: crate::FormatSpec,
        snapshot: &SnapshotFile,
    ) -> Result<bool> {
        if self.frontier >= snapshot.len() {
            return Ok(false);
        }
        let mut scanner = crate::file::NativeStreamScanner::from_offset_framing_only(
            spec,
            snapshot.clone(),
            self.frontier,
        )?;
        let mut moved = false;
        loop {
            let entry = match scanner.next_entry() {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(Error::Io(error)) => return Err(Error::Io(error)),
                Err(_) => break,
            };
            let end = entry.checked_physical_end()?;
            if entry.block_id == SYNC_BLOCK_ID {
                if entry.payload_len != SYNC_PAYLOAD_LEN as u64 {
                    break;
                }
                let mut payload = [0u8; SYNC_PAYLOAD_LEN];
                snapshot.read_exact_at(entry.payload_offset, &mut payload)?;
                let verified = match decode(&payload) {
                    Some(marker) => verify_link(
                        spec,
                        snapshot,
                        self.committed,
                        entry.record_offset,
                        marker,
                        self.in_place,
                    )?,
                    None => false,
                };
                if !verified {
                    // Only the sync in flight at a crash can be torn: every
                    // earlier one completed its barrier before the next began.
                    // A complete marker after this one therefore means this
                    // link was damaged after it was durable — corruption, which
                    // must not be cut away as if it were a torn tail.
                    if later_marker(&mut scanner)? {
                        return Err(Error::ChecksumMismatch {
                            offset: entry.record_offset,
                        });
                    }
                    break;
                }
                self.committed = end;
                self.in_place = false;
                moved = true;
            } else if rewritten_in_place(entry.block_id) {
                self.in_place = true;
            }
            self.frontier = end;
        }
        Ok(moved)
    }
}

/// Whether a sync marker frames anywhere after the scanner's position.
fn later_marker(scanner: &mut crate::file::NativeStreamScanner) -> Result<bool> {
    loop {
        match scanner.next_entry() {
            Ok(Some(entry)) if entry.block_id == SYNC_BLOCK_ID => return Ok(true),
            Ok(Some(_)) => {}
            Ok(None) => return Ok(false),
            Err(Error::Io(error)) => return Err(Error::Io(error)),
            Err(_) => return Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trips_and_rejects_damage() {
        let mut chain = SyncChain::new(100);
        chain.extend(b"abc");
        let payload = chain.payload(103).unwrap();
        let marker = decode(&payload).unwrap();
        assert_eq!(marker.start, 100);
        assert_eq!(marker.len, 3);
        assert_eq!(marker.crc, crc32fast::hash(b"abc"));
        for index in 0..SYNC_PAYLOAD_LEN {
            let mut damaged = payload;
            damaged[index] ^= 1;
            assert!(decode(&damaged).is_none(), "byte {index}");
        }
        assert!(chain.payload(99).is_err());
    }
}
