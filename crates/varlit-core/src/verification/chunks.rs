//! Compositional proofs of actual header/entry parsing and full-width table bounds.
//! Whole arbitrary-length byte parsing is covered by native tests and fuzzing.
use super::*;

#[kani::proof]
fn full_width_chunk_table_bounds_iteration_by_real_input() {
    let count: usize = kani::any();
    let len: usize = kani::any();
    let result = table_extent(count, len);
    let wide = count as u128 * 16 + 32;
    assert_eq!(
        result.is_some(),
        wide <= len as u128,
        "chunk table exceeds real input"
    );
    if let Some(end) = result {
        assert_eq!(end as u128, wide);
        assert_eq!((end - 32) / 16, count);
        assert!(count <= len / 16, "chunk count escaped the input bound");
    }
}

#[kani::proof]
#[kani::unwind(18)]
fn arbitrary_chunk_entry_bytes_are_checked() {
    let bytes: [u8; 16] = kani::any();
    let result = parse_entry(&bytes, 0);
    assert_eq!(result.is_ok(), bytes[12..16] == [0; 4]);
    if let Ok(entry) = &result {
        assert_eq!(
            entry.uncompressed_len,
            u32::from_le_bytes(bytes[0..4].try_into().unwrap())
        );
        assert_eq!(
            entry.stored_len,
            u32::from_le_bytes(bytes[4..8].try_into().unwrap())
        );
    }
    std::mem::forget(result);
}

#[kani::proof]
#[kani::unwind(35)]
fn full_width_chunk_header_fields_are_validated() {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(b"VCHK\x01\x00\x01\x00");
    let chunk: u64 = kani::any();
    let total: u64 = kani::any();
    let count: u32 = kani::any();
    bytes[8..16].copy_from_slice(&chunk.to_le_bytes());
    bytes[16..24].copy_from_slice(&total.to_le_bytes());
    bytes[24..28].copy_from_slice(&count.to_le_bytes());
    let result = parse_header(&bytes);
    assert_eq!(result.is_ok(), chunk > 0 && chunk <= u32::MAX as u64);
    if let Ok(h) = &result {
        assert_eq!(h.chunk_len as u64, chunk);
        assert_eq!(h.uncompressed_len, total);
        assert_eq!(h.chunk_count, count);
    }
    std::mem::forget(result);
}

#[kani::proof]
#[kani::unwind(18)]
fn bounded_whole_chunk_validator_accepts_a_complete_container() {
    let mut bytes = [0u8; 49];
    bytes[..8].copy_from_slice(b"VCHK\x01\x00\x01\x00");
    bytes[8..16].copy_from_slice(&1u64.to_le_bytes());
    bytes[16..24].copy_from_slice(&1u64.to_le_bytes());
    bytes[24..28].copy_from_slice(&1u32.to_le_bytes());
    bytes[32..36].copy_from_slice(&1u32.to_le_bytes());
    bytes[36..40].copy_from_slice(&1u32.to_le_bytes());
    bytes[48] = kani::any();
    let result = validate_chunked_bytes(&bytes, false);
    assert!(result.is_ok(), "complete one-chunk container rejected");
    std::mem::forget(result);
}
