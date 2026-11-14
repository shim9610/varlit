//! Scalar transitions shared by decoder and collection budgets.
//! No allocation, I/O, callback or container-size bound is needed here.

#[cfg_attr(kani, kani::ensures(|result: &Option<u64>| match result {
    Some(next) => request <= remaining && *next == remaining - request,
    None => request > remaining,
}))]
#[inline(always)]
pub(crate) fn charge(remaining: u64, request: u64) -> Option<u64> {
    remaining.checked_sub(request)
}

#[cfg_attr(kani, kani::ensures(|result: &Option<usize>| {
    let end = position as u128 + length as u128;
    match result {
        Some(next) => end <= available as u128 && *next as u128 == end,
        None => end > available as u128,
    }
}))]
#[inline(always)]
pub(crate) fn read_end(position: usize, length: usize, available: usize) -> Option<usize> {
    position.checked_add(length).filter(|end| *end <= available)
}

#[cfg_attr(kani, kani::ensures(|result: &bool| *result ==
    (linked && start <= position && position <= available && after <= before))) ]
#[inline(always)]
pub(crate) fn callback_exit(
    linked: bool,
    start: usize,
    position: usize,
    available: usize,
    before: u64,
    after: u64,
) -> bool {
    linked && start <= position && position <= available && after <= before
}

#[cfg_attr(kani, kani::ensures(|result: &bool| *result == match (expected, actual) {
    (Some(a), Some(b)) => std::ptr::eq(a,b),
    _ => false,
}))]
#[inline(always)]
pub(crate) fn same_scope(expected: Option<&u8>, actual: Option<&u8>) -> bool {
    match (expected, actual) {
        (Some(a), Some(b)) => std::ptr::eq(a, b),
        _ => false,
    }
}

#[cfg(kani)]
#[path = "verification/decode_transitions.rs"]
mod proofs;
