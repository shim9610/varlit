//! Typed matrix access over any host of a tiled matrix: a native file, or an
//! indexed (high-cardinality) primary. A host hands over its spec, its
//! [`TiledMatrix`] and the source the matrix reads through; everything typed
//! (encoding a slot, decoding cells within the read limits, batching) is here
//! once.

use super::api::TiledMatrix;
use super::store::TileSource;
use crate::format::ReadLimitKey;
use crate::{
    Error, FormatSpec, MatrixCursor, MatrixKey, MatrixPage, MatrixRegion, Result, VarlitMatrixBlock,
};

/// Cells decoded per batch of a typed read: the payload scratch is at most
/// this many cells, or 64 KiB, whichever is fewer.
use super::api::batch_cells;
/// What a batch key read's sort order costs per key.
const KEY_ORDER_BYTES: u64 = 32;

pub(crate) fn stride<T: VarlitMatrixBlock>(spec: FormatSpec) -> Result<usize> {
    crate::matrix::ensure_matrix_block::<T>(spec)?;
    usize::try_from(T::SLOT_STRIDE)
        .ok()
        .filter(|stride| *stride != 0)
        .ok_or(Error::InvalidMatrixLayout)
}

/// The decode budget left once `inline` bytes are charged.
fn decode_budget(spec: FormatSpec, inline: u64) -> Result<u64> {
    spec.read_limits
        .check(ReadLimitKey::MaterializedBytes, inline)?;
    Ok(spec
        .read_limits
        .require(ReadLimitKey::MaterializedBytes)?
        .unwrap_or(u64::MAX)
        - inline)
}

/// Reads the committed cells at `keys` into `out`, item `i` from `keys[i]`,
/// a bounded batch (one snapshot) at a time.
pub(crate) fn read_cells<T: VarlitMatrixBlock>(
    spec: FormatSpec,
    tiles: &TiledMatrix,
    source: &dyn TileSource,
    keys: &[MatrixKey],
    out: &mut [T],
) -> Result<()> {
    if out.len() != keys.len() {
        return Err(Error::MatrixSizeMismatch {
            expected: keys.len() as u64,
            actual: out.len() as u64,
        });
    }
    let stride = stride::<T>(spec)?;
    let batch = batch_cells(stride, keys.len());
    let inline = (out.len() as u64)
        .checked_mul(size_of::<T>() as u64)
        .and_then(|n| n.checked_add((batch * stride) as u64))
        .and_then(|n| n.checked_add(batch as u64 * KEY_ORDER_BYTES))
        .ok_or(Error::InvalidMatrixLayout)?;
    let mut remaining = decode_budget(spec, inline)?;
    tiles.with_payloads(batch * stride, |scratch| {
        let mut done = 0;
        while done < keys.len() {
            let count = batch.min(keys.len() - done);
            let bytes = &mut scratch[..count * stride];
            tiles.read_cells(source, T::ID, &keys[done..done + count], bytes)?;
            for (target, payload) in out[done..done + count]
                .iter_mut()
                .zip(bytes.chunks_exact(stride))
            {
                *target = crate::codec::Decoder::decode_from_slice_accounted(
                    payload,
                    T::ENDIAN.unwrap_or(spec.endian),
                    &mut remaining,
                )?;
            }
            done += count;
        }
        Ok(())
    })
}

/// Encoded counterpart of [`read_cells`]: one snapshot for the whole call.
pub(crate) fn read_cells_bytes<T: VarlitMatrixBlock>(
    spec: FormatSpec,
    tiles: &TiledMatrix,
    source: &dyn TileSource,
    keys: &[MatrixKey],
    out: &mut [u8],
) -> Result<()> {
    let stride = stride::<T>(spec)?;
    if !out.len().is_multiple_of(stride) {
        return Err(Error::InvalidMatrixLayout);
    }
    let order = (keys.len() as u64).saturating_mul(KEY_ORDER_BYTES);
    spec.read_limits.check(
        ReadLimitKey::MaterializedBytes,
        (out.len() as u64).saturating_add(order),
    )?;
    tiles.read_cells(source, T::ID, keys, out)
}

/// Reads committed cells of `region` row-major from `start` into `out`,
/// decoded a bounded batch (one snapshot) at a time.
pub(crate) fn read_region<T: VarlitMatrixBlock>(
    spec: FormatSpec,
    tiles: &TiledMatrix,
    source: &dyn TileSource,
    region: MatrixRegion,
    start: MatrixKey,
    out: &mut [T],
) -> Result<MatrixPage> {
    let stride = stride::<T>(spec)?;
    let batch = batch_cells(stride, out.len());
    let inline = (out.len() as u64)
        .checked_mul(size_of::<T>() as u64)
        .and_then(|n| n.checked_add((batch * stride) as u64))
        .ok_or(Error::InvalidMatrixLayout)?;
    let mut remaining = decode_budget(spec, inline)?;
    tiles.with_payloads(batch * stride, |scratch| {
        let mut done = 0usize;
        let mut next = Some(start);
        while done < out.len() {
            let Some(at) = next else { break };
            let want = batch.min(out.len() - done);
            let bytes = &mut scratch[..want * stride];
            let (filled, after) = tiles.read_region(source, T::ID, region, at, bytes)?;
            for (target, payload) in out[done..done + filled]
                .iter_mut()
                .zip(bytes.chunks_exact(stride))
            {
                *target = crate::codec::Decoder::decode_from_slice_accounted(
                    payload,
                    T::ENDIAN.unwrap_or(spec.endian),
                    &mut remaining,
                )?;
            }
            done += filled;
            next = after;
        }
        if out.is_empty() {
            // Nothing asked for: validate the region, report where it starts.
            tiles.read_region(source, T::ID, region, start, &mut [])?;
        }
        Ok(MatrixPage {
            filled: done,
            next: next.map(|key| MatrixCursor::new(region, key)),
        })
    })
}

/// Encoded counterpart of [`read_region`]: one snapshot for the page.
pub(crate) fn read_region_bytes<T: VarlitMatrixBlock>(
    spec: FormatSpec,
    tiles: &TiledMatrix,
    source: &dyn TileSource,
    region: MatrixRegion,
    start: MatrixKey,
    out: &mut [u8],
) -> Result<MatrixPage> {
    let stride = stride::<T>(spec)?;
    if !out.len().is_multiple_of(stride) {
        return Err(Error::InvalidMatrixLayout);
    }
    spec.read_limits
        .check(ReadLimitKey::MaterializedBytes, out.len() as u64)?;
    let (filled, next) = tiles.read_region(source, T::ID, region, start, out)?;
    Ok(MatrixPage {
        filled,
        next: next.map(|key| MatrixCursor::new(region, key)),
    })
}
