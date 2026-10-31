//! Cross-product regression for format identity and stream/index publication.
//! Runtime verification settings must not change the declared wire format.
#![cfg(all(feature = "integrity", feature = "compression-zstd"))]

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
        ..StreamOptions::default()
    };
    let mut writer = VarlitStreamWriter::create(spec, &path, options)?;
    writer.push_info(&item(1, 41))?;
    writer.sync()?;
    let mut reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
    let mut cursor = reader.blocks::<Item>()?;
    assert_eq!(cursor.next().unwrap()?, item(1, 41));
    assert!(cursor.next().is_none());
    writer.push_info(&item(2, 42))?;
    writer.sync()?;
    reader.follow_blocks(&mut cursor)?;
    assert_eq!(cursor.next().unwrap()?, item(2, 42));
    assert!(cursor.next().is_none());
    writer.compact_index()?;
    reader.follow_blocks(&mut cursor)?;
    drop(writer);
    let mut writer = VarlitStreamWriter::open(spec, &path, options)?;
    writer.push_info(&item(3, 43))?;
    writer.sync()?;
    drop(writer);
    reader.follow_blocks(&mut cursor)?;
    assert_eq!(cursor.next().unwrap()?, item(3, 43));
    assert!(cursor.next().is_none());
    drop(cursor);
    drop(reader);
    let mut state = path.as_os_str().to_owned();
    state.push(".vks");
    std::fs::remove_file(state)?;
    let expected = 4 + u64::from(spec.manifest_policy == ManifestPolicy::Embedded);
    assert_eq!(
        bootstrap_stream_checkpoint(spec, &path, options)?.records,
        expected
    );
    let reader = VarlitStreamReader::open(spec, &path, options)?;
    assert_eq!(
        reader
            .blocks::<Item>()?
            .collect::<varlit::Result<Vec<_>>>()?,
        [item(1, 41), item(2, 42), item(3, 43)]
    );
    assert_eq!(reader.verify_all()?, expected);
    let tight = StreamOptions {
        limits: ResourceLimits::MISSING.with_max_logical_payload_len(32),
        ..StreamOptions::default()
    };
    let limited = VarlitStreamReader::open(spec, &path, tight)?;
    assert!(matches!(
        limited
            .blocks::<Item>()?
            .collect::<varlit::Result<Vec<_>>>(),
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
fn indexed_case(spec: FormatSpec) -> TestResult {
    const BLOCKS: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Item>()];
    let spec = spec
        .with_index_policy(IndexPolicy::KeyedOffsetChain)
        .with_computed_schema_hash();
    let plan = DiskIndexPlan::canonical(spec, BLOCKS)?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("index.varlit");
    let options = DiskIndexOptions {
        limits: runtime(IntegrityVerification::AtOpen),
        ..DiskIndexOptions::default()
    };
    let mut writer = VarlitIndexedWriter::create(spec, &path, options, plan)?;
    writer.push_info(&item(1, 41))?;
    writer.sync()?;
    let mut reader = VarlitIndexedReader::open(spec, &path, DiskIndexOptions::default(), plan)?;
    assert_eq!(reader.get::<Item>(&1)?, Some(item(1, 41)));
    writer.push_info(&item(1, 42))?;
    writer.push_info(&item(2, 43))?;
    writer.delete_info::<Item>(&2)?;
    writer.sync()?;
    assert_eq!(reader.get::<Item>(&1)?, Some(item(1, 41)));
    reader.follow()?;
    assert_eq!(reader.get::<Item>(&1)?, Some(item(1, 42)));
    assert_eq!(reader.get::<Item>(&2)?, None);
    writer.compact_index()?;
    reader.follow()?;
    drop(writer);
    let mut writer = VarlitIndexedWriter::open(spec, &path, options, plan)?;
    writer.push_info(&item(3, 44))?;
    writer.sync()?;
    drop(writer);
    reader.follow()?;
    assert_eq!(reader.get::<Item>(&3)?, Some(item(3, 44)));
    drop(reader);
    std::fs::remove_file(disk_index_sidecar_path(&path))?;
    let expected = 6 + u64::from(spec.manifest_policy == ManifestPolicy::Embedded);
    assert_eq!(
        rebuild_disk_index(spec, &path, options, plan)?.records,
        expected
    );
    let reader = VarlitIndexedReader::open(spec, &path, options, plan)?;
    assert_eq!(reader.get::<Item>(&1)?, Some(item(1, 42)));
    assert_eq!(reader.get::<Item>(&2)?, None);
    assert_eq!(reader.get::<Item>(&3)?, Some(item(3, 44)));
    assert_eq!(reader.verify_all()?, expected);
    let tight = DiskIndexOptions {
        limits: ResourceLimits::MISSING.with_max_logical_payload_len(32),
        ..DiskIndexOptions::default()
    };
    let limited = VarlitIndexedReader::open(spec, &path, tight, plan)?;
    assert!(matches!(
        limited.get::<Item>(&1),
        Err(Error::LimitExceeded { .. })
    ));
    Ok(())
}
#[test]
fn format_policies_survive_stream_and_index_lifecycles() -> TestResult {
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
                            // deliberately unsupported by stream/index APIs.
                            // Exercise their rejection instead of filtering them out.
                            let dir = tempfile::tempdir()?;
                            let path = dir.path().join("unsupported.varlit");
                            assert!(matches!(
                                VarlitStreamWriter::create(spec, &path, StreamOptions::default()),
                                Err(Error::StreamingUnsupported)
                            ));
                            const BLOCKS: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Item>()];
                            let plan = DiskIndexPlan::canonical(spec, BLOCKS)?;
                            assert!(matches!(
                                VarlitIndexedWriter::create(
                                    spec,
                                    &path,
                                    DiskIndexOptions::default(),
                                    plan
                                ),
                                Err(Error::StreamingUnsupported)
                            ));
                            unsupported += 1;
                            continue;
                        }
                        stream_case(spec).map_err(|e| format!("stream {integrity:?} {endian:?} {commit:?} {compression:?} {manifest:?}: {e}"))?;
                        indexed_case(spec).map_err(|e| format!("indexed {integrity:?} {endian:?} {commit:?} {compression:?} {manifest:?}: {e}"))?;
                        cases += 1;
                    }
                }
            }
        }
    }
    println!(
        "format_combinations={cases} lifecycle_runs={} unsupported_combinations={unsupported} runtime_limits_and_wrong_format_rejections=passed",
        cases * 2
    );
    Ok(())
}
