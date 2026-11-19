//! What the writer and a reader read keyed storage through: only the
//! [`View`] bytes come through differs.

use crate::Result;
use crate::matrix_tiles::store::TileSource;
use crate::matrix_tiles::view::RingReader;

/// Reads the file as one snapshot shows it.
pub(crate) trait View {
    fn read(&mut self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()>;
}

/// A reader's view: its snapshot, checked on every read against the ring
/// moving past it.
impl View for RingReader {
    fn read(&mut self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        Self::read(self, source, at, buf)
    }
}

/// A view and the file it reads.
pub(crate) struct Ctx<'a> {
    pub view: &'a mut dyn View,
    pub source: &'a dyn TileSource,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(view: &'a mut dyn View, source: &'a dyn TileSource) -> Self {
        Self { view, source }
    }

    pub(crate) fn read(&mut self, at: u64, buf: &mut [u8]) -> Result<()> {
        self.view.read(self.source, at, buf)
    }
}
