//! A reader of keyed storage: one published generation, kept until it
//! follows. Every read is checked against the ring: a reader the ring has
//! moved past gets `MatrixSnapshotExpired` and must follow, never a mixed
//! answer.

use super::lookup::{self, Located};
use super::{Fixed, META_FIXED_LEN, META_STATE_AT, State};
use crate::Result;
use crate::matrix_tiles::store::TileSource;
use crate::matrix_tiles::view::RingReader;

pub(crate) struct KeyedReader {
    meta: u64,
    seed: [u64; 2],
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
            seed: fixed.seed,
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

    /// Finds `key` of `block_id` in this snapshot.
    pub(crate) fn find(
        &mut self,
        source: &dyn TileSource,
        block_id: u32,
        key: &[u8],
    ) -> Result<Option<Located>> {
        lookup::find(
            &mut self.reader,
            source,
            self.seed,
            &self.state,
            block_id,
            key,
        )
    }

    /// Reads a located record's payload into `out` and checks it.
    pub(crate) fn read_payload(
        &mut self,
        source: &dyn TileSource,
        found: &Located,
        out: &mut [u8],
    ) -> Result<()> {
        lookup::read_payload(&mut self.reader, source, found, out)
    }
}
