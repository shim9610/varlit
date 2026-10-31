#![cfg(feature = "mmap")]

use std::fs::remove_file;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use varlit::{
    Endian, Error, ReadLimits, RecordIndexEntry, VarlitBlock, decode_from_slice, varlit_format,
};

#[cfg(feature = "zero-copy")]
use varlit::{BlockKind, Decoder, Encoder, VarlitDecode, VarlitEncode};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 80, version = 1, kind = "fixed")]
struct MmapPoint {
    x: u32,
    y: u32,
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 81, version = 1, kind = "variable")]
struct MmapMessage {
    #[varlit(field_id = 1)]
    id: u32,
    #[varlit(field_id = 2)]
    text: String,
}

#[cfg(feature = "zero-copy")]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    VarlitBlock,
    zerocopy::FromBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
)]
#[repr(C)]
#[varlit(id = 82, version = 1, kind = "fixed")]
struct RawPoint {
    x: u32,
    y: u32,
}

#[cfg(feature = "zero-copy")]
unsafe impl varlit::VarlitRawFixedBlock for RawPoint {
    const RAW_ENDIAN: Endian = Endian::Little;
}

#[cfg(feature = "zero-copy")]
#[derive(
    Clone,
    Debug,
    PartialEq,
    VarlitBlock,
    zerocopy::FromBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
)]
#[repr(C)]
#[varlit(id = 83, version = 1, kind = "variable")]
struct RawVariable {
    #[varlit(field_id = 1)]
    id: u32,
}

#[cfg(feature = "zero-copy")]
unsafe impl varlit::VarlitRawFixedBlock for RawVariable {
    const RAW_ENDIAN: Endian = Endian::Little;
}

#[cfg(feature = "zero-copy")]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    VarlitBlock,
    zerocopy::FromBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
)]
#[repr(C)]
#[varlit(id = 84, version = 1, kind = "fixed")]
struct BigEndianRawPoint {
    x: u32,
    y: u32,
}

#[cfg(feature = "zero-copy")]
unsafe impl varlit::VarlitRawFixedBlock for BigEndianRawPoint {
    const RAW_ENDIAN: Endian = Endian::Big;
}

#[cfg(feature = "zero-copy")]
#[derive(
    Clone, Copy, Debug, PartialEq, zerocopy::FromBytes, zerocopy::Immutable, zerocopy::KnownLayout,
)]
#[repr(C)]
struct SizeMismatchRawPoint {
    x: u32,
}

#[cfg(feature = "zero-copy")]
impl VarlitEncode for SizeMismatchRawPoint {
    const WIRE_TYPE: varlit::WireType = varlit::WireType::Nested;

    fn encode_varlit(&self, encoder: &mut Encoder) -> varlit::Result<()> {
        encoder.write_u32(self.x);
        encoder.write_u32(self.x.wrapping_add(1));
        Ok(())
    }
}

#[cfg(feature = "zero-copy")]
impl VarlitDecode for SizeMismatchRawPoint {
    const WIRE_TYPE: varlit::WireType = varlit::WireType::Nested;

    fn decode_varlit(decoder: &mut Decoder<'_>) -> varlit::Result<Self> {
        let x = decoder.read_u32()?;
        let _extra = decoder.read_u32()?;
        Ok(Self { x })
    }
}

#[cfg(feature = "zero-copy")]
impl VarlitBlock for SizeMismatchRawPoint {
    const ID: u32 = 85;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0xDE827495F17CC9D9;
    const IS_KEYED: bool = false;
}

#[cfg(feature = "zero-copy")]
unsafe impl varlit::VarlitRawFixedBlock for SizeMismatchRawPoint {
    const RAW_ENDIAN: Endian = Endian::Little;
}

#[cfg(feature = "zero-copy")]
varlit_format! {
    pub struct MmapFormat {
        magic: b"MZ";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        blocks: [
            MmapPoint,
            MmapMessage,
            RawPoint,
            RawVariable,
            BigEndianRawPoint,
            SizeMismatchRawPoint
        ];
    }
}

#[cfg(not(feature = "zero-copy"))]
varlit_format! {
    pub struct MmapFormat {
        magic: b"MZ";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        blocks: [MmapPoint, MmapMessage];
    }
}

#[test]
fn mmap_payload_windows_match_indexed_payload_reads() -> varlit::Result<()> {
    let path = temp_path("payload_windows");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&MmapPoint { x: 10, y: 20 })?;
        file.push(&MmapMessage {
            id: 7,
            text: "hello mmap".to_string(),
        })?;
        file.push(&MmapPoint { x: 30, y: 40 })?;
        file.flush()?;
    }

    let file = MmapFormat::open_readonly(&path)?;
    // SAFETY: The fixture is not modified while the mapping is alive.
    let mmap = unsafe { file.mmap_payloads()? };

    let snapshot = file.index_entries();
    for entry in &snapshot {
        assert_eq!(mmap.payload_window(entry)?, entry.read_payload(&path)?);
    }

    let first = mmap
        .block_payload_window::<MmapPoint>(0)?
        .expect("first fixed payload");
    assert_eq!(
        decode_from_slice::<MmapPoint>(first, Endian::Little)?,
        MmapPoint { x: 10, y: 20 }
    );

    let second = mmap
        .block_payload_window::<MmapPoint>(1)?
        .expect("second fixed payload");
    assert_eq!(
        decode_from_slice::<MmapPoint>(second, Endian::Little)?,
        MmapPoint { x: 30, y: 40 }
    );
    assert!(mmap.block_payload_window::<MmapPoint>(2)?.is_none());

    let variable = mmap
        .block_payload_window::<MmapMessage>(0)?
        .expect("variable payload");
    assert_eq!(
        decode_from_slice::<MmapMessage>(variable, Endian::Little)?,
        MmapMessage {
            id: 7,
            text: "hello mmap".to_string()
        }
    );
    assert!(mmap.block_payload_window::<MmapMessage>(1)?.is_none());

    drop(mmap);
    drop(file);
    cleanup(&path);
    Ok(())
}

#[test]
fn mmap_rejects_entries_outside_snapshot() -> varlit::Result<()> {
    let path = temp_path("forged_entry");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&MmapPoint { x: 1, y: 2 })?;
        file.flush()?;
    }

    let file = MmapFormat::open_readonly(&path)?;
    // SAFETY: The fixture is not modified while the mapping is alive.
    let mmap = unsafe { file.mmap_payloads()? };
    let mut forged: RecordIndexEntry = mmap.index_entries()[0].clone();
    forged.sequence = forged.sequence.saturating_add(1);

    assert!(matches!(
        mmap.payload_window(&forged),
        Err(Error::MmapEntryNotInSnapshot)
    ));

    drop(mmap);
    drop(file);
    cleanup(&path);
    Ok(())
}

#[cfg(all(feature = "integrity", feature = "zero-copy"))]
#[test]
fn mapped_record_reads_verify_crc_before_exposing_bytes() -> varlit::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    use varlit::{IndexPolicy, IntegrityPolicy, IntegrityVerification};

    for policy in [IntegrityPolicy::Crc32, IntegrityPolicy::Crc32WithHeader] {
        for footers in [false, true] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("integrity.varlit");
            let spec = MmapFormat::spec()
                .with_integrity_policy(policy)
                .with_index_policy(IndexPolicy::ScanOnOpen.with_block_offset_chain(footers))
                .with_resource_limits(
                    ReadLimits::UNTRUSTED
                        .with_integrity_verification(IntegrityVerification::OnDemand),
                );
            let value = RawPoint { x: 10, y: 20 };
            let expected = varlit::encode_to_vec(&value, Endian::Little)?;
            let mut writer = spec.create(&path)?;
            writer.push(&value)?;
            writer.sync()?;
            drop(writer);
            let reader = spec.open_readonly(&path)?;
            let entry = reader.index_entries()[0].clone();
            // SAFETY: these mappings are dropped before the fixture is edited.
            let mapped = unsafe { reader.mmap_payloads()? };
            for _ in 0..3 {
                assert_eq!(unsafe { mapped.raw_fixed::<RawPoint>(0)? }, Some(&value));
                assert_eq!(mapped.payload_window(&entry)?, expected);
            }
            let window = unsafe { reader.mmap_payload_window(&entry)? };
            assert_eq!(window.as_bytes(), mapped.payload_window(&entry)?);
            drop(window);
            drop(mapped);
            drop(reader);

            let mut file = std::fs::OpenOptions::new().write(true).open(&path)?;
            file.seek(SeekFrom::Start(entry.payload_offset))?;
            file.write_all(&999u32.to_le_bytes())?;
            file.sync_all()?;
            drop(file);

            // The default OnDemand policy deliberately opens the damaged file.
            // Every path returning its payload must nevertheless reject it.
            let reader = spec.open_readonly(&path)?;
            assert!(matches!(
                reader.blocks::<RawPoint>()?.get(0),
                Err(Error::ChecksumMismatch { .. })
            ));
            // SAFETY: the damaged file remains immutable throughout these maps.
            let mapped = unsafe { reader.mmap_payloads()? };
            for _ in 0..2 {
                assert!(matches!(
                    mapped.payload_window(&entry),
                    Err(Error::ChecksumMismatch { .. })
                ));
                assert!(matches!(
                    mapped.block_payload_window::<RawPoint>(0),
                    Err(Error::ChecksumMismatch { .. })
                ));
                assert!(matches!(
                    unsafe { mapped.raw_fixed::<RawPoint>(0) },
                    Err(Error::ChecksumMismatch { .. })
                ));
                assert!(matches!(
                    unsafe { reader.mmap_payload_window(&entry) },
                    Err(Error::ChecksumMismatch { .. })
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn mmap_constructor_enforces_captured_mapping_length_limit() -> varlit::Result<()> {
    let path = temp_path("mapping_limit");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&MmapPoint { x: 1, y: 2 })?;
        file.flush()?;
    }
    let file_len = std::fs::metadata(&path)?.len();
    let runtime = ReadLimits::finite_all(u64::MAX).with_max_mmap_len(file_len - 1);
    let reader = MmapFormat::open_reader_with_limits(&path, runtime)?;

    // SAFETY: No mapping is created because the constructor rejects the limit.
    assert!(matches!(
        unsafe { reader.mmap_payloads() },
        Err(Error::LimitExceeded {
            resource: "mmap length",
            ..
        })
    ));

    cleanup(&path);
    Ok(())
}

#[test]
fn mmap_snapshot_refreshes_after_append() -> varlit::Result<()> {
    let path = temp_path("snapshot_append");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&MmapPoint { x: 1, y: 2 })?;
        file.flush()?;
    }

    let mut file = MmapFormat::open(&path)?;
    // SAFETY: Appends occur only after the snapshot mapping is dropped.
    let snapshot = unsafe { file.mmap_payloads()? };
    assert_eq!(snapshot.len(), 1);
    drop(snapshot);

    file.push(&MmapPoint { x: 3, y: 4 })?;
    file.flush()?;

    // SAFETY: No mutation occurs while the fresh mapping is alive.
    let fresh = unsafe { file.mmap_payloads()? };
    assert_eq!(fresh.len(), 2);
    let appended = fresh
        .block_payload_window::<MmapPoint>(1)?
        .expect("appended fixed payload");
    assert_eq!(
        decode_from_slice::<MmapPoint>(appended, Endian::Little)?,
        MmapPoint { x: 3, y: 4 }
    );

    drop(fresh);
    drop(file);
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "zero-copy")]
#[test]
fn zero_copy_raw_fixed_successfully_views_canonical_little_endian_payload() -> varlit::Result<()> {
    let path = temp_path("raw_fixed");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&RawPoint {
            x: 0x1122_3344,
            y: 0x5566_7788,
        })?;
        file.flush()?;
    }

    let file = MmapFormat::open_readonly(&path)?;
    // SAFETY: The fixture is not modified while the mapping is alive.
    let mmap = unsafe { file.mmap_payloads()? };
    assert_eq!(
        unsafe { mmap.raw_fixed::<RawPoint>(0) }?,
        Some(&RawPoint {
            x: 0x1122_3344,
            y: 0x5566_7788
        })
    );
    assert!(unsafe { mmap.raw_fixed::<RawPoint>(1) }?.is_none());

    drop(mmap);
    drop(file);
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "zero-copy")]
#[test]
fn zero_copy_rejects_variable_raw_block() -> varlit::Result<()> {
    let path = temp_path("raw_variable");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&RawVariable { id: 9 })?;
        file.flush()?;
    }

    let file = MmapFormat::open_readonly(&path)?;
    // SAFETY: The fixture is not modified while the mapping is alive.
    let mmap = unsafe { file.mmap_payloads()? };

    assert!(matches!(
        unsafe { mmap.raw_fixed::<RawVariable>(0) },
        Err(Error::ZeroCopyBlockKindMismatch {
            actual: BlockKind::Variable
        })
    ));

    drop(mmap);
    drop(file);
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "zero-copy")]
#[test]
fn zero_copy_rejects_raw_endian_mismatch() -> varlit::Result<()> {
    let path = temp_path("raw_endian");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&BigEndianRawPoint { x: 1, y: 2 })?;
        file.flush()?;
    }

    let file = MmapFormat::open_readonly(&path)?;
    // SAFETY: The fixture is not modified while the mapping is alive.
    let mmap = unsafe { file.mmap_payloads()? };

    assert!(matches!(
        unsafe { mmap.raw_fixed::<BigEndianRawPoint>(0) },
        Err(Error::ZeroCopyEndianMismatch {
            expected: Endian::Little,
            actual: Endian::Big
        })
    ));

    drop(mmap);
    drop(file);
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "zero-copy")]
#[test]
fn zero_copy_rejects_raw_payload_size_mismatch() -> varlit::Result<()> {
    let path = temp_path("raw_size");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&SizeMismatchRawPoint { x: 5 })?;
        file.flush()?;
    }

    let file = MmapFormat::open_readonly(&path)?;
    // SAFETY: The fixture is not modified while the mapping is alive.
    let mmap = unsafe { file.mmap_payloads()? };

    assert!(matches!(
        unsafe { mmap.raw_fixed::<SizeMismatchRawPoint>(0) },
        Err(Error::ZeroCopyPayloadSizeMismatch {
            expected: 4,
            actual: 8
        })
    ));

    drop(mmap);
    drop(file);
    cleanup(&path);
    Ok(())
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempPath {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for TempPath {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

impl AsRef<std::path::Path> for TempPath {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

fn temp_path(name: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir.path().join(format!(
        "varlit_mmap_zero_copy_{name}_{}_{}_{}.vrv",
        std::process::id(),
        std::thread::current().name().unwrap_or("anon"),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    TempPath { path, _dir: dir }
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}

#[test]
fn individual_mapping_works_without_a_resident_directory_or_whole_file_budget() -> varlit::Result<()>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("window.varlit");
    {
        let mut file = MmapFormat::create(&path)?;
        for x in 0..32 {
            file.push(&MmapPoint { x, y: x + 1 })?;
        }
        file.flush()?;
    }
    let spec = MmapFormat::spec().with_resource_limits(ReadLimits::STANDARD.with_max_mmap_len(8));
    let mut entries = Vec::new();
    let file = varlit::VarlitFile::open_readonly_without_directory(spec, &path, &mut entries)?;
    let points: Vec<_> = entries
        .iter()
        .filter(|r| r.block_id == MmapPoint::ID)
        .collect();
    // SAFETY: the writer is dropped and no process changes this fixture.
    let first = unsafe { file.mmap_payload_window(points[0])? };
    // SAFETY: same immutable fixture; both windows have independent owners.
    let last = unsafe { file.mmap_payload_window(points[31])? };
    assert_eq!(
        decode_from_slice::<MmapPoint>(first.as_bytes(), Endian::Little)?,
        MmapPoint { x: 0, y: 1 }
    );
    assert_eq!(
        decode_from_slice::<MmapPoint>(last.as_bytes(), Endian::Little)?,
        MmapPoint { x: 31, y: 32 }
    );
    drop(first);
    assert_eq!(last.as_bytes().len(), 8);
    let mut forged = points[31].clone();
    forged.block_id += 1;
    // SAFETY: immutable fixture; forged metadata is refused before mapping.
    assert!(matches!(
        unsafe { file.mmap_payload_window(&forged) },
        Err(Error::MmapEntryNotInSnapshot)
    ));
    Ok(())
}
