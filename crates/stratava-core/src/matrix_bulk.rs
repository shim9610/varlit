//! Contiguous matrix reads. Dimension 1 is the fast axis; no scalar-read loop.
use super::*;

const RUN_CELLS: usize = 1024;
const RUN_BYTES: usize = 64 * 1024;

struct RunPlan {
    first: u64,
    width: u64,
    stride: usize,
    cells_per_region: u64,
}

impl StratavaFile {
    fn matrix_run_plan<T: StratavaMatrixBlock>(
        &self,
        start: MatrixKey,
        count: usize,
    ) -> Result<RunPlan> {
        self.ensure_chunk_block_access::<T>()?;
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        let width = matrix
            .dimension(T::DIMENSIONS[1])
            .ok_or(Error::InvalidMatrixLayout)?;
        let rows = matrix
            .dimension(T::DIMENSIONS[0])
            .ok_or(Error::InvalidMatrixLayout)?;
        let stride = usize::try_from(T::SLOT_STRIDE).map_err(|_| Error::InvalidMatrixLayout)?;
        if width == 0 || stride == 0 {
            return Err(Error::InvalidMatrixLayout);
        }
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, T::SLOT_STRIDE)?;
        let cells_per_region = rows.checked_mul(width).ok_or(Error::InvalidMatrixLayout)?;
        if count == 0 {
            return Ok(RunPlan {
                first: 0,
                width,
                stride,
                cells_per_region,
            });
        }
        let bad_key = || Error::MatrixKeyOutOfBounds {
            scan: start.scan,
            ch: start.ch,
        };
        if start.ch >= width {
            return Err(bad_key());
        }
        let first = start
            .scan
            .checked_mul(width)
            .and_then(|v| v.checked_add(start.ch))
            .ok_or_else(bad_key)?;
        let end = first.checked_add(count as u64).ok_or_else(bad_key)?;
        if self.spec.growing_rows_per_chunk().is_none() && end > cells_per_region {
            return Err(bad_key());
        }
        Ok(RunPlan {
            first,
            width,
            stride,
            cells_per_region,
        })
    }

    fn matrix_run_compression_workspace(&self, plan: &RunPlan, count: usize) -> Result<u64> {
        let end = plan
            .first
            .checked_add(count as u64)
            .ok_or(Error::InvalidMatrixLayout)?;
        if count == 0 || end <= plan.cells_per_region || self.spec.chunk_compression.is_none() {
            return Ok(0);
        }
        // At most one stored and one decoded sub-block coexist. The decoder
        // rejects stored extents larger than their canonical raw extent.
        compressed_cell_width(
            plan.stride as u64,
            !matches!(self.spec.integrity_policy, IntegrityPolicy::None),
        )
        .max(MATRIX_CHUNK_SUB_BLOCK_BYTES)
        .checked_mul(2)
        .ok_or(Error::InvalidMatrixChunk)
    }

    /// Read consecutive cells in the declared storage order into caller-owned values.
    ///
    /// For `dims = [row, col]`, columns are contiguous, then the next row follows.
    /// Declare `dims = [col, row]` to favor column scans instead. `MatrixKey` always
    /// names dimension 0 then dimension 1. Growing dimensions must be dimension 0.
    /// Runs can cross rows and growing chunks. Each bounded batch reads payload,
    /// commit bits and checksums together; this does not call the scalar cell API.
    ///
    /// All requested cells must be committed. On error, a prefix of `out` may have
    /// changed; discard the requested result. Empty output validates the type only.
    /// Decode allocations are charged cumulatively, plus the output's inline size
    /// and one reusable payload buffer (at most 64 KiB, or one wider cell).
    /// The reader stays on its captured generation until explicit `follow`.
    pub fn read_matrix_run_into<T: StratavaMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [T],
    ) -> Result<()> {
        let plan = self.matrix_run_plan::<T>(start, out.len())?;
        if out.is_empty() {
            return Ok(());
        }
        let batch = RUN_CELLS
            .min((RUN_BYTES / plan.stride).max(1))
            .min(out.len());
        let scratch_len = batch
            .checked_mul(plan.stride)
            .ok_or(Error::InvalidMatrixLayout)?;
        let compression = self.matrix_run_compression_workspace(&plan, out.len())?;
        let inline = (out.len() as u64)
            .checked_mul(size_of::<T>() as u64)
            .and_then(|n| n.checked_add(scratch_len as u64))
            .and_then(|n| n.checked_add(compression))
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
        let mut at = plan.first;
        let mut done = 0;
        while done < out.len() {
            let count = batch.min(out.len() - done).min(
                usize::try_from(plan.cells_per_region - at % plan.cells_per_region)
                    .unwrap_or(usize::MAX),
            );
            let bytes = &mut scratch[..count * plan.stride];
            self.read_matrix_run_batch::<T>(&plan, at, bytes)?;
            for (target, payload) in out[done..done + count]
                .iter_mut()
                .zip(bytes.chunks_exact(plan.stride))
            {
                *target = crate::codec::Decoder::decode_from_slice_accounted(
                    payload,
                    T::ENDIAN.unwrap_or(self.spec.endian),
                    &mut remaining,
                )?;
            }
            done += count;
            at += count as u64;
        }
        Ok(())
    }

    /// Read one complete row of the declared storage order. `out.len()` must
    /// equal dimension 1. With `dims = [col, row]` this is a physical column.
    pub fn read_matrix_row_into<T: StratavaMatrixBlock>(
        &self,
        row: u64,
        out: &mut [T],
    ) -> Result<()> {
        let plan = self.matrix_run_plan::<T>(MatrixKey::new(row, 0), out.len())?;
        if out.len() as u64 != plan.width {
            return Err(Error::MatrixSizeMismatch {
                expected: plan.width,
                actual: out.len() as u64,
            });
        }
        self.read_matrix_run_into(MatrixKey::new(row, 0), out)
    }

    /// Read validated encoded cells in storage order into caller-owned bytes.
    /// `out.len()` must be a multiple of `T::SLOT_STRIDE`. No typed decoding occurs.
    /// Range, visibility and checksum checks are the same as the typed run API.
    /// On failure the buffer is unspecified and must not be consumed. Memory for
    /// the supplied output is charged to `MaterializedBytes`; internal metadata
    /// scratch is fixed-size. Compressed chunks additionally need one sub-block.
    pub fn read_matrix_run_bytes_into<T: StratavaMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        let stride = usize::try_from(T::SLOT_STRIDE).map_err(|_| Error::InvalidMatrixLayout)?;
        if stride == 0 || !out.len().is_multiple_of(stride) {
            return Err(Error::InvalidMatrixLayout);
        }
        let plan = self.matrix_run_plan::<T>(start, out.len() / stride)?;
        let materialized = (out.len() as u64)
            .checked_add(self.matrix_run_compression_workspace(&plan, out.len() / stride)?)
            .ok_or(Error::InvalidMatrixLayout)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::MaterializedBytes, materialized)?;
        let mut at = plan.first;
        let mut done = 0;
        let batch = RUN_CELLS.min((RUN_BYTES / stride).max(1));
        while done < out.len() {
            let count = batch.min((out.len() - done) / stride).min(
                usize::try_from(plan.cells_per_region - at % plan.cells_per_region)
                    .unwrap_or(usize::MAX),
            );
            self.read_matrix_run_batch::<T>(&plan, at, &mut out[done..done + count * stride])?;
            done += count * stride;
            at += count as u64;
        }
        Ok(())
    }

    fn read_matrix_run_batch<T: StratavaMatrixBlock>(
        &self,
        plan: &RunPlan,
        at: u64,
        out: &mut [u8],
    ) -> Result<()> {
        let key = MatrixKey::new(at / plan.width, at % plan.width);
        let Some((chunk, local_row)) = self.chunk_for_row(key.scan) else {
            return crate::matrix::read_run_payload::<T>(
                self.spec,
                self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?,
                self.file.matrix_region_reader(),
                key,
                out,
            );
        };
        let count = out.len() / plan.stride;
        if let Some((block, ordinal)) = self.open_chunk_cell::<T>(key, chunk, local_row)? {
            require_committed(&block.commit, ordinal, count)?;
            let begin = usize::try_from(
                ordinal
                    .checked_mul(T::SLOT_STRIDE)
                    .ok_or(Error::InvalidMatrixChunk)?,
            )
            .map_err(|_| Error::InvalidMatrixChunk)?;
            out.copy_from_slice(
                block
                    .slots
                    .get(begin..begin + out.len())
                    .ok_or(Error::InvalidMatrixChunk)?,
            );
            return Ok(());
        }
        let Some((location, ordinal)) = self.locate_chunk_cell::<T>(key, chunk, local_row)? else {
            return Err(Error::MatrixNotCommitted);
        };
        let bit_byte = ordinal / 8;
        let bit_len = (ordinal % 8 + count as u64).div_ceil(8) as usize;
        let mut bits = [0u8; RUN_CELLS / 8 + 1];
        self.snapshot.read_exact_at(
            location
                .commit_offset
                .checked_add(bit_byte)
                .ok_or(Error::InvalidMatrixChunk)?,
            &mut bits[..bit_len],
        )?;
        note_chunk_bytes_read(bit_len as u64);
        require_committed(&bits[..bit_len], ordinal % 8, count)?;
        if location.compressed_slots {
            return self.read_compressed_matrix_run(location, ordinal, out);
        }
        let offset = location
            .slots_offset
            .checked_add(
                ordinal
                    .checked_mul(location.stride)
                    .ok_or(Error::InvalidMatrixChunk)?,
            )
            .ok_or(Error::InvalidMatrixChunk)?;
        self.snapshot.read_exact_at(offset, out)?;
        note_chunk_bytes_read(out.len() as u64);
        if let Some(base) = location.crc_offset {
            let mut crcs = [0u8; RUN_CELLS * 4];
            let offset = base
                .checked_add(ordinal.checked_mul(4).ok_or(Error::InvalidMatrixChunk)?)
                .ok_or(Error::InvalidMatrixChunk)?;
            self.snapshot
                .read_exact_at(offset, &mut crcs[..count * 4])?;
            note_chunk_bytes_read((count * 4) as u64);
            for (i, payload) in out.chunks_exact(plan.stride).enumerate() {
                let expected =
                    u32::from_le_bytes(crcs[i * 4..i * 4 + 4].try_into().expect("checksum"));
                let actual = crc32_bytes(payload)?;
                if expected != actual {
                    return Err(Error::MatrixChecksumMismatch {
                        offset: location.slots_offset + (ordinal + i as u64) * location.stride,
                        expected,
                        actual,
                    });
                }
            }
        }
        Ok(())
    }

    fn read_compressed_matrix_run(
        &self,
        location: ChunkBlockLocation,
        mut ordinal: u64,
        out: &mut [u8],
    ) -> Result<()> {
        let width = compressed_cell_width(location.stride, location.cell_crc);
        let per = cells_per_sub_block(width);
        let stride = usize::try_from(location.stride).map_err(|_| Error::InvalidMatrixChunk)?;
        let mut done = 0;
        while done < out.len() {
            let raw = self.read_compressed_chunk_sub_block(location, ordinal)?;
            let count = ((out.len() - done) / stride).min((per - ordinal % per) as usize);
            for i in 0..count {
                let at = usize::try_from((ordinal % per + i as u64) * width)
                    .map_err(|_| Error::InvalidMatrixChunk)?;
                let payload = raw.get(at..at + stride).ok_or(Error::InvalidMatrixChunk)?;
                if location.cell_crc {
                    let crc = raw
                        .get(at + stride..at + stride + 4)
                        .ok_or(Error::InvalidMatrixChunk)?;
                    let expected = u32::from_le_bytes(crc.try_into().expect("checksum"));
                    let actual = crc32_bytes(payload)?;
                    if expected != actual {
                        return Err(Error::MatrixChecksumMismatch {
                            offset: location.slots_offset,
                            expected,
                            actual,
                        });
                    }
                }
                out[done + i * stride..done + (i + 1) * stride].copy_from_slice(payload);
            }
            done += count * stride;
            ordinal += count as u64;
        }
        Ok(())
    }
}

fn require_committed(bits: &[u8], first: u64, count: usize) -> Result<()> {
    let end = first
        .checked_add(count as u64)
        .ok_or(Error::InvalidMatrixChunk)?;
    let mut at = first;
    while at < end {
        let take = (8 - at % 8).min(end - at);
        let mask = (((1u16 << take) - 1) << (at % 8)) as u8;
        let byte = *bits
            .get((at / 8) as usize)
            .ok_or(Error::InvalidMatrixChunk)?;
        if byte & mask != mask {
            return Err(Error::MatrixNotCommitted);
        }
        at += take;
    }
    Ok(())
}
