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

use super::lookup::{self, Ctx, Located, View, slot_at};
use super::pages::{HeapView, Pages};
use super::sip::Sip;
use super::{
    BUCKET_HEADER, BUCKET_PAGE, CLASSES, ClassEntry, Fixed, ITEM_HEADER, ITEM_SPACE, Item,
    LOG_ENTRIES, LOG_ENTRY, LOG_HEADER, MAX_BUDGET, MAX_CLASSES, MAX_INDEX, META_FIXED_LEN,
    META_LEN, META_OVERFLOW_HEAD_AT, META_STATE_AT, MIN_BUDGET, NO_ITEM, PAGE_BYTES, PAGE_LINK,
    RESERVED_BLOCK, SLOT_LEN, SLOTS_PER_PAGE, Slot, State, chain_bytes, chain_shape, checked_ref,
    class_entry_at, class_size, corrupt, item_ref, page_header, radix, ref_class, ref_index,
    set_page_header,
};
use crate::matrix_tiles::ring_store::RingStore;
use crate::matrix_tiles::store::{TileSource, TileStore};
use crate::matrix_tiles::view::directory_offset;
use crate::matrix_tiles::{DIR_PAGE_BYTES, RecordKind};
use crate::{Error, Result};

const HEADER_BYTES: usize = crate::width::small(BUCKET_HEADER);
const SLOT_BYTES: usize = crate::width::small(SLOT_LEN);
const STEP: usize = lookup::STEP;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeapStats {
    pub splits: u64,
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
    pages: Pages,
    tuning: Tuning,
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
    /// `ring_len` bytes, one empty bucket and its directory, published.
    pub(crate) fn create(
        store: &mut dyn TileStore,
        ring_len: u64,
        ratio_permille: u16,
        budget: u64,
        seed: [u64; 2],
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
            seed,
            ring: (0, 0),
            budget,
        };
        let meta = store.append_zeroed(RecordKind::KeyedMeta, &fixed.encode(), META_LEN)?;
        let ring = RingStore::create(store, ring_len, meta + META_OVERFLOW_HEAD_AT)?;
        fixed.ring = (ring.ring.base, ring.ring.len);
        store.write_at(meta, &fixed.encode())?;
        let mut heap = Self::with_parts(meta, fixed, ring, State::default(), tuning)?;
        let root = heap.new_dir_page(store)?;
        let bucket = heap.alloc_bucket_page(store)?;
        let page = heap.pages.page_new(&mut heap.ring, store, bucket)?;
        set_page_header(page, 0, 0);
        heap.ring.stage(store, root, &bucket.to_le_bytes())?;
        heap.state.dir = (root, 1);
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
            pages: Pages::new()?,
            tuning,
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
        self.state.hole_bytes + (2 * CLASSES as u64 + log_pages) * PAGE_BYTES
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
            || self.pages.is_dirty()
            || self.classes != self.staged_classes
            || self.state != self.staged
    }

    pub(crate) fn publish(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        self.pages.flush(&mut self.ring, store)?;
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
            pages: &self.pages,
        }
    }

    fn read(&self, store: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        self.pages.read(&self.ring, store, at, buf)
    }

    fn route(&self, block_id: u32, key: &[u8]) -> u32 {
        lookup::route(self.fixed.seed, block_id, key)
    }

    /// Finds `key` of `block_id` as this writer sees it.
    pub(crate) fn find(
        &self,
        store: &dyn TileSource,
        block_id: u32,
        key: &[u8],
    ) -> Result<Option<Located>> {
        let mut view = self.view();
        let mut ctx = self.ctx(&mut view, store);
        lookup::find(&mut ctx, self.fixed.seed, &self.state, block_id, key)
    }

    /// Reads a located record's payload into `out`, which must be its
    /// length, and checks it.
    pub(crate) fn read_payload(
        &self,
        store: &dyn TileSource,
        found: &Located,
        out: &mut [u8],
    ) -> Result<()> {
        let mut view = self.view();
        let mut ctx = self.ctx(&mut view, store);
        lookup::read_payload(&mut ctx, found, out)
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
        let total = key.len() as u64 + payload.len() as u64;
        let bytes = chain_bytes(total);
        // A round changes at most the change budget (bytes written, moved
        // and freed): past it, the round so far is published and the mark
        // raised to it.
        if self.round_alloc > 0 && self.round_alloc + bytes > self.fixed.budget {
            self.end_round(store)?;
        }
        let route = self.route(block_id, key);
        let head = self.write_record(store, block_id, route, key_len, key, payload)?;
        self.round_alloc += bytes;
        self.state.live += bytes;
        let freed = match self.find(store, block_id, key)? {
            Some(found) => {
                // The same key's old record: point the slot at the new one.
                let old = found.slot.item;
                self.set_slot(store, found.page, found.index, Slot { route, item: head })?;
                self.release(store, old)?
            }
            None => {
                self.insert_slot(
                    store,
                    self.state.bucket_of(route),
                    Slot { route, item: head },
                )?;
                self.state.keys += 1;
                while self.state.keys * 4 > self.state.buckets() * SLOTS_PER_PAGE as u64 * 3 {
                    self.split(store)?;
                }
                0
            }
        };
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
        let Some(found) = self.find(store, block_id, key)? else {
            return Ok(false);
        };
        self.remove_slot(store, found.page, found.index)?;
        self.state.keys -= 1;
        let freed = self.release(store, found.slot.item)?;
        self.fill(store, freed)?;
        Ok(true)
    }

    /// Writes a record's items at the ends of their classes and returns the
    /// head.
    fn write_record(
        &mut self,
        store: &mut dyn TileStore,
        block_id: u32,
        route: u32,
        key_len: u32,
        key: &[u8],
        payload: &[u8],
    ) -> Result<u64> {
        let total = key.len() as u64 + payload.len() as u64;
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
                block_or_route: if index == 0 { block_id } else { route },
                key_len: if index == 0 { key_len } else { 0 },
                crc: 0,
                dead: 0,
                next,
                total_or_prev: if index == 0 { total } else { prev },
            };
            let mut crc = crc32fast::Hasher::new();
            crc.update(&header.crc_prefix());
            for_parts(key, payload, at, len, &mut |bytes| crc.update(bytes));
            header.crc = crc.finalize();
            // A taken hole is written as a move writes it: its header through
            // the ring, so a crash leaves it dead.
            let in_place = taken || self.in_place(item);
            self.write_header(store, item, &header, !taken && in_place)?;
            // In the units a move writes: STEP pieces from the header on.
            let mut step = [0u8; STEP];
            let mut done = 0u64;
            while done < len {
                let n = crate::width::bounded(len - done, STEP);
                let mut filled = 0usize;
                for_parts(key, payload, at + done, n as u64, &mut |bytes| {
                    step[filled..filled + bytes.len()].copy_from_slice(bytes);
                    filled += bytes.len();
                });
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
            let at = self.take_page(store)?;
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
    fn take_page(&mut self, store: &mut dyn TileStore) -> Result<u64> {
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
                return Ok(head);
            }
        }
        self.state.pages += 1;
        self.stats.pages_appended += 1;
        self.ring
            .place_record(store, RecordKind::Heap, &[], PAGE_BYTES)
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
            let page = self.take_page(store)?;
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

    /// What points at live item `item`: its slot (a head) or the item
    /// before it. Only for items not marked dead.
    fn referrer(&self, store: &dyn TileSource, item: u64) -> Result<Option<Referrer>> {
        let header = self.item(store, item)?;
        if header.head {
            let route = self.head_route(store, item, &header)?;
            return Ok(self
                .slot_with(store, route, |slot, _| Ok(slot.item == item))?
                .map(|(page, index)| Referrer::Slot(page, index, route)));
        }
        // A live continuation: the item before it points at it.
        let prev = checked_ref(header.total_or_prev)?;
        Ok((self.item(store, prev)?.next == item).then_some(Referrer::Prev(prev)))
    }

    /// The route of the record headed by `head`, from its key.
    fn head_route(&self, store: &dyn TileSource, head: u64, header: &Item) -> Result<u32> {
        let mut hasher = Sip::new(self.fixed.seed);
        hasher.write(&header.block_or_route.to_le_bytes());
        let mut left = u64::from(header.key_len);
        if left > 0 {
            let mut view = self.view();
            let mut ctx = self.ctx(&mut view, store);
            lookup::stream(&mut ctx, head, &mut |bytes| {
                let n = crate::width::bounded(left, bytes.len());
                hasher.write(&bytes[..n]);
                left -= n as u64;
                Ok(left > 0)
            })?;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the low 32 bits route and fingerprint"
        )]
        let route = hasher.finish() as u32;
        Ok(route)
    }

    /// The first slot of route `route` that `matches`: `(page, index)`.
    fn slot_with(
        &self,
        store: &dyn TileSource,
        route: u32,
        mut matches: impl FnMut(&Slot, &Self) -> Result<bool>,
    ) -> Result<Option<(u64, usize)>> {
        let mut at = self.bucket(store, self.state.bucket_of(route))?;
        let mut page = [0u8; crate::width::small(BUCKET_PAGE)];
        while at != 0 {
            self.read(store, at, &mut page)?;
            let (count, next) = page_header(&page)?;
            for index in 0..count {
                let slot = slot_at(&page, index);
                if slot.route == route && matches(&slot, self)? {
                    return Ok(Some((at, index)));
                }
            }
            at = next;
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
            Referrer::Slot(page, index, route) => {
                self.set_slot(store, page, index, Slot { route, item: to })?;
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
        // Every record a slot reaches is made of live items.
        let mut reached = 0u64;
        for bucket in 0..self.state.buckets() {
            let mut at = self.bucket(store, bucket).map_err(text)?;
            let mut page = [0u8; crate::width::small(BUCKET_PAGE)];
            while at != 0 {
                self.read(store, at, &mut page).map_err(text)?;
                let (count, next) = page_header(&page).map_err(text)?;
                for index in 0..count {
                    let mut item = slot_at(&page, index).item;
                    while item != NO_ITEM {
                        let (class, at_index) = (ref_class(item), ref_index(item));
                        if at_index >= self.classes[class].end
                            || self.item(store, item).map_err(text)?.dead != 0
                        {
                            return Err(format!("a record reaches dead item {item:x}"));
                        }
                        reached += class_size(class);
                        item = self.item(store, item).map_err(text)?.next;
                    }
                }
                at = next;
            }
        }
        if reached != live || live != self.state.live {
            return Err(format!(
                "records reach {reached} bytes, {live} live, state says {}",
                self.state.live
            ));
        }
        Ok(())
    }

    // ---- buckets ----

    fn set_slot(
        &mut self,
        store: &mut dyn TileStore,
        page: u64,
        index: usize,
        slot: Slot,
    ) -> Result<()> {
        let bytes = self.pages.page_mut(&mut self.ring, store, page)?;
        put_slot(bytes, index, &slot.encode());
        Ok(())
    }

    /// Replaces the bucket page at `at` with `page`.
    fn put_page(
        &mut self,
        store: &mut dyn TileStore,
        at: u64,
        page: &[u8; crate::width::small(BUCKET_PAGE)],
    ) -> Result<()> {
        self.pages
            .page_new(&mut self.ring, store, at)?
            .copy_from_slice(page);
        Ok(())
    }

    fn insert_slot(&mut self, store: &mut dyn TileStore, bucket: u64, slot: Slot) -> Result<()> {
        let mut at = self.bucket(store, bucket)?;
        loop {
            let mut header = [0u8; crate::width::small(BUCKET_HEADER)];
            self.read(store, at, &mut header)?;
            let (count, next) = page_header(&header)?;
            if count < SLOTS_PER_PAGE {
                let page = self.pages.page_mut(&mut self.ring, store, at)?;
                put_slot(page, count, &slot.encode());
                set_page_header(page, count + 1, next);
                return Ok(());
            }
            if next == 0 {
                let page_at = self.alloc_bucket_page(store)?;
                let page = self.pages.page_new(&mut self.ring, store, page_at)?;
                set_page_header(page, 1, 0);
                put_slot(page, 0, &slot.encode());
                let page = self.pages.page_mut(&mut self.ring, store, at)?;
                set_page_header(page, count, page_at);
                return Ok(());
            }
            at = next;
        }
    }

    fn remove_slot(&mut self, store: &mut dyn TileStore, page: u64, index: usize) -> Result<()> {
        let bytes = self.pages.page_mut(&mut self.ring, store, page)?;
        let (count, next) = page_header(bytes)?;
        let last = count.checked_sub(1).ok_or(corrupt("keyed bucket page"))?;
        if index != last {
            let from = HEADER_BYTES + last * SLOT_BYTES;
            let to = HEADER_BYTES + index * SLOT_BYTES;
            bytes.copy_within(from..from + SLOT_BYTES, to);
        }
        set_page_header(bytes, last, next);
        Ok(())
    }

    /// Splits the bucket at the split pointer: slots whose next routing bit
    /// is set move to a new bucket, the rest are packed back into the old
    /// bucket's pages, and pages left over go to the free list.
    fn split(&mut self, store: &mut dyn TileStore) -> Result<()> {
        let from = self.state.split;
        let bit = 1u64 << self.state.level;
        let to = from + bit;
        let head = self.bucket(store, from)?;
        let to_head = self.alloc_bucket_page(store)?;
        let dir = self.state.dir;
        self.state.dir = self.dir_set(store, dir, to, to_head)?;

        let mut read = [0u8; crate::width::small(BUCKET_PAGE)];
        let mut stay = [0u8; crate::width::small(BUCKET_PAGE)];
        let mut moved = [0u8; crate::width::small(BUCKET_PAGE)];
        let (mut stay_count, mut moved_count) = (0usize, 0usize);
        // Stay pages overwrite the old chain from its head: never ahead of
        // the page being read, since they hold no more slots than were read.
        let mut stay_at = head;
        let mut moved_at = to_head;
        let mut at = head;
        while at != 0 {
            self.read(store, at, &mut read)?;
            let (count, next) = page_header(&read)?;
            for index in 0..count {
                let start = HEADER_BYTES + index * SLOT_BYTES;
                let bytes = &read[start..start + SLOT_BYTES];
                if u64::from(Slot::decode(bytes).route) & bit != 0 {
                    if moved_count == SLOTS_PER_PAGE {
                        let next_page = self.alloc_bucket_page(store)?;
                        set_page_header(&mut moved, moved_count, next_page);
                        self.put_page(store, moved_at, &moved)?;
                        moved_at = next_page;
                        moved_count = 0;
                    }
                    put_slot(&mut moved, moved_count, bytes);
                    moved_count += 1;
                } else {
                    if stay_count == SLOTS_PER_PAGE {
                        let next_page = self.next_bucket_page(store, stay_at)?;
                        set_page_header(&mut stay, stay_count, next_page);
                        self.put_page(store, stay_at, &stay)?;
                        stay_at = next_page;
                        stay_count = 0;
                    }
                    put_slot(&mut stay, stay_count, bytes);
                    stay_count += 1;
                }
            }
            at = next;
        }
        let mut spare = self.next_bucket_page(store, stay_at)?;
        set_page_header(&mut stay, stay_count, 0);
        zero_slots(&mut stay, stay_count);
        self.put_page(store, stay_at, &stay)?;
        set_page_header(&mut moved, moved_count, 0);
        zero_slots(&mut moved, moved_count);
        self.put_page(store, moved_at, &moved)?;
        while spare != 0 {
            let next = self.next_bucket_page(store, spare)?;
            self.free_bucket_page(store, spare)?;
            spare = next;
        }
        self.state = self.state.after_split();
        self.stats.splits += 1;
        Ok(())
    }

    fn next_bucket_page(&self, store: &dyn TileSource, page: u64) -> Result<u64> {
        let mut header = [0u8; crate::width::small(BUCKET_HEADER)];
        self.read(store, page, &mut header)?;
        Ok(page_header(&header)?.1)
    }

    fn alloc_bucket_page(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        let head = self.state.free_buckets;
        if head != 0 {
            let mut link = [0u8; 8];
            self.read(store, head + 8, &mut link)?;
            self.state.free_buckets = u64::from_le_bytes(link);
            return Ok(head);
        }
        let mut header = [0u8; crate::width::small(BUCKET_HEADER)];
        set_page_header(&mut header, 0, 0);
        self.ring
            .place_record(store, RecordKind::Bucket, &header, BUCKET_PAGE)
    }

    fn free_bucket_page(&mut self, store: &mut dyn TileStore, page: u64) -> Result<()> {
        let free = self.state.free_buckets;
        let bytes = self.pages.page_new(&mut self.ring, store, page)?;
        set_page_header(bytes, 0, free);
        self.state.free_buckets = page;
        Ok(())
    }

    /// The first page of bucket `bucket`.
    fn bucket(&self, store: &dyn TileSource, bucket: u64) -> Result<u64> {
        let mut view = self.view();
        let mut ctx = self.ctx(&mut view, store);
        lookup::bucket(&mut ctx, &self.state, bucket)
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
}

/// What points at a live item.
#[derive(Clone, Copy, Debug)]
enum Referrer {
    /// Its slot: page, index, route.
    Slot(u64, usize, u32),
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

fn put_slot(page: &mut [u8], index: usize, bytes: &[u8]) {
    let start = HEADER_BYTES + index * SLOT_BYTES;
    page[start..start + SLOT_BYTES].copy_from_slice(bytes);
}

fn zero_slots(page: &mut [u8], from: usize) {
    let start = HEADER_BYTES + from * SLOT_BYTES;
    page[start..].fill(0);
}
