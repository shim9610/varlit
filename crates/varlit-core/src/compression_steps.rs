//! Allocation-free framing decisions shared by decoding and symbolic checks.

/// Every accepted backend step consumes input or produces bounded output.
/// Thus even a backend that returns a perpetual nonzero hint cannot make this
/// wrapper spin without exhausting one of the two finite extents.
#[cfg(any(feature = "integrity", kani))]
pub(super) fn stream_progress(
    before: usize,
    after: usize,
    input_len: usize,
    decoded: u64,
    written: usize,
    expected: u64,
) -> Option<u64> {
    let next = decoded.checked_add(written as u64)?;
    (before <= after && after <= input_len && next <= expected && (after > before || written != 0))
        .then_some(next)
}

pub(super) fn output_capacity(declared: Option<u64>, bound: u64, maximum: usize) -> Option<usize> {
    match declared {
        Some(actual) if actual > bound || actual > maximum as u64 => None,
        Some(actual) => Some(actual as usize),
        None => Some(usize::try_from(bound).unwrap_or(usize::MAX).min(maximum)),
    }
}

/// One complete modern zstd block; success strictly advances within the input.
/// Payload contents are checked later by zstd. `header` is the wire u24.
pub(super) fn block_extent(offset: usize, header: u32, len: usize) -> Option<(usize, u64)> {
    let size = (header >> 3) as usize;
    if size > 128 * 1024 {
        return None;
    }
    let (stored, regenerated) = match (header >> 1) & 3 {
        0 => (size, size),
        1 => (1, size),
        2 => (size, 128 * 1024),
        _ => return None,
    };
    let next = offset.checked_add(3)?.checked_add(stored)?;
    (next <= len).then_some((next, regenerated as u64))
}

#[cfg(kani)]
#[path = "verification/compression.rs"]
mod proofs;
