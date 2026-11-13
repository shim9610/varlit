//! A matrix in an indexed (high-cardinality) primary.
//!
//! The matrix's storage records (metadata, ring, directory pages, tiles) are
//! appended through the indexed writer like any unindexed record, so the
//! sidecar covers them and recovery replays them; their payloads are then
//! rewritten in place, which the sync-marker chain leaves out of its
//! checksums (`sync_marker`). The matrix keeps its own freshness: its ring
//! publishes generations, and a generation names the primary length it
//! needs, so one a recovery cut short is never adopted.
//!
//! A sync appends the records' marker first and then publishes the matrix,
//! whose last barrier makes both durable.

use super::*;
use crate::matrix_tiles::RecordKind;
use crate::matrix_tiles::api::TiledMatrix;
use crate::matrix_tiles::host;
use crate::matrix_tiles::store::{TileSource, TileStore};
use crate::{
    MatrixCellStatus, MatrixCursor, MatrixDimensions, MatrixKey, MatrixPage, MatrixRegion,
    VarlitMatrixBlock,
};

/// Records framed from the start of the log when looking for the matrix
/// metadata record: it follows the creation nonce and, at most, a manifest.
const META_SEARCH_RECORDS: usize = 8;

impl TileSource for SnapshotFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.read_exact_at(offset, buf)
    }
    fn len(&self) -> Result<u64> {
        Ok(SnapshotFile::len(self))
    }
}

/// The metadata record of the matrix in `snapshot`, if the format declares
/// one and the primary carries it.
fn locate_meta(spec: FormatSpec, snapshot: &SnapshotFile) -> Result<Option<u64>> {
    if !spec.has_matrix_blocks() {
        return Ok(None);
    }
    let mut scanner = NativeStreamScanner::from_snapshot_framing_only(spec, snapshot.clone())?;
    for _ in 0..META_SEARCH_RECORDS {
        let Some(entry) = scanner.next_entry()? else {
            break;
        };
        if entry.block_id == crate::file::MATRIX_TILE_BLOCK_ID {
            return Ok(
                (entry.block_version == RecordKind::Meta.version()).then_some(entry.payload_offset)
            );
        }
    }
    Ok(None)
}

/// The matrix of an indexed primary as a reader's snapshot shows it.
pub(super) fn open_reader(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
) -> Result<Option<TiledMatrix>> {
    let Some(meta) = locate_meta(spec, snapshot)? else {
        if spec.has_matrix_blocks() {
            return Err(Error::MatrixLayoutMissing);
        }
        return Ok(None);
    };
    TiledMatrix::open_reader(spec, snapshot, meta).map(Some)
}

/// The writer's store: appends covered records, rewrites them in place.
struct IndexedTiles<'a> {
    writer: &'a mut VarlitIndexedWriter,
}

impl TileSource for IndexedTiles<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.writer.stream.read_logical(offset, buf)
    }
    fn len(&self) -> Result<u64> {
        Ok(self.writer.stream.snapshot().len())
    }
}

impl TileStore for IndexedTiles<'_> {
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.writer.stream.overwrite_matrix_bytes(offset, bytes)
    }

    fn append(&mut self, kind: RecordKind, payload: &[u8]) -> Result<u64> {
        self.writer.append_matrix_record(kind, payload)
    }

    fn sync(&mut self) -> Result<()> {
        self.writer.stream.sync_primary_data()
    }
}

impl VarlitIndexedWriter {
    /// Creates an indexed primary carrying a matrix of `dims`: the indexed
    /// primary as [`Self::create`] makes it, then the matrix, made durable
    /// before this returns.
    pub fn create_with_dims(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: DiskIndexOptions,
        plan: DiskIndexPlan,
        dims: MatrixDimensions,
    ) -> Result<Self> {
        if !spec.has_matrix_blocks() {
            return Err(Error::MatrixLayoutMissing);
        }
        let mut writer = Self::create(spec, path, options, plan)?;
        let nonce = crate::file::fresh_stream_creation_nonce();
        let mut store = IndexedTiles {
            writer: &mut writer,
        };
        let tiles = TiledMatrix::create(spec, &dims, nonce, &mut store)?;
        writer.tiles = Some(tiles);
        writer.sync_with(true)?;
        Ok(writer)
    }

    /// Opens the matrix this primary carries, recovering its ring (after
    /// the primary's own recovery, so a generation needing records that
    /// recovery cut off is dropped).
    pub(super) fn open_matrix(&mut self) -> Result<()> {
        let spec = self.stream.spec();
        let Some(meta) = locate_meta(spec, self.stream.snapshot())? else {
            if spec.has_matrix_blocks() {
                return Err(Error::MatrixLayoutMissing);
            }
            return Ok(());
        };
        let mut store = IndexedTiles { writer: self };
        let tiles = TiledMatrix::open_writer(spec, &mut store, meta)?;
        self.tiles = Some(tiles);
        Ok(())
    }

    pub(super) fn matrix_is_dirty(&self) -> bool {
        self.tiles.as_ref().is_some_and(TiledMatrix::is_dirty)
    }

    pub(super) fn publish_matrix(&mut self) -> Result<()> {
        self.with_tiles(|tiles, store| tiles.publish(store).map(drop))
    }

    /// Appends a matrix storage record, covered in the pending sidecar batch
    /// like an unindexed record. Returns its payload offset.
    fn append_matrix_record(&mut self, kind: RecordKind, payload: &[u8]) -> Result<u64> {
        self.ensure_not_poisoned()?;
        let spec = self.stream.spec();
        spec.read_limits.check(
            crate::format::ReadLimitKey::RecordPayloadLen,
            payload.len() as u64,
        )?;
        self.ensure_batch()?;
        self.ensure_coverage_capacity(spec.index_policy.block_offset_chain)?;
        let old_eof = self.stream.snapshot().len();
        let record = self.stream.prepare_matrix_record(kind.version(), payload)?;
        let info = self.stream.append_matrix_record(record)?;
        self.publish_record_coverage(old_eof, info, crate::file::MATRIX_TILE_BLOCK_ID)?;
        Ok(info.payload_offset)
    }

    fn with_tiles<R>(
        &mut self,
        op: impl FnOnce(&mut TiledMatrix, &mut IndexedTiles<'_>) -> Result<R>,
    ) -> Result<R> {
        let mut tiles = self.tiles.take().ok_or(Error::MatrixLayoutMissing)?;
        let result = {
            let mut store = IndexedTiles { writer: self };
            op(&mut tiles, &mut store)
        };
        self.tiles = Some(tiles);
        result
    }

    /// A matrix mutation: refused on a poisoned writer; an I/O failure
    /// poisons it (what reached the file is no longer known).
    fn matrix_mutation<R>(
        &mut self,
        op: impl FnOnce(&mut TiledMatrix, &mut IndexedTiles<'_>, &mut Vec<u8>) -> Result<R>,
    ) -> Result<R> {
        self.ensure_not_poisoned()?;
        let mut scratch = std::mem::take(&mut self.matrix_scratch);
        let result = self.with_tiles(|tiles, store| op(tiles, store, &mut scratch));
        scratch.clear();
        self.matrix_scratch = scratch;
        if matches!(result, Err(Error::Io(_))) {
            self.stream.poison();
        }
        result
    }

    fn matrix_read(&self) -> Result<(&TiledMatrix, &VarlitStreamWriter)> {
        Ok((
            self.tiles.as_ref().ok_or(Error::MatrixLayoutMissing)?,
            &self.stream,
        ))
    }

    /// Stores `value` as `key`'s cell, uncommitted until
    /// [`Self::commit_matrix_cell`]; durable and visible at the next
    /// [`Self::sync`].
    pub fn write_matrix_cell<T: VarlitMatrixBlock>(
        &mut self,
        key: MatrixKey,
        value: &T,
    ) -> Result<()> {
        let spec = self.stream.spec();
        self.matrix_mutation(|tiles, store, scratch| {
            host::write_cell(spec, tiles, store, key, value, scratch)
        })
    }

    pub fn commit_matrix_cell<T: VarlitMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        crate::matrix::ensure_matrix_block::<T>(self.stream.spec())?;
        self.matrix_mutation(|tiles, store, _| tiles.commit_cell(store, T::ID, key))
    }

    pub fn clear_matrix_cell<T: VarlitMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        crate::matrix::ensure_matrix_block::<T>(self.stream.spec())?;
        self.matrix_mutation(|tiles, store, _| tiles.clear_cell(store, T::ID, key))
    }

    pub fn matrix_cell_status<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        let (tiles, stream) = self.matrix_read()?;
        host::cell_status::<T>(stream.spec(), tiles, &WriterSource(stream), key)
    }

    pub fn read_matrix_cell<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
        out: &mut T,
    ) -> Result<()> {
        let (tiles, stream) = self.matrix_read()?;
        host::read_cell(stream.spec(), tiles, &WriterSource(stream), key, out)
    }

    /// See [`crate::VarlitFile::read_matrix_cells_into`].
    pub fn read_matrix_cells_into<T: VarlitMatrixBlock>(
        &self,
        keys: &[MatrixKey],
        out: &mut [T],
    ) -> Result<()> {
        let (tiles, stream) = self.matrix_read()?;
        host::read_cells(stream.spec(), tiles, &WriterSource(stream), keys, out)
    }

    /// See [`crate::VarlitFile::read_matrix_region_into`].
    pub fn read_matrix_region_into<T: VarlitMatrixBlock>(
        &self,
        region: MatrixRegion,
        out: &mut [T],
    ) -> Result<MatrixPage> {
        let (tiles, stream) = self.matrix_read()?;
        host::read_region(
            stream.spec(),
            tiles,
            &WriterSource(stream),
            region,
            region.first(),
            out,
        )
    }

    /// See [`crate::VarlitFile::read_matrix_region_from`].
    pub fn read_matrix_region_from<T: VarlitMatrixBlock>(
        &self,
        cursor: MatrixCursor,
        out: &mut [T],
    ) -> Result<MatrixPage> {
        let (tiles, stream) = self.matrix_read()?;
        host::read_region(
            stream.spec(),
            tiles,
            &WriterSource(stream),
            cursor.region(),
            cursor.next_key(),
            out,
        )
    }

    /// See [`crate::VarlitFile::read_matrix_row_into`].
    pub fn read_matrix_row_into<T: VarlitMatrixBlock>(
        &self,
        row: u64,
        out: &mut [T],
    ) -> Result<()> {
        let (tiles, stream) = self.matrix_read()?;
        host::read_row(stream.spec(), tiles, &WriterSource(stream), row, out)
    }
}

/// A writer's reads of its own primary.
struct WriterSource<'a>(&'a VarlitStreamWriter);

impl TileSource for WriterSource<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_logical(offset, buf)
    }
    fn len(&self) -> Result<u64> {
        Ok(self.0.snapshot().len())
    }
}

impl VarlitIndexedReader {
    fn matrix(&self) -> Result<&TiledMatrix> {
        self.tiles.as_ref().ok_or(Error::MatrixLayoutMissing)
    }

    /// The matrix generation this reader shows; [`Self::follow`] moves it.
    pub fn matrix_generation(&self) -> Result<u64> {
        Ok(self.matrix()?.generation())
    }

    pub fn matrix_cell_status<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        host::cell_status::<T>(
            self.stream.spec(),
            self.matrix()?,
            self.stream.snapshot(),
            key,
        )
    }

    pub fn read_matrix_cell<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
        out: &mut T,
    ) -> Result<()> {
        host::read_cell(
            self.stream.spec(),
            self.matrix()?,
            self.stream.snapshot(),
            key,
            out,
        )
    }

    /// See [`crate::VarlitFile::read_matrix_cells_into`].
    pub fn read_matrix_cells_into<T: VarlitMatrixBlock>(
        &self,
        keys: &[MatrixKey],
        out: &mut [T],
    ) -> Result<()> {
        host::read_cells(
            self.stream.spec(),
            self.matrix()?,
            self.stream.snapshot(),
            keys,
            out,
        )
    }

    /// See [`crate::VarlitFile::read_matrix_region_into`].
    pub fn read_matrix_region_into<T: VarlitMatrixBlock>(
        &self,
        region: MatrixRegion,
        out: &mut [T],
    ) -> Result<MatrixPage> {
        host::read_region(
            self.stream.spec(),
            self.matrix()?,
            self.stream.snapshot(),
            region,
            region.first(),
            out,
        )
    }

    /// See [`crate::VarlitFile::read_matrix_region_from`].
    pub fn read_matrix_region_from<T: VarlitMatrixBlock>(
        &self,
        cursor: MatrixCursor,
        out: &mut [T],
    ) -> Result<MatrixPage> {
        host::read_region(
            self.stream.spec(),
            self.matrix()?,
            self.stream.snapshot(),
            cursor.region(),
            cursor.next_key(),
            out,
        )
    }

    /// See [`crate::VarlitFile::read_matrix_row_into`].
    pub fn read_matrix_row_into<T: VarlitMatrixBlock>(
        &self,
        row: u64,
        out: &mut [T],
    ) -> Result<()> {
        host::read_row(
            self.stream.spec(),
            self.matrix()?,
            self.stream.snapshot(),
            row,
            out,
        )
    }
}
