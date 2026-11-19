//! The shape of [`KeyedHeap`]'s trees: setting leaf cells, splitting full
//! pages and choosing separators, removing emptied pages and merging
//! under-full leaves.

use super::*;

impl KeyedHeap {
    // ---- the tree ----

    /// Sets the leaf cell at `path` (a new or a replaced record).
    pub(super) fn leaf_set(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        cell: &[u8],
        replace: bool,
    ) -> Result<()> {
        let index = path.index[0];
        let slot = self.nodes.load(&mut self.ring, store, path.pages[0], &[])?;
        let limit = self.tuning.node_bytes;
        let fitted = {
            let (page, scratch) = self.nodes.with_scratch(slot);
            let old = if replace {
                node::entry(page, index)?.1
            } else {
                0
            };
            let cost = if replace {
                cell.len()
            } else {
                node::cost(cell.len())
            };
            if used(page) - old + cost > limit {
                false
            } else if replace {
                node::replace(page, index, cell, scratch)?
            } else {
                node::insert(page, index, cell, scratch)?
            }
        };
        if fitted {
            return Ok(());
        }
        if replace {
            node::remove(self.nodes.page_mut(slot), index)?;
        }
        self.split_leaf(store, path, index, cell)
    }

    /// Where cell `index` of the buffered page `slot` keeps its key.
    pub(super) fn key_loc(&self, slot: usize, index: usize, interior: bool) -> Result<KeyLoc> {
        Ok(tree::held_body(self.nodes.page(slot), self.nodes.at(slot), index, interior)?.key)
    }

    /// Splits the full leaf of `path`, then inserts `cell` at `index`.
    pub(super) fn split_leaf(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        index: usize,
        cell: &[u8],
    ) -> Result<()> {
        let left_slot = self.nodes.load(&mut self.ring, store, path.pages[0], &[])?;
        let (right, right_slot) = self.new_node(store, node::Kind::Leaf, &[left_slot])?;
        let n = node::count(self.nodes.page(left_slot));
        // A key past every key of the leaf starts the new page alone: keys
        // put in order fill their pages.
        let m = if index >= n {
            n
        } else {
            split_point(self.nodes.page(left_slot))
        };
        {
            let (left, right_page, scratch) = self.nodes.pair_with_scratch(left_slot, right_slot);
            for moved in m..n {
                let (offset, len) = node::entry(left, moved)?;
                // Half a page fits a page; a cell that does not is a damaged
                // page, never a cell to drop.
                if !node::insert(right_page, moved - m, &left[offset..offset + len], scratch)? {
                    return Err(corrupt("keyed leaf split"));
                }
            }
            for moved in (m..n).rev() {
                node::remove(left, moved)?;
            }
            let fitted = if m < n && index <= m {
                node::insert(left, index, cell, scratch)?
            } else {
                node::insert(right_page, index - m, cell, scratch)?
            };
            if !fitted {
                return Err(corrupt("keyed leaf split"));
            }
        }
        let left_count = node::count(self.nodes.page(left_slot));
        if left_count == 0 {
            // A full leaf keeps at least one cell on the left.
            return Err(corrupt("keyed leaf split"));
        }
        let low = self.key_loc(left_slot, left_count - 1, false)?;
        let high = self.key_loc(right_slot, 0, false)?;
        let sep = self.separator(store, &KeySrc::Loc(low), &KeySrc::Loc(high))?;
        self.stats.splits += 1;
        self.insert_entry(store, path, 1, path.index[1] + 1, right, &sep)
    }

    /// The separator (without its child) for two neighbouring keys: the
    /// shortest prefix of `high` greater than `low`, its bytes past
    /// [`tree::SEP_LOCAL`] in an overflow chain.
    pub(super) fn separator(
        &mut self,
        store: &mut dyn TileStore,
        low: &KeySrc<'_>,
        high: &KeySrc<'_>,
    ) -> Result<SepBody> {
        let common = {
            let mut view = self.view();
            let mut ctx = self.ctx(&mut view, &*store);
            tree::common_prefix(&mut ctx, low, high)?
        };
        let len = (common + 1).min(high.len());
        let local = tree::sep_local_len(len);
        let mut sep = SepBody::default();
        let mut at = 1;
        at += node::write_varint(&mut sep.bytes[at..], len);
        let local_bytes = crate::width::index(local)?;
        self.rec_read(
            &*store,
            &RecSrc::Key(*high),
            0,
            &mut sep.bytes[at..at + local_bytes],
        )?;
        at += local_bytes;
        if local < len {
            sep.bytes[0] = tree::FLAG_OVERFLOW;
            let head = self.write_chain(store, &RecSrc::Key(*high), local, len)?;
            sep.bytes[at..at + 8].copy_from_slice(&head.to_le_bytes());
            at += 8;
        }
        sep.len = at;
        Ok(sep)
    }

    /// Inserts the entry `child | sep` at `index` of the interior page at
    /// `level` of `path` (a new root past the top), splitting as needed.
    pub(super) fn insert_entry(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        level: usize,
        index: usize,
        child: u64,
        sep: &SepBody,
    ) -> Result<()> {
        let mut cell = [0u8; 8 + SEP_MAX];
        cell[..8].copy_from_slice(&child.to_le_bytes());
        cell[8..8 + sep.len].copy_from_slice(&sep.bytes[..sep.len]);
        let cell = &cell[..8 + sep.len];
        if level == path.height {
            if path.height == tree::MAX_HEIGHT {
                return Err(corrupt("keyed tree height"));
            }
            let (root, slot) = self.new_node(store, node::Kind::Interior, &[])?;
            let mut first = [0u8; 10];
            first[..8].copy_from_slice(&self.root().0.to_le_bytes());
            let (page, scratch) = self.nodes.with_scratch(slot);
            if !(node::insert(page, 0, &first, scratch)? && node::insert(page, 1, cell, scratch)?) {
                return Err(corrupt("keyed tree root"));
            }
            let height = self.root().1 + 1;
            self.set_root((root, height))?;
            return Ok(());
        }
        let slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[level], &[])?;
        let limit = self.tuning.node_bytes;
        let (page, scratch) = self.nodes.with_scratch(slot);
        if used(page) + node::cost(cell.len()) <= limit && node::insert(page, index, cell, scratch)?
        {
            return Ok(());
        }
        self.split_interior(store, path, level, index, cell)
    }

    /// Splits the full interior page at `level` of `path` and inserts `cell`
    /// at `index`: the separator of the first entry moved right goes up, and
    /// that entry keeps only its child.
    pub(super) fn split_interior(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        level: usize,
        index: usize,
        cell: &[u8],
    ) -> Result<()> {
        let left_slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[level], &[])?;
        let (right, right_slot) = self.new_node(store, node::Kind::Interior, &[left_slot])?;
        let n = node::count(self.nodes.page(left_slot));
        let m = split_point(self.nodes.page(left_slot)).max(1);
        let mut up = SepBody::default();
        {
            let (left, right_page, scratch) = self.nodes.pair_with_scratch(left_slot, right_slot);
            for moved in m..n {
                let (offset, len) = node::entry(left, moved)?;
                let entry = &left[offset..offset + len];
                let ok = if moved == m {
                    up.len = len - 8;
                    up.bytes[..up.len].copy_from_slice(&entry[8..]);
                    let mut first = [0u8; 10];
                    first[..8].copy_from_slice(&entry[..8]);
                    node::insert(right_page, 0, &first, scratch)?
                } else {
                    node::insert(right_page, moved - m, entry, scratch)?
                };
                if !ok {
                    return Err(corrupt("keyed interior split"));
                }
            }
            for moved in (m..n).rev() {
                node::remove(left, moved)?;
            }
            let fitted = if index <= m {
                node::insert(left, index, cell, scratch)?
            } else {
                node::insert(right_page, index - m, cell, scratch)?
            };
            if !fitted {
                return Err(corrupt("keyed interior split"));
            }
        }
        self.stats.splits += 1;
        self.insert_entry(
            store,
            path,
            level + 1,
            path.index[level + 1] + 1,
            right,
            &up,
        )
    }

    /// The interior cell `index` of the buffered page `slot`, parsed.
    pub(super) fn interior_cell(&self, slot: usize, index: usize) -> Result<Body> {
        tree::held_body(self.nodes.page(slot), self.nodes.at(slot), index, true)
    }

    /// The separator bytes (without the child) of interior cell `index` of
    /// the buffered page `slot`.
    pub(super) fn raw_separator(&self, slot: usize, index: usize) -> Result<SepBody> {
        let page = self.nodes.page(slot);
        let (offset, len) = node::entry(page, index)?;
        let mut bytes = [0u8; SEP_MAX];
        let sep = len.checked_sub(8).ok_or(corrupt("keyed tree cell"))?;
        bytes
            .get_mut(..sep)
            .ok_or(corrupt("keyed tree cell"))?
            .copy_from_slice(&page[offset + 8..offset + len]);
        Ok(SepBody { bytes, len: sep })
    }

    /// Removes the entry pointing at the page at `level` of `path` from its
    /// parent and frees the page, which holds nothing. A parent left empty
    /// goes too; a root left with one child is replaced by it.
    pub(super) fn remove_child(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        level: usize,
    ) -> Result<()> {
        let kind = if level == 0 {
            node::Kind::Leaf
        } else {
            node::Kind::Interior
        };
        self.free_node(store, path.pages[level], kind)?;
        let parent = level + 1;
        let index = path.index[parent];
        let slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[parent], &[])?;
        let old = self.interior_cell(slot, index)?;
        if old.overflowed() {
            self.free_chain(store, old.key.overflow)?;
        }
        let slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[parent], &[])?;
        if index == 0 && node::count(self.nodes.page(slot)) > 1 {
            // The next child becomes the first: its separator becomes the
            // lower bound of the whole page, which lives in the lowest
            // ancestor where the path does not take the first child. Keys
            // the removed child held went to the page before it (merges) or
            // are gone (deletes), so that bound may rise to it.
            let ancestor = (parent + 1..path.height).find(|&level| path.index[level] > 0);
            let next = self.interior_cell(slot, 1)?;
            let raw = self.raw_separator(slot, 1)?;
            let mut first = [0u8; 10];
            first[..8].copy_from_slice(&next.child.to_le_bytes());
            {
                let (page, scratch) = self.nodes.with_scratch(slot);
                node::replace(page, 1, &first, scratch)?;
                node::remove(page, index)?;
            }
            match ancestor {
                Some(level) => {
                    self.set_separator(store, path, level, path.index[level], &raw)?;
                }
                // The leftmost page of the tree: no lower bound.
                None if next.overflowed() => self.free_chain(store, next.key.overflow)?,
                None => {}
            }
        } else {
            node::remove(self.nodes.page_mut(slot), index)?;
        }
        let slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[parent], &[])?;
        let left = node::count(self.nodes.page(slot));
        if parent + 1 == path.height {
            if left == 1 && path.height > 1 {
                self.collapse_root(store)?;
            }
            return Ok(());
        }
        if left == 0 {
            return self.remove_child(store, path, parent);
        }
        Ok(())
    }

    /// Replaces a root with one child by that child, while there is one.
    pub(super) fn collapse_root(&mut self, store: &mut dyn TileStore) -> Result<()> {
        while self.root().1 > 1 {
            let (root, height) = self.root();
            let slot = self.nodes.load(&mut self.ring, store, root, &[])?;
            if node::count(self.nodes.page(slot)) != 1 {
                return Ok(());
            }
            let child = self.interior_cell(slot, 0)?.child;
            self.free_node(store, root, node::Kind::Interior)?;
            self.set_root((child, height - 1))?;
        }
        Ok(())
    }

    /// Replaces the separator of entry `index` of the interior page at
    /// `level` of `path` (its child stays), splitting as needed.
    pub(super) fn set_separator(
        &mut self,
        store: &mut dyn TileStore,
        path: &Path,
        level: usize,
        index: usize,
        sep: &SepBody,
    ) -> Result<()> {
        let slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[level], &[])?;
        let old = self.interior_cell(slot, index)?;
        if old.overflowed() {
            self.free_chain(store, old.key.overflow)?;
        }
        let slot = self
            .nodes
            .load(&mut self.ring, store, path.pages[level], &[])?;
        node::remove(self.nodes.page_mut(slot), index)?;
        self.insert_entry(store, path, level, index, old.child, sep)
    }

    /// Merges the leaf of `path`, under a quarter full, with a neighbour
    /// under the same parent when the two fit one page (the right one
    /// first, then the left one): the right page's cells move into the left
    /// page, and the right page leaves the tree.
    pub(super) fn merge_leaf(&mut self, store: &mut dyn TileStore, path: &Path) -> Result<()> {
        let index = path.index[1];
        let count = path.counts[1];
        let mut pairs = [None, None];
        if index + 1 < count {
            pairs[0] = Some((index, index + 1));
        }
        if index > 0 {
            pairs[1] = Some((index - 1, index));
        }
        for (left_index, right_index) in pairs.into_iter().flatten() {
            let parent = self.nodes.load(&mut self.ring, store, path.pages[1], &[])?;
            let left_at = self.interior_cell(parent, left_index)?.child;
            let right_at = self.interior_cell(parent, right_index)?.child;
            let left = self.nodes.load(&mut self.ring, store, left_at, &[])?;
            let right = self.nodes.load(&mut self.ring, store, right_at, &[left])?;
            if used(self.nodes.page(left)) + used(self.nodes.page(right)) > self.tuning.node_bytes {
                continue;
            }
            {
                let (left_page, right_page, scratch) = self.nodes.pair_with_scratch(left, right);
                for moved in 0..node::count(right_page) {
                    let (offset, len) = node::entry(right_page, moved)?;
                    let end = node::count(left_page);
                    if !node::insert(left_page, end, &right_page[offset..offset + len], scratch)? {
                        return Err(corrupt("keyed leaf merge"));
                    }
                }
                while node::count(right_page) > 0 {
                    node::remove(right_page, 0)?;
                }
            }
            let mut right_path = *path;
            right_path.pages[0] = right_at;
            right_path.index[1] = right_index;
            self.stats.merges += 1;
            return self.remove_child(store, &right_path, 0);
        }
        Ok(())
    }
}

/// A separator without its child: flags, length, the bytes its cell keeps
/// and an overflow page.
#[derive(Clone, Copy, Debug)]
pub(super) struct SepBody {
    bytes: [u8; SEP_MAX],
    len: usize,
}

impl Default for SepBody {
    fn default() -> Self {
        Self {
            bytes: [0; SEP_MAX],
            len: 0,
        }
    }
}

/// The entry to split a full page at: the first whose entries and cells
/// before it take half the page's used bytes, and at least one.
fn split_point(page: &[u8]) -> usize {
    let n = node::count(page);
    let used = used(page);
    let mut seen = 0usize;
    for index in 0..n {
        if seen * 2 >= used {
            return index.max(1);
        }
        let len = node::entry(page, index).map_or(0, |(_, len)| len);
        seen += node::cost(len);
    }
    n.saturating_sub(1).max(1)
}
