//! A reader of keyed storage: one published generation, kept until it
//! follows. Every read is checked against the ring: a reader the ring has
//! moved past gets `MatrixSnapshotExpired` and must follow, never a mixed
//! answer.

use super::lookup::{Ctx, View};
use super::tree::{self, KeySrc};
use super::{Fixed, META_FIXED_LEN, META_STATE_AT, State};
use crate::matrix_tiles::store::TileSource;
use crate::matrix_tiles::view::RingReader;
use crate::{Error, Result};

pub(crate) struct KeyedReader {
    meta: u64,
    reader: RingReader,
    state: State,
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
        };
        reader.load_state(source)?;
        Ok(reader)
    }

    fn load_state(&mut self, source: &dyn TileSource) -> Result<()> {
        let mut raw = [0u8; State::FIELDS * 8];
        self.reader
            .read(source, self.meta + META_STATE_AT, &mut raw)?;
        self.state = State::decode(&raw)?;
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
                self.meta,
                &self.state,
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
    meta: u64,
    state: &State,
    block_id: u32,
    buf: &mut Vec<u8>,
    key_at: usize,
    max_payload: u64,
    encode: &mut dyn FnMut(&mut Vec<u8>) -> Result<()>,
) -> Result<bool> {
    // The tree key is the block id, big-endian, then the key: it goes
    // into `buf` before the key the caller encodes.
    buf.clear();
    encode(buf)?;
    let key_at = key_at.min(buf.len());
    buf.splice(key_at..key_at, block_id.to_be_bytes());
    let key = buf.get(key_at..).ok_or(Error::InvalidFormatSpec(
        "keyed lookup key starts past its buffer",
    ))?;
    let mut ctx = Ctx::new(view, source, meta, None);
    let path = tree::find(&mut ctx, state.root, &KeySrc::Mem(key))?;
    if !path.found {
        return Ok(false);
    }
    let body = tree::read_body(
        &mut ctx,
        path.pages[0],
        path.counts[0],
        path.index[0],
        false,
    )?;
    let len = tree::value_len(&body);
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
