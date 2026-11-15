//! Unit-test helpers that read a whole fixture through the paged APIs.
//!
//! The library builds no whole-file list; tests that assert over every record
//! of a small file page through it here, in pages of `PAGE` so the cursor
//! path is exercised too.

pub(crate) const PAGE: usize = 7;

/// Every index entry of `$file`, in file order. Panics on a read error.
macro_rules! all_entries {
    ($file:expr) => {{
        let source = &$file;
        let mut out = vec![crate::RecordIndexEntry::default(); crate::test_pages::PAGE];
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
        let mut out: Vec<$ty> = (0..crate::test_pages::PAGE)
            .map(|_| <$ty as crate::VarlitEmpty>::empty())
            .collect();
        let mut payload = Vec::new();
        let mut all: Vec<$ty> = Vec::new();
        (|| -> crate::Result<crate::test_pages::Blocks<$ty>> {
            let mut page = source.blocks::<$ty>(&mut payload, &mut out)?;
            loop {
                all.extend(out.drain(..page.filled));
                out.extend((0..page.filled).map(|_| <$ty as crate::VarlitEmpty>::empty()));
                match page.next {
                    Some(next) => page = source.blocks_from::<$ty>(next, &mut payload, &mut out)?,
                    None => return Ok(crate::test_pages::Blocks(std::mem::take(&mut all))),
                }
            }
        })()
    }};
}

/// A decoded block list with the read surface tests used on `BlockVec`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Blocks<T>(pub(crate) Vec<T>);

#[allow(dead_code)]
impl<T: Clone> Blocks<T> {
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn get(&self, index: usize, out: &mut T) -> crate::Result<bool> {
        Ok(match self.0.get(index) {
            Some(value) => {
                out.clone_from(value);
                true
            }
            None => false,
        })
    }

    pub(crate) fn get_value(&self, index: usize) -> crate::Result<Option<T>> {
        Ok(self.0.get(index).cloned())
    }
}
