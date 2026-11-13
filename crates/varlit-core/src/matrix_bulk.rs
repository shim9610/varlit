//! Bounded matrix axis reads. Dimension 1 is contiguous; dimension 0 is strided.
use super::*;

/// Cells decoded per batch of a typed read: the payload scratch is at most
/// this many cells, or 64 KiB, whichever is fewer.
const RUN_CELLS: usize = 1024;
const RUN_BYTES: usize = 64 * 1024;

impl VarlitFile {
    fn matrix_stride<T: VarlitMatrixBlock>(&self) -> Result<usize> {
        crate::matrix::ensure_matrix_block::<T>(self.spec)?;
        usize::try_from(T::SLOT_STRIDE)
            .ok()
            .filter(|stride| *stride != 0)
            .ok_or(Error::InvalidMatrixLayout)
    }

    /// Width of dimension 1, the contiguous one.
    fn matrix_width(&self) -> Result<u64> {
        Ok(self.tiles_read()?.0.geom.dims[1])
    }

    /// Reads `out.len() / stride` committed cells in storage order from
    /// `start` (row-major, crossing rows) into `out`.
    fn read_run_payloads<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        let stride = self.matrix_stride::<T>()?;
        let width = self.matrix_width()?;
        let (tiles, source) = self.tiles_read()?;
        let mut key = start;
        let mut done = 0usize;
        while done < out.len() {
            let in_row = ((width - key.ch.min(width)) as usize)
                .min((out.len() - done) / stride)
                .max(1);
            tiles.read_row_run(&source, T::ID, key, &mut out[done..done + in_row * stride])?;
            done += in_row * stride;
            key = MatrixKey::new(key.scan + 1, 0);
        }
        Ok(())
    }

    /// Reads `out.len() / stride` committed cells down dimension 0 from
    /// `start` into `out`.
    fn read_column_payloads<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        let stride = self.matrix_stride::<T>()?;
        let (tiles, source) = self.tiles_read()?;
        for (i, cell) in out.chunks_exact_mut(stride).enumerate() {
            tiles.read_cell(
                &source,
                T::ID,
                MatrixKey::new(start.scan + i as u64, start.ch),
                cell,
            )?;
        }
        Ok(())
    }

    /// Typed reads through a bounded payload scratch: `fill` puts the
    /// payloads of `count` cells starting at cell `done` into the scratch.
    fn decode_cells<T: VarlitMatrixBlock>(
        &self,
        out: &mut [T],
        fill: &dyn Fn(usize, &mut [u8]) -> Result<()>,
    ) -> Result<()> {
        let stride = self.matrix_stride::<T>()?;
        if out.is_empty() {
            return Ok(());
        }
        let batch = RUN_CELLS.min((RUN_BYTES / stride).max(1)).min(out.len());
        let scratch_len = batch * stride;
        let inline = (out.len() as u64)
            .checked_mul(size_of::<T>() as u64)
            .and_then(|n| n.checked_add(scratch_len as u64))
            .ok_or(Error::InvalidMatrixLayout)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::MaterializedBytes, inline)?;
        let mut remaining = self
            .spec
            .read_limits
            .require(ReadLimitKey::MaterializedBytes)?
            .unwrap_or(u64::MAX)
            - inline;
        let mut scratch = try_zeroed_vec(scratch_len as u64, "matrix run payload")?;
        let mut done = 0;
        while done < out.len() {
            let count = batch.min(out.len() - done);
            let bytes = &mut scratch[..count * stride];
            fill(done, bytes)?;
            for (target, payload) in out[done..done + count]
                .iter_mut()
                .zip(bytes.chunks_exact(stride))
            {
                *target = crate::codec::Decoder::decode_from_slice_accounted(
                    payload,
                    T::ENDIAN.unwrap_or(self.spec.endian),
                    &mut remaining,
                )?;
            }
            done += count;
        }
        Ok(())
    }

    fn check_bytes_output<T: VarlitMatrixBlock>(&self, out: &[u8]) -> Result<()> {
        let stride = self.matrix_stride::<T>()?;
        if !out.len().is_multiple_of(stride) {
            return Err(Error::InvalidMatrixLayout);
        }
        self.spec
            .read_limits
            .check(ReadLimitKey::MaterializedBytes, out.len() as u64)
    }

    /// The cell `offset` cells after `start` in storage order.
    fn run_key(start: MatrixKey, offset: u64, width: u64) -> MatrixKey {
        let linear = start.ch + offset;
        MatrixKey::new(start.scan + linear / width, linear % width)
    }

    /// Read a segment along dimension 0, holding dimension 1 fixed.
    ///
    /// Output item `i` is `(start.scan + i, start.ch)`. Only selected cells
    /// must be committed. Output plus at most 64 KiB of payload scratch are
    /// charged to `MaterializedBytes`. On error discard the output. Reads
    /// keep the captured generation until an explicit `follow`.
    pub fn read_matrix_column_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [T],
    ) -> Result<()> {
        self.decode_cells(out, &|done, bytes| {
            self.read_column_payloads::<T>(
                MatrixKey::new(start.scan + done as u64, start.ch),
                bytes,
            )
        })
    }

    /// Encoded counterpart of [`Self::read_matrix_column_into`]. `out.len()`
    /// must be a multiple of `T::SLOT_STRIDE`.
    pub fn read_matrix_column_bytes_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        self.check_bytes_output::<T>(out)?;
        self.read_column_payloads::<T>(start, out)
    }

    /// Read consecutive cells in the declared storage order into
    /// caller-owned values. Runs cross rows. All requested cells must be
    /// committed. On error, a prefix of `out` may have changed.
    pub fn read_matrix_run_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [T],
    ) -> Result<()> {
        let width = self.matrix_width()?;
        self.tiles_read()?.0.geom.check_key(start)?;
        self.decode_cells(out, &|done, bytes| {
            self.read_run_payloads::<T>(Self::run_key(start, done as u64, width), bytes)
        })
    }

    /// Read one complete row of the declared storage order. `out.len()` must
    /// equal dimension 1.
    pub fn read_matrix_row_into<T: VarlitMatrixBlock>(
        &self,
        row: u64,
        out: &mut [T],
    ) -> Result<()> {
        let width = self.matrix_width()?;
        if out.len() as u64 != width {
            return Err(Error::MatrixSizeMismatch {
                expected: width,
                actual: out.len() as u64,
            });
        }
        self.read_matrix_run_into(MatrixKey::new(row, 0), out)
    }

    /// Read validated encoded cells in storage order into caller-owned
    /// bytes. `out.len()` must be a multiple of `T::SLOT_STRIDE`.
    pub fn read_matrix_run_bytes_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        self.check_bytes_output::<T>(out)?;
        self.tiles_read()?.0.geom.check_key(start)?;
        self.read_run_payloads::<T>(start, out)
    }

    /// Reads committed cells of a region into caller-owned values,
    /// row-major, as many as `out` holds. The returned page says how many
    /// were filled and, when the region goes on, the cursor to continue
    /// from with [`Self::read_matrix_region_from`]. A row is
    /// `MatrixRegion::row`, a column `MatrixRegion::column`.
    ///
    /// Every cell must be committed. The whole page is one snapshot: a
    /// reader validates it once, not once per cell. On error the output may
    /// hold a prefix and must be discarded. `out` and one tile row of scratch
    /// are charged to `MaterializedBytes`, decoded allocations on top.
    pub fn read_matrix_region_into<T: VarlitMatrixBlock>(
        &self,
        region: MatrixRegion,
        out: &mut [T],
    ) -> Result<MatrixPage> {
        self.read_region_values(region, region.first(), out)
    }

    /// Continues a region read where `cursor` stopped.
    pub fn read_matrix_region_from<T: VarlitMatrixBlock>(
        &self,
        cursor: MatrixCursor,
        out: &mut [T],
    ) -> Result<MatrixPage> {
        self.read_region_values(cursor.region(), cursor.next_key(), out)
    }

    /// Encoded counterpart of [`Self::read_matrix_region_into`]: `out`
    /// receives whole `T::SLOT_STRIDE` payloads, and `filled` counts cells.
    pub fn read_matrix_region_bytes_into<T: VarlitMatrixBlock>(
        &self,
        region: MatrixRegion,
        out: &mut [u8],
    ) -> Result<MatrixPage> {
        self.read_region_payloads::<T>(region, region.first(), out)
    }

    /// Continues an encoded region read where `cursor` stopped.
    pub fn read_matrix_region_bytes_from<T: VarlitMatrixBlock>(
        &self,
        cursor: MatrixCursor,
        out: &mut [u8],
    ) -> Result<MatrixPage> {
        self.read_region_payloads::<T>(cursor.region(), cursor.next_key(), out)
    }

    /// Reads the committed cells at `keys` into caller-owned values, item
    /// `i` from `keys[i]`. Keys may come in any order and repeat; this is
    /// for picking a few cells out of wide rows (a handful of channels of
    /// the latest row) without reading the rows whole. `out.len()` must
    /// equal `keys.len()`.
    ///
    /// Keys are read grouped by tile: one bitmap read per tile, one read
    /// per span of nearby slots. Every key must be in bounds and committed.
    /// Keys are read through a bounded payload scratch, up to 64 KiB of
    /// payloads at a time; each batch is one snapshot validation. `out`,
    /// the scratch and 32 bytes per batch key are charged to
    /// `MaterializedBytes`. On error discard the output.
    pub fn read_matrix_cells_into<T: VarlitMatrixBlock>(
        &self,
        keys: &[MatrixKey],
        out: &mut [T],
    ) -> Result<()> {
        let (tiles, source) = self.tiles_read()?;
        crate::matrix_tiles::host::read_cells(self.spec, tiles, &source, keys, out)
    }

    /// Encoded counterpart of [`Self::read_matrix_cells_into`]: `out`
    /// receives `keys.len()` whole `T::SLOT_STRIDE` payloads, in key order,
    /// read as one snapshot. `out` and 32 bytes per key are charged to
    /// `MaterializedBytes`.
    pub fn read_matrix_cells_bytes_into<T: VarlitMatrixBlock>(
        &self,
        keys: &[MatrixKey],
        out: &mut [u8],
    ) -> Result<()> {
        let (tiles, source) = self.tiles_read()?;
        crate::matrix_tiles::host::read_cells_bytes::<T>(self.spec, tiles, &source, keys, out)
    }

    fn read_region_payloads<T: VarlitMatrixBlock>(
        &self,
        region: MatrixRegion,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<MatrixPage> {
        let (tiles, source) = self.tiles_read()?;
        crate::matrix_tiles::host::read_region_bytes::<T>(
            self.spec, tiles, &source, region, start, out,
        )
    }

    fn read_region_values<T: VarlitMatrixBlock>(
        &self,
        region: MatrixRegion,
        start: MatrixKey,
        out: &mut [T],
    ) -> Result<MatrixPage> {
        let (tiles, source) = self.tiles_read()?;
        crate::matrix_tiles::host::read_region(self.spec, tiles, &source, region, start, out)
    }
}
