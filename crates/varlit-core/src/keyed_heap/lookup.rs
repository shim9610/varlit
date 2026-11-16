//! Finding a key's record, the same for the writer and for a reader: only
//! the [`View`] its bytes come through differs.

use super::{
    BUCKET_HEADER, BUCKET_PAGE, CHUNK_HEADER, Chunk, SLOT_LEN, Slot, State, corrupt, page_header,
    sip,
};
use crate::matrix_tiles::ring_store::RingStore;
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

/// The writer's view: what it staged over what it published.
pub(crate) struct StoreView<'a>(pub(crate) &'a RingStore);

impl View for StoreView<'_> {
    fn read(&mut self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read(source, at, buf)
    }
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
            let chunk = chunk(view, source, slot.at())?;
            if chunk.block_id == block_id
                && chunk.key_len as usize == key.len()
                && key_equals(view, source, slot.at() + CHUNK_HEADER, key)?
            {
                return Ok(Some(Located {
                    slot,
                    page: at,
                    index,
                    chunk,
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
    let mut crc = crc32fast::Hasher::new();
    crc.update(&found.chunk.encode()[..12]);
    let mut step = [0u8; STEP];
    let mut done = 0u64;
    let key_len = u64::from(found.chunk.key_len);
    while done < key_len {
        let n = crate::width::bounded(key_len - done, STEP);
        view.read(
            source,
            found.slot.at() + CHUNK_HEADER + done,
            &mut step[..n],
        )?;
        crc.update(&step[..n]);
        done += n as u64;
    }
    crc.update(out);
    if crc.finalize() != found.chunk.crc {
        return Err(Error::ChecksumMismatch {
            offset: found.slot.at(),
        });
    }
    Ok(())
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
