//! Test-only helpers that read a whole file through the paged APIs.
//!
//! `index_entries` and `blocks::<T>` fill a caller's buffer and hand back a
//! cursor; nothing in the library builds a whole-file list any more. Tests
//! that want to assert over every record of a small fixture page through it
//! here, in pages of `PAGE` so the cursor path is exercised too.

#![allow(dead_code, unused_macros)]

pub const PAGE: usize = 7;

/// Every index entry of `$file`, in file order. Panics on a read error, as
/// the infallible `index_entries()` this replaces silently did not.
macro_rules! all_entries {
    ($file:expr) => {{
        let source = &$file;
        let mut out = vec![::varlit::RecordIndexEntry::default(); $crate::pages::PAGE];
        let mut all = Vec::new();
        let mut page = source.index_entries(&mut out).expect("index entries page");
        loop {
            all.extend_from_slice(&out[..page.filled]);
            match page.next {
                Some(next) => {
                    page = source
                        .index_entries_from(next, &mut out)
                        .expect("index entries page")
                }
                None => break all,
            }
        }
    }};
}

/// Every record of block `$ty` in `$file`, decoded, as a [`Blocks`].
macro_rules! all_blocks {
    ($file:expr, $ty:ty) => {{
        let source = &$file;
        let mut out: Vec<$ty> = (0..$crate::pages::PAGE)
            .map(|_| <$ty as ::varlit::VarlitEmpty>::empty())
            .collect();
        let mut payload = Vec::new();
        let mut all: Vec<$ty> = Vec::new();
        (|| -> ::varlit::Result<$crate::pages::Blocks<$ty>> {
            let mut page = source.blocks::<$ty>(&mut payload, &mut out)?;
            loop {
                all.extend(out.drain(..page.filled));
                out.extend((0..page.filled).map(|_| <$ty as ::varlit::VarlitEmpty>::empty()));
                match page.next {
                    Some(next) => page = source.blocks_from::<$ty>(next, &mut payload, &mut out)?,
                    None => return Ok($crate::pages::Blocks(std::mem::take(&mut all))),
                }
            }
        })()
    }};
}

/// Every record a generated paged accessor pair (`$first`, `$from`) returns,
/// e.g. `all_generated!(reader, points, points_from)`.
macro_rules! all_generated {
    ($file:expr, $first:ident, $from:ident) => {{
        let source = &$file;
        let mut out: Vec<_> = (0..$crate::pages::PAGE)
            .map(|_| ::varlit::VarlitEmpty::empty())
            .collect();
        let mut payload = Vec::new();
        let mut all = Vec::new();
        (|| -> ::varlit::Result<$crate::pages::Blocks<_>> {
            let mut page = source.$first(&mut payload, out.as_mut_slice())?;
            loop {
                all.extend(out.drain(..page.filled));
                out.extend((0..page.filled).map(|_| ::varlit::VarlitEmpty::empty()));
                match page.next {
                    Some(next) => page = source.$from(next, &mut payload, out.as_mut_slice())?,
                    None => return Ok($crate::pages::Blocks(std::mem::take(&mut all))),
                }
            }
        })()
    }};
}

/// A decoded block list with the read surface tests used on `BlockVec`.
#[derive(Clone, Debug, PartialEq)]
pub struct Blocks<T>(pub Vec<T>);

impl<T: Clone> Blocks<T> {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Copies record `index` into `out`; `false` past the end.
    pub fn get(&self, index: usize, out: &mut T) -> varlit::Result<bool> {
        Ok(match self.0.get(index) {
            Some(value) => {
                out.clone_from(value);
                true
            }
            None => false,
        })
    }

    pub fn get_value(&self, index: usize) -> varlit::Result<Option<T>> {
        Ok(self.0.get(index).cloned())
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }

    /// Like `get`; the payload buffer is unused, kept for call-site shape.
    pub fn get_into(
        &self,
        index: usize,
        _payload: &mut Vec<u8>,
        out: &mut T,
    ) -> varlit::Result<bool> {
        self.get(index, out)
    }

    pub fn cursor(&self) -> BlocksCursor<'_, T> {
        BlocksCursor {
            blocks: self,
            at: 0,
        }
    }
}

/// Forward reads over a [`Blocks`], in the shape `BlockCursor` had.
pub struct BlocksCursor<'a, T> {
    blocks: &'a Blocks<T>,
    at: usize,
}

impl<T: Clone> BlocksCursor<'_, T> {
    pub fn next_into(&mut self, out: &mut T) -> varlit::Result<bool> {
        let found = self.blocks.get(self.at, out)?;
        if found {
            self.at += 1;
        }
        Ok(found)
    }
}

#[allow(unused_imports)]
pub(crate) use {all_blocks, all_entries, all_generated};
