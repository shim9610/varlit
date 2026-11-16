//! Keyed records stored in place: a dense record heap found through slot
//! buckets (see `docs/design/bounded-keyed-storage.md`).
//!
//! Every structure lives in internal in-place records of the primary file and
//! changes through the write-ahead ring of a
//! [`RingStore`](crate::matrix_tiles::ring_store::RingStore), exactly as
//! matrix tiles do, so a publication is atomic and a reader keeps its
//! snapshot until the ring moves past it.
//!
//! - **Metadata** (one record): the ring's place, the free-space ratio, the
//!   per-file hash seed, and the mutable state: the bucket directory root,
//!   the linear-hashing level and split pointer, counters and list heads.
//! - **Heap regions**: records packed densely. A key's record is overwritten
//!   where it is when the new value fits; otherwise it moves and its old
//!   place becomes a hole. Every chunk of a region starts with a header that
//!   gives its length, so a region can be walked; a free chunk is marked.
//! - **Slot buckets**: 4 KiB pages of 16-byte slots (hash low bits, region,
//!   offset in the region), found through a radix directory by bucket
//!   number. Buckets grow one at a time by linear hashing.
//!
//! A slot is the only thing that makes a record live: a chunk no slot points
//! at is free whatever its header says. Holes are reused through a fixed
//! cache, and when holes exceed the declared ratio of live bytes the writer
//! empties the sparsest region by moving its live records, after which the
//! whole region is reused.

pub(crate) mod heap;
#[cfg(kani)]
#[path = "../verification/keyed_heap.rs"]
mod heap_proofs;
pub(crate) mod lookup;
pub(crate) mod pages;
pub(crate) mod reader;
pub(crate) mod sip;
#[cfg(test)]
mod tests;

use crate::{Error, Result};

pub(crate) const META_MAGIC: &[u8; 4] = b"VKMT";
pub(crate) const META_VERSION: u16 = 1;
/// The fixed, checksummed part of the metadata record.
pub(crate) const META_FIXED_LEN: u64 = 64;
/// Where the ring keeps its overflow list head: the first mutable field.
pub(crate) const META_OVERFLOW_HEAD_AT: u64 = META_FIXED_LEN;
/// The mutable state, changed through the ring.
pub(crate) const META_STATE_AT: u64 = META_OVERFLOW_HEAD_AT + 8;
pub(crate) const META_STATE_LEN: u64 = State::FIELDS as u64 * 8;
pub(crate) const META_LEN: u64 = META_STATE_AT + META_STATE_LEN;

pub(crate) const REGION_MAGIC: &[u8; 4] = b"VHRG";
/// `magic | 0 | len | live | next_all | next_free | freed`: a multiple of
/// the chunk alignment, so chunks stay aligned.
pub(crate) const REGION_HEADER: u64 = 48;
/// Payload bytes of an ordinary heap region. A record larger than half of
/// this gets a region of its own.
pub(crate) const REGION_BYTES: u64 = 1 << 20;

/// `payload_len | block_id | key_len | crc32`.
pub(crate) const CHUNK_HEADER: u64 = 16;
/// The block id of a free chunk; its payload length is the chunk's length
/// less the header.
pub(crate) const FREE_BLOCK: u32 = u32::MAX;

pub(crate) const BUCKET_MAGIC: &[u8; 4] = b"VBKT";
pub(crate) const BUCKET_PAGE: u64 = 4096;
/// `magic | count | next page`.
pub(crate) const BUCKET_HEADER: u64 = 16;
/// `hash low bits | offset in region | region`.
pub(crate) const SLOT_LEN: u64 = 16;
pub(crate) const SLOTS_PER_PAGE: usize = ((BUCKET_PAGE - BUCKET_HEADER) / SLOT_LEN) as usize;

/// The fixed part of the metadata record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fixed {
    /// Free-space ratio in thousandths of live bytes.
    pub ratio_permille: u16,
    pub seed: [u64; 2],
    pub ring: (u64, u64),
}

impl Fixed {
    pub(crate) fn encode(&self) -> [u8; crate::width::small(META_FIXED_LEN)] {
        let mut out = [0u8; crate::width::small(META_FIXED_LEN)];
        out[0..4].copy_from_slice(META_MAGIC);
        out[4..6].copy_from_slice(&META_VERSION.to_le_bytes());
        out[6..8].copy_from_slice(&self.ratio_permille.to_le_bytes());
        out[8..16].copy_from_slice(&self.seed[0].to_le_bytes());
        out[16..24].copy_from_slice(&self.seed[1].to_le_bytes());
        out[24..32].copy_from_slice(&self.ring.0.to_le_bytes());
        out[32..40].copy_from_slice(&self.ring.1.to_le_bytes());
        let crc = crc32fast::hash(&out[..56]);
        out[56..60].copy_from_slice(&crc.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8; crate::width::small(META_FIXED_LEN)]) -> Result<Self> {
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight"));
        if &bytes[0..4] != META_MAGIC
            || u16::from_le_bytes([bytes[4], bytes[5]]) != META_VERSION
            || crc32fast::hash(&bytes[..56])
                != u32::from_le_bytes(bytes[56..60].try_into().expect("four"))
        {
            return Err(corrupt("keyed storage metadata"));
        }
        let ratio_permille = u16::from_le_bytes([bytes[6], bytes[7]]);
        let ring = (word(24), word(32));
        if !(10..=50).contains(&ratio_permille)
            || !(crate::matrix_tiles::wal::MIN_RING..=crate::matrix_tiles::wal::MAX_RING_BYTES)
                .contains(&ring.1)
        {
            return Err(corrupt("keyed storage metadata"));
        }
        Ok(Self {
            ratio_permille,
            seed: [word(8), word(16)],
            ring,
        })
    }
}

/// The mutable state in the metadata record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct State {
    /// Bucket directory root: `(page, height)`.
    pub dir: (u64, u64),
    /// Linear hashing: buckets are `2^level + split`.
    pub level: u64,
    pub split: u64,
    /// Live keys.
    pub keys: u64,
    /// Bytes of live chunks.
    pub live: u64,
    /// Payload bytes of every heap region.
    pub heap: u64,
    /// The region new chunks are appended to, and how much of it is used.
    pub cur: u64,
    pub cur_used: u64,
    /// Newest region (each links the older one), and the lists of free
    /// regions and free bucket pages.
    pub regions: u64,
    pub free_regions: u64,
    /// The last free region: freed regions join the queue here.
    pub free_tail: u64,
    pub free_pages: u64,
    /// Payload bytes of the regions on the free list.
    pub free: u64,
}

impl State {
    pub(crate) const FIELDS: usize = 14;

    pub(crate) fn encode(&self) -> [u8; Self::FIELDS * 8] {
        let fields = [
            self.dir.0,
            self.dir.1,
            self.level,
            self.split,
            self.keys,
            self.live,
            self.heap,
            self.cur,
            self.cur_used,
            self.regions,
            self.free_regions,
            self.free_pages,
            self.free,
            self.free_tail,
        ];
        let mut out = [0u8; Self::FIELDS * 8];
        for (index, field) in fields.into_iter().enumerate() {
            out[index * 8..index * 8 + 8].copy_from_slice(&field.to_le_bytes());
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8; Self::FIELDS * 8]) -> Result<Self> {
        let word = |index: usize| {
            u64::from_le_bytes(bytes[index * 8..index * 8 + 8].try_into().expect("eight"))
        };
        let state = Self {
            dir: (word(0), word(1)),
            level: word(2),
            split: word(3),
            keys: word(4),
            live: word(5),
            heap: word(6),
            cur: word(7),
            cur_used: word(8),
            regions: word(9),
            free_regions: word(10),
            free_pages: word(11),
            free: word(12),
            free_tail: word(13),
        };
        if state.level >= 32
            || state.split >= 1u64 << state.level
            || state
                .live
                .checked_add(state.free)
                .is_none_or(|used| used > state.heap)
            || state.dir.0 == 0
            || state.dir.1 == 0
            || state.dir.1 > crate::matrix_tiles::view::DIR_MAX_HEIGHT
        {
            return Err(corrupt("keyed storage state"));
        }
        Ok(state)
    }

    /// Buckets in the table.
    pub(crate) const fn buckets(&self) -> u64 {
        (1u64 << self.level) + self.split
    }

    /// Bytes of the heap no live chunk, free region or unused tail of the
    /// current region accounts for: the holes the free-space ratio governs.
    pub(crate) const fn holes(&self, cur_tail: u64) -> u64 {
        self.heap
            .saturating_sub(self.live)
            .saturating_sub(self.free)
            .saturating_sub(cur_tail)
    }

    /// The table after the bucket at the split pointer is split: its slots
    /// with routing bit `level` set go to bucket `split + 2^level`.
    pub(crate) const fn after_split(&self) -> Self {
        let mut next = *self;
        next.split += 1;
        if next.split == 1u64 << next.level {
            next.level += 1;
            next.split = 0;
        }
        next
    }

    /// The bucket a key whose hash has these low bits belongs to.
    pub(crate) const fn bucket_of(&self, route: u32) -> u64 {
        let route = route as u64;
        let low = route & ((1u64 << self.level) - 1);
        if low < self.split {
            route & ((1u64 << (self.level + 1)) - 1)
        } else {
            low
        }
    }
}

/// A heap region's header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Region {
    pub len: u64,
    pub live: u64,
    pub next_all: u64,
    pub next_free: u64,
    /// The generation that freed the region: its chunk boundaries may change
    /// only once that generation is in place (see [`heap`]).
    pub freed: u64,
}

impl Region {
    pub(crate) fn encode(&self) -> [u8; crate::width::small(REGION_HEADER)] {
        let mut out = [0u8; crate::width::small(REGION_HEADER)];
        out[0..4].copy_from_slice(REGION_MAGIC);
        out[8..16].copy_from_slice(&self.len.to_le_bytes());
        out[16..24].copy_from_slice(&self.live.to_le_bytes());
        out[24..32].copy_from_slice(&self.next_all.to_le_bytes());
        out[32..40].copy_from_slice(&self.next_free.to_le_bytes());
        out[40..48].copy_from_slice(&self.freed.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8; crate::width::small(REGION_HEADER)]) -> Result<Self> {
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight"));
        let region = Self {
            len: word(8),
            live: word(16),
            next_all: word(24),
            next_free: word(32),
            freed: word(40),
        };
        if &bytes[0..4] != REGION_MAGIC || region.live > region.len {
            return Err(corrupt("keyed heap region"));
        }
        Ok(region)
    }
}

/// A chunk's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chunk {
    pub payload_len: u32,
    pub block_id: u32,
    pub key_len: u32,
    pub crc: u32,
}

impl Chunk {
    pub(crate) fn encode(&self) -> [u8; crate::width::small(CHUNK_HEADER)] {
        let mut out = [0u8; crate::width::small(CHUNK_HEADER)];
        out[0..4].copy_from_slice(&self.payload_len.to_le_bytes());
        out[4..8].copy_from_slice(&self.block_id.to_le_bytes());
        out[8..12].copy_from_slice(&self.key_len.to_le_bytes());
        out[12..16].copy_from_slice(&self.crc.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8; crate::width::small(CHUNK_HEADER)]) -> Self {
        let half = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four"));
        Self {
            payload_len: half(0),
            block_id: half(4),
            key_len: half(8),
            crc: half(12),
        }
    }

    /// A free chunk of `len` bytes, header included.
    pub(crate) fn free(len: u64) -> Result<Self> {
        Ok(Self {
            payload_len: u32::try_from(len - CHUNK_HEADER)
                .map_err(|_| corrupt("keyed heap chunk length"))?,
            block_id: FREE_BLOCK,
            key_len: 0,
            crc: 0,
        })
    }

    /// The chunk's length, header and padding included.
    pub(crate) const fn len(&self) -> u64 {
        if self.block_id == FREE_BLOCK {
            CHUNK_HEADER + self.payload_len as u64
        } else {
            heap_chunk_len(self.key_len as u64, self.payload_len as u64)
        }
    }
}

/// Chunks are whole multiples of this, so what is left when a chunk takes
/// part of a hole can always carry a free chunk's header.
pub(crate) const CHUNK_ALIGN: u64 = CHUNK_HEADER;

/// The length of a chunk holding a key and a payload of these lengths.
pub(crate) const fn heap_chunk_len(key_len: u64, payload_len: u64) -> u64 {
    (CHUNK_HEADER + key_len + payload_len).next_multiple_of(CHUNK_ALIGN)
}

/// A slot in a bucket page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Slot {
    /// The low 32 bits of the key's hash: they route the key to its bucket
    /// at every level, and serve as its fingerprint.
    pub route: u32,
    /// Where the chunk is: its offset inside its region, and the region.
    pub offset: u32,
    pub region: u64,
}

impl Slot {
    pub(crate) fn encode(&self) -> [u8; crate::width::small(SLOT_LEN)] {
        let mut out = [0u8; crate::width::small(SLOT_LEN)];
        out[0..4].copy_from_slice(&self.route.to_le_bytes());
        out[4..8].copy_from_slice(&self.offset.to_le_bytes());
        out[8..16].copy_from_slice(&self.region.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Self {
        Self {
            route: u32::from_le_bytes(bytes[0..4].try_into().expect("four")),
            offset: u32::from_le_bytes(bytes[4..8].try_into().expect("four")),
            region: u64::from_le_bytes(bytes[8..16].try_into().expect("eight")),
        }
    }

    /// The chunk's logical offset.
    pub(crate) const fn at(&self) -> u64 {
        self.region + self.offset as u64
    }
}

/// A bucket page's header: its slot count and the next page of its chain.
pub(crate) fn page_header(bytes: &[u8]) -> Result<(usize, u64)> {
    let count = u32::from_le_bytes(bytes[4..8].try_into().expect("four")) as usize;
    let next = u64::from_le_bytes(bytes[8..16].try_into().expect("eight"));
    if &bytes[0..4] != BUCKET_MAGIC || count > SLOTS_PER_PAGE {
        return Err(corrupt("keyed bucket page"));
    }
    Ok((count, next))
}

pub(crate) fn set_page_header(bytes: &mut [u8], count: usize, next: u64) {
    bytes[0..4].copy_from_slice(BUCKET_MAGIC);
    bytes[4..8].copy_from_slice(&crate::width::small32(count).to_le_bytes());
    bytes[8..16].copy_from_slice(&next.to_le_bytes());
}

pub(crate) const fn corrupt(what: &'static str) -> Error {
    Error::InvalidCanonicalEncoding(what)
}

// A bucket page is staged as 16-byte units: its header and each slot.
const _: () = assert!(BUCKET_HEADER == SLOT_LEN);
// Region payloads start chunk-aligned.
const _: () = assert!(REGION_HEADER.is_multiple_of(CHUNK_ALIGN));
