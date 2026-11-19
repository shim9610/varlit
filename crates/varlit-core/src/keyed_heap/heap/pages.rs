//! Record bytes and pages of [`KeyedHeap`]: reading a record or a
//! separator, writing and freeing overflow chains, and taking pages from the
//! free list or carving them from blocks.

use super::*;

impl KeyedHeap {
    // ---- record bytes ----

    /// Reads bytes `[at, at + buf.len())` of a record or a separator.
    pub(super) fn rec_read(
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
    pub(super) fn write_chain(
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
            self.round_bytes += PAGE_BYTES;
            at += n;
            if next == NO_PAGE {
                return Ok(head);
            }
            page_at = next;
            in_place = next_in_place;
        }
    }

    /// Frees every page of the overflow chain from `head`.
    pub(super) fn free_chain(&mut self, store: &mut dyn TileStore, head: u64) -> Result<()> {
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
    pub(super) fn alloc_page(&mut self, store: &mut dyn TileStore, use_: Use) -> Result<u64> {
        Ok(self.alloc_page_placed(store, use_)?.0)
    }

    /// [`Self::alloc_page`], and whether the page's bytes past its link may
    /// be written in place: it was carved, or freed by a generation already
    /// in place (so nothing the ring holds covers it but links).
    pub(super) fn alloc_page_placed(
        &mut self,
        store: &mut dyn TileStore,
        use_: Use,
    ) -> Result<(u64, bool)> {
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

    pub(super) fn take_free(&mut self, store: &mut dyn TileStore) -> Result<Option<(u64, u64)>> {
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
    pub(super) fn carve(&mut self, store: &mut dyn TileStore) -> Result<u64> {
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
    pub(super) fn free_page(
        &mut self,
        store: &mut dyn TileStore,
        page: u64,
        use_: Use,
    ) -> Result<()> {
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
        self.round_bytes += 32;
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
    pub(super) fn new_node(
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

    pub(super) fn free_node(
        &mut self,
        store: &mut dyn TileStore,
        at: u64,
        kind: node::Kind,
    ) -> Result<()> {
        let use_ = match kind {
            node::Kind::Leaf => Use::Leaf,
            node::Kind::Interior => Use::Interior,
        };
        self.free_page(store, at, use_)
    }
}
