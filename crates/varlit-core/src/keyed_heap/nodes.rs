//! The writer's tree pages: 64 page buffers and a scratch page, allocated
//! once when the writer is opened (the 256 KiB the bucket page buffers took
//! before), through which every tree page is read and changed.
//!
//! A changed page is staged whole through the ring when it is written back
//! (when its buffer is needed for another page, or at publication), and
//! only when its bytes changed since they were read: a reader lays one
//! range over a page, whatever changed in it.

use super::lookup::View;
use super::node::NODE_BYTES;
use crate::matrix_tiles::ring_store::RingStore;
use crate::matrix_tiles::store::{TileSource, TileStore};
use crate::{Error, Result};

/// Page buffers.
pub(crate) const SLOTS: usize = 64;

#[derive(Clone, Copy, Debug, Default)]
struct Tag {
    /// The page's offset; `0` for an empty buffer.
    at: u64,
    dirty: bool,
    used: u64,
    /// The checksum of the page as the file and the ring show it; `None`
    /// for a new page, which is always written.
    shown: Option<u32>,
}

pub(crate) struct Nodes {
    bytes: Box<[u8]>,
    scratch: Box<[u8]>,
    tags: [Tag; SLOTS],
    clock: u64,
}

impl Nodes {
    pub(crate) fn new() -> Result<Self> {
        let buffer = |len: usize| -> Result<Box<[u8]>> {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(len)
                .map_err(|_| Error::AllocationFailed {
                    resource: "keyed tree page buffers",
                    requested: len as u64,
                })?;
            bytes.resize(len, 0);
            Ok(bytes.into_boxed_slice())
        };
        Ok(Self {
            bytes: buffer(SLOTS * NODE_BYTES)?,
            scratch: buffer(NODE_BYTES)?,
            tags: [Tag::default(); SLOTS],
            clock: 0,
        })
    }

    fn range(slot: usize) -> core::ops::Range<usize> {
        slot * NODE_BYTES..(slot + 1) * NODE_BYTES
    }

    fn slot_of(&self, at: u64) -> Option<usize> {
        self.tags.iter().position(|tag| tag.at == at && at != 0)
    }

    fn slot_containing(&self, offset: u64) -> Option<usize> {
        self.tags
            .iter()
            .position(|tag| tag.at != 0 && tag.at <= offset && offset - tag.at < NODE_BYTES as u64)
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
            let bytes = self.bytes[Self::range(slot)]
                .get(start..start + buf.len())
                .ok_or(Error::InvalidCanonicalEncoding(
                    "a read crosses a keyed tree page",
                ))?;
            buf.copy_from_slice(bytes);
            return Ok(());
        }
        ring.read(store, at, buf)
    }

    /// A buffer for the page at `at`, not one of `keep`: the one holding
    /// it, else an empty one or the least recently used (written back
    /// first).
    fn take(
        &mut self,
        ring: &mut RingStore,
        store: &mut dyn TileStore,
        at: u64,
        keep: &[usize],
    ) -> Result<(usize, bool)> {
        self.clock += 1;
        if let Some(slot) = self.slot_of(at) {
            self.tags[slot].used = self.clock;
            return Ok((slot, true));
        }
        let slot = (0..SLOTS)
            .filter(|slot| !keep.contains(slot))
            .min_by_key(|&slot| (self.tags[slot].at != 0, self.tags[slot].used))
            .ok_or(Error::InvalidCanonicalEncoding("keyed tree page buffers"))?;
        self.write_back(ring, store, slot)?;
        self.tags[slot] = Tag {
            at,
            dirty: false,
            used: self.clock,
            shown: None,
        };
        Ok((slot, false))
    }

    /// The buffer holding the page at `at`, read when not held, never one of
    /// `keep`.
    pub(crate) fn load(
        &mut self,
        ring: &mut RingStore,
        store: &mut dyn TileStore,
        at: u64,
        keep: &[usize],
    ) -> Result<usize> {
        let (slot, held) = self.take(ring, store, at, keep)?;
        if !held {
            if let Err(error) = ring.read(&*store, at, &mut self.bytes[Self::range(slot)]) {
                self.tags[slot] = Tag::default();
                return Err(error);
            }
            self.tags[slot].shown = Some(crc32fast::hash(&self.bytes[Self::range(slot)]));
        }
        Ok(slot)
    }

    /// A buffer for a new page, zeroed.
    pub(crate) fn create(
        &mut self,
        ring: &mut RingStore,
        store: &mut dyn TileStore,
        at: u64,
        keep: &[usize],
    ) -> Result<usize> {
        let (slot, _) = self.take(ring, store, at, keep)?;
        self.bytes[Self::range(slot)].fill(0);
        self.tags[slot].dirty = true;
        self.tags[slot].shown = None;
        Ok(slot)
    }

    pub(crate) fn page(&self, slot: usize) -> &[u8] {
        &self.bytes[Self::range(slot)]
    }

    pub(crate) fn page_mut(&mut self, slot: usize) -> &mut [u8] {
        self.tags[slot].dirty = true;
        &mut self.bytes[Self::range(slot)]
    }

    pub(crate) fn at(&self, slot: usize) -> u64 {
        self.tags[slot].at
    }

    /// A buffer to change, and the scratch page.
    pub(crate) fn with_scratch(&mut self, slot: usize) -> (&mut [u8], &mut [u8]) {
        self.tags[slot].dirty = true;
        (&mut self.bytes[Self::range(slot)], &mut self.scratch)
    }

    /// Two buffers to change, and the scratch page.
    pub(crate) fn pair_with_scratch(
        &mut self,
        a: usize,
        b: usize,
    ) -> (&mut [u8], &mut [u8], &mut [u8]) {
        assert!(a != b && a < SLOTS && b < SLOTS);
        self.tags[a].dirty = true;
        self.tags[b].dirty = true;
        let (low, high, swap) = if a < b { (a, b, false) } else { (b, a, true) };
        let (first, second) = self.bytes.split_at_mut(high * NODE_BYTES);
        let low_page = &mut first[low * NODE_BYTES..(low + 1) * NODE_BYTES];
        let high_page = &mut second[..NODE_BYTES];
        let scratch = &mut self.scratch[..];
        if swap {
            (high_page, low_page, scratch)
        } else {
            (low_page, high_page, scratch)
        }
    }

    /// Forgets the page at `at` without writing it back: it was freed.
    pub(crate) fn forget(&mut self, at: u64) {
        if let Some(slot) = self.slot_of(at) {
            self.tags[slot] = Tag::default();
        }
    }

    /// Stages a changed buffer whole, when its bytes differ from what the
    /// file and the ring show.
    fn write_back(
        &mut self,
        ring: &mut RingStore,
        store: &mut dyn TileStore,
        slot: usize,
    ) -> Result<()> {
        let tag = self.tags[slot];
        if tag.at == 0 || !tag.dirty {
            return Ok(());
        }
        let page = &self.bytes[Self::range(slot)];
        let now = crc32fast::hash(page);
        if tag.shown != Some(now) {
            ring.stage(store, tag.at, page)?;
        }
        self.tags[slot].dirty = false;
        self.tags[slot].shown = Some(now);
        Ok(())
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.tags.iter().any(|tag| tag.dirty)
    }

    /// Writes back every changed buffer.
    pub(crate) fn flush(&mut self, ring: &mut RingStore, store: &mut dyn TileStore) -> Result<()> {
        for slot in 0..SLOTS {
            self.write_back(ring, store, slot)?;
        }
        Ok(())
    }
}

/// The writer's view of the file for lookups: its pages over its ring.
pub(crate) struct HeapView<'a> {
    pub(crate) ring: &'a RingStore,
    pub(crate) nodes: &'a Nodes,
}

impl View for HeapView<'_> {
    fn read(&mut self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        self.nodes.read(self.ring, source, at, buf)
    }
}
