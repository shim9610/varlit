use super::*;

#[kani::proof]
#[kani::unwind(10)]
fn limited_writes_preserve_progress_across_failure_and_retry() {
    let initial: u64 = kani::any();
    let maximum: u64 = kani::any();
    let length: usize = kani::any();
    kani::assume(length <= 4);
    let bytes = [0u8; 4];
    let mut position = initial;
    let mut written = 0u64;
    let mut calls = 0usize;
    let fail_at: usize = kani::any();
    let interrupt_at: usize = kani::any();
    let result = limited_write_all(&mut position, &bytes[..length], maximum, |slice| {
        calls += 1;
        assert!(
            initial as u128 + length as u128 <= maximum as u128,
            "over-limit write reached backend"
        );
        if calls == interrupt_at {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if calls == fail_at {
            return Err(io::ErrorKind::StorageFull.into());
        }
        let n: usize = kani::any();
        kani::assume(n <= slice.len()); // std::io::Write contract, not a file assumption.
        written += n as u64;
        Ok(n)
    });
    let succeeded = result.is_ok();
    std::mem::forget(result);
    assert_eq!(
        position as u128,
        initial as u128 + written as u128,
        "partial write position lost"
    );
    assert!(calls <= length + 1, "write loop did not progress");
    if succeeded {
        assert_eq!(written as usize, length, "successful write lost bytes");
    }
    // One more actual driver invocation must use the updated position.
    let result = limited_write_all(&mut position, &[0], maximum, |_| {
        assert!(
            (initial as u128 + written as u128 + 1) <= maximum as u128,
            "retry used stale position"
        );
        Ok(1)
    });
    std::mem::forget(result);
}

#[kani::proof]
#[kani::unwind(6)]
fn valid_limited_writes_are_not_rejected() {
    let initial: u64 = kani::any();
    let maximum: u64 = kani::any();
    kani::assume(initial as u128 + 4 <= maximum as u128);
    let mut position = initial;
    let result = limited_write_all(&mut position, &[0; 4], maximum, |_| Ok(1));
    assert!(result.is_ok(), "valid limited write rejected");
    std::mem::forget(result);
    assert_eq!(position, initial + 4);
}
