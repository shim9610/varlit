//! The test-only consistency check of [`KeyedHeap`].

use super::*;

impl KeyedHeap {
    /// Checks the tree (page kinds, keys in order within their separators'
    /// bounds, overflow chains), the free list, and the counts the state
    /// keeps.
    pub(crate) fn check_consistency(
        &self,
        store: &dyn TileSource,
    ) -> std::result::Result<(), String> {
        type Bound = Option<Vec<u8>>;
        let text = |error: Error| format!("{error}");
        let mut seen = std::collections::HashSet::new();
        let (mut leaves, mut interiors, mut overflow, mut keys, mut bytes) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        let mut page_bytes = vec![0u8; PAGE];
        let full_key = |loc: KeyLoc| -> std::result::Result<Vec<u8>, String> {
            let mut view = self.view();
            let mut ctx = self.ctx(&mut view, store);
            let mut key = vec![0u8; usize::try_from(loc.len).map_err(|e| e.to_string())?];
            KeySrc::Loc(loc).read(&mut ctx, 0, &mut key).map_err(text)?;
            Ok(key)
        };
        let chain = |head: u64,
                     len: u64,
                     seen: &mut std::collections::HashSet<u64>|
         -> std::result::Result<u64, String> {
            let mut page = head;
            let (mut held, mut pages) = (0u64, 0u64);
            while page != NO_PAGE {
                if !seen.insert(page) {
                    return Err(format!("overflow page {page} reached twice"));
                }
                let mut header = [0u8; 16];
                self.read(store, page + 16, &mut header).map_err(text)?;
                if &header[..4] != tree::OVERFLOW_MAGIC {
                    return Err(format!("page {page} is not an overflow page"));
                }
                held += u64::from(u16::from_le_bytes([header[4], header[5]]));
                pages += 1;
                page = u64::from_le_bytes(header[8..16].try_into().expect("eight"));
            }
            if held != len {
                return Err(format!("overflow chain {head} holds {held} of {len}"));
            }
            Ok(pages)
        };
        let mut stack: Vec<(u64, usize, Bound, Bound)> = Vec::new();
        let mut roots = Vec::new();
        for slot in &self.roots.slots[..self.roots.used()] {
            let height = usize::try_from(slot.height).map_err(|e| e.to_string())?;
            stack.push((slot.page, height - 1, None, None));
            roots.push(slot.page);
        }
        while let Some((page, level, low, high)) = stack.pop() {
            if !seen.insert(page) {
                return Err(format!("tree page {page} reached twice"));
            }
            self.read(store, page, &mut page_bytes).map_err(text)?;
            let kind = node::kind(&page_bytes).map_err(|error| text(error.into()))?;
            let interior = level > 0;
            if (kind == node::Kind::Interior) != interior {
                return Err(format!("page {page} at level {level} is {kind:?}"));
            }
            let count = node::count(&page_bytes);
            if interior {
                interiors += 1;
                if count == 0 {
                    return Err(format!("interior page {page} is empty"));
                }
            } else {
                leaves += 1;
                keys += count as u64;
                if count == 0 && !roots.contains(&page) {
                    return Err(format!("leaf {page} is empty"));
                }
            }
            let mut previous: Option<Vec<u8>> = None;
            let mut children = Vec::new();
            for index in 0..count {
                let body = tree::held_body(&page_bytes, page, index, interior).map_err(text)?;
                let total = body.key.len + body.value_len;
                if body.overflowed() {
                    overflow += chain(body.key.overflow, total - body.key.local, &mut seen)?;
                }
                if !interior {
                    bytes += total;
                }
                if interior && index == 0 {
                    if body.key.len != 0 {
                        return Err(format!("page {page}: the first separator is not empty"));
                    }
                    children.push((body.child, low.clone()));
                    continue;
                }
                let key = full_key(body.key)?;
                if previous.as_ref().is_some_and(|previous| *previous >= key)
                    || low.as_ref().is_some_and(|low| key < *low)
                    || high.as_ref().is_some_and(|high| key >= *high)
                {
                    return Err(format!("page {page}: entry {index} out of order"));
                }
                previous = Some(key.clone());
                if interior {
                    children.push((body.child, Some(key)));
                }
            }
            for (index, (child, child_low)) in children.iter().enumerate() {
                let child_high = children
                    .get(index + 1)
                    .map_or_else(|| high.clone(), |next| next.1.clone());
                stack.push((*child, level - 1, child_low.clone(), child_high));
            }
        }
        let mut free = 0u64;
        let mut page = self.state.free_head;
        while page != NO_PAGE {
            if !seen.insert(page) {
                return Err(format!("free page {page} is in use or listed twice"));
            }
            free += 1;
            if free > self.state.free {
                return Err("free list longer than counted".into());
            }
            let mut link = [0u8; 16];
            self.read(store, page, &mut link).map_err(text)?;
            let by = u64::from_le_bytes(link[8..].try_into().expect("eight"));
            if by > self.ring.published() + 1 {
                return Err(format!("free page {page} freed by generation {by}"));
            }
            page = u64::from_le_bytes(link[..8].try_into().expect("eight"));
        }
        let counted = (
            leaves,
            interiors,
            overflow,
            free,
            keys,
            bytes,
            leaves + interiors + overflow + free,
        );
        let state = (
            self.state.leaves,
            self.state.interiors,
            self.state.overflow,
            self.state.free,
            self.state.keys,
            self.state.record_bytes,
            self.state.pages,
        );
        if counted != state {
            return Err(format!(
                "leaves, interior, overflow, free pages, keys, bytes, pages: \
                 {counted:?}, state says {state:?}"
            ));
        }
        Ok(())
    }
}
