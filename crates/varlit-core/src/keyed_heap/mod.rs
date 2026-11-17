//! Keyed records stored once per key, in a B-tree of 4 KiB pages kept
//! inside the file (see `docs/design/bounded-keyed-storage.md`).
//!
//! Every page lives in keyed storage blocks, internal records of the
//! primary file, and changes through the write-ahead ring of a
//! [`RingStore`](crate::matrix_tiles::ring_store::RingStore), as matrix
//! tiles do, so a publication is atomic and a reader keeps its snapshot
//! until the ring moves past it.
//!
//! - **Metadata** (one record): the ring's place, the free-space ratio, the
//!   change budget and the mutable state: the tree's root, counters, the
//!   free page list and the block pages are being carved from.
//! - **Blocks**: records of whole 4 KiB pages, appended when the free page
//!   list has nothing to give; each new block is twice the last, up to
//!   16 MiB.
//! - **Tree pages** ([`node`]): leaves hold records (key then value) in key
//!   order; interior pages hold a child and a separator per entry. A record
//!   or separator too long for its page continues in a chain of overflow
//!   pages.
//! - **Free pages**: a list, oldest first, each page's first 16 bytes naming
//!   the next and the generation that freed it. A page is taken again once
//!   no reader or recovery can show a generation that used it.

pub(crate) mod heap;
#[cfg(kani)]
#[path = "../verification/keyed_heap.rs"]
mod heap_proofs;
pub(crate) mod lookup;
pub(crate) mod node;
pub(crate) mod nodes;
pub(crate) mod reader;
#[cfg(test)]
mod tests;
pub(crate) mod tree;

use crate::{Error, Result};

pub(crate) const META_MAGIC: &[u8; 4] = b"VKMT";
pub(crate) const META_VERSION: u16 = 6;
/// The fixed, checksummed part of the metadata record.
pub(crate) const META_FIXED_LEN: u64 = 64;
/// Where the ring keeps its overflow list head: the first mutable field.
pub(crate) const META_OVERFLOW_HEAD_AT: u64 = META_FIXED_LEN;
/// The mutable state, changed through the ring.
pub(crate) const META_STATE_AT: u64 = META_OVERFLOW_HEAD_AT + 8;
pub(crate) const META_STATE_LEN: u64 = State::FIELDS as u64 * 8;
/// The first branch: one root per keyed block type, changed through the
/// ring.
pub(crate) const META_ROOTS_AT: u64 = META_STATE_AT + META_STATE_LEN;
pub(crate) const META_ROOTS_LEN: u64 = MAX_TYPES as u64 * ROOT_BYTES as u64;
pub(crate) const META_LEN: u64 = META_ROOTS_AT + META_ROOTS_LEN;
/// Keyed block types one file holds, and the bytes of each one's root.
pub(crate) const MAX_TYPES: usize = 64;
const ROOT_BYTES: usize = 16;

/// Every page of keyed storage: tree, overflow and free pages.
pub(crate) const PAGE_BYTES: u64 = node::NODE_BYTES as u64;
/// The first block, the largest a writer appends (1 MiB: the most a file
/// carries in pages not used yet), and the largest a file may hold, in
/// pages.
pub(crate) const FIRST_BLOCK_PAGES: u64 = 16;
pub(crate) const NEW_BLOCK_PAGES: u64 = 256;
pub(crate) const MAX_BLOCK_PAGES: u64 = 4096;
/// The change budget's bounds and default.
pub(crate) const MIN_BUDGET: u64 = 1 << 20;
pub(crate) const MAX_BUDGET: u64 = 1 << 40;
pub(crate) const DEFAULT_BUDGET: u64 = 64 << 20;

/// No page.
pub(crate) const NO_PAGE: u64 = 0;

/// A block id no record may use.
pub(crate) const RESERVED_BLOCK: u32 = u32::MAX;

// ---- metadata ----

/// The fixed part of the metadata record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fixed {
    /// Free-space ratio in thousandths of live bytes.
    pub ratio_permille: u16,
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
            || word(8) != 0
            || word(16) != 0
            || !(MIN_BUDGET..=MAX_BUDGET).contains(&budget)
            || !(crate::matrix_tiles::wal::MIN_RING..=crate::matrix_tiles::wal::MAX_RING_BYTES)
                .contains(&ring.1)
        {
            return Err(corrupt("keyed storage metadata"));
        }
        Ok(Self {
            ratio_permille,
            ring,
            budget,
        })
    }
}

/// The mutable state in the metadata record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct State {
    /// Live keys, and the bytes of their keys and values.
    pub keys: u64,
    pub record_bytes: u64,
    /// Pages carved from blocks so far: every page the storage has.
    pub pages: u64,
    /// The free page list, oldest first, and its length.
    pub free_head: u64,
    pub free_tail: u64,
    pub free: u64,
    /// The block pages are being carved from: its first page, its pages and
    /// how many are carved.
    pub block_at: u64,
    pub block_pages: u64,
    pub block_used: u64,
    /// Leaf, interior and overflow pages.
    pub leaves: u64,
    pub interiors: u64,
    pub overflow: u64,
}

impl State {
    pub(crate) const FIELDS: usize = 16;

    const fn fields(&self) -> [u64; Self::FIELDS] {
        [
            0,
            0,
            self.keys,
            self.record_bytes,
            self.pages,
            self.free_head,
            self.free_tail,
            self.free,
            self.block_at,
            self.block_pages,
            self.block_used,
            self.leaves,
            self.interiors,
            self.overflow,
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
            keys: word(2),
            record_bytes: word(3),
            pages: word(4),
            free_head: word(5),
            free_tail: word(6),
            free: word(7),
            block_at: word(8),
            block_pages: word(9),
            block_used: word(10),
            leaves: word(11),
            interiors: word(12),
            overflow: word(13),
        };
        if word(0) != 0
            || word(1) != 0
            || state.block_used > state.block_pages
            || state.block_pages > MAX_BLOCK_PAGES
            || state.leaves + state.interiors + state.overflow + state.free != state.pages
            || (state.free == 0) != (state.free_head == NO_PAGE)
            || word(14) != 0
            || word(15) != 0
        {
            return Err(corrupt("keyed storage state"));
        }
        Ok(state)
    }
}

/// One keyed block type's tree: its block id, root page and height
/// (1: the root is a leaf). An unused slot is all zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TypeRoot {
    pub block_id: u32,
    pub height: u32,
    pub page: u64,
}

impl TypeRoot {
    pub(crate) const fn root(&self) -> (u64, u64) {
        (self.page, self.height as u64)
    }
}

/// The first branch of keyed storage: the slots of the block types, in the
/// order each first stored a key. A key is stored without its block id;
/// the slot says which type's tree holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Roots {
    pub slots: [TypeRoot; MAX_TYPES],
}

impl Default for Roots {
    fn default() -> Self {
        Self {
            slots: [TypeRoot::default(); MAX_TYPES],
        }
    }
}

impl Roots {
    /// The slot of `block_id`, when it has stored a key.
    pub(crate) fn find(&self, block_id: u32) -> Option<usize> {
        self.slots
            .iter()
            .take_while(|slot| slot.height != 0)
            .position(|slot| slot.block_id == block_id)
    }

    /// Slots in use: they come first.
    pub(crate) fn used(&self) -> usize {
        self.slots
            .iter()
            .take_while(|slot| slot.height != 0)
            .count()
    }

    pub(crate) fn encode(&self) -> [u8; crate::width::small(META_ROOTS_LEN)] {
        let mut out = [0u8; crate::width::small(META_ROOTS_LEN)];
        for (slot, bytes) in self.slots.iter().zip(out.as_chunks_mut::<ROOT_BYTES>().0) {
            bytes[0..4].copy_from_slice(&slot.block_id.to_le_bytes());
            bytes[4..8].copy_from_slice(&slot.height.to_le_bytes());
            bytes[8..16].copy_from_slice(&slot.page.to_le_bytes());
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8; crate::width::small(META_ROOTS_LEN)]) -> Result<Self> {
        let mut roots = Self::default();
        for (slot, raw) in roots
            .slots
            .iter_mut()
            .zip(bytes.as_chunks::<ROOT_BYTES>().0)
        {
            *slot = TypeRoot {
                block_id: u32::from_le_bytes(raw[0..4].try_into().expect("four")),
                height: u32::from_le_bytes(raw[4..8].try_into().expect("four")),
                page: u64::from_le_bytes(raw[8..16].try_into().expect("eight")),
            };
        }
        let used = roots.used();
        for (index, slot) in roots.slots.iter().enumerate() {
            let valid = if index < used {
                slot.page != NO_PAGE
                    && slot.height as usize <= tree::MAX_HEIGHT
                    && slot.block_id != RESERVED_BLOCK
                    && roots.slots[..index]
                        .iter()
                        .all(|other| other.block_id != slot.block_id)
            } else {
                *slot == TypeRoot::default()
            };
            if !valid {
                return Err(corrupt("keyed storage roots"));
            }
        }
        Ok(roots)
    }
}

pub(crate) const fn corrupt(what: &'static str) -> Error {
    Error::InvalidCanonicalEncoding(what)
}
