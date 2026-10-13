//! Owner-local copy-on-write mirror of a persisted bitmap page index.
//! A staged root is installed only after the corresponding file mutation. The
//! small immutable RAM base and bounded edit overlay avoid cloning the full
//! directory for each staged edit. Only budget overflow creates a scratch
//! tree; its page cache and edit batches remain bounded and owner-local.
//! Scratch is never published or synced. Sharing immutable RAM nodes between
//! a handle's staged roots does not share mutable reader caches.
use crate::{Error, Result, disk_index::ScratchIndex};
use std::{cell::Cell, collections::BTreeMap, sync::Arc};

type Key = (u8, u64);
type Entry = (Key, Option<u64>);
// Conservative structural allowance including the bounded edit overlay.
const ENTRY_BYTES: usize = 256;
const EDITS: usize = 64;

#[derive(Clone, Debug, Default)]
pub(super) struct BitmapDirectory {
    tree: Option<ScratchIndex>,
    memory: Arc<BTreeMap<Key, Option<u64>>>,
    pending: Vec<Entry>,
    memory_count: usize,
    limit: usize,
    count: u64,
    distinct: u64,
    // Repeated cells on the same bitmap page do not perform scratch I/O.
    recent: Cell<Option<(u64, Option<u64>)>>,
}

fn key(kind: u8, n: u64) -> Vec<u8> {
    let mut key = vec![kind];
    key.extend_from_slice(&n.to_be_bytes());
    key
}
fn value(n: Option<u64>) -> [u8; 52] {
    let mut v = [0; 52];
    if let Some(n) = n {
        v[0] = 1;
        v[1..9].copy_from_slice(&n.to_le_bytes());
    }
    v
}
fn decode(v: [u8; 52]) -> Option<u64> {
    (v[0] != 0).then(|| u64::from_le_bytes(v[1..9].try_into().unwrap()))
}
impl BitmapDirectory {
    pub(super) fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }
    pub(super) fn empty_like(&self) -> Self {
        Self::with_limit(self.limit)
    }
    /// Fold prior edits while this handle uniquely owns the RAM base, before
    /// borrowing an immutable root for the next stage. Normal writer staging
    /// then copies only a tiny overlay, never the whole growing directory.
    /// Existing snapshots keep their old base; disk edits stay batched.
    pub(super) fn prepare_stage(&mut self) -> Result<()> {
        if self.tree.is_none() && Arc::get_mut(&mut self.memory).is_some() {
            self.flush_pending()?;
        }
        Ok(())
    }
    pub(super) fn set_limit(&mut self, limit: usize) -> Result<()> {
        if limit == self.limit {
            return Ok(());
        }
        if self.tree.is_none() && self.memory_rows().saturating_mul(ENTRY_BYTES) > limit {
            self.spill(limit)?;
        }
        self.limit = limit;
        if let Some(tree) = &mut self.tree {
            tree.set_cache_bytes(limit / 4);
        }
        if self.pending.len() > self.edit_limit() {
            self.flush_pending()?;
        }
        Ok(())
    }
    fn edit_limit(&self) -> usize {
        (self.limit / ENTRY_BYTES).min(EDITS)
    }
    fn memory_rows(&self) -> usize {
        self.memory_count
    }
    #[cfg(test)]
    fn resident_bytes(&self) -> u64 {
        if self.tree.is_some() {
            (self.limit / 4 + self.pending.capacity() * std::mem::size_of::<Entry>()) as u64
        } else {
            (self.memory.len() * 128 + self.pending.capacity() * std::mem::size_of::<Entry>())
                as u64
        }
    }
    fn spill(&mut self, limit: usize) -> Result<()> {
        let mut values = BTreeMap::new();
        for (&(kind, n), &v) in self.memory.iter() {
            values.insert(key(kind, n), value(v));
        }
        for &((kind, n), v) in &self.pending {
            values.insert(key(kind, n), value(v));
        }
        let mut tree = ScratchIndex::new(limit / 4)?;
        if !values.is_empty() {
            tree.apply(&values)?;
        }
        self.tree = Some(tree);
        self.memory = Arc::default();
        self.memory_count = 0;
        self.pending.clear();
        Ok(())
    }
    fn flush_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if let Some(tree) = &mut self.tree {
            let values = self
                .pending
                .iter()
                .map(|&((kind, n), v)| (key(kind, n), value(v)))
                .collect();
            tree.apply(&values)?;
        } else {
            let memory = Arc::make_mut(&mut self.memory);
            for &(key, v) in &self.pending {
                memory.insert(key, v);
            }
        }
        self.pending.clear();
        Ok(())
    }
    pub(super) fn len(&self) -> u64 {
        self.count
    }
    pub(super) fn distinct(&self) -> u64 {
        self.distinct
    }
    pub(super) fn slot(&self, at: u64) -> Result<Option<u64>> {
        if at >= self.count {
            return Ok(None);
        }
        self.get(0, at)
    }
    fn get(&self, kind: u8, n: u64) -> Result<Option<u64>> {
        if let Some((_, v)) = self.pending.iter().rev().find(|(key, _)| *key == (kind, n)) {
            return Ok(*v);
        }
        match &self.tree {
            Some(t) => Ok(t.get(&key(kind, n))?.and_then(decode)),
            None => Ok(self.memory.get(&(kind, n)).copied().flatten()),
        }
    }
    pub(super) fn first(&self, page: u64) -> Result<Option<u64>> {
        if let Some((p, at)) = self.recent.get()
            && p == page
        {
            return Ok(at);
        }
        let at = self.get(1, page)?;
        self.recent.set(Some((page, at)));
        Ok(at)
    }
    pub(super) fn contains(&self, page: u64) -> Result<bool> {
        Ok(self.first(page)?.is_some())
    }
    fn apply(&mut self, updates: impl IntoIterator<Item = Entry>) -> Result<()> {
        for (k, v) in updates {
            if self.get(k.0, k.1)? == v {
                continue;
            }
            let exists =
                self.memory.contains_key(&k) || self.pending.iter().any(|(key, _)| *key == k);
            if self.tree.is_none()
                && (self.memory_rows() + usize::from(!exists)).saturating_mul(ENTRY_BYTES)
                    > self.limit
            {
                self.spill(self.limit)?;
            }
            if let Some((_, old)) = self.pending.iter_mut().find(|(key, _)| *key == k) {
                *old = v;
            } else {
                if self.pending.len() == self.edit_limit() {
                    self.flush_pending()?;
                }
                self.pending
                    .try_reserve_exact(1)
                    .map_err(|e| Error::Io(std::io::Error::other(e)))?;
                self.pending.push((k, v));
            }
            if self.edit_limit() == 0 {
                self.flush_pending()?;
            }
            if self.tree.is_none() && !exists {
                self.memory_count += 1;
            }
        }
        self.recent.set(None);
        Ok(())
    }
    pub(super) fn push(&mut self, page: u64) -> Result<()> {
        let first = self.first(page)?;
        self.apply(
            std::iter::once(((0, self.count), Some(page)))
                .chain(first.is_none().then_some(((1, page), Some(self.count)))),
        )?;
        self.recent.set(Some((page, first.or(Some(self.count)))));
        self.count += 1;
        self.distinct += u64::from(first.is_none());
        Ok(())
    }
    pub(super) fn set_slot(&mut self, at: u64, page: u64) -> Result<()> {
        self.apply([((0, at), Some(page))])?;
        self.count = self.count.max(at + 1);
        Ok(())
    }
    pub(super) fn set_first(&mut self, page: u64, first: Option<u64>) -> Result<()> {
        let previous = self.first(page)?;
        self.apply([((1, page), first)])?;
        self.distinct = self.distinct - u64::from(previous.is_some()) + u64::from(first.is_some());
        Ok(())
    }
    pub(super) fn truncate(&mut self, count: u64) {
        self.count = self.count.min(count);
    }
    pub(super) fn clear(&mut self) {
        *self = self.empty_like();
    }
    pub(super) fn visit_pages(&self, mut f: impl FnMut(u64, u64) -> Result<()>) -> Result<()> {
        if let Some(t) = &self.tree {
            t.visit(|k, v| {
                if k.len() != 9 {
                    return Err(Error::InvalidMatrixLayout);
                }
                let id = u64::from_be_bytes(k[1..].try_into().unwrap());
                if self.pending.iter().any(|(key, _)| *key == (k[0], id)) {
                    return Ok(());
                }
                if k[0] == 1
                    && let Some(at) = decode(v)
                {
                    f(id, at)?;
                }
                Ok(())
            })?;
        } else {
            for (&(kind, n), &v) in self.memory.iter() {
                if kind == 1
                    && !self.pending.iter().any(|(key, _)| *key == (kind, n))
                    && let Some(at) = v
                {
                    f(n, at)?;
                }
            }
        }
        for &((kind, n), v) in &self.pending {
            if kind == 1
                && let Some(at) = v
            {
                f(n, at)?;
            }
        }
        Ok(())
    }
    pub(super) fn swapped(&self, page: u64, slot: u64, moved: u64, last: u64) -> Result<Self> {
        let mut next = self.clone();
        // On header failure with no swap the persisted array is unchanged.
        if slot != last {
            next.set_slot(slot, moved)?;
            next.set_first(page, None)?;
            if self.first(moved)? == Some(last) {
                next.set_first(moved, Some(slot))?;
            }
        }
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_writer_stages_share_the_base_and_only_copy_edits() -> Result<()> {
        let mut directory = BitmapDirectory::with_limit(1024 * 1024);
        for page in 0..1000 {
            directory.prepare_stage()?;
            let mut next = directory.clone();
            next.push(page)?;
            assert!(Arc::ptr_eq(&directory.memory, &next.memory));
            assert_eq!(directory.len(), page);
            assert_eq!(next.slot(page)?, Some(page));
            directory = next;
        }
        assert!(directory.tree.is_none());
        Ok(())
    }
    #[test]
    fn bounded_memory_spills_without_changing_held_snapshots() -> Result<()> {
        for limit in [0, 256, 4096, 1024 * 1024] {
            let mut original = BitmapDirectory::with_limit(limit);
            for page in 0..80 {
                original.push(page)?;
            }
            let mut next = original.clone();
            for page in 80..300 {
                next.push(page)?;
            }
            let mut moved = next.swapped(7, 7, 299, 299)?;
            moved.set_first(7, None)?;
            moved.truncate(299);
            assert_eq!(original.len(), 80);
            assert_eq!(next.len(), 300);
            assert_eq!(original.slot(7)?, Some(7));
            assert_eq!(next.first(7)?, Some(7));
            assert_eq!(moved.slot(7)?, Some(299));
            assert_eq!(moved.first(7)?, None);
            assert_eq!(moved.first(299)?, Some(7));
            let mut pages = BTreeMap::new();
            moved.visit_pages(|page, slot| {
                pages.insert(page, slot);
                Ok(())
            })?;
            assert_eq!(pages.len(), 299);
            for page in 0..299 {
                if page != 7 {
                    assert_eq!(pages[&page], page);
                }
            }
            assert_eq!(pages[&299], 7);
            assert!(next.resident_bytes() <= limit as u64 + 256);
            if limit == 1024 * 1024 {
                assert!(next.tree.is_none());
            } else {
                assert!(next.tree.is_some());
            }
        }
        Ok(())
    }
    #[test]
    fn reducing_a_directory_budget_spills_and_preserves_values() -> Result<()> {
        let mut dir = BitmapDirectory::with_limit(1024 * 1024);
        for i in 0..100 {
            dir.push(i)?;
        }
        let old = dir.clone();
        dir.set_limit(0)?;
        assert!(dir.tree.is_some());
        assert!(old.tree.is_none());
        for i in 0..100 {
            assert_eq!(dir.slot(i)?, old.slot(i)?);
        }
        Ok(())
    }
    #[test]
    fn staged_roots_preserve_old_slots_and_duplicate_first_positions() -> Result<()> {
        let mut original = BitmapDirectory::default();
        for page in [2, 1, 2] {
            original.push(page)?;
        }
        let mut staged = original.swapped(1, 1, 2, 2)?;
        staged.truncate(2);
        assert_eq!(original.slot(1)?, Some(1));
        assert_eq!(original.first(1)?, Some(1));
        assert_eq!(staged.slot(1)?, Some(2));
        assert_eq!(staged.first(2)?, Some(0));
        drop(staged);
        original.push(3)?;
        assert_eq!(original.slot(3)?, Some(3));
        assert_eq!(original.first(2)?, Some(0));
        assert_eq!(original.distinct(), 3);
        Ok(())
    }
}
