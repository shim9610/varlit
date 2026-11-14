//! The tile engine's store: this file's append log.
//!
//! Matrix storage records are appended through the ordinary record writer
//! (so they are framed, sequenced, anchored and counted like every record)
//! with [`RECORD_FLAG_IN_PLACE`], and then changed in place through the
//! primary handle, which steps around anchors.

use super::*;
use crate::matrix_tiles::RecordKind;
use crate::matrix_tiles::api::TiledMatrix;
use crate::matrix_tiles::store::{TileSource, TileStore};

/// The writer's store: appends records, overwrites them, syncs the file.
pub(super) struct FileTiles<'a> {
    file: &'a mut VarlitFile,
    /// Whether a record was appended since this store was handed out.
    pub(super) appended: bool,
}

impl TileSource for FileTiles<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.file.file.read_logical_exact(offset, buf)
    }
    fn len(&self) -> Result<u64> {
        Ok(self.file.file.len()?)
    }
}

impl TileStore for FileTiles<'_> {
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.file.check_write()?;
        self.file.file.overwrite_matrix_bytes(offset, bytes)
    }

    fn append(&mut self, kind: RecordKind, payload: &[u8]) -> Result<u64> {
        self.file
            .spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, payload.len() as u64)?;
        let info = self.file.ensure_write()?.write_record_with_prev_key(
            MATRIX_TILE_BLOCK_ID,
            kind.version(),
            RECORD_FLAG_INTERNAL | RECORD_FLAG_IN_PLACE,
            0,
            payload,
            None,
        )?;
        self.appended = true;
        Ok(info.payload_offset)
    }

    fn sync(&mut self) -> Result<()> {
        // A marker format makes appended records part of the log only at a
        // commit point; matrix records must never sit past one, where a
        // recovery would cut them off from the ring that points at them.
        if self.appended {
            self.file.close_matrix_commit_point()?;
            self.appended = false;
        }
        // `sync_data`: it makes the data and everything needed to read it
        // back durable, a grown length included. Timestamps are not part of
        // what a publication promises, and a full `sync_all` would commit
        // them to the filesystem journal on every publication.
        self.file.file.sync_data()?;
        Ok(())
    }
}

/// A reader's source: positional reads of the primary handle.
pub(super) struct FileTilesRead<'a>(pub(super) &'a RecordFile);

impl TileSource for FileTilesRead<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_logical_exact(offset, buf)
    }
    fn len(&self) -> Result<u64> {
        Ok(self.0.len()?)
    }
}

impl VarlitFile {
    /// Runs `op` on the tiled matrix with this file as its store. A writer
    /// that has not opened its matrix yet opens it here.
    pub(super) fn with_tiles<R>(
        &mut self,
        op: impl FnOnce(&mut TiledMatrix, &mut FileTiles<'_>) -> Result<R>,
    ) -> Result<R> {
        self.ensure_tiles_writer()?;
        let mut tiles = self.tiles.take().ok_or(Error::MatrixLayoutMissing)?;
        let mut store = FileTiles {
            file: self,
            appended: false,
        };
        let result = op(&mut tiles, &mut store);
        let _ = self.tiles.set(tiles);
        result
    }

    /// The tiled matrix and a source to read it through. A handle that has
    /// not opened its matrix yet opens it for reading here.
    pub(super) fn tiles_read(&self) -> Result<(&TiledMatrix, FileTilesRead<'_>)> {
        if !self.spec.has_matrix_blocks() {
            return Err(Error::MatrixLayoutMissing);
        }
        if self.tiles.get().is_none() {
            let opened = TiledMatrix::open_reader(
                self.spec,
                &FileTilesRead(&self.file),
                self.matrix_meta_offset()?,
            )?;
            let _ = self.tiles.set(opened);
        }
        let tiles = self.tiles.get().ok_or(Error::MatrixLayoutMissing)?;
        Ok((tiles, FileTilesRead(&self.file)))
    }

    /// Opens this handle's matrix now: a writer for a writable handle, a
    /// reader whose snapshot is the generation published at this moment.
    pub(super) fn open_tiles_now(&mut self) -> Result<()> {
        if !self.spec.has_matrix_blocks() {
            return Ok(());
        }
        if self.mode == OpenMode::ReadWrite {
            self.ensure_tiles_writer()
        } else {
            self.tiles_read().map(drop)
        }
    }

    /// Opens the matrix as its writer, replaying the ring, when this is a
    /// writable handle that has not yet.
    pub(super) fn ensure_tiles_writer(&mut self) -> Result<()> {
        if !self.spec.has_matrix_blocks() || self.mode != OpenMode::ReadWrite {
            return Ok(());
        }
        if self.tiles.get().is_some_and(TiledMatrix::is_writer) {
            return Ok(());
        }
        let _ = self.tiles.take();
        let spec = self.spec;
        let meta = self.matrix_meta_offset()?;
        let mut store = FileTiles {
            file: self,
            appended: false,
        };
        let tiles = TiledMatrix::open_writer(spec, &mut store, meta)?;
        let _ = self.tiles.set(tiles);
        Ok(())
    }

    /// The payload offset of a matrix's metadata record: the first record
    /// of the append log.
    pub(super) fn matrix_meta_offset(&self) -> Result<u64> {
        Ok(append_log_start_for_file(self)? + RECORD_HEADER_LEN)
    }

    /// Lays out a new tiled matrix as the first records of this file's log.
    pub(super) fn create_tiles(
        &mut self,
        dims: &MatrixDimensions,
        creation_id: [u8; MATRIX_CREATION_NONCE_LEN],
    ) -> Result<()> {
        let spec = self.spec;
        let expected = self.matrix_meta_offset()?;
        let mut store = FileTiles {
            file: self,
            appended: false,
        };
        let tiles = TiledMatrix::create(spec, dims, creation_id, &mut store)?;
        if tiles.meta != expected {
            return Err(Error::InvalidMatrixLayout);
        }
        let _ = self.tiles.set(tiles);
        Ok(())
    }

    /// The creation nonce of this file's matrix.
    pub(super) fn matrix_nonce(&self) -> Result<[u8; MATRIX_CREATION_NONCE_LEN]> {
        Ok(self.tiles_read()?.0.creation_id)
    }

    /// Whether this writer holds matrix changes not yet published.
    pub(super) fn matrix_is_dirty(&self) -> bool {
        self.tiles.get().is_some_and(TiledMatrix::is_dirty)
    }

    /// Publishes staged matrix changes, if any: one barrier.
    pub(super) fn publish_tiles(&mut self) -> Result<()> {
        if !self.tiles.get().is_some_and(TiledMatrix::is_dirty) {
            return Ok(());
        }
        self.with_tiles(|tiles, store| tiles.publish(store).map(drop))
    }

    /// Closes a commit point before matrix records become durable, so a
    /// marker format never leaves them past its last marker.
    pub(super) fn close_matrix_commit_point(&mut self) -> Result<()> {
        if self.spec.commit_policy.is_transaction_marker()
            && self.has_uncommitted_since_last_commit()
        {
            self.write_commit_marker()?;
            self.close_commit_point();
        }
        Ok(())
    }
}
