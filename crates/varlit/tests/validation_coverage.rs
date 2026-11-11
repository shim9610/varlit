//! New source-level interfaces need executable valid/invalid contracts.
//! The frozen legacy inventory records the existing surface, not a claim of
//! complete historical coverage. New entries cannot join by refreshing it.
#![cfg(all(feature = "integrity", feature = "compression-zstd", feature = "mmap"))]

#[path = "common/private_tempdir.rs"]
mod private_tempdir;
use quote::ToTokens;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use syn::visit::Visit;
#[allow(dead_code)]
#[path = "support/source_audit.rs"]
mod source_audit;
use source_audit::{Audit, Surface};
#[allow(dead_code)]
#[path = "support/validation_cases.rs"]
mod cases;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn inventory() -> BTreeMap<String, Surface> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args([
            "metadata",
            "--locked",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
        ])
        .current_dir(&root)
        .output()
        .expect("run Cargo metadata for coverage inventory");
    assert!(
        output.status.success(),
        "Cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let members: BTreeSet<_> = metadata["workspace_members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect();
    let mut surfaces = BTreeMap::new();
    for package in metadata["packages"].as_array().unwrap() {
        if !members.contains(package["id"].as_str().unwrap()) {
            continue;
        }
        let name = package["name"].as_str().unwrap();
        let Some(library) = package["targets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|target| {
                target["kind"].as_array().unwrap().iter().any(|kind| {
                    matches!(
                        kind.as_str(),
                        Some("lib" | "proc-macro" | "rlib" | "dylib" | "cdylib" | "staticlib")
                    )
                })
            })
        else {
            continue;
        };
        let entrypoint = Path::new(library["src_path"].as_str().unwrap());
        let prefix = match name {
            "varlit" => "facade",
            "varlit-core" => "core",
            "varlit-macros" => "macros",
            other => other,
        };
        let mut audit = Audit {
            root: entrypoint.parent().unwrap().into(),
            crate_name: prefix.into(),
            inventory_only: true,
            generator_crate: library["kind"]
                .as_array()
                .unwrap()
                .iter()
                .any(|kind| kind == "proc-macro"),
            ..Audit::default()
        };
        audit.inspect_file(entrypoint);
        assert!(audit.errors.is_empty(), "{}", audit.errors.join("\n"));
        // Empty libraries still own their Cargo features and will be inventoried
        // as soon as a declaration is added. No directory allowlist to maintain.
        surfaces.extend(audit.inventory);
        for (feature, dependencies) in package["features"].as_object().unwrap() {
            let mut dependencies: Vec<_> = dependencies
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            dependencies.sort_unstable();
            let signature =
                source_audit::fingerprint(&serde_json::to_string(&dependencies).unwrap());
            surfaces.insert(
                format!("feature/{name}/{feature}@{signature}"),
                Surface {
                    signature,
                    reference: feature.clone(),
                },
            );
        }
    }
    assert!(!surfaces.is_empty(), "empty workspace interface inventory");
    surfaces
}

struct Contract {
    symbol: &'static str,
    exercise: Option<&'static str>,
    valid_source: &'static str,
    invalid_source: &'static str,
    valid: fn() -> TestResult,
    invalid: fn() -> TestResult,
}

// A row contains runnable callbacks, not a test-name string. Both bodies are
// compiled and executed by the gate. The source check catches empty/stub rows;
// choosing meaningful assertions still requires review.
macro_rules! contract {
    (@exercise) => { None };
    (@exercise $name:literal) => { Some($name) };
    ($symbol:literal, $(generated $exercise:literal,)? valid $valid:block, invalid $invalid:block) => {
        Contract {
            symbol: $symbol,
            exercise: contract!(@exercise $($exercise)?),
            valid_source: stringify!($valid),
            invalid_source: stringify!($invalid),
            valid: || -> TestResult { $valid },
            invalid: || -> TestResult { $invalid },
        }
    };
}

#[derive(Clone, Debug, PartialEq, varlit::VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "fixed")]
struct Probe {
    value: u64,
}

varlit::varlit_format! {
    struct ProbeFormat { magic: b"VCONTRACT"; version: 1; endian: little; integrity: crc32_with_header; blocks: [Probe]; }
}

varlit::varlit_format! {
    pub format OwnershipHeader {
        magic: b"OWHD"; version: 1; preset: none;
        layout {
            file_header Header { bytes magic = b"OWHD"; u16 kind; }
            segment Data repeat until_eof {
                lead_in Lead { bytes magic = b"DATA"; u16 kind;
                    i64 next = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw = finalize(target = raw_region_start, relative_to = after_lead_in);
                }
                metadata Meta; raw_region Raw;
            }
        }
    }
}
varlit::varlit_format! {
    pub format OwnershipIndex {
        magic: b"OWIX"; version: 1; index: keyed_offset_chain;
        blocks { variable OwnedEntry(id = 71, key = [id], key_index = disk) { id: u64, value: u64, } }
    }
}

fn probe(
    damaged: bool,
) -> Result<(tempfile::TempDir, varlit::VarlitFile, u64), Box<dyn std::error::Error>> {
    use std::io::{Seek, SeekFrom, Write};
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("contract.varlit");
    let mut writer = ProbeFormat::spec().create(&path)?;
    let record = writer.push_info(&Probe { value: 19 })?;
    writer.sync()?;
    drop(writer);
    let reader = ProbeFormat::spec().open_readonly(&path)?;
    if damaged {
        let entry = reader.record_entry_at(record.record_offset)?;
        let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
        file.seek(SeekFrom::Start(entry.payload_offset))?;
        file.write_all(&[0x7f])?;
    }
    Ok((directory, reader, record.record_offset))
}

/// `read_block_at` into a fresh `Probe`, returning it by value for the asserts.
fn read_probe_at(reader: &varlit::VarlitFile, offset: u64) -> varlit::Result<Probe> {
    let mut probe = <Probe as varlit::VarlitEmpty>::empty();
    reader.read_block_at::<Probe>(offset, &mut Vec::new(), &mut probe)?;
    Ok(probe)
}

fn read_policy_probe(
    policy: varlit::ReadValidation,
    damaged: bool,
    limited: bool,
) -> Result<varlit::Result<Probe>, Box<dyn std::error::Error>> {
    let (directory, _original, offset) = probe(damaged)?;
    let mut limits = varlit::ReadLimits::STANDARD.with_read_validation(policy);
    if limited {
        limits = limits.with_max_record_payload_len(0);
    }
    Ok(ProbeFormat::spec()
        .open_readonly_with_limits(directory.path().join("contract.varlit"), limits)
        .and_then(|reader| read_probe_at(&reader, offset)))
}

struct ReplacingCodec;
impl varlit::VarlitDecode for ReplacingCodec {
    const WIRE_TYPE: varlit::WireType = varlit::WireType::Nested;
    fn decode_varlit(d: &mut varlit::Decoder<'_>) -> varlit::Result<Self> {
        *d = varlit::Decoder::new_limited(&[], varlit::Endian::Little, u64::MAX);
        Ok(Self)
    }
}

varlit::varlit_format! {
    struct QualificationProbe {
        magic: b"QUALIFY"; version: 1; endian: little; schema_hash: computed;
        integrity: crc32_with_header; blocks: [Probe];
    }
}

fn qualification_probe(
    damaged: bool,
) -> Result<varlit::FileCheckReport, Box<dyn std::error::Error>> {
    use std::io::{Seek, SeekFrom, Write};
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("qualify.varlit");
    let mut writer = QualificationProbe::create(&path)?;
    let entry = writer.push_info(&Probe { value: 19 })?;
    let offset = writer.record_entry_at(entry.record_offset)?.payload_offset;
    writer.sync()?;
    drop(writer);
    if damaged {
        let mut bytes = std::fs::OpenOptions::new().write(true).open(&path)?;
        bytes.seek(SeekFrom::Start(offset))?;
        bytes.write_all(&[0x7f])?;
    }
    Ok(QualificationProbe::check_file(&path)?)
}

/// Inverts one byte in place, as a lost or torn page would.
fn flip_byte(path: &Path, offset: u64) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut byte = [0u8];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut byte)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&[!byte[0]])
}

/// A codec whose every element encode is refused.
struct RefusingEncode;
impl varlit::VarlitEncode for RefusingEncode {
    const WIRE_TYPE: varlit::WireType = varlit::WireType::Nested;
    fn encode_varlit(&self, _encoder: &mut varlit::Encoder) -> varlit::Result<()> {
        Err(varlit::Error::InvalidCanonicalEncoding(
            "contract element refusal",
        ))
    }
}

/// In-place decode presence flags: a defaulted field absent from the bytes is
/// reset, a required one is refused.
#[derive(Clone, Debug, PartialEq, varlit::VarlitBlock)]
#[varlit(id = 72, version = 1, kind = "variable")]
struct IntoWindow {
    #[varlit(field_id = 1)]
    w: u64,
    #[varlit(field_id = 2, default)]
    added: Vec<u32>,
}
#[derive(Clone, Debug, PartialEq, varlit::VarlitBlock)]
#[varlit(id = 72, version = 1, kind = "variable")]
struct IntoWindowRequiredOnly {
    #[varlit(field_id = 1)]
    w: u64,
}
#[derive(Clone, Debug, PartialEq, varlit::VarlitBlock)]
#[varlit(id = 72, version = 1, kind = "variable")]
struct IntoWindowDefaultOnly {
    #[varlit(field_id = 2)]
    added: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, varlit::VarlitBlock)]
#[varlit(id = 73, version = 1, kind = "variable", key = "id")]
struct KeyedProbe {
    #[varlit(field_id = 1)]
    id: u64,
    #[varlit(field_id = 2)]
    body: String,
}

varlit::varlit_format! {
    struct KeyedProbeFormat { magic: b"VKEYEDPR"; version: 1; endian: little; integrity: crc32_with_header; blocks: [KeyedProbe]; }
}

/// The keyed collection over `KeyedProbe { id: 7, .. }`, built before the
/// optional damage so that only the later read meets it.
fn keyed_probe(
    damaged: bool,
) -> Result<(tempfile::TempDir, varlit::KeyedBlockVec<u64, KeyedProbe>), Box<dyn std::error::Error>>
{
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("keyed.varlit");
    let mut writer = KeyedProbeFormat::spec().create(&path)?;
    writer.push_keyed(&KeyedProbe {
        id: 7,
        body: "seven".into(),
    })?;
    writer.sync()?;
    let mut entries = Vec::new();
    writer.block_entries_into::<KeyedProbe>(&mut entries)?;
    drop(writer);
    let keyed = KeyedProbeFormat::spec()
        .open_readonly(&path)?
        .keyed_blocks::<KeyedProbe>()?;
    if damaged {
        flip_byte(
            &path,
            entries[0].payload_offset + entries[0].payload_len - 1,
        )?;
    }
    Ok((directory, keyed))
}

varlit::varlit_format! {
    pub format IntoSlots {
        magic: b"VINTOSLT"; version: 1; schema_hash: computed;
        header_slots { capacity: 256; integrity: rolling; blocks: [SlotMark]; }
        blocks {
            variable SlotMark(id = 77) { at: u64, }
            variable SlotOther(id = 78) { at: u64, }
        }
    }
}

/// A file whose header region holds `SlotMark { at: 5 }`.
fn slots_fixture() -> Result<(tempfile::TempDir, std::path::PathBuf), Box<dyn std::error::Error>> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("slots.varlit");
    let mut writer = IntoSlots::create_writer(&path)?;
    writer.write_header_block(&SlotMark { at: 5 })?;
    drop(writer);
    Ok((directory, path))
}

varlit::varlit_format! {
    pub format GridMatrix {
        magic: b"VGRIDMTX"; version: 1; endian: little;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks { matrix GridCell(id = 74, dims = [row, col], category = data) { value: u64, } }
    }
}

/// A 2x2 matrix whose cell (0, 0) holds 11.
fn grid_fixture() -> Result<(tempfile::TempDir, std::path::PathBuf), Box<dyn std::error::Error>> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("grid.varlit");
    let mut writer = GridMatrix::create_writer_with_dims(&path, GridMatrixDims { row: 2, col: 2 })?;
    writer.write_grid_cell(GridCellKey { row: 0, col: 0 }, &GridCell { value: 11 })?;
    writer.commit_grid_cell(GridCellKey { row: 0, col: 0 })?;
    writer.sync()?;
    drop(writer);
    Ok((directory, path))
}

varlit::varlit_format! {
    pub format IntoIndex {
        magic: b"VINTOIDX"; version: 1; integrity: crc32_with_header; index: keyed_offset_chain;
        blocks {
            variable IndexedEntry(id = 75, key = [id], key_index = disk) { id: u64, value: u64, }
            variable ChainEntry(id = 76, key = [id]) { id: u64, value: u64, }
        }
    }
}

/// A reader over an indexed file holding `IndexedEntry { id: 7, value: 70 }`,
/// opened before the optional damage so that only the later read meets it.
fn indexed_fixture(
    damaged: bool,
) -> Result<(tempfile::TempDir, IntoIndexIndexedReader), Box<dyn std::error::Error>> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("indexed.varlit");
    let mut writer = IntoIndex::create_indexed_writer(&path, varlit::DiskIndexOptions::default())?;
    let info = writer.push_indexed_entry(&IndexedEntry { id: 7, value: 70 })?;
    writer.sync()?;
    drop(writer);
    let reader = IntoIndex::open_indexed_reader(&path, varlit::DiskIndexOptions::default())?;
    if damaged {
        flip_byte(&path, info.payload_offset)?;
    }
    Ok((directory, reader))
}

varlit::varlit_format! {
    struct BarrierProbe {
        magic: b"VBARRIER"; version: 1; endian: little; integrity: crc32_with_header;
        recovery: truncate_tail; index: [scan_on_open, block_offset_chain];
        commit: transaction_marker(explicit, single_barrier); blocks: [Probe];
    }
}
varlit::varlit_format! {
    struct WeakBarrierProbe {
        magic: b"VWEAKBAR"; version: 1; endian: little; integrity: crc32;
        recovery: truncate_tail; index: [scan_on_open, block_offset_chain];
        commit: transaction_marker(on_flush, single_barrier); blocks: [Probe];
    }
}

/// Commits `Probe { value: 1 }` durably, then `Probe { value: 2 }` in a second
/// transaction, optionally tears that second record, and returns the values a
/// fresh read-only open reports as committed.
fn single_barrier_probe(
    spec: varlit::FormatSpec,
    tear: bool,
) -> Result<Vec<u64>, Box<dyn std::error::Error>> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("barrier.varlit");
    let mut file = spec.create(&path)?;
    file.push(&Probe { value: 1 })?;
    file.commit_durable()?;
    let torn = file.push_info(&Probe { value: 2 })?.payload_offset;
    file.commit_durable()?;
    drop(file);
    if tear {
        flip_byte(&path, torn)?;
    }
    let reader = spec.open_readonly(&path)?;
    let (mut scratch, mut out, mut values) = (
        Vec::new(),
        <Probe as varlit::VarlitEmpty>::empty(),
        Vec::new(),
    );
    for entry in reader.block_chain(<Probe as varlit::VarlitBlock>::ID)? {
        reader.decode_block_into(&entry?, &mut scratch, &mut out)?;
        values.push(out.value);
    }
    values.reverse();
    Ok(values)
}

const CONTRACTS: &[Contract] = &[
    contract!("core/conformance.rs::conformance::FileCheck::field:block_id@fed9d3e59fe29e18",
        valid { let report = qualification_probe(false)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert_eq!(c.block_id, Some(1)); Ok(()) },
        invalid { let report = qualification_probe(true)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert_eq!(c.block_id, Some(1)); assert!(!report.passed()); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheck::field:code@a29a3e1b26944a31",
        valid { let report = qualification_probe(false)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert_eq!(c.code, "block.typed_payloads"); Ok(()) },
        invalid { let report = qualification_probe(true)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert_eq!(c.code, "block.typed_payloads"); assert!(!report.passed()); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheck::field:error@f1c4c51ad2c3dca4",
        valid { let report = qualification_probe(false)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert!(c.error.is_none()); Ok(()) },
        invalid { let report = qualification_probe(true)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert!(c.error.is_some()); assert!(!report.passed()); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheck::field:message@a29a3e1b26944a31",
        valid { let report = qualification_probe(false)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert_eq!(c.message, "check completed"); Ok(()) },
        invalid { let report = qualification_probe(true)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert!(!c.message.is_empty()); assert!(!report.passed()); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheck::field:offset@0504e5cb6c5993db",
        valid { let report = qualification_probe(false)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert!(c.offset.is_some()); Ok(()) },
        invalid { let report = qualification_probe(true)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert!(c.offset.is_some()); assert!(!report.passed()); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheck::field:records_checked@4d35d3193e8d66f2",
        valid { let report = qualification_probe(false)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert_eq!(c.records_checked, 1); Ok(()) },
        invalid { let report = qualification_probe(true)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert_eq!(c.records_checked, 0); assert!(!report.passed()); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheck::field:status@b264600bb885d58f",
        valid { let report = qualification_probe(false)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert_eq!(c.status, varlit::FileCheckStatus::Passed); Ok(()) },
        invalid { let report = qualification_probe(true)?; let c = report.checks().iter().find(|c| c.block_id == Some(1)).unwrap(); assert_eq!(c.status, varlit::FileCheckStatus::Violated); assert!(!report.passed()); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheckReport::fn:checks@2119a90258941a3e",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); assert!(report.checks().iter().all(|c| c.error.is_none())); Ok(()) },
        invalid { let report = qualification_probe(true)?; assert!(!report.passed()); assert!(report.checks().iter().any(|c| c.error.is_some())); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheckReport::fn:passed@fc2e046a72543323",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); Ok(()) },
        invalid { let report = qualification_probe(true)?; assert!(!report.passed()); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheckReport::fn:preconditions@a9b749d031dc723d",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); assert!(!report.preconditions().is_empty()); Ok(()) },
        invalid { let report = qualification_probe(true)?; assert!(!report.passed()); assert!(!report.preconditions().is_empty()); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheckStatus::variant:NotApplicable@99e9951548b15d3f",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); assert!(report.checks().iter().any(|c| c.status == varlit::FileCheckStatus::NotApplicable)); Ok(()) },
        invalid { let report = qualification_probe(true)?; assert!(!report.passed()); assert!(report.checks().iter().any(|c| c.status == varlit::FileCheckStatus::NotApplicable)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheckStatus::variant:Passed@f39e32f2714673d9",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); assert!(report.checks().iter().all(|c| c.status != varlit::FileCheckStatus::Violated)); assert!(report.checks().iter().any(|c| c.status == varlit::FileCheckStatus::Passed)); Ok(()) },
        invalid { let report = qualification_probe(true)?; assert!(!report.passed()); assert!(report.checks().iter().any(|c| c.status == varlit::FileCheckStatus::Violated)); assert!(report.checks().iter().any(|c| c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheckStatus::variant:Unverified@c94578030c702562",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); assert!(report.checks().iter().all(|c| c.status != varlit::FileCheckStatus::Unverified)); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let report = QualificationProbe::check_file(dir.path().join("missing"))?; assert!(!report.passed()); assert!(report.checks().iter().any(|c| c.status == varlit::FileCheckStatus::Unverified)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheckStatus::variant:Violated@fedc9a584729feb7",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); assert!(report.checks().iter().all(|c| c.status != varlit::FileCheckStatus::Violated)); Ok(()) },
        invalid { let report = qualification_probe(true)?; assert!(!report.passed()); assert!(report.checks().iter().any(|c| c.status == varlit::FileCheckStatus::Violated)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::RecordValidator::fn:keyed@80f13f289fc95211",
        valid { let dir = private_tempdir::private_tempdir()?; let r = varlit::__core::check_file(OwnershipIndex::spec(), dir.path().join("missing"), &[varlit::__core::RecordValidator::keyed::<OwnedEntry>()])?; assert!(r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[varlit::__core::RecordValidator::keyed::<OwnedEntry>()])?; assert!(!r.passed()); assert!(!r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::RecordValidator::fn:of@dff8df83dcb8825e",
        valid { let dir = private_tempdir::private_tempdir()?; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[varlit::__core::RecordValidator::of::<Probe>()])?; assert!(r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let v = varlit::__core::RecordValidator::of::<Probe>(); let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[v, v])?; assert!(!r.passed()); assert!(!r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::RecordValidator::fn:requiring_disk_index@e957c756694053c1",
        valid { let dir = private_tempdir::private_tempdir()?; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[varlit::__core::RecordValidator::of::<Probe>().requiring_disk_index()])?; assert!(r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); assert!(r.checks().iter().any(|c| c.code == "sidecar.coherence" && c.status == varlit::FileCheckStatus::Unverified)); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let v = varlit::__core::RecordValidator::of::<Probe>().requiring_disk_index(); let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[v, v])?; assert!(!r.passed()); assert!(!r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::enum:FileCheckStatus@cbf29ce484222325",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); assert!(report.checks().iter().all(|c| c.status != varlit::FileCheckStatus::Violated)); Ok(()) },
        invalid { let report = qualification_probe(true)?; assert!(!report.passed()); assert!(report.checks().iter().any(|c| c.status == varlit::FileCheckStatus::Violated)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::fn:check_file@b1b57b61204f2e51",
        valid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("ok"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 1 })?; w.sync()?; } let r = QualificationProbe::check_file_with_resource_limits(&path, varlit::ReadLimits::STANDARD)?; assert!(r.passed(), "{r:#?}"); let r = QualificationProbe::check_file(&path)?; assert!(r.passed()); let r = varlit::check_file(QualificationProbe::spec(), &path, &[varlit::__core::RecordValidator::of::<Probe>()])?; assert!(r.passed()); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let r = QualificationProbe::check_file(dir.path().join("missing"))?; assert!(!r.passed()); let r = QualificationProbe::check_file_with_resource_limits(dir.path().join("missing"), varlit::ReadLimits::STANDARD)?; assert!(!r.passed()); let r = varlit::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[])?; assert!(!r.passed()); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::struct:FileCheck@cbf29ce484222325",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); let c: &varlit::FileCheck = &report.checks()[0]; assert_eq!(c.status, varlit::FileCheckStatus::Passed); Ok(()) },
        invalid { let report = qualification_probe(true)?; assert!(!report.passed()); let c: &varlit::FileCheck = report.checks().iter().find(|c| c.error.is_some()).unwrap(); assert_eq!(c.status, varlit::FileCheckStatus::Violated); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::struct:FileCheckReport@cbf29ce484222325",
        valid { let report: varlit::FileCheckReport = qualification_probe(false)?; assert!(report.passed()); Ok(()) },
        invalid { let report: varlit::FileCheckReport = qualification_probe(true)?; assert!(!report.passed()); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::struct:RecordValidator@cbf29ce484222325",
        valid { let dir = private_tempdir::private_tempdir()?; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[varlit::__core::RecordValidator::of::<Probe>()])?; assert!(r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let v = varlit::__core::RecordValidator::of::<Probe>(); let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[v, v])?; assert!(!r.passed()); assert!(!r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("core/diagnostics.rs::diagnostics::FormatSelfTest::fn:run_strict@47cc67381b8bce09",
        valid { let dir = private_tempdir::private_tempdir()?; let r = QualificationProbe::self_test(dir.path().join("ok")).with_block(Probe { value: 1 }).cleanup(true).run_strict(); assert!(r.passed(), "{r:#?}"); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let r = QualificationProbe::self_test(dir.path().join("missing")).run_strict(); assert!(!r.passed()); assert!(!dir.path().join("missing").exists()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:FileCheck@86941e1d8527cdb2",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); let c: &varlit::FileCheck = &report.checks()[0]; assert_eq!(c.status, varlit::FileCheckStatus::Passed); Ok(()) },
        invalid { let report = qualification_probe(true)?; assert!(!report.passed()); let c: &varlit::FileCheck = report.checks().iter().find(|c| c.error.is_some()).unwrap(); assert_eq!(c.status, varlit::FileCheckStatus::Violated); Ok(()) }
    ),
    contract!("core/lib.rs::::export:FileCheckReport@6df6600a85461108",
        valid { let report: varlit::FileCheckReport = qualification_probe(false)?; assert!(report.passed()); Ok(()) },
        invalid { let report: varlit::FileCheckReport = qualification_probe(true)?; assert!(!report.passed()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:FileCheckStatus@71ad0e33bf874f7c",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); assert!(report.checks().iter().all(|c| c.status != varlit::FileCheckStatus::Violated)); Ok(()) },
        invalid { let report = qualification_probe(true)?; assert!(!report.passed()); assert!(report.checks().iter().any(|c| c.status == varlit::FileCheckStatus::Violated)); Ok(()) }
    ),
    contract!("core/lib.rs::::export:RecordValidator@69fcf6ce187a63bd",
        valid { let dir = private_tempdir::private_tempdir()?; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[varlit::__core::RecordValidator::of::<Probe>()])?; assert!(r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let v = varlit::__core::RecordValidator::of::<Probe>(); let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[v, v])?; assert!(!r.passed()); assert!(!r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("core/lib.rs::::export:check_file@b50c368a3166740f",
        valid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("ok"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 1 })?; w.sync()?; } let r = QualificationProbe::check_file_with_resource_limits(&path, varlit::ReadLimits::STANDARD)?; assert!(r.passed(), "{r:#?}"); let r = QualificationProbe::check_file(&path)?; assert!(r.passed()); let r = varlit::check_file(QualificationProbe::spec(), &path, &[varlit::__core::RecordValidator::of::<Probe>()])?; assert!(r.passed()); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let r = QualificationProbe::check_file(dir.path().join("missing"))?; assert!(!r.passed()); let r = QualificationProbe::check_file_with_resource_limits(dir.path().join("missing"), varlit::ReadLimits::STANDARD)?; assert!(!r.passed()); let r = varlit::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[])?; assert!(!r.passed()); Ok(()) }
    ),
    contract!("core/traits.rs::traits::VarlitBlock::const:__VARLIT_RECORD_VALIDATOR@9de51896de2574bb",
        valid { let dir = private_tempdir::private_tempdir()?; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[<Probe as varlit::VarlitBlock>::__VARLIT_RECORD_VALIDATOR])?; assert!(r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let v = <Probe as varlit::VarlitBlock>::__VARLIT_RECORD_VALIDATOR; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[v, v])?; assert!(!r.passed()); assert!(!r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:expand_format@d153ebf947c54d34", generated "check_file",
        valid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("ok"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 1 })?; w.sync()?; } let r = QualificationProbe::check_file_with_resource_limits(&path, varlit::ReadLimits::STANDARD)?; assert!(r.passed(), "{r:#?}"); let r = QualificationProbe::check_file(&path)?; assert!(r.passed()); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let r = QualificationProbe::check_file(dir.path().join("missing"))?; assert!(!r.passed()); let r = QualificationProbe::check_file_with_resource_limits(dir.path().join("missing"), varlit::ReadLimits::STANDARD)?; assert!(!r.passed()); Ok(()) }
    ),
    contract!("core/codec.rs::codec::VarlitDecode::const:__VARLIT_BUILTIN@902bd1bc3b4320a2",
        valid { let evidence:Option<varlit::__core::BuiltinCodec<u64>>=<u64 as varlit::VarlitDecode>::__VARLIT_BUILTIN; assert!(evidence.is_some()); Ok(()) },
        invalid { let evidence:Option<varlit::__core::BuiltinCodec<ReplacingCodec>>=<ReplacingCodec as varlit::VarlitDecode>::__VARLIT_BUILTIN; assert!(evidence.is_none()); Ok(()) }
    ),
    contract!("core/codec.rs::codec::struct:BuiltinCodec@253d4ad21b3759f9",
        valid { let evidence:Option<varlit::__core::BuiltinCodec<u64>>=<u64 as varlit::VarlitDecode>::__VARLIT_BUILTIN; assert!(evidence.is_some()); Ok(()) },
        invalid { let evidence:Option<varlit::__core::BuiltinCodec<ReplacingCodec>>=<ReplacingCodec as varlit::VarlitDecode>::__VARLIT_BUILTIN; assert!(evidence.is_none()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:BuiltinCodec@49371ea8ff55b020",
        valid { let evidence:Option<varlit::__core::BuiltinCodec<u64>>=<u64 as varlit::VarlitDecode>::__VARLIT_BUILTIN; assert!(evidence.is_some()); Ok(()) },
        invalid { let evidence:Option<varlit::__core::BuiltinCodec<ReplacingCodec>>=<ReplacingCodec as varlit::VarlitDecode>::__VARLIT_BUILTIN; assert!(evidence.is_none()); Ok(()) }
    ),
    contract!("core/codec.rs::codec::Decoder < 'a >::fn:decode_value@7a987c34ca158f10",
        valid { let bytes=7u64.to_le_bytes(); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert_eq!(d.decode_value::<u64>()?,7); Ok(()) },
        invalid { let mut d=varlit::Decoder::new_limited(&[],varlit::Endian::Little,8); assert!(d.decode_value::<ReplacingCodec>().is_err()); assert_eq!(d.materialization_remaining(),0); Ok(()) }
    ),
    contract!("core/codec.rs::codec::macro:unsigned@240e5fcd78cd3def", generated "decode_value",
        valid { let bytes=7u64.to_le_bytes(); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert_eq!(d.decode_value::<u64>()?,7); Ok(()) },
        invalid { let mut d=varlit::Decoder::new(&[0;7],varlit::Endian::Little); assert!(d.decode_value::<u64>().is_err()); Ok(()) }
    ),
    contract!("core/codec.rs::codec::macro:signed@5cf1ba49e26da49e", generated "decode_value",
        valid { let bytes=(-7i64).to_le_bytes(); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert_eq!(d.decode_value::<i64>()?,-7); Ok(()) },
        invalid { let mut d=varlit::Decoder::new(&[0;7],varlit::Endian::Little); assert!(d.decode_value::<i64>().is_err()); Ok(()) }
    ),
    contract!("core/codec.rs::codec::macro:tuple_codec@1c32925194107c29", generated "decode_value",
        valid { let bytes=varlit::encode_to_vec(&(7u64,8u64),varlit::Endian::Little)?; let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert_eq!(d.decode_value::<(u64,u64)>()?,(7,8)); Ok(()) },
        invalid { let bytes=7u64.to_le_bytes(); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert!(d.decode_value::<(u64,ReplacingCodec)>().is_err()); Ok(()) }
    ),
    contract!("core/codec.rs::codec::macro:vec_seq_codec@205dbef4697c0ba6", generated "decode_value",
        valid { let value=vec![String::from("a"),String::from("bc")]; let bytes=varlit::encode_to_vec(&value,varlit::Endian::Little)?; let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert_eq!(d.decode_value::<Vec<String>>()?,value); Ok(()) },
        invalid { let bytes=u64::MAX.to_le_bytes(); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert!(d.decode_value::<Vec<String>>().is_err()); Ok(()) }
    ),
    contract!("core/codec.rs::codec::macro:vec_seq_codec@83385d869280e38e", generated "decode_value_into",
        valid { let value=vec![String::from("a"),String::from("bc")]; let bytes=varlit::encode_to_vec(&value,varlit::Endian::Little)?; let mut out=vec![String::from("stale");5]; let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); d.decode_value_into(&mut out)?; assert_eq!(out,value); assert_eq!(d.remaining(),0); Ok(()) },
        invalid { let mut bytes=varlit::encode_to_vec(&vec![String::from("abc")],varlit::Endian::Little)?; *bytes.last_mut().unwrap()=0xff; let mut out=vec![String::from("keep")]; let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert!(matches!(d.decode_value_into(&mut out),Err(varlit::Error::InvalidUtf8))); Ok(()) }
    ),
    contract!("core/codec.rs::codec::macro:vec_fixed_codec@85296e605afc8b10", generated "decode_value",
        valid { for endian in [varlit::Endian::Little,varlit::Endian::Big] { let bytes=varlit::encode_to_vec(&vec![7u16,0x0102],endian)?; let mut d=varlit::Decoder::new(&bytes,endian); assert_eq!(d.decode_value::<Vec<u16>>()?,vec![7,0x0102]); } Ok(()) },
        invalid { let mut bytes=varlit::encode_to_vec(&vec![7u16,8],varlit::Endian::Little)?; bytes[..8].copy_from_slice(&u64::MAX.to_le_bytes()); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert!(d.decode_value::<Vec<u16>>().is_err()); let fresh=varlit::encode_to_vec(&vec![7u16,8],varlit::Endian::Little)?; let mut d=varlit::Decoder::new_limited(&fresh,varlit::Endian::Little,3); assert!(matches!(d.decode_value::<Vec<u16>>(),Err(varlit::Error::LimitExceeded { .. }))); Ok(()) }
    ),
    contract!("core/codec.rs::codec::macro:vec_fixed_codec@fde558bea70560fe", generated "decode_value_into",
        valid { let bytes=varlit::encode_to_vec(&vec![true,false,true],varlit::Endian::Little)?; let mut out=vec![false;9]; let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); d.decode_value_into(&mut out)?; assert_eq!(out,[true,false,true]); Ok(()) },
        invalid { let mut bytes=varlit::encode_to_vec(&vec![true,false],varlit::Endian::Little)?; *bytes.last_mut().unwrap()=2; let mut out=Vec::<bool>::new(); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert!(d.decode_value_into(&mut out).is_err()); Ok(()) }
    ),
    contract!("macros/modules/block_derive.rs::block_derive::generator:expand_varlit_block@105799ce1f8bb744", generated "decode_value_into",
        valid { let bytes=varlit::encode_to_vec(&Probe{value:7},varlit::Endian::Little)?; let mut out=<Probe as varlit::VarlitEmpty>::empty(); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); d.decode_value_into(&mut out)?; assert_eq!(out,Probe{value:7}); Ok(()) },
        invalid { let bytes=varlit::encode_to_vec(&Probe{value:7},varlit::Endian::Little)?; let mut out=<Probe as varlit::VarlitEmpty>::empty(); let mut d=varlit::Decoder::new(&bytes[..bytes.len()-1],varlit::Endian::Little); assert!(d.decode_value_into(&mut out).is_err()); Ok(()) }
    ),
    contract!("macros/modules/block_derive.rs::block_derive::generator:seen_ident@eb2d230ef54d4dca", generated "decode_from_slice_into",
        valid { let bytes=varlit::encode_to_vec(&IntoWindowRequiredOnly{w:3},varlit::Endian::Little)?; let mut out=IntoWindow{w:9,added:vec![1,2]}; varlit::decode_from_slice_into(&bytes,varlit::Endian::Little,&mut out)?; assert_eq!(out,IntoWindow{w:3,added:Vec::new()}); Ok(()) },
        invalid { let bytes=varlit::encode_to_vec(&IntoWindowDefaultOnly{added:vec![1]},varlit::Endian::Little)?; let mut out=IntoWindow{w:9,added:vec![1,2]}; assert!(matches!(varlit::decode_from_slice_into(&bytes,varlit::Endian::Little,&mut out),Err(varlit::Error::MissingField { .. }))); Ok(()) }
    ),
    contract!("core/format.rs::format::ReadValidation::const:FULL@65630b1aec77469e",
        valid { assert_eq!(read_policy_probe(varlit::ReadValidation::FULL, false, false)??, Probe { value: 19 }); Ok(()) },
        invalid { let result = read_policy_probe(varlit::ReadValidation::FULL, false, true)?; assert!(matches!(result, Err(varlit::Error::LimitExceeded { .. }))); Ok(()) }
    ),
    contract!("core/format.rs::format::ReadValidation::const:CRC_ONLY@df8c023edf7c9919",
        valid { assert_eq!(read_policy_probe(varlit::ReadValidation::CRC_ONLY, false, false)??, Probe { value: 19 }); Ok(()) },
        invalid { let result = read_policy_probe(varlit::ReadValidation::CRC_ONLY, false, true)?; assert!(matches!(result, Err(varlit::Error::LimitExceeded { .. }))); Ok(()) }
    ),
    contract!("core/format.rs::format::ReadValidation::const:STRUCTURE_ONLY@209e08687f0843d8",
        valid { assert_eq!(read_policy_probe(varlit::ReadValidation::STRUCTURE_ONLY, false, false)??, Probe { value: 19 }); Ok(()) },
        invalid { let result = read_policy_probe(varlit::ReadValidation::STRUCTURE_ONLY, false, true)?; assert!(matches!(result, Err(varlit::Error::LimitExceeded { .. }))); Ok(()) }
    ),
    contract!("core/format.rs::format::ReadValidation::const:TRUSTED@6311c0c4b06413e8",
        valid { assert_eq!(read_policy_probe(varlit::ReadValidation::TRUSTED, false, false)??, Probe { value: 19 }); Ok(()) },
        invalid { let result = read_policy_probe(varlit::ReadValidation::TRUSTED, false, true)?; assert!(matches!(result, Err(varlit::Error::LimitExceeded { .. }))); Ok(()) }
    ),
    contract!("core/format.rs::format::struct:ReadValidation@cbf29ce484222325",
        valid { let policy = varlit::ReadValidation { data_crc: false, structure: false }; assert_eq!(read_policy_probe(policy, true, false)??, Probe { value: 127 }); Ok(()) },
        invalid { let policy = varlit::ReadValidation { data_crc: true, structure: true }; assert!(matches!(read_policy_probe(policy, true, false)?, Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/lib.rs::::export:ReadValidation@5132be33be37925b",
        valid { let policy = varlit::ReadValidation { data_crc: false, structure: false }; assert_eq!(read_policy_probe(policy, true, false)??, Probe { value: 127 }); Ok(()) },
        invalid { let policy = varlit::ReadValidation { data_crc: true, structure: true }; assert!(matches!(read_policy_probe(policy, true, false)?, Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/format.rs::format::ReadValidation::field:data_crc@cd2fd49bc6b014bd",
        valid { let mut policy = varlit::ReadValidation::FULL; policy.data_crc = false; assert_eq!(read_policy_probe(policy, false, false)??, Probe { value: 19 }); Ok(()) },
        invalid { let mut policy = varlit::ReadValidation::FULL; policy.data_crc = false; assert!(matches!(read_policy_probe(policy, false, true)?, Err(varlit::Error::LimitExceeded { .. }))); Ok(()) }
    ),
    contract!("core/format.rs::format::ReadValidation::field:structure@cd2fd49bc6b014bd",
        valid { let mut policy = varlit::ReadValidation::FULL; policy.structure = false; assert_eq!(read_policy_probe(policy, false, false)??, Probe { value: 19 }); Ok(()) },
        invalid { let mut policy = varlit::ReadValidation::FULL; policy.structure = false; assert!(matches!(read_policy_probe(policy, false, true)?, Err(varlit::Error::LimitExceeded { .. }))); Ok(()) }
    ),
    contract!("core/format.rs::format::ReadLimits::field:read_validation@55eeb0f6e20f3ad1",
        valid { let mut limits = varlit::ReadLimits::STANDARD; limits.read_validation = Some(varlit::ReadValidation::TRUSTED); assert_eq!(read_policy_probe(limits.read_validation.unwrap(), true, false)??, Probe { value: 127 }); Ok(()) },
        invalid { let mut limits = varlit::ReadLimits::STANDARD; limits.read_validation = Some(varlit::ReadValidation::TRUSTED); assert!(matches!(read_policy_probe(limits.read_validation.unwrap(), false, true)?, Err(varlit::Error::LimitExceeded { .. }))); Ok(()) }
    ),
    contract!("core/format.rs::format::ReadLimits::fn:with_read_validation@cefa6f5f700ab26f",
        valid { let limits = varlit::ReadLimits::MISSING.with_read_validation(varlit::ReadValidation::CRC_ONLY); assert_eq!(read_policy_probe(limits.read_validation.unwrap(), false, false)??, Probe { value: 19 }); Ok(()) },
        invalid { let limits = varlit::ReadLimits::MISSING.with_read_validation(varlit::ReadValidation::CRC_ONLY); assert!(matches!(read_policy_probe(limits.read_validation.unwrap(), true, false)?, Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/format.rs::format::ReadLimits::fn:effective_read_validation@b2c35212607618ef",
        valid { let policy = varlit::ReadLimits::MISSING.effective_read_validation(varlit::IntegrityPolicy::None); assert_eq!(policy, varlit::ReadValidation::TRUSTED); assert_eq!(read_policy_probe(policy, false, false)??, Probe { value: 19 }); Ok(()) },
        invalid { let policy = varlit::ReadLimits::MISSING.with_read_validation(varlit::ReadValidation::FULL).effective_read_validation(varlit::IntegrityPolicy::None); let spec = ProbeFormat::spec().with_integrity_policy(varlit::IntegrityPolicy::None).with_read_limits(varlit::ReadLimits::STANDARD.with_read_validation(policy)); assert!(matches!(spec.validate(), Err(varlit::Error::InvalidFormatSpec(_)))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:read_block_at@6da421ebaa8eb841",
        valid {
            let (_directory, reader, offset) = probe(false)?;
            let mut out = <Probe as varlit::VarlitEmpty>::empty();
            reader.read_block_at::<Probe>(offset, &mut Vec::new(), &mut out)?;
            assert_eq!(out, Probe { value: 19 });
            Ok(())
        },
        invalid {
            let (_directory, reader, offset) = probe(true)?;
            let mut out = <Probe as varlit::VarlitEmpty>::empty();
            assert!(matches!(reader.read_block_at::<Probe>(offset, &mut Vec::new(), &mut out), Err(varlit::Error::ChecksumMismatch { .. })));
            Ok(())
        }
    ),
    contract!("core/format.rs::format::ReadLimits::const:STANDARD@7b34437e092b5a48",
        valid {
            let (directory, _reader, offset) = probe(false)?;
            let reader = ProbeFormat::spec().open_readonly_with_limits(
                directory.path().join("contract.varlit"), varlit::ReadLimits::STANDARD)?;
            assert_eq!(read_probe_at(&reader, offset)?, Probe { value: 19 });
            Ok(())
        },
        invalid {
            let (directory, _reader, offset) = probe(false)?;
            let result = ProbeFormat::spec().open_readonly_with_limits(
                directory.path().join("contract.varlit"),
                varlit::ReadLimits::STANDARD.with_max_record_payload_len(0))
                .and_then(|reader| read_probe_at(&reader, offset));
            assert!(matches!(result, Err(varlit::Error::LimitExceeded { limit: 0, .. })));
            Ok(())
        }
    ),
    contract!("core/scan_control.rs::scan_control::ScanCancellationToken::fn:try_new@d8624e1b9e8ccf6f",
        valid { let token = varlit::ScanCancellationToken::try_new()?; assert!(!token.is_cancelled()); token.cancel(); assert!(token.is_cancelled()); Ok(()) },
        invalid { let result = refusing_allocations(|| varlit::ScanCancellationToken::try_new().map(|token| token.is_cancelled())); assert!(matches!(result, Err(varlit::Error::Io(e)) if e.kind() == std::io::ErrorKind::OutOfMemory)); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:high_cardinality_api_tokens@6a81c2271828ef0c", generated "get_indexed_entry",
        valid { let plan = OwnershipIndex::disk_index_plan()?; assert_eq!(plan.descriptors().len(), 1); assert!(plan.validate(OwnershipIndex::spec()).is_ok()); let (_directory, reader) = indexed_fixture(false)?; let mut out = <IndexedEntry as varlit::VarlitEmpty>::empty(); assert!(reader.get_indexed_entry(&7, &mut out)?); assert_eq!(out, IndexedEntry { id: 7, value: 70 }); let before = out.clone(); assert!(!reader.get_indexed_entry(&8, &mut out)?); assert_eq!(out, before); Ok(()) },
        invalid { let plan = OwnershipIndex::disk_index_plan()?; let error = plan.validate(ProbeFormat::spec()).unwrap_err(); assert!(matches!(error, varlit::DiskIndexError::PlanBlockMissing { .. })); let error = refusing_allocations(|| varlit::Error::from(error)); assert!(matches!(error, varlit::Error::AllocationFailed { .. })); let (_directory, reader) = indexed_fixture(true)?; let mut out = <IndexedEntry as varlit::VarlitEmpty>::empty(); assert!(matches!(reader.get_indexed_entry(&7, &mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:layout_field_struct_tokens@1b03f39d4b518018", generated "__varlit_layout_values",
        valid { let fields = OwnershipHeaderHeaderLayoutFields { kind: 42 }.__varlit_layout_values()?; assert_eq!(fields.len(), 1); assert_eq!(fields[0].value.to_u16("kind")?, 42); Ok(()) },
        invalid { let fields = OwnershipHeaderHeaderLayoutFields { kind: 42 }; let result = refusing_allocations(|| fields.__varlit_layout_values()); assert!(matches!(result, Err(varlit::Error::AllocationFailed { .. }))); assert_eq!(fields.kind, 42); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:layout_field_to_value_tokens@5525b6594eca5be7", generated "__varlit_layout_values",
        valid { let fields = OwnershipHeaderHeaderLayoutFields { kind: 42 }.__varlit_layout_values()?; assert_eq!(fields.len(), 1); assert_eq!(fields[0].value.to_u16("kind")?, 42); Ok(()) },
        invalid { let fields = OwnershipHeaderHeaderLayoutFields { kind: 42 }; let result = refusing_allocations(|| fields.__varlit_layout_values()); assert!(matches!(result, Err(varlit::Error::AllocationFailed { .. }))); assert_eq!(fields.kind, 42); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:layout_header_typed_tokens@095075385290c05c", generated "create_layout_writer_with_typed_header",
        valid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("header"); let mut writer = OwnershipHeader::create_layout_writer_with_typed_header(&path, OwnershipHeaderHeaderLayoutFields { kind: 42 })?; writer.flush()?; assert_eq!(&std::fs::read(path)?[..6], b"OWHD\x2a\x00"); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("header"); let result = refusing_allocations(|| OwnershipHeader::create_layout_writer_with_typed_header(&path, OwnershipHeaderHeaderLayoutFields { kind: 42 })); assert!(matches!(result, Err(varlit::Error::AllocationFailed { .. }))); assert!(!path.exists()); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:layout_writer_segment_method_tokens@6d7f0cdfe27d767e", generated "write_data",
        valid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("segment"); let mut writer = OwnershipHeader::create_layout_writer_with_typed_header(&path, OwnershipHeaderHeaderLayoutFields { kind: 42 })?; let info = writer.write_data(OwnershipHeaderDataLayoutWrite { fields: OwnershipHeaderDataLayoutFields { kind: 7 }, footer_fields: OwnershipHeaderDataLayoutFooterFields, metadata: b"", raw: b"data" })?; assert_eq!(info.kind()?, 7); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("segment"); let mut writer = OwnershipHeader::create_layout_writer_with_typed_header(&path, OwnershipHeaderHeaderLayoutFields { kind: 42 })?; writer.flush()?; let before = std::fs::read(&path)?; let result = refusing_allocations(|| writer.write_data(OwnershipHeaderDataLayoutWrite { fields: OwnershipHeaderDataLayoutFields { kind: 7 }, footer_fields: OwnershipHeaderDataLayoutFooterFields, metadata: b"", raw: b"data" })); assert!(matches!(result, Err(varlit::Error::AllocationFailed { .. }))); writer.flush()?; assert_eq!(std::fs::read(path)?, before); Ok(()) }
    ),
    contract!("core/fallible.rs::fallible::TryClone::fn:try_clone@4c103a12486804c7",
        valid {
            use varlit::TryClone;
            let value = vec![Some(String::from("fallible"))];
            assert_eq!(value.try_clone()?, value);
            assert_eq!(u64::try_clone_slice(&[1, 2, 3])?, [1, 2, 3]);
            Ok(())
        },
        invalid {
            use varlit::TryClone;
            struct Refuses;
            impl TryClone for Refuses {
                fn try_clone(&self) -> varlit::Result<Self> {
                    Err(varlit::Error::AllocationFailed { resource: "contract clone", requested: 1 })
                }
            }
            assert!(matches!(vec![Refuses].try_clone(), Err(varlit::Error::AllocationFailed { .. })));
            assert!(Refuses::try_clone_slice(&[Refuses]).is_err());
            Ok(())
        }
    ),
    contract!("core/fallible.rs::fallible::TryClone::fn:try_clone_slice@d5101c54b8f22ed5",
        valid {
            use varlit::TryClone;
            let value = vec![Some(String::from("fallible"))];
            assert_eq!(value.try_clone()?, value);
            assert_eq!(u64::try_clone_slice(&[1, 2, 3])?, [1, 2, 3]);
            Ok(())
        },
        invalid {
            use varlit::TryClone;
            struct Refuses;
            impl TryClone for Refuses {
                fn try_clone(&self) -> varlit::Result<Self> {
                    Err(varlit::Error::AllocationFailed { resource: "contract clone", requested: 1 })
                }
            }
            assert!(matches!(vec![Refuses].try_clone(), Err(varlit::Error::AllocationFailed { .. })));
            assert!(Refuses::try_clone_slice(&[Refuses]).is_err());
            Ok(())
        }
    ),
    contract!("core/fallible.rs::fallible::macro:copy_clone@98ecf16d6ba68408", generated "try_clone_slice",
        valid {
            use varlit::TryClone;
            let value = vec![Some(String::from("fallible"))];
            assert_eq!(value.try_clone()?, value);
            assert_eq!(u64::try_clone_slice(&[1, 2, 3])?, [1, 2, 3]);
            Ok(())
        },
        invalid {
            use varlit::TryClone;
            struct Refuses;
            impl TryClone for Refuses {
                fn try_clone(&self) -> varlit::Result<Self> {
                    Err(varlit::Error::AllocationFailed { resource: "contract clone", requested: 1 })
                }
            }
            assert!(matches!(vec![Refuses].try_clone(), Err(varlit::Error::AllocationFailed { .. })));
            assert!(Refuses::try_clone_slice(&[Refuses]).is_err());
            Ok(())
        }
    ),
    contract!("core/fallible.rs::fallible::macro:copy_clone@e451f0eff70d84c4", generated "try_clone_slice",
        valid {
            use varlit::TryClone;
            let value = vec![Some(String::from("fallible"))];
            assert_eq!(value.try_clone()?, value);
            assert_eq!(u64::try_clone_slice(&[1, 2, 3])?, [1, 2, 3]);
            Ok(())
        },
        invalid {
            use varlit::TryClone;
            struct Refuses;
            impl TryClone for Refuses {
                fn try_clone(&self) -> varlit::Result<Self> {
                    Err(varlit::Error::AllocationFailed { resource: "contract clone", requested: 1 })
                }
            }
            assert!(matches!(vec![Refuses].try_clone(), Err(varlit::Error::AllocationFailed { .. })));
            assert!(Refuses::try_clone_slice(&[Refuses]).is_err());
            Ok(())
        }
    ),
    contract!("core/fallible.rs::fallible::trait:TryClone@cbf29ce484222325",
        valid {
            use varlit::TryClone;
            let value = vec![Some(String::from("fallible"))];
            assert_eq!(value.try_clone()?, value);
            assert_eq!(u64::try_clone_slice(&[1, 2, 3])?, [1, 2, 3]);
            Ok(())
        },
        invalid {
            use varlit::TryClone;
            struct Refuses;
            impl TryClone for Refuses {
                fn try_clone(&self) -> varlit::Result<Self> {
                    Err(varlit::Error::AllocationFailed { resource: "contract clone", requested: 1 })
                }
            }
            assert!(matches!(vec![Refuses].try_clone(), Err(varlit::Error::AllocationFailed { .. })));
            assert!(Refuses::try_clone_slice(&[Refuses]).is_err());
            Ok(())
        }
    ),
    contract!("core/lib.rs::::export:TryClone@4aefc82d5057d594",
        valid {
            use varlit::TryClone;
            let value = vec![Some(String::from("fallible"))];
            assert_eq!(value.try_clone()?, value);
            assert_eq!(u64::try_clone_slice(&[1, 2, 3])?, [1, 2, 3]);
            Ok(())
        },
        invalid {
            use varlit::TryClone;
            struct Refuses;
            impl TryClone for Refuses {
                fn try_clone(&self) -> varlit::Result<Self> {
                    Err(varlit::Error::AllocationFailed { resource: "contract clone", requested: 1 })
                }
            }
            assert!(matches!(vec![Refuses].try_clone(), Err(varlit::Error::AllocationFailed { .. })));
            assert!(Refuses::try_clone_slice(&[Refuses]).is_err());
            Ok(())
        }
    ),
    contract!("core/fallible.rs::fallible::fn:try_box_uninit@7c6aee6d6de73bc3",
        valid { let value = Box::write(varlit::try_box_uninit::<u64>()?, 7); assert_eq!(*value, 7); Ok(()) },
        invalid { let result = refusing_allocations(|| varlit::try_box_uninit::<u64>().map(drop)); assert!(matches!(result, Err(varlit::Error::AllocationFailed { .. }))); Ok(()) }
    ),
    contract!("core/lib.rs::::export:try_box_uninit@0ef305f46cad72a7",
        valid { let value = Box::write(varlit::try_box_uninit::<u64>()?, 7); assert_eq!(*value, 7); Ok(()) },
        invalid { let result = refusing_allocations(|| varlit::try_box_uninit::<u64>().map(drop)); assert!(matches!(result, Err(varlit::Error::AllocationFailed { .. }))); Ok(()) }
    ),
    contract!("core/fallible.rs::fallible::fn:try_box_with@ddecaa8ec6cd266b",
        valid { let value = varlit::try_box_with(|| Ok(7u64))?; assert_eq!(*value, 7); Ok(()) },
        invalid { let invoked = std::cell::Cell::new(false); let result = refusing_allocations(|| varlit::try_box_with(|| { invoked.set(true); Ok(7u64) })); assert!(matches!(result, Err(varlit::Error::AllocationFailed { .. }))); assert!(!invoked.get()); assert!(matches!(varlit::try_box_with::<u64>(|| Err(varlit::Error::InvalidMatrixLayout)), Err(varlit::Error::InvalidMatrixLayout))); Ok(()) }
    ),
    contract!("core/lib.rs::::export:try_box_with@c1a6bf4795c41fb6",
        valid { let value = varlit::try_box_with(|| Ok(7u64))?; assert_eq!(*value, 7); Ok(()) },
        invalid { let invoked = std::cell::Cell::new(false); let result = refusing_allocations(|| varlit::try_box_with(|| { invoked.set(true); Ok(7u64) })); assert!(matches!(result, Err(varlit::Error::AllocationFailed { .. }))); assert!(!invoked.get()); assert!(matches!(varlit::try_box_with::<u64>(|| Err(varlit::Error::InvalidMatrixLayout)), Err(varlit::Error::InvalidMatrixLayout))); Ok(()) }
    ),
    contract!("core/fallible.rs::fallible::fn:try_box@36a82d3283743b0b",
        valid { let value = varlit::try_box(7u64)?; assert_eq!(*value, 7); Ok(()) },
        invalid {
            let result = refusing_allocations(|| varlit::try_box(7u64));
            assert!(matches!(result, Err(varlit::Error::AllocationFailed { resource: "boxed value", requested: 8 })));
            Ok(())
        }
    ),
    contract!("core/lib.rs::::export:try_box@2b506e7fd8ea20d1",
        valid { let value = varlit::try_box(7u64)?; assert_eq!(*value, 7); Ok(()) },
        invalid {
            let result = refusing_allocations(|| varlit::try_box(7u64));
            assert!(matches!(result, Err(varlit::Error::AllocationFailed { resource: "boxed value", requested: 8 })));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::AllocationFailureContext::variant:Appended@3e545469e68cc7aa",
        valid {
            let context = varlit::AllocationFailureContext::Appended { sequence: 7 };
            let error = varlit::Error::ErrorContextAllocationFailed { context: context.clone() };
            assert!(matches!(error, varlit::Error::ErrorContextAllocationFailed { context: actual } if actual == context));
            Ok(())
        },
        invalid {
            let context = varlit::AllocationFailureContext::Appended { sequence: 7 };
            let error = refusing_allocations(|| varlit::try_box(context)).unwrap_err();
            assert!(matches!(error, varlit::Error::AllocationFailed { .. }));
            assert!(!matches!(error, varlit::Error::ErrorContextAllocationFailed { .. }));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::AllocationFailureContext::variant:Committed@45da4a7a68fd6d31",
        valid {
            let context = varlit::AllocationFailureContext::Committed { sequence: 7 };
            let error = varlit::Error::ErrorContextAllocationFailed { context: context.clone() };
            assert!(matches!(error, varlit::Error::ErrorContextAllocationFailed { context: actual } if actual == context));
            Ok(())
        },
        invalid {
            let context = varlit::AllocationFailureContext::Committed { sequence: 7 };
            let error = refusing_allocations(|| varlit::try_box(context)).unwrap_err();
            assert!(matches!(error, varlit::Error::AllocationFailed { .. }));
            assert!(!matches!(error, varlit::Error::ErrorContextAllocationFailed { .. }));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::AllocationFailureContext::variant:ParentSyncPending@1ca51b4e9b780955",
        valid {
            let context = varlit::AllocationFailureContext::ParentSyncPending;
            let error = varlit::Error::ErrorContextAllocationFailed { context: context.clone() };
            assert!(matches!(error, varlit::Error::ErrorContextAllocationFailed { context: actual } if actual == context));
            Ok(())
        },
        invalid {
            let context = varlit::AllocationFailureContext::ParentSyncPending;
            let error = refusing_allocations(|| varlit::try_box(context)).unwrap_err();
            assert!(matches!(error, varlit::Error::AllocationFailed { .. }));
            assert!(!matches!(error, varlit::Error::ErrorContextAllocationFailed { .. }));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::AllocationFailureContext::variant:PublishedIndex@7f5272beab4899bb",
        valid {
            let context = varlit::AllocationFailureContext::PublishedIndex { sequence: 7 };
            let error = varlit::Error::ErrorContextAllocationFailed { context: context.clone() };
            assert!(matches!(error, varlit::Error::ErrorContextAllocationFailed { context: actual } if actual == context));
            Ok(())
        },
        invalid {
            let context = varlit::AllocationFailureContext::PublishedIndex { sequence: 7 };
            let error = refusing_allocations(|| varlit::try_box(context)).unwrap_err();
            assert!(matches!(error, varlit::Error::AllocationFailed { .. }));
            assert!(!matches!(error, varlit::Error::ErrorContextAllocationFailed { .. }));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::AllocationFailureContext::variant:PublishedRebind@cdc4933fe9a8855f",
        valid {
            let context = varlit::AllocationFailureContext::PublishedRebind { sequence: 7, parent_sync_pending: true };
            let error = varlit::Error::ErrorContextAllocationFailed { context: context.clone() };
            assert!(matches!(error, varlit::Error::ErrorContextAllocationFailed { context: actual } if actual == context));
            Ok(())
        },
        invalid {
            let context = varlit::AllocationFailureContext::PublishedRebind { sequence: 7, parent_sync_pending: true };
            let error = refusing_allocations(|| varlit::try_box(context)).unwrap_err();
            assert!(matches!(error, varlit::Error::AllocationFailed { .. }));
            assert!(!matches!(error, varlit::Error::ErrorContextAllocationFailed { .. }));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::AllocationFailureContext::variant:ReusePublicationUncertain@9d4ea81e875b4593",
        valid {
            let context = varlit::AllocationFailureContext::ReusePublicationUncertain { generation: 7 };
            let error = varlit::Error::ErrorContextAllocationFailed { context: context.clone() };
            assert!(matches!(error, varlit::Error::ErrorContextAllocationFailed { context: actual } if actual == context));
            Ok(())
        },
        invalid {
            let context = varlit::AllocationFailureContext::ReusePublicationUncertain { generation: 7 };
            let error = refusing_allocations(|| varlit::try_box(context)).unwrap_err();
            assert!(matches!(error, varlit::Error::AllocationFailed { .. }));
            assert!(!matches!(error, varlit::Error::ErrorContextAllocationFailed { .. }));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::AllocationFailureContext::variant:ReusePublished@8674705202a61abc",
        valid {
            let context = varlit::AllocationFailureContext::ReusePublished { generation: 7 };
            let error = varlit::Error::ErrorContextAllocationFailed { context: context.clone() };
            assert!(matches!(error, varlit::Error::ErrorContextAllocationFailed { context: actual } if actual == context));
            Ok(())
        },
        invalid {
            let context = varlit::AllocationFailureContext::ReusePublished { generation: 7 };
            let error = refusing_allocations(|| varlit::try_box(context)).unwrap_err();
            assert!(matches!(error, varlit::Error::AllocationFailed { .. }));
            assert!(!matches!(error, varlit::Error::ErrorContextAllocationFailed { .. }));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::Error::variant:ErrorContextAllocationFailed@23755b55305b9bb1",
        valid {
            let context = varlit::AllocationFailureContext::PublishedRebind { sequence: 7, parent_sync_pending: true };
            let error = varlit::Error::ErrorContextAllocationFailed { context: context.clone() };
            assert!(matches!(error, varlit::Error::ErrorContextAllocationFailed { context: actual } if actual == context));
            Ok(())
        },
        invalid {
            let context = varlit::AllocationFailureContext::PublishedRebind { sequence: 7, parent_sync_pending: true };
            let error = refusing_allocations(|| varlit::try_box(context)).unwrap_err();
            assert!(matches!(error, varlit::Error::AllocationFailed { .. }));
            assert!(!matches!(error, varlit::Error::ErrorContextAllocationFailed { .. }));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::enum:AllocationFailureContext@cbf29ce484222325",
        valid {
            let context = varlit::AllocationFailureContext::PublishedRebind { sequence: 7, parent_sync_pending: true };
            let error = varlit::Error::ErrorContextAllocationFailed { context: context.clone() };
            assert!(matches!(error, varlit::Error::ErrorContextAllocationFailed { context: actual } if actual == context));
            Ok(())
        },
        invalid {
            let context = varlit::AllocationFailureContext::PublishedRebind { sequence: 7, parent_sync_pending: true };
            let error = refusing_allocations(|| varlit::try_box(context)).unwrap_err();
            assert!(matches!(error, varlit::Error::AllocationFailed { .. }));
            assert!(!matches!(error, varlit::Error::ErrorContextAllocationFailed { .. }));
            Ok(())
        }
    ),
    contract!("core/lib.rs::::export:AllocationFailureContext@4ccf3b018025dd40",
        valid {
            let context = varlit::AllocationFailureContext::PublishedRebind { sequence: 7, parent_sync_pending: true };
            let error = varlit::Error::ErrorContextAllocationFailed { context: context.clone() };
            assert!(matches!(error, varlit::Error::ErrorContextAllocationFailed { context: actual } if actual == context));
            Ok(())
        },
        invalid {
            let context = varlit::AllocationFailureContext::PublishedRebind { sequence: 7, parent_sync_pending: true };
            let error = refusing_allocations(|| varlit::try_box(context)).unwrap_err();
            assert!(matches!(error, varlit::Error::AllocationFailed { .. }));
            assert!(!matches!(error, varlit::Error::ErrorContextAllocationFailed { .. }));
            Ok(())
        }
    ),
    contract!("core/chunks.rs::chunks::ChunkedBytes::fn:decoded_len@b0b7b3a38476cd40",
        valid {
            let chunks = varlit::ChunkedBytes::from_zstd_chunks(&[3; 33], 16, varlit::CompressionLevel::Fast)?;
            assert_eq!(chunks.decoded_len(), 33);
            Ok(())
        },
        invalid {
            let chunks = varlit::ChunkedBytes::from_zstd_chunks(&[3; 33], 16, varlit::CompressionLevel::Fast)?;
            let mut bytes = chunks.into_encoded(); bytes[40] ^= 1;
            let damaged = varlit::ChunkedBytes::from_encoded(bytes)?;
            assert_eq!(damaged.decoded_len(), 33); // declaration is not authentication
            assert!(damaged.decode_to_vec_limited(33).is_err());
            Ok(())
        }
    ),
    contract!("core/chunks.rs::chunks::ChunkedBytes::fn:stream_range@6061cb400976e6d8",
        valid {
            let chunks = varlit::ChunkedBytes::from_zstd_chunks(&[3; 33], 16, varlit::CompressionLevel::Fast)?;
            assert!(chunks.decode_to_vec_limited(8).is_err());
            let mut count = 0;
            chunks.stream_range(7..24, &mut [0; 3], 1024, |at, bytes| {
                assert_eq!(at, 7 + count); assert!(bytes.iter().all(|&b| b == 3));
                count += bytes.len() as u64; Ok(())
            })?;
            assert_eq!(count, 17);
            Ok(())
        },
        invalid {
            let chunks = varlit::ChunkedBytes::from_zstd_chunks(&[3; 33], 16, varlit::CompressionLevel::Fast)?;
            let mut bytes = chunks.into_encoded(); bytes[40] ^= 1;
            let damaged = varlit::ChunkedBytes::from_encoded(bytes)?;
            assert!(matches!(damaged.stream_range(0..1, &mut [0; 3], 1024, |_, _| Ok(())), Err(varlit::Error::ChunkChecksumMismatch { .. })));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::Error::variant:BufferTooSmall@fd4e892a42401b1a",
        valid {
            let chunks = varlit::ChunkedBytes::from_zstd_chunks(&[3; 33], 16, varlit::CompressionLevel::Fast)?;
            let result = chunks.stream_range(0..1, &mut [0; 1], 1024, |_, _| Ok(()));
            assert!(!matches!(result, Err(varlit::Error::BufferTooSmall { .. })));
            result?; Ok(())
        },
        invalid {
            let chunks = varlit::ChunkedBytes::from_zstd_chunks(&[3; 33], 16, varlit::CompressionLevel::Fast)?;
            assert!(matches!(chunks.stream_range(0..1, &mut [], 1024, |_, _| panic!("delivery before buffer refusal")), Err(varlit::Error::BufferTooSmall { required: 1, provided: 0, .. })));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::Error::variant:InvalidChunkedRange@ae2ff6d176b1722e",
        valid {
            let chunks = varlit::ChunkedBytes::from_zstd_chunks(&[3; 33], 16, varlit::CompressionLevel::Fast)?;
            let result = chunks.stream_range(33..33, &mut [], 0, |_, _| panic!("empty delivery"));
            assert!(!matches!(result, Err(varlit::Error::InvalidChunkedRange { .. })));
            result?; Ok(())
        },
        invalid {
            let chunks = varlit::ChunkedBytes::from_zstd_chunks(&[3; 33], 16, varlit::CompressionLevel::Fast)?;
            assert!(matches!(chunks.stream_range(33..34, &mut [0; 1], 1024, |_, _| panic!("out of bounds delivery")), Err(varlit::Error::InvalidChunkedRange { start: 33, end: 34, len: 33 })));
            Ok(())
        }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:open_lazy_or_scan@b67fa4b69671d7fd",
        valid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            let result = varlit::VarlitFile::open_lazy_or_scan(spec, &path);
            assert!(result?.block_tail_offset(1).is_some());
            Ok(())
        },
        invalid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            assert!(matches!(varlit::VarlitFile::open_lazy_or_scan(spec, path.with_extension("missing")),
                Err(varlit::Error::Io(ref error)) if error.kind() == std::io::ErrorKind::NotFound));
            Ok(())
        }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:open_lazy_or_scan_with_report@86dfac747b0f39f9",
        valid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            let result = varlit::VarlitFile::open_lazy_or_scan_with_report(spec, &path);
            let (file, route) = result?; assert_eq!(route, varlit::LazyOpenSource::FullScan); assert!(file.block_tail_offset(1).is_some());
            Ok(())
        },
        invalid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            assert!(matches!(varlit::VarlitFile::open_lazy_or_scan_with_report(spec, path.with_extension("missing")),
                Err(varlit::Error::Io(ref error)) if error.kind() == std::io::ErrorKind::NotFound));
            Ok(())
        }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:open_readonly_lazy_or_scan@a6d84ace33390fb0",
        valid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            let result = varlit::VarlitFile::open_readonly_lazy_or_scan(spec, &path);
            assert!(result?.block_tail_offset(1).is_some());
            Ok(())
        },
        invalid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            assert!(matches!(varlit::VarlitFile::open_readonly_lazy_or_scan(spec, path.with_extension("missing")),
                Err(varlit::Error::Io(ref error)) if error.kind() == std::io::ErrorKind::NotFound));
            Ok(())
        }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:open_readonly_lazy_or_scan_with_report@0197d791d06f9d16",
        valid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            let result = varlit::VarlitFile::open_readonly_lazy_or_scan_with_report(spec, &path);
            let (file, route) = result?; assert_eq!(route, varlit::LazyOpenSource::FullScan); assert!(file.block_tail_offset(1).is_some());
            Ok(())
        },
        invalid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            assert!(matches!(varlit::VarlitFile::open_readonly_lazy_or_scan_with_report(spec, path.with_extension("missing")),
                Err(varlit::Error::Io(ref error)) if error.kind() == std::io::ErrorKind::NotFound));
            Ok(())
        }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:open_readonly_scan@d4e1e84b92bcf335",
        valid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            let result = varlit::VarlitFile::open_readonly_scan(spec, &path);
            assert!(result?.block_tail_offset(1).is_some());
            Ok(())
        },
        invalid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            assert!(matches!(varlit::VarlitFile::open_readonly_scan(spec, path.with_extension("missing")),
                Err(varlit::Error::Io(ref error)) if error.kind() == std::io::ErrorKind::NotFound));
            Ok(())
        }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:open_scan@be289bb319408808",
        valid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            let result = varlit::VarlitFile::open_scan(spec, &path);
            assert!(result?.block_tail_offset(1).is_some());
            Ok(())
        },
        invalid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            assert!(matches!(varlit::VarlitFile::open_scan(spec, path.with_extension("missing")),
                Err(varlit::Error::Io(ref error)) if error.kind() == std::io::ErrorKind::NotFound));
            Ok(())
        }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:open_lazy_or_scan@b67fa4b69671d7fd",
        valid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            let result = varlit::VarlitWriter::open_lazy_or_scan(spec, &path);
            assert!(result?.into_inner().block_tail_offset(1).is_some());
            Ok(())
        },
        invalid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            assert!(matches!(varlit::VarlitWriter::open_lazy_or_scan(spec, path.with_extension("missing")),
                Err(varlit::Error::Io(ref error)) if error.kind() == std::io::ErrorKind::NotFound));
            Ok(())
        }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:open_lazy_or_scan_with_report@86dfac747b0f39f9",
        valid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            let result = varlit::VarlitWriter::open_lazy_or_scan_with_report(spec, &path);
            let (file, route) = result?; assert_eq!(route, varlit::LazyOpenSource::FullScan); assert!(file.into_inner().block_tail_offset(1).is_some());
            Ok(())
        },
        invalid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            assert!(matches!(varlit::VarlitWriter::open_lazy_or_scan_with_report(spec, path.with_extension("missing")),
                Err(varlit::Error::Io(ref error)) if error.kind() == std::io::ErrorKind::NotFound));
            Ok(())
        }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:open_scan@be289bb319408808",
        valid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            let result = varlit::VarlitWriter::open_scan(spec, &path);
            assert!(result?.into_inner().block_tail_offset(1).is_some());
            Ok(())
        },
        invalid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            assert!(matches!(varlit::VarlitWriter::open_scan(spec, path.with_extension("missing")),
                Err(varlit::Error::Io(ref error)) if error.kind() == std::io::ErrorKind::NotFound));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::Error::variant:LazyOpenMetadataUnavailable@540105b595b44cdf",
        valid {
            let (_directory, path, spec) = lazy_open_fixture(true)?;
            let result = varlit::VarlitFile::open_readonly_lazy(spec, &path);
            assert!(!matches!(result, Err(varlit::Error::LazyOpenMetadataUnavailable { .. })));
            result?; Ok(())
        },
        invalid {
            let (_directory, path, spec) = lazy_open_fixture(false)?;
            assert!(matches!(varlit::VarlitFile::open_readonly_lazy(spec, &path),
                Err(varlit::Error::LazyOpenMetadataUnavailable { metadata: varlit::LazyOpenSource::Digest })));
            Ok(())
        }
    ),
    contract!("core/error.rs::error::Error::variant:LazyOpenVerificationRequiresScan@17c2504a486e68d9",
        valid {
            let (_directory, path, spec) = lazy_open_fixture(true)?;
            let result = varlit::VarlitFile::open_readonly_lazy(spec, &path);
            assert!(!matches!(result, Err(varlit::Error::LazyOpenVerificationRequiresScan)));
            result?; Ok(())
        },
        invalid {
            let (_directory, path, spec) = lazy_open_fixture(true)?;
            let spec = spec.with_read_limits(spec.read_limits.with_integrity_verification(varlit::IntegrityVerification::AtOpen));
            assert!(matches!(varlit::VarlitFile::open_readonly_lazy(spec, &path),
                Err(varlit::Error::LazyOpenVerificationRequiresScan)));
            Ok(())
        }
    ),
    // Decoding into caller-owned values: codec surface.
    contract!("core/codec.rs::codec::Decoder < 'a >::fn:decode_value_into@78a2e7760734eeca",
        valid { let bytes=varlit::encode_to_vec(&vec![7u64,8],varlit::Endian::Little)?; let mut out=vec![1u64;9]; let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); d.decode_value_into(&mut out)?; assert_eq!(out,[7,8]); assert_eq!(d.remaining(),0); Ok(()) },
        invalid { let bytes=varlit::encode_to_vec(&vec![7u64,8],varlit::Endian::Little)?; let mut out=Vec::<u64>::new(); let mut d=varlit::Decoder::new_limited(&bytes,varlit::Endian::Little,15); assert!(matches!(d.decode_value_into(&mut out),Err(varlit::Error::LimitExceeded { .. }))); let mut d=varlit::Decoder::new_limited(&[],varlit::Endian::Little,8); let mut replaced=ReplacingCodec; assert!(d.decode_value_into(&mut replaced).is_err()); Ok(()) }
    ),
    contract!("core/codec.rs::codec::Decoder < 'a >::fn:decode_nested_into@9f6b3fafbe037f0b",
        valid { let inner=varlit::encode_to_vec(&vec![7u64,8],varlit::Endian::Little)?; let mut d=varlit::Decoder::new(&[],varlit::Endian::Little); let mut out=vec![1u64;9]; d.decode_nested_into(&inner,&mut out)?; assert_eq!(out,[7,8]); Ok(()) },
        invalid { let inner=varlit::encode_to_vec(&vec![7u64,8],varlit::Endian::Little)?; let mut d=varlit::Decoder::new_limited(&[],varlit::Endian::Little,15); let mut out=Vec::<u64>::new(); assert!(matches!(d.decode_nested_into(&inner,&mut out),Err(varlit::Error::LimitExceeded { .. }))); let mut d=varlit::Decoder::new(&[],varlit::Endian::Little); let mut long=inner.clone(); long.push(0); assert!(matches!(d.decode_nested_into(&long,&mut out),Err(varlit::Error::TrailingBytes { .. }))); Ok(()) }
    ),
    contract!("core/codec.rs::codec::Decoder < 'a >::fn:decode_from_slice_limited_into@8d35b58fb7948b6c",
        valid { let bytes=varlit::encode_to_vec(&vec![7u64,8],varlit::Endian::Little)?; let mut out=vec![1u64;9]; varlit::Decoder::decode_from_slice_limited_into(&bytes,varlit::Endian::Little,16,&mut out)?; assert_eq!(out,[7,8]); Ok(()) },
        invalid { let bytes=varlit::encode_to_vec(&vec![7u64,8],varlit::Endian::Little)?; let mut out=Vec::<u64>::new(); assert!(matches!(varlit::Decoder::decode_from_slice_limited_into(&bytes,varlit::Endian::Little,15,&mut out),Err(varlit::Error::LimitExceeded { limit: 15, .. }))); Ok(()) }
    ),
    contract!("core/codec.rs::codec::Decoder < 'a >::fn:decode_from_slice_accounted_into@b6f649a71a9ca0cb",
        valid { let bytes=varlit::encode_to_vec(&vec![7u64,8],varlit::Endian::Little)?; let mut remaining=1024; let mut out=vec![1u64;9]; varlit::Decoder::decode_from_slice_accounted_into(&bytes,varlit::Endian::Little,&mut remaining,&mut out)?; assert_eq!(out,[7,8]); assert_eq!(remaining,1024-16); Ok(()) },
        invalid { let bytes=varlit::encode_to_vec(&vec![7u64,8],varlit::Endian::Little)?; let mut remaining=15; let mut out=Vec::<u64>::new(); assert!(matches!(varlit::Decoder::decode_from_slice_accounted_into(&bytes,varlit::Endian::Little,&mut remaining,&mut out),Err(varlit::Error::LimitExceeded { .. }))); assert!(remaining<=15); Ok(()) }
    ),
    contract!("core/codec.rs::codec::VarlitDecode::fn:decode_varlit_into@683fe642c844d98f",
        valid { let bytes=varlit::encode_to_vec(&String::from("abc"),varlit::Endian::Little)?; let mut out=String::from("a stale longer value"); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); <String as varlit::VarlitDecode>::decode_varlit_into(&mut out,&mut d)?; assert_eq!(out,"abc"); assert_eq!(d.remaining(),0); Ok(()) },
        invalid { let mut bytes=varlit::encode_to_vec(&String::from("abc"),varlit::Endian::Little)?; *bytes.last_mut().unwrap()=0xff; let mut out=String::new(); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert!(matches!(<String as varlit::VarlitDecode>::decode_varlit_into(&mut out,&mut d),Err(varlit::Error::InvalidUtf8))); Ok(()) }
    ),
    contract!("core/codec.rs::codec::VarlitEmpty::fn:empty@f2d70a92c0ed97d0",
        valid { let mut out=<Probe as varlit::VarlitEmpty>::empty(); assert_eq!(out,Probe{value:0}); varlit::decode_from_slice_into(&varlit::encode_to_vec(&Probe{value:7},varlit::Endian::Little)?,varlit::Endian::Little,&mut out)?; assert_eq!(out,Probe{value:7}); Ok(()) },
        invalid { let bytes=varlit::encode_to_vec(&Probe{value:7},varlit::Endian::Little)?; let mut out=<Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(varlit::decode_from_slice_into(&bytes[..bytes.len()-1],varlit::Endian::Little,&mut out),Err(varlit::Error::UnexpectedEof))); Ok(()) }
    ),
    contract!("core/codec.rs::codec::trait:VarlitEmpty@cbf29ce484222325",
        valid { let mut out=<Probe as varlit::VarlitEmpty>::empty(); assert_eq!(out,Probe{value:0}); varlit::decode_from_slice_into(&varlit::encode_to_vec(&Probe{value:7},varlit::Endian::Little)?,varlit::Endian::Little,&mut out)?; assert_eq!(out,Probe{value:7}); Ok(()) },
        invalid { let bytes=varlit::encode_to_vec(&Probe{value:7},varlit::Endian::Little)?; let mut out=<Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(varlit::decode_from_slice_into(&bytes[..bytes.len()-1],varlit::Endian::Little,&mut out),Err(varlit::Error::UnexpectedEof))); Ok(()) }
    ),
    contract!("core/lib.rs::::export:VarlitEmpty@4b57d884a6fd10de",
        valid { let mut out=<Probe as varlit::VarlitEmpty>::empty(); assert_eq!(out,Probe{value:0}); varlit::decode_from_slice_into(&varlit::encode_to_vec(&Probe{value:7},varlit::Endian::Little)?,varlit::Endian::Little,&mut out)?; assert_eq!(out,Probe{value:7}); Ok(()) },
        invalid { let bytes=varlit::encode_to_vec(&Probe{value:7},varlit::Endian::Little)?; let mut out=<Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(varlit::decode_from_slice_into(&bytes[..bytes.len()-1],varlit::Endian::Little,&mut out),Err(varlit::Error::UnexpectedEof))); Ok(()) }
    ),
    contract!("core/codec.rs::codec::macro:empty_zero@c53b949f57373b8b", generated "empty",
        valid { assert_eq!(<u64 as varlit::VarlitEmpty>::empty(),0); assert_eq!(<String as varlit::VarlitEmpty>::empty(),""); assert!(!<bool as varlit::VarlitEmpty>::empty()); let mut out=<u64 as varlit::VarlitEmpty>::empty(); varlit::decode_from_slice_into(&7u64.to_le_bytes(),varlit::Endian::Little,&mut out)?; assert_eq!(out,7); Ok(()) },
        invalid { let mut out=<bool as varlit::VarlitEmpty>::empty(); assert!(varlit::decode_from_slice_into(&[2u8],varlit::Endian::Little,&mut out).is_err()); let mut out=<u64 as varlit::VarlitEmpty>::empty(); assert!(matches!(varlit::decode_from_slice_into(&[7u8;7],varlit::Endian::Little,&mut out),Err(varlit::Error::UnexpectedEof))); Ok(()) }
    ),
    contract!("core/codec.rs::codec::macro:empty_zero@c9d1905d6a340899", generated "empty",
        valid { assert_eq!(<i32 as varlit::VarlitEmpty>::empty(),0); assert_eq!(<f64 as varlit::VarlitEmpty>::empty(),0.0); assert_eq!(<() as varlit::VarlitEmpty>::empty(),()); let mut out=<String as varlit::VarlitEmpty>::empty(); varlit::decode_from_slice_into(&varlit::encode_to_vec(&String::from("x"),varlit::Endian::Little)?,varlit::Endian::Little,&mut out)?; assert_eq!(out,"x"); Ok(()) },
        invalid { let mut bytes=varlit::encode_to_vec(&String::from("x"),varlit::Endian::Little)?; *bytes.last_mut().unwrap()=0xff; let mut out=<String as varlit::VarlitEmpty>::empty(); assert!(matches!(varlit::decode_from_slice_into(&bytes,varlit::Endian::Little,&mut out),Err(varlit::Error::InvalidUtf8))); Ok(()) }
    ),
    contract!("core/codec.rs::codec::VarlitEncode::fn:__varlit_encode_slice@af18c0ae6b18a6cb",
        valid { let mut e=varlit::Encoder::new(varlit::Endian::Little); <u16 as varlit::VarlitEncode>::__varlit_encode_slice(&[1,0x0203],&mut e)?; assert_eq!(e.into_inner(),[1,0,3,2]); let mut e=varlit::Encoder::new(varlit::Endian::Big); <u16 as varlit::VarlitEncode>::__varlit_encode_slice(&[1,0x0203],&mut e)?; assert_eq!(e.into_inner(),[0,1,2,3]); Ok(()) },
        invalid { let mut e=varlit::Encoder::new(varlit::Endian::Little); assert!(matches!(<RefusingEncode as varlit::VarlitEncode>::__varlit_encode_slice(&[RefusingEncode],&mut e),Err(varlit::Error::InvalidCanonicalEncoding(_)))); assert!(e.into_inner().is_empty()); Ok(()) }
    ),
    contract!("core/codec.rs::codec::fn:decode_from_slice_into@82f903a674bd08be",
        valid { let value=vec![String::from("a"),String::from("bc")]; let bytes=varlit::encode_to_vec(&value,varlit::Endian::Big)?; let mut out=vec![String::from("stale");4]; varlit::decode_from_slice_into(&bytes,varlit::Endian::Big,&mut out)?; assert_eq!(out,value); Ok(()) },
        invalid { let mut bytes=varlit::encode_to_vec(&String::from("abc"),varlit::Endian::Little)?; bytes.push(0); let mut out=String::new(); assert!(matches!(varlit::decode_from_slice_into(&bytes,varlit::Endian::Little,&mut out),Err(varlit::Error::TrailingBytes { .. }))); Ok(()) }
    ),
    contract!("core/lib.rs::::export:decode_from_slice_into@cf2a7f5c7baac916",
        valid { let value=vec![String::from("a"),String::from("bc")]; let bytes=varlit::encode_to_vec(&value,varlit::Endian::Big)?; let mut out=vec![String::from("stale");4]; varlit::decode_from_slice_into(&bytes,varlit::Endian::Big,&mut out)?; assert_eq!(out,value); Ok(()) },
        invalid { let mut bytes=varlit::encode_to_vec(&String::from("abc"),varlit::Endian::Little)?; bytes.push(0); let mut out=String::new(); assert!(matches!(varlit::decode_from_slice_into(&bytes,varlit::Endian::Little,&mut out),Err(varlit::Error::TrailingBytes { .. }))); Ok(()) }
    ),
    contract!("core/codec.rs::codec::fn:encode_field_in_place@47e59c96bef055a0",
        valid { let value=vec![1u16,2]; let mut staged=varlit::Encoder::new(varlit::Endian::Little); let payload=staged.encode_nested_to_vec(&value)?; varlit::__core::write_field(&mut staged,3,<Vec<u16> as varlit::VarlitEncode>::WIRE_TYPE,&payload)?; let mut direct=varlit::Encoder::new(varlit::Endian::Little); varlit::encode_field_in_place(&mut direct,3,&value)?; assert_eq!(direct.into_inner(),staged.into_inner()); Ok(()) },
        invalid { let mut e=varlit::Encoder::new(varlit::Endian::Little); assert!(matches!(varlit::encode_field_in_place(&mut e,0,&7u64),Err(varlit::Error::InvalidCanonicalEncoding(_)))); assert!(e.into_inner().is_empty()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:encode_field_in_place@c438d8e7b9006f80",
        valid { let value=vec![1u16,2]; let mut staged=varlit::Encoder::new(varlit::Endian::Little); let payload=staged.encode_nested_to_vec(&value)?; varlit::__core::write_field(&mut staged,3,<Vec<u16> as varlit::VarlitEncode>::WIRE_TYPE,&payload)?; let mut direct=varlit::Encoder::new(varlit::Endian::Little); varlit::encode_field_in_place(&mut direct,3,&value)?; assert_eq!(direct.into_inner(),staged.into_inner()); Ok(()) },
        invalid { let mut e=varlit::Encoder::new(varlit::Endian::Little); assert!(matches!(varlit::encode_field_in_place(&mut e,0,&7u64),Err(varlit::Error::InvalidCanonicalEncoding(_)))); assert!(e.into_inner().is_empty()); Ok(()) }
    ),
    // Decoding into caller-owned values: collections and streams.
    contract!("core/collections.rs::collections::BlockVec < T >::fn:get@c84dea65ea52461c",
        valid { let (_directory, reader, _offset) = probe(false)?; let blocks = reader.blocks::<Probe>()?; let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(blocks.get(0, &mut out)?); assert_eq!(out, Probe { value: 19 }); assert!(!blocks.get(1, &mut out)?); assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(true)?; let blocks = reader.blocks::<Probe>()?; let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(blocks.get(0, &mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/collections.rs::collections::BlockVec < T >::fn:get_into@ff6c17469d86c610",
        valid { let (_directory, reader, _offset) = probe(false)?; let blocks = reader.blocks::<Probe>()?; let (mut payload, mut out) = (Vec::new(), <Probe as varlit::VarlitEmpty>::empty()); assert!(blocks.get_into(0, &mut payload, &mut out)?); assert_eq!(out, Probe { value: 19 }); assert!(!blocks.get_into(1, &mut payload, &mut out)?); assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(true)?; let blocks = reader.blocks::<Probe>()?; let (mut payload, mut out) = (Vec::new(), <Probe as varlit::VarlitEmpty>::empty()); assert!(matches!(blocks.get_into(0, &mut payload, &mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/collections.rs::collections::BlockVec < T >::fn:cursor@36d86d7d49fea490",
        valid { let (_directory, reader, _offset) = probe(false)?; let blocks = reader.blocks::<Probe>()?; let mut cursor = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(cursor.next_into(&mut out)?); assert_eq!(out, Probe { value: 19 }); assert!(!cursor.next_into(&mut out)?); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(true)?; let blocks = reader.blocks::<Probe>()?; let mut cursor = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(cursor.next_into(&mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/collections.rs::collections::BlockCursor < '_ , T >::fn:next_into@3543e49de9ffc7d2",
        valid { let (_directory, reader, _offset) = probe(false)?; let blocks = reader.blocks::<Probe>()?; let mut cursor = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(cursor.next_into(&mut out)?); assert_eq!(out, Probe { value: 19 }); assert!(!cursor.next_into(&mut out)?); assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(true)?; let blocks = reader.blocks::<Probe>()?; let mut cursor = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(cursor.next_into(&mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/collections.rs::collections::struct:BlockCursor@4866f029fb7abaa7",
        valid { let (_directory, reader, _offset) = probe(false)?; let blocks = reader.blocks::<Probe>()?; let mut cursor: varlit::BlockCursor<'_, Probe> = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(cursor.next_into(&mut out)?); assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(true)?; let blocks = reader.blocks::<Probe>()?; let mut cursor: varlit::BlockCursor<'_, Probe> = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(cursor.next_into(&mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/lib.rs::::export:BlockCursor@9ee06a732cea7753",
        valid { let (_directory, reader, _offset) = probe(false)?; let blocks = reader.blocks::<Probe>()?; let mut cursor: varlit::BlockCursor<'_, Probe> = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(cursor.next_into(&mut out)?); assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(true)?; let blocks = reader.blocks::<Probe>()?; let mut cursor: varlit::BlockCursor<'_, Probe> = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(cursor.next_into(&mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/collections.rs::collections::KeyedBlockVec < K , T >::fn:get@f0d838e8727d3d62",
        valid { let (_directory, keyed) = keyed_probe(false)?; let mut out = <KeyedProbe as varlit::VarlitEmpty>::empty(); assert!(keyed.get(&7, &mut out)?); assert_eq!(out, KeyedProbe { id: 7, body: "seven".into() }); assert!(!keyed.get(&8, &mut out)?); assert_eq!(out.id, 7); Ok(()) },
        invalid { let (_directory, keyed) = keyed_probe(true)?; let mut out = <KeyedProbe as varlit::VarlitEmpty>::empty(); assert!(matches!(keyed.get(&7, &mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/stream.rs::stream::StreamingBlocks < T >::fn:next_into@3543e49de9ffc7d2",
        valid { let (_directory, reader) = indexed_fixture(false)?; let reader = reader.into_inner(); let mut blocks = reader.blocks::<IndexedEntry>()?; let mut out = <IndexedEntry as varlit::VarlitEmpty>::empty(); assert!(blocks.next_into(&mut out)?); assert_eq!(out, IndexedEntry { id: 7, value: 70 }); assert!(!blocks.next_into(&mut out)?); assert_eq!(out.value, 70); Ok(()) },
        invalid { let (_directory, reader) = indexed_fixture(true)?; let reader = reader.into_inner(); let mut blocks = reader.blocks::<IndexedEntry>()?; let mut out = <IndexedEntry as varlit::VarlitEmpty>::empty(); assert!(matches!(blocks.next_into(&mut out), Err(varlit::Error::ChecksumMismatch { .. }))); assert!(!blocks.next_into(&mut out)?); Ok(()) }
    ),
    contract!("core/indexed.rs::indexed::VarlitIndexedReader::fn:get@dc02727698bf29fb",
        valid { let (_directory, reader) = indexed_fixture(false)?; let reader = reader.into_inner(); let mut out = <IndexedEntry as varlit::VarlitEmpty>::empty(); assert!(reader.get::<IndexedEntry>(&7, &mut out)?); assert_eq!(out, IndexedEntry { id: 7, value: 70 }); assert!(!reader.get::<IndexedEntry>(&8, &mut out)?); assert_eq!(out.value, 70); Ok(()) },
        invalid { let (_directory, reader) = indexed_fixture(false)?; let reader = reader.into_inner(); let mut out = <ChainEntry as varlit::VarlitEmpty>::empty(); assert!(reader.get::<ChainEntry>(&7, &mut out).unwrap_err().to_string().contains("key_index = disk")); let (_damaged, reader) = indexed_fixture(true)?; let reader = reader.into_inner(); let mut out = <IndexedEntry as varlit::VarlitEmpty>::empty(); assert!(matches!(reader.get::<IndexedEntry>(&7, &mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    // Decoding into caller-owned values: file handles.
    contract!("core/file.rs::file::VarlitFile::fn:decode_block_into@74051333b12869e1",
        valid { let (_directory, reader, offset) = probe(false)?; let entry = reader.record_entry_at(offset)?; let mut out = <Probe as varlit::VarlitEmpty>::empty(); reader.decode_block_into(&entry, &mut Vec::new(), &mut out)?; assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, offset) = probe(false)?; let mut entry = reader.record_entry_at(offset)?; entry.block_version += 1; let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(reader.decode_block_into(&entry, &mut Vec::new(), &mut out), Err(varlit::Error::BlockVersionMismatch { .. }))); let (_damaged, reader, offset) = probe(true)?; let entry = reader.record_entry_at(offset)?; assert!(matches!(reader.decode_block_into(&entry, &mut Vec::new(), &mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::DirectoryRead < '_ , D >::fn:decode_block_into@74051333b12869e1",
        valid { let (_directory, reader, _offset) = probe(false)?; let mut entries = Vec::new(); reader.index_entries_into(&mut entries)?; let view = reader.with_directory(&entries)?; let mut out = <Probe as varlit::VarlitEmpty>::empty(); view.decode_block_into(&entries[0], &mut Vec::new(), &mut out)?; assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(false)?; let mut entries = Vec::new(); reader.index_entries_into(&mut entries)?; let view = reader.with_directory(&entries)?; let mut forged = entries[0].clone(); forged.block_version += 1; let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(view.decode_block_into(&forged, &mut Vec::new(), &mut out), Err(varlit::Error::BlockVersionMismatch { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:decode_block_into@74051333b12869e1",
        valid { let (directory, _file, _offset) = probe(false)?; let reader = ProbeFormat::spec().open_reader(directory.path().join("contract.varlit"))?; let mut entries = Vec::new(); reader.index_entries_into(&mut entries)?; let mut out = <Probe as varlit::VarlitEmpty>::empty(); reader.decode_block_into(&entries[0], &mut Vec::new(), &mut out)?; assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (directory, _file, _offset) = probe(false)?; let reader = ProbeFormat::spec().open_reader(directory.path().join("contract.varlit"))?; let mut entries = Vec::new(); reader.index_entries_into(&mut entries)?; let mut forged = entries[0].clone(); forged.block_version += 1; let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(reader.decode_block_into(&forged, &mut Vec::new(), &mut out), Err(varlit::Error::BlockVersionMismatch { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:decode_block_into@74051333b12869e1",
        valid { let directory = private_tempdir::private_tempdir()?; let mut writer = ProbeFormat::spec().create_writer(directory.path().join("writer.varlit"))?; writer.push(&Probe { value: 23 })?; let mut entries = Vec::new(); writer.index_entries_into(&mut entries)?; let mut out = <Probe as varlit::VarlitEmpty>::empty(); writer.decode_block_into(&entries[0], &mut Vec::new(), &mut out)?; assert_eq!(out, Probe { value: 23 }); Ok(()) },
        invalid { let directory = private_tempdir::private_tempdir()?; let mut writer = ProbeFormat::spec().create_writer(directory.path().join("writer.varlit"))?; writer.push(&Probe { value: 23 })?; let mut entries = Vec::new(); writer.index_entries_into(&mut entries)?; let mut forged = entries[0].clone(); forged.block_version += 1; let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(writer.decode_block_into(&forged, &mut Vec::new(), &mut out), Err(varlit::Error::BlockVersionMismatch { .. }))); Ok(()) }
    ),
    contract!("core/file/mmap_views.rs::file::mmap_views::MmapPayloads::fn:decode_block_into@74051333b12869e1",
        valid { let (_directory, reader, offset) = probe(false)?; let entry = reader.record_entry_at(offset)?; // SAFETY: the contract owns the file and does not modify it while mapped.
            let map = unsafe { reader.mmap_payloads()? }; let mut out = <Probe as varlit::VarlitEmpty>::empty(); map.decode_block_into(&entry, &mut Vec::new(), &mut out)?; assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, offset) = probe(false)?; let mut entry = reader.record_entry_at(offset)?; entry.block_version += 1; // SAFETY: the contract owns the file and does not modify it while mapped.
            let map = unsafe { reader.mmap_payloads()? }; let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(map.decode_block_into(&entry, &mut Vec::new(), &mut out), Err(varlit::Error::BlockVersionMismatch { .. }))); Ok(()) }
    ),
    // Mapping a range of records.
    contract!("core/file.rs::file::VarlitFile::fn:mmap_payload_range@b95d73919576f4a4",
        valid { let (_directory, file, offset) = probe(false)?; let entry = file.record_entry_at(offset)?; // SAFETY: the contract owns the file and does not modify it while mapped.
            let map = unsafe { file.mmap_payload_range(0..1)? }; let mut out = <Probe as varlit::VarlitEmpty>::empty(); map.decode_block_into(&entry, &mut Vec::new(), &mut out)?; assert_eq!(out, Probe { value: 19 }); // SAFETY: as above.
            let empty = unsafe { file.mmap_payload_range(1..1)? }; assert!(empty.is_empty()); assert!(matches!(empty.payload_window(&entry), Err(varlit::Error::MmapEntryNotInSnapshot))); Ok(()) },
        invalid { let (_directory, file, _offset) = probe(false)?; // SAFETY: nothing is mapped for a refused range.
            assert!(matches!(unsafe { file.mmap_payload_range(0..2) }, Err(varlit::Error::MmapRecordRangeOutOfBounds { start: 0, end: 2, records: 1 }))); let (_damaged, file, offset) = probe(true)?; let entry = file.record_entry_at(offset)?; // SAFETY: the contract owns the file and does not modify it while mapped.
            let map = unsafe { file.mmap_payload_range(0..1)? }; assert!(matches!(map.payload_window(&entry), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:mmap_payload_range@b95d73919576f4a4",
        valid { let (directory, _file, _offset) = probe(false)?; let reader = ProbeFormat::spec().open_reader(directory.path().join("contract.varlit"))?; // SAFETY: the contract owns the file and does not modify it while mapped.
            let map = unsafe { reader.mmap_payload_range(0..1)? }; let mut out = <Probe as varlit::VarlitEmpty>::empty(); map.decode_block_into(&map.index_entries()[0].clone(), &mut Vec::new(), &mut out)?; assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (directory, _file, _offset) = probe(false)?; let reader = ProbeFormat::spec().open_reader(directory.path().join("contract.varlit"))?; // SAFETY: nothing is mapped for a refused range.
            assert!(matches!(unsafe { reader.mmap_payload_range(1..3) }, Err(varlit::Error::MmapRecordRangeOutOfBounds { .. }))); Ok(()) }
    ),
    contract!("core/error.rs::error::Error::variant:MmapRecordRangeOutOfBounds@8376ae248452f5ad",
        valid { let (_directory, file, _offset) = probe(false)?; // SAFETY: the contract owns the file and does not modify it while mapped.
            let fits = unsafe { file.mmap_payload_range(0..1) }; assert!(!matches!(fits, Err(varlit::Error::MmapRecordRangeOutOfBounds { .. }))); assert_eq!(fits?.len(), 1); Ok(()) },
        invalid { let (_directory, file, _offset) = probe(false)?; // SAFETY: nothing is mapped for a refused range.
            let error = unsafe { file.mmap_payload_range(0..5) }.unwrap_err(); assert!(matches!(error, varlit::Error::MmapRecordRangeOutOfBounds { start: 0, end: 5, records: 1 })); assert!(error.to_string().contains("0..5")); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:record_count@a4d70077dd9c37f9",
        valid { let (_directory, file, _offset) = probe(false)?; assert_eq!(file.record_count()?, 1); Ok(()) },
        invalid { let (directory, _file, _offset) = probe(false)?; let file = varlit::VarlitFile::open_readonly_without_directory(ProbeFormat::spec(), directory.path().join("contract.varlit"), &mut Vec::new())?; assert!(matches!(file.record_count(), Err(varlit::Error::NoResidentDirectory { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:record_count@a4d70077dd9c37f9",
        valid { let (directory, _file, _offset) = probe(false)?; let reader = ProbeFormat::spec().open_reader(directory.path().join("contract.varlit"))?; assert_eq!(reader.record_count()?, 1); Ok(()) },
        invalid { let (directory, _file, _offset) = probe(false)?; let reader = varlit::VarlitReader::open_without_directory(ProbeFormat::spec(), directory.path().join("contract.varlit"), &mut Vec::new())?; assert!(matches!(reader.record_count(), Err(varlit::Error::NoResidentDirectory { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:read_header_block@7d069e615ec8e885",
        valid { let (_directory, path) = slots_fixture()?; let file = IntoSlots::spec().open_readonly(&path)?; let mut out = <SlotMark as varlit::VarlitEmpty>::empty(); assert!(file.read_header_block::<SlotMark>(&mut out)?); assert_eq!(out, SlotMark { at: 5 }); Ok(()) },
        invalid { let (_directory, path) = slots_fixture()?; let file = IntoSlots::spec().open_readonly(&path)?; let mut out = <SlotOther as varlit::VarlitEmpty>::empty(); assert!(matches!(file.read_header_block::<SlotOther>(&mut out), Err(varlit::Error::InvalidFormatSpec(_)))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:read_header_block@7d069e615ec8e885",
        valid { let (_directory, path) = slots_fixture()?; let reader = IntoSlots::spec().open_reader(&path)?; let mut out = <SlotMark as varlit::VarlitEmpty>::empty(); assert!(reader.read_header_block::<SlotMark>(&mut out)?); assert_eq!(out, SlotMark { at: 5 }); Ok(()) },
        invalid { let (_directory, path) = slots_fixture()?; let reader = IntoSlots::spec().open_reader(&path)?; let mut out = <SlotOther as varlit::VarlitEmpty>::empty(); assert!(matches!(reader.read_header_block::<SlotOther>(&mut out), Err(varlit::Error::InvalidFormatSpec(_)))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:read_header_block@7d069e615ec8e885",
        valid { let (_directory, path) = slots_fixture()?; let writer = IntoSlots::spec().open_writer(&path)?; let mut out = <SlotMark as varlit::VarlitEmpty>::empty(); assert!(writer.read_header_block::<SlotMark>(&mut out)?); assert_eq!(out, SlotMark { at: 5 }); Ok(()) },
        invalid { let (_directory, path) = slots_fixture()?; let writer = IntoSlots::spec().open_writer(&path)?; let mut out = <SlotOther as varlit::VarlitEmpty>::empty(); assert!(matches!(writer.read_header_block::<SlotOther>(&mut out), Err(varlit::Error::InvalidFormatSpec(_)))); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:typed_api_tokens@88befb9cd83dcea9", generated "read_header_block",
        valid { let directory = private_tempdir::private_tempdir()?; let mut writer = IntoSlots::create_writer(directory.path().join("typed.varlit"))?; let mut out = <SlotMark as varlit::VarlitEmpty>::empty(); assert!(!writer.read_header_block::<SlotMark>(&mut out)?); writer.write_header_block(&SlotMark { at: 9 })?; assert!(writer.read_header_block::<SlotMark>(&mut out)?); assert_eq!(out, SlotMark { at: 9 }); Ok(()) },
        invalid { let directory = private_tempdir::private_tempdir()?; let writer = IntoSlots::create_writer(directory.path().join("typed.varlit"))?; let mut out = <SlotOther as varlit::VarlitEmpty>::empty(); assert!(matches!(writer.read_header_block::<SlotOther>(&mut out), Err(varlit::Error::InvalidFormatSpec(_)))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:read_matrix_cell@b1223a9f317f6615",
        valid { let (_directory, path) = grid_fixture()?; let file = GridMatrix::spec().open_readonly(&path)?; let mut out = <GridCell as varlit::VarlitEmpty>::empty(); file.read_matrix_cell::<GridCell>(varlit::MatrixKey::new(0, 0), &mut out)?; assert_eq!(out, GridCell { value: 11 }); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let file = GridMatrix::spec().open_readonly(&path)?; let mut out = <GridCell as varlit::VarlitEmpty>::empty(); assert!(matches!(file.read_matrix_cell::<GridCell>(varlit::MatrixKey::new(5, 0), &mut out), Err(varlit::Error::MatrixKeyOutOfBounds { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:read_matrix_cell@b1223a9f317f6615",
        valid { let (_directory, path) = grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = <GridCell as varlit::VarlitEmpty>::empty(); reader.read_matrix_cell::<GridCell>(varlit::MatrixKey::new(0, 0), &mut out)?; assert_eq!(out, GridCell { value: 11 }); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = <GridCell as varlit::VarlitEmpty>::empty(); assert!(matches!(reader.read_matrix_cell::<GridCell>(varlit::MatrixKey::new(5, 0), &mut out), Err(varlit::Error::MatrixKeyOutOfBounds { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:read_matrix_cell@b1223a9f317f6615",
        valid { let (_directory, path) = grid_fixture()?; let writer = GridMatrix::spec().open_writer(&path)?; let mut out = <GridCell as varlit::VarlitEmpty>::empty(); writer.read_matrix_cell::<GridCell>(varlit::MatrixKey::new(0, 0), &mut out)?; assert_eq!(out, GridCell { value: 11 }); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let writer = GridMatrix::spec().open_writer(&path)?; let mut out = <GridCell as varlit::VarlitEmpty>::empty(); assert!(matches!(writer.read_matrix_cell::<GridCell>(varlit::MatrixKey::new(5, 0), &mut out), Err(varlit::Error::MatrixKeyOutOfBounds { .. }))); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:matrix_reader_methods@f8eef4f5c4e48605", generated "grid_cell",
        valid { let (_directory, path) = grid_fixture()?; let reader = GridMatrix::open_reader(&path)?; let mut out = <GridCell as varlit::VarlitEmpty>::empty(); reader.grid_cell(GridCellKey { row: 0, col: 0 }, &mut out)?; assert_eq!(out, GridCell { value: 11 }); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let reader = GridMatrix::open_reader(&path)?; let mut out = <GridCell as varlit::VarlitEmpty>::empty(); assert!(matches!(reader.grid_cell(GridCellKey { row: 5, col: 0 }, &mut out), Err(varlit::Error::MatrixKeyOutOfBounds { .. }))); Ok(()) }
    ),
    contract!("core/experimental_matrix.rs::experimental_matrix::MatrixReader::fn:get@260ec515078f92a6",
        valid {
            // The module exists only under this feature; the gate runs with --all-features.
            #[cfg(feature = "experimental-matrix-reuse")]
            {
                use varlit::experimental_matrix::{MatrixOptions, MatrixWriter};
                let directory = private_tempdir::private_tempdir()?;
                let dims = || varlit::MatrixDimensions::from_pairs([("row", 4), ("col", 3)]);
                let options = MatrixOptions { reuse: true, rows_per_chunk: 4, ..MatrixOptions::default() };
                let mut writer = MatrixWriter::create(GridMatrix::spec().with_growing_matrix_dimension("row", 4), directory.path().join("reuse"), dims(), options)?;
                writer.put(varlit::MatrixKey::new(1, 2), &GridCell { value: 11 })?;
                writer.sync()?;
                let reader = writer.reader()?;
                let mut out = <GridCell as varlit::VarlitEmpty>::empty();
                reader.get::<GridCell>(varlit::MatrixKey::new(1, 2), &mut out)?;
                assert_eq!(out, GridCell { value: 11 });
            }
            Ok(())
        },
        invalid {
            #[cfg(feature = "experimental-matrix-reuse")]
            {
                use varlit::experimental_matrix::{MatrixOptions, MatrixWriter};
                let directory = private_tempdir::private_tempdir()?;
                let dims = || varlit::MatrixDimensions::from_pairs([("row", 4), ("col", 3)]);
                let options = MatrixOptions { reuse: true, rows_per_chunk: 4, ..MatrixOptions::default() };
                let mut writer = MatrixWriter::create(GridMatrix::spec().with_growing_matrix_dimension("row", 4), directory.path().join("reuse"), dims(), options)?;
                writer.put(varlit::MatrixKey::new(1, 2), &GridCell { value: 11 })?;
                writer.sync()?;
                let reader = writer.reader()?;
                let mut out = <GridCell as varlit::VarlitEmpty>::empty();
                assert!(reader.get::<GridCell>(varlit::MatrixKey::new(0, 9), &mut out).is_err());
            }
            Ok(())
        }
    ),
    // Single-barrier durable commits.
    contract!("core/format.rs::format::TransactionMarkerMode::variant:OnFlushSingleBarrier@5e8ea34cee9765a5",
        valid { let mode = varlit::TransactionMarkerMode::OnFlushSingleBarrier; let spec = BarrierProbe::spec().with_commit_policy(varlit::CommitPolicy::TransactionMarker(mode)); assert!(spec.commit_policy.marker_on_flush()); assert_eq!(single_barrier_probe(spec, false)?, [1, 2]); Ok(()) },
        invalid { let mode = varlit::TransactionMarkerMode::OnFlushSingleBarrier; let spec = BarrierProbe::spec().with_commit_policy(varlit::CommitPolicy::TransactionMarker(mode)); assert_eq!(single_barrier_probe(spec, true)?, [1]); let weak = spec.with_integrity_policy(varlit::IntegrityPolicy::Crc32); assert!(matches!(single_barrier_probe(weak, false).unwrap_err().downcast::<varlit::Error>().map(|e| *e), Ok(varlit::Error::InvalidFormatSpec(_)))); Ok(()) }
    ),
    contract!("core/format.rs::format::TransactionMarkerMode::variant:ExplicitSingleBarrier@12dbc5c0f735e778",
        valid { let mode = varlit::TransactionMarkerMode::ExplicitSingleBarrier; let spec = BarrierProbe::spec().with_commit_policy(varlit::CommitPolicy::TransactionMarker(mode)); assert!(!spec.commit_policy.marker_on_flush()); assert_eq!(single_barrier_probe(spec, false)?, [1, 2]); Ok(()) },
        invalid { let mode = varlit::TransactionMarkerMode::ExplicitSingleBarrier; let spec = BarrierProbe::spec().with_commit_policy(varlit::CommitPolicy::TransactionMarker(mode)); assert_eq!(single_barrier_probe(spec, true)?, [1]); let weak = spec.with_integrity_policy(varlit::IntegrityPolicy::Crc32); assert!(matches!(single_barrier_probe(weak, false).unwrap_err().downcast::<varlit::Error>().map(|e| *e), Ok(varlit::Error::InvalidFormatSpec(_)))); Ok(()) }
    ),
    contract!("core/format.rs::format::CommitPolicy::fn:single_barrier@1ee9aa2e9b5e0fb8",
        valid { let policy = varlit::CommitPolicy::TransactionMarker(varlit::TransactionMarkerMode::ExplicitSingleBarrier); assert!(policy.single_barrier()); assert!(varlit::CommitPolicy::TransactionMarker(varlit::TransactionMarkerMode::OnFlushSingleBarrier).single_barrier()); assert_eq!(single_barrier_probe(BarrierProbe::spec().with_commit_policy(policy), false)?, [1, 2]); Ok(()) },
        invalid { for policy in [varlit::CommitPolicy::None, varlit::CommitPolicy::RecordFooter, varlit::CommitPolicy::TransactionMarker(varlit::TransactionMarkerMode::OnFlush), varlit::CommitPolicy::TransactionMarker(varlit::TransactionMarkerMode::Explicit)] { assert!(!policy.single_barrier()); } let weak = BarrierProbe::spec().with_integrity_policy(varlit::IntegrityPolicy::Crc32); assert!(weak.commit_policy.single_barrier()); assert!(matches!(single_barrier_probe(weak, false).unwrap_err().downcast::<varlit::Error>().map(|e| *e), Ok(varlit::Error::InvalidFormatSpec(_)))); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:commit_tokens@424e8c3c9ce4b699", generated "spec",
        valid { let spec = BarrierProbe::spec(); assert_eq!(spec.commit_policy, varlit::CommitPolicy::TransactionMarker(varlit::TransactionMarkerMode::ExplicitSingleBarrier)); assert_eq!(single_barrier_probe(spec, false)?, [1, 2]); Ok(()) },
        invalid { let weak = WeakBarrierProbe::spec(); assert_eq!(weak.commit_policy, varlit::CommitPolicy::TransactionMarker(varlit::TransactionMarkerMode::OnFlushSingleBarrier)); assert!(matches!(single_barrier_probe(weak, false).unwrap_err().downcast::<varlit::Error>().map(|e| *e), Ok(varlit::Error::InvalidFormatSpec(_)))); Ok(()) }
    ),
    contract!("macros/modules/format_parse.rs::format_parse::generator:parse_commit_choice@915b2eb3e81984ba", generated "spec",
        valid { let spec = BarrierProbe::spec(); assert!(spec.commit_policy.single_barrier()); assert!(!spec.commit_policy.marker_on_flush()); assert_eq!(single_barrier_probe(spec, false)?, [1, 2]); Ok(()) },
        invalid { let weak = WeakBarrierProbe::spec(); assert!(weak.commit_policy.single_barrier()); assert!(weak.commit_policy.marker_on_flush()); assert!(matches!(single_barrier_probe(weak, false).unwrap_err().downcast::<varlit::Error>().map(|e| *e), Ok(varlit::Error::InvalidFormatSpec(_)))); Ok(()) }
    ),
    contract!("core/scalable_fault.rs::scalable_fault::const:REQUIRED_POINTS@f2ec619599566a9a",
        valid {
            // The module exists only under this feature; the gate runs with --all-features.
            #[cfg(feature = "scalable-fault-injection")]
            {
                let points = varlit::__core::scalable_fault::REQUIRED_POINTS;
                assert!(points.contains(&"checkpoint.head_copy"));
                assert_eq!(points.iter().collect::<BTreeSet<_>>().len(), points.len());
            }
            Ok(())
        },
        invalid {
            #[cfg(feature = "scalable-fault-injection")]
            {
                // The withdrawn data-sync point is no longer a registered persistence boundary.
                let points = varlit::__core::scalable_fault::REQUIRED_POINTS;
                assert!(!points.contains(&"checkpoint.data_sync"));
                assert!(!varlit::__core::scalable_fault::MATRIX_REUSE_POINTS.contains(&"checkpoint.data_sync"));
            }
            Ok(())
        }
    ),
];

fn lazy_open_fixture(
    summary: bool,
) -> Result<(tempfile::TempDir, std::path::PathBuf, varlit::FormatSpec), Box<dyn std::error::Error>>
{
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("lazy-contract.varlit");
    let spec = ProbeFormat::spec()
        .with_index_policy(varlit::IndexPolicy::BlockOffsetChain.with_open_digest_on_flush(summary))
        .with_commit_policy(varlit::CommitPolicy::RecordFooter);
    let mut writer = spec.create(&path)?;
    writer.push(&Probe { value: 7 })?;
    writer.flush()?;
    drop(writer);
    Ok((directory, path, spec))
}

#[derive(Default)]
struct Evidence {
    identifiers: BTreeSet<String>,
    calls: BTreeSet<String>,
    assertions: usize,
}

impl<'ast> Visit<'ast> for Evidence {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*call.func {
            self.calls
                .insert(path.path.segments.last().unwrap().ident.to_string());
        }
        syn::visit::visit_expr_call(self, call);
    }
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.calls.insert(call.method.to_string());
        syn::visit::visit_expr_method_call(self, call);
    }
    fn visit_member(&mut self, member: &'ast syn::Member) {
        if let syn::Member::Unnamed(index) = member {
            self.identifiers.insert(index.index.to_string());
        }
        syn::visit::visit_member(self, member);
    }
    fn visit_ident(&mut self, id: &'ast syn::Ident) {
        self.identifiers.insert(id.to_string());
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let name = mac.path.segments.last().unwrap().ident.to_string();
        self.identifiers.insert(name.clone());
        if matches!(
            name.as_str(),
            "assert" | "assert_eq" | "assert_ne" | "prop_assert" | "prop_assert_eq"
        ) {
            self.assertions += 1;
        }
        // Assertions contain the API expression itself. Parse normal macro
        // arguments where possible, retaining identifier tokens for DSLs.
        fn identifiers(tokens: impl ToTokens, out: &mut BTreeSet<String>) {
            for token in tokens.to_token_stream() {
                match token {
                    proc_macro2::TokenTree::Ident(id) => {
                        out.insert(id.to_string());
                    }
                    proc_macro2::TokenTree::Group(group) => identifiers(group.stream(), out),
                    _ => {}
                }
            }
        }
        identifiers(mac.tokens.clone(), &mut self.identifiers);
        if let Ok(expressions) = mac.parse_body_with(
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated,
        ) {
            for expression in expressions {
                self.visit_expr(&expression);
            }
        } else if name == "matches" {
            let first = mac.parse_body_with(|input: syn::parse::ParseStream| {
                let expression = input.parse::<syn::Expr>()?;
                let _rest = input.parse::<proc_macro2::TokenStream>()?;
                Ok(expression)
            });
            if let Ok(expression) = first {
                self.visit_expr(&expression);
            }
        }
    }
}

fn check_registration(surface: &Surface, contract: &Contract) -> Result<(), String> {
    for (name, source) in [
        ("valid", contract.valid_source),
        ("invalid", contract.invalid_source),
    ] {
        let block = syn::parse_str::<syn::Block>(source).map_err(|e| e.to_string())?;
        let mut evidence = Evidence::default();
        evidence.visit_block(&block);
        let generated =
            contract.symbol.contains("::generator:") || contract.symbol.contains("::macro:");
        let reference = contract.exercise.unwrap_or(&surface.reference);
        if generated != contract.exercise.is_some() {
            return Err(format!(
                "{}: only generator/macro contracts must declare the generated API they exercise",
                contract.symbol
            ));
        }
        let referenced = if contract.symbol.starts_with("feature/") {
            // Core-only features have no matching cfg in this facade test
            // crate. Exercise their observable API under workspace all-features.
            !evidence.calls.is_empty()
        } else if generated || contract.symbol.contains("::fn:") {
            evidence.calls.contains(reference)
        } else {
            evidence.identifiers.contains(reference)
        };
        if !referenced || evidence.assertions == 0 {
            return Err(format!(
                "{}: {name} contract must reference {} and assert its result",
                contract.symbol, surface.reference
            ));
        }
    }
    Ok(())
}

fn missing_contracts(
    current: &BTreeMap<String, Surface>,
    legacy: &BTreeSet<String>,
    contracts: &[Contract],
) -> Result<(), String> {
    let mut covered = legacy.clone();
    let mut seen = BTreeSet::new();
    for contract in contracts {
        if !seen.insert(contract.symbol) {
            return Err(format!("duplicate contract: {}", contract.symbol));
        }
        let Some(surface) = current.get(contract.symbol) else {
            let stem = contract.symbol.split('@').next().unwrap();
            let candidates: Vec<_> = current.keys().filter(|id| id.starts_with(stem)).collect();
            return Err(format!(
                "stale contract/signature: {}; current candidates: {candidates:?}",
                contract.symbol
            ));
        };
        check_registration(surface, contract)?;
        covered.insert(contract.symbol.into());
    }
    let missing: Vec<_> = current
        .keys()
        .filter(|key| !covered.contains(*key))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "new/changed interfaces require valid + invalid executable contracts (do not refresh the legacy inventory):\n{}",
            missing
                .into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

fn verify_policy_domains(current: &BTreeMap<String, Surface>) -> Result<(), String> {
    for domain in cases::DOMAINS {
        (domain.check_values)();
        let prefix = format!("core/format.rs::format::{}::variant:", domain.name);
        let actual: BTreeSet<_> = current
            .iter()
            .filter(|(id, _)| id.starts_with(&prefix))
            .map(|(_, surface)| surface.reference.as_str())
            .collect();
        let declared: BTreeSet<_> = domain.variants.iter().copied().collect();
        if actual != declared || actual.is_empty() || declared.len() != domain.variants.len() {
            return Err(format!(
                "{}: executable policy cases differ from source variants: source={actual:?}, cases={declared:?}",
                domain.name
            ));
        }
    }
    Ok(())
}

fn execute(contracts: &[Contract]) -> TestResult {
    for contract in contracts {
        (contract.valid)().map_err(|e| format!("{} valid: {e}", contract.symbol))?;
        (contract.invalid)().map_err(|e| format!("{} invalid: {e}", contract.symbol))?;
    }
    Ok(())
}

#[test]
fn every_new_interface_has_executed_validation_contracts() -> TestResult {
    let current = inventory();
    if let Some(output) = std::env::var_os("VARLIT_API_CATALOGUE_OUT") {
        std::fs::write(
            output,
            serde_json::to_vec_pretty(&current.keys().collect::<Vec<_>>())?,
        )?;
    }
    let snapshot = include_str!("support/validation_legacy.txt");
    let lines: Vec<_> = snapshot
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .collect();
    assert_eq!(
        source_audit::fingerprint(&(lines.join("\n") + "\n")),
        "ce0dd9b2f5f3ee70",
        "the legacy inventory is frozen; register executable contracts instead of refreshing it"
    );
    let legacy: BTreeSet<_> = lines.into_iter().map(str::to_owned).collect();
    verify_policy_domains(&current)?;
    missing_contracts(&current, &legacy, CONTRACTS)?;
    execute(CONTRACTS)
}

fn fixture(source: &str) -> BTreeMap<String, Surface> {
    let mut audit = Audit {
        file: "probe.rs".into(),
        crate_name: "core".into(),
        inventory_only: true,
        ..Audit::default()
    };
    audit.visit_file(&syn::parse_file(source).unwrap());
    audit.inventory
}

#[test]
fn new_methods_fields_variants_and_signature_changes_require_contracts() {
    let old = "pub struct Reader { pub policy: bool } pub enum Mode { A } impl Reader { pub fn get(&self) {} }";
    let legacy = fixture(old).into_keys().collect();
    for new in [
        format!("{old} impl Reader {{ pub fn fetch_anything(&self) {{}} }}"),
        old.replace("pub policy: bool", "pub policy: bool, pub new_policy: u8"),
        old.replace("Mode { A }", "Mode { A, B }"),
        old.replace("fn get(&self)", "fn get(&self, offset: u64)"),
        format!("{old} pub mod new_module {{ pub fn load() {{}} }}"),
        format!("{old} pub trait Extension {{ type Value; const LIMIT: u64; }}"),
    ] {
        assert!(
            missing_contracts(&fixture(&new), &legacy, &[]).is_err(),
            "accepted {new}"
        );
    }
    // Ordinary implementation edits keep their existing regression obligations;
    // this specific gate inventories interfaces, not arbitrary semantics.
    let comments = format!("// New words are not interfaces\n{old}");
    assert!(missing_contracts(&fixture(&comments), &legacy, &[]).is_ok());
}

#[test]
fn stub_or_unrelated_callbacks_cannot_count_as_api_coverage() {
    let surface = Surface {
        signature: String::new(),
        reference: "read_block_at".into(),
    };
    let reference = CONTRACTS
        .iter()
        .find(|contract| contract.symbol.contains("::fn:read_block_at@"))
        .unwrap();
    for stub in [
        "{ Ok(()) }",
        "{ assert!(true); Ok(()) }",
        "{ let f = Reader::read_block_at; assert!(true); Ok(()) }",
        "{ let text = \"reader.read_block_at(); assert!(true)\"; Ok(()) }",
    ] {
        let mut contract = Contract {
            invalid_source: stub,
            ..*reference
        };
        assert!(check_registration(&surface, &contract).is_err());
        contract.invalid_source = reference.invalid_source;
        assert!(check_registration(&surface, &contract).is_ok());
    }
}

#[test]
fn registered_callbacks_are_executed_and_their_failure_is_fatal() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let contracts = [Contract {
        symbol: "fixture",
        exercise: None,
        valid_source: "{}",
        invalid_source: "{}",
        valid: || {
            CALLS.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
        invalid: || {
            CALLS.fetch_add(1, Ordering::Relaxed);
            Err("intentional contract failure".into())
        },
    }];
    assert!(
        execute(&contracts)
            .unwrap_err()
            .to_string()
            .contains("intentional contract failure")
    );
    assert_eq!(CALLS.load(Ordering::Relaxed), 2);
}

#[test]
fn unregistered_policy_variants_fail_even_for_non_exhaustive_enums() {
    let mut current = inventory();
    assert!(verify_policy_domains(&current).is_ok());
    current.insert(
        "core/format.rs::format::IntegrityVerification::variant:Future@probe".into(),
        Surface {
            signature: String::new(),
            reference: "Future".into(),
        },
    );
    assert!(verify_policy_domains(&current).is_err());
}

#[test]
fn case_generators_retain_every_registered_shape_and_parameter() {
    use varlit::*;
    let commits = cases::commits();
    for shape in cases::COMMIT_SHAPES {
        assert!(
            commits
                .iter()
                .any(|value| std::mem::discriminant(value) == std::mem::discriminant(shape))
        );
    }
    for &mode in cases::TRANSACTIONS {
        assert!(commits.contains(&CommitPolicy::TransactionMarker(mode)));
    }
    let compressed = cases::compression();
    for shape in cases::COMPRESSION_SHAPES {
        assert!(
            compressed
                .iter()
                .any(|value| std::mem::discriminant(value) == std::mem::discriminant(shape))
        );
    }
    for &algorithm in cases::ALGORITHMS {
        for &header in cases::HEADERS {
            for &level in cases::LEVELS {
                assert!(compressed.iter().any(|policy| matches!(policy,
                    CompressionPolicy::VariableBlocks(value) if value.algorithm == algorithm && value.header_mode == header && value.level == level)));
            }
        }
    }
}

// Dedicated current-thread refusal; other concurrently running contracts are untouched.
struct ContractAllocator;
thread_local! { static REFUSE_CONTRACT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
fn contract_refuses() -> bool {
    REFUSE_CONTRACT
        .try_with(std::cell::Cell::get)
        .unwrap_or(false)
}
// SAFETY: forwarding preserves System's layout/ownership; refusal returns null.
unsafe impl std::alloc::GlobalAlloc for ContractAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if contract_refuses() {
            std::ptr::null_mut()
        } else {
            unsafe { std::alloc::GlobalAlloc::alloc(&std::alloc::System, layout) }
        }
    }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        if contract_refuses() {
            std::ptr::null_mut()
        } else {
            unsafe { std::alloc::GlobalAlloc::alloc_zeroed(&std::alloc::System, layout) }
        }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        if contract_refuses() {
            std::ptr::null_mut()
        } else {
            unsafe { std::alloc::GlobalAlloc::realloc(&std::alloc::System, pointer, layout, size) }
        }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::GlobalAlloc::dealloc(&std::alloc::System, pointer, layout) }
    }
}
#[global_allocator]
static CONTRACT_ALLOCATOR: ContractAllocator = ContractAllocator;
fn refusing_allocations<T>(body: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            REFUSE_CONTRACT.with(|state| state.set(false));
        }
    }
    REFUSE_CONTRACT.with(|state| state.set(true));
    let _reset = Reset;
    body()
}
