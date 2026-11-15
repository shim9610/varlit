//! Full-width indices, at most39 backing bytes; no filesystem/allocation model.
use super::buffered_row_at;

#[kani::proof]
fn full_width_buffered_rows_match_the_actual_window() {
    let bytes = [
        0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38,
    ];
    let len: u8 = kani::any();
    kani::assume(len <= 39);
    let first: usize = kani::any();
    let index: usize = kani::any();
    let row = buffered_row_at::<13>(&bytes[..len as usize], first, index);
    let expected = index >= first && (index as u128 - first as u128) * 13 + 13 <= len as u128;
    assert_eq!(row.is_some(), expected);
    if let Some(row) = row {
        let start = (index - first) * 13;
        assert_eq!(row[0], start as u8);
        let byte: u8 = kani::any();
        kani::assume(byte < 13);
        assert_eq!(row[byte as usize], (start + byte as usize) as u8);
    }
}
