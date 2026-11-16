//! The writer's bucket pages: a fixed set of page buffers, allocated when the
//! writer is opened, through which every bucket page is read and changed.
//!
//! A changed page is staged whole, once, when the writer publishes (or when
//! its buffer is needed for another page), never slot by slot: a reader then
//! lays at most one ring range over a bucket page it reads, instead of one
//! per slot changed since the last checkpoint.

use super::BUCKET_PAGE;
use super::lookup::View;
use crate::matrix_tiles::ring_store::RingStore;
use crate::matrix_tiles::store::{TileSource, TileStore};
use crate::{Error, Result};

/// Bucket pages a writer keeps.
pub(crate) const PAGES: usize = 64;
const PAGE: usize = crate::width::small(BUCKET_PAGE);

#[derive(Clone, Copy, Debug, Default)]
struct Tag {
    /// The page's offset; `0` for an empty buffer.
    at: u64,
    dirty: bool,
    used: u64,
}

pub(crate) struct Pages {
    bytes: Box<[u8]>,
    tags: [Tag; PAGES],
    clock: u64,
}

impl Pages {
    pub(crate) fn new() -> Result<Self> {
        let len = PAGES * PAGE;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| Error::AllocationFailed {
                resource: "keyed bucket page buffers",
                requested: len as u64,
            })?;
        bytes.resize(len, 0);
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
            tags: [Tag::default(); PAGES],
            clock: 0,
        })
    }

    /// The buffer holding the page at `at`, if any.
    fn slot_of(&self, at: u64) -> Option<usize> {
        self.tags.iter().position(|tag| tag.at == at && at != 0)
    }

    /// The buffer holding the page that contains `offset`, if any.
    fn slot_containing(&self, offset: u64) -> Option<usize> {
        self.tags
            .iter()
            .position(|tag| tag.at != 0 && tag.at <= offset && offset - tag.at < BUCKET_PAGE)
    }

    fn page(&self, slot: usize) -> &[u8] {
        &self.bytes[slot * PAGE..(slot + 1) * PAGE]
    }

    /// Reads as the writer sees the file: buffered pages over `ring`.
    pub(crate) fn read(
        &self,
        ring: &RingStore,
        store: &dyn TileSource,
        at: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        if let Some(slot) = self.slot_containing(at) {
            let start = crate::width::index(at - self.tags[slot].at)?;
            let page = self.page(slot);
            let bytes =
                page.get(start..start + buf.len())
                    .ok_or(Error::InvalidCanonicalEncoding(
                        "a read crosses a keyed bucket page",
                    ))?;
            buf.copy_from_slice(bytes);
            return Ok(());
        }
        ring.read(store, at, buf)
    }

    /// A buffer for the page at `at`: the one holding it, else an empty one
    /// or the least recently used (staged first when changed).
    fn take(
        &mut self,
        ring: &mut RingStore,
        store: &mut dyn TileStore,
        at: u64,
    ) -> Result<(usize, bool)> {
        self.clock += 1;
        if let Some(slot) = self.slot_of(at) {
            self.tags[slot].used = self.clock;
            return Ok((slot, true));
        }
        let slot = (0..PAGES)
            .min_by_key(|&slot| (self.tags[slot].at != 0, self.tags[slot].used))
            .expect("pages");
        let tag = self.tags[slot];
        if tag.dirty {
            ring.stage(store, tag.at, &self.bytes[slot * PAGE..(slot + 1) * PAGE])?;
        }
        self.tags[slot] = Tag {
            at,
            dirty: false,
            used: self.clock,
        };
        Ok((slot, false))
    }

    /// The page at `at`, to change: staged whole when the writer publishes.
    pub(crate) fn page_mut(
        &mut self,
        ring: &mut RingStore,
        store: &mut dyn TileStore,
        at: u64,
    ) -> Result<&mut [u8]> {
        let (slot, held) = self.take(ring, store, at)?;
        let range = slot * PAGE..(slot + 1) * PAGE;
        if !held {
            // A failed read leaves nothing buffered.
            if let Err(error) = ring.read(&*store, at, &mut self.bytes[range.clone()]) {
                self.tags[slot] = Tag::default();
                return Err(error);
            }
        }
        self.tags[slot].dirty = true;
        Ok(&mut self.bytes[range])
    }

    /// The page at `at`, zeroed, to fill: its old bytes are not read.
    pub(crate) fn page_new(
        &mut self,
        ring: &mut RingStore,
        store: &mut dyn TileStore,
        at: u64,
    ) -> Result<&mut [u8]> {
        let (slot, _) = self.take(ring, store, at)?;
        let range = slot * PAGE..(slot + 1) * PAGE;
        self.bytes[range.clone()].fill(0);
        self.tags[slot].dirty = true;
        Ok(&mut self.bytes[range])
    }

    /// Whether a page was changed since it was last staged.
    pub(crate) fn is_dirty(&self) -> bool {
        self.tags.iter().any(|tag| tag.dirty)
    }

    /// Stages every changed page whole.
    pub(crate) fn flush(&mut self, ring: &mut RingStore, store: &mut dyn TileStore) -> Result<()> {
        for slot in 0..PAGES {
            if self.tags[slot].dirty {
                ring.stage(
                    store,
                    self.tags[slot].at,
                    &self.bytes[slot * PAGE..(slot + 1) * PAGE],
                )?;
                self.tags[slot].dirty = false;
            }
        }
        Ok(())
    }
}

/// The writer's view of the file for lookups: its pages over its ring.
pub(crate) struct HeapView<'a> {
    pub(crate) ring: &'a RingStore,
    pub(crate) pages: &'a Pages,
}

impl View for HeapView<'_> {
    fn read(&mut self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        self.pages.read(self.ring, source, at, buf)
    }
}
