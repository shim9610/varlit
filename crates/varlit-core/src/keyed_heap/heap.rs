//! The single writer of keyed storage: a B-tree of 4 KiB pages.
//!
//! Memory is fixed when the writer is opened: the ring's buffers, 64 page
//! buffers and a scratch page. Records are read and written through stack
//! buffers; puts, deletes, splits and merges allocate nothing.
//!
//! **Records live in the leaves.** A leaf cell holds a record (key then
//! value) whole when it is short, else its first bytes and an overflow
//! chain for the rest, sized so the overflow pages are full. A put finds
//! its key, writes the new cell over the old one (or inserts it), and
//! splits a leaf that no longer holds its cells: the upper half moves to a
//! new page, and the shortest prefix between the two halves goes to the
//! parent. A delete removes the cell; a leaf left under a quarter full
//! merges with a neighbour when they fit one page, and an emptied page
//! leaves the tree.
//!
//! **Pages change through the ring.** A changed page is staged whole,
//! through the page buffers, so a publication is atomic and readers keep
//! their snapshots; nothing is written in place but pages of a block
//! appended in the open generation.
//!
//! **Pages come from the free list, then from blocks.** A freed page is
//! linked to the end of the free list with the generation that freed it,
//! and taken again once the ring's mark reaches that generation: no reader
//! and no recovery can show a generation that used it. When the free list
//! has none to give, pages are carved from the last block; when it is
//! used up, a new block (twice the last, up to 1 MiB) is appended. Pages
//! freed by deleting most keys stay in the file, for new keys to take; a
//! file that shrank for good is rebuilt.

use super::lookup::{Ctx, View};
use super::node;
use super::nodes::{HeapView, Nodes};
use super::tree::{self, Body, KeyLoc, KeySrc, Path};
use super::{
    FIRST_BLOCK_PAGES, Fixed, MAX_BUDGET, MAX_TYPES, META_FIXED_LEN, META_LEN,
    META_OVERFLOW_HEAD_AT, META_ROOTS_AT, META_STATE_AT, MIN_BUDGET, NEW_BLOCK_PAGES, NO_PAGE,
    PAGE_BYTES, RESERVED_BLOCK, Roots, State, TypeRoot, corrupt,
};
use crate::matrix_tiles::RecordKind;
use crate::matrix_tiles::ring_store::RingStore;
use crate::matrix_tiles::store::{TileSource, TileStore};
use crate::{Error, Result};

const PAGE: usize = node::NODE_BYTES;
/// The most a leaf cell takes: flags, two lengths, the local bytes and an
/// overflow page.
const CELL_MAX: usize = 1 + 10 + 10 + crate::width::small(tree::MAX_LOCAL) + 8;
/// The most a separator takes (without its child).
const SEP_MAX: usize = 1 + 10 + crate::width::small(tree::SEP_LOCAL) + 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeapStats {
    pub splits: u64,
    pub merges: u64,
    /// Pages carved from blocks, taken from the free list, and blocks
    /// appended.
    pub pages_carved: u64,
    pub pages_reused: u64,
    pub blocks: u64,
    /// Times the writer raised the mark so freed pages could be taken.
    pub marks: u64,
    /// Rounds the writer published by itself: the change budget was spent.
    pub rounds: u64,
    /// Checkpoints of every publication, run when they filled half the ring.
    pub checkpoints: u64,
}

/// Knobs of the writer, chosen at open. Tests shrink the bytes a page may
/// use to grow deep trees from few keys.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tuning {
    pub node_bytes: usize,
}

impl Tuning {
    pub(crate) const DEFAULT: Self = Self {
        node_bytes: node::CAPACITY,
    };
}

/// What a page holds, as the state counts pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    Leaf,
    Interior,
    Overflow,
}

pub(crate) struct KeyedHeap {
    pub(crate) meta: u64,
    fixed: Fixed,
    pub(crate) ring: RingStore,
    state: State,
    /// The state as the last publication staged it.
    staged: State,
    /// The first branch: a root per block type, and as last staged.
    roots: Roots,
    staged_roots: Roots,
    /// The slot of the type the open put or delete changes.
    slot: usize,
    nodes: Nodes,
    tuning: Tuning,
    /// Bytes the open round wrote, against the change budget: what it wrote
    /// back, not what each change might touch.
    round_bytes: u64,
    /// Pages freed by the open generation, and by published generations the
    /// mark has not reached: not to be taken yet.
    freed_open: u64,
    freed_waiting: u64,
    pub(crate) stats: HeapStats,
}

impl KeyedHeap {
    /// Lays out empty keyed storage: the metadata record and a ring of
    /// `ring_len` bytes, published. A block type gets its tree when it
    /// stores its first key.
    pub(crate) fn create(
        store: &mut dyn TileStore,
        ring_len: u64,
        ratio_permille: u16,
        budget: u64,
        tuning: Tuning,
    ) -> Result<Self> {
        if !(10..=50).contains(&ratio_permille) {
            return Err(Error::InvalidFormatSpec(
                "keyed storage free-space ratio must be 1 to 5 %",
            ));
        }
        if !(MIN_BUDGET..=MAX_BUDGET).contains(&budget) {
            return Err(Error::InvalidFormatSpec(
                "keyed storage change budget must be 1 MiB to 1 TiB",
            ));
        }
        let mut fixed = Fixed {
            ratio_permille,
            ring: (0, 0),
            budget,
        };
        let meta = store.append_zeroed(RecordKind::KeyedMeta, &fixed.encode(), META_LEN)?;
        let ring = RingStore::create(store, ring_len, meta + META_OVERFLOW_HEAD_AT)?;
        fixed.ring = (ring.ring.base, ring.ring.len);
        store.write_at(meta, &fixed.encode())?;
        let mut heap = Self::with_parts(meta, fixed, ring, State::default(), tuning)?;
        heap.staged_roots = Roots::default();
        heap.ring
            .stage(store, meta + META_STATE_AT, &heap.state.encode())?;
        heap.publish(store)?;
        Ok(heap)
    }

    /// Opens keyed storage whose metadata payload is at `meta`.
    pub(crate) fn open(store: &mut dyn TileStore, meta: u64, tuning: Tuning) -> Result<Self> {
        let mut bytes = [0u8; crate::width::small(META_FIXED_LEN)];
        store.read_at(meta, &mut bytes)?;
        let fixed = Fixed::decode(&bytes)?;
        let ring = RingStore::open(store, fixed.ring, meta + META_OVERFLOW_HEAD_AT)?;
        let mut raw = [0u8; State::FIELDS * 8];
        ring.read(store, meta + META_STATE_AT, &mut raw)?;
        let state = State::decode(&raw)?;
        let mut raw = [0u8; crate::width::small(super::META_ROOTS_LEN)];
        ring.read(store, meta + META_ROOTS_AT, &mut raw)?;
        let roots = Roots::decode(&raw)?;
        let mut heap = Self::with_parts(meta, fixed, ring, state, tuning)?;
        heap.staged = state;
        heap.roots = roots;
        heap.staged_roots = roots;
        // What generations before the last publication freed may be taken
        // once no reader keeps one.
        heap.raise(store)?;
        Ok(heap)
    }

    fn with_parts(
        meta: u64,
        fixed: Fixed,
        ring: RingStore,
        state: State,
        tuning: Tuning,
    ) -> Result<Self> {
        Ok(Self {
            meta,
            fixed,
            ring,
            state,
            staged: State::default(),
            roots: Roots::default(),
            staged_roots: Roots::default(),
            slot: 0,
            nodes: Nodes::new()?,
            tuning,
            round_bytes: 0,
            freed_open: 0,
            freed_waiting: 0,
            stats: HeapStats::default(),
        })
    }

    pub(crate) const fn state(&self) -> &State {
        &self.state
    }

    /// Whether anything changed since the last publication.
    pub(crate) fn is_dirty(&self) -> bool {
        self.ring.is_dirty()
            || self.nodes.is_dirty()
            || self.state != self.staged
            || self.roots != self.staged_roots
    }

    /// The root of the type the open change is on.
    fn root(&self) -> (u64, u64) {
        self.roots.slots[self.slot].root()
    }

    fn set_root(&mut self, (page, height): (u64, u64)) -> Result<()> {
        let slot = &mut self.roots.slots[self.slot];
        slot.page = page;
        slot.height = u32::try_from(height).map_err(|_| corrupt("keyed tree height"))?;
        Ok(())
    }

    /// Makes `block_id`'s tree the one changes go to; a type storing its
    /// first key gets the next slot and an empty leaf as its root.
    fn enter(&mut self, store: &mut dyn TileStore, block_id: u32, create: bool) -> Result<bool> {
        if let Some(slot) = self.roots.find(block_id) {
            self.slot = slot;
            return Ok(true);
        }
        if !create {
            return Ok(false);
        }
        let slot = self.roots.used();
        if slot == MAX_TYPES {
            return Err(Error::LimitExceeded {
                resource: "keyed block types",
                actual: MAX_TYPES as u64 + 1,
                limit: MAX_TYPES as u64,
            });
        }
        let (page, _) = self.new_node(store, node::Kind::Leaf, &[])?;
        self.roots.slots[slot] = TypeRoot {
            block_id,
            height: 1,
            page,
        };
        self.slot = slot;
        Ok(true)
    }

    /// The height of `block_id`'s tree, when it has one.
    #[cfg(test)]
    pub(crate) fn height(&self, block_id: u32) -> Option<u64> {
        self.roots
            .find(block_id)
            .map(|slot| self.roots.slots[slot].root().1)
    }

    pub(crate) fn publish(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        self.nodes.flush(&mut self.ring, store)?;
        if self.state != self.staged {
            self.ring
                .stage(store, self.meta + META_STATE_AT, &self.state.encode())?;
            self.staged = self.state;
        }
        if self.roots != self.staged_roots {
            self.ring
                .stage(store, self.meta + META_ROOTS_AT, &self.roots.encode())?;
            self.staged_roots = self.roots;
        }
        let generation = self.ring.publish(store)?;
        self.round_bytes = 0;
        self.nodes.take_written();
        self.freed_waiting += core::mem::take(&mut self.freed_open);
        // Once publications fill half the ring, put all of them in place in
        // one pass, before a frame finds no room and checkpoints a few of
        // them: one read of the ring and two barriers per half ring, and a
        // page changed by several publications copied once.
        if self.ring.published_ring_bytes() * 2 >= self.ring.ring_area() {
            self.ring.checkpoint(store, u64::MAX)?;
            self.stats.checkpoints += 1;
            self.raise(store)?;
        }
        Ok(generation)
    }

    /// Raises the ring's mark to the last publication: pages it freed may
    /// be taken.
    fn raise(&mut self, store: &mut dyn TileStore) -> Result<()> {
        if self.ring.mark() < self.ring.published() {
            self.ring.raise_mark(store)?;
            self.stats.marks += 1;
        }
        self.freed_waiting = 0;
        Ok(())
    }

    /// Bytes of freed pages that may wait for the mark: the free-space
    /// ratio of the record bytes, at least 1 MiB, at most half the change
    /// budget.
    fn patience(&self) -> u64 {
        (self.state.record_bytes / 1000 * u64::from(self.fixed.ratio_permille))
            .max(1 << 20)
            .min(self.fixed.budget / 2)
    }

    /// Publishes the open round and raises the mark to it.
    fn end_round(&mut self, store: &mut dyn TileStore) -> Result<()> {
        if self.is_dirty() {
            self.publish(store)?;
            self.stats.rounds += 1;
        }
        self.raise(store)
    }

    /// Ring room a round keeps for what it has not staged yet: the page
    /// buffers, written back at publication. A round that would need more
    /// ends first, so publications stay in the ring instead of streaming
    /// into indexes appended to the file (which would grow it).
    fn ring_reserve(&self) -> u64 {
        let buffers = (super::nodes::SLOTS as u64 + 2) * (PAGE_BYTES + 64);
        buffers.min(self.ring.ring_area() / 4)
    }

    /// Ends the round when it has written the budget's worth (tree pages
    /// written back, overflow pages, free-list links) or filled the ring but
    /// the reserve, and raises the mark when freed pages have waited past
    /// the patience.
    fn after_change(&mut self, store: &mut dyn TileStore) -> Result<()> {
        self.round_bytes += self.nodes.take_written();
        if self.round_bytes >= self.fixed.budget || self.ring.ring_room() < self.ring_reserve() {
            return self.end_round(store);
        }
        if self.freed_waiting * PAGE_BYTES > self.patience() {
            self.raise(store)?;
        }
        Ok(())
    }

    // ---- lookups ----

    fn view(&self) -> HeapView<'_> {
        HeapView {
            ring: &self.ring,
            nodes: &self.nodes,
        }
    }

    fn read(&self, store: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        self.nodes.read(&self.ring, store, at, buf)
    }

    fn ctx<'a>(&'a self, view: &'a mut dyn View, store: &'a dyn TileSource) -> Ctx<'a> {
        Ctx::new(view, store)
    }

    /// Finds `key` of `block_id` as this writer sees it.
    pub(crate) fn find(
        &self,
        store: &dyn TileSource,
        block_id: u32,
        key: &[u8],
    ) -> Result<Option<Found>> {
        let Some(slot) = self.roots.find(block_id) else {
            return Ok(None);
        };
        let mut page = [0u8; PAGE];
        let mut view = self.view();
        let mut ctx = self.ctx(&mut view, store);
        let key = KeySrc::Parts(key);
        let (_, body) = tree::find(&mut ctx, self.roots.slots[slot].root(), &key, &mut page)?;
        Ok(body.map(|body| Found { body }))
    }

    /// Positions a scan of `block_id`'s records at the first key at or past
    /// `from` (past it when not inclusive), or at the first key; `None`
    /// when the type has no records.
    pub(crate) fn seek(
        &self,
        store: &dyn TileSource,
        block_id: u32,
        from: Option<(&[u8], bool)>,
    ) -> Result<Option<tree::Cursor>> {
        let Some(slot) = self.roots.find(block_id) else {
            return Ok(None);
        };
        let mut page = [0u8; PAGE];
        let mut view = self.view();
        let mut ctx = self.ctx(&mut view, store);
        tree::seek(&mut ctx, self.roots.slots[slot].root(), from, &mut page).map(Some)
    }

    /// Visits records in key order from `cursor` (see [`tree::scan`]).
    pub(crate) fn scan(
        &self,
        store: &dyn TileSource,
        cursor: tree::Cursor,
        visit: &mut tree::Visit<'_>,
    ) -> Result<()> {
        let mut page = [0u8; PAGE];
        let mut view = self.view();
        let mut ctx = self.ctx(&mut view, store);
        tree::scan(&mut ctx, cursor, &mut page, visit)
    }

    /// Reads a found record's payload into `out`, which must be its length.
    pub(crate) fn read_payload(
        &self,
        store: &dyn TileSource,
        found: &Found,
        out: &mut [u8],
    ) -> Result<()> {
        if out.len() as u64 != found.payload_len() {
            return Err(Error::BufferTooSmall {
                resource: "keyed record payload",
                required: crate::width::index(found.payload_len())?,
                provided: out.len(),
            });
        }
        let mut view = self.view();
        let mut ctx = self.ctx(&mut view, store);
        tree::read_value(&mut ctx, &found.body, out)
    }

    /// Finds `key` through the page buffers. Returns the path and the leaf
    /// cell when found.
    fn find_path(
        &mut self,
        store: &mut dyn TileStore,
        key: &KeySrc<'_>,
    ) -> Result<(Path, Option<Body>)> {
        let (mut at, height) = self.root();
        let height = crate::width::index(height)?;
        let mut path = Path::new(height);
        for level in (0..height).rev() {
            let slot = self.nodes.load(&mut self.ring, store, at, &[])?;
            let mut view = self.view();
            let mut ctx = self.ctx(&mut view, &*store);
            let found = tree::step(&mut ctx, self.nodes.page(slot), at, key, level, &mut path)?;
            if level == 0 {
                return Ok((path, found));
            }
            at = path.pages[level - 1];
        }
        Ok((path, None))
    }

    // ---- puts and deletes ----

    /// Stores `payload` as the record of `key` of `block_id`, replacing the
    /// one there is.
    pub(crate) fn put(
        &mut self,
        store: &mut dyn TileStore,
        block_id: u32,
        key: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        if block_id == RESERVED_BLOCK {
            return Err(corrupt("reserved keyed block id"));
        }
        self.enter(store, block_id, true)?;
        let key_len = key.len() as u64;
        let value_len = payload.len() as u64;
        let total = key_len + value_len;
        let (path, old) = self.find_path(store, &KeySrc::Parts(key))?;
        if let Some(old) = old {
            if old.overflowed() {
                self.free_chain(store, old.key.overflow)?;
            }
            self.state.record_bytes -= old.key.len + old.value_len;
        } else {
            self.state.keys += 1;
        }
        self.state.record_bytes += total;
        let record = RecSrc::Record(key, payload);
        let local = tree::local_len(total);
        let mut cell = [0u8; CELL_MAX];
        let mut at = 1;
        at += node::write_varint(&mut cell[at..], key_len);
        at += node::write_varint(&mut cell[at..], value_len);
        let local_bytes = crate::width::index(local)?;
        self.rec_read(&*store, &record, 0, &mut cell[at..at + local_bytes])?;
        at += local_bytes;
        if local < total {
            cell[0] = tree::FLAG_OVERFLOW;
            let head = self.write_chain(store, &record, local, total)?;
            cell[at..at + 8].copy_from_slice(&head.to_le_bytes());
            at += 8;
        }
        self.leaf_set(store, &path, &cell[..at], old.is_some())?;
        self.after_change(store)
    }

    /// Removes `key` of `block_id`; `false` when there is none.
    pub(crate) fn delete(
        &mut self,
        store: &mut dyn TileStore,
        block_id: u32,
        key: &[u8],
    ) -> Result<bool> {
        if !self.enter(store, block_id, false)? {
            return Ok(false);
        }
        let (path, old) = self.find_path(store, &KeySrc::Parts(key))?;
        let Some(old) = old else {
            return Ok(false);
        };
        if old.overflowed() {
            self.free_chain(store, old.key.overflow)?;
        }
        self.state.record_bytes -= old.key.len + old.value_len;
        self.state.keys -= 1;
        let slot = self.nodes.load(&mut self.ring, store, path.pages[0], &[])?;
        node::remove(self.nodes.page_mut(slot), path.index[0])?;
        if path.height > 1 {
            if node::count(self.nodes.page(slot)) == 0 {
                self.remove_child(store, &path, 0)?;
            } else if used(self.nodes.page(slot)) < self.tuning.node_bytes / 4 {
                self.merge_leaf(store, &path)?;
            }
        }
        self.after_change(store)?;
        Ok(true)
    }
}

#[cfg(test)]
mod consistency;
mod pages;
mod rebalance;

/// Where a key's record is: its leaf cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub body: Body,
}

impl Found {
    pub(crate) const fn payload_len(&self) -> u64 {
        self.body.value_len
    }
}

/// Where record bytes come from.
enum RecSrc<'a> {
    /// A record: the key, then the value.
    Record(&'a [u8], &'a [u8]),
    /// A key, as a separator's source.
    Key(KeySrc<'a>),
}

/// Bytes a tree page's entries and cells use.
fn used(page: &[u8]) -> usize {
    node::CAPACITY - node::free(page)
}
