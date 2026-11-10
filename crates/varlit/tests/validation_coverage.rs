//! New source-level interfaces need executable valid/invalid contracts.
//! The frozen legacy inventory records the existing surface, not a claim of
//! complete historical coverage. New entries cannot join by refreshing it.
#![cfg(all(feature = "integrity", feature = "compression-zstd", feature = "mmap"))]

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
    let directory = tempfile::tempdir()?;
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
        .and_then(|reader| reader.read_block_at::<Probe>(offset)))
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
    let directory = tempfile::tempdir()?;
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
        invalid { let dir = tempfile::tempdir()?; let report = QualificationProbe::check_file(dir.path().join("missing"))?; assert!(!report.passed()); assert!(report.checks().iter().any(|c| c.status == varlit::FileCheckStatus::Unverified)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::FileCheckStatus::variant:Violated@fedc9a584729feb7",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); assert!(report.checks().iter().all(|c| c.status != varlit::FileCheckStatus::Violated)); Ok(()) },
        invalid { let report = qualification_probe(true)?; assert!(!report.passed()); assert!(report.checks().iter().any(|c| c.status == varlit::FileCheckStatus::Violated)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::RecordValidator::fn:keyed@80f13f289fc95211",
        valid { let dir = tempfile::tempdir()?; let r = varlit::__core::check_file(OwnershipIndex::spec(), dir.path().join("missing"), &[varlit::__core::RecordValidator::keyed::<OwnedEntry>()])?; assert!(r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) },
        invalid { let dir = tempfile::tempdir()?; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[varlit::__core::RecordValidator::keyed::<OwnedEntry>()])?; assert!(!r.passed()); assert!(!r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::RecordValidator::fn:of@dff8df83dcb8825e",
        valid { let dir = tempfile::tempdir()?; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[varlit::__core::RecordValidator::of::<Probe>()])?; assert!(r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) },
        invalid { let dir = tempfile::tempdir()?; let v = varlit::__core::RecordValidator::of::<Probe>(); let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[v, v])?; assert!(!r.passed()); assert!(!r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::RecordValidator::fn:requiring_disk_index@e957c756694053c1",
        valid { let dir = tempfile::tempdir()?; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[varlit::__core::RecordValidator::of::<Probe>().requiring_disk_index()])?; assert!(r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); assert!(r.checks().iter().any(|c| c.code == "sidecar.coherence" && c.status == varlit::FileCheckStatus::Unverified)); Ok(()) },
        invalid { let dir = tempfile::tempdir()?; let v = varlit::__core::RecordValidator::of::<Probe>().requiring_disk_index(); let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[v, v])?; assert!(!r.passed()); assert!(!r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::enum:FileCheckStatus@cbf29ce484222325",
        valid { let report = qualification_probe(false)?; assert!(report.passed()); assert!(report.checks().iter().all(|c| c.status != varlit::FileCheckStatus::Violated)); Ok(()) },
        invalid { let report = qualification_probe(true)?; assert!(!report.passed()); assert!(report.checks().iter().any(|c| c.status == varlit::FileCheckStatus::Violated)); Ok(()) }
    ),
    contract!("core/conformance.rs::conformance::fn:check_file@b1b57b61204f2e51",
        valid { let dir = tempfile::tempdir()?; let path = dir.path().join("ok"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 1 })?; w.sync()?; } let r = QualificationProbe::check_file_with_resource_limits(&path, varlit::ReadLimits::STANDARD)?; assert!(r.passed(), "{r:#?}"); let r = QualificationProbe::check_file(&path)?; assert!(r.passed()); let r = varlit::check_file(QualificationProbe::spec(), &path, &[varlit::__core::RecordValidator::of::<Probe>()])?; assert!(r.passed()); Ok(()) },
        invalid { let dir = tempfile::tempdir()?; let r = QualificationProbe::check_file(dir.path().join("missing"))?; assert!(!r.passed()); let r = QualificationProbe::check_file_with_resource_limits(dir.path().join("missing"), varlit::ReadLimits::STANDARD)?; assert!(!r.passed()); let r = varlit::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[])?; assert!(!r.passed()); Ok(()) }
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
        valid { let dir = tempfile::tempdir()?; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[varlit::__core::RecordValidator::of::<Probe>()])?; assert!(r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) },
        invalid { let dir = tempfile::tempdir()?; let v = varlit::__core::RecordValidator::of::<Probe>(); let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[v, v])?; assert!(!r.passed()); assert!(!r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("core/diagnostics.rs::diagnostics::FormatSelfTest::fn:run_strict@47cc67381b8bce09",
        valid { let dir = tempfile::tempdir()?; let r = QualificationProbe::self_test(dir.path().join("ok")).with_block(Probe { value: 1 }).cleanup(true).run_strict(); assert!(r.passed(), "{r:#?}"); Ok(()) },
        invalid { let dir = tempfile::tempdir()?; let r = QualificationProbe::self_test(dir.path().join("missing")).run_strict(); assert!(!r.passed()); assert!(!dir.path().join("missing").exists()); Ok(()) }
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
        valid { let dir = tempfile::tempdir()?; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[varlit::__core::RecordValidator::of::<Probe>()])?; assert!(r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) },
        invalid { let dir = tempfile::tempdir()?; let v = varlit::__core::RecordValidator::of::<Probe>(); let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[v, v])?; assert!(!r.passed()); assert!(!r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("core/lib.rs::::export:check_file@b50c368a3166740f",
        valid { let dir = tempfile::tempdir()?; let path = dir.path().join("ok"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 1 })?; w.sync()?; } let r = QualificationProbe::check_file_with_resource_limits(&path, varlit::ReadLimits::STANDARD)?; assert!(r.passed(), "{r:#?}"); let r = QualificationProbe::check_file(&path)?; assert!(r.passed()); let r = varlit::check_file(QualificationProbe::spec(), &path, &[varlit::__core::RecordValidator::of::<Probe>()])?; assert!(r.passed()); Ok(()) },
        invalid { let dir = tempfile::tempdir()?; let r = QualificationProbe::check_file(dir.path().join("missing"))?; assert!(!r.passed()); let r = QualificationProbe::check_file_with_resource_limits(dir.path().join("missing"), varlit::ReadLimits::STANDARD)?; assert!(!r.passed()); let r = varlit::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[])?; assert!(!r.passed()); Ok(()) }
    ),
    contract!("core/traits.rs::traits::VarlitBlock::const:__VARLIT_RECORD_VALIDATOR@9de51896de2574bb",
        valid { let dir = tempfile::tempdir()?; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[<Probe as varlit::VarlitBlock>::__VARLIT_RECORD_VALIDATOR])?; assert!(r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) },
        invalid { let dir = tempfile::tempdir()?; let v = <Probe as varlit::VarlitBlock>::__VARLIT_RECORD_VALIDATOR; let r = varlit::__core::check_file(QualificationProbe::spec(), dir.path().join("missing"), &[v, v])?; assert!(!r.passed()); assert!(!r.checks().iter().any(|c| c.code == "registry.complete" && c.status == varlit::FileCheckStatus::Passed)); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:expand_format@d153ebf947c54d34", generated "check_file",
        valid { let dir = tempfile::tempdir()?; let path = dir.path().join("ok"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 1 })?; w.sync()?; } let r = QualificationProbe::check_file_with_resource_limits(&path, varlit::ReadLimits::STANDARD)?; assert!(r.passed(), "{r:#?}"); let r = QualificationProbe::check_file(&path)?; assert!(r.passed()); Ok(()) },
        invalid { let dir = tempfile::tempdir()?; let r = QualificationProbe::check_file(dir.path().join("missing"))?; assert!(!r.passed()); let r = QualificationProbe::check_file_with_resource_limits(dir.path().join("missing"), varlit::ReadLimits::STANDARD)?; assert!(!r.passed()); Ok(()) }
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
    contract!("core/codec.rs::codec::macro:unsigned@c3911cf13072ec65", generated "decode_value",
        valid { let bytes=7u64.to_le_bytes(); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert_eq!(d.decode_value::<u64>()?,7); Ok(()) },
        invalid { let mut d=varlit::Decoder::new(&[0;7],varlit::Endian::Little); assert!(d.decode_value::<u64>().is_err()); Ok(()) }
    ),
    contract!("core/codec.rs::codec::macro:signed@2b2247a26cddb1a0", generated "decode_value",
        valid { let bytes=(-7i64).to_le_bytes(); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert_eq!(d.decode_value::<i64>()?,-7); Ok(()) },
        invalid { let mut d=varlit::Decoder::new(&[0;7],varlit::Endian::Little); assert!(d.decode_value::<i64>().is_err()); Ok(()) }
    ),
    contract!("core/codec.rs::codec::macro:tuple_codec@1c32925194107c29", generated "decode_value",
        valid { let bytes=varlit::encode_to_vec(&(7u64,8u64),varlit::Endian::Little)?; let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert_eq!(d.decode_value::<(u64,u64)>()?,(7,8)); Ok(()) },
        invalid { let bytes=7u64.to_le_bytes(); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert!(d.decode_value::<(u64,ReplacingCodec)>().is_err()); Ok(()) }
    ),
    contract!("core/codec.rs::codec::macro:vec_seq_codec@bdf64ac8b7fee908", generated "decode_value",
        valid { let bytes=varlit::encode_to_vec(&vec![7u64,8],varlit::Endian::Little)?; let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert_eq!(d.decode_value::<Vec<u64>>()?,vec![7,8]); Ok(()) },
        invalid { let bytes=u64::MAX.to_le_bytes(); let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert!(d.decode_value::<Vec<u64>>().is_err()); Ok(()) }
    ),
    contract!("macros/modules/block_derive.rs::block_derive::generator:expand_varlit_block@e94efcb3d4fc7112", generated "decode_value",
        valid { let bytes=varlit::encode_to_vec(&Probe{value:7},varlit::Endian::Little)?; let mut d=varlit::Decoder::new(&bytes,varlit::Endian::Little); assert_eq!(d.decode_value::<Probe>()?,Probe{value:7}); Ok(()) },
        invalid { let bytes=varlit::encode_to_vec(&Probe{value:7},varlit::Endian::Little)?; let mut d=varlit::Decoder::new(&bytes[..bytes.len()-1],varlit::Endian::Little); assert!(d.decode_value::<Probe>().is_err()); Ok(()) }
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
    contract!("core/file.rs::file::VarlitFile::fn:read_block_at@8e571d1498d213bf",
        valid {
            let (_directory, reader, offset) = probe(false)?;
            assert_eq!(reader.read_block_at::<Probe>(offset)?, Probe { value: 19 });
            Ok(())
        },
        invalid {
            let (_directory, reader, offset) = probe(true)?;
            assert!(matches!(reader.read_block_at::<Probe>(offset), Err(varlit::Error::ChecksumMismatch { .. })));
            Ok(())
        }
    ),
    contract!("core/format.rs::format::ReadLimits::const:STANDARD@7b34437e092b5a48",
        valid {
            let (directory, _reader, offset) = probe(false)?;
            let reader = ProbeFormat::spec().open_readonly_with_limits(
                directory.path().join("contract.varlit"), varlit::ReadLimits::STANDARD)?;
            assert_eq!(reader.read_block_at::<Probe>(offset)?, Probe { value: 19 });
            Ok(())
        },
        invalid {
            let (directory, _reader, offset) = probe(false)?;
            let result = ProbeFormat::spec().open_readonly_with_limits(
                directory.path().join("contract.varlit"),
                varlit::ReadLimits::STANDARD.with_max_record_payload_len(0))
                .and_then(|reader| reader.read_block_at::<Probe>(offset));
            assert!(matches!(result, Err(varlit::Error::LimitExceeded { limit: 0, .. })));
            Ok(())
        }
    ),
    contract!("core/scan_control.rs::scan_control::ScanCancellationToken::fn:try_new@d8624e1b9e8ccf6f",
        valid { let token = varlit::ScanCancellationToken::try_new()?; assert!(!token.is_cancelled()); token.cancel(); assert!(token.is_cancelled()); Ok(()) },
        invalid { let result = refusing_allocations(|| varlit::ScanCancellationToken::try_new().map(|token| token.is_cancelled())); assert!(matches!(result, Err(varlit::Error::Io(e)) if e.kind() == std::io::ErrorKind::OutOfMemory)); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:high_cardinality_api_tokens@61f1678165055f69", generated "disk_index_plan",
        valid { let plan = OwnershipIndex::disk_index_plan()?; assert_eq!(plan.descriptors().len(), 1); assert!(plan.validate(OwnershipIndex::spec()).is_ok()); Ok(()) },
        invalid { let plan = OwnershipIndex::disk_index_plan()?; let error = plan.validate(ProbeFormat::spec()).unwrap_err(); assert!(matches!(error, varlit::DiskIndexError::PlanBlockMissing { .. })); let error = refusing_allocations(|| varlit::Error::from(error)); assert!(matches!(error, varlit::Error::AllocationFailed { .. })); Ok(()) }
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
        valid { let dir = tempfile::tempdir()?; let path = dir.path().join("header"); let mut writer = OwnershipHeader::create_layout_writer_with_typed_header(&path, OwnershipHeaderHeaderLayoutFields { kind: 42 })?; writer.flush()?; assert_eq!(&std::fs::read(path)?[..6], b"OWHD\x2a\x00"); Ok(()) },
        invalid { let dir = tempfile::tempdir()?; let path = dir.path().join("header"); let result = refusing_allocations(|| OwnershipHeader::create_layout_writer_with_typed_header(&path, OwnershipHeaderHeaderLayoutFields { kind: 42 })); assert!(matches!(result, Err(varlit::Error::AllocationFailed { .. }))); assert!(!path.exists()); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:layout_writer_segment_method_tokens@6d7f0cdfe27d767e", generated "write_data",
        valid { let dir = tempfile::tempdir()?; let path = dir.path().join("segment"); let mut writer = OwnershipHeader::create_layout_writer_with_typed_header(&path, OwnershipHeaderHeaderLayoutFields { kind: 42 })?; let info = writer.write_data(OwnershipHeaderDataLayoutWrite { fields: OwnershipHeaderDataLayoutFields { kind: 7 }, footer_fields: OwnershipHeaderDataLayoutFooterFields, metadata: b"", raw: b"data" })?; assert_eq!(info.kind()?, 7); Ok(()) },
        invalid { let dir = tempfile::tempdir()?; let path = dir.path().join("segment"); let mut writer = OwnershipHeader::create_layout_writer_with_typed_header(&path, OwnershipHeaderHeaderLayoutFields { kind: 42 })?; writer.flush()?; let before = std::fs::read(&path)?; let result = refusing_allocations(|| writer.write_data(OwnershipHeaderDataLayoutWrite { fields: OwnershipHeaderDataLayoutFields { kind: 7 }, footer_fields: OwnershipHeaderDataLayoutFooterFields, metadata: b"", raw: b"data" })); assert!(matches!(result, Err(varlit::Error::AllocationFailed { .. }))); writer.flush()?; assert_eq!(std::fs::read(path)?, before); Ok(()) }
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
];

fn lazy_open_fixture(
    summary: bool,
) -> Result<(tempfile::TempDir, std::path::PathBuf, varlit::FormatSpec), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
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
