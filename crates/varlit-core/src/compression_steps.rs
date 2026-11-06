//! Allocation-free framing decisions shared by decoding and symbolic checks.

/// Every accepted backend step consumes input or produces bounded output.
/// Thus even a backend that returns a perpetual nonzero hint cannot make this
/// wrapper spin without exhausting one of the two finite extents.
#[cfg(any(feature = "integrity", kani))]
#[cfg_attr(kani, kani::ensures(|result: &Option<u64>| {
    let total = decoded as u128 + written as u128;
    let admitted = before <= after && after <= input_len
        && total <= expected as u128 && (after > before || written != 0);
    match result {
        Some(next) => admitted && *next as u128 == total,
        None => !admitted,
    }
}))]
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

#[cfg_attr(kani, kani::ensures(|result: &Option<usize>| match declared {
    Some(n) if n > bound || n > maximum as u64 => result.is_none(),
    Some(n) => *result == Some(n as usize),
    None => *result == Some((bound as u128).min(maximum as u128) as usize),
}))]
pub(super) fn output_capacity(declared: Option<u64>, bound: u64, maximum: usize) -> Option<usize> {
    match declared {
        Some(actual) if actual > bound || actual > maximum as u64 => None,
        Some(actual) => Some(actual as usize),
        None => Some(usize::try_from(bound).unwrap_or(usize::MAX).min(maximum)),
    }
}

/// One complete modern zstd block; success strictly advances within the input.
/// Payload contents are checked later by zstd. `header` is the wire u24.
#[cfg_attr(kani, kani::ensures(|result: &Option<(usize, u64)>| {
    let size = (header >> 3) as u128;
    let kind = (header >> 1) & 3;
    let end = offset as u128 + 3 + if kind == 1 { 1 } else { size };
    let admitted = size <= 131072 && kind != 3 && end <= len as u128;
    match result {
        Some((next, output)) => admitted && *next as u128 == end
            && *output as u128 == if kind == 2 { 131072 } else { size },
        None => !admitted,
    }
}))]
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

#[cfg(kani)]
#[path = "verification/compression_contracts.rs"]
mod contract_proofs;
