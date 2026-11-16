//! An ordered map of byte ranges with a capacity fixed when it is created.
//!
//! The engine's ring overlays live here. All their memory is taken once, when
//! a matrix is opened, sized by the ring: inserting, replacing, removing,
//! looking up and walking never allocate, so none of them can fail for lack
//! of memory. A full map refuses a new key with `false`; the caller decides
//! what to do instead (the writer streams the rest of a publication to disk).
//!
//! The map is a treap over an arena of nodes linked by index. A node's
//! priority is a keyed hash of its key, so the tree's shape does not depend on
//! insertion order and a file cannot be built to unbalance it without knowing
//! the key. Nodes keep their parent, so an in-order walk needs no stack.

use crate::{Error, Result};

const NIL: u32 = u32::MAX;

/// How many bytes a stored range covers.
pub(crate) trait RangeLen {
    fn range_len(&self) -> u64;
}

#[derive(Clone, Copy)]
struct Node<V> {
    key: u64,
    value: V,
    left: u32,
    right: u32,
    parent: u32,
}

pub(crate) struct FixedRanges<V> {
    nodes: Box<[Node<V>]>,
    root: u32,
    /// Head of the free list, linked through `left`.
    free: u32,
    len: usize,
    seed: u64,
}

impl<V: Copy + Default + RangeLen> FixedRanges<V> {
    /// A map holding at most `capacity` ranges, its memory taken now.
    pub(crate) fn with_capacity(capacity: usize, resource: &'static str) -> Result<Self> {
        let capacity = capacity.clamp(1, NIL as usize - 1);
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(capacity)
            .map_err(|_| Error::AllocationFailed {
                resource,
                requested: (capacity * std::mem::size_of::<Node<V>>()) as u64,
            })?;
        nodes.resize(
            capacity,
            Node {
                key: 0,
                value: V::default(),
                left: NIL,
                right: NIL,
                parent: NIL,
            },
        );
        let random = std::collections::hash_map::RandomState::new();
        let seed = std::hash::BuildHasher::hash_one(&random, 0x7265_6e61_u64) | 1;
        let mut map = Self {
            nodes: nodes.into_boxed_slice(),
            root: NIL,
            free: NIL,
            len: 0,
            seed,
        };
        map.clear();
        Ok(map)
    }

    pub(crate) fn capacity(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// Forgets every range.
    pub(crate) fn clear(&mut self) {
        let last = self.nodes.len() - 1;
        for (index, node) in self.nodes.iter_mut().enumerate() {
            node.left = if index == last {
                NIL
            } else {
                // The capacity is below `NIL`, so every index fits.
                u32::try_from(index + 1).unwrap_or(NIL)
            };
            node.right = NIL;
            node.parent = NIL;
        }
        self.free = 0;
        self.root = NIL;
        self.len = 0;
    }

    fn priority(&self, key: u64) -> u64 {
        let mixed = (key ^ self.seed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (mixed ^ (mixed >> 29)).wrapping_mul(0xBF58_476D_1CE4_E5B9)
    }

    fn node(&self, index: u32) -> &Node<V> {
        &self.nodes[index as usize]
    }

    fn node_mut(&mut self, index: u32) -> &mut Node<V> {
        &mut self.nodes[index as usize]
    }

    /// The node holding `key`.
    fn find(&self, key: u64) -> u32 {
        let mut at = self.root;
        while at != NIL {
            let node = self.node(at);
            if key == node.key {
                return at;
            }
            at = if key < node.key {
                node.left
            } else {
                node.right
            };
        }
        NIL
    }

    pub(crate) fn get(&self, key: u64) -> Option<&V> {
        let at = self.find(key);
        (at != NIL).then(|| &self.node(at).value)
    }

    /// Puts `value` at `key`, replacing what was there. `false` when the key
    /// is new and the map is full: nothing changed.
    pub(crate) fn insert(&mut self, key: u64, value: V) -> bool {
        let mut parent = NIL;
        let mut at = self.root;
        while at != NIL {
            let node = self.node(at);
            if key == node.key {
                self.node_mut(at).value = value;
                return true;
            }
            parent = at;
            at = if key < node.key {
                node.left
            } else {
                node.right
            };
        }
        if self.free == NIL {
            return false;
        }
        let new = self.free;
        self.free = self.node(new).left;
        *self.node_mut(new) = Node {
            key,
            value,
            left: NIL,
            right: NIL,
            parent,
        };
        if parent == NIL {
            self.root = new;
        } else if key < self.node(parent).key {
            self.node_mut(parent).left = new;
        } else {
            self.node_mut(parent).right = new;
        }
        self.len += 1;
        let priority = self.priority(key);
        while self.node(new).parent != NIL
            && priority > self.priority(self.node(self.node(new).parent).key)
        {
            self.rotate_up(new);
        }
        true
    }

    /// Removes `key`; returns its value.
    #[cfg(test)]
    pub(crate) fn remove(&mut self, key: u64) -> Option<V> {
        let at = self.find(key);
        if at == NIL {
            return None;
        }
        Some(self.remove_node(at))
    }

    fn remove_node(&mut self, at: u32) -> V {
        // Rotate it down until it is a leaf, keeping the heap order.
        loop {
            let Node { left, right, .. } = *self.node(at);
            let child = match (left, right) {
                (NIL, NIL) => break,
                (NIL, only) | (only, NIL) => only,
                (left, right) => {
                    if self.priority(self.node(left).key) > self.priority(self.node(right).key) {
                        left
                    } else {
                        right
                    }
                }
            };
            self.rotate_up(child);
        }
        let parent = self.node(at).parent;
        if parent == NIL {
            self.root = NIL;
        } else if self.node(parent).left == at {
            self.node_mut(parent).left = NIL;
        } else {
            self.node_mut(parent).right = NIL;
        }
        let value = self.node(at).value;
        let free = self.free;
        let node = self.node_mut(at);
        node.left = free;
        node.right = NIL;
        node.parent = NIL;
        self.free = at;
        self.len -= 1;
        value
    }

    /// Lifts `x` above its parent.
    fn rotate_up(&mut self, x: u32) {
        let p = self.node(x).parent;
        let g = self.node(p).parent;
        if self.node(p).left == x {
            let moved = self.node(x).right;
            self.node_mut(p).left = moved;
            if moved != NIL {
                self.node_mut(moved).parent = p;
            }
            self.node_mut(x).right = p;
        } else {
            let moved = self.node(x).left;
            self.node_mut(p).right = moved;
            if moved != NIL {
                self.node_mut(moved).parent = p;
            }
            self.node_mut(x).left = p;
        }
        self.node_mut(p).parent = x;
        self.node_mut(x).parent = g;
        if g == NIL {
            self.root = x;
        } else if self.node(g).left == p {
            self.node_mut(g).left = x;
        } else {
            self.node_mut(g).right = x;
        }
    }

    fn first(&self) -> u32 {
        let mut at = self.root;
        if at == NIL {
            return NIL;
        }
        while self.node(at).left != NIL {
            at = self.node(at).left;
        }
        at
    }

    fn successor(&self, mut at: u32) -> u32 {
        if self.node(at).right != NIL {
            at = self.node(at).right;
            while self.node(at).left != NIL {
                at = self.node(at).left;
            }
            return at;
        }
        loop {
            let parent = self.node(at).parent;
            if parent == NIL || self.node(parent).left == at {
                return parent;
            }
            at = parent;
        }
    }

    /// The node with the largest key not above `key`, else the first node.
    fn floor_or_first(&self, key: u64) -> u32 {
        let mut at = self.root;
        let mut best = NIL;
        while at != NIL {
            let node = self.node(at);
            if node.key <= key {
                best = at;
                at = node.right;
            } else {
                at = node.left;
            }
        }
        if best == NIL { self.first() } else { best }
    }

    /// Calls `visit` with every range overlapping `[at, at + len)`, in key
    /// order. Ranges never overlap each other, so only the one starting at
    /// or before `at` can reach into it from below.
    pub(crate) fn for_each_overlapping(
        &self,
        at: u64,
        len: u64,
        visit: &mut dyn FnMut(u64, &V) -> Result<()>,
    ) -> Result<()> {
        let end = at.saturating_add(len);
        let mut node = self.floor_or_first(at);
        while node != NIL {
            let Node { key, value, .. } = self.node(node);
            if *key >= end {
                break;
            }
            if key.saturating_add(value.range_len()) > at {
                visit(*key, value)?;
            }
            node = self.successor(node);
        }
        Ok(())
    }

    /// Calls `visit` with every range, in key order.
    #[cfg_attr(not(any(test, feature = "mmap")), allow(dead_code))]
    pub(crate) fn for_each(&self, visit: &mut dyn FnMut(u64, &V) -> Result<()>) -> Result<()> {
        let mut node = self.first();
        while node != NIL {
            let Node { key, value, .. } = self.node(node);
            visit(*key, value)?;
            node = self.successor(node);
        }
        Ok(())
    }

    /// Removes every range `keep` refuses.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(u64, &V) -> bool) {
        let mut node = self.first();
        while node != NIL {
            let next = self.successor(node);
            let Node { key, value, .. } = *self.node(node);
            if !keep(key, &value) {
                self.remove_node(node);
            }
            node = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    struct Len(u64, u64);
    impl RangeLen for Len {
        fn range_len(&self) -> u64 {
            self.0
        }
    }

    /// The map against a `BTreeMap` model through random inserts, removals,
    /// retains, overlap queries and a full map.
    #[test]
    fn matches_an_ordered_model() {
        let mut map: FixedRanges<Len> = FixedRanges::with_capacity(200, "test").unwrap();
        let mut model: BTreeMap<u64, Len> = BTreeMap::new();
        let mut state = 0x1234_5678_9abc_def0_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..20_000u64 {
            // Units of 16 bytes on a 16-byte grid never overlap.
            let key = (next() % 400) * 16;
            match next() % 10 {
                0..=5 => {
                    let value = Len(16, round);
                    let inserted = map.insert(key, value);
                    if model.contains_key(&key) || model.len() < 200 {
                        assert!(inserted);
                        model.insert(key, value);
                    } else {
                        assert!(!inserted);
                    }
                }
                6..=7 => assert_eq!(map.remove(key), model.remove(&key)),
                8 => {
                    let cut = next() % 20_000;
                    map.retain(|_, value| value.1 >= cut);
                    model.retain(|_, value| value.1 >= cut);
                }
                _ => {
                    let at = next() % 7000;
                    let len = next() % 200;
                    let mut seen = Vec::new();
                    map.for_each_overlapping(at, len, &mut |key, value| {
                        seen.push((key, *value));
                        Ok(())
                    })
                    .unwrap();
                    let want: Vec<_> = model
                        .iter()
                        .filter(|(key, value)| **key < at + len && **key + value.0 > at)
                        .map(|(key, value)| (*key, *value))
                        .collect();
                    assert_eq!(seen, want, "[{at}, +{len})");
                }
            }
            assert_eq!(map.len(), model.len());
        }
        let mut all = Vec::new();
        map.for_each(&mut |key, value| {
            all.push((key, *value));
            Ok(())
        })
        .unwrap();
        assert_eq!(all, model.into_iter().collect::<Vec<_>>());
    }
}
