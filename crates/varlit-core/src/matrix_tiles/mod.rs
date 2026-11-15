//! Tiled matrix storage.
//!
//! A matrix block's cells live in **tiles** of the shape its spec declares
//! (`tile = [rows, cols]`). A tile is an internal record in the append log,
//! appended the first time any of its cells is written, so a never-written
//! tile does not exist and every dimension can grow. Cells are then
//! overwritten **in place**.
//!
//! In-place writing alone would let a crash leave half a publication behind
//! and would take the old value from a reader that has not followed. Both are
//! handled by a write-ahead ring inside the same file ([`wal`]):
//!
//! - a publication appends its byte ranges to the ring as one or more frames,
//!   syncs the file once, and only then is visible;
//! - the ranges reach their place in the file at a **checkpoint**, which runs
//!   only when the ring needs room and only as far as it needs to;
//! - a reader keeps, for its snapshot, an *overlay* of the ranges published
//!   after the last checkpoint it saw, read from the ring. The main file never
//!   holds anything newer than the oldest generation the ring still keeps, so
//!   a reader whose snapshot is still in the ring reads exactly its snapshot.
//!   One that fell further behind than the ring holds gets
//!   [`Error::MatrixSnapshotExpired`](crate::Error::MatrixSnapshotExpired)
//!   instead of a mixed answer. Nothing is locked.
//!
//! Tile directories (a radix table per block keyed by the Morton code of the
//! tile coordinates) and the matrix metadata are byte ranges like any other,
//! so they take the same path.
//!
//! This module is the storage engine over a [`TileStore`]; the matrix API in
//! `file/` maps keys, commit bits and checksums onto it.

pub(crate) mod api;
pub(crate) mod geometry;
pub(crate) mod host;
pub(crate) mod ranges;
mod reader_lock;
pub(crate) mod store;
pub(crate) mod view;
pub(crate) mod wal;
pub(crate) mod writer;

#[cfg(test)]
mod tests;

#[allow(unused_imports)]
pub(crate) use geometry::{BlockGeom, Geometry, TileCoord};
#[allow(unused_imports)]
pub(crate) use store::TileStore;
#[allow(unused_imports)]
pub(crate) use view::TileReader;
#[allow(unused_imports)]
pub(crate) use writer::TileWriter;

/// One radix directory page: 512 little-endian `u64` offsets.
pub(crate) const DIR_FANOUT: u64 = 512;
pub(crate) const DIR_PAGE_BYTES: u64 = DIR_FANOUT * 8;
/// Bits of a Morton key each directory level resolves.
pub(crate) const DIR_LEVEL_BITS: u32 = 9;

/// Kinds of internal record the engine appends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordKind {
    Meta,
    Ring,
    Directory,
    Tile,
}

impl RecordKind {
    /// The block version a storage record of this kind carries.
    pub(crate) const fn version(self) -> u16 {
        match self {
            Self::Meta => 1,
            Self::Ring => 2,
            Self::Directory => 3,
            Self::Tile => 4,
        }
    }
}
