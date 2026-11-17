//! The writer's tree pages: three page buffers and a scratch page, allocated
//! once when the writer is opened (the same 256 KiB the bucket page buffers
//! took), through which every tree page is changed.
//!
//! A changed page is written back in 4 KiB segments, only those whose bytes
//! differ from what the file and the ring show: through the ring when a
//! generation from the mark on may read the page, in place when the page
//! was appended or taken from the pool in this generation (except its first
//! segment, whose pool link the published state may still follow). Every
//! segment is staged whole, so a reader lays one range per segment.

use super::lookup::View;
use super::node::NODE_BYTES;
use crate::matrix_tiles::ring_store::RingStore;
use crate::matrix_tiles::store::{TileSource, TileStore};
use crate::{Error, Result};

/// Page buffers; the scratch page comes after them.
pub(crate) const SLOTS: usize = 3;
/// The unit a page is written back in.
pub(crate) const SEGMENT: usize = 4096;
const SEGMENTS: usize = NODE_BYTES / SEGMENT;

/// How a buffered page may be written back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A generation a reader or a recovery may show uses the page.
    #[default]
    Staged,
    /// No such generation uses the page: written in place, but for its
    /// first segment when it came from the pool.
    Unused { pooled: bool },
}

#[derive(Clone, Copy, Debug, Default)]
struct Tag {
    /// The page's offset; `0` for an empty buffer.
    at: u64,
    dirty: bool,
    mode: Mode,
    used: u64,
}

pub(crate) struct Nodes {
    bytes: Box<[u8]>,
    tags: [Tag; SLOTS],
    clock: u64,
}

impl Nodes {
    pub(crate) fn new() -> Result<Self> {
        let len = (SLOTS + 1) * NODE_BYTES;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| Error::AllocationFailed {
                resource: "keyed tree page buffers",
                requested: len as u64,
            })?;
        bytes.resize(len, 0);
        Ok(Self {
            bytes: bytes.into_boxed_slice(),
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
        self.tags.iter().position(|tag| {
            tag.at != 0 && tag.at <= offset && offset - tag.at < NODE_BYTES as u64
        })
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
            mode: Mode::Staged,
            used: self.clock,
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
        if !held
            && let Err(error) = ring.read(&*store, at, &mut self.bytes[Self::range(slot)])
        {
            self.tags[slot] = Tag::default();
            return Err(error);
        }
        Ok(slot)
    }

    /// A buffer for a page no generation from the mark on uses, zeroed.
    pub(crate) fn create(
        &mut self,
        ring: &mut RingStore,
        store: &mut dyn TileStore,
        at: u64,
        pooled: bool,
        keep: &[usize],
    ) -> Result<usize> {
        let (slot, _) = self.take(ring, store, at, keep)?;
        self.bytes[Self::range(slot)].fill(0);
        self.tags[slot].dirty = true;
        self.tags[slot].mode = Mode::Unused { pooled };
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

    /// Two different buffers, both to change.
    pub(crate) fn pair_mut(&mut self, a: usize, b: usize) -> (&mut [u8], &mut [u8]) {
        assert!(a != b && a < SLOTS && b < SLOTS);
        self.tags[a].dirty = true;
        self.tags[b].dirty = true;
        let (low, high, swap) = if a < b { (a, b, false) } else { (b, a, true) };
        let (first, second) = self.bytes.split_at_mut(high * NODE_BYTES);
        let low_page = &mut first[low * NODE_BYTES..(low + 1) * NODE_BYTES];
        let high_page = &mut second[..NODE_BYTES];
        if swap { (high_page, low_page) } else { (low_page, high_page) }
    }

    /// A buffer to change, and the scratch page.
    pub(crate) fn with_scratch(&mut self, slot: usize) -> (&mut [u8], &mut [u8]) {
        self.tags[slot].dirty = true;
        let (pages, scratch) = self.bytes.split_at_mut(SLOTS * NODE_BYTES);
        (&mut pages[Self::range(slot)], &mut scratch[..NODE_BYTES])
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
        let (pages, scratch) = self.bytes.split_at_mut(SLOTS * NODE_BYTES);
        let (low, high, swap) = if a < b { (a, b, false) } else { (b, a, true) };
        let (first, second) = pages.split_at_mut(high * NODE_BYTES);
        let low_page = &mut first[low * NODE_BYTES..(low + 1) * NODE_BYTES];
        let high_page = &mut second[..NODE_BYTES];
        let scratch = &mut scratch[..NODE_BYTES];
        if swap {
            (high_page, low_page, scratch)
        } else {
            (low_page, high_page, scratch)
        }
    }

    /// The scratch page alone.
    pub(crate) fn scratch(&mut self) -> &mut [u8] {
        &mut self.bytes[SLOTS * NODE_BYTES..]
    }

    /// Forgets the page at `at` without writing it back: it was freed.
    pub(crate) fn forget(&mut self, at: u64) {
        if let Some(slot) = self.slot_of(at) {
            self.tags[slot] = Tag::default();
        }
    }

    /// Writes back the segments of a changed buffer whose bytes differ from
    /// what the file and the ring show.
    fn write_back(&mut self, ring: &mut RingStore, store: &mut dyn TileStore, slot: usize) -> Result<()> {
        let tag = self.tags[slot];
        if tag.at == 0 || !tag.dirty {
            return Ok(());
        }
        let (pages, scratch) = self.bytes.split_at_mut(SLOTS * NODE_BYTES);
        let page = &pages[Self::range(slot)];
        let shown = &mut scratch[..SEGMENT];
        for segment in 0..SEGMENTS {
            let bytes = &page[segment * SEGMENT..(segment + 1) * SEGMENT];
            let at = tag.at + (segment * SEGMENT) as u64;
            ring.read(&*store, at, shown)?;
            if shown == bytes {
                continue;
            }
            let in_place = match tag.mode {
                Mode::Staged => false,
                Mode::Unused { pooled } => !(pooled && segment == 0),
            };
            if !(in_place && ring.write_unreferenced(store, at, bytes)?) {
                ring.stage(store, at, bytes)?;
            }
        }
        self.tags[slot].dirty = false;
        Ok(())
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.tags.iter().any(|tag| tag.dirty)
    }

    /// Writes back every changed buffer; after a publication every page is
    /// one a generation uses.
    pub(crate) fn flush(&mut self, ring: &mut RingStore, store: &mut dyn TileStore) -> Result<()> {
        for slot in 0..SLOTS {
            self.write_back(ring, store, slot)?;
        }
        Ok(())
    }

    /// After a publication: every buffered page is now one a generation
    /// uses.
    pub(crate) fn published(&mut self) {
        for tag in &mut self.tags {
            tag.mode = Mode::Staged;
        }
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
