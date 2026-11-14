//! Producer/consumer contracts for the actual native wire implementations.
//! Scalar fields retain their complete Rust ranges. The fixed 32-byte framing
//! bounds the implementation loops, not file length or record count.
use super::*;

#[kani::proof]
#[kani::unwind(40)]
fn successful_native_header_roundtrips_all_fields() {
    let header = RecordHeaderFields {
        block_id: kani::any(),
        block_version: kani::any(),
        flags: kani::any(),
        sequence: kani::any(),
        payload_len: kani::any(),
        checksum: kani::any(),
        uncompressed_len_hint: kani::any(),
    };
    let offset: u64 = kani::any();
    let footer_len = if kani::any() {
        native_record_footer_len()
    } else {
        0
    };
    let end = u128::from(offset)
        + u128::from(native_record_header_len())
        + u128::from(header.payload_len)
        + u128::from(footer_len);
    kani::assume(end <= u128::from(u64::MAX));

    let bytes = match encode_native_record_header(header, offset, footer_len) {
        Ok(bytes) => bytes,
        Err(error) => {
            std::mem::forget(error);
            assert!(false, "representable native header was rejected");
            return;
        }
    };
    // The wire oracle is independent of the shared role/offset table. An
    // encoder and decoder with the same accidental transposition must fail.
    assert_eq!(bytes.len(), 32);
    assert_eq!(&bytes[0..4], &header.block_id.to_le_bytes());
    assert_eq!(&bytes[4..6], &header.block_version.to_le_bytes());
    assert_eq!(&bytes[6..8], &header.flags.to_le_bytes());
    assert_eq!(&bytes[8..16], &header.sequence.to_le_bytes());
    assert_eq!(&bytes[16..24], &header.payload_len.to_le_bytes());
    assert_eq!(&bytes[24..28], &header.checksum.to_le_bytes());
    assert_eq!(&bytes[28..32], &header.uncompressed_len_hint.to_le_bytes());
    let mut input = bytes.as_slice();
    let decoded = match read_native_record_header(&mut input, offset) {
        Ok(decoded) => decoded,
        Err(error) => {
            std::mem::forget(error);
            assert!(false, "successfully encoded native header was rejected");
            return;
        }
    };
    assert_eq!(decoded.fields, header);
    assert_eq!(decoded.lead_in_len, 32);
    assert!(input.is_empty());
    kani::cover!(offset == 0 && header.payload_len == 0);
    kani::cover!(end == u128::from(u64::MAX));
}

#[kani::proof]
#[kani::unwind(40)]
fn valid_native_footer_roundtrips_all_fields() {
    let offset: u64 = kani::any();
    let footer_offset: u64 = kani::any();
    let footer = RecordFooterFields {
        prev_same_block_offset: kani::any(),
        prev_same_key_offset: kani::any(),
        mutable_flags: kani::any(),
    };
    // These are the writer's backward-link conditions. Zero denotes absence
    // on the wire and cannot represent a present predecessor.
    kani::assume(
        footer
            .prev_same_block_offset
            .is_none_or(|p| p != 0 && p < offset),
    );
    kani::assume(
        footer
            .prev_same_key_offset
            .is_none_or(|p| p != 0 && p < offset),
    );

    let bytes = match encode_native_record_footer(footer) {
        Ok(bytes) => bytes,
        Err(error) => {
            std::mem::forget(error);
            assert!(false, "valid native footer was rejected by the encoder");
            return;
        }
    };
    let link_flags = u16::from(footer.prev_same_block_offset.is_some())
        | (u16::from(footer.prev_same_key_offset.is_some()) << 1);
    assert_eq!(bytes.len(), 32);
    assert_eq!(&bytes[0..4], b"VRF1");
    assert_eq!(&bytes[4..6], &1u16.to_le_bytes());
    assert_eq!(&bytes[6..8], &link_flags.to_le_bytes());
    assert_eq!(
        &bytes[8..16],
        &footer.prev_same_block_offset.unwrap_or(0).to_le_bytes()
    );
    assert_eq!(
        &bytes[16..24],
        &footer.prev_same_key_offset.unwrap_or(0).to_le_bytes()
    );
    assert_eq!(&bytes[24..28], &0u32.to_le_bytes());
    assert_eq!(&bytes[28..32], &footer.mutable_flags.to_le_bytes());
    let decoded = match decode_native_record_footer(&bytes, footer_offset, offset) {
        Ok(decoded) => decoded,
        Err(error) => {
            std::mem::forget(error);
            assert!(
                false,
                "successfully encoded valid native footer was rejected"
            );
            return;
        }
    };
    assert_eq!(decoded, footer);
    kani::cover!(footer.prev_same_block_offset.is_none() && footer.prev_same_key_offset.is_none());
    kani::cover!(footer.prev_same_block_offset.is_some() && footer.prev_same_key_offset.is_some());
}
