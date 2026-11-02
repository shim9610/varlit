use super::*;

#[kani::proof]
fn streaming_step_is_bounded_and_strictly_progresses() {
    let before: usize = kani::any();
    let after: usize = kani::any();
    let input_len: usize = kani::any();
    let decoded: u64 = kani::any();
    let written: usize = kani::any();
    let expected: u64 = kani::any();
    let next = stream_progress(before, after, input_len, decoded, written, expected);
    let sum = decoded as u128 + written as u128;
    let valid = before <= after
        && after <= input_len
        && sum <= expected as u128
        && (after > before || written != 0);
    assert_eq!(
        next.is_some(),
        valid,
        "stream step acceptance differs from wide oracle"
    );
    if let Some(next) = next {
        assert!(next <= expected, "stream output exceeds declared chunk");
        assert!(after > before || next > decoded, "stream did not progress");
        assert_eq!(next as u128, sum);
    }
}

#[kani::proof]
fn output_requests_respect_both_limits() {
    let declared: Option<u64> = kani::any();
    let bound: u64 = kani::any();
    let maximum: usize = kani::any();
    let result = output_capacity(declared, bound, maximum);
    if let Some(capacity) = result {
        assert!(capacity <= maximum, "output exceeds caller limit");
        assert!(
            capacity as u128 <= bound as u128,
            "output exceeds complete blocks"
        );
        if let Some(declared) = declared {
            assert_eq!(capacity as u64, declared);
        } else {
            assert_eq!(capacity as u128, (bound as u128).min(maximum as u128));
        }
    }
    assert_eq!(
        result.is_some(),
        declared.is_none_or(|n| n <= bound && n <= maximum as u64),
        "valid output reservation rejected"
    );
}

#[kani::proof]
fn arbitrary_block_step_progresses_or_rejects() {
    let offset: usize = kani::any();
    let header: u32 = kani::any();
    let len: usize = kani::any();
    let result = block_extent(offset, header, len);
    let size = (header >> 3) as u128;
    let kind = (header >> 1) & 3;
    let stored = if kind == 1 { 1 } else { size };
    let end = offset as u128 + 3 + stored;
    let valid = size <= 131072 && kind != 3 && end <= len as u128;
    assert_eq!(
        result.is_some(),
        valid,
        "block acceptance differs from wide oracle"
    );
    if let Some((next, output)) = result {
        assert!(
            next > offset && next <= len,
            "block parser failed to progress"
        );
        assert_eq!(next as u128, end);
        assert_eq!(output as u128, if kind == 2 { 131072 } else { size });
        assert!(output <= 131072, "block output bound exceeded");
    }
}
