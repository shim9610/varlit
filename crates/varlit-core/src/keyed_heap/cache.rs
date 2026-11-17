//! A reader's tree pages, kept in memory taken once when the reader opens:
//! 512 pages (2 MiB), two-way set associative. A page read whole through
//! the reader's snapshot is kept; the next lookup that needs it takes it
//! from memory instead of the file.
//!
//! A snapshot's bytes never change while it is valid, so a kept page stays
//! right until the reader follows to another generation (the cache is then
//! emptied) or a lookup finds the ring moved under it (its pages may be
//! wrong, and the cache is emptied too).

use super::lookup::View;
use super::node::NODE_BYTES;
use crate::matrix_tiles::store::TileSource;
use crate::{Error, Result};

const SETS: usize = 256;
const WAYS: usize = 2;

#[derive(Clone, Copy, Default)]
struct Way {
    /// The page's offset; `0` for none.
    at: u64,
    used: u64,
}

pub(crate) struct PageCache {
    pages: Box<[u8]>,
    ways: Box<[Way]>,
    clock: u64,
}

impl PageCache {
    pub(crate) fn new() -> Result<Self> {
        let len = SETS * WAYS * NODE_BYTES;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(len)
            .map_err(|_| Error::AllocationFailed {
                resource: "keyed reader page cache",
                requested: len as u64,
            })?;
        pages.resize(len, 0);
        let mut ways = Vec::new();
        ways.try_reserve_exact(SETS * WAYS)
            .map_err(|_| Error::AllocationFailed {
                resource: "keyed reader page cache",
                requested: (SETS * WAYS * core::mem::size_of::<Way>()) as u64,
            })?;
        ways.resize(SETS * WAYS, Way::default());
        Ok(Self {
            pages: pages.into_boxed_slice(),
            ways: ways.into_boxed_slice(),
            clock: 0,
        })
    }

    pub(crate) fn clear(&mut self) {
        self.ways.fill(Way::default());
    }

    fn set(at: u64) -> usize {
        let page = at / NODE_BYTES as u64;
        (page.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as usize % SETS
    }

    fn get(&mut self, at: u64) -> Option<&[u8]> {
        let set = Self::set(at);
        self.clock += 1;
        let index = (set * WAYS..set * WAYS + WAYS).find(|&i| self.ways[i].at == at)?;
        self.ways[index].used = self.clock;
        Some(&self.pages[index * NODE_BYTES..(index + 1) * NODE_BYTES])
    }

    fn put(&mut self, at: u64, page: &[u8]) {
        let set = Self::set(at);
        self.clock += 1;
        let index = (set * WAYS..set * WAYS + WAYS)
            .min_by_key(|&i| (self.ways[i].at != 0, self.ways[i].used))
            .unwrap_or(set * WAYS);
        self.ways[index] = Way {
            at,
            used: self.clock,
        };
        self.pages[index * NODE_BYTES..(index + 1) * NODE_BYTES].copy_from_slice(page);
    }
}

/// A view whose whole-page reads go through `cache`.
pub(crate) struct Cached<'a> {
    pub(crate) inner: &'a mut dyn View,
    pub(crate) cache: &'a mut PageCache,
    /// Whether any read went to `inner`.
    pub(crate) missed: bool,
}

impl View for Cached<'_> {
    fn read(&mut self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        if buf.len() != NODE_BYTES || at == 0 {
            self.missed = true;
            return self.inner.read(source, at, buf);
        }
        if let Some(page) = self.cache.get(at) {
            buf.copy_from_slice(page);
            return Ok(());
        }
        self.missed = true;
        self.inner.read(source, at, buf)?;
        self.cache.put(at, buf);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A set keeps its two most recently used pages, each with its own
    /// bytes; clearing forgets them all.
    #[test]
    fn a_set_keeps_its_two_most_recent_pages() {
        let mut cache = PageCache::new().unwrap();
        let page = |fill: u8| [fill; NODE_BYTES];
        // Three pages of one set.
        let base = NODE_BYTES as u64;
        let same: Vec<u64> = (1..10_000u64)
            .map(|n| n * base)
            .filter(|&at| PageCache::set(at) == PageCache::set(base))
            .take(3)
            .collect();
        cache.put(same[0], &page(1));
        cache.put(same[1], &page(2));
        assert_eq!(cache.get(same[0]).unwrap()[0], 1);
        cache.put(same[2], &page(3));
        // The least recently used, `same[1]`, made room.
        assert!(cache.get(same[1]).is_none());
        assert_eq!(cache.get(same[0]).unwrap()[0], 1);
        assert_eq!(cache.get(same[2]).unwrap()[0], 3);
        cache.clear();
        assert!(cache.get(same[0]).is_none());
    }
}
