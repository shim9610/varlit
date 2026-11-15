use std::io::{Seek, SeekFrom, Write};
#[path = "common/private_tempdir.rs"]
mod private_tempdir;
use std::sync::atomic::{AtomicBool, Ordering};
use varlit::{FileCheckStatus, VarlitBlock, VarlitFile, varlit_format};

varlit_format! {
    pub format CheckedFormat {
        magic: b"CHECKED"; version: 1; endian: little; schema_hash: computed;
        blocks {
            fixed Point(id = 1) { x: u64, y: i64 }
            variable Reading(id = 2, key = [station]) { station: u64, samples: Vec<u64> }
            fixed Flag(id = 3) { value: bool }
        }
    }
}

varlit_format! {
    pub struct Chained {
        magic: b"CHECKCHAIN"; version: 1; endian: little; schema_hash: computed;
        index: [block_offset_chain, keyed_offset_chain]; commit: record_footer;
        blocks: [Point, Reading, Flag];
    }
}

fn populate(spec: varlit::FormatSpec, path: &std::path::Path) -> varlit::Result<u64> {
    let mut writer = VarlitFile::create(spec, path)?;
    writer.push(&Point {
        x: u64::MAX,
        y: i64::MIN,
    })?;
    writer.push_keyed(&Reading {
        station: 9,
        samples: vec![0, 1, u64::MAX],
    })?;
    let flag = writer.push_info(&Flag { value: true })?;
    writer.flush()?;
    writer.sync()?;
    let offset = writer.record_entry_at(flag.record_offset)?.payload_offset;
    Ok(offset)
}

#[test]
fn generated_registry_checks_every_user_type_and_preserves_file() -> varlit::Result<()> {
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("file.varlit");
    populate(CheckedFormat::spec(), &path)?;
    let before = std::fs::read(&path)?;
    let report = CheckedFormat::check_file(&path)?;
    assert!(report.passed(), "{report:#?}");
    for id in [1, 2, 3] {
        let check = report
            .checks()
            .iter()
            .find(|c| c.block_id == Some(id))
            .unwrap();
        assert_eq!(check.status, FileCheckStatus::Passed);
        assert_eq!(check.records_checked, 1);
    }
    assert!(!report.preconditions().is_empty());
    assert_eq!(std::fs::read(&path)?, before);
    Ok(())
}

#[test]
fn missing_samples_fail_strict_mode_before_creating_any_file() {
    let dir = private_tempdir::private_tempdir().unwrap();
    let path = dir.path().join("sample.varlit");
    let report = CheckedFormat::self_test(&path)
        .with_block(Point { x: 1, y: 2 })
        .cleanup(true)
        .run_strict();
    assert!(!report.passed(), "{report:#?}");
    assert!(report.failures().any(|c| c.name == "coverage.block.2"));
    assert!(report.failures().any(|c| c.name == "coverage.block.3"));
    assert!(!path.exists());
    // Existing sample-only behavior is unchanged.
    assert!(
        CheckedFormat::self_test(&path)
            .with_block(Point { x: 1, y: 2 })
            .cleanup(true)
            .run()
            .passed()
    );
}

#[test]
fn complete_sample_template_roundtrips_all_declared_types() {
    let dir = private_tempdir::private_tempdir().unwrap();
    let report = CheckedFormat::self_test(dir.path().join("sample.varlit"))
        .with_block(Point { x: 1, y: -2 })
        .with_keyed_block(Reading {
            station: 42,
            samples: vec![0, u64::MAX],
        })
        .with_block(Flag { value: false })
        .cleanup(true)
        .run_strict();
    assert!(report.passed(), "{report:#?}");
}

#[test]
fn structurally_readable_but_invalid_bool_names_block_and_offset() -> varlit::Result<()> {
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("bad.varlit");
    let offset = populate(CheckedFormat::spec(), &path)?;
    let mut raw = std::fs::OpenOptions::new().write(true).open(&path)?;
    raw.seek(SeekFrom::Start(offset))?;
    raw.write_all(&[2])?;
    drop(raw);
    let report = CheckedFormat::check_file(&path)?;
    assert!(!report.passed());
    let failed = report
        .checks()
        .iter()
        .find(|c| c.block_id == Some(3))
        .unwrap();
    assert_eq!(failed.status, FileCheckStatus::Violated, "{report:#?}");
    assert!(failed.offset.is_some());
    assert!(failed.error.is_some());
    Ok(())
}

#[test]
fn missing_validator_or_missing_file_data_never_passes() -> varlit::Result<()> {
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("file.varlit");
    populate(CheckedFormat::spec(), &path)?;
    let missing = varlit::check_file(
        CheckedFormat::spec(),
        &path,
        &[
            Point::__VARLIT_RECORD_VALIDATOR,
            Reading::__VARLIT_RECORD_VALIDATOR,
        ],
    )?;
    assert!(!missing.passed());
    assert_eq!(
        missing
            .checks()
            .iter()
            .find(|c| c.code == "registry.complete")
            .unwrap()
            .status,
        FileCheckStatus::Unverified
    );
    let sparse = dir.path().join("sparse.varlit");
    {
        let mut writer = CheckedFormat::create(&sparse)?;
        writer.push(&Point { x: 1, y: 2 })?;
        writer.sync()?;
    }
    let report = CheckedFormat::check_file(&sparse)?;
    assert!(!report.passed());
    assert_eq!(
        report
            .checks()
            .iter()
            .find(|c| c.block_id == Some(2))
            .unwrap()
            .status,
        FileCheckStatus::Unverified
    );
    Ok(())
}

#[test]
fn active_writer_is_unverified_and_is_not_touched() -> varlit::Result<()> {
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("busy.varlit");
    let writer = CheckedFormat::create(&path)?;
    let report = CheckedFormat::check_file(&path)?;
    assert!(!report.passed());
    let check = report
        .checks()
        .iter()
        .find(|c| c.code == "file.writer_marker_absent")
        .unwrap();
    assert_eq!(check.status, FileCheckStatus::Unverified);
    drop(writer);
    Ok(())
}

#[test]
fn derived_key_checker_rejects_a_predecessor_for_another_key() -> varlit::Result<()> {
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("wrong-key.varlit");
    {
        let mut writer = VarlitFile::create(Chained::spec(), &path)?;
        writer.push(&Point { x: 1, y: 2 })?;
        writer.push(&Flag { value: true })?;
        let first = writer.push_keyed_info(&Reading {
            station: 1,
            samples: vec![],
        })?;
        writer.push_with_prev_key_info(
            &Reading {
                station: 2,
                samples: vec![],
            },
            Some(first.record_offset),
        )?;
        writer.sync()?;
    }
    let report = Chained::check_file(&path)?;
    assert!(!report.passed());
    let failed = report
        .checks()
        .iter()
        .find(|c| c.block_id == Some(2))
        .unwrap();
    assert_eq!(failed.status, FileCheckStatus::Violated, "{report:#?}");
    assert!(matches!(
        failed.error,
        Some(varlit::Error::InvalidCanonicalEncoding(
            "keyed predecessor belongs to a different key"
        ))
    ));
    Ok(())
}

#[test]
fn exhausted_budget_is_unverified_instead_of_success() -> varlit::Result<()> {
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("limited.varlit");
    populate(CheckedFormat::spec(), &path)?;
    let report = CheckedFormat::check_file_with_resource_limits(
        &path,
        varlit::ReadLimits::STANDARD.with_max_materialized_bytes(0),
    )?;
    assert!(!report.passed());
    assert!(
        report
            .checks()
            .iter()
            .any(|c| c.status == FileCheckStatus::Unverified && c.error.is_some()),
        "{report:#?}"
    );
    Ok(())
}

#[test]
fn nonresident_records_are_decoded_too() -> varlit::Result<()> {
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("nonresident.varlit");
    let spec = Chained::spec().with_block_residency(&[varlit::BlockResidencyDescriptor {
        block_id: 3,
        resident: false,
    }]);
    let offset = populate(spec, &path)?;
    let validators = [
        Point::__VARLIT_RECORD_VALIDATOR,
        Reading::__VARLIT_RECORD_VALIDATOR,
        Flag::__VARLIT_RECORD_VALIDATOR,
    ];
    assert!(varlit::check_file(spec, &path, &validators)?.passed());
    let mut raw = std::fs::OpenOptions::new().write(true).open(&path)?;
    raw.seek(SeekFrom::Start(offset))?;
    raw.write_all(&[2])?;
    drop(raw);
    let report = varlit::check_file(spec, &path, &validators)?;
    assert!(!report.passed());
    assert_eq!(
        report
            .checks()
            .iter()
            .find(|c| c.block_id == Some(3))
            .unwrap()
            .status,
        FileCheckStatus::Violated
    );
    Ok(())
}

static PANIC_DECODE: AtomicBool = AtomicBool::new(false);
#[derive(Clone, Debug, PartialEq)]
pub struct Callback(u64);
impl varlit::VarlitEmpty for Callback {
    fn empty() -> Self {
        Self(0)
    }
}
impl varlit::VarlitEncode for Callback {
    const WIRE_TYPE: varlit::WireType = varlit::WireType::U64;
    const SCHEMA_ID: u64 = 123456;
    fn encode_varlit(&self, encoder: &mut varlit::Encoder) -> varlit::Result<()> {
        varlit::VarlitEncode::encode_varlit(&self.0, encoder)
    }
}
impl varlit::VarlitDecode for Callback {
    const WIRE_TYPE: varlit::WireType = varlit::WireType::U64;
    const SCHEMA_ID: u64 = 123456;
    fn decode_varlit(decoder: &mut varlit::Decoder<'_>) -> varlit::Result<Self> {
        assert!(
            !PANIC_DECODE.swap(false, Ordering::SeqCst),
            "intentional codec panic"
        );
        Ok(Self(<u64 as varlit::VarlitDecode>::decode_varlit(decoder)?))
    }
}
varlit_format! {
    pub format CallbackFormat {
        magic: b"CALLBACK"; version: 1; endian: little; schema_hash: computed;
        blocks { fixed CallbackBlock(id = 8) { value: Callback } }
    }
}

#[test]
fn codec_panic_cannot_be_overwritten_by_a_later_success() -> varlit::Result<()> {
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("callback.varlit");
    {
        let mut file = CallbackFormat::create(&path)?;
        file.push(&CallbackBlock { value: Callback(1) })?;
        file.push(&CallbackBlock { value: Callback(2) })?;
        file.sync()?;
    }
    PANIC_DECODE.store(true, Ordering::SeqCst);
    let report = CallbackFormat::check_file(&path)?;
    assert!(!report.passed(), "{report:#?}");
    let check = report
        .checks()
        .iter()
        .find(|c| c.block_id == Some(8))
        .unwrap();
    assert_eq!(check.status, FileCheckStatus::Unverified);
    assert_eq!(check.records_checked, 0);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn trusted_read_policy_does_not_disable_stored_crc_verification() -> varlit::Result<()> {
    varlit_format! {
        struct CrcFormat {
            magic: b"CRC-CHECK"; version: 1; endian: little; schema_hash: computed;
            integrity: crc32; blocks: [Point, Reading, Flag];
        }
    }
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("crc.varlit");
    let offset = populate(CrcFormat::spec(), &path)?;
    let mut raw = std::fs::OpenOptions::new().write(true).open(&path)?;
    raw.seek(SeekFrom::Start(offset))?;
    raw.write_all(&[0])?; // Another valid bool: typed decoding alone is insufficient.
    drop(raw);
    let report = CrcFormat::check_file_with_resource_limits(
        &path,
        varlit::ReadLimits::STANDARD.with_read_validation(varlit::ReadValidation::TRUSTED),
    )?;
    assert!(!report.passed());
    assert_eq!(
        report
            .checks()
            .iter()
            .find(|c| c.code == "file.integrity")
            .unwrap()
            .status,
        FileCheckStatus::Violated,
        "{report:#?}"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trailing(u64);
impl varlit::VarlitEncode for Trailing {
    const WIRE_TYPE: varlit::WireType = varlit::WireType::Bytes;
    const SCHEMA_ID: u64 = 998877;
    fn encode_varlit(&self, e: &mut varlit::Encoder) -> varlit::Result<()> {
        varlit::VarlitEncode::encode_varlit(&self.0, e)?;
        e.write_u8(42);
        Ok(())
    }
}
impl varlit::VarlitDecode for Trailing {
    const WIRE_TYPE: varlit::WireType = varlit::WireType::Bytes;
    const SCHEMA_ID: u64 = 998877;
    fn decode_varlit(d: &mut varlit::Decoder<'_>) -> varlit::Result<Self> {
        Ok(Self(<u64 as varlit::VarlitDecode>::decode_varlit(d)?))
    }
}
impl VarlitBlock for Trailing {
    const ID: u32 = 9;
    const VERSION: u16 = 1;
    const KIND: varlit::BlockKind = varlit::BlockKind::Fixed;
    const ENDIAN: Option<varlit::Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 998877;
}
varlit_format! {
    struct TrailingFormat {
        magic: b"TRAILING"; version: 1; endian: little; schema_hash: computed;
        blocks: [Trailing];
    }
}

#[test]
fn manual_decoder_must_consume_the_complete_payload() -> varlit::Result<()> {
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("trailing.varlit");
    {
        let mut file = TrailingFormat::create(&path)?;
        file.push(&Trailing(7))?;
        file.sync()?;
    }
    let report = TrailingFormat::check_file(&path)?;
    assert!(!report.passed());
    let failed = report
        .checks()
        .iter()
        .find(|c| c.block_id == Some(9))
        .unwrap();
    assert!(
        matches!(
            failed.error,
            Some(varlit::Error::TrailingBytes { remaining: 1 })
        ),
        "{report:#?}"
    );
    Ok(())
}

varlit_format! {
    pub format DiskChecked {
        magic: b"DISK-CHECK"; version: 1; endian: little; schema_hash: computed;
        index: keyed_offset_chain;
        blocks {
            variable DiskReading(id = 4, key = [station], key_index = disk) {
                station: u64, value: u64,
            }
        }
    }
}

#[test]
fn declared_disk_companion_is_not_silently_certified() -> varlit::Result<()> {
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("disk.varlit");
    {
        let mut file = VarlitFile::create(DiskChecked::spec(), &path)?;
        file.push_keyed(&DiskReading {
            station: 1,
            value: 2,
        })?;
        file.sync()?;
    }
    let report = DiskChecked::check_file(&path)?;
    assert!(!report.passed());
    let check = report
        .checks()
        .iter()
        .find(|c| c.code == "sidecar.coherence")
        .unwrap();
    assert_eq!(check.status, FileCheckStatus::Unverified, "{report:#?}");
    Ok(())
}

varlit_format! {
    struct BufferedPublication {
        magic: b"PUBLISH-CHECK"; version: 1; endian: little; schema_hash: computed;
        commit: transaction_marker(on_flush);
        blocks: [Point, Reading, Flag];
    }
}

#[test]
fn published_prefix_does_not_certify_unpublished_tail_bytes() -> varlit::Result<()> {
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("partial-publication.varlit");
    populate(BufferedPublication::spec(), &path)?;
    {
        let mut file = VarlitFile::open(BufferedPublication::spec(), &path)?;
        file.push(&Flag { value: false })?;
        // Close without the explicit publication operation.
    }
    let report = BufferedPublication::check_file(&path)?;
    assert!(!report.passed(), "{report:#?}");
    assert_eq!(
        report
            .checks()
            .iter()
            .find(|c| c.code == "file.published_extent")
            .unwrap()
            .status,
        FileCheckStatus::Unverified,
        "{report:#?}"
    );
    Ok(())
}

varlit_format! {
    format MatrixChecked {
        magic: b"MATRIX-CHECK"; version: 1; endian: little; schema_hash: computed;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks { matrix Cell(id = 10, dims = [row, col], category = data) { value: u64, } }
    }
}

#[test]
fn matrix_samples_work_but_uninspected_cell_payloads_are_not_certified() -> varlit::Result<()> {
    let dir = private_tempdir::private_tempdir()?;
    let path = dir.path().join("matrix.varlit");
    let dims = varlit::MatrixDimensions::from_pairs([("row", 2), ("col", 2)]);
    let samples = MatrixChecked::self_test(&path)
        .with_dims(dims.clone())
        .with_matrix_cell(varlit::MatrixKey::new(0, 0), Cell { value: 99 })
        .run_strict();
    assert!(samples.passed(), "{samples:#?}");
    let report = MatrixChecked::check_file(&path)?;
    assert!(!report.passed(), "{report:#?}");
    assert_eq!(
        report
            .checks()
            .iter()
            .find(|c| c.block_id == Some(10))
            .unwrap()
            .status,
        FileCheckStatus::Unverified
    );
    Ok(())
}

#[test]
fn schema_warnings_do_not_become_a_strict_pass() {
    let dir = private_tempdir::private_tempdir().unwrap();
    let path = dir.path().join("unbound.varlit");
    let spec = varlit::FormatSpec {
        schema_hash: 0,
        ..CheckedFormat::spec()
    };
    let report = spec
        .self_test(&path)
        .with_block(Point { x: 1, y: 2 })
        .with_keyed_block(Reading {
            station: 1,
            samples: vec![],
        })
        .with_block(Flag { value: true })
        .run_strict();
    assert!(!report.passed());
    assert!(!path.exists());
}

varlit_format! {
    format PhysicalChecked {
        magic: b"PHYC"; version: 1; preset: none;
        layout {
            file_header Header { bytes signature = b"PHYC"; u16 version = 1; }
            segment Data repeat until_eof {
                lead_in Lead {
                    bytes tag = b"DATA";
                    i64 next = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw = finalize(target = raw_region_start, relative_to = after_lead_in);
                }
                metadata Meta; raw_region Raw;
            }
        }
    }
}

#[test]
fn physical_layout_checks_structure_without_claiming_raw_type_coverage() -> varlit::Result<()> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("physical.bin");
    {
        let mut writer = PhysicalChecked::create_layout_writer(&path)?;
        writer.write_segment(varlit::SegmentWrite {
            name: "Data",
            fields: &[],
            footer_fields: &[],
            metadata: b"meta",
            raw: b"data",
        })?;
        writer.flush()?;
    }
    let report = PhysicalChecked::check_file(&path)?;
    assert!(!report.passed());
    assert_eq!(
        report
            .checks()
            .iter()
            .find(|c| c.code == "file.structure")
            .unwrap()
            .status,
        FileCheckStatus::Passed,
        "{report:#?}"
    );
    assert_eq!(
        report
            .checks()
            .iter()
            .find(|c| c.code == "registry.complete")
            .unwrap()
            .status,
        FileCheckStatus::Unverified
    );
    let mut file = std::fs::OpenOptions::new().write(true).open(&path)?;
    file.write_all(b"BAD!")?;
    drop(file);
    let report = PhysicalChecked::check_file(&path)?;
    assert_eq!(
        report
            .checks()
            .iter()
            .find(|c| c.code == "file.structure")
            .unwrap()
            .status,
        FileCheckStatus::Violated
    );
    Ok(())
}

#[test]
fn delete_then_reinsert_uses_the_tombstone_key_as_predecessor() -> varlit::Result<()> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("delete-reinsert.varlit");
    populate(Chained::spec(), &path)?;
    {
        let mut file = VarlitFile::open(Chained::spec(), &path)?;
        file.delete::<Reading>(&varlit::VarlitKeyedBlock::key(&Reading {
            station: 9,
            samples: vec![],
        }))?;
        file.push_keyed(&Reading {
            station: 9,
            samples: vec![5, 7],
        })?;
        file.sync()?;
    }
    let report = Chained::check_file(&path)?;
    assert!(report.passed(), "{report:#?}");
    Ok(())
}

#[test]
fn malformed_standalone_tombstone_payload_does_not_pass() -> varlit::Result<()> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("bad-tombstone.varlit");
    populate(Chained::spec(), &path)?;
    let offset = {
        let mut file = VarlitFile::open(Chained::spec(), &path)?;
        let key = varlit::VarlitKeyedBlock::key(&Reading {
            station: 9,
            samples: vec![],
        });
        let tombstone = file.delete_info::<Reading>(&key)?;
        file.sync()?;
        file.record_entry_at(tombstone.record_offset)?
            .payload_offset
    };
    let mut bytes = std::fs::OpenOptions::new().write(true).open(&path)?;
    bytes.seek(SeekFrom::Start(offset + 4))?;
    bytes.write_all(&u64::MAX.to_le_bytes())?;
    drop(bytes);
    let report = Chained::check_file(&path)?;
    assert!(!report.passed());
    assert!(
        report
            .checks()
            .iter()
            .any(|c| c.block_id == Some(2) && c.status == FileCheckStatus::Violated),
        "{report:#?}"
    );
    Ok(())
}

impl varlit::VarlitMerge for Reading {
    type Op = u64;
    fn apply_op(&mut self, op: u64) -> varlit::Result<()> {
        self.samples.push(op);
        Ok(())
    }
}

#[test]
fn unregistered_merge_operation_codec_is_explicitly_unverified() -> varlit::Result<()> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("operation.varlit");
    populate(Chained::spec(), &path)?;
    {
        let mut file = VarlitFile::open(Chained::spec(), &path)?;
        let key = varlit::VarlitKeyedBlock::key(&Reading {
            station: 9,
            samples: vec![],
        });
        file.push_op::<Reading>(&key, &7)?;
        file.sync()?;
    }
    let report = Chained::check_file(&path)?;
    assert!(!report.passed());
    assert!(
        report
            .checks()
            .iter()
            .any(|c| c.code == "registry.complete" && c.status == FileCheckStatus::Unverified),
        "{report:#?}"
    );
    Ok(())
}

#[test]
fn a_tombstone_does_not_replace_a_required_value_sample() -> varlit::Result<()> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("tombstone-only.varlit");
    {
        let mut file = VarlitFile::create(Chained::spec(), &path)?;
        file.push(&Point { x: 1, y: 2 })?;
        file.push(&Flag { value: true })?;
        let key = varlit::VarlitKeyedBlock::key(&Reading {
            station: 9,
            samples: vec![],
        });
        file.delete::<Reading>(&key)?;
        file.sync()?;
    }
    let report = Chained::check_file(&path)?;
    assert!(!report.passed());
    let block = report
        .checks()
        .iter()
        .find(|c| c.block_id == Some(2))
        .unwrap();
    assert_eq!(block.records_checked, 0);
    assert_eq!(block.status, FileCheckStatus::Unverified, "{report:#?}");
    Ok(())
}
