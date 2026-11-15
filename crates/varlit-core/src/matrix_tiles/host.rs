//! Typed matrix access over any host of a tiled matrix: a native file, or an
//! indexed (high-cardinality) primary. A host hands over its spec, its
//! [`TiledMatrix`] and the source the matrix reads through; everything typed
//! (encoding a slot, decoding cells within the read limits, batching) is here
//! once.

use super::api::TiledMatrix;
use super::store::{TileSource, TileStore};
use crate::format::ReadLimitKey;
use crate::{
    Error, FormatSpec, MatrixCellStatus, MatrixCursor, MatrixKey, MatrixPage, MatrixRegion, Result,
    VarlitMatrixBlock,
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

fn zeroed(len: usize, resource: &'static str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested: len as u64,
        })?;
    bytes.resize(len, 0);
    Ok(bytes)
}

/// Encodes `value` into exactly `T::SLOT_STRIDE` bytes, using `scratch`.
pub(crate) fn encode_slot<T: VarlitMatrixBlock>(
    spec: FormatSpec,
    value: &T,
    scratch: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    match crate::codec::encode_into_limited(
        value,
        T::ENDIAN.unwrap_or(spec.endian),
        T::SLOT_STRIDE.saturating_add(1),
        "matrix slot payload",
        scratch,
    ) {
        Ok(()) if scratch.len() as u64 == T::SLOT_STRIDE => {
            let mut out = zeroed(0, "matrix slot payload")?;
            out.try_reserve_exact(scratch.len())
                .map_err(|_| Error::AllocationFailed {
                    resource: "matrix slot payload",
                    requested: scratch.len() as u64,
                })?;
            out.extend_from_slice(scratch);
            Ok(out)
        }
        Ok(()) => Err(Error::MatrixSizeMismatch {
            expected: T::SLOT_STRIDE,
            actual: scratch.len() as u64,
        }),
        Err(Error::LimitExceeded { actual, .. }) => Err(Error::MatrixSizeMismatch {
            expected: T::SLOT_STRIDE,
            actual,
        }),
        Err(error) => Err(error),
    }
}

/// Stores `value` as `key`'s slot, uncommitted.
pub(crate) fn write_cell<T: VarlitMatrixBlock>(
    spec: FormatSpec,
    tiles: &mut TiledMatrix,
    store: &mut dyn TileStore,
    key: MatrixKey,
    value: &T,
    scratch: &mut Vec<u8>,
) -> Result<()> {
    stride::<T>(spec)?;
    let payload = encode_slot(spec, value, scratch)?;
    tiles.write_cell(store, T::ID, key, &payload)
}

pub(crate) fn cell_status<T: VarlitMatrixBlock>(
    spec: FormatSpec,
    tiles: &TiledMatrix,
    source: &dyn TileSource,
    key: MatrixKey,
) -> Result<MatrixCellStatus> {
    crate::matrix::ensure_matrix_block::<T>(spec)?;
    tiles.cell_status(source, T::ID, key)
}

/// Decodes one committed cell into `out`.
pub(crate) fn read_cell<T: VarlitMatrixBlock>(
    spec: FormatSpec,
    tiles: &TiledMatrix,
    source: &dyn TileSource,
    key: MatrixKey,
    out: &mut T,
) -> Result<()> {
    let stride = stride::<T>(spec)?;
    spec.read_limits
        .check(ReadLimitKey::MaterializedBytes, stride as u64)?;
    tiles.with_payloads(stride, |payload| {
        tiles.read_cell(source, T::ID, key, payload)?;
        let limit = spec
            .read_limits
            .require(ReadLimitKey::MaterializedBytes)?
            .unwrap_or(u64::MAX)
            - stride as u64;
        crate::codec::Decoder::decode_from_slice_limited_into(
            payload,
            T::ENDIAN.unwrap_or(spec.endian),
            limit,
            out,
        )
    })
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

/// Reads one whole row (`out.len()` must equal dimension 1).
pub(crate) fn read_row<T: VarlitMatrixBlock>(
    spec: FormatSpec,
    tiles: &TiledMatrix,
    source: &dyn TileSource,
    row: u64,
    out: &mut [T],
) -> Result<()> {
    let width = tiles.geom.dims[1];
    if out.len() as u64 != width {
        return Err(Error::MatrixSizeMismatch {
            expected: width,
            actual: out.len() as u64,
        });
    }
    let page = read_region(
        spec,
        tiles,
        source,
        MatrixRegion::row(row, 0..width),
        MatrixKey::new(row, 0),
        out,
    )?;
    debug_assert!(page.next.is_none());
    Ok(())
}
