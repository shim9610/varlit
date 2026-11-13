//! Byte ranges keyed by where they go, with every allocation fallible.
//!
//! The engine holds two such maps (a writer's staged ranges, a snapshot's
//! ring overlay). An ordered map would serve, but a `BTreeMap` cannot refuse
//! an allocation: its node allocation aborts the process. This keeps the
//! ranges in a hash map (whose growth is reserved before an insert) and
//! finds the ones overlapping a read through a second hash map from
//! [`BUCKET`]-byte buckets to the ranges touching them.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::{BuildHasher, Hasher};

use crate::{Error, Result};

/// Bytes a bucket covers. A range is listed in every bucket it touches.
pub(crate) const BUCKET: u64 = 4096;

pub(crate) trait RangeLen {
    fn range_len(&self) -> u64;
}

impl RangeLen for Vec<u8> {
    fn range_len(&self) -> u64 {
        self.len() as u64
    }
}

/// Hashes the `u64` offsets these maps are keyed by: a multiply-xorshift
/// mix under two odd keys drawn per map from the standard library's random
/// state. Far cheaper than SipHash per key, and a file cannot be built to
/// collide its offsets without knowing the keys.
#[derive(Clone, Copy)]
pub(crate) struct OffsetHash {
    k0: u64,
    k1: u64,
}

impl OffsetHash {
    pub(crate) fn new() -> Self {
        let random = std::collections::hash_map::RandomState::new();
        Self {
            k0: random.hash_one(0x5157_u64) | 1,
            k1: random.hash_one(0x7a1e_u64) | 1,
        }
    }
}

impl BuildHasher for OffsetHash {
    type Hasher = OffsetHasher;
    fn build_hasher(&self) -> OffsetHasher {
        OffsetHasher {
            k0: self.k0,
            k1: self.k1,
            state: 0,
        }
    }
}

pub(crate) struct OffsetHasher {
    k0: u64,
    k1: u64,
    state: u64,
}

impl Hasher for OffsetHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }
    fn write_u64(&mut self, value: u64) {
        let mut x = (self.state ^ value).wrapping_mul(self.k0);
        x ^= x >> 32;
        self.state = x.wrapping_mul(self.k1);
    }
    fn finish(&self) -> u64 {
        self.state ^ (self.state >> 29)
    }
}

/// The targets touching one bucket: a few inline (the usual case, a
/// bucket holds a handful of slots), the rest in a vector allocated only
/// when they do not fit.
#[derive(Default)]
struct Bucket {
    inline: [u64; BUCKET_INLINE],
    len: u8,
    more: Vec<u64>,
}

const BUCKET_INLINE: usize = 4;

impl Bucket {
    /// Room for one more target, reserved so `add` cannot fail.
    fn reserve(&mut self) -> std::result::Result<(), std::collections::TryReserveError> {
        if usize::from(self.len) < BUCKET_INLINE {
            Ok(())
        } else {
            self.more.try_reserve(1)
        }
    }
    fn add(&mut self, target: u64) {
        if usize::from(self.len) < BUCKET_INLINE {
            self.inline[usize::from(self.len)] = target;
            self.len += 1;
        } else {
            self.more.push(target);
        }
    }
    fn iter(&self) -> impl Iterator<Item = &u64> {
        self.inline[..usize::from(self.len)]
            .iter()
            .chain(self.more.iter())
    }
    fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Keeps the targets `keep` accepts, refilling the inline part from the
    /// rest. Allocates nothing.
    fn retain(&mut self, mut keep: impl FnMut(u64) -> bool) {
        let mut kept = 0usize;
        for i in 0..usize::from(self.len) {
            let target = self.inline[i];
            if keep(target) {
                self.inline[kept] = target;
                kept += 1;
            }
        }
        self.more.retain(|target| keep(*target));
        while kept < BUCKET_INLINE && !self.more.is_empty() {
            self.inline[kept] = self.more.remove(0);
            kept += 1;
        }
        self.len = kept as u8;
    }
}

pub(crate) struct Ranges<V> {
    values: HashMap<u64, V, OffsetHash>,
    buckets: HashMap<u64, Bucket, OffsetHash>,
    resource: &'static str,
}

fn refused(resource: &'static str, requested: u64) -> Error {
    Error::AllocationFailed {
        resource,
        requested,
    }
}

impl<V: RangeLen> Ranges<V> {
    pub(crate) fn new(resource: &'static str) -> Self {
        let hash = OffsetHash::new();
        Self {
            values: HashMap::with_hasher(hash),
            buckets: HashMap::with_hasher(hash),
            resource,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub(crate) fn get(&self, target: u64) -> Option<&V> {
        self.values.get(&target)
    }

    /// Sets the range at `target`, replacing one already there (which must
    /// be the same unit, so the same length). Returns the replaced value.
    /// Everything it needs is reserved first: a refusal changes nothing.
    pub(crate) fn insert(&mut self, target: u64, value: V) -> Result<Option<V>> {
        let len = value.range_len().max(1);
        if let Some(old) = self.values.get_mut(&target) {
            return Ok(Some(std::mem::replace(old, value)));
        }
        let first = target / BUCKET;
        let last = (target + len - 1) / BUCKET;
        self.values
            .try_reserve(1)
            .map_err(|_| refused(self.resource, 32))?;
        self.buckets
            .try_reserve((last - first + 1) as usize)
            .map_err(|_| refused(self.resource, 32))?;
        for bucket in first..=last {
            let list = match self.buckets.entry(bucket) {
                Entry::Occupied(list) => list.into_mut(),
                Entry::Vacant(slot) => slot.insert(Bucket::default()),
            };
            list.reserve().map_err(|_| refused(self.resource, 8))?;
        }
        for bucket in first..=last {
            self.buckets
                .get_mut(&bucket)
                .expect("reserved above")
                .add(target);
        }
        self.values.insert(target, value);
        Ok(None)
    }

    /// Calls `visit` with each range overlapping `[offset, offset + len)`,
    /// possibly more than once for a range spanning several buckets.
    pub(crate) fn for_each_overlapping(
        &self,
        offset: u64,
        len: u64,
        visit: &mut dyn FnMut(u64, &V) -> Result<()>,
    ) -> Result<()> {
        if len == 0 || self.values.is_empty() {
            return Ok(());
        }
        let end = offset + len;
        for bucket in offset / BUCKET..=(end - 1) / BUCKET {
            let Some(list) = self.buckets.get(&bucket) else {
                continue;
            };
            for target in list.iter() {
                let Some(value) = self.values.get(target) else {
                    continue;
                };
                if *target < end && target + value.range_len() > offset {
                    visit(*target, value)?;
                }
            }
        }
        Ok(())
    }

    /// Keeps the ranges `keep` accepts. Allocates nothing.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(u64, &V) -> bool) {
        self.values.retain(|target, value| keep(*target, value));
        let values = &self.values;
        self.buckets.retain(|_, list| {
            list.retain(|target| values.contains_key(&target));
            !list.is_empty()
        });
    }

    pub(crate) fn clear(&mut self) {
        self.values.clear();
        self.buckets.clear();
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, &V)> {
        self.values.iter().map(|(target, value)| (*target, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_ranges_are_found_across_buckets_and_after_retain() {
        let mut ranges: Ranges<Vec<u8>> = Ranges::new("test ranges");
        ranges.insert(100, vec![1; 10]).unwrap();
        ranges.insert(4090, vec![2; 20]).unwrap();
        ranges.insert(20_000, vec![3; 9000]).unwrap();
        let found = |ranges: &Ranges<Vec<u8>>, offset, len| {
            let mut out = Vec::new();
            ranges
                .for_each_overlapping(offset, len, &mut |target, _| {
                    if !out.contains(&target) {
                        out.push(target);
                    }
                    Ok(())
                })
                .unwrap();
            out.sort_unstable();
            out
        };
        assert_eq!(found(&ranges, 0, 101), vec![100]);
        assert_eq!(found(&ranges, 110, 10), Vec::<u64>::new());
        assert_eq!(found(&ranges, 4095, 2), vec![4090]);
        assert_eq!(found(&ranges, 28_000, 1), vec![20_000]);
        assert_eq!(found(&ranges, 0, 50_000), vec![100, 4090, 20_000]);
        assert_eq!(ranges.insert(100, vec![9; 10]).unwrap(), Some(vec![1; 10]));
        ranges.retain(|target, _| target != 4090);
        assert_eq!(found(&ranges, 0, 50_000), vec![100, 20_000]);
    }

    #[test]
    fn a_bucket_past_its_inline_targets_keeps_them_all_through_retain() {
        let mut ranges: Ranges<Vec<u8>> = Ranges::new("test ranges");
        // Nine ranges in one bucket: four inline, five spilled.
        for i in 0..9u64 {
            ranges.insert(i * 100, vec![i as u8; 10]).unwrap();
        }
        let found = |ranges: &Ranges<Vec<u8>>| {
            let mut out = Vec::new();
            ranges
                .for_each_overlapping(0, 4096, &mut |target, _| {
                    out.push(target);
                    Ok(())
                })
                .unwrap();
            out.sort_unstable();
            out
        };
        assert_eq!(found(&ranges), (0..9).map(|i| i * 100).collect::<Vec<_>>());
        // Drop the inline ones and some spilled ones; the rest move inline.
        ranges.retain(|target, _| target >= 400 && target != 600);
        assert_eq!(found(&ranges), vec![400, 500, 700, 800]);
        ranges.insert(900, vec![9; 10]).unwrap();
        assert_eq!(found(&ranges), vec![400, 500, 700, 800, 900]);
        ranges.retain(|_, _| false);
        assert!(found(&ranges).is_empty());
        assert!(ranges.is_empty());
    }
}
