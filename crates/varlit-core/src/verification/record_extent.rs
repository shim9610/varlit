//! Full-width arithmetic proof; no restriction on payload offset or length.
use super::{RECORD_FOOTER_LEN, RecordIndexEntry};

#[kani::proof]
fn checked_extent_matches_wide_arithmetic() {
    let offset: u64 = kani::any();
    let len: u64 = kani::any();
    let footer: bool = kani::any();
    let entry = RecordIndexEntry {
        block_id: 0,
        block_version: 0,
        flags: 0,
        sequence: 0,
        record_offset: 0,
        payload_offset: offset,
        payload_len: len,
        checksum: 0,
        uncompressed_len_hint: 0,
        footer_offset: footer.then_some(0),
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        mutable_flags: 0,
        committed: false,
    };
    let expected =
        u128::from(offset) + u128::from(len) + u128::from(footer) * u128::from(RECORD_FOOTER_LEN);
    let result = entry.checked_physical_end();
    match &result {
        Ok(end) => {
            assert_eq!(u128::from(*end), expected);
        }
        Err(_) => {
            assert!(expected > u128::from(u64::MAX));
        }
    }
    // Error's other variants contain recursive boxed causes. This harness
    // proves arithmetic, not unrelated recursive destruction; this function's
    // error is LengthOverflow, which owns no allocation.
    std::mem::forget(result);
    assert_eq!(
        entry.physical_end(),
        expected.min(u128::from(u64::MAX)) as u64
    );
}
