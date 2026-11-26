//! A private sorted index on a temporary file, for in-memory work that
//! outgrows its buffer: tail tracking, external merges, spilled rows. No
//! writer admission, publication or durability: the file is disposable
//! scratch owned by one handle.

use std::collections::BTreeMap;

mod tree;

/// Bytes of the value stored under each key.
const LATEST_LEN: usize = 52;

/// Why a scratch index operation failed.
#[derive(Debug)]
pub(crate) enum ScratchError {
    Io(std::io::Error),
    /// The scratch file does not hold what this handle wrote to it.
    Corrupt(&'static str),
    /// A library check on the way, such as a field width.
    Library(crate::Error),
}

pub(crate) type ScratchResult<T> = std::result::Result<T, ScratchError>;

impl From<std::io::Error> for ScratchError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<crate::Error> for ScratchError {
    fn from(error: crate::Error) -> Self {
        Self::Library(error)
    }
}

fn scratch_error(error: ScratchError) -> crate::Error {
    match error {
        ScratchError::Library(error) => error,
        ScratchError::Io(error) => crate::Error::Io(error),
        ScratchError::Corrupt(message) => crate::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    }
}

/// Private temporary index used when an in-memory operation outgrows its
/// buffer. No writer admission, shared cache, publication or durability I/O:
/// this file is disposable scratch owned by a single handle.
pub(crate) struct ScratchIndex {
    file: crate::fallible::Arc<std::fs::File>,
    root: tree::Root,
    reader: tree::Reader,
    cache_bytes: usize,
    end: u64,
    maximum: Option<u64>,
    /// Pages the copy-on-write updates leave behind, reused by the next ones.
    /// Nothing but this handle reads the file, so a page the previous root
    /// referenced is free the moment the new root is in place: the file grows
    /// with the tree, not with the number of updates.
    alloc: tree::PageAlloc,
    /// One per handle sharing this file: clones share it, so a count above
    /// one means another root may still reference a released page.
    sharers: crate::fallible::Arc<()>,
}
impl Clone for ScratchIndex {
    fn clone(&self) -> Self {
        Self {
            file: self.file.clone(),
            root: self.root,
            cache_bytes: self.cache_bytes,
            end: self.end,
            maximum: self.maximum,
            // A clone starts with nothing to reuse; neither side releases a
            // page while the file is shared (see `apply_iter`).
            alloc: tree::PageAlloc::new(self.alloc.building),
            sharers: self.sharers.clone(),
            reader: tree::Reader::new(
                self.file.clone(),
                self.end,
                u32::MAX as usize,
                self.cache_bytes,
            )
            .with_format(tree::PageFormat::Reuse),
        }
    }
}
impl std::fmt::Debug for ScratchIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScratchIndex")
            .field("rows", &self.root.count)
            .finish()
    }
}
impl ScratchIndex {
    pub(crate) fn new(cache_bytes: usize) -> crate::Result<Self> {
        let (file, maximum) = crate::spill_file::SpillFile::new()?.into_parts();
        crate::spill_file::check_extent(maximum, tree::PAGE as u64)?;
        let file = crate::fallible::try_arc(file)?;
        file.set_len(tree::PAGE as u64)?;
        let reader = tree::Reader::new(
            file.clone(),
            tree::PAGE as u64,
            u32::MAX as usize,
            cache_bytes,
        )
        .with_format(tree::PageFormat::Reuse);
        Ok(Self {
            file,
            root: tree::Root::default(),
            reader,
            cache_bytes,
            end: tree::PAGE as u64,
            maximum,
            alloc: tree::PageAlloc::new(1),
            sharers: crate::fallible::try_arc(())?,
        })
    }
    pub(crate) fn get(&self, key: &[u8]) -> crate::Result<Option<[u8; 52]>> {
        self.reader.get(self.root, key).map_err(scratch_error)
    }
    pub(crate) fn visit(
        &self,
        mut visitor: impl FnMut(Vec<u8>, [u8; 52]) -> crate::Result<()>,
    ) -> crate::Result<()> {
        let mut failure = None;
        let result = self.reader.visit(self.root, &mut |key, value| {
            if let Err(error) = visitor(key, value) {
                failure = Some(error);
                return Err(ScratchError::Io(std::io::ErrorKind::Other.into()));
            }
            Ok(())
        });
        if let Some(error) = failure {
            return Err(error);
        }
        result.map_err(scratch_error)
    }
    pub(crate) fn apply(&mut self, values: &BTreeMap<Vec<u8>, [u8; 52]>) -> crate::Result<()> {
        self.apply_iter(values.iter())
    }
    /// Updates must be unique and in ascending key order.
    pub(crate) fn apply_iter<'a>(
        &mut self,
        values: impl ExactSizeIterator<Item = (&'a Vec<u8>, &'a [u8; 52])>,
    ) -> crate::Result<()> {
        // A refused limited batch must not consume the remaining scratch
        // allowance with unpublished nodes. Preserve all pre-existing clones.
        let prior = self
            .maximum
            .map(|_| self.file.metadata().map(|m| m.len()))
            .transpose()?;
        let building = self.alloc.building;
        let root = match tree::apply_with_limit(
            &self.file,
            &self.reader,
            self.root,
            values,
            self.maximum,
            Some(&mut self.alloc),
        ) {
            Ok(root) => root,
            Err(error) => {
                // The old root still stands: pages it references that this
                // attempt released go back to pending-forever rather than
                // reusable, and nothing they hold was overwritten (pages of an
                // older generation are only ever released, never reused,
                // within one update).
                self.alloc.pending.retain(|&(_, tag)| tag < building);
                if let Some(prior) = prior {
                    self.file.set_len(prior)?;
                }
                return Err(scratch_error(error));
            }
        };
        // The new root is in place. If this handle is the file's only reader,
        // every page the update released is free now; while a clone shares
        // the file, its root may still reference them, so they stay retired.
        if crate::fallible::Arc::strong_count(&self.sharers) == 1 {
            self.alloc.release(building).map_err(scratch_error)?;
        } else {
            self.alloc.pending.clear();
        }
        self.alloc.building = building + 1;
        self.end = self.file.metadata()?.len();
        self.reader = tree::Reader::new(
            self.file.clone(),
            self.end,
            u32::MAX as usize,
            self.cache_bytes,
        )
        .with_format(tree::PageFormat::Reuse);
        self.root = root;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::Result;

    use super::*;

    /// Repeated updates of the same keys must not grow the scratch file: the
    /// pages each copy-on-write update leaves behind are reused by the next.
    #[test]
    fn scratch_updates_reuse_released_pages() -> Result<()> {
        let mut index = ScratchIndex::new(64 * 1024)?;
        let batch = |round: u64| -> BTreeMap<Vec<u8>, [u8; 52]> {
            (0..2_000u64)
                .map(|key| {
                    let mut row = [0; 52];
                    row[..8].copy_from_slice(&round.to_le_bytes());
                    (key.to_be_bytes().to_vec(), row)
                })
                .collect()
        };
        index.apply(&batch(0))?;
        let after_first = index.file.metadata()?.len();
        for round in 1..200 {
            index.apply(&batch(round))?;
        }
        let after_all = index.file.metadata()?.len();
        assert!(
            after_all <= after_first * 3,
            "200 rewrites grew the scratch file from {after_first} to {after_all} bytes"
        );
        let row = index.get(&7u64.to_be_bytes())?.expect("key 7");
        assert_eq!(u64::from_le_bytes(row[..8].try_into().unwrap()), 199);
        Ok(())
    }
}
