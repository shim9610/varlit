//! Finding a key's record, the same for the writer and for a reader: only
//! the [`View`] its bytes come through differs.

use super::{
    BUCKET_HEADER, BUCKET_PAGE, CHUNK_HEADER, Chunk, SLOT_LEN, Slot, State, corrupt, page_header,
    sip,
};
use crate::matrix_tiles::store::TileSource;
use crate::matrix_tiles::view::{RingReader, directory_offset};
use crate::matrix_tiles::{DIR_FANOUT, DIR_LEVEL_BITS};
use crate::{Error, Result};

/// Bytes read per step when a key is compared or checksummed.
const STEP: usize = 4096;

/// Reads the file as one snapshot shows it.
pub(crate) trait View {
    fn read(&mut self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()>;
}

/// A reader's view: its snapshot, checked on every read against the ring
/// moving past it.
impl View for RingReader {
    fn read(&mut self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        Self::read(self, source, at, buf)
    }
}

/// Where a key's record is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Located {
    pub slot: Slot,
    /// The bucket page holding the slot, and its index there.
    pub page: u64,
    pub index: usize,
    pub chunk: Chunk,
    /// The checksum of the chunk's header fields and key, which the payload
    /// continues.
    pub prefix_crc: u32,
}

impl Located {
    /// Where the payload starts.
    pub(crate) const fn payload_at(&self) -> u64 {
        self.slot.at() + CHUNK_HEADER + self.chunk.key_len as u64
    }
}

/// The low 32 bits of a key's seeded hash: its route and fingerprint.
pub(crate) fn route(seed: [u64; 2], block_id: u32, key: &[u8]) -> u32 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the low 32 bits route and fingerprint"
    )]
    let route = sip::hash(seed, &[&block_id.to_le_bytes(), key]) as u32;
    route
}

/// Finds `key` of `block_id`.
pub(crate) fn find(
    view: &mut dyn View,
    source: &dyn TileSource,
    seed: [u64; 2],
    state: &State,
    block_id: u32,
    key: &[u8],
) -> Result<Option<Located>> {
    let route = route(seed, block_id, key);
    let mut at = bucket(view, source, state, state.bucket_of(route))?;
    let mut page = [0u8; crate::width::small(BUCKET_PAGE)];
    while at != 0 {
        view.read(source, at, &mut page)?;
        let (count, next) = page_header(&page)?;
        for index in 0..count {
            let slot = slot_at(&page, index);
            if slot.route != route {
                continue;
            }
            if let Some((chunk, prefix_crc)) = record_at(view, source, slot.at(), block_id, key)? {
                return Ok(Some(Located {
                    slot,
                    page: at,
                    index,
                    chunk,
                    prefix_crc,
                }));
            }
        }
        at = next;
    }
    Ok(None)
}

/// Reads a located record's payload into `out`, which must be its length,
/// and checks the chunk's checksum.
pub(crate) fn read_payload(
    view: &mut dyn View,
    source: &dyn TileSource,
    found: &Located,
    out: &mut [u8],
) -> Result<()> {
    if out.len() != found.chunk.payload_len as usize {
        return Err(Error::BufferTooSmall {
            resource: "keyed record payload",
            required: found.chunk.payload_len as usize,
            provided: out.len(),
        });
    }
    view.read(source, found.payload_at(), out)?;
    let mut crc = crc32fast::Hasher::new_with_initial(found.prefix_crc);
    crc.update(out);
    if crc.finalize() != found.chunk.crc {
        return Err(Error::ChecksumMismatch {
            offset: found.slot.at(),
        });
    }
    Ok(())
}

/// The chunk at `at` when it holds `key` of `block_id`, with the checksum
/// of its header fields and key. A key that fits one step is read with the
/// header in one read.
fn record_at(
    view: &mut dyn View,
    source: &dyn TileSource,
    at: u64,
    block_id: u32,
    key: &[u8],
) -> Result<Option<(Chunk, u32)>> {
    let header_len = crate::width::small(CHUNK_HEADER);
    let mut step = [0u8; STEP];
    let end = header_len + key.len();
    // A chunk shorter than the combined read (another key's, under a
    // colliding route) can end at the end of the file: when that read
    // fails, the header and the key are read on their own.
    let together = end <= STEP && view.read(source, at, &mut step[..end]).is_ok();
    if !together {
        view.read(source, at, &mut step[..header_len])?;
    }
    let chunk = Chunk::decode(step[..header_len].try_into().expect("header"));
    if chunk.block_id != block_id || chunk.key_len as usize != key.len() {
        return Ok(None);
    }
    let matches = if together {
        step[header_len..end] == *key
    } else {
        key_equals(view, source, at + CHUNK_HEADER, key)?
    };
    if !matches {
        return Ok(None);
    }
    let mut crc = crc32fast::Hasher::new();
    crc.update(&chunk.encode()[..12]);
    crc.update(key);
    Ok(Some((chunk, crc.finalize())))
}

pub(crate) fn chunk(view: &mut dyn View, source: &dyn TileSource, at: u64) -> Result<Chunk> {
    let mut header = [0u8; crate::width::small(CHUNK_HEADER)];
    view.read(source, at, &mut header)?;
    Ok(Chunk::decode(&header))
}

fn key_equals(view: &mut dyn View, source: &dyn TileSource, at: u64, key: &[u8]) -> Result<bool> {
    let mut step = [0u8; STEP];
    for (index, piece) in key.chunks(STEP).enumerate() {
        let read = &mut step[..piece.len()];
        view.read(source, at + (index * STEP) as u64, read)?;
        if read != piece {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The first page of bucket `bucket`.
pub(crate) fn bucket(
    view: &mut dyn View,
    source: &dyn TileSource,
    state: &State,
    bucket: u64,
) -> Result<u64> {
    let (mut page, height) = state.dir;
    if height < 8 && bucket >> (u64::from(DIR_LEVEL_BITS) * height) != 0 {
        return Err(corrupt("keyed bucket directory"));
    }
    for level in (0..height).rev() {
        let index = (bucket >> (u64::from(DIR_LEVEL_BITS) * level)) & (DIR_FANOUT - 1);
        let mut entry = [0u8; 8];
        view.read(source, page + index * 8, &mut entry)?;
        page = directory_offset(u64::from_le_bytes(entry))?;
        if page == 0 {
            return Err(corrupt("keyed bucket directory"));
        }
    }
    Ok(page)
}

pub(crate) fn slot_at(page: &[u8], index: usize) -> Slot {
    let start = crate::width::small(BUCKET_HEADER) + index * crate::width::small(SLOT_LEN);
    Slot::decode(&page[start..start + crate::width::small(SLOT_LEN)])
}
