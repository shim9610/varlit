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
    /// Bytes the open round changed, against the change budget.
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
        self.freed_waiting += core::mem::take(&mut self.freed_open);
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

    /// Ends the round when it has changed the budget's worth or filled the
    /// ring but the reserve, and raises the mark when freed pages have
    /// waited past the patience.
    fn after_change(&mut self, store: &mut dyn TileStore) -> Result<()> {
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
        self.round_bytes += total + PAGE_BYTES;
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
        self.round_bytes += PAGE_BYTES;
        self.after_change(store)?;
        Ok(true)
    }

    // ---- record bytes ----

    /// Reads bytes `[at, at + buf.len())` of a record or a separator.
    fn rec_read(
        &self,
        store: &dyn TileSource,
        src: &RecSrc<'_>,
        at: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        match src {
            RecSrc::Record(key, payload) => {
                let key_len = key.len() as u64;
                let mut done = 0usize;
                if at < key_len {
                    let n = crate::width::bounded(key_len - at, buf.len());
                    let mut view = self.view();
                    let mut ctx = self.ctx(&mut view, store);
                    KeySrc::Parts(key).read(&mut ctx, at, &mut buf[..n])?;
                    done = n;
                }
                if done < buf.len() {
                    let start = crate::width::index(at + done as u64 - key_len)?;
                    let n = buf.len() - done;
                    buf[done..].copy_from_slice(&payload[start..start + n]);
                }
                Ok(())
            }
            RecSrc::Key(key) => {
                let mut view = self.view();
                let mut ctx = self.ctx(&mut view, store);
                key.read(&mut ctx, at, buf)
            }
        }
    }

    /// Writes bytes `[from, to)` of `src` to a chain of overflow pages and
    /// returns its first page. Every page but the last is full.
    fn write_chain(
        &mut self,
        store: &mut dyn TileStore,
        src: &RecSrc<'_>,
        from: u64,
        to: u64,
    ) -> Result<u64> {
        let (head, mut in_place) = self.alloc_page_placed(store, Use::Overflow)?;
        let mut page_at = head;
        let mut at = from;
        loop {
            let n = (to - at).min(tree::OVERFLOW_DATA);
            let (next, next_in_place) = if at + n < to {
                self.alloc_page_placed(store, Use::Overflow)?
            } else {
                (NO_PAGE, false)
            };
            let mut page = [0u8; PAGE];
            page[16..20].copy_from_slice(tree::OVERFLOW_MAGIC);
            #[allow(clippy::cast_possible_truncation, reason = "at most a page")]
            page[20..22].copy_from_slice(&(n as u16).to_le_bytes());
            page[24..32].copy_from_slice(&next.to_le_bytes());
            let bytes = crate::width::index(n)?;
            self.rec_read(
                &*store,
                src,
                at,
                &mut page[tree::OVERFLOW_AT..tree::OVERFLOW_AT + bytes],
            )?;
            // A page carved, or freed by a generation in place, is one no
            // generation from the mark on uses and nothing staged covers
            // past its link: its bytes past the link go straight to the
            // file. The link, which the published free list may still
            // follow, is left as it is.
            if !(in_place
                && self
                    .ring
                    .write_unreferenced(store, page_at + 16, &page[16..])?)
            {
                self.ring.stage(store, page_at, &page)?;
            }
            at += n;
            if next == NO_PAGE {
                return Ok(head);
            }
            page_at = next;
            in_place = next_in_place;
        }
    }

    /// Frees every page of the overflow chain from `head`.
    fn free_chain(&mut self, store: &mut dyn TileStore, head: u64) -> Result<()> {
        let mut page = head;
        // The chain is as long as its record: bounded.
        while page != NO_PAGE {
            let mut header = [0u8; 16];
            self.read(store, page + 16, &mut header)?;
            if &header[..4] != tree::OVERFLOW_MAGIC {
                return Err(corrupt("keyed overflow page"));
            }
            let next = u64::from_le_bytes(header[8..16].try_into().expect("eight"));
            self.free_page(store, page, Use::Overflow)?;
            page = next;
        }
        Ok(())
    }

    // ---- pages ----

    /// A page for the tree or an overflow chain: the free list's oldest once
    /// the mark has reached the generation that freed it, else one carved
    /// from the last block, else from a new block.
    fn alloc_page(&mut self, store: &mut dyn TileStore, use_: Use) -> Result<u64> {
        Ok(self.alloc_page_placed(store, use_)?.0)
    }

    /// [`Self::alloc_page`], and whether the page's bytes past its link may
    /// be written in place: it was carved, or freed by a generation already
    /// in place (so nothing the ring holds covers it but links).
    fn alloc_page_placed(&mut self, store: &mut dyn TileStore, use_: Use) -> Result<(u64, bool)> {
        let (page, in_place) = match self.take_free(store)? {
            Some((page, freed_by)) => (page, freed_by <= self.ring.checkpointed()),
            None => (self.carve(store)?, true),
        };
        match use_ {
            Use::Leaf => self.state.leaves += 1,
            Use::Interior => self.state.interiors += 1,
            Use::Overflow => self.state.overflow += 1,
        }
        Ok((page, in_place))
    }

    fn take_free(&mut self, store: &mut dyn TileStore) -> Result<Option<(u64, u64)>> {
        let head = self.state.free_head;
        if head == NO_PAGE {
            return Ok(None);
        }
        let mut link = [0u8; 16];
        self.read(store, head, &mut link)?;
        let next = u64::from_le_bytes(link[..8].try_into().expect("eight"));
        let freed_by = u64::from_le_bytes(link[8..].try_into().expect("eight"));
        if freed_by > self.ring.mark() {
            // Waiting for the mark: raise it when the free pages are worth
            // more than the patience and their generation is published.
            if freed_by <= self.ring.published() && self.state.free * PAGE_BYTES > self.patience() {
                self.raise(store)?;
            } else {
                return Ok(None);
            }
        }
        self.state.free_head = next;
        if next == NO_PAGE {
            self.state.free_tail = NO_PAGE;
        }
        self.state.free -= 1;
        self.stats.pages_reused += 1;
        Ok(Some((head, freed_by)))
    }

    /// A page carved from the last block, or from a new one.
    fn carve(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        if self.state.block_used == self.state.block_pages {
            let pages = if self.state.block_pages == 0 {
                FIRST_BLOCK_PAGES
            } else {
                (self.state.block_pages * 2).min(NEW_BLOCK_PAGES)
            };
            let at = self
                .ring
                .place_record(store, RecordKind::Heap, &[], pages * PAGE_BYTES)?;
            self.state.block_at = at;
            self.state.block_pages = pages;
            self.state.block_used = 0;
            self.stats.blocks += 1;
        }
        let page = self.state.block_at + self.state.block_used * PAGE_BYTES;
        self.state.block_used += 1;
        self.state.pages += 1;
        self.stats.pages_carved += 1;
        Ok(page)
    }

    /// Puts `page`, which nothing uses any more, at the end of the free
    /// list. Its first sixteen bytes become its link, through the ring: a
    /// published generation may still read the page.
    fn free_page(&mut self, store: &mut dyn TileStore, page: u64, use_: Use) -> Result<()> {
        self.nodes.forget(page);
        match use_ {
            Use::Leaf => self.state.leaves -= 1,
            Use::Interior => self.state.interiors -= 1,
            Use::Overflow => self.state.overflow -= 1,
        }
        let freeing = self.ring.published() + 1;
        let mut link = [0u8; 16];
        link[8..].copy_from_slice(&freeing.to_le_bytes());
        self.ring.stage(store, page, &link)?;
        let tail = self.state.free_tail;
        if tail == NO_PAGE {
            self.state.free_head = page;
        } else {
            // The old tail's link changes too, and now waits for this
            // generation as well.
            let mut old = [0u8; 16];
            old[..8].copy_from_slice(&page.to_le_bytes());
            old[8..].copy_from_slice(&freeing.to_le_bytes());
            self.ring.stage(store, tail, &old)?;
        }
        self.state.free_tail = page;
        self.state.free += 1;
        self.freed_open += 1;
        Ok(())
    }

    /// A new tree page, buffered and laid out empty.
    fn new_node(
        &mut self,
        store: &mut dyn TileStore,
        kind: node::Kind,
        keep: &[usize],
    ) -> Result<(u64, usize)> {
        let use_ = match kind {
            node::Kind::Leaf => Use::Leaf,
            node::Kind::Interior => Use::Interior,
        };
        let at = self.alloc_page(store, use_)?;
        let slot = self.nodes.create(&mut self.ring, store, at, keep)?;
        node::init(self.nodes.page_mut(slot), kind);
        Ok((at, slot))
    }

    fn free_node(&mut self, store: &mut dyn TileStore, at: u64, kind: node::Kind) -> Result<()> {
        let use_ = match kind {
            node::Kind::Leaf => Use::Leaf,
            node::Kind::Interior => Use::Interior,
        };
        self.free_page(store, at, use_)
    }

    // ---- the tree ----

    /// Sets the leaf cell at `path` (a new or a replaced record).
    fn leaf_set(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        cell: &[u8],
        replace: bool,
    ) -> Result<()> {
        let index = path.index[0];
        let slot = self.nodes.load(&mut self.ring, store, path.pages[0], &[])?;
        let limit = self.tuning.node_bytes;
        let fitted = {
            let (page, scratch) = self.nodes.with_scratch(slot);
            let old = if replace {
                node::entry(page, index)?.1
            } else {
                0
            };
            let cost = if replace {
                cell.len()
            } else {
                node::cost(cell.len())
            };
            if used(page) - old + cost > limit {
                false
            } else if replace {
                node::replace(page, index, cell, scratch)?
            } else {
                node::insert(page, index, cell, scratch)
            }
        };
        if fitted {
            return Ok(());
        }
        if replace {
            node::remove(self.nodes.page_mut(slot), index)?;
        }
        self.split_leaf(store, path, index, cell)
    }

    /// Where cell `index` of the buffered page `slot` keeps its key.
    fn key_loc(&self, slot: usize, index: usize, interior: bool) -> Result<KeyLoc> {
        Ok(tree::held_body(self.nodes.page(slot), self.nodes.at(slot), index, interior)?.key)
    }

    /// Splits the full leaf of `path`, then inserts `cell` at `index`.
    fn split_leaf(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        index: usize,
        cell: &[u8],
    ) -> Result<()> {
        let left_slot = self.nodes.load(&mut self.ring, store, path.pages[0], &[])?;
        let (right, right_slot) = self.new_node(store, node::Kind::Leaf, &[left_slot])?;
        let n = node::count(self.nodes.page(left_slot));
        // A key past every key of the leaf starts the new page alone: keys
        // put in order fill their pages.
        let m = if index >= n {
            n
        } else {
            split_point(self.nodes.page(left_slot))
        };
        {
            let (left, right_page, scratch) = self.nodes.pair_with_scratch(left_slot, right_slot);
            for moved in m..n {
                let (offset, len) = node::entry(left, moved)?;
                let ok = node::insert(right_page, moved - m, &left[offset..offset + len], scratch);
                debug_assert!(ok, "half a page fits a page");
            }
            for moved in (m..n).rev() {
                node::remove(left, moved)?;
            }
            let fitted = if m < n && index <= m {
                node::insert(left, index, cell, scratch)
            } else {
                node::insert(right_page, index - m, cell, scratch)
            };
            if !fitted {
                return Err(corrupt("keyed leaf split"));
            }
        }
        let left_count = node::count(self.nodes.page(left_slot));
        if left_count == 0 {
            // A full leaf keeps at least one cell on the left.
            return Err(corrupt("keyed leaf split"));
        }
        let low = self.key_loc(left_slot, left_count - 1, false)?;
        let high = self.key_loc(right_slot, 0, false)?;
        let sep = self.separator(store, &KeySrc::Loc(low), &KeySrc::Loc(high))?;
        self.stats.splits += 1;
        self.insert_entry(store, path, 1, path.index[1] + 1, right, &sep)
    }

    /// The separator (without its child) for two neighbouring keys: the
    /// shortest prefix of `high` greater than `low`, its bytes past
    /// [`tree::SEP_LOCAL`] in an overflow chain.
    fn separator(
        &mut self,
        store: &mut dyn TileStore,
        low: &KeySrc<'_>,
        high: &KeySrc<'_>,
    ) -> Result<SepBody> {
        let common = {
            let mut view = self.view();
            let mut ctx = self.ctx(&mut view, &*store);
            tree::common_prefix(&mut ctx, low, high)?
        };
        let len = (common + 1).min(high.len());
        let local = tree::sep_local_len(len);
        let mut sep = SepBody::default();
        let mut at = 1;
        at += node::write_varint(&mut sep.bytes[at..], len);
        let local_bytes = crate::width::index(local)?;
        self.rec_read(
            &*store,
            &RecSrc::Key(*high),
            0,
            &mut sep.bytes[at..at + local_bytes],
        )?;
        at += local_bytes;
        if local < len {
            sep.bytes[0] = tree::FLAG_OVERFLOW;
            let head = self.write_chain(store, &RecSrc::Key(*high), local, len)?;
            sep.bytes[at..at + 8].copy_from_slice(&head.to_le_bytes());
            at += 8;
        }
        sep.len = at;
        Ok(sep)
    }

    /// Inserts the entry `child | sep` at `index` of the interior page at
    /// `level` of `path` (a new root past the top), splitting as needed.
    fn insert_entry(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        level: usize,
        index: usize,
        child: u64,
        sep: &SepBody,
    ) -> Result<()> {
        let mut cell = [0u8; 8 + SEP_MAX];
        cell[..8].copy_from_slice(&child.to_le_bytes());
        cell[8..8 + sep.len].copy_from_slice(&sep.bytes[..sep.len]);
        let cell = &cell[..8 + sep.len];
        if level == path.height {
            if path.height == tree::MAX_HEIGHT {
                return Err(corrupt("keyed tree height"));
            }
            let (root, slot) = self.new_node(store, node::Kind::Interior, &[])?;
            let mut first = [0u8; 10];
            first[..8].copy_from_slice(&self.root().0.to_le_bytes());
            let (page, scratch) = self.nodes.with_scratch(slot);
            if !(node::insert(page, 0, &first, scratch) && node::insert(page, 1, cell, scratch)) {
                return Err(corrupt("keyed tree root"));
            }
            let height = self.root().1 + 1;
            self.set_root((root, height))?;
            return Ok(());
        }
        let slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[level], &[])?;
        let limit = self.tuning.node_bytes;
        let (page, scratch) = self.nodes.with_scratch(slot);
        if used(page) + node::cost(cell.len()) <= limit && node::insert(page, index, cell, scratch)
        {
            return Ok(());
        }
        self.split_interior(store, path, level, index, cell)
    }

    /// Splits the full interior page at `level` of `path` and inserts `cell`
    /// at `index`: the separator of the first entry moved right goes up, and
    /// that entry keeps only its child.
    fn split_interior(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        level: usize,
        index: usize,
        cell: &[u8],
    ) -> Result<()> {
        let left_slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[level], &[])?;
        let (right, right_slot) = self.new_node(store, node::Kind::Interior, &[left_slot])?;
        let n = node::count(self.nodes.page(left_slot));
        let m = split_point(self.nodes.page(left_slot)).max(1);
        let mut up = SepBody::default();
        {
            let (left, right_page, scratch) = self.nodes.pair_with_scratch(left_slot, right_slot);
            for moved in m..n {
                let (offset, len) = node::entry(left, moved)?;
                let entry = &left[offset..offset + len];
                let ok = if moved == m {
                    up.len = len - 8;
                    up.bytes[..up.len].copy_from_slice(&entry[8..]);
                    let mut first = [0u8; 10];
                    first[..8].copy_from_slice(&entry[..8]);
                    node::insert(right_page, 0, &first, scratch)
                } else {
                    node::insert(right_page, moved - m, entry, scratch)
                };
                debug_assert!(ok, "half a page fits a page");
            }
            for moved in (m..n).rev() {
                node::remove(left, moved)?;
            }
            let fitted = if index <= m {
                node::insert(left, index, cell, scratch)
            } else {
                node::insert(right_page, index - m, cell, scratch)
            };
            if !fitted {
                return Err(corrupt("keyed interior split"));
            }
        }
        self.stats.splits += 1;
        self.insert_entry(
            store,
            path,
            level + 1,
            path.index[level + 1] + 1,
            right,
            &up,
        )
    }

    /// The interior cell `index` of the buffered page `slot`, parsed.
    fn interior_cell(&self, slot: usize, index: usize) -> Result<Body> {
        tree::held_body(self.nodes.page(slot), self.nodes.at(slot), index, true)
    }

    /// The separator bytes (without the child) of interior cell `index` of
    /// the buffered page `slot`.
    fn raw_separator(&self, slot: usize, index: usize) -> Result<SepBody> {
        let page = self.nodes.page(slot);
        let (offset, len) = node::entry(page, index)?;
        let mut bytes = [0u8; SEP_MAX];
        let sep = len.checked_sub(8).ok_or(corrupt("keyed tree cell"))?;
        bytes
            .get_mut(..sep)
            .ok_or(corrupt("keyed tree cell"))?
            .copy_from_slice(&page[offset + 8..offset + len]);
        Ok(SepBody { bytes, len: sep })
    }

    /// Removes the entry pointing at the page at `level` of `path` from its
    /// parent and frees the page, which holds nothing. A parent left empty
    /// goes too; a root left with one child is replaced by it.
    fn remove_child(&mut self, store: &mut dyn TileStore, path: &Path, level: usize) -> Result<()> {
        let kind = if level == 0 {
            node::Kind::Leaf
        } else {
            node::Kind::Interior
        };
        self.free_node(store, path.pages[level], kind)?;
        let parent = level + 1;
        let index = path.index[parent];
        let slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[parent], &[])?;
        let old = self.interior_cell(slot, index)?;
        if old.overflowed() {
            self.free_chain(store, old.key.overflow)?;
        }
        let slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[parent], &[])?;
        if index == 0 && node::count(self.nodes.page(slot)) > 1 {
            // The next child becomes the first: its separator becomes the
            // lower bound of the whole page, which lives in the lowest
            // ancestor where the path does not take the first child. Keys
            // the removed child held went to the page before it (merges) or
            // are gone (deletes), so that bound may rise to it.
            let ancestor = (parent + 1..path.height).find(|&level| path.index[level] > 0);
            let next = self.interior_cell(slot, 1)?;
            let raw = self.raw_separator(slot, 1)?;
            let mut first = [0u8; 10];
            first[..8].copy_from_slice(&next.child.to_le_bytes());
            {
                let (page, scratch) = self.nodes.with_scratch(slot);
                node::replace(page, 1, &first, scratch)?;
                node::remove(page, index)?;
            }
            match ancestor {
                Some(level) => {
                    self.set_separator(store, path, level, path.index[level], &raw)?;
                }
                // The leftmost page of the tree: no lower bound.
                None if next.overflowed() => self.free_chain(store, next.key.overflow)?,
                None => {}
            }
        } else {
            node::remove(self.nodes.page_mut(slot), index)?;
        }
        let slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[parent], &[])?;
        let left = node::count(self.nodes.page(slot));
        if parent + 1 == path.height {
            if left == 1 && path.height > 1 {
                self.collapse_root(store)?;
            }
            return Ok(());
        }
        if left == 0 {
            return self.remove_child(store, path, parent);
        }
        Ok(())
    }

    /// Replaces a root with one child by that child, while there is one.
    fn collapse_root(&mut self, store: &mut dyn TileStore) -> Result<()> {
        while self.root().1 > 1 {
            let (root, height) = self.root();
            let slot = self.nodes.load(&mut self.ring, store, root, &[])?;
            if node::count(self.nodes.page(slot)) != 1 {
                return Ok(());
            }
            let child = self.interior_cell(slot, 0)?.child;
            self.free_node(store, root, node::Kind::Interior)?;
            self.set_root((child, height - 1))?;
        }
        Ok(())
    }

    /// Replaces the separator of entry `index` of the interior page at
    /// `level` of `path` (its child stays), splitting as needed.
    fn set_separator(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        level: usize,
        index: usize,
        sep: &SepBody,
    ) -> Result<()> {
        let slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[level], &[])?;
        let old = self.interior_cell(slot, index)?;
        if old.overflowed() {
            self.free_chain(store, old.key.overflow)?;
        }
        let slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[level], &[])?;
        node::remove(self.nodes.page_mut(slot), index)?;
        self.insert_entry(store, path, level, index, old.child, sep)
    }

    /// Merges the leaf of `path`, under a quarter full, with a neighbour
    /// under the same parent when the two fit one page (the right one
    /// first, then the left one): the right page's cells move into the left
    /// page, and the right page leaves the tree.
    fn merge_leaf(&mut self, store: &mut dyn TileStore, path: &Path) -> Result<()> {
        let index = path.index[1];
        let count = path.counts[1];
        let mut pairs = [None, None];
        if index + 1 < count {
            pairs[0] = Some((index, index + 1));
        }
        if index > 0 {
            pairs[1] = Some((index - 1, index));
        }
        for (left_index, right_index) in pairs.into_iter().flatten() {
            let parent = self.nodes.load(&mut self.ring, store, path.pages[1], &[])?;
            let left_at = self.interior_cell(parent, left_index)?.child;
            let right_at = self.interior_cell(parent, right_index)?.child;
            let left = self.nodes.load(&mut self.ring, store, left_at, &[])?;
            let right = self.nodes.load(&mut self.ring, store, right_at, &[left])?;
            if used(self.nodes.page(left)) + used(self.nodes.page(right)) > self.tuning.node_bytes {
                continue;
            }
            {
                let (left_page, right_page, scratch) = self.nodes.pair_with_scratch(left, right);
                for moved in 0..node::count(right_page) {
                    let (offset, len) = node::entry(right_page, moved)?;
                    let end = node::count(left_page);
                    if !node::insert(left_page, end, &right_page[offset..offset + len], scratch) {
                        return Err(corrupt("keyed leaf merge"));
                    }
                }
                while node::count(right_page) > 0 {
                    node::remove(right_page, 0)?;
                }
            }
            let mut right_path = *path;
            right_path.pages[0] = right_at;
            right_path.index[1] = right_index;
            self.stats.merges += 1;
            return self.remove_child(store, &right_path, 0);
        }
        Ok(())
    }
}

#[cfg(test)]
impl KeyedHeap {
    /// Checks the tree (page kinds, keys in order within their separators'
    /// bounds, overflow chains), the free list, and the counts the state
    /// keeps.
    pub(crate) fn check_consistency(
        &self,
        store: &dyn TileSource,
    ) -> std::result::Result<(), String> {
        type Bound = Option<Vec<u8>>;
        let text = |error: Error| format!("{error}");
        let mut seen = std::collections::HashSet::new();
        let (mut leaves, mut interiors, mut overflow, mut keys, mut bytes) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        let mut page_bytes = vec![0u8; PAGE];
        let full_key = |loc: KeyLoc| -> std::result::Result<Vec<u8>, String> {
            let mut view = self.view();
            let mut ctx = self.ctx(&mut view, store);
            let mut key = vec![0u8; usize::try_from(loc.len).map_err(|e| e.to_string())?];
            KeySrc::Loc(loc).read(&mut ctx, 0, &mut key).map_err(text)?;
            Ok(key)
        };
        let chain = |head: u64,
                     len: u64,
                     seen: &mut std::collections::HashSet<u64>|
         -> std::result::Result<u64, String> {
            let mut page = head;
            let (mut held, mut pages) = (0u64, 0u64);
            while page != NO_PAGE {
                if !seen.insert(page) {
                    return Err(format!("overflow page {page} reached twice"));
                }
                let mut header = [0u8; 16];
                self.read(store, page + 16, &mut header).map_err(text)?;
                if &header[..4] != tree::OVERFLOW_MAGIC {
                    return Err(format!("page {page} is not an overflow page"));
                }
                held += u64::from(u16::from_le_bytes([header[4], header[5]]));
                pages += 1;
                page = u64::from_le_bytes(header[8..16].try_into().expect("eight"));
            }
            if held != len {
                return Err(format!("overflow chain {head} holds {held} of {len}"));
            }
            Ok(pages)
        };
        let mut stack: Vec<(u64, usize, Bound, Bound)> = Vec::new();
        let mut roots = Vec::new();
        for slot in &self.roots.slots[..self.roots.used()] {
            let height = usize::try_from(slot.height).map_err(|e| e.to_string())?;
            stack.push((slot.page, height - 1, None, None));
            roots.push(slot.page);
        }
        while let Some((page, level, low, high)) = stack.pop() {
            if !seen.insert(page) {
                return Err(format!("tree page {page} reached twice"));
            }
            self.read(store, page, &mut page_bytes).map_err(text)?;
            let kind = node::kind(&page_bytes).map_err(text)?;
            let interior = level > 0;
            if (kind == node::Kind::Interior) != interior {
                return Err(format!("page {page} at level {level} is {kind:?}"));
            }
            let count = node::count(&page_bytes);
            if interior {
                interiors += 1;
                if count == 0 {
                    return Err(format!("interior page {page} is empty"));
                }
            } else {
                leaves += 1;
                keys += count as u64;
                if count == 0 && !roots.contains(&page) {
                    return Err(format!("leaf {page} is empty"));
                }
            }
            let mut previous: Option<Vec<u8>> = None;
            let mut children = Vec::new();
            for index in 0..count {
                let body = tree::held_body(&page_bytes, page, index, interior).map_err(text)?;
                let total = body.key.len + body.value_len;
                if body.overflowed() {
                    overflow += chain(body.key.overflow, total - body.key.local, &mut seen)?;
                }
                if !interior {
                    bytes += total;
                }
                if interior && index == 0 {
                    if body.key.len != 0 {
                        return Err(format!("page {page}: the first separator is not empty"));
                    }
                    children.push((body.child, low.clone()));
                    continue;
                }
                let key = full_key(body.key)?;
                if previous.as_ref().is_some_and(|previous| *previous >= key)
                    || low.as_ref().is_some_and(|low| key < *low)
                    || high.as_ref().is_some_and(|high| key >= *high)
                {
                    return Err(format!("page {page}: entry {index} out of order"));
                }
                previous = Some(key.clone());
                if interior {
                    children.push((body.child, Some(key)));
                }
            }
            for (index, (child, child_low)) in children.iter().enumerate() {
                let child_high = children
                    .get(index + 1)
                    .map_or_else(|| high.clone(), |next| next.1.clone());
                stack.push((*child, level - 1, child_low.clone(), child_high));
            }
        }
        let mut free = 0u64;
        let mut page = self.state.free_head;
        while page != NO_PAGE {
            if !seen.insert(page) {
                return Err(format!("free page {page} is in use or listed twice"));
            }
            free += 1;
            if free > self.state.free {
                return Err("free list longer than counted".into());
            }
            let mut link = [0u8; 16];
            self.read(store, page, &mut link).map_err(text)?;
            let by = u64::from_le_bytes(link[8..].try_into().expect("eight"));
            if by > self.ring.published() + 1 {
                return Err(format!("free page {page} freed by generation {by}"));
            }
            page = u64::from_le_bytes(link[..8].try_into().expect("eight"));
        }
        let counted = (
            leaves,
            interiors,
            overflow,
            free,
            keys,
            bytes,
            leaves + interiors + overflow + free,
        );
        let state = (
            self.state.leaves,
            self.state.interiors,
            self.state.overflow,
            self.state.free,
            self.state.keys,
            self.state.record_bytes,
            self.state.pages,
        );
        if counted != state {
            return Err(format!(
                "leaves, interior, overflow, free pages, keys, bytes, pages: \
                 {counted:?}, state says {state:?}"
            ));
        }
        Ok(())
    }
}

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

/// A separator without its child: flags, length, the bytes its cell keeps
/// and an overflow page.
#[derive(Clone, Copy, Debug)]
struct SepBody {
    bytes: [u8; SEP_MAX],
    len: usize,
}

impl Default for SepBody {
    fn default() -> Self {
        Self {
            bytes: [0; SEP_MAX],
            len: 0,
        }
    }
}

/// Where record bytes come from.
enum RecSrc<'a> {
    /// A record: the key, then the value.
    Record(&'a [u8], &'a [u8]),
    /// A key, as a separator's source.
    Key(KeySrc<'a>),
}

/// The entry to split a full page at: the first whose entries and cells
/// before it take half the page's used bytes, and at least one.
fn split_point(page: &[u8]) -> usize {
    let n = node::count(page);
    let used = used(page);
    let mut seen = 0usize;
    for index in 0..n {
        if seen * 2 >= used {
            return index.max(1);
        }
        let len = node::entry(page, index).map_or(0, |(_, len)| len);
        seen += node::cost(len);
    }
    n.saturating_sub(1).max(1)
}

/// Bytes a tree page's entries and cells use.
fn used(page: &[u8]) -> usize {
    node::CAPACITY - node::free(page)
}
