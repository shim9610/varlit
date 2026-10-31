//! Compare independent framing routes using complete entries, not selected
//! fields. Framing-only routes deliberately defer payload CRC verification;
//! scanners deliberately distinguish a recoverable tail from an invalid frame.
use super::*;
use std::io::Cursor;

fn spec() -> FormatSpec {
    FormatSpec::new(
        b"FRAMECHECK",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        &[],
    )
    .with_read_limits(crate::ReadLimits::finite_all(u64::MAX))
}

fn scan(spec: FormatSpec, bytes: &[u8], offset: u64) -> Result<RecordRead> {
    read_record_entry_at(
        spec,
        &mut Cursor::new(bytes),
        bytes.len() as u64,
        offset,
        ScanChecks {
            partial_boundary: None,
            checksum_boundary: None,
            verify_checksums: true,
        },
        &mut ScanAccounting::default(),
    )
}

fn snapshot(bytes: &[u8]) -> Result<SnapshotFile> {
    let mut file = tempfile::tempfile()?;
    file.write_all(bytes)?;
    SnapshotFile::new(file)
}

fn append_frame(
    spec: FormatSpec,
    bytes: &mut Vec<u8>,
    flags: u16,
    payload: &[u8],
    footer: RecordFooterFields,
) -> Result<RecordIndexEntry> {
    let record_offset = bytes.len() as u64;
    let payload_offset = record_offset + RECORD_HEADER_LEN;
    let mut header = RecordHeaderFields {
        block_id: 37,
        block_version: 3,
        flags,
        sequence: record_offset + 11,
        payload_len: payload.len() as u64,
        checksum: 0,
        uncompressed_len_hint: 0,
    };
    let footer_bytes = if spec.spec_needs_record_footer() {
        encode_record_footer(footer)?.to_vec()
    } else {
        Vec::new()
    };
    header.checksum = checksum_record_fields(spec, record_offset, header, payload, &footer_bytes)?;
    bytes.extend_from_slice(&encode_native_record_header(
        header,
        record_offset,
        footer_bytes.len() as u64,
    )?);
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&footer_bytes);
    Ok(RecordIndexEntry {
        block_id: header.block_id,
        block_version: header.block_version,
        flags,
        sequence: header.sequence,
        record_offset,
        payload_offset,
        payload_len: header.payload_len,
        checksum: header.checksum,
        uncompressed_len_hint: 0,
        footer_offset: (!footer_bytes.is_empty()).then_some(payload_offset + header.payload_len),
        prev_same_block_offset: footer.prev_same_block_offset,
        prev_same_key_offset: footer.prev_same_key_offset,
        mutable_flags: footer.mutable_flags,
        committed: true,
    })
}

// This test requires integrity specifically to make every malformed header's
// CRC valid. Native and ASan all-feature jobs exhaust the entire u16 domain.
#[cfg(feature = "integrity")]
#[test]
fn every_record_flag_word_has_the_same_framing_verdict() -> Result<()> {
    let spec = spec().with_integrity_policy(IntegrityPolicy::Crc32WithHeader);
    let footer = RecordFooterFields {
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        mutable_flags: 0,
    };
    let mut bytes = Vec::new();
    for flags in 0..=u16::MAX {
        append_frame(spec, &mut bytes, flags, b"payload", footer)?;
    }
    let snapshot = snapshot(&bytes)?;
    let stride = RECORD_HEADER_LEN + b"payload".len() as u64;
    for flags in 0..=u16::MAX {
        let offset = u64::from(flags) * stride;
        let sequential = scan(spec, &bytes, offset);
        let positional = read_record_entry_positional(spec, &snapshot, offset);
        // Independent wire contract: bit 0 is compression, bit 15 internal.
        // Adding a flag requires an explicit contract/test update.
        let unknown = flags & !0x8001;
        if unknown != 0 {
            assert!(
                matches!(sequential, Err(Error::UnknownRecordFlags(bits)) if bits == unknown),
                "scanner accepted/misclassified flags {flags:#06x}: {sequential:?}"
            );
            assert!(
                matches!(positional, Err(Error::UnknownRecordFlags(bits)) if bits == unknown),
                "positional accepted/misclassified flags {flags:#06x}: {positional:?}"
            );
        } else if flags & 1 != 0 {
            // No registered compression contract; even the known compression
            // bit is invalid here. Both paths must enforce that rule too.
            assert!(matches!(
                sequential,
                Err(Error::UnregisteredBlock(37) | Error::InvalidCompressionHeader)
            ));
            assert!(matches!(
                positional,
                Err(Error::UnregisteredBlock(37) | Error::InvalidCompressionHeader)
            ));
        } else {
            let RecordRead::Entry(sequential) = sequential? else {
                panic!("complete record became tail")
            };
            assert_eq!(sequential, positional?);
        }
    }
    Ok(())
}

#[test]
fn framing_routes_preserve_every_footer_field() -> Result<()> {
    let policies = if cfg!(feature = "integrity") {
        vec![
            IntegrityPolicy::None,
            IntegrityPolicy::Crc32,
            IntegrityPolicy::Crc32WithHeader,
        ]
    } else {
        vec![IntegrityPolicy::None]
    };
    for integrity in policies {
        for footer_enabled in [false, true] {
            for liveness in [LivenessPolicy::None, LivenessPolicy::FooterFlags] {
                // FooterFlags requires a format that actually stores footers.
                // FormatSpec validation rejects the other pairing at open.
                if !footer_enabled && liveness == LivenessPolicy::FooterFlags {
                    continue;
                }
                let spec = spec()
                    .with_integrity_policy(integrity)
                    .with_commit_policy(if footer_enabled {
                        CommitPolicy::RecordFooter
                    } else {
                        CommitPolicy::None
                    })
                    .with_liveness_policy(liveness);
                let mut bytes = vec![0; 128];
                let mut entries = Vec::new();
                let words: Vec<u32> = if liveness == LivenessPolicy::FooterFlags {
                    std::iter::once(0)
                        .chain((0..32).map(|bit| 1u32 << bit))
                        .chain([u32::MAX])
                        .collect()
                } else {
                    vec![0]
                };
                for word in words {
                    for payload in [b"".as_slice(), b"x", b"distinct payload"] {
                        let footer = RecordFooterFields {
                            prev_same_block_offset: spec.spec_needs_record_footer().then_some(32),
                            prev_same_key_offset: spec.spec_needs_record_footer().then_some(64),
                            mutable_flags: word,
                        };
                        entries.push(append_frame(spec, &mut bytes, 0, payload, footer)?);
                    }
                }
                let snapshot = snapshot(&bytes)?;
                for expected in entries {
                    let RecordRead::Entry(sequential) = scan(spec, &bytes, expected.record_offset)?
                    else {
                        panic!("complete record became tail")
                    };
                    assert_eq!(sequential, expected);
                    assert_eq!(
                        read_record_entry_positional(spec, &snapshot, expected.record_offset)?,
                        expected
                    );
                    assert_eq!(
                        fault_record_entry(&snapshot, spec, expected.record_offset, true)?,
                        expected
                    );
                    // Commit state belongs to the directory, not the bytes.
                    assert_eq!(
                        fault_record_entry(&snapshot, spec, expected.record_offset, false)?,
                        RecordIndexEntry {
                            committed: false,
                            ..expected
                        }
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn no_truncated_frame_is_returned_as_a_complete_record() -> Result<()> {
    for footer_enabled in [false, true] {
        let spec = spec().with_commit_policy(if footer_enabled {
            CommitPolicy::RecordFooter
        } else {
            CommitPolicy::None
        });
        let mut bytes = Vec::new();
        append_frame(
            spec,
            &mut bytes,
            0,
            b"payload",
            RecordFooterFields {
                prev_same_block_offset: None,
                prev_same_key_offset: None,
                mutable_flags: 0,
            },
        )?;
        for end in 0..bytes.len() {
            let partial = &bytes[..end];
            let snapshot = snapshot(partial)?;
            assert!(
                !matches!(scan(spec, partial, 0), Ok(RecordRead::Entry(_))),
                "accepted prefix {end}"
            );
            assert!(
                read_record_entry_positional(spec, &snapshot, 0).is_err(),
                "accepted prefix {end}"
            );
        }
    }
    Ok(())
}
