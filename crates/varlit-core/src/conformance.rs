//! Explicit, read-only checks for a particular file and its declared registry.
//! This module is not used by ordinary read/write entry points.
use std::path::Path;

use crate::{
    BlockKind, DiagnosticDomain, Error, FormatSpec, IntegrityPolicy, RecordIndexEntry, Result,
    VarlitBlock, VarlitFile, VarlitKeyedBlock,
};

/// Outcome of one required conformance check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileCheckStatus {
    Passed,
    Violated,
    Unverified,
    NotApplicable,
}

/// A stable condition identifier, its location, and any underlying refusal.
#[derive(Debug)]
pub struct FileCheck {
    pub code: &'static str,
    pub status: FileCheckStatus,
    pub block_id: Option<u32>,
    pub offset: Option<u64>,
    pub records_checked: u64,
    pub message: &'static str,
    pub error: Option<Error>,
}

impl FileCheck {
    fn pending(code: &'static str, block_id: Option<u32>) -> Self {
        Self {
            code,
            status: FileCheckStatus::Unverified,
            block_id,
            offset: None,
            records_checked: 0,
            message: "check did not complete",
            error: None,
        }
    }

    fn fail(&mut self, error: Error) {
        self.status = match (&error, crate::classify_error(&error)) {
            (
                Error::LimitExceeded { .. }
                | Error::MissingResourceLimit { .. }
                | Error::DecompressedLengthLimitExceeded { .. },
                _,
            ) => FileCheckStatus::Unverified,
            (_, DiagnosticDomain::Environment | DiagnosticDomain::FeatureGate) => {
                FileCheckStatus::Unverified
            }
            _ => FileCheckStatus::Violated,
        };
        self.message = "see the underlying error; this condition did not pass";
        self.error = Some(error);
    }

    fn pass(&mut self) {
        self.status = FileCheckStatus::Passed;
        self.message = "check completed";
    }
}

/// Results for one inspection, under [`Self::preconditions`].
///
/// A pass is not a formal proof, a certificate for future file mutations, or
/// a proof of a custom codec on inputs that were not inspected. No check is
/// installed in the application's ordinary I/O path.
#[derive(Debug)]
pub struct FileCheckReport {
    checks: Vec<FileCheck>,
    preconditions: &'static [&'static str],
}

impl FileCheckReport {
    pub fn checks(&self) -> &[FileCheck] {
        &self.checks
    }

    pub fn preconditions(&self) -> &'static [&'static str] {
        self.preconditions
    }

    /// Every required check must finish; missing coverage is not a pass.
    pub fn passed(&self) -> bool {
        !self.checks.is_empty()
            && self.checks.iter().all(|c| {
                matches!(
                    c.status,
                    FileCheckStatus::Passed | FileCheckStatus::NotApplicable
                )
            })
    }
}

/// Typed dispatch emitted by `varlit_format!`. Its fields cannot be forged.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct RecordValidator {
    id: u32,
    definition: fn(FormatSpec) -> Result<()>,
    decode: fn(&VarlitFile, &RecordIndexEntry, &mut Vec<u8>) -> Result<()>,
    keyed_links: bool,
    disk_index: bool,
}

impl RecordValidator {
    pub const fn of<T: VarlitBlock>() -> Self {
        Self {
            id: T::ID,
            definition: crate::collections::ensure_registered_block::<T>,
            decode: decode_record::<T>,
            keyed_links: !T::IS_KEYED,
            disk_index: false,
        }
    }

    pub const fn keyed<T: VarlitKeyedBlock>() -> Self {
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        Self {
            id: T::ID,
            definition: crate::collections::ensure_registered_block::<T>,
            decode: decode_keyed_record::<T>,
            keyed_links: true,
            disk_index: false,
        }
    }

    /// Marks a companion requirement; it cannot be waived by the file checker.
    pub const fn requiring_disk_index(mut self) -> Self {
        self.disk_index = true;
        self
    }
}

fn decode_record<T: VarlitBlock>(
    file: &VarlitFile,
    entry: &RecordIndexEntry,
    scratch: &mut Vec<u8>,
) -> Result<()> {
    file.decode_block_into::<T>(entry, scratch).map(|_| ())
}

fn decode_keyed_record<T: VarlitKeyedBlock>(
    file: &VarlitFile,
    entry: &RecordIndexEntry,
    scratch: &mut Vec<u8>,
) -> Result<()> {
    let value = file.decode_block_into::<T>(entry, scratch)?;
    if let Some(offset) = entry.prev_same_key_offset {
        let current = value.key();
        drop(value);
        let previous = file.read_block_at::<T>(offset)?.key();
        if current != previous {
            return Err(Error::InvalidCanonicalEncoding(
                "keyed predecessor belongs to a different key",
            ));
        }
    }
    Ok(())
}

struct Registered<'a> {
    validator: &'a RecordValidator,
    check: usize,
}

/// Checks an existing file with a complete typed registry.
///
/// Generated formats supply `validators` automatically. Manual specs can use
/// [`RecordValidator::of`] / [`RecordValidator::keyed`]. Missing or duplicate
/// validators cannot pass. The file and its companion files must be quiescent
/// throughout the call. Custom codec/key callbacks must terminate and satisfy
/// their own contracts; this checker does not sandbox application code.
///
/// Matrix cell payloads and custom physical-layout payload mappings currently
/// report `Unverified`. Their bytes are not silently treated as typed records.
/// A declared block with no inspected records also reports `Unverified`: use
/// a fixture containing every block to qualify a whole format.
pub fn check_file<P: AsRef<Path>>(
    spec: FormatSpec,
    path: P,
    validators: &[RecordValidator],
) -> Result<FileCheckReport> {
    const BASE: usize = 9;
    let capacity =
        spec.blocks
            .len()
            .checked_add(BASE)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "file check conditions",
            })?;
    let mut checks = Vec::new();
    checks
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource: "file check conditions",
            requested: capacity.saturating_mul(size_of::<FileCheck>()) as u64,
        })?;
    for code in [
        "spec.valid",
        "file.structure",
        "file.integrity",
        "registry.complete",
        "schema.pinned",
        "file.writer_marker_absent",
        "file.manifest",
        "sidecar.coherence",
        "file.published_extent",
    ] {
        checks.push(FileCheck::pending(code, None));
    }
    for block in spec.blocks {
        checks.push(FileCheck::pending("block.typed_payloads", Some(block.id)));
    }
    let mut report = FileCheckReport {
        checks,
        preconditions: &[
            "The file and companion files remain unchanged throughout inspection.",
            "Schema and custom codec declarations describe the intended data interpretation.",
            "Callbacks terminate without aborting, violating Rust safety, or modifying the inspected file.",
            "The result applies only to this inspection; later mutations require another check.",
            "This inspection does not establish fsync success or future crash durability.",
        ],
    };
    if validators.iter().any(|v| v.disk_index) {
        report.checks[7].message = "the declared disk-index companion has not been checked";
    } else {
        report.checks[7].status = FileCheckStatus::NotApplicable;
        report.checks[7].message = "no disk-index companion is declared by this checker registry";
    }
    if let Err(e) = spec.validate() {
        report.checks[0].fail(e);
        return Ok(report);
    }
    let spec = spec.with_strict_read_validation().ordinary_read();
    report.checks[0].pass();
    if spec.schema_hash == 0 {
        report.checks[4].message = "schema pinning is disabled; use schema_hash: computed";
    } else if spec.schema_hash != spec.computed_schema_hash() {
        report.checks[4].message = "literal schema hash differs from computed schema identity";
    } else {
        report.checks[4].pass();
    }
    match VarlitFile::inspect_writer_lock(&path) {
        Ok(None) => report.checks[5].pass(),
        Ok(Some(_)) => {
            report.checks[5].message =
                "a writer marker exists; close writers and resolve stale markers before inspection";
            return Ok(report);
        }
        Err(e) => {
            report.checks[5].fail(e);
            return Ok(report);
        }
    }
    if !spec.layout.is_varlit_native_default() {
        match spec.inspect_layout_file(&path) {
            Ok(_) => report.checks[1].pass(),
            Err(e) => report.checks[1].fail(e),
        }
        report.checks[2].message = "custom physical-layout integrity needs its adapter contract";
        report.checks[3].message =
            "custom raw-region mapping has not been checked against typed blocks";
        for check in &mut report.checks[BASE..] {
            check.message = "custom physical-layout payload mapping has not been checked";
        }
        return Ok(report);
    }
    let mut registry = Vec::new();
    registry
        .try_reserve_exact(validators.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "file check registry",
            requested: validators.len().saturating_mul(size_of::<Registered<'_>>()) as u64,
        })?;
    for validator in validators {
        registry.push(Registered {
            validator,
            check: usize::MAX,
        });
    }
    registry.sort_unstable_by_key(|r| r.validator.id);
    if spec.blocks.is_empty()
        || registry
            .windows(2)
            .any(|r| r[0].validator.id == r[1].validator.id)
    {
        report.checks[3].message = "supply exactly one typed validator for every declared block";
        return Ok(report);
    }
    let mut complete = validators.len() == spec.blocks.len();
    for (i, block) in spec.blocks.iter().enumerate() {
        let Ok(at) = registry.binary_search_by_key(&block.id, |r| r.validator.id) else {
            report.checks[BASE + i].message = "declared block has no typed validator";
            complete = false;
            continue;
        };
        registry[at].check = BASE + i;
        if let Err(e) = (registry[at].validator.definition)(spec) {
            report.checks[BASE + i].fail(e);
            complete = false;
        }
    }
    if !complete {
        report.checks[3].message = "typed registry coverage or type identity is incomplete";
        return Ok(report);
    }
    report.checks[3].pass();
    let file = match VarlitFile::open_readonly(spec, path) {
        Ok(file) => file,
        Err(e) => {
            report.checks[1].fail(e);
            return Ok(report);
        }
    };
    let physical_len = match file
        .snapshot()
        .try_clone_file()
        .and_then(|f| Ok(f.metadata()?.len()))
    {
        Ok(len) => len,
        Err(e) => {
            report.checks[8].fail(e);
            return Ok(report);
        }
    };
    report.checks[1].pass();
    match file.schema_manifest() {
        Ok(Some(m))
            if m.format_version == spec.version
                && m.endian == spec.endian
                && m.schema_hash == spec.schema_hash =>
        {
            report.checks[6].pass()
        }
        Ok(Some(_)) => report.checks[6].fail(Error::InvalidCanonicalEncoding(
            "embedded manifest differs from supplied schema",
        )),
        Ok(None) if spec.manifest_policy == crate::ManifestPolicy::None => {
            report.checks[6].status = FileCheckStatus::NotApplicable;
            report.checks[6].message = "this format does not require an embedded manifest";
        }
        Ok(None) => report.checks[6].message = "required embedded manifest is missing",
        Err(e) => report.checks[6].fail(e),
    }
    if spec.integrity_policy == IntegrityPolicy::None {
        report.checks[2].status = FileCheckStatus::NotApplicable;
        report.checks[2].message = "this format stores no data CRC";
    } else {
        match file.verify_all() {
            Ok(_) => report.checks[2].pass(),
            Err(e) => report.checks[2].fail(e),
        }
    }
    let mut scratch = Vec::new();
    let mut inspected_end = 0u64;
    let entries = match file.record_entries() {
        Ok(entries) => entries,
        Err(e) => {
            report.checks[1].fail(e);
            return Ok(report);
        }
    };
    for entry in entries {
        match entry {
            Ok(entry) => {
                match entry.checked_physical_end() {
                    Ok(end) => inspected_end = inspected_end.max(end),
                    Err(e) => {
                        report.checks[8].fail(e);
                        return Ok(report);
                    }
                }
                check_entry(&file, &entry, &registry, &mut report, &mut scratch);
            }
            Err(e) => {
                report.checks[1].fail(e);
                return Ok(report);
            }
        }
    }
    for descriptor in spec.block_residency.iter().filter(|d| !d.resident) {
        let chain = match file.block_chain(descriptor.block_id) {
            Ok(chain) => chain,
            Err(e) => {
                report.checks[1].fail(e);
                return Ok(report);
            }
        };
        for entry in chain {
            match entry {
                Ok(entry) => {
                    match entry.checked_physical_end() {
                        Ok(end) => inspected_end = inspected_end.max(end),
                        Err(e) => {
                            report.checks[8].fail(e);
                            return Ok(report);
                        }
                    }
                    check_entry(&file, &entry, &registry, &mut report, &mut scratch);
                }
                Err(e) => {
                    report.checks[1].fail(e);
                    return Ok(report);
                }
            }
        }
    }
    if !spec.has_matrix_blocks() && inspected_end == physical_len {
        report.checks[8].pass();
    } else {
        report.checks[8].offset = Some(inspected_end);
        report.checks[8].message =
            "physical bytes beyond the inspected publication or matrix region remain unverified";
    }
    for (i, block) in spec.blocks.iter().enumerate() {
        let check = &mut report.checks[BASE + i];
        if block.kind == BlockKind::Matrix {
            check.status = FileCheckStatus::Unverified;
            check.message = "matrix cell payload coverage requires a matrix-specific check";
        } else if check.records_checked == 0 && check.error.is_none() {
            check.message = "no records of this declared type were inspected; add a fixture sample";
        }
    }
    Ok(report)
}

fn check_entry(
    file: &VarlitFile,
    entry: &RecordIndexEntry,
    registry: &[Registered<'_>],
    report: &mut FileCheckReport,
    scratch: &mut Vec<u8>,
) {
    let Ok(at) = registry.binary_search_by_key(&entry.block_id, |r| r.validator.id) else {
        if !crate::file::INTERNAL_BLOCK_IDS.contains(&entry.block_id) {
            report.checks[3].offset = Some(entry.record_offset);
            report.checks[3].fail(Error::UnregisteredBlock(entry.block_id));
        }
        return; // Internal records are handled by the native reader/CRC walk.
    };
    let registered = &registry[at];
    let check = &mut report.checks[registered.check];
    if check.error.is_some() {
        return;
    }
    check.offset = Some(entry.record_offset);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (registered.validator.decode)(file, entry, scratch)
    })) {
        Ok(Ok(())) => {
            check.records_checked += 1;
            if file.spec().index_policy.keyed_offset_chain && !registered.validator.keyed_links {
                check.message = "manual keyed block has no keyed predecessor checker";
            } else {
                check.pass();
            }
        }
        Ok(Err(e)) => check.fail(e),
        Err(_) => {
            check.status = FileCheckStatus::Unverified;
            check.message = "application codec/key callback panicked";
            check.error = Some(Error::InvalidCanonicalEncoding(
                "codec/key callback panicked during inspection",
            ));
        }
    }
}
