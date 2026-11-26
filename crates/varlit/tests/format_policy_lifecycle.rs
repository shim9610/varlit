//! Cross-product regression for format identity and stream publication.
//! Runtime verification settings must not change the declared wire format.
#![cfg(all(feature = "integrity", feature = "compression-zstd"))]
#[path = "support/pages.rs"]
mod pages;

use varlit::*;
#[allow(dead_code)]
#[path = "support/validation_cases.rs"]
mod cases;
type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable", key = "id")]
struct Item {
    #[varlit(field_id = 1)]
    id: u64,
    #[varlit(field_id = 2)]
    bytes: Vec<u8>,
}
varlit_format! { struct Format {
    magic: b"POLICIES"; version: 1; endian: little; blocks: [Item];
} }
/// Drains the rest of the walk into a `Vec`, through one reused value.
fn collect_rest(cursor: &mut StreamingBlocks<Item>) -> varlit::Result<Vec<Item>> {
    let mut out = Item::empty();
    let mut all = Vec::new();
    while cursor.next_into(&mut out)? {
        all.push(out.clone());
    }
    Ok(all)
}
fn item(id: u64, byte: u8) -> Item {
    Item {
        id,
        bytes: vec![byte; 4096],
    }
}
fn runtime(verification: IntegrityVerification) -> ResourceLimits {
    ResourceLimits::MISSING
        .with_integrity_verification(verification)
        .with_max_record_payload_len(1024 * 1024)
        .with_max_logical_payload_len(1024 * 1024)
        .with_max_materialized_bytes(4 * 1024 * 1024)
}
fn stream_case(spec: FormatSpec) -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("stream.varlit");
    let options = StreamOptions {
        limits: runtime(IntegrityVerification::AtOpen),
    };
    let mut writer = VarlitStreamWriter::create(spec, &path, options)?;
    writer.push_info(&item(1, 41))?;
    writer.sync()?;
    let mut reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
    let mut cursor = reader.blocks::<Item>()?;
    let mut next = Item::empty();
    assert!(cursor.next_into(&mut next)?);
    assert_eq!(next, item(1, 41));
    assert!(!cursor.next_into(&mut next)?);
    writer.push_info(&item(2, 42))?;
    writer.sync()?;
    reader.follow_blocks(&mut cursor)?;
    assert!(cursor.next_into(&mut next)?);
    assert_eq!(next, item(2, 42));
    assert!(!cursor.next_into(&mut next)?);
    reader.follow_blocks(&mut cursor)?;
    drop(writer);
    let mut writer = VarlitStreamWriter::open(spec, &path, options)?;
    writer.push_info(&item(3, 43))?;
    writer.sync()?;
    drop(writer);
    reader.follow_blocks(&mut cursor)?;
    assert!(cursor.next_into(&mut next)?);
    assert_eq!(next, item(3, 43));
    assert!(!cursor.next_into(&mut next)?);
    drop(cursor);
    drop(reader);
    // The creation nonce, three items and the three sync markers that closed
    // them, plus an embedded manifest.
    let expected = 7 + u64::from(spec.manifest_policy == ManifestPolicy::Embedded);
    let reader = VarlitStreamReader::open(spec, &path, options)?;
    assert_eq!(
        collect_rest(&mut reader.blocks::<Item>()?)?,
        [item(1, 41), item(2, 42), item(3, 43)]
    );
    assert_eq!(reader.verify_all()?, expected);
    let tight = StreamOptions {
        limits: ResourceLimits::MISSING.with_max_logical_payload_len(32),
    };
    let limited = VarlitStreamReader::open(spec, &path, tight)?;
    assert!(matches!(
        collect_rest(&mut limited.blocks::<Item>()?),
        Err(Error::LimitExceeded { .. })
    ));
    let wrong = spec
        .with_integrity_policy(if spec.integrity_policy == IntegrityPolicy::None {
            IntegrityPolicy::Crc32
        } else {
            IntegrityPolicy::None
        })
        .with_computed_schema_hash();
    assert!(matches!(
        VarlitStreamReader::open(wrong, &path, options),
        Err(Error::SchemaHashMismatch { .. })
    ));
    Ok(())
}

#[test]
fn format_policies_survive_stream_lifecycles() -> TestResult {
    let mut cases = 0;
    let mut unsupported = 0;
    for &integrity in cases::INTEGRITY {
        for &endian in cases::ENDIANS {
            for commit in cases::commits() {
                for compression in cases::compression() {
                    for &manifest in cases::MANIFESTS {
                        let mut spec = Format::spec()
                            .with_integrity_policy(integrity)
                            .with_commit_policy(commit)
                            .with_manifest_policy(manifest)
                            .with_index_policy(IndexPolicy::BlockOffsetChain)
                            .with_compression_policy(compression);
                        spec.endian = endian;
                        let spec = spec.with_computed_schema_hash();
                        assert_eq!(
                            spec.computed_schema_hash(),
                            spec.with_resource_limits(runtime(IntegrityVerification::AtOpen))
                                .computed_schema_hash()
                        );
                        if commit.is_transaction_marker() {
                            // These declarations are valid native formats but
                            // deliberately unsupported by the stream API.
                            // Exercise their rejection instead of filtering them out.
                            let dir = tempfile::tempdir()?;
                            let path = dir.path().join("unsupported.varlit");
                            // A single-barrier marker without header checksums
                            // is refused as a format before the stream API is
                            // asked about it.
                            let invalid_spec = commit.single_barrier()
                                && integrity != IntegrityPolicy::Crc32WithHeader;
                            let refused =
                                VarlitStreamWriter::create(spec, &path, StreamOptions::default());
                            assert!(
                                matches!(refused, Err(Error::StreamingUnsupported))
                                    || (invalid_spec
                                        && matches!(refused, Err(Error::InvalidFormatSpec(_)))),
                                "{:?}",
                                refused.as_ref().err()
                            );
                            unsupported += 1;
                            continue;
                        }
                        stream_case(spec).map_err(|e| format!("stream {integrity:?} {endian:?} {commit:?} {compression:?} {manifest:?}: {e}"))?;
                        cases += 1;
                    }
                }
            }
        }
    }
    println!(
        "format_combinations={cases} lifecycle_runs={cases} unsupported_combinations={unsupported} runtime_limits_and_wrong_format_rejections=passed"
    );
    Ok(())
}
