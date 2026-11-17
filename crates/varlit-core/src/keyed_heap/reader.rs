//! A reader of keyed storage: one published generation, kept until it
//! follows. Every read is checked against the ring: a reader the ring has
//! moved past gets `MatrixSnapshotExpired` and must follow, never a mixed
//! answer.

use super::lookup::{Ctx, View};
use super::node::NODE_BYTES;
use super::tree::{self, KeySrc};
use super::{Fixed, META_FIXED_LEN, META_ROOTS_AT, META_ROOTS_LEN, META_STATE_AT, Roots, State};
use crate::matrix_tiles::store::TileSource;
use crate::matrix_tiles::view::RingReader;
use crate::{Error, Result};

pub(crate) struct KeyedReader {
    meta: u64,
    reader: RingReader,
    state: State,
    roots: Roots,
}

impl KeyedReader {
    /// Opens the newest published generation of the keyed storage whose
    /// metadata payload is at `meta`.
    pub(crate) fn open(source: &dyn TileSource, meta: u64) -> Result<Self> {
        let mut bytes = [0u8; crate::width::small(META_FIXED_LEN)];
        source.read_at(meta, &mut bytes)?;
        let fixed = Fixed::decode(&bytes)?;
        let mut reader = Self {
            meta,
            reader: RingReader::open(source, fixed.ring)?,
            state: State::default(),
            roots: Roots::default(),
        };
        reader.load_state(source)?;
        Ok(reader)
    }

    fn load_state(&mut self, source: &dyn TileSource) -> Result<()> {
        let mut raw = [0u8; State::FIELDS * 8];
        self.reader
            .read(source, self.meta + META_STATE_AT, &mut raw)?;
        let state = State::decode(&raw)?;
        let mut raw = [0u8; crate::width::small(META_ROOTS_LEN)];
        self.reader
            .read(source, self.meta + META_ROOTS_AT, &mut raw)?;
        self.roots = Roots::decode(&raw)?;
        self.state = state;
        Ok(())
    }

    /// The generation this reader shows.
    pub(crate) const fn snapshot(&self) -> u64 {
        self.reader.snapshot()
    }

    pub(crate) const fn state(&self) -> &State {
        &self.state
    }

    /// Adopts the newest published generation. Returns whether it changed.
    pub(crate) fn follow(&mut self, source: &dyn TileSource) -> Result<bool> {
        if !self.reader.follow(source)? {
            return Ok(false);
        }
        self.load_state(source)?;
        Ok(true)
    }

    /// Positions a scan of `block_id`'s records (see
    /// [`super::heap::KeyedHeap::seek`]). Its reads, and the scan's, are
    /// checked against the ring one by one: a reader the ring moves past
    /// gets `MatrixSnapshotExpired`.
    pub(crate) fn seek(
        &mut self,
        source: &dyn TileSource,
        block_id: u32,
        from: Option<(&[u8], bool)>,
    ) -> Result<Option<tree::Cursor>> {
        let Some(slot) = self.roots.find(block_id) else {
            return Ok(None);
        };
        let mut page = [0u8; NODE_BYTES];
        let mut ctx = Ctx::new(&mut self.reader, source);
        tree::seek(&mut ctx, self.roots.slots[slot].root(), from, &mut page).map(Some)
    }

    /// Visits records in key order from `cursor` (see [`tree::scan`]).
    pub(crate) fn scan(
        &mut self,
        source: &dyn TileSource,
        cursor: tree::Cursor,
        visit: &mut tree::Visit<'_>,
    ) -> Result<()> {
        let mut page = [0u8; NODE_BYTES];
        let mut ctx = Ctx::new(&mut self.reader, source);
        tree::scan(&mut ctx, cursor, &mut page, visit)
    }

    /// Finds `key` of `block_id` and reads its payload into `buf`, checking
    /// the ring once for the whole lookup instead of on every read; runs it
    /// again (up to eight times) when the ring moved under it.
    ///
    /// `encode` writes the key into `buf` at `key_at..` before each attempt:
    /// the payload overwrites it. A payload longer than `max_payload` is
    /// refused. Returns whether the key was found.
    pub(crate) fn get(
        &mut self,
        source: &dyn TileSource,
        block_id: u32,
        buf: &mut Vec<u8>,
        key_at: usize,
        max_payload: u64,
        encode: &mut dyn FnMut(&mut Vec<u8>) -> Result<()>,
    ) -> Result<bool> {
        for _ in 0..8 {
            let mut view = Unsettled {
                reader: &mut self.reader,
                oldest: u64::MAX,
            };
            let result = attempt(
                &mut view,
                source,
                &self.roots,
                block_id,
                buf,
                key_at,
                max_payload,
                encode,
            );
            let oldest = view.oldest;
            // Settle even after an error: a damaged-looking record read
            // while the ring moved is read again, not reported.
            if self.reader.settle(source, oldest)? {
                return result;
            }
        }
        Err(Error::MatrixSnapshotExpired {
            snapshot: self.reader.snapshot(),
            checkpointed: 0,
        })
    }
}

/// One unvalidated lookup of [`KeyedReader::get`].
#[allow(clippy::too_many_arguments, reason = "one lookup's inputs")]
fn attempt(
    view: &mut Unsettled<'_>,
    source: &dyn TileSource,
    roots: &Roots,
    block_id: u32,
    buf: &mut Vec<u8>,
    key_at: usize,
    max_payload: u64,
    encode: &mut dyn FnMut(&mut Vec<u8>) -> Result<()>,
) -> Result<bool> {
    encode(buf)?;
    let key = buf.get(key_at..).ok_or(Error::InvalidFormatSpec(
        "keyed lookup key starts past its buffer",
    ))?;
    let Some(slot) = roots.find(block_id) else {
        return Ok(false);
    };
    let mut page = [0u8; NODE_BYTES];
    let mut ctx = Ctx::new(view, source);
    let key = KeySrc::Parts(key);
    let (_, found) = tree::find(&mut ctx, roots.slots[slot].root(), &key, &mut page)?;
    let Some(body) = found else {
        return Ok(false);
    };
    let len = body.value_len;
    if len > max_payload {
        return Err(Error::LimitExceeded {
            resource: "keyed record payload",
            actual: len,
            limit: max_payload,
        });
    }
    let len = crate::width::index(len)?;
    buf.clear();
    buf.try_reserve(len).map_err(|_| Error::AllocationFailed {
        resource: "keyed record payload",
        requested: len as u64,
    })?;
    buf.resize(len, 0);
    tree::read_value(&mut ctx, &body, buf)?;
    Ok(true)
}

/// Reads of one lookup that do not check the ring; [`RingReader::settle`]
/// checks them together at its end.
struct Unsettled<'a> {
    reader: &'a mut RingReader,
    /// The oldest generation any read laid.
    oldest: u64,
}

impl View for Unsettled<'_> {
    fn read(&mut self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        let laid = self.reader.read_unsettled(source, at, buf)?;
        self.oldest = self.oldest.min(laid);
        Ok(())
    }
}
