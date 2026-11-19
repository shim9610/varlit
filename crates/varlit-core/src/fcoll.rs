//! Internal collection facade.
//!
//! Internal maps, sets and queues that never cross the public API go through
//! these aliases, so the `unstable-jzap` feature can swap the `std`
//! containers for the explicitly fallible `jzap-std` ones without changing a
//! public signature. With the feature off, the aliases are the `std` types and
//! every helper compiles to the call it replaces.
//!
//! Inserts go through [`Put`], [`PutSet`] and [`PushBack`] so one call site
//! serves both backends: the `std` impls insert directly (callers reserve
//! first where they already did), the `jzap` impls report a refused
//! allocation as [`Error::AllocationFailed`] instead of aborting.
//!
//! Hash maps keep `std`'s `RandomState` in both builds. Both hash with
//! SipHash-1-3, so a comparison between the two builds measures the
//! containers, not the hash function, and construction stays infallible.

use crate::{Error, Result};
use std::hash::{BuildHasher, Hash, RandomState};

#[cfg(not(feature = "unstable-jzap"))]
mod imp {
    pub(crate) type HashMap<K, V> = std::collections::HashMap<K, V>;
    pub(crate) type BTreeMap<K, V> = std::collections::BTreeMap<K, V>;
    pub(crate) type BTreeSet<T> = std::collections::BTreeSet<T>;
    pub(crate) type VecDeque<T> = std::collections::VecDeque<T>;
}

#[cfg(feature = "unstable-jzap")]
mod imp {
    pub(crate) type HashMap<K, V> = jzap::collections::HashMap<K, V, std::hash::RandomState>;
    pub(crate) type BTreeMap<K, V> = jzap::collections::BTreeMap<K, V>;
    pub(crate) type BTreeSet<T> = jzap::collections::BTreeSet<T>;
    pub(crate) type VecDeque<T> = jzap::deque::VecDeque<T>;
}

pub(crate) use imp::{BTreeMap, BTreeSet, HashMap, VecDeque};

/// An empty hash map. Infallible in both builds: no allocation, and the
/// hasher is `std`'s.
pub(crate) fn hash_map<K, V>() -> HashMap<K, V> {
    HashMap::with_hasher(RandomState::new())
}

#[cfg(feature = "unstable-jzap")]
fn refused(resource: &'static str, error: jzap::Error) -> Error {
    let requested = match error {
        jzap::Error::Allocation { size, .. } => size as u64,
        jzap::Error::Budget { requested, .. } => requested as u64,
        _ => 0,
    };
    Error::AllocationFailed {
        resource,
        requested,
    }
}

/// Fallible map insert: returns the replaced value.
pub(crate) trait Put<K, V> {
    fn put(&mut self, key: K, value: V, resource: &'static str) -> Result<Option<V>>;
}

/// Fallible set insert: returns whether the value was new.
pub(crate) trait PutSet<T> {
    fn put(&mut self, value: T, resource: &'static str) -> Result<bool>;
}

/// Fallible queue append.
pub(crate) trait PushBack<T> {
    fn push(&mut self, value: T, resource: &'static str) -> Result<()>;
}

#[cfg(not(feature = "unstable-jzap"))]
impl<K: Eq + Hash, V, S: BuildHasher> Put<K, V> for std::collections::HashMap<K, V, S> {
    #[inline]
    fn put(&mut self, key: K, value: V, _: &'static str) -> Result<Option<V>> {
        Ok(self.insert(key, value))
    }
}

#[cfg(not(feature = "unstable-jzap"))]
impl<K: Ord, V> Put<K, V> for std::collections::BTreeMap<K, V> {
    #[inline]
    fn put(&mut self, key: K, value: V, _: &'static str) -> Result<Option<V>> {
        Ok(self.insert(key, value))
    }
}

#[cfg(not(feature = "unstable-jzap"))]
impl<T: Ord> PutSet<T> for std::collections::BTreeSet<T> {
    #[inline]
    fn put(&mut self, value: T, _: &'static str) -> Result<bool> {
        Ok(self.insert(value))
    }
}

#[cfg(not(feature = "unstable-jzap"))]
impl<T> PushBack<T> for std::collections::VecDeque<T> {
    #[inline]
    fn push(&mut self, value: T, _: &'static str) -> Result<()> {
        self.push_back(value);
        Ok(())
    }
}

#[cfg(feature = "unstable-jzap")]
impl<K: Eq + Hash, V, S: BuildHasher> Put<K, V> for jzap::collections::HashMap<K, V, S> {
    #[inline]
    fn put(&mut self, key: K, value: V, resource: &'static str) -> Result<Option<V>> {
        self.try_insert(key, value)
            .map_err(|rejected| refused(resource, rejected.error))
    }
}

#[cfg(feature = "unstable-jzap")]
impl<K: Ord, V> Put<K, V> for jzap::collections::BTreeMap<K, V> {
    #[inline]
    fn put(&mut self, key: K, value: V, resource: &'static str) -> Result<Option<V>> {
        self.try_insert(key, value)
            .map_err(|rejected| refused(resource, rejected.error))
    }
}

#[cfg(feature = "unstable-jzap")]
impl<T: Ord> PutSet<T> for jzap::collections::BTreeSet<T> {
    #[inline]
    fn put(&mut self, value: T, resource: &'static str) -> Result<bool> {
        self.try_insert(value)
            .map_err(|rejected| refused(resource, rejected.error))
    }
}

#[cfg(feature = "unstable-jzap")]
impl<T> PushBack<T> for jzap::deque::VecDeque<T> {
    #[inline]
    fn push(&mut self, value: T, resource: &'static str) -> Result<()> {
        self.try_push_back(value)
            .map_err(|rejected| refused(resource, rejected.error))
    }
}

/// `try_reserve` with the crate's error, for both backends.
pub(crate) trait Reserve {
    fn reserve_for(&mut self, additional: usize, resource: &'static str) -> Result<()>;
}

macro_rules! reserve_std {
    ($($ty:ty => [$($g:tt)*]),* $(,)?) => {$(
        impl<$($g)*> Reserve for $ty {
            #[inline]
            fn reserve_for(&mut self, additional: usize, resource: &'static str) -> Result<()> {
                self.try_reserve(additional).map_err(|_| Error::AllocationFailed {
                    resource,
                    requested: additional as u64,
                })
            }
        }
    )*};
}

reserve_std!(
    std::collections::HashMap<K, V, S> => [K: Eq + Hash, V, S: BuildHasher],
    std::collections::VecDeque<T> => [T],
);

#[cfg(feature = "unstable-jzap")]
impl<K: Eq + Hash, V, S: BuildHasher> Reserve for jzap::collections::HashMap<K, V, S> {
    #[inline]
    fn reserve_for(&mut self, additional: usize, resource: &'static str) -> Result<()> {
        self.try_reserve(additional)
            .map_err(|error| refused(resource, error))
    }
}

#[cfg(feature = "unstable-jzap")]
impl<T> Reserve for jzap::deque::VecDeque<T> {
    #[inline]
    fn reserve_for(&mut self, additional: usize, resource: &'static str) -> Result<()> {
        self.try_reserve(additional)
            .map_err(|error| refused(resource, error))
    }
}
