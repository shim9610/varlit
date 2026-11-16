//! The single writer of keyed storage: puts, deletes and finds records, and
//! keeps free space at the ends of the size classes.
//!
//! Memory is fixed when the writer is opened: the ring's buffers, 64 bucket
//! page buffers and one entry per size class. Records are read and written
//! through stack buffers; puts, deletes, splits and hole filling allocate
//! nothing.
//!
//! **New values go to the end of their class.** A put appends its items at
//! the ends of their classes (indexes no generation uses), writes them
//! straight to the file, and changes only the slot and class entries through
//! the ring. The items it replaces are logged as holes.
//!
//! **Holes are filled from the end.** A hole is filled once the generation
//! that freed it is in place: the class's last item moves into it (and its
//! old place is logged as a hole in turn), or, when the hole is the last
//! item, the class shrinks by one. A class only shrinks by the hole being
//! filled, so the log never names an index past a class's end, and an index
//! is reused only after it was dead in every generation a reader or a
//! recovery may use.
//!
//! **Staging units.** A reader's overlay keeps one range per start offset
//! and lays overlapping ranges in offset order, so every byte this writer
//! stages is staged in the same units each time: a bucket page whole (from
//! the page buffers, once per publication), an item's header and its bytes
//! (split at page boundaries), a class entry, an eight-byte directory entry,
//! a page's first sixteen bytes when it joins the pool.

use super::lookup::{self, Ctx, Located, View, slot_at};
use super::pages::{HeapView, Pages};
use super::sip::Sip;
use super::{
    BITS_PER_PAGE, BUCKET_HEADER, BUCKET_PAGE, CLASSES, ClassEntry, Fixed, ITEM_HEADER, Item,
    LOG_ENTRIES, LOG_ENTRY, LOG_HEADER, MAX_BUDGET, MAX_CLASSES, MAX_INDEX, META_FIXED_LEN,
    META_LEN, META_OVERFLOW_HEAD_AT, META_STATE_AT, MIN_BUDGET, NO_ITEM, PAGE_BYTES,
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
    /// Holes taken by new items, filled by moving a class's last item, and
    /// removed by shrinking a class.
    pub holes_reused: u64,
    pub moves: u64,
    pub shrinks: u64,
    pub pages_appended: u64,
    pub pages_reused: u64,
    /// Checkpoints run so logged holes could be filled.
    pub checkpoints: u64,
    /// Rounds the writer published by itself: the change budget was spent.
    pub rounds: u64,
}

/// Knobs of the writer's own work, chosen at open. Tests turn them down so
/// that what builds up in a large file builds up in a small one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tuning {
    /// Holes filled per item a put or delete frees, at least.
    pub fills_per_free: u64,
}

impl Tuning {
    pub(crate) const DEFAULT: Self = Self { fills_per_free: 2 };
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
    /// Bytes of holes logged by the open generation, and by published
    /// generations not yet in place: not fillable before a checkpoint.
    holes_open: u64,
    holes_waiting: u64,
    /// Where each class's next search for a bitmap hole starts.
    cursors: [u64; MAX_CLASSES],
    /// The class compaction looks at next.
    compact_class: usize,
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
        // Holes the published state logged wait for its checkpoint.
        let in_bitmaps: u64 = heap.classes[..CLASSES]
            .iter()
            .enumerate()
            .map(|(class, entry)| entry.holes * class_size(class))
            .sum();
        heap.holes_waiting = state.hole_bytes.saturating_sub(in_bitmaps);
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
            cursors: [0; MAX_CLASSES],
            compact_class: 0,
            stats: HeapStats::default(),
        })
    }

    pub(crate) const fn state(&self) -> &State {
        &self.state
    }

    /// Heap bytes that hold no live item: holes, the unused ends of class
    /// pages, log pages and pooled pages.
    #[cfg(test)]
    pub(crate) const fn idle(&self) -> u64 {
        (self.state.pages * PAGE_BYTES).saturating_sub(self.state.live)
    }

    /// What [`Self::idle`] may reach at any point: two rounds of the change
    /// budget (holes not yet fillable), one partial page per class, the
    /// pages the hole log takes, and a few pages of slack.
    #[cfg(test)]
    pub(crate) fn idle_bound(&self) -> u64 {
        let log_pages = self.state.logged / LOG_ENTRIES + 2;
        // A bitmap page per class in use, at most.
        let bitmap_pages = CLASSES as u64;
        self.allowance()
            + 2 * self.fixed.budget
            + (CLASSES as u64 + log_pages + bitmap_pages + 8) * PAGE_BYTES
    }

    /// Idle bytes in reusable holes the ratio allows before compaction
    /// moves items to return pages: the declared ratio of live bytes, at
    /// least a page.
    fn allowance(&self) -> u64 {
        (self.state.live / 1000 * u64::from(self.fixed.ratio_permille)).max(PAGE_BYTES)
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
        Ctx {
            view,
            source: store,
            meta: self.meta,
            classes: Some(&self.classes[..CLASSES]),
        }
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
        // A round allocates at most the change budget: past it, the round
        // so far is published and put in place, and its holes filled.
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
    /// head. The items are new: nothing a published generation uses.
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
        let chain = self.state.next_chain;
        self.state.next_chain += 1;
        let class_at = |index: u64| if index < full { CLASSES - 1 } else { last };
        let head = self.alloc(store, class_at(0))?;
        let mut item = head;
        let mut prev = NO_ITEM;
        let mut at = 0u64;
        for index in 0..=full {
            let class = ref_class(item);
            let len = (class_size(class) - ITEM_HEADER).min(total - at);
            let next = if index < full {
                self.alloc(store, class_at(index + 1))?
            } else {
                NO_ITEM
            };
            let mut header = Item {
                len: crate::width::small32(crate::width::index(len)?),
                head: index == 0,
                block_or_route: if index == 0 { block_id } else { route },
                key_len: if index == 0 { key_len } else { 0 },
                crc: 0,
                chain,
                next,
                total_or_prev: if index == 0 { total } else { prev },
            };
            let mut crc = crc32fast::Hasher::new();
            crc.update(&header.crc_prefix());
            for_parts(key, payload, at, len, &mut |bytes| crc.update(bytes));
            header.crc = crc.finalize();
            self.write_item(store, item, 0, &header.encode())?;
            let mut done = 0u64;
            let mut failed = None;
            for_parts(key, payload, at, len, &mut |bytes| {
                if failed.is_none()
                    && let Err(error) = self.write_item(store, item, ITEM_HEADER + done, bytes)
                {
                    failed = Some(error);
                }
                done += bytes.len() as u64;
            });
            if let Some(error) = failed {
                return Err(error);
            }
            at += len;
            prev = item;
            item = next;
        }
        Ok(head)
    }

    /// Logs every item of the record headed by `head` as a hole. Returns
    /// the items freed.
    fn release(&mut self, store: &mut dyn TileStore, head: u64) -> Result<u64> {
        let mut item = head;
        let mut freed = 0u64;
        while item != NO_ITEM {
            let next = self.item(store, item)?.next;
            let size = class_size(ref_class(item));
            self.state.live = self
                .state
                .live
                .checked_sub(size)
                .ok_or(corrupt("keyed heap accounting"))?;
            self.log_hole(store, item)?;
            freed += 1;
            item = next;
        }
        Ok(freed)
    }

    /// Publishes the open round and puts it in place.
    fn end_round(&mut self, store: &mut dyn TileStore) -> Result<()> {
        if self.is_dirty() {
            self.publish(store)?;
            self.stats.rounds += 1;
        }
        self.put_in_place(store)
    }

    fn put_in_place(&mut self, store: &mut dyn TileStore) -> Result<()> {
        if self.ring.published() > self.ring.checkpointed() {
            self.ring.checkpoint(store, u64::MAX)?;
            self.stats.checkpoints += 1;
        }
        self.holes_waiting = 0;
        Ok(())
    }

    // ---- items ----

    /// Writes `bytes` at byte `from` of item `item`, straight to the file
    /// where nothing staged covers them, split at page boundaries.
    fn write_item(
        &mut self,
        store: &mut dyn TileStore,
        item: u64,
        from: u64,
        bytes: &[u8],
    ) -> Result<()> {
        let class = ref_class(item);
        let mut at = ref_index(item) * class_size(class) + from;
        let mut done = 0usize;
        while done < bytes.len() {
            let page = self.page_of(store, class, at / PAGE_BYTES)?;
            let within = at % PAGE_BYTES;
            let n = crate::width::bounded(PAGE_BYTES - within, bytes.len() - done);
            let piece = &bytes[done..done + n];
            if !self.ring.write_unreferenced(store, page + within, piece)? {
                self.ring.stage(store, page + within, piece)?;
            }
            done += n;
            at += n as u64;
        }
        Ok(())
    }

    /// Changes the header of a live item: through the ring, with the
    /// publication.
    fn rewrite_header(
        &mut self,
        store: &mut dyn TileStore,
        item: u64,
        header: &Item,
    ) -> Result<()> {
        let class = ref_class(item);
        let at = ref_index(item) * class_size(class);
        let bytes = header.encode();
        let mut done = 0usize;
        while done < bytes.len() {
            let offset = at + done as u64;
            let page = self.page_of(store, class, offset / PAGE_BYTES)?;
            let within = offset % PAGE_BYTES;
            let n = crate::width::bounded(PAGE_BYTES - within, bytes.len() - done);
            self.ring
                .stage(store, page + within, &bytes[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    fn page_of(&self, store: &dyn TileSource, class: usize, page: u64) -> Result<u64> {
        let mut view = self.view();
        let at = radix::get(&mut view, store, self.classes[class].dir, page)?;
        if at == 0 {
            return Err(corrupt("keyed item page"));
        }
        Ok(at)
    }

    /// An item of `class` no generation uses: a hole of the class's bitmap
    /// when it has one, else a new item at its end.
    fn alloc(&mut self, store: &mut dyn TileStore, class: usize) -> Result<u64> {
        if self.classes[class].holes > 0 {
            let index = self.next_hole(store, class)?;
            self.set_bit(store, class, Bitmap::Holes, index, false)?;
            self.classes[class].holes -= 1;
            self.state.hole_bytes -= class_size(class);
            self.stats.holes_reused += 1;
            return Ok(item_ref(class, index));
        }
        self.append(store, class)
    }

    /// A new item at the end of `class`: an index no generation uses.
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
        let mapped = (index * size).div_ceil(PAGE_BYTES);
        let needed = ((index + 1) * size).div_ceil(PAGE_BYTES);
        for page in mapped..needed {
            let at = self.take_page(store)?;
            let root = self.classes[class].dir;
            self.classes[class].dir = self.dir_set(store, root, page, at)?;
        }
        self.classes[class].end = index + 1;
        Ok(item_ref(class, index))
    }

    /// Shrinks `class` by its last item, which is a hole no generation uses,
    /// returning pages left empty to the pool.
    fn shrink(&mut self, store: &mut dyn TileStore, class: usize) -> Result<()> {
        let end = self.classes[class].end;
        let size = class_size(class);
        let needed = ((end - 1) * size).div_ceil(PAGE_BYTES);
        let mapped = (end * size).div_ceil(PAGE_BYTES);
        for page in needed..mapped {
            let at = self.page_of(store, class, page)?;
            let root = self.classes[class].dir;
            self.classes[class].dir = self.dir_set(store, root, page, 0)?;
            self.pool_page(store, at)?;
        }
        self.classes[class].end = end - 1;
        self.stats.shrinks += 1;
        Ok(())
    }

    // ---- pages ----

    /// A page for a class or the log: the pool's oldest, once the
    /// generation that pooled it is in place, else a new one.
    fn take_page(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        let head = self.state.pool_head;
        if head != 0 {
            let mut link = [0u8; 16];
            self.read(store, head, &mut link)?;
            let next = u64::from_le_bytes(link[..8].try_into().expect("eight"));
            let pooled_by = u64::from_le_bytes(link[8..].try_into().expect("eight"));
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
    /// through the ring: a published generation may still read them.
    fn pool_page(&mut self, store: &mut dyn TileStore, page: u64) -> Result<()> {
        let mut link = [0u8; 16];
        link[8..].copy_from_slice(&(self.ring.published() + 1).to_le_bytes());
        self.ring.stage(store, page, &link)?;
        let tail = self.state.pool_tail;
        if tail == 0 {
            self.state.pool_head = page;
        } else {
            let mut old = [0u8; 16];
            self.read(store, tail, &mut old)?;
            old[..8].copy_from_slice(&page.to_le_bytes());
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
        self.set_bit(
            store,
            ref_class(item),
            Bitmap::Pending,
            ref_index(item),
            true,
        )?;
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

    /// Takes the oldest entry off the log; the hole it names stays a hole
    /// (it moves to its class's bitmap).
    fn log_pop(&mut self, store: &mut dyn TileStore) -> Result<()> {
        self.state.logged -= 1;
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

    /// Moves logged holes whose freeing generation is in place to their
    /// classes' bitmaps, shrinks classes whose last items are holes, and
    /// when reusable holes pass the allowance, moves classes' last items
    /// into their holes (at least the tuning's rate per item freed).
    fn fill(&mut self, store: &mut dyn TileStore, freed: u64) -> Result<()> {
        if self.ring.published() == self.ring.checkpointed() {
            self.holes_waiting = 0;
        }
        // Freed items wait for a checkpoint before they are reusable: once
        // more wait than the ratio allows (at least 1 MiB, at most half a
        // budget), run it.
        let patience = self.allowance().max(1 << 20).min(self.fixed.budget / 2);
        if self.holes_waiting > patience {
            self.put_in_place(store)?;
        }
        while let Some((item, freed_by)) = self.log_peek(store)? {
            if freed_by > self.ring.checkpointed() {
                break;
            }
            self.log_pop(store)?;
            let class = ref_class(item);
            if ref_index(item) >= self.classes[class].end {
                return Err(corrupt("keyed hole past its class"));
            }
            self.set_bit(store, class, Bitmap::Pending, ref_index(item), false)?;
            self.set_bit(store, class, Bitmap::Holes, ref_index(item), true)?;
            self.classes[class].holes += 1;
            self.shrink_holes(store, class)?;
        }
        let mut work = freed * self.tuning.fills_per_free + 1;
        while work > 0 && self.reusable_hole_bytes() > self.allowance() {
            if !self.compact_one(store)? {
                break;
            }
            work -= 1;
        }
        Ok(())
    }

    fn reusable_hole_bytes(&self) -> u64 {
        self.classes[..CLASSES]
            .iter()
            .enumerate()
            .map(|(class, entry)| entry.holes * class_size(class))
            .sum()
    }

    /// Shrinks `class` while its last item is a bitmap hole.
    fn shrink_holes(&mut self, store: &mut dyn TileStore, class: usize) -> Result<()> {
        while self.classes[class].end > 0 {
            let last = self.classes[class].end - 1;
            if !self.bit(store, class, Bitmap::Holes, last)? {
                break;
            }
            self.set_bit(store, class, Bitmap::Holes, last, false)?;
            self.classes[class].holes -= 1;
            self.state.hole_bytes -= class_size(class);
            self.shrink(store, class)?;
        }
        Ok(())
    }

    /// Moves the last item of a class with reusable holes into its lowest
    /// hole; `false` when no class can do so now.
    fn compact_one(&mut self, store: &mut dyn TileStore) -> Result<bool> {
        for _ in 0..CLASSES {
            let class = self.compact_class;
            self.compact_class = (class + 1) % CLASSES;
            if self.classes[class].holes == 0 {
                continue;
            }
            let index = self.classes[class].end - 1;
            let last = item_ref(class, index);
            // Holes at the end were shrunk; a freed item still logged is
            // not moved: it becomes a hole when its turn comes.
            if self.bit(store, class, Bitmap::Pending, index)? {
                continue;
            }
            let referrer = self
                .referrer(store, last)?
                .ok_or(corrupt("keyed item without a referrer"))?;
            let target = self.next_hole(store, class)?;
            self.set_bit(store, class, Bitmap::Holes, target, false)?;
            self.classes[class].holes -= 1;
            let hole = item_ref(class, target);
            self.move_item(store, last, hole, referrer)?;
            // The old place is a hole a published generation still uses.
            self.state.hole_bytes -= class_size(class);
            self.log_hole(store, last)?;
            self.stats.moves += 1;
            return Ok(true);
        }
        Ok(false)
    }

    fn map_root(&self, class: usize, map: Bitmap) -> (u64, u64) {
        match map {
            Bitmap::Holes => self.classes[class].bits,
            Bitmap::Pending => self.classes[class].pending,
        }
    }

    /// Where bit `index` of one of `class`'s bitmaps is: `(page, byte)`;
    /// page `0` when the bitmap has no page there.
    fn bit_at(
        &self,
        store: &dyn TileSource,
        class: usize,
        map: Bitmap,
        index: u64,
    ) -> Result<(u64, u64)> {
        let mut view = self.view();
        let root = self.map_root(class, map);
        let page = radix::get(&mut view, store, root, index / BITS_PER_PAGE)?;
        Ok((page, (index % BITS_PER_PAGE) / 64 * 8))
    }

    fn bit(&self, store: &dyn TileSource, class: usize, map: Bitmap, index: u64) -> Result<bool> {
        let (page, at) = self.bit_at(store, class, map, index)?;
        if page == 0 {
            return Ok(false);
        }
        let mut word = [0u8; 8];
        self.read(store, page + at, &mut word)?;
        Ok(u64::from_le_bytes(word) >> (index % 64) & 1 == 1)
    }

    fn set_bit(
        &mut self,
        store: &mut dyn TileStore,
        class: usize,
        map: Bitmap,
        index: u64,
        value: bool,
    ) -> Result<()> {
        let (mut page, at) = self.bit_at(store, class, map, index)?;
        if page == 0 {
            if !value {
                return Ok(());
            }
            page = self.take_page(store)?;
            // A page from the pool holds old bytes: clear it, in the units
            // its words are staged in.
            let zero = [0u8; 8];
            for word in 0..PAGE_BYTES / 8 {
                if !self
                    .ring
                    .write_unreferenced(store, page + word * 8, &zero)?
                {
                    self.ring.stage(store, page + word * 8, &zero)?;
                }
            }
            let root = self.map_root(class, map);
            let root = self.dir_set(store, root, index / BITS_PER_PAGE, page)?;
            match map {
                Bitmap::Holes => self.classes[class].bits = root,
                Bitmap::Pending => self.classes[class].pending = root,
            }
        }
        let mut word = [0u8; 8];
        self.read(store, page + at, &mut word)?;
        let mut bits = u64::from_le_bytes(word);
        if value {
            bits |= 1 << (index % 64);
        } else {
            bits &= !(1 << (index % 64));
        }
        // The bitmap is part of the published state: through the ring.
        self.ring.stage(store, page + at, &bits.to_le_bytes())
    }

    /// The first bitmap hole of `class` from its cursor on, wrapping once.
    fn next_hole(&mut self, store: &dyn TileSource, class: usize) -> Result<u64> {
        let end = self.classes[class].end;
        let start = self.cursors[class].min(end) / 64 * 64;
        let mut index = start;
        let mut wrapped = false;
        loop {
            if index >= end {
                if wrapped {
                    return Err(corrupt("keyed hole bitmap"));
                }
                wrapped = true;
                index = 0;
            }
            let (page, at) = self.bit_at(store, class, Bitmap::Holes, index)?;
            if page != 0 {
                let mut word = [0u8; 8];
                self.read(store, page + at, &mut word)?;
                let bits = u64::from_le_bytes(word) >> (index % 64);
                if bits != 0 {
                    let found = index + u64::from(bits.trailing_zeros());
                    if found < end {
                        self.cursors[class] = found;
                        return Ok(found);
                    }
                }
                index = (index / 64 + 1) * 64;
            } else {
                // A page without bits: skip to the next.
                index = (index / BITS_PER_PAGE + 1) * BITS_PER_PAGE;
            }
            if wrapped && index >= start {
                return Err(corrupt("keyed hole bitmap"));
            }
        }
    }

    /// What points at live item `item`: its slot (a head) or the item
    /// before it. Only for items the bitmaps show live.
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

    /// Moves live item `from` to hole `to` of the same class and points its
    /// referrer and the item after it there.
    fn move_item(
        &mut self,
        store: &mut dyn TileStore,
        from: u64,
        to: u64,
        referrer: Referrer,
    ) -> Result<()> {
        let header = self.item(store, from)?;
        // In the units a put writes: the header, then the bytes.
        self.write_item(store, to, 0, &header.encode())?;
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
            self.write_item(store, to, ITEM_HEADER + done, &step[..n])?;
            done += n as u64;
        }
        match referrer {
            Referrer::Slot(page, index, route) => {
                self.set_slot(store, page, index, Slot { route, item: to })?;
            }
            Referrer::Prev(prev) => {
                let mut before = self.item(store, prev)?;
                before.next = to;
                self.rewrite_header(store, prev, &before)?;
            }
        }
        if header.next != NO_ITEM {
            let mut after = self.item(store, header.next)?;
            after.total_or_prev = to;
            self.rewrite_header(store, header.next, &after)?;
        }
        Ok(())
    }

    // ---- consistency (tests) ----

    /// Checks that every index below each class's end is live or logged
    /// once, that live items add up to the state's, and that no hole is
    /// past its class.
    #[cfg(test)]
    pub(crate) fn check_consistency(
        &self,
        store: &dyn TileSource,
    ) -> std::result::Result<(), String> {
        let text = |error: Error| format!("{error}");
        let mut logged = std::collections::HashSet::new();
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
            if !logged.insert(item) {
                return Err(format!("hole {item:x} logged twice"));
            }
            at += 1;
        }
        if logged
            .iter()
            .any(|&item| ref_index(item) >= self.classes[ref_class(item)].end)
        {
            return Err("a hole past its class".into());
        }
        let (mut live, mut hole_bytes) = (0u64, 0u64);
        for class in 0..CLASSES {
            let mut holes = 0u64;
            for index in 0..self.classes[class].end {
                let item = item_ref(class, index);
                let hole = self.bit(store, class, Bitmap::Holes, index).map_err(text)?;
                let pending = self
                    .bit(store, class, Bitmap::Pending, index)
                    .map_err(text)?;
                if pending != logged.contains(&item) {
                    return Err(format!(
                        "item {item:x}: pending {pending}, logged {}",
                        !pending
                    ));
                }
                if hole && pending {
                    return Err(format!("item {item:x} both a hole and pending"));
                }
                holes += u64::from(hole);
                if hole || pending {
                    hole_bytes += class_size(class);
                } else if self.referrer(store, item).map_err(text)?.is_some() {
                    live += class_size(class);
                } else {
                    return Err(format!("live item {item:x} without a referrer"));
                }
            }
            if holes != self.classes[class].holes {
                return Err(format!(
                    "class {class}: {holes} holes, entry says {}",
                    self.classes[class].holes
                ));
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
                            || self
                                .bit(store, class, Bitmap::Holes, at_index)
                                .map_err(text)?
                            || self
                                .bit(store, class, Bitmap::Pending, at_index)
                                .map_err(text)?
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
        if reached != live {
            return Err(format!("records reach {reached} bytes, {live} live"));
        }
        if live != self.state.live || hole_bytes != self.state.hole_bytes {
            return Err(format!(
                "live {live}/{} holes {hole_bytes}/{}",
                self.state.live, self.state.hole_bytes
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

/// One of a class's two bitmaps.
#[derive(Clone, Copy, Debug)]
enum Bitmap {
    /// Items no generation since the last checkpoint freed: reusable.
    Holes,
    /// Items freed and logged, waiting to become holes.
    Pending,
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
