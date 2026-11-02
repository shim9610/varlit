//! Public range streaming, including fallback after a whole-output refusal.
use varlit_core::{ChunkedBytes, Error};

#[cfg(not(all(feature = "compression-zstd", feature = "integrity")))]
#[test]
fn nonempty_stream_reports_the_missing_feature() {
    let frame = [0x28, 0xb5, 0x2f, 0xfd, 0x20, 1, 9, 0, 0, 7];
    let mut encoded = vec![0; 48];
    encoded[..8].copy_from_slice(b"VCHK\x01\0\x01\0");
    encoded[8..16].copy_from_slice(&1u64.to_le_bytes());
    encoded[16..24].copy_from_slice(&1u64.to_le_bytes());
    encoded[24..28].copy_from_slice(&1u32.to_le_bytes());
    encoded[32..36].copy_from_slice(&1u32.to_le_bytes());
    encoded[36..40].copy_from_slice(&(frame.len() as u32).to_le_bytes());
    encoded.extend_from_slice(&frame);
    let chunks = ChunkedBytes::from_encoded(encoded).unwrap();
    let error = chunks
        .stream_range(0..1, &mut [0; 1], 1024, |_, _| {
            panic!("unexpected delivery")
        })
        .unwrap_err();
    #[cfg(not(feature = "integrity"))]
    assert!(matches!(error, Error::IntegrityFeatureDisabled));
    #[cfg(feature = "integrity")]
    assert!(matches!(error, Error::CompressionFeatureDisabled));
}

#[cfg(all(feature = "compression-zstd", feature = "integrity"))]
mod enabled {
    use super::*;
    use varlit_core::{CompressionLevel, Result};
    const WINDOW: usize = 8 * 1024 * 1024;

    fn container(stored: &[u8], plain: &[u8]) -> ChunkedBytes {
        let mut bytes = vec![0; 48];
        bytes[..8].copy_from_slice(b"VCHK\x01\0\x01\0");
        bytes[8..16].copy_from_slice(&(plain.len() as u64).to_le_bytes());
        bytes[16..24].copy_from_slice(&(plain.len() as u64).to_le_bytes());
        bytes[24..28].copy_from_slice(&1u32.to_le_bytes());
        bytes[32..36].copy_from_slice(&(plain.len() as u32).to_le_bytes());
        bytes[36..40].copy_from_slice(&(stored.len() as u32).to_le_bytes());
        bytes[40..44].copy_from_slice(&crc32fast::hash(plain).to_le_bytes());
        bytes.extend_from_slice(stored);
        ChunkedBytes::from_encoded(bytes).unwrap()
    }

    #[test]
    fn fallback_streams_requested_ranges_with_one_byte_scratch() -> Result<()> {
        let data: Vec<_> = (0..8193).map(|i| (i % 251) as u8).collect();
        let chunks = ChunkedBytes::from_zstd_chunks(&data, 1024, CompressionLevel::Fast)?;
        assert_eq!(chunks.decoded_len(), data.len() as u64);
        assert!(matches!(
            chunks.decode_to_vec_limited(64),
            Err(Error::DecompressedLengthLimitExceeded { .. })
        ));
        for range in [0..8193, 7..25, 1023..2050, 8192..8193, 8193..8193] {
            for capacity in [1, 7, 4096] {
                let mut buffer = vec![0; capacity];
                let pointer = buffer.as_ptr();
                let mut position = range.start;
                chunks.stream_range(range.clone(), &mut buffer, WINDOW, |at, bytes| {
                    assert_eq!(at, position);
                    assert!(!bytes.is_empty());
                    assert!(bytes.len() <= capacity);
                    assert_eq!(bytes, &data[at as usize..at as usize + bytes.len()]);
                    position += bytes.len() as u64;
                    Ok(())
                })?;
                assert_eq!(position, range.end);
                assert_eq!(pointer, buffer.as_ptr());
            }
        }
        Ok(())
    }

    #[test]
    fn unknown_size_concatenated_skippable_and_legacy_frames() -> Result<()> {
        let data: Vec<_> = (0..4097).map(|i| (i % 251) as u8).collect();
        let mut encoder = zstd::bulk::Compressor::new(1)?;
        encoder.include_contentsize(false)?;
        let mut stored = vec![0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0];
        stored.extend(encoder.compress(&data[..2000])?);
        stored.extend(encoder.compress(&[])?);
        stored.extend(encoder.compress(&data[2000..])?);
        let cases = [
            (container(&stored, &data), data.as_slice()),
            (
                container(
                    include_bytes!("fixtures/zstd-legacy.bin"),
                    include_bytes!("fixtures/zstd-legacy.txt"),
                ),
                include_bytes!("fixtures/zstd-legacy.txt").as_slice(),
            ),
        ];
        for (chunks, expected) in cases {
            let mut position = 0;
            chunks.stream_range(
                0..chunks.decoded_len(),
                &mut [0; 17],
                WINDOW,
                |at, bytes| {
                    assert_eq!(at, position);
                    assert_eq!(bytes, &expected[at as usize..at as usize + bytes.len()]);
                    position += bytes.len() as u64;
                    Ok(())
                },
            )?;
            assert_eq!(position, expected.len() as u64);
        }
        Ok(())
    }

    #[test]
    fn legacy_windows_and_large_output_shortcuts_cannot_bypass_the_cap() -> Result<()> {
        let legacy = container(
            include_bytes!("fixtures/zstd-legacy.bin"),
            include_bytes!("fixtures/zstd-legacy.txt"),
        );
        let mut calls = 0;
        let result = legacy.stream_range(0..1, &mut [0; 17], 1024, |_, _| {
            calls += 1;
            Ok(())
        });
        assert!(matches!(
            result,
            Err(Error::LimitExceeded {
                actual: 2048,
                limit: 1024,
                ..
            })
        ));
        assert_eq!(calls, 0);
        let modern = ChunkedBytes::from_zstd_chunks(&[7; 65536], 65536, CompressionLevel::Fast)?;
        // zstd can choose its one-shot path when output holds the whole frame.
        // Our preflight applies the explicit policy consistently in that case.
        assert!(matches!(
            modern.stream_range(0..1, &mut [0; 65536], 1024, |_, _| {
                calls += 1;
                Ok(())
            }),
            Err(Error::LimitExceeded { .. })
        ));
        assert_eq!(calls, 0);
        Ok(())
    }

    #[test]
    fn validates_entire_selected_chunk_including_suffix_outside_range() -> Result<()> {
        let data = vec![3; 4096];
        let chunks = ChunkedBytes::from_zstd_chunks(&data, 2048, CompressionLevel::Fast)?;
        let mut encoded = chunks.into_encoded();
        encoded[40] ^= 1; // first chunk's container CRC
        let chunks = ChunkedBytes::from_encoded(encoded)?;
        let mut delivered = 0;
        let result = chunks.stream_range(0..1, &mut [0; 13], WINDOW, |_, bytes| {
            delivered += bytes.len();
            Ok(())
        });
        assert_eq!(delivered, 1); // provisional, then the full-chunk CRC fails
        assert!(matches!(
            result,
            Err(Error::ChunkChecksumMismatch { chunk_index: 0, .. })
        ));
        // An unrelated corrupt chunk does not defeat an explicitly local read.
        chunks.stream_range(2048..2049, &mut [0; 13], WINDOW, |_, bytes| {
            assert_eq!(bytes, &[3]);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn callback_failure_stops_and_retry_has_no_retained_progress() -> Result<()> {
        let chunks = ChunkedBytes::from_zstd_chunks(&[5; 4096], 1024, CompressionLevel::Fast)?;
        let mut calls = 0;
        let result = chunks.stream_range(31..3000, &mut [0; 19], WINDOW, |_, _| {
            calls += 1;
            Err(Error::InvalidImmediatePolicy)
        });
        assert!(matches!(result, Err(Error::InvalidImmediatePolicy)));
        assert_eq!(calls, 1);
        let mut count = 0;
        chunks.stream_range(31..3000, &mut [0; 19], WINDOW, |at, bytes| {
            assert_eq!(at, 31 + count);
            count += bytes.len() as u64;
            Ok(())
        })?;
        assert_eq!(count, 2969);
        Ok(())
    }

    #[test]
    fn buffer_range_and_window_refusals_are_retryable() -> Result<()> {
        let chunks = ChunkedBytes::from_zstd_chunks(&[8; 65536], 65536, CompressionLevel::Fast)?;
        let mut calls = 0;
        let mut callback = |_: u64, _: &[u8]| {
            calls += 1;
            Ok(())
        };
        assert!(matches!(
            chunks.stream_range(0..1, &mut [], WINDOW, &mut callback),
            Err(Error::BufferTooSmall { required: 1, .. })
        ));
        for range in [
            std::ops::Range { start: 2, end: 1 },
            0..65537,
            u64::MAX..u64::MAX,
        ] {
            assert!(matches!(
                chunks.stream_range(range, &mut [0; 1], WINDOW, &mut callback),
                Err(Error::InvalidChunkedRange { .. })
            ));
        }
        assert!(matches!(
            chunks.stream_range(0..1, &mut [0; 1], 0, &mut callback),
            Err(Error::LimitExceeded { .. })
        ));
        assert!(
            chunks
                .stream_range(0..1, &mut [0; 1], 1024, &mut callback)
                .is_err()
        );
        assert_eq!(calls, 0);
        chunks.stream_range(0..1, &mut [0; 1], WINDOW, |at, bytes| {
            assert_eq!(at, 0);
            assert_eq!(bytes, &[8]);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn malformed_frames_and_false_unknown_lengths_never_deliver_past_bounds() -> Result<()> {
        let data = vec![7; 4096];
        let mut encoder = zstd::bulk::Compressor::new(1)?;
        encoder.include_contentsize(false)?;
        let stored = encoder.compress(&data)?;
        for length in [1, 4095, 4097] {
            let chunks = container(&stored, &vec![7; length]);
            let mut count = 0;
            assert!(
                chunks
                    .stream_range(0..length as u64, &mut [0; 23], WINDOW, |_, bytes| {
                        count += bytes.len();
                        assert!(count <= length);
                        Ok(())
                    })
                    .is_err()
            );
        }
        for bytes in [
            &stored[..stored.len() - 1],
            b"invalid".as_slice(),
            &[0x28, 0xb5, 0x2f, 0xfd, 0xa0, 0, 0, 0x40, 0, 9, 0, 0, 7],
        ] {
            let chunks = container(bytes, &data);
            assert!(
                chunks
                    .stream_range(0..1, &mut [0; 7], WINDOW, |_, _| Ok(()))
                    .is_err()
            );
        }
        Ok(())
    }
}

#[test]
fn empty_ranges_need_no_features_buffer_or_backend() {
    let mut bytes = vec![0; 32];
    bytes[..8].copy_from_slice(b"VCHK\x01\0\x01\0");
    bytes[8..16].copy_from_slice(&1u64.to_le_bytes());
    let chunks = ChunkedBytes::from_encoded(bytes).unwrap();
    assert_eq!(chunks.decoded_len(), 0);
    chunks
        .stream_range(0..0, &mut [], 0, |_, _| panic!("empty delivery"))
        .unwrap();
    assert!(matches!(
        chunks.stream_range(0..1, &mut [0; 1], 1024, |_, _| Ok(())),
        Err(Error::InvalidChunkedRange { .. })
    ));
}
