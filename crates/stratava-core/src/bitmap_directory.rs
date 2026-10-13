//! Owner-local copy-on-write mirror of a persisted bitmap page index.
//! A staged root is installed only after the corresponding file mutation. The
//! unnamed scratch file is never published or synced and has no shared cache.
use crate::{Error, Result, disk_index::ScratchIndex};
use std::{cell::Cell, collections::BTreeMap};

#[derive(Clone, Debug, Default)]
pub(super) struct BitmapDirectory {
    tree: Option<ScratchIndex>,
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
        match &self.tree {
            Some(t) => Ok(t.get(&key(kind, n))?.and_then(decode)),
            None => Ok(None),
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
    fn apply(&mut self, updates: BTreeMap<Vec<u8>, [u8; 52]>) -> Result<()> {
        if self.tree.is_none() {
            self.tree = Some(ScratchIndex::new(0)?);
        }
        self.tree.as_mut().unwrap().apply(&updates)?;
        self.recent.set(None);
        Ok(())
    }
    pub(super) fn push(&mut self, page: u64) -> Result<()> {
        let first = self.first(page)?;
        let mut updates = BTreeMap::new();
        updates.insert(key(0, self.count), value(Some(page)));
        if first.is_none() {
            updates.insert(key(1, page), value(Some(self.count)));
        }
        self.apply(updates)?;
        self.recent.set(Some((page, first.or(Some(self.count)))));
        self.count += 1;
        self.distinct += u64::from(first.is_none());
        Ok(())
    }
    pub(super) fn set_slot(&mut self, at: u64, page: u64) -> Result<()> {
        self.apply(BTreeMap::from([(key(0, at), value(Some(page)))]))?;
        self.count = self.count.max(at + 1);
        Ok(())
    }
    pub(super) fn set_first(&mut self, page: u64, first: Option<u64>) -> Result<()> {
        let previous = self.first(page)?;
        self.apply(BTreeMap::from([(key(1, page), value(first))]))?;
        self.distinct = self.distinct - u64::from(previous.is_some()) + u64::from(first.is_some());
        Ok(())
    }
    pub(super) fn truncate(&mut self, count: u64) {
        self.count = self.count.min(count);
    }
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
    pub(super) fn visit_pages(&self, mut f: impl FnMut(u64, u64) -> Result<()>) -> Result<()> {
        if let Some(t) = &self.tree {
            t.visit(|k, v| {
                if k.len() != 9 {
                    return Err(Error::InvalidMatrixLayout);
                }
                if k[0] == 1
                    && let Some(at) = decode(v)
                {
                    f(u64::from_be_bytes(k[1..].try_into().unwrap()), at)?;
                }
                Ok(())
            })?;
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
