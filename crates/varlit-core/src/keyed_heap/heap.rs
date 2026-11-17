//! The single writer of keyed storage: puts, deletes and finds records, and
//! keeps free space at the ends of the size classes.
//!
//! Memory is fixed when the writer is opened: the ring's buffers, 64 bucket
//! page buffers and two numbers per size class. Records are read and written
//! through stack buffers; puts, deletes, splits and hole filling allocate
//! nothing.
//!
//! **New items go to the end of their class.** Items `[0, end)` of a class
//! are live, or dead and logged; nothing else. A put appends its items at
//! the ends of their classes and changes the slot and class entries through
//! the ring. The items it replaces are marked dead and logged.
//!
//! **Holes are filled from the end, once.** A logged item is taken once the
//! ring's mark reaches the generation that freed it: no reader and no
//! recovery shows an older generation, so none uses it. The class's last
//! live item is written into it in place (its header through the ring, so
//! that a crash leaves the hole marked dead), its referrer is repointed, and
//! the class shrinks by one; dead items at the end shrink the class with no
//! write at all. When freed items wait longer than the free-space ratio
//! allows, the writer raises the mark: older readers must follow.
//!
//! **Where items are written in place.** An index below `hi` (the largest
//! end any generation from the mark on had) may still be read by a reader
//! or a recovery: it changes through the ring. Indexes from `hi` on, and
//! holes the mark has passed, are written in place when nothing staged
//! covers them.
//!
//! **Staging units.** A reader's overlay keeps one range per start offset
//! and lays overlapping ranges in offset order, so every byte this writer
//! stages is staged in the same units each time: a bucket page whole (from
//! the page buffers, once per publication), an item's header, its bytes in
//! [`STEP`] pieces from the header on (all split at page boundaries), a class
//! entry, an eight-byte directory entry, a page's first sixteen bytes when
//! it joins the pool.

use super::lookup::{self, Ctx, View};
use super::node::{self, NODE_BYTES};
use super::nodes::{HeapView, Nodes};
use super::tree::{self, Body, KeyLoc, KeySrc, Path};
use super::{
    CLASSES, ClassEntry, Fixed, ITEM_HEADER, ITEM_SPACE, Item, LOG_ENTRIES, LOG_ENTRY, LOG_HEADER,
    MAX_BUDGET, MAX_CLASSES, MAX_INDEX, META_FIXED_LEN, META_LEN, META_OVERFLOW_HEAD_AT,
    META_STATE_AT, MIN_BUDGET, NO_ITEM, PAGE_BYTES, PAGE_LINK, RESERVED_BLOCK, State, chain_bytes,
    chain_shape, checked_ref, class_entry_at, class_size, corrupt, item_ref, radix, ref_class,
    ref_index,
};
use core::cmp::Ordering;
use crate::matrix_tiles::ring_store::RingStore;
use crate::matrix_tiles::store::{TileSource, TileStore};
use crate::matrix_tiles::view::directory_offset;
use crate::matrix_tiles::{DIR_PAGE_BYTES, RecordKind};
use crate::{Error, Result};

const STEP: usize = lookup::STEP;
/// The most record bytes a leaf body holds inline (the ratio caps it lower).
const INLINE_MAX: usize = 4096;
/// The most a separator body (without its child) takes.
const SEP_MAX: usize = 1 + 10 + tree::KEY_PREFIX + 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeapStats {
    pub splits: u64,
    /// Leaves emptied by packing.
    pub packs: u64,
    /// Holes filled by a class's last live item, items a class shrank by,
    /// and log entries that named an item already gone or reused.
    pub moves: u64,
    /// Holes a put wrote its new item into.
    pub taken: u64,
    pub shrinks: u64,
    pub stale: u64,
    pub pages_appended: u64,
    pub pages_reused: u64,
    /// Times the writer raised the mark so freed items could be taken, and
    /// put the ring in place so a pooled page could be taken.
    pub marks: u64,
    pub checkpoints: u64,
    /// Rounds the writer published by itself: the change budget was spent.
    pub rounds: u64,
}

/// Knobs of the writer's own work, chosen at open. Tests turn them down so
/// that what builds up in a large file builds up in a small one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tuning {
    /// Logged bytes taken per byte a put or delete frees, at least (holes a
    /// put writes into count). One keeps the log bounded.
    pub fills_per_free: u64,
}

impl Tuning {
    pub(crate) const DEFAULT: Self = Self { fills_per_free: 1 };
}

pub(crate) struct KeyedHeap {
    pub(crate) meta: u64,
    fixed: Fixed,
    pub(crate) ring: RingStore,
    state: State,
    /// The state as the last publication staged it.
    staged: State,
    classes: [ClassEntry; MAX_CLASSES],
    staged_classes: [ClassEntry; MAX_CLASSES],
    nodes: Nodes,
    tuning: Tuning,
    /// The leaf packing works on next: a child index per interior level.
    pack_cursor: [usize; tree::MAX_HEIGHT],
    /// Bytes allocated by the open round, against the change budget.
    round_alloc: u64,
    /// Bytes of items freed by the open generation, and by published
    /// generations the mark has not reached: not to be taken yet.
    holes_open: u64,
    holes_waiting: u64,
    /// Per class, the largest end any generation from the mark on had:
    /// indexes below it may be read by a reader or a recovery.
    hi: [u64; MAX_CLASSES],
    /// Bytes of logged items the open operation's puts took.
    taken_bytes: u64,
    pub(crate) stats: HeapStats,
}

impl KeyedHeap {
    /// Lays out empty keyed storage: the metadata record, a ring of
    /// `ring_len` bytes and an empty leaf as the root, published.
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
        let (root, _) = heap.new_node(store, node::Kind::Leaf, &[])?;
        heap.state.root = (root, 1);
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
        let mut heap = Self::with_parts(meta, fixed, ring, state, tuning)?;
        heap.staged = state;
        for class in 0..CLASSES {
            let mut raw = [0u8; crate::width::small(super::CLASS_ENTRY_LEN)];
            heap.ring
                .read(store, class_entry_at(meta, class), &mut raw)?;
            heap.classes[class] = ClassEntry::decode(&raw)?;
        }
        heap.staged_classes = heap.classes;
        // What generations before the last publication used is unknown
        // here: no reader may keep one.
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
            classes: [ClassEntry::default(); MAX_CLASSES],
            staged_classes: [ClassEntry::default(); MAX_CLASSES],
            nodes: Nodes::new()?,
            tuning,
            pack_cursor: [0; tree::MAX_HEIGHT],
            round_alloc: 0,
            holes_open: 0,
            holes_waiting: 0,
            hi: [0; MAX_CLASSES],
            taken_bytes: 0,
            stats: HeapStats::default(),
        })
    }

    pub(crate) const fn state(&self) -> &State {
        &self.state
    }

    /// The end of class `class`.
    #[cfg(test)]
    pub(crate) const fn class_end(&self, class: usize) -> u64 {
        self.classes[class].end
    }

    /// What the pages in use (all but pooled ones) may hold beyond the
    /// live bytes, at any point: the logged items, a partial and a spare
    /// page per class, and the log's pages. Exact, not a slack: dead items
    /// below a class's end are all logged.
    #[cfg(test)]
    pub(crate) fn idle_bound(&self) -> u64 {
        let log_pages = self.state.logged.div_ceil(LOG_ENTRIES) + 1;
        self.state.hole_bytes
            + (2 * CLASSES as u64 + log_pages + self.state.interiors) * PAGE_BYTES
            + self.state.leaf_free
            + self.state.leaves * node::ENTRIES_AT as u64
    }

    /// Bytes records take: items and leaf entries.
    #[cfg(test)]
    pub(crate) fn live_bytes(&self) -> u64 {
        self.state.live + self.state.leaves * node::CAPACITY as u64 - self.state.leaf_free
    }

    /// What the free bytes in leaves may reach: the ratio of what leaves
    /// hold and a few pages.
    #[cfg(test)]
    pub(crate) fn leaf_free_bound(&self) -> u64 {
        let used = self.state.leaves * node::CAPACITY as u64 - self.state.leaf_free;
        used / 1000 * u64::from(self.fixed.ratio_permille) + 4 * node::CAPACITY as u64
    }

    /// What the logged items may reach. Freed items of the open round are
    /// at most a round (the change budget and one record); those published
    /// and waiting for the mark at most the patience, a round and a record,
    /// since the mark is raised once they pass the patience; and those the
    /// mark has passed never more than the largest such batch, since every
    /// operation takes at least as many logged bytes as it frees.
    #[cfg(test)]
    pub(crate) fn logged_bound(&self, largest_record: u64) -> u64 {
        2 * self.patience() + 3 * (self.fixed.budget + largest_record)
    }

    /// Bytes freed items may wait before the writer raises the mark: the
    /// free-space ratio of the live bytes, at least 1 MiB, at most half the
    /// change budget.
    fn patience(&self) -> u64 {
        (self.state.live / 1000 * u64::from(self.fixed.ratio_permille))
            .max(1 << 20)
            .min(self.fixed.budget / 2)
    }

    /// Raises the ring's mark to the last publication: what it freed may be
    /// taken, and only what it uses must change through the ring.
    fn raise(&mut self, store: &mut dyn TileStore) -> Result<()> {
        if self.ring.mark() < self.ring.published() {
            self.ring.raise_mark(store)?;
            self.stats.marks += 1;
        }
        self.holes_waiting = 0;
        for class in 0..CLASSES {
            self.hi[class] = self.staged_classes[class].end;
        }
        Ok(())
    }

    /// Whether anything changed since the last publication.
    pub(crate) fn is_dirty(&self) -> bool {
        self.ring.is_dirty()
            || self.nodes.is_dirty()
            || self.classes != self.staged_classes
            || self.state != self.staged
    }

    pub(crate) fn publish(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        self.nodes.flush(&mut self.ring, store)?;
        for class in 0..CLASSES {
            if self.classes[class] != self.staged_classes[class] {
                self.ring.stage(
                    store,
                    class_entry_at(self.meta, class),
                    &self.classes[class].encode(),
                )?;
                self.staged_classes[class] = self.classes[class];
            }
        }
        if self.state != self.staged {
            self.ring
                .stage(store, self.meta + META_STATE_AT, &self.state.encode())?;
            self.staged = self.state;
        }
        let generation = self.ring.publish(store)?;
        self.nodes.published();
        self.round_alloc = 0;
        self.holes_waiting += std::mem::take(&mut self.holes_open);
        for class in 0..CLASSES {
            self.hi[class] = self.hi[class].max(self.classes[class].end);
        }
        Ok(generation)
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
        Ctx::new(view, store, self.meta, Some(&self.classes[..CLASSES]))
    }

    fn item(&self, store: &dyn TileSource, item: u64) -> Result<Item> {
        let mut view = self.view();
        self.ctx(&mut view, store).item(item)
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
        let key_len = u32::try_from(key.len()).map_err(|_| Error::LimitExceeded {
            resource: "keyed record key",
            actual: key.len() as u64,
            limit: u64::from(u32::MAX),
        })?;
        let tree_key = 4 + key.len() as u64;
        let total = key.len() as u64 + payload.len() as u64;
        let mut varint = [0u8; 10];
        let varint_len = node::put_varint(&mut varint, tree_key);
        let inline_len = 1 + varint_len as u64 + tree_key + payload.len() as u64;
        let chained = inline_len > self.inline_cap() as u64;
        let bytes = if chained { chain_bytes(total) } else { inline_len };
        // A round changes at most the change budget (bytes written, moved
        // and freed): past it, the round so far is published and the mark
        // raised to it.
        if self.round_alloc > 0 && self.round_alloc + bytes > self.fixed.budget {
            self.end_round(store)?;
        }
        let block = block_id.to_be_bytes();
        let mut body = [0u8; INLINE_MAX];
        body[1..1 + varint_len].copy_from_slice(&varint[..varint_len]);
        let mut at = 1 + varint_len;
        if chained {
            body[0] = tree::FLAG_CHAIN;
            let head = self.write_chain(store, block_id, key_len, false, &RecSrc::Parts(key, payload))?;
            self.state.live += bytes;
            let inline = crate::width::bounded(tree_key, tree::KEY_PREFIX);
            body[at..at + 4].copy_from_slice(&block);
            body[at + 4..at + inline].copy_from_slice(&key[..inline - 4]);
            at += inline;
            body[at..at + 8].copy_from_slice(&total.to_le_bytes());
            body[at + 8..at + 16].copy_from_slice(&head.to_le_bytes());
            at += 16;
        } else {
            body[at..at + 4].copy_from_slice(&block);
            body[at + 4..at + 4 + key.len()].copy_from_slice(key);
            at += 4 + key.len();
            body[at..at + payload.len()].copy_from_slice(payload);
            at += payload.len();
        }
        self.round_alloc += bytes;
        let leaf_free = self.state.leaf_free;
        let path = self.find_path(store, &KeySrc::Parts(block, key))?;
        let mut freed = 0;
        if path.found {
            let old = self.body_at(store, path.pages[0], path.index[0], false)?;
            if old.chained() {
                freed = self.release(store, old.key.head)?;
            }
        } else {
            self.state.keys += 1;
        }
        self.leaf_set(store, &path, &body[..at], path.found)?;
        let opened = self.state.leaf_free.saturating_sub(leaf_free);
        self.pack(store, self.pack_work(opened))?;
        self.fill(store, freed)
    }

    /// Removes `key` of `block_id`; `false` when there is none.
    pub(crate) fn delete(
        &mut self,
        store: &mut dyn TileStore,
        block_id: u32,
        key: &[u8],
    ) -> Result<bool> {
        if self.round_alloc >= self.fixed.budget {
            self.end_round(store)?;
        }
        let path = self.find_path(store, &KeySrc::Parts(block_id.to_be_bytes(), key))?;
        if !path.found {
            return Ok(false);
        }
        let old = self.body_at(store, path.pages[0], path.index[0], false)?;
        let mut freed = 0;
        if old.chained() {
            freed = self.release(store, old.key.head)?;
        }
        let leaf_free = self.state.leaf_free;
        let slot = self.nodes.load(&mut self.ring, store, path.pages[0], &[])?;
        let before = free_of(self.nodes.page(slot));
        node::remove(self.nodes.page_mut(slot), path.index[0])?;
        let after = free_of(self.nodes.page(slot));
        self.state.leaf_free = self.state.leaf_free + after - before;
        self.round_alloc += after - before;
        self.state.keys -= 1;
        if node::count(self.nodes.page(slot)) == 0 && self.state.leaves > 1 {
            self.remove_child(store, &path, 0)?;
        }
        let opened = self.state.leaf_free.saturating_sub(leaf_free);
        self.pack(store, self.pack_work(opened))?;
        self.fill(store, freed)?;
        Ok(true)
    }

    /// Writes a chain of items holding the record bytes `src` gives (a
    /// record's key then payload, or a separator past its block id) at the
    /// ends of their classes, or into freed items the mark has passed, and
    /// returns the head.
    fn write_chain(
        &mut self,
        store: &mut dyn TileStore,
        block_id: u32,
        key_len: u32,
        separator: bool,
        src: &RecSrc<'_>,
    ) -> Result<u64> {
        let total = src.len();
        let (full, last) = chain_shape(total);
        let class_at = |index: u64| if index < full { CLASSES - 1 } else { last };
        let (head, mut taken) = self.alloc(store, class_at(0))?;
        let mut item = head;
        let mut prev = NO_ITEM;
        let mut at = 0u64;
        for index in 0..=full {
            let class = ref_class(item);
            let len = (class_size(class) - ITEM_HEADER).min(total - at);
            let (next, next_taken) = if index < full {
                self.alloc(store, class_at(index + 1))?
            } else {
                (NO_ITEM, false)
            };
            let mut header = Item {
                len: crate::width::small32(crate::width::index(len)?),
                head: index == 0,
                separator,
                block_or_route: if index == 0 { block_id } else { 0 },
                key_len: if index == 0 { key_len } else { 0 },
                crc: 0,
                dead: 0,
                next,
                total_or_prev: if index == 0 { total } else { prev },
            };
            let mut crc = crc32fast::Hasher::new();
            crc.update(&header.crc_prefix());
            let mut step = [0u8; STEP];
            let mut done = 0u64;
            while done < len {
                let n = crate::width::bounded(len - done, STEP);
                self.rec_read(&*store, src, at + done, &mut step[..n])?;
                crc.update(&step[..n]);
                done += n as u64;
            }
            header.crc = crc.finalize();
            // A taken hole is written as a move writes it: its header through
            // the ring, so a crash leaves it dead.
            let in_place = taken || self.in_place(item);
            self.write_header(store, item, &header, !taken && in_place)?;
            // In the units a move writes: STEP pieces from the header on.
            let mut done = 0u64;
            while done < len {
                let n = crate::width::bounded(len - done, STEP);
                self.rec_read(&*store, src, at + done, &mut step[..n])?;
                self.write_bytes(store, item, ITEM_HEADER + done, &step[..n], in_place)?;
                done += n as u64;
            }
            at += len;
            prev = item;
            item = next;
            taken = next_taken;
        }
        Ok(head)
    }

    /// Reads bytes `[at, at + buf.len())` of a chain's record bytes.
    fn rec_read(&self, store: &dyn TileSource, src: &RecSrc<'_>, at: u64, buf: &mut [u8]) -> Result<()> {
        match src {
            RecSrc::Parts(key, payload) => {
                let mut filled = 0usize;
                for_parts(key, payload, at, buf.len() as u64, &mut |bytes| {
                    buf[filled..filled + bytes.len()].copy_from_slice(bytes);
                    filled += bytes.len();
                });
                Ok(())
            }
            RecSrc::Key(key, from, _) => {
                let mut view = self.view();
                let mut ctx = self.ctx(&mut view, store);
                key.read(&mut ctx, from + at, buf)
            }
        }
    }

    /// Whether item `item`, about to be written, may be written in place:
    /// no generation from the mark on had it below its class's end.
    fn in_place(&self, item: u64) -> bool {
        ref_index(item) >= self.hi[ref_class(item)]
    }

    /// Marks every item of the record headed by `head` dead and logs it.
    /// Returns the bytes freed, which count against the round.
    fn release(&mut self, store: &mut dyn TileStore, head: u64) -> Result<u64> {
        let mut item = head;
        let mut freed = 0u64;
        let freeing = self.ring.published() + 1;
        while item != NO_ITEM {
            let mut header = self.item(store, item)?;
            let next = header.next;
            let size = class_size(ref_class(item));
            self.state.live = self
                .state
                .live
                .checked_sub(size)
                .ok_or(corrupt("keyed heap accounting"))?;
            header.dead = freeing;
            let in_place = self.in_place(item);
            self.write_header(store, item, &header, in_place)?;
            self.log_hole(store, item)?;
            freed += size;
            item = next;
        }
        self.round_alloc += freed;
        Ok(freed)
    }

    /// Publishes the open round and raises the mark to it.
    fn end_round(&mut self, store: &mut dyn TileStore) -> Result<()> {
        if self.is_dirty() {
            self.publish(store)?;
            self.stats.rounds += 1;
        }
        self.raise(store)
    }

    // ---- items ----

    /// Writes `bytes` at byte `from` of item `item`, split at page
    /// boundaries: in place when `in_place` and nothing staged covers them,
    /// else through the ring.
    fn write_bytes(
        &mut self,
        store: &mut dyn TileStore,
        item: u64,
        from: u64,
        bytes: &[u8],
        in_place: bool,
    ) -> Result<()> {
        let class = ref_class(item);
        let mut at = ref_index(item) * class_size(class) + from;
        let mut done = 0usize;
        while done < bytes.len() {
            let page = self.page_of(store, class, at / ITEM_SPACE)?;
            let within = PAGE_LINK + at % ITEM_SPACE;
            let n = crate::width::bounded(PAGE_BYTES - within, bytes.len() - done);
            let piece = &bytes[done..done + n];
            if !(in_place && self.ring.write_unreferenced(store, page + within, piece)?) {
                self.ring.stage(store, page + within, piece)?;
            }
            done += n;
            at += n as u64;
        }
        Ok(())
    }

    /// Writes item `item`'s header: the one unit every header write uses.
    fn write_header(
        &mut self,
        store: &mut dyn TileStore,
        item: u64,
        header: &Item,
        in_place: bool,
    ) -> Result<()> {
        self.write_bytes(store, item, 0, &header.encode(), in_place)
    }

    fn page_of(&self, store: &dyn TileSource, class: usize, page: u64) -> Result<u64> {
        let mut view = self.view();
        let at = radix::get(&mut view, store, self.classes[class].dir, page)?;
        if at == 0 {
            return Err(corrupt("keyed item page"));
        }
        Ok(at)
    }

    /// An item of `class` to write: the oldest logged item when it is of
    /// this class and the mark has passed it (`true`: write it as a move
    /// does), else a new item at the class's end. Taking the hole directly
    /// spares appending and then moving.
    fn alloc(&mut self, store: &mut dyn TileStore, class: usize) -> Result<(u64, bool)> {
        if let Some((item, freed_by)) = self.log_peek(store)?
            && freed_by <= self.ring.mark()
            && ref_class(item) == class
            && ref_index(item) < self.classes[class].end
            && self.item(store, item)?.dead == freed_by
        {
            self.log_pop(store, item)?;
            self.stats.taken += 1;
            self.taken_bytes += class_size(class);
            return Ok((item, true));
        }
        Ok((self.append(store, class)?, false))
    }

    /// A new item at the end of `class`.
    fn append(&mut self, store: &mut dyn TileStore, class: usize) -> Result<u64> {
        let index = self.classes[class].end;
        if index >= MAX_INDEX {
            return Err(Error::LimitExceeded {
                resource: "keyed size class items",
                actual: index,
                limit: MAX_INDEX,
            });
        }
        let size = class_size(class);
        let mapped = (index * size).div_ceil(ITEM_SPACE);
        let needed = ((index + 1) * size).div_ceil(ITEM_SPACE);
        for page in mapped..needed {
            // The spare page past the end, if the class kept one.
            let mut view = self.view();
            if radix::get(&mut view, store, self.classes[class].dir, page)? != 0 {
                continue;
            }
            let (at, _) = self.take_page(store)?;
            let root = self.classes[class].dir;
            self.classes[class].dir = self.dir_set(store, root, page, at)?;
        }
        self.classes[class].end = index + 1;
        Ok(item_ref(class, index))
    }

    /// Shrinks `class` by its last item, which is dead. A class keeps one
    /// spare page past the pages its items need, so that a class shrinking
    /// and growing across a page boundary does not take a page each time:
    /// when it needs one page fewer, that page becomes the spare and the
    /// old spare goes to the pool. Nothing is written: a generation may
    /// still use the item, and the pool keeps a page until it is in place.
    fn shrink(&mut self, store: &mut dyn TileStore, class: usize) -> Result<()> {
        let end = self.classes[class].end;
        let size = class_size(class);
        let needed = ((end - 1) * size).div_ceil(ITEM_SPACE);
        let mapped = (end * size).div_ceil(ITEM_SPACE);
        if needed < mapped {
            let spare = mapped;
            let mut view = self.view();
            let at = radix::get(&mut view, store, self.classes[class].dir, spare)?;
            if at != 0 {
                let root = self.classes[class].dir;
                self.classes[class].dir = self.dir_set(store, root, spare, 0)?;
                self.pool_page(store, at)?;
            }
        }
        self.classes[class].end = end - 1;
        self.stats.shrinks += 1;
        Ok(())
    }

    // ---- pages ----

    /// A page for a class or the log: the pool's oldest once the last
    /// generation that staged its link is in place, else a new one. When
    /// that generation is published and the pool holds more than the
    /// patience, the ring is put in place first: a page may go to another
    /// class, whose items start at other offsets, so nothing the ring holds
    /// may still cover it.
    fn take_page(&mut self, store: &mut dyn TileStore) -> Result<(u64, bool)> {
        let head = self.state.pool_head;
        if head != 0 {
            let mut link = [0u8; 16];
            self.read(store, head, &mut link)?;
            let next = u64::from_le_bytes(link[..8].try_into().expect("eight"));
            let pooled_by = u64::from_le_bytes(link[8..].try_into().expect("eight"));
            // Putting the ring in place costs two barriers: only once the
            // pool holds more than the patience, else the file grows by a
            // page instead.
            if pooled_by > self.ring.checkpointed()
                && pooled_by <= self.ring.published()
                && self.state.pooled * PAGE_BYTES > self.patience()
            {
                self.ring.checkpoint(store, u64::MAX)?;
                self.stats.checkpoints += 1;
            }
            if pooled_by <= self.ring.checkpointed() {
                self.state.pool_head =
                    directory_offset(next).map_err(|_| corrupt("keyed page pool"))?;
                if self.state.pool_head == 0 {
                    self.state.pool_tail = 0;
                }
                self.state.pooled -= 1;
                self.stats.pages_reused += 1;
                return Ok((head, true));
            }
        }
        self.state.pages += 1;
        self.stats.pages_appended += 1;
        let at = self
            .ring
            .place_record(store, RecordKind::Heap, &[], PAGE_BYTES)?;
        Ok((at, false))
    }

    /// Puts `page` at the end of the pool. Its first sixteen bytes change
    /// through the ring: a published generation may still read them. A
    /// pooled page's link names the last generation that staged it.
    fn pool_page(&mut self, store: &mut dyn TileStore, page: u64) -> Result<()> {
        let mut link = [0u8; 16];
        link[8..].copy_from_slice(&(self.ring.published() + 1).to_le_bytes());
        self.ring.stage(store, page, &link)?;
        let tail = self.state.pool_tail;
        if tail == 0 {
            self.state.pool_head = page;
        } else {
            // The old tail's link changes too: it now waits for this
            // generation as well, so nothing the ring holds covers a page
            // when it leaves the pool.
            let mut old = [0u8; 16];
            old[..8].copy_from_slice(&page.to_le_bytes());
            old[8..].copy_from_slice(&(self.ring.published() + 1).to_le_bytes());
            self.ring.stage(store, tail, &old)?;
        }
        self.state.pool_tail = page;
        self.state.pooled += 1;
        Ok(())
    }

    // ---- the hole log ----

    fn log_hole(&mut self, store: &mut dyn TileStore, item: u64) -> Result<()> {
        // Entries are never written where a published log may still have
        // one: the log only grows at its tail, and a consumed page waits in
        // the pool until the generation that consumed it is in place.
        if self.state.log_tail == 0 || self.state.log_tail_at == LOG_ENTRIES {
            let (page, _) = self.take_page(store)?;
            // The new page's link: staged, in the unit the pool uses.
            self.ring.stage(store, page, &[0u8; 16])?;
            let old = self.state.log_tail;
            if old == 0 {
                self.state.log_head = page;
                self.state.log_head_at = 0;
            } else if self.state.logged == 0 {
                // Empty, and its only page used up: start on the new one.
                self.state.log_head = page;
                self.state.log_head_at = 0;
                self.pool_page(store, old)?;
            } else {
                // The old tail is part of the published log: its link
                // changes with the publication.
                let mut link = [0u8; 16];
                link[..8].copy_from_slice(&page.to_le_bytes());
                self.ring.stage(store, old, &link)?;
            }
            self.state.log_tail = page;
            self.state.log_tail_at = 0;
        }
        let mut entry = [0u8; 16];
        entry[..8].copy_from_slice(&item.to_le_bytes());
        entry[8..].copy_from_slice(&(self.ring.published() + 1).to_le_bytes());
        let at = self.state.log_tail + LOG_HEADER + self.state.log_tail_at * LOG_ENTRY;
        if !self.ring.write_unreferenced(store, at, &entry)? {
            self.ring.stage(store, at, &entry)?;
        }
        self.state.log_tail_at += 1;
        let size = class_size(ref_class(item));
        self.state.logged += 1;
        self.state.hole_bytes += size;
        self.holes_open += size;
        Ok(())
    }

    /// The oldest logged hole: `(item, generation that freed it)`.
    fn log_peek(&self, store: &dyn TileSource) -> Result<Option<(u64, u64)>> {
        if self.state.logged == 0 {
            return Ok(None);
        }
        let at = self.state.log_head + LOG_HEADER + self.state.log_head_at * LOG_ENTRY;
        let mut entry = [0u8; 16];
        self.read(store, at, &mut entry)?;
        let item = checked_ref(u64::from_le_bytes(entry[..8].try_into().expect("eight")))?;
        let freed = u64::from_le_bytes(entry[8..].try_into().expect("eight"));
        Ok(Some((item, freed)))
    }

    /// Takes the oldest entry, naming `item`, off the log.
    fn log_pop(&mut self, store: &mut dyn TileStore, item: u64) -> Result<()> {
        self.state.logged -= 1;
        self.state.hole_bytes -= class_size(ref_class(item));
        self.state.log_head_at += 1;
        if self.state.log_head_at == LOG_ENTRIES && self.state.log_head != self.state.log_tail {
            let mut link = [0u8; 16];
            self.read(store, self.state.log_head, &mut link)?;
            let next = directory_offset(u64::from_le_bytes(link[..8].try_into().expect("eight")))
                .map_err(|_| corrupt("keyed hole log"))?;
            if next == 0 {
                return Err(corrupt("keyed hole log"));
            }
            let done = self.state.log_head;
            self.state.log_head = next;
            self.state.log_head_at = 0;
            self.pool_page(store, done)?;
        }
        Ok(())
    }

    // ---- holes ----

    /// Takes logged items the mark has passed, at least the tuning's rate
    /// in bytes per byte freed, less what the operation's puts took (and one
    /// item when nothing was freed or taken): each fills
    /// with its class's last live item, or the class shrinks past it.
    /// Raises the mark first when freed items have waited past the patience.
    ///
    /// Taking at least as many bytes as are freed keeps what the log holds
    /// bounded: the items the mark has passed never outgrow the largest
    /// batch that waited for it.
    fn fill(&mut self, store: &mut dyn TileStore, freed: u64) -> Result<()> {
        if self.holes_waiting > self.patience() {
            self.raise(store)?;
        }
        // Holes this operation's puts took count as taken.
        let taken = std::mem::take(&mut self.taken_bytes);
        let mut work = if freed == 0 && taken == 0 {
            1
        } else {
            (freed * self.tuning.fills_per_free).saturating_sub(taken)
        };
        while work > 0 {
            let Some((item, freed_by)) = self.log_peek(store)? else {
                break;
            };
            if freed_by > self.ring.mark() {
                break;
            }
            let class = ref_class(item);
            let index = ref_index(item);
            work = work.saturating_sub(class_size(class));
            // An entry names its item until the class shrinks past it; past
            // that the index may hold another item, live or freed later.
            if index >= self.classes[class].end || self.item(store, item)?.dead != freed_by {
                self.log_pop(store, item)?;
                self.stats.stale += 1;
                continue;
            }
            let last = self.classes[class].end - 1;
            if last == index {
                self.shrink(store, class)?;
                self.log_pop(store, item)?;
                continue;
            }
            let tail = item_ref(class, last);
            if self.item(store, tail)?.dead != 0 {
                // A dead item at the end: its own entry finds it gone.
                self.shrink(store, class)?;
                continue;
            }
            let referrer = self
                .referrer(store, tail)?
                .ok_or(corrupt("keyed item without a referrer"))?;
            self.move_item(store, tail, item, referrer)?;
            self.shrink(store, class)?;
            self.log_pop(store, item)?;
            self.round_alloc += class_size(class);
            self.stats.moves += 1;
        }
        Ok(())
    }

    /// What points at live item `item`: the leaf entry of its record or the
    /// interior entry of its separator (a head), or the item before it. Only
    /// for items not marked dead.
    fn referrer(&self, store: &dyn TileSource, item: u64) -> Result<Option<Referrer>> {
        let header = self.item(store, item)?;
        if !header.head {
            // A live continuation: the item before it points at it.
            let prev = checked_ref(header.total_or_prev)?;
            return Ok((self.item(store, prev)?.next == item).then_some(Referrer::Prev(prev)));
        }
        let key = KeySrc::Item {
            block: header.block_or_route.to_be_bytes(),
            head: item,
            len: 4 + u64::from(header.key_len),
        };
        if !header.separator {
            let path = self.find_path(store, &key)?;
            if !path.found {
                return Ok(None);
            }
            let body = self.body_at(store, path.pages[0], path.index[0], false)?;
            return Ok((body.key.head == item).then_some(Referrer::Entry(path.pages[0], path.index[0])));
        }
        // A separator: the interior entry holding it, on the way down.
        let mut view = self.view();
        let mut ctx = self.ctx(&mut view, store);
        let mut page = self.state.root.0;
        for _ in 1..self.state.root.1 {
            let (_, count) = tree::read_header(&mut ctx, page)?;
            let (index, found) = tree::search_page(&mut ctx, page, count, &key, true)?;
            if found {
                let body = tree::read_body(&mut ctx, page, count, index, true)?;
                return Ok((body.key.head == item).then_some(Referrer::Entry(page, index)));
            }
            page = tree::read_body(&mut ctx, page, count, index - 1, true)?.child;
        }
        Ok(None)
    }

    /// Writes live item `from` into dead item `to` of the same class, which
    /// no generation from the mark on uses, and points its referrer and the
    /// item after it there. The bytes go in place where nothing staged
    /// covers them; the header goes through the ring, so a crash before the
    /// publication leaves `to` marked dead.
    fn move_item(
        &mut self,
        store: &mut dyn TileStore,
        from: u64,
        to: u64,
        referrer: Referrer,
    ) -> Result<()> {
        let header = self.item(store, from)?;
        let len = u64::from(header.len);
        let mut step = [0u8; STEP];
        let mut done = 0u64;
        while done < len {
            let n = crate::width::bounded(len - done, STEP);
            {
                let mut view = self.view();
                self.ctx(&mut view, store)
                    .read_item(from, ITEM_HEADER + done, &mut step[..n])?;
            }
            self.write_bytes(store, to, ITEM_HEADER + done, &step[..n], true)?;
            done += n as u64;
        }
        self.write_header(store, to, &header, false)?;
        match referrer {
            Referrer::Entry(page, index) => {
                // A chained body ends with its head item.
                let slot = self.nodes.load(&mut self.ring, store, page, &[])?;
                let bytes = self.nodes.page_mut(slot);
                let (offset, len) = node::entry(bytes, index)?;
                bytes[offset + len - 8..offset + len].copy_from_slice(&to.to_le_bytes());
            }
            Referrer::Prev(prev) => {
                let mut before = self.item(store, prev)?;
                before.next = to;
                self.write_header(store, prev, &before, false)?;
            }
        }
        if header.next != NO_ITEM {
            let mut after = self.item(store, header.next)?;
            after.total_or_prev = to;
            self.write_header(store, header.next, &after, false)?;
        }
        Ok(())
    }

    // ---- consistency (tests) ----

    /// Checks that every item below each class's end is live (a record
    /// reaches it) or marked dead and logged, that live items add up to the
    /// state's, and that the log's bytes are the state's.
    #[cfg(test)]
    pub(crate) fn check_consistency(
        &self,
        store: &dyn TileSource,
    ) -> std::result::Result<(), String> {
        let text = |error: Error| format!("{error}");
        let mut logged = std::collections::HashSet::new();
        let mut logged_bytes = 0u64;
        let mut at_page = self.state.log_head;
        let mut at = self.state.log_head_at;
        for _ in 0..self.state.logged {
            if at == LOG_ENTRIES {
                let mut link = [0u8; 16];
                self.read(store, at_page, &mut link).map_err(text)?;
                at_page = u64::from_le_bytes(link[..8].try_into().expect("eight"));
                at = 0;
            }
            let mut entry = [0u8; 16];
            self.read(store, at_page + LOG_HEADER + at * LOG_ENTRY, &mut entry)
                .map_err(text)?;
            let item = u64::from_le_bytes(entry[..8].try_into().expect("eight"));
            logged_bytes += class_size(ref_class(item));
            logged.insert(item);
            at += 1;
        }
        // The pool: a chain of `pooled` pages, none mapped by a class.
        let mut mapped = std::collections::HashSet::new();
        for class in 0..CLASSES {
            let size = class_size(class);
            let pages = (self.classes[class].end * size).div_ceil(ITEM_SPACE) + 1;
            let mut view = self.view();
            for page in 0..pages {
                let at =
                    radix::get(&mut view, store, self.classes[class].dir, page).map_err(text)?;
                if at != 0 && !mapped.insert(at) {
                    return Err(format!("page {at} mapped twice"));
                }
            }
        }
        let mut at = self.state.pool_head;
        let mut pooled = 0u64;
        while at != 0 {
            if mapped.contains(&at) {
                return Err(format!("pooled page {at} is mapped"));
            }
            let mut link = [0u8; 16];
            self.read(store, at, &mut link).map_err(text)?;
            let by = u64::from_le_bytes(link[8..].try_into().expect("eight"));
            if by > self.ring.published() + 1 {
                return Err(format!("pooled page {at} by generation {by}"));
            }
            pooled += 1;
            if pooled > self.state.pooled {
                return Err("pool longer than counted".into());
            }
            at = u64::from_le_bytes(link[..8].try_into().expect("eight"));
        }
        if pooled != self.state.pooled {
            return Err(format!(
                "pool holds {pooled}, state says {}",
                self.state.pooled
            ));
        }
        if logged_bytes != self.state.hole_bytes {
            return Err(format!(
                "log holds {logged_bytes} bytes, state says {}",
                self.state.hole_bytes
            ));
        }
        let mut live = 0u64;
        for class in 0..CLASSES {
            for index in 0..self.classes[class].end {
                let item = item_ref(class, index);
                let header = self.item(store, item).map_err(text)?;
                if header.dead != 0 {
                    if !logged.contains(&item) {
                        return Err(format!("dead item {item:x} not logged"));
                    }
                } else if self.referrer(store, item).map_err(text)?.is_some() {
                    live += class_size(class);
                } else {
                    return Err(format!("live item {item:x} without a referrer"));
                }
            }
        }
        // The tree: every page of the right kind, keys in order within the
        // bounds their separators give, chains made of live items, and the
        // counts the state keeps.
        let mut reached = 0u64;
        let (mut leaves, mut interiors, mut keys, mut leaf_free) = (0u64, 0u64, 0u64, 0u64);
        let mut tree_pages = std::collections::HashSet::new();
        let mut page_bytes = vec![0u8; NODE_BYTES];
        let full_key = |loc: KeyLoc| -> std::result::Result<Vec<u8>, String> {
            let mut view = self.view();
            let mut ctx = self.ctx(&mut view, store);
            let mut key = vec![0u8; usize::try_from(loc.len).map_err(|e| e.to_string())?];
            KeySrc::Loc(loc).read(&mut ctx, 0, &mut key).map_err(text)?;
            Ok(key)
        };
        let mut chain = |head: u64, reached: &mut u64| -> std::result::Result<(), String> {
            let mut item = head;
            while item != NO_ITEM {
                let (class, at_index) = (ref_class(item), ref_index(item));
                if at_index >= self.classes[class].end
                    || self.item(store, item).map_err(text)?.dead != 0
                {
                    return Err(format!("a record reaches dead item {item:x}"));
                }
                *reached += class_size(class);
                item = self.item(store, item).map_err(text)?.next;
            }
            Ok(())
        };
        // (page, level, low bound, high bound)
        let height = usize::try_from(self.state.root.1).map_err(|e| e.to_string())?;
        let mut stack: Vec<(u64, usize, Option<Vec<u8>>, Option<Vec<u8>>)> =
            vec![(self.state.root.0, height - 1, None, None)];
        while let Some((page, level, low, high)) = stack.pop() {
            if !tree_pages.insert(page) {
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
                leaf_free += free_of(&page_bytes);
            }
            let mut previous: Option<Vec<u8>> = None;
            let mut children = Vec::new();
            for index in 0..count {
                let (offset, len) = node::entry(&page_bytes, index).map_err(text)?;
                let head = &page_bytes[offset..offset + len.min(tree::HEAD_MAX)];
                let body = tree::parse(head, page + offset as u64, len, interior).map_err(text)?;
                if body.chained() {
                    chain(body.key.head, &mut reached)?;
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
        if (leaves, interiors, keys, leaf_free)
            != (
                self.state.leaves,
                self.state.interiors,
                self.state.keys,
                self.state.leaf_free,
            )
        {
            return Err(format!(
                "tree holds {leaves} leaves, {interiors} interior pages, {keys} keys, \
                 {leaf_free} free leaf bytes; state says {:?}",
                self.state
            ));
        }
        let mut at = self.state.pool_head;
        while at != 0 {
            if tree_pages.contains(&at) || mapped.contains(&at) {
                return Err(format!("pooled page {at} is in use"));
            }
            let mut link = [0u8; 16];
            self.read(store, at, &mut link).map_err(text)?;
            at = u64::from_le_bytes(link[..8].try_into().expect("eight"));
        }
        if reached != live || live != self.state.live {
            return Err(format!(
                "records reach {reached} bytes, {live} live, state says {}",
                self.state.live
            ));
        }
        Ok(())
    }

    // ---- directories ----

    /// Maps `key` to `value` in the directory `root` and returns the root,
    /// grown when it did not reach `key`.
    fn dir_set(
        &mut self,
        store: &mut dyn TileStore,
        root: (u64, u64),
        key: u64,
        value: u64,
    ) -> Result<(u64, u64)> {
        let (mut page, mut height) = root;
        let needed = radix::height_for(key);
        if page == 0 {
            page = self.new_dir_page(store)?;
            height = 1;
        }
        while height < needed {
            let taller = self.new_dir_page(store)?;
            self.ring.stage(store, taller, &page.to_le_bytes())?;
            page = taller;
            height += 1;
        }
        let root = (page, height);
        for level in (1..height).rev() {
            let at = page + radix::index_at(key, level) * 8;
            let mut entry = [0u8; 8];
            self.read(store, at, &mut entry)?;
            let mut child = directory_offset(u64::from_le_bytes(entry))
                .map_err(|_| corrupt("keyed directory entry"))?;
            if child == 0 {
                child = self.new_dir_page(store)?;
                self.ring.stage(store, at, &child.to_le_bytes())?;
            }
            page = child;
        }
        self.ring.stage(
            store,
            page + radix::index_at(key, 0) * 8,
            &value.to_le_bytes(),
        )?;
        Ok(root)
    }

    fn new_dir_page(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        self.ring
            .place_record(store, RecordKind::Directory, &[], DIR_PAGE_BYTES)
    }

    // ---- the tree ----

    /// Record bytes a leaf may hold inline: half the free-space ratio of a
    /// page, so a leaf packed record by record leaves at most that much
    /// free.
    fn inline_cap(&self) -> usize {
        (NODE_BYTES * usize::from(self.fixed.ratio_permille) / 2000).clamp(256, INLINE_MAX)
    }

    /// Packing owed for `opened` bytes left free in leaves: a pass over the
    /// leaves moves about everything they hold and gathers the free bytes
    /// into whole pages, so keeping them within half the ratio takes twice
    /// `opened` divided by the ratio, in bytes of leaves passed.
    fn pack_work(&self, opened: u64) -> u64 {
        opened * 2000 / u64::from(self.fixed.ratio_permille) * self.tuning.fills_per_free
            + node::CAPACITY as u64
    }

    fn find_path(&self, store: &dyn TileSource, key: &KeySrc<'_>) -> Result<Path> {
        let mut view = self.view();
        let mut ctx = self.ctx(&mut view, store);
        tree::find(&mut ctx, self.state.root, key)
    }

    fn body_at(&self, store: &dyn TileSource, page: u64, index: usize, interior: bool) -> Result<Body> {
        let mut view = self.view();
        let mut ctx = self.ctx(&mut view, store);
        let (_, count) = tree::read_header(&mut ctx, page)?;
        tree::read_body(&mut ctx, page, count, index, interior)
    }

    fn compare_keys(&self, store: &dyn TileSource, a: &KeySrc<'_>, b: &KeySrc<'_>) -> Result<Ordering> {
        let mut view = self.view();
        let mut ctx = self.ctx(&mut view, store);
        tree::compare(&mut ctx, a, b)
    }

    /// Finds `key` of `block_id` as this writer sees it.
    pub(crate) fn find(
        &self,
        store: &dyn TileSource,
        block_id: u32,
        key: &[u8],
    ) -> Result<Option<Found>> {
        let path = self.find_path(store, &KeySrc::Parts(block_id.to_be_bytes(), key))?;
        if !path.found {
            return Ok(None);
        }
        let body = self.body_at(store, path.pages[0], path.index[0], false)?;
        Ok(Some(Found { body }))
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

    /// Sets the leaf body for `key` (a new or replaced record) at `path`.
    fn leaf_set(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        body: &[u8],
        replace: bool,
    ) -> Result<()> {
        let leaf = path.pages[0];
        let index = path.index[0];
        let slot = self.nodes.load(&mut self.ring, store, leaf, &[])?;
        let before = free_of(self.nodes.page(slot));
        let fitted = {
            let (page, scratch) = self.nodes.with_scratch(slot);
            if replace {
                node::replace(page, index, body, scratch)?
            } else {
                node::insert(page, index, body, scratch)
            }
        };
        if fitted {
            let after = free_of(self.nodes.page(slot));
            self.state.leaf_free = self.state.leaf_free + after - before;
            return Ok(());
        }
        if replace {
            let page = self.nodes.page_mut(slot);
            node::remove(page, index)?;
            let after = free_of(self.nodes.page(slot));
            self.state.leaf_free = self.state.leaf_free + after - before;
        }
        self.split_leaf(store, path, index, body)
    }

    /// A new page for the tree, buffered and laid out empty.
    fn new_node(&mut self, store: &mut dyn TileStore, kind: node::Kind, keep: &[usize]) -> Result<(u64, usize)> {
        let (at, pooled) = self.take_page(store)?;
        let slot = self.nodes.create(&mut self.ring, store, at, pooled, keep)?;
        node::init(self.nodes.page_mut(slot), kind);
        match kind {
            node::Kind::Leaf => {
                self.state.leaves += 1;
                self.state.leaf_free += node::CAPACITY as u64;
            }
            node::Kind::Interior => self.state.interiors += 1,
        }
        Ok((at, slot))
    }

    /// Frees a tree page that holds nothing any more.
    fn free_node(&mut self, store: &mut dyn TileStore, at: u64, kind: node::Kind) -> Result<()> {
        self.nodes.forget(at);
        match kind {
            node::Kind::Leaf => {
                self.state.leaves -= 1;
                self.state.leaf_free -= node::CAPACITY as u64;
            }
            node::Kind::Interior => self.state.interiors -= 1,
        }
        self.pool_page(store, at)
    }

    /// Where entry `index` of the buffered page `slot` keeps its key.
    fn key_loc(&self, slot: usize, index: usize, interior: bool) -> Result<KeyLoc> {
        let page = self.nodes.page(slot);
        let (offset, len) = node::entry(page, index)?;
        let head = &page[offset..offset + len.min(tree::HEAD_MAX)];
        Ok(tree::parse(head, self.nodes.at(slot) + offset as u64, len, interior)?.key)
    }

    /// Splits the full leaf of `path`, then inserts `body` at `index`.
    fn split_leaf(&mut self, store: &mut dyn TileStore, path: &Path, index: usize, body: &[u8]) -> Result<()> {
        let left_slot = self.nodes.load(&mut self.ring, store, path.pages[0], &[])?;
        let before = free_of(self.nodes.page(left_slot));
        let (right, right_slot) = self.new_node(store, node::Kind::Leaf, &[left_slot])?;
        let n = node::count(self.nodes.page(left_slot));
        let m = if index >= n { n } else { split_point(self.nodes.page(left_slot)) };
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
                node::insert(left, index, body, scratch)
            } else {
                node::insert(right_page, index - m, body, scratch)
            };
            if !fitted {
                return Err(corrupt("keyed leaf split"));
            }
        }
        let after = free_of(self.nodes.page(left_slot)) + free_of(self.nodes.page(right_slot));
        // `new_node` counted the new leaf as wholly free.
        self.state.leaf_free = self.state.leaf_free + after - before - node::CAPACITY as u64;
        let left_count = node::count(self.nodes.page(left_slot));
        let low = self.key_loc(left_slot, left_count - 1, false)?;
        let high = self.key_loc(right_slot, 0, false)?;
        let sep = self.separator(store, &KeySrc::Loc(low), &KeySrc::Loc(high))?;
        self.stats.splits += 1;
        self.insert_entry(store, path, 1, path.index[1] + 1, right, &sep)
    }

    /// The separator body (without its child) for two neighbouring keys: the
    /// shortest prefix of `high` greater than `low`, inline, or in a chain
    /// when longer than [`tree::KEY_PREFIX`].
    fn separator(&mut self, store: &mut dyn TileStore, low: &KeySrc<'_>, high: &KeySrc<'_>) -> Result<SepBody> {
        let common = {
            let mut view = self.view();
            let mut ctx = self.ctx(&mut view, &*store);
            tree::common_prefix(&mut ctx, low, high)?
        };
        let len = (common + 1).min(high.len());
        let mut sep = SepBody::default();
        let inline = crate::width::bounded(len, tree::KEY_PREFIX);
        let mut prefix = [0u8; tree::KEY_PREFIX];
        {
            let mut view = self.view();
            let mut ctx = self.ctx(&mut view, &*store);
            high.read(&mut ctx, 0, &mut prefix[..inline])?;
        }
        let mut at = 0;
        sep.bytes[at] = if len > tree::KEY_PREFIX as u64 { tree::FLAG_CHAIN } else { 0 };
        at += 1;
        at += node::put_varint(&mut sep.bytes[at..], len);
        sep.bytes[at..at + inline].copy_from_slice(&prefix[..inline]);
        at += inline;
        if len > tree::KEY_PREFIX as u64 {
            let block = u32::from_be_bytes(prefix[..4].try_into().expect("four"));
            let key_len = u32::try_from(len - 4).map_err(|_| Error::LimitExceeded {
                resource: "keyed separator",
                actual: len,
                limit: u64::from(u32::MAX),
            })?;
            let head = self.write_chain(store, block, key_len, true, &RecSrc::Key(*high, 4, len - 4))?;
            sep.bytes[at..at + 8].copy_from_slice(&len.to_le_bytes());
            sep.bytes[at + 8..at + 16].copy_from_slice(&head.to_le_bytes());
            at += 16;
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
        let mut body = [0u8; 8 + SEP_MAX];
        body[..8].copy_from_slice(&child.to_le_bytes());
        body[8..8 + sep.len].copy_from_slice(&sep.bytes[..sep.len]);
        let body = &body[..8 + sep.len];
        if level == path.height {
            if path.height == tree::MAX_HEIGHT {
                return Err(corrupt("keyed tree height"));
            }
            let (root, slot) = self.new_node(store, node::Kind::Interior, &[])?;
            let mut first = [0u8; 10];
            first[..8].copy_from_slice(&self.state.root.0.to_le_bytes());
            let (page, scratch) = self.nodes.with_scratch(slot);
            if !(node::insert(page, 0, &first, scratch) && node::insert(page, 1, body, scratch)) {
                return Err(corrupt("keyed tree root"));
            }
            self.state.root = (root, self.state.root.1 + 1);
            return Ok(());
        }
        let slot = self.nodes.load(&mut self.ring, store, path.pages[level], &[])?;
        let (page, scratch) = self.nodes.with_scratch(slot);
        if node::insert(page, index, body, scratch) {
            return Ok(());
        }
        self.split_interior(store, path, level, index, body)
    }

    /// Splits the full interior page at `level` of `path` and inserts
    /// `body` at `index`: the separator of the first entry moved right goes
    /// up, and that entry keeps only its child.
    fn split_interior(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        level: usize,
        index: usize,
        body: &[u8],
    ) -> Result<()> {
        let left_slot = self.nodes.load(&mut self.ring, store, path.pages[level], &[])?;
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
                node::insert(left, index, body, scratch)
            } else {
                node::insert(right_page, index - m, body, scratch)
            };
            if !fitted {
                return Err(corrupt("keyed interior split"));
            }
        }
        self.stats.splits += 1;
        self.insert_entry(store, path, level + 1, path.index[level + 1] + 1, right, &up)
    }

    /// Removes the entry pointing at the page at `level` of `path` from its
    /// parent and frees the page, which holds nothing. A parent left empty
    /// goes too; a root left with one child is replaced by it.
    fn remove_child(&mut self, store: &mut dyn TileStore, path: &Path, level: usize) -> Result<()> {
        let kind = if level == 0 { node::Kind::Leaf } else { node::Kind::Interior };
        self.free_node(store, path.pages[level], kind)?;
        let parent = level + 1;
        let index = path.index[parent];
        let slot = self.nodes.load(&mut self.ring, store, path.pages[parent], &[])?;
        let old = {
            let page = self.nodes.page(slot);
            let (offset, len) = node::entry(page, index)?;
            tree::parse(&page[offset..offset + len.min(tree::HEAD_MAX)], 0, len, true)?
        };
        if old.chained() {
            self.release(store, old.key.head)?;
        }
        let next_sep = if index == 0 && node::count(self.nodes.page(slot)) > 1 {
            let page = self.nodes.page(slot);
            let (offset, len) = node::entry(page, 1)?;
            Some(tree::parse(&page[offset..offset + len.min(tree::HEAD_MAX)], 0, len, true)?)
        } else {
            None
        };
        if let Some(next) = next_sep {
            // The next child becomes the first: it loses its lower bound.
            if next.chained() {
                self.release(store, next.key.head)?;
            }
            let slot = self.nodes.load(&mut self.ring, store, path.pages[parent], &[])?;
            let mut first = [0u8; 10];
            first[..8].copy_from_slice(&next.child.to_le_bytes());
            let (page, scratch) = self.nodes.with_scratch(slot);
            node::replace(page, 1, &first, scratch)?;
        }
        let slot = self.nodes.load(&mut self.ring, store, path.pages[parent], &[])?;
        node::remove(self.nodes.page_mut(slot), index)?;
        let left = node::count(self.nodes.page(slot));
        if parent + 1 == path.height {
            // The root: replaced by its only child.
            if left == 1 && path.height > 1 {
                let child = tree::parse(
                    node::body(self.nodes.page(slot), 0)?,
                    0,
                    node::body(self.nodes.page(slot), 0)?.len(),
                    true,
                )?
                .child;
                self.free_node(store, path.pages[parent], node::Kind::Interior)?;
                self.state.root = (child, self.state.root.1 - 1);
                self.collapse_root(store)?;
            }
            return Ok(());
        }
        if left == 0 {
            return self.remove_child(store, path, parent);
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
        let slot = self.nodes.load(&mut self.ring, store, path.pages[level], &[])?;
        let old = {
            let page = self.nodes.page(slot);
            let (offset, len) = node::entry(page, index)?;
            tree::parse(&page[offset..offset + len.min(tree::HEAD_MAX)], 0, len, true)?
        };
        if old.chained() {
            self.release(store, old.key.head)?;
        }
        let slot = self.nodes.load(&mut self.ring, store, path.pages[level], &[])?;
        node::remove(self.nodes.page_mut(slot), index)?;
        self.insert_entry(store, path, level, index, old.child, sep)
    }

    // ---- packing ----

    /// The path to the leaf the packing cursor is at.
    fn cursor_path(&self, store: &dyn TileSource, cursor: &[usize; tree::MAX_HEIGHT]) -> Result<Path> {
        let mut view = self.view();
        let mut ctx = self.ctx(&mut view, store);
        let height = crate::width::index(self.state.root.1)?;
        let mut path = Path {
            pages: [0; tree::MAX_HEIGHT],
            index: [0; tree::MAX_HEIGHT],
            counts: [0; tree::MAX_HEIGHT],
            height,
            found: false,
        };
        let mut page = self.state.root.0;
        for level in (0..height).rev() {
            let (_, count) = tree::read_header(&mut ctx, page)?;
            path.pages[level] = page;
            path.counts[level] = count;
            if level > 0 {
                let index = cursor[level].min(count.saturating_sub(1));
                path.index[level] = index;
                page = tree::read_body(&mut ctx, page, count, index, true)?.child;
            }
        }
        Ok(path)
    }

    /// The cursor of the leaf after the one `path` leads to; `None` past the
    /// last leaf.
    fn next_cursor(path: &Path) -> Option<[usize; tree::MAX_HEIGHT]> {
        let mut cursor = path.index;
        for level in 1..path.height {
            if cursor[level] + 1 < path.counts[level] {
                cursor[level] += 1;
                for lower in 1..level {
                    cursor[lower] = 0;
                }
                return Some(cursor);
            }
        }
        None
    }

    /// Packs leaves in key order while their free bytes pass the ratio of
    /// what they hold (and two pages): records move from the leaf after the
    /// cursor's into its free space, and a leaf left empty goes. Moves at
    /// least `work` bytes, or stops when nothing is left to pack.
    fn pack(&mut self, store: &mut dyn TileStore, work: u64) -> Result<()> {
        let mut work = work;
        let mut steps = 0u64;
        while work > 0 && self.leaf_slack() && self.state.leaves > 1 {
            steps += 1;
            if steps > 2 * self.state.leaves + 4 {
                break;
            }
            let left_path = self.cursor_path(store, &self.pack_cursor)?;
            let Some(next) = Self::next_cursor(&left_path) else {
                self.pack_cursor = [0; tree::MAX_HEIGHT];
                continue;
            };
            let right_path = self.cursor_path(store, &next)?;
            let left_slot = self.nodes.load(&mut self.ring, store, left_path.pages[0], &[])?;
            let right_slot = self.nodes.load(&mut self.ring, store, right_path.pages[0], &[left_slot])?;
            let before = free_of(self.nodes.page(left_slot)) + free_of(self.nodes.page(right_slot));
            let mut moved = 0u64;
            {
                let (left, right, scratch) = self.nodes.pair_with_scratch(left_slot, right_slot);
                while node::count(right) > 0 {
                    let (offset, len) = node::entry(right, 0)?;
                    let end = node::count(left);
                    if !node::insert(left, end, &right[offset..offset + len], scratch) {
                        break;
                    }
                    node::remove(right, 0)?;
                    moved += len as u64;
                }
            }
            let after = free_of(self.nodes.page(left_slot)) + free_of(self.nodes.page(right_slot));
            self.state.leaf_free = self.state.leaf_free + after - before;
            self.round_alloc += moved;
            work = work.saturating_sub(node::CAPACITY as u64);
            if node::count(self.nodes.page(right_slot)) == 0 {
                self.stats.packs += 1;
                self.remove_child(store, &right_path, 0)?;
                continue;
            }
            if moved > 0 {
                // The separator between the two leaves, where their paths
                // part.
                let level = (1..right_path.height)
                    .rev()
                    .find(|&level| right_path.index[level] != left_path.index[level])
                    .ok_or(corrupt("keyed tree paths"))?;
                let left_slot = self.nodes.load(&mut self.ring, store, left_path.pages[0], &[])?;
                let right_slot =
                    self.nodes.load(&mut self.ring, store, right_path.pages[0], &[left_slot])?;
                let low = self.key_loc(left_slot, node::count(self.nodes.page(left_slot)) - 1, false)?;
                let high = self.key_loc(right_slot, 0, false)?;
                let sep = self.separator(store, &KeySrc::Loc(low), &KeySrc::Loc(high))?;
                self.set_separator(store, &right_path, level, right_path.index[level], &sep)?;
            }
            self.pack_cursor = next;
        }
        Ok(())
    }

    /// Whether leaves hold more free bytes than the ratio allows.
    fn leaf_slack(&self) -> bool {
        let used = self.state.leaves * node::CAPACITY as u64 - self.state.leaf_free;
        self.state.leaf_free
            > used / 1000 * u64::from(self.fixed.ratio_permille) + 2 * node::CAPACITY as u64
    }

    /// Replaces a root with one child by that child, while there is one.
    fn collapse_root(&mut self, store: &mut dyn TileStore) -> Result<()> {
        while self.state.root.1 > 1 {
            let root = self.state.root.0;
            let body = {
                let mut view = self.view();
                let mut ctx = self.ctx(&mut view, &*store);
                let (_, count) = tree::read_header(&mut ctx, root)?;
                if count != 1 {
                    return Ok(());
                }
                tree::read_body(&mut ctx, root, count, 0, true)?
            };
            self.free_node(store, root, node::Kind::Interior)?;
            self.state.root = (body.child, self.state.root.1 - 1);
        }
        Ok(())
    }
}

/// What points at a live item.
#[derive(Clone, Copy, Debug)]
enum Referrer {
    /// A tree page's entry: a leaf's record or an interior separator.
    Entry(u64, usize),
    /// The item before it in its record.
    Prev(u64),
}

/// Calls `visit` with the parts of `key ‖ payload` in `[at, at + len)`.
fn for_parts(key: &[u8], payload: &[u8], at: u64, len: u64, visit: &mut dyn FnMut(&[u8])) {
    // Within `key ‖ payload`, which are in memory.
    let start = usize::try_from(at).expect("within the record");
    let end = usize::try_from(at + len).expect("within the record");
    let key_end = key.len();
    if start < key_end {
        visit(&key[start..end.min(key_end)]);
    }
    if end > key_end {
        visit(&payload[start.max(key_end) - key_end..end - key_end]);
    }
}

/// Where a key's record is: its leaf body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub body: Body,
}

impl Found {
    pub(crate) const fn payload_len(&self) -> u64 {
        tree::value_len(&self.body)
    }
}

/// A separator body without its child: flags, length, the key bytes kept in
/// the page, and a chain's length and head.
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

/// Where a chain's record bytes come from.
enum RecSrc<'a> {
    /// A record: its key then its payload.
    Parts(&'a [u8], &'a [u8]),
    /// `len` bytes of a key from byte `from`.
    Key(KeySrc<'a>, u64, u64),
}

impl RecSrc<'_> {
    fn len(&self) -> u64 {
        match self {
            Self::Parts(key, payload) => key.len() as u64 + payload.len() as u64,
            Self::Key(_, _, len) => *len,
        }
    }
}

/// The entry to split a full page at: the first whose entries and bodies
/// before it take half the page's used bytes, and at least one.
fn split_point(page: &[u8]) -> usize {
    let n = node::count(page);
    let used = node::CAPACITY - node::free(page);
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

/// A page's free bytes, as the state counts them.
fn free_of(page: &[u8]) -> u64 {
    node::free(page) as u64
}
