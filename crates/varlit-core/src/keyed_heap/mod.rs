//! Keyed records stored once per key: size-class item arrays found through
//! slot buckets (see `docs/design/bounded-keyed-storage.md`).
//!
//! Every structure lives in internal records of the primary file and
//! changes through the write-ahead ring of a
//! [`RingStore`](crate::matrix_tiles::ring_store::RingStore), as matrix
//! tiles do, so a publication is atomic and a reader keeps its snapshot
//! until the ring moves past it.
//!
//! - **Metadata** (one record): the ring's place, the free-space ratio, the
//!   change budget, the per-file hash seed, the mutable state (bucket
//!   directory, linear hashing, counters, the page pool and the hole log)
//!   and one entry per size class.
//! - **Size classes**: about a hundred item sizes, 1/16 apart, from 48 bytes
//!   to 64 KiB. A class is a dense array of fixed-size items over 64 KiB
//!   pages, mapped through a radix directory; free space is only at its end.
//! - **Records**: a chain of items, full 64 KiB items then one of the class
//!   the rest fits. A record's bytes are its key then its payload.
//! - **Slot buckets**: 4 KiB pages of 16-byte slots (hash low bits, head
//!   item), found through a radix directory by bucket number; buckets grow
//!   one at a time by linear hashing.
//!
//! A slot is the only thing that makes a record live. An item freed is
//! marked dead in its header and logged. Once every reader and recovery
//! shows a generation that no longer uses it (the ring's mark has reached
//! the generation that freed it), the class's last live item is written into
//! it once and its referrer repointed, and the class shrinks by one: the
//! items below a class's end are live, or dead and logged, and nothing else.

pub(crate) mod heap;
#[cfg(kani)]
#[path = "../verification/keyed_heap.rs"]
mod heap_proofs;
pub(crate) mod lookup;
pub(crate) mod node;
pub(crate) mod pages;
pub(crate) mod radix;
pub(crate) mod reader;
pub(crate) mod sip;
pub(crate) mod tree;
#[cfg(test)]
mod tests;

use crate::{Error, Result};

pub(crate) const META_MAGIC: &[u8; 4] = b"VKMT";
pub(crate) const META_VERSION: u16 = 3;
/// The fixed, checksummed part of the metadata record.
pub(crate) const META_FIXED_LEN: u64 = 64;
/// Where the ring keeps its overflow list head: the first mutable field.
pub(crate) const META_OVERFLOW_HEAD_AT: u64 = META_FIXED_LEN;
/// The mutable state, changed through the ring.
pub(crate) const META_STATE_AT: u64 = META_OVERFLOW_HEAD_AT + 8;
pub(crate) const META_STATE_LEN: u64 = State::FIELDS as u64 * 8;
/// One [`ClassEntry`] per possible class, changed through the ring.
pub(crate) const META_CLASSES_AT: u64 = META_STATE_AT + META_STATE_LEN;
pub(crate) const CLASS_ENTRY_LEN: u64 = 32;
pub(crate) const MAX_CLASSES: usize = 128;
pub(crate) const META_LEN: u64 = META_CLASSES_AT + MAX_CLASSES as u64 * CLASS_ENTRY_LEN;

/// Item pages, log pages and pooled pages: one size for all.
pub(crate) const PAGE_BYTES: u64 = 64 << 10;
/// A class page's first bytes: its link while it is pooled, never an
/// item's. Item bytes fill the rest of each page, and an item may continue
/// on the class's next page.
pub(crate) const PAGE_LINK: u64 = 16;
pub(crate) const ITEM_SPACE: u64 = PAGE_BYTES - PAGE_LINK;
/// The largest item; longer records are chains.
pub(crate) const MAX_ITEM: u64 = PAGE_BYTES;
/// Every item starts with a header of this length.
pub(crate) const ITEM_HEADER: u64 = 40;
/// The change budget's bounds: at least one page's worth of items many
/// times over, at most 1 TiB.
pub(crate) const MIN_BUDGET: u64 = 1 << 20;
pub(crate) const MAX_BUDGET: u64 = 1 << 40;
/// The change budget when a format declares none.
pub(crate) const DEFAULT_BUDGET: u64 = 64 << 20;

/// `hash low bits | 0 | head item`.
pub(crate) const BUCKET_MAGIC: &[u8; 4] = b"VBKT";
pub(crate) const BUCKET_PAGE: u64 = 4096;
/// `magic | count | next page`.
pub(crate) const BUCKET_HEADER: u64 = 16;
pub(crate) const SLOT_LEN: u64 = 16;
pub(crate) const SLOTS_PER_PAGE: usize = ((BUCKET_PAGE - BUCKET_HEADER) / SLOT_LEN) as usize;

/// An item reference: class in the top byte, index below.
pub(crate) const NO_ITEM: u64 = u64::MAX;
pub(crate) const MAX_INDEX: u64 = (1 << 56) - 1;

/// A block id no record may use.
pub(crate) const RESERVED_BLOCK: u32 = u32::MAX;

// ---- size classes ----

#[allow(
    clippy::cast_possible_truncation,
    reason = "every class is at most MAX_ITEM (64 KiB)"
)]
const fn build_classes() -> ([u32; MAX_CLASSES], usize) {
    let mut sizes = [0u32; MAX_CLASSES];
    let mut count = 0;
    // 16-byte steps up to 256 bytes, the smallest holding a header and a
    // byte.
    let mut size = 48u64;
    while size < 256 {
        sizes[count] = size as u32;
        count += 1;
        size += 16;
    }
    // Then 1/16 apart, rounded up to 16 bytes, to the largest item.
    loop {
        sizes[count] = size as u32;
        count += 1;
        if size >= MAX_ITEM {
            break;
        }
        let next = (size * 17 / 16).next_multiple_of(16);
        size = if next > MAX_ITEM { MAX_ITEM } else { next };
    }
    (sizes, count)
}

const CLASS_TABLE: ([u32; MAX_CLASSES], usize) = build_classes();
/// The number of size classes.
pub(crate) const CLASSES: usize = CLASS_TABLE.1;
const _: () = assert!(CLASSES <= MAX_CLASSES);

/// The item size of class `class`.
pub(crate) const fn class_size(class: usize) -> u64 {
    CLASS_TABLE.0[class] as u64
}

/// The smallest class whose items hold `len` bytes (header included);
/// `None` when `len` is larger than the largest item.
pub(crate) const fn class_of(len: u64) -> Option<usize> {
    if len > MAX_ITEM {
        return None;
    }
    let (mut low, mut high) = (0usize, CLASSES - 1);
    while low < high {
        let mid = (low + high) / 2;
        if class_size(mid) >= len {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    Some(low)
}

pub(crate) const fn item_ref(class: usize, index: u64) -> u64 {
    ((class as u64) << 56) | index
}

pub(crate) const fn ref_class(item: u64) -> usize {
    (item >> 56) as usize
}

pub(crate) const fn ref_index(item: u64) -> u64 {
    item & MAX_INDEX
}

/// A reference read from the file, checked to name an existing class.
pub(crate) const fn checked_ref(item: u64) -> Result<u64> {
    if ref_class(item) >= CLASSES {
        return Err(corrupt("keyed item reference"));
    }
    Ok(item)
}

// ---- metadata ----

/// The fixed part of the metadata record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fixed {
    /// Free-space ratio in thousandths of live bytes.
    pub ratio_permille: u16,
    pub seed: [u64; 2],
    pub ring: (u64, u64),
    /// Bytes one round may allocate before the writer publishes it.
    pub budget: u64,
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
        out[40..48].copy_from_slice(&self.budget.to_le_bytes());
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
        let budget = word(40);
        if !(10..=50).contains(&ratio_permille)
            || !(MIN_BUDGET..=MAX_BUDGET).contains(&budget)
            || !(crate::matrix_tiles::wal::MIN_RING..=crate::matrix_tiles::wal::MAX_RING_BYTES)
                .contains(&ring.1)
        {
            return Err(corrupt("keyed storage metadata"));
        }
        Ok(Self {
            ratio_permille,
            seed: [word(8), word(16)],
            ring,
            budget,
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
    /// Bytes of live items (whole items: headers and rounding included).
    pub live: u64,
    /// Item, log and pooled pages, all [`PAGE_BYTES`] long.
    pub pages: u64,
    /// The page pool: pages no class or log uses, oldest first.
    pub pool_head: u64,
    pub pool_tail: u64,
    pub pooled: u64,
    /// Free bucket pages.
    pub free_buckets: u64,
    /// The log of items freed, oldest first: its first page and entry, its
    /// last page and entry count there, and its entries. An entry is taken
    /// once the ring's mark reaches the generation that freed it.
    pub log_head: u64,
    pub log_head_at: u64,
    pub log_tail: u64,
    pub log_tail_at: u64,
    pub logged: u64,
    /// Bytes of the items the log names.
    pub hole_bytes: u64,
}

impl State {
    pub(crate) const FIELDS: usize = 19;

    const fn fields(&self) -> [u64; Self::FIELDS] {
        [
            self.dir.0,
            self.dir.1,
            self.level,
            self.split,
            self.keys,
            self.live,
            self.pages,
            self.pool_head,
            self.pool_tail,
            self.pooled,
            self.free_buckets,
            self.log_head,
            self.log_head_at,
            self.log_tail,
            self.log_tail_at,
            self.logged,
            self.hole_bytes,
            0,
            0,
        ]
    }

    pub(crate) fn encode(&self) -> [u8; Self::FIELDS * 8] {
        let mut out = [0u8; Self::FIELDS * 8];
        for (index, field) in self.fields().into_iter().enumerate() {
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
            pages: word(6),
            pool_head: word(7),
            pool_tail: word(8),
            pooled: word(9),
            free_buckets: word(10),
            log_head: word(11),
            log_head_at: word(12),
            log_tail: word(13),
            log_tail_at: word(14),
            logged: word(15),
            hole_bytes: word(16),
        };
        if state.level >= 32
            || state.split >= 1u64 << state.level
            || state.pooled > state.pages
            || state
                .live
                .checked_add(state.hole_bytes)
                .is_none_or(|used| used > state.pages.saturating_mul(PAGE_BYTES))
            || state.log_head_at > LOG_ENTRIES
            || state.log_tail_at > LOG_ENTRIES
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

    /// The table after its last bucket is merged back into the bucket it
    /// was split from: the inverse of [`Self::after_split`]. The table must
    /// have more than one bucket.
    pub(crate) const fn after_merge(&self) -> Self {
        let mut next = *self;
        if next.split == 0 {
            next.level -= 1;
            next.split = (1u64 << next.level) - 1;
        } else {
            next.split -= 1;
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

/// One size class: its items `[0, end)`, each live or dead and logged, and
/// the radix directory of its pages. Free space is only past `end`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClassEntry {
    pub end: u64,
    /// Directory root page and height (`0, 0` for none).
    pub dir: (u64, u64),
}

impl ClassEntry {
    pub(crate) fn encode(&self) -> [u8; crate::width::small(CLASS_ENTRY_LEN)] {
        let mut out = [0u8; crate::width::small(CLASS_ENTRY_LEN)];
        out[0..8].copy_from_slice(&self.end.to_le_bytes());
        out[8..16].copy_from_slice(&self.dir.0.to_le_bytes());
        out[16..24].copy_from_slice(&self.dir.1.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8; crate::width::small(CLASS_ENTRY_LEN)]) -> Result<Self> {
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight"));
        let entry = Self {
            end: word(0),
            dir: (word(8), word(16)),
        };
        if entry.end > MAX_INDEX
            || entry.dir.1 > crate::matrix_tiles::view::DIR_MAX_HEIGHT
            || (entry.dir.0 == 0) != (entry.dir.1 == 0)
            || word(24) != 0
        {
            return Err(corrupt("keyed size class"));
        }
        Ok(entry)
    }
}

/// The metadata offset of class `class`'s entry.
pub(crate) const fn class_entry_at(meta: u64, class: usize) -> u64 {
    meta + META_CLASSES_AT + class as u64 * CLASS_ENTRY_LEN
}

// ---- items ----

/// An item's header. A head item starts a record (`block_id`, `key_len`,
/// the record's `total` length); a continuation item names its record's
/// key `route` and its `prev` item. Both carry the `next` item and `dead`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Item {
    /// Record bytes in this item.
    pub len: u32,
    pub head: bool,
    /// A head: the block id; a continuation: the key's route.
    pub block_or_route: u32,
    /// A head: the key's length; a continuation: zero.
    pub key_len: u32,
    /// CRC32 of the header's first eight bytes and this item's bytes.
    pub crc: u32,
    /// `0` while the item is live; once freed, the generation that freed
    /// it. Outside the checksum: it changes on its own.
    pub dead: u64,
    pub next: u64,
    /// A head: the record's total length; a continuation: the item before.
    pub total_or_prev: u64,
}

/// `len`'s top bit marks a continuation item.
const CONTINUATION: u32 = 1 << 31;

impl Item {
    pub(crate) fn encode(&self) -> [u8; crate::width::small(ITEM_HEADER)] {
        let mut out = [0u8; crate::width::small(ITEM_HEADER)];
        let len = if self.head {
            self.len
        } else {
            self.len | CONTINUATION
        };
        out[0..4].copy_from_slice(&len.to_le_bytes());
        out[4..8].copy_from_slice(&self.block_or_route.to_le_bytes());
        out[8..12].copy_from_slice(&self.key_len.to_le_bytes());
        out[12..16].copy_from_slice(&self.crc.to_le_bytes());
        out[16..24].copy_from_slice(&self.dead.to_le_bytes());
        out[24..32].copy_from_slice(&self.next.to_le_bytes());
        out[32..40].copy_from_slice(&self.total_or_prev.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8; crate::width::small(ITEM_HEADER)]) -> Self {
        let half = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four"));
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight"));
        let len = half(0);
        Self {
            len: len & !CONTINUATION,
            head: len & CONTINUATION == 0,
            block_or_route: half(4),
            key_len: half(8),
            crc: half(12),
            dead: word(16),
            next: word(24),
            total_or_prev: word(32),
        }
    }

    /// The bytes the checksum starts with: the encoded length and the
    /// block id or route.
    pub(crate) fn crc_prefix(&self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out.copy_from_slice(&self.encode()[..8]);
        out
    }
}

/// How a record of `total` bytes (key and payload) is split into items:
/// the number of full items and the class of the last one.
pub(crate) const fn chain_shape(total: u64) -> (u64, usize) {
    let per_full = MAX_ITEM - ITEM_HEADER;
    if total + ITEM_HEADER <= MAX_ITEM {
        // `class_of` is `Some` for any length up to `MAX_ITEM`.
        return (0, class_of_fitting(ITEM_HEADER + total));
    }
    let full = (total - 1) / per_full;
    let rest = total - full * per_full;
    (full, class_of_fitting(ITEM_HEADER + rest))
}

const fn class_of_fitting(len: u64) -> usize {
    match class_of(len) {
        Some(class) => class,
        None => CLASSES - 1,
    }
}

/// The bytes of the items a record of `total` bytes takes.
pub(crate) const fn chain_bytes(total: u64) -> u64 {
    let (full, last) = chain_shape(total);
    full * MAX_ITEM + class_size(last)
}

// ---- the hole log ----

/// A log page: its next page, then entries `item | generation freed`.
pub(crate) const LOG_HEADER: u64 = 16;
pub(crate) const LOG_ENTRY: u64 = 16;
pub(crate) const LOG_ENTRIES: u64 = (PAGE_BYTES - LOG_HEADER) / LOG_ENTRY;

// ---- slots ----

/// A slot in a bucket page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Slot {
    /// The low 32 bits of the key's hash: they route the key to its bucket
    /// at every level, and serve as its fingerprint.
    pub route: u32,
    /// The record's head item.
    pub item: u64,
}

impl Slot {
    pub(crate) fn encode(&self) -> [u8; crate::width::small(SLOT_LEN)] {
        let mut out = [0u8; crate::width::small(SLOT_LEN)];
        out[0..4].copy_from_slice(&self.route.to_le_bytes());
        out[8..16].copy_from_slice(&self.item.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Self {
        Self {
            route: u32::from_le_bytes(bytes[0..4].try_into().expect("four")),
            item: u64::from_le_bytes(bytes[8..16].try_into().expect("eight")),
        }
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

// A bucket page is staged whole; its header is one slot long.
const _: () = assert!(BUCKET_HEADER == SLOT_LEN);
// The smallest class holds a header and at least one byte.
const _: () = assert!(class_size(0) > ITEM_HEADER);
// Every class is a multiple of 16 bytes, the largest exactly one page.
const _: () = assert!(class_size(CLASSES - 1) == MAX_ITEM);
