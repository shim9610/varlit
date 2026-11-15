//! Proofs of the chunked-record geometry and of the table-entry checks a
//! chunk read makes before it reads or allocates anything for the chunk.
//! Whole payload parsing (headers with their checksum, compressed frames) is
//! covered by native tests.
use super::*;

/// Every chunk of any chunked length is 1..=64 KiB long, and the chunks
/// cover the logical payload exactly: the last ends at the logical length.
#[kani::proof]
fn chunk_geometry_covers_the_payload_exactly() {
    let logical_len: u64 = kani::any();
    kani::assume(logical_len > CHUNK_LEN && logical_len <= 1 << 40);
    let header = ChunkHeader::new(None, logical_len);
    assert!(header.count >= 2);
    assert!((header.count - 1) * CHUNK_LEN < logical_len);
    assert!(header.count * CHUNK_LEN >= logical_len);
    let index: u64 = kani::any();
    kani::assume(index < header.count);
    let len = header.chunk_len(index);
    assert!(len >= 1 && len <= CHUNK_LEN);
    assert!(index * CHUNK_LEN + len <= logical_len);
    if index + 1 == header.count {
        assert_eq!(index * CHUNK_LEN + len, logical_len);
    } else {
        assert_eq!(len, CHUNK_LEN);
    }
    assert_eq!(
        header.data_at(),
        CHUNK_HEADER_LEN + CHUNK_ENTRY_LEN * header.count
    );
}

/// For arbitrary table entries, an uncompressed chunk's extent is accepted
/// exactly when it is ascending, as long as the chunk, inside the data, and
/// (for the last chunk) ends the data; an accepted extent lies inside the
/// payload.
#[kani::proof]
fn chunk_extent_accepts_exactly_well_formed_entries() {
    let logical_len: u64 = 2 * CHUNK_LEN + 5;
    let header = ChunkHeader::new(None, logical_len);
    let index: u64 = kani::any();
    kani::assume(index < header.count);
    let previous_end: u64 = kani::any();
    let end: u64 = kani::any();
    let reserved: u32 = kani::any();
    let payload_len: u64 = kani::any();
    let mut read = |at: u64, buf: &mut [u8]| -> Result<()> {
        // Entry `index - 1` holds `previous_end`, entry `index` holds `end`.
        let entry_at = |i: u64| CHUNK_HEADER_LEN + i * CHUNK_ENTRY_LEN;
        let value = if at == entry_at(index) {
            end
        } else {
            previous_end
        };
        buf[0..8].copy_from_slice(&value.to_le_bytes());
        buf[8..12].copy_from_slice(&0u32.to_le_bytes());
        buf[12..16].copy_from_slice(&reserved.to_le_bytes());
        Ok(())
    };
    let result = chunk_extent(&mut read, &header, payload_len, index);
    let start = if index == 0 { 0 } else { previous_end };
    let data_at = header.data_at();
    let well_formed = reserved == 0
        && payload_len >= data_at
        && end >= start
        && end - start == header.chunk_len(index)
        && end <= payload_len - data_at
        && (index + 1 != header.count || end == payload_len - data_at);
    assert_eq!(result.is_ok(), well_formed);
    if let Ok((from, to, _)) = &result {
        assert!(from <= to && *to <= payload_len);
    }
    // As the other parser proofs: the error's drop glue is recursive and
    // outside what this proof is about.
    std::mem::forget(result);
}
