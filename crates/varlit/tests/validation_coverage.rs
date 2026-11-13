//! New source-level interfaces need executable valid/invalid contracts.
//! The frozen legacy inventory records the existing surface, not a claim of
//! complete historical coverage. New entries cannot join by refreshing it.
#![cfg(all(feature = "integrity", feature = "compression-zstd", feature = "mmap"))]
#[path = "support/pages.rs"]
mod pages;

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
    pub format PagedDsl {
        magic: b"PGDSL"; version: 1;
        blocks { fixed Tally(id = 72) { value: u64, } }
    }
}

/// A `PagedDsl` file of `count` tallies, for the generated paged accessors.
fn paged_dsl(
    count: u64,
) -> Result<(tempfile::TempDir, std::path::PathBuf), Box<dyn std::error::Error>> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("paged.vdsl");
    let mut writer = PagedDsl::create_writer(&path)?;
    for value in 0..count {
        writer.push_tally(&Tally { value })?;
    }
    writer.flush()?;
    drop(writer);
    Ok((directory, path))
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

varlit::varlit_format! {
    pub format IndexedGridMatrix {
        magic: b"VIDXGRID"; version: 1; endian: little;
        limits {
            file_len: 8_589_934_592; records: 4_000_000; index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592; record_payload: 67_108_864; logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824; segments: 4_000_000; matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000; matrix_bitmap: 64_000_000; matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456; matrix_slot_region: 8_589_934_592; sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks {
            matrix IdxCell(id = 75, dims = [row, col], category = data) { value: u64, }
            fixed IdxEntry(id = 76, key = [id], key_index = disk) { id: u64, value: u64 }
        }
    }
}

varlit::varlit_format! {
    pub format IndexedNoMatrix {
        magic: b"VIDXNOMX"; version: 1; endian: little;
        limits {
            file_len: 8_589_934_592; records: 4_000_000; index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592; record_payload: 67_108_864; logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824; segments: 4_000_000; sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        blocks { fixed NoMxEntry(id = 77, key = [id], key_index = disk) { id: u64, value: u64 } }
    }
}

fn idx_plan(spec: varlit::FormatSpec) -> varlit::Result<varlit::DiskIndexPlan> {
    const BLOCKS: &[varlit::DiskIndexedBlock] = &[varlit::DiskIndexedBlock::of::<IdxEntry>()];
    Ok(varlit::DiskIndexPlan::canonical(spec, BLOCKS)?)
}

fn idx_dims() -> varlit::MatrixDimensions {
    varlit::MatrixDimensions::from_pairs([("row", 2), ("col", 2)])
}

/// An indexed primary with a 2x2 matrix: every cell committed (`full`, (r, c)
/// holds 10 * r + c + 1) or only (0, 0) (holding 1).
fn indexed_grid_fixture(
    full: bool,
) -> Result<(tempfile::TempDir, std::path::PathBuf), Box<dyn std::error::Error>> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("indexed-grid.varlit");
    let spec = IndexedGridMatrix::spec();
    let mut writer = varlit::VarlitIndexedWriter::create_with_dims(
        spec,
        &path,
        varlit::DiskIndexOptions::default(),
        idx_plan(spec)?,
        idx_dims(),
    )?;
    for row in 0..2u64 {
        for col in 0..2u64 {
            if !full && (row, col) != (0, 0) {
                continue;
            }
            let key = varlit::MatrixKey::new(row, col);
            writer.write_matrix_cell(
                key,
                &IdxCell {
                    value: 10 * row + col + 1,
                },
            )?;
            writer.commit_matrix_cell::<IdxCell>(key)?;
        }
    }
    writer.push_info(&IdxEntry { id: 1, value: 2 })?;
    writer.sync()?;
    drop(writer);
    Ok((directory, path))
}

fn idx_reader(path: &std::path::Path) -> varlit::Result<varlit::VarlitIndexedReader> {
    let spec = IndexedGridMatrix::spec();
    varlit::VarlitIndexedReader::open(
        spec,
        path,
        varlit::DiskIndexOptions::default(),
        idx_plan(spec)?,
    )
}

fn idx_writer(path: &std::path::Path) -> varlit::Result<varlit::VarlitIndexedWriter> {
    let spec = IndexedGridMatrix::spec();
    varlit::VarlitIndexedWriter::open(
        spec,
        path,
        varlit::DiskIndexOptions::default(),
        idx_plan(spec)?,
    )
}

varlit::varlit_format! {
    pub format ChunkFmt {
        magic: b"VCHUNKFX"; version: 1; endian: little; integrity: crc32_with_header;
        blocks { variable ChunkBlob(id = 78, key = [id], key_index = disk) { id: u64, bytes: Vec<u8>, } }
    }
}

/// A file with one chunked record (id 1, 200 000 bytes) and one short one
/// (id 2, no bytes), as native and as indexed primaries.
fn chunk_fixture()
-> Result<(tempfile::TempDir, std::path::PathBuf, std::path::PathBuf), Box<dyn std::error::Error>> {
    let directory = private_tempdir::private_tempdir()?;
    let native = directory.path().join("chunks.varlit");
    let indexed = directory.path().join("chunks-indexed.varlit");
    let big = ChunkBlob {
        id: 1,
        bytes: (0..200_000u32).map(|i| (i % 251) as u8).collect(),
    };
    let small = ChunkBlob {
        id: 2,
        bytes: Vec::new(),
    };
    let mut writer = ChunkFmt::spec().create_writer(&native)?;
    writer.push_keyed(&big)?;
    writer.push_keyed(&small)?;
    writer.sync()?;
    drop(writer);
    let mut writer =
        ChunkFmt::create_indexed_writer(&indexed, varlit::DiskIndexOptions::default())?;
    writer.push_chunk_blob(&big)?;
    writer.push_chunk_blob(&small)?;
    writer.sync()?;
    drop(writer);
    Ok((directory, native, indexed))
}

/// The fixture's entries: the chunked record first, then the short one.
fn chunk_entries(
    path: &std::path::Path,
) -> Result<Vec<varlit::RecordIndexEntry>, Box<dyn std::error::Error>> {
    let reader = ChunkFmt::spec().open_reader(path)?;
    let mut entries = Vec::new();
    reader.block_entries_into::<ChunkBlob>(&mut entries)?;
    Ok(entries)
}

fn chunk_indexed(path: &std::path::Path) -> varlit::Result<varlit::VarlitIndexedReader> {
    const BLOCKS: &[varlit::DiskIndexedBlock] = &[varlit::DiskIndexedBlock::of::<ChunkBlob>()];
    varlit::VarlitIndexedReader::open(
        ChunkFmt::spec(),
        path,
        varlit::DiskIndexOptions::default(),
        varlit::DiskIndexPlan::canonical(ChunkFmt::spec(), BLOCKS)?,
    )
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

/// A 2x2 matrix with every cell committed: (r, c) holds 10 * r + c + 1.
fn full_grid_fixture() -> Result<(tempfile::TempDir, std::path::PathBuf), Box<dyn std::error::Error>>
{
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("full-grid.varlit");
    let mut writer = GridMatrix::create_writer_with_dims(&path, GridMatrixDims { row: 2, col: 2 })?;
    for row in 0..2u64 {
        for col in 0..2u64 {
            let key = GridCellKey { row, col };
            writer.write_grid_cell(
                key,
                &GridCell {
                    value: 10 * row + col + 1,
                },
            )?;
            writer.commit_grid_cell(key)?;
        }
    }
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

/// Syncs one `IndexedEntry` per round under `options` and returns how many
/// sidecar confirmations a reader observed, checking that every synced entry
/// is readable whether or not it was confirmed.
fn confirmations(
    options: varlit::DiskIndexOptions,
    rounds: u64,
) -> Result<u64, Box<dyn std::error::Error>> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("confirm.varlit");
    let mut writer = IntoIndex::create_indexed_writer(&path, options)?;
    let reader = IntoIndex::open_indexed_reader(&path, options)?;
    let before = reader.snapshot_status()?.confirmed_generation;
    for round in 0..rounds {
        writer.push_indexed_entry(&IndexedEntry {
            id: round,
            value: round,
        })?;
        writer.sync()?;
    }
    let after = reader.snapshot_status()?.confirmed_generation;
    let fresh = IntoIndex::open_indexed_reader(&path, options)?;
    let mut out = <IndexedEntry as varlit::VarlitEmpty>::empty();
    assert!(fresh.get_indexed_entry(&(rounds - 1), &mut out)?);
    assert_eq!(out.value, rounds - 1);
    drop(writer);
    Ok(after - before)
}

/// A reader holds the first generation while the writer confirms `rounds`
/// syncs of rewrites under `reclaim_after`; returns its first lookup failure.
fn stale_lookup(
    reclaim_after: u64,
    rounds: u64,
) -> Result<Option<varlit::Error>, Box<dyn std::error::Error>> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("reclaim.varlit");
    let options = varlit::DiskIndexOptions {
        confirm: varlit::DiskIndexConfirmPolicy::EVERY_SYNC,
        reclaim_after,
        ..varlit::DiskIndexOptions::default()
    };
    let mut writer = IntoIndex::create_indexed_writer(&path, options)?;
    for id in 0..2_000 {
        writer.push_indexed_entry(&IndexedEntry { id, value: 0 })?;
    }
    writer.sync()?;
    let stale = IntoIndex::open_indexed_reader(&path, options)?;
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    for round in 1..=rounds {
        for _ in 0..40 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            writer.push_indexed_entry(&IndexedEntry {
                id: seed % 2_000,
                value: round,
            })?;
        }
        writer.sync()?;
    }
    let mut out = <IndexedEntry as varlit::VarlitEmpty>::empty();
    for id in 0..2_000 {
        if let Err(error) = stale.get_indexed_entry(&id, &mut out) {
            return Ok(Some(error));
        }
    }
    Ok(None)
}

varlit::varlit_format! {
    struct ResumeProbe {
        magic: b"VRESUMEP"; version: 1; endian: little; integrity: crc32_with_header;
        index: header_tails; commit: transaction_marker(on_flush); blocks: [Probe];
    }
}

/// A file of three `Probe` records, its first record offset and its end.
fn resume_probe()
-> Result<(tempfile::TempDir, std::path::PathBuf, u64, u64), Box<dyn std::error::Error>> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("resume.varlit");
    let mut writer = ResumeProbe::spec().create(&path)?;
    for value in 0..3 {
        writer.push(&Probe { value })?;
    }
    writer.flush()?;
    drop(writer);
    let file = varlit::VarlitFile::open_readonly_lazy(ResumeProbe::spec(), &path)?;
    let mut buffer = Vec::new();
    let start = file.record_map(&mut buffer)?.resume_offset();
    let mut map = file.record_map(&mut buffer)?;
    map.fill()?;
    let end = map.resume_offset();
    Ok((directory, path, start, end))
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

varlit::varlit_format! {
    struct TailProbe {
        magic: b"VTAILPRB"; version: 1; endian: little; schema_hash: computed;
        integrity: crc32; index: header_tails; commit: transaction_marker(on_flush);
        blocks: [Probe];
    }
}

/// An 8 KiB spacing with 256-byte anchors, so a few thousand probes cross
/// many anchors.
const SMALL_ANCHORS: varlit::AnchorPolicy = varlit::AnchorPolicy::every(13, true);

/// `records` probes written under `policy`, closed.
fn anchored_probe(
    policy: varlit::AnchorPolicy,
    records: u64,
) -> Result<(tempfile::TempDir, std::path::PathBuf), Box<dyn std::error::Error>> {
    let directory = private_tempdir::private_tempdir()?;
    let path = directory.path().join("anchored.varlit");
    let mut file = ProbeFormat::spec().with_anchors(policy).create(&path)?;
    for value in 0..records {
        file.push(&Probe { value })?;
    }
    file.flush()?;
    drop(file);
    Ok((directory, path))
}

/// Damages every anchor the file holds whole.
fn damage_all_anchors(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let layout = ProbeFormat::spec()
        .open_readonly(path)?
        .anchor_layout()
        .ok_or("an anchored file")?;
    let physical = std::fs::metadata(path)?.len();
    let mut k = 1;
    while layout.anchor_physical(k).ok_or("an anchor")? + layout.hole() <= physical {
        damage_anchor(path, k)?;
        k += 1;
    }
    assert!(k > 2, "the fixture crossed too few anchors");
    Ok(())
}

/// Flips a byte inside anchor `k`'s reserved range.
fn damage_anchor(path: &std::path::Path, k: u64) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{Seek, SeekFrom, Write};
    let layout = ProbeFormat::spec()
        .open_readonly(path)?
        .anchor_layout()
        .ok_or("an anchored file")?;
    let at = layout.anchor_physical(k).ok_or("an anchor")? + 16;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    file.seek(SeekFrom::Start(at))?;
    let mut byte = [0u8; 1];
    std::io::Read::read_exact(&mut file, &mut byte)?;
    file.seek(SeekFrom::Start(at))?;
    file.write_all(&[byte[0] ^ 0xff])?;
    Ok(())
}

/// A probe file of `records` records and the page/cursor helpers its paged
/// contracts share.
fn paged_probe(
    records: u64,
) -> Result<(tempfile::TempDir, std::path::PathBuf), Box<dyn std::error::Error>> {
    anchored_probe(varlit::AnchorPolicy::NONE, records)
}

/// A cursor past the end of any 20-record probe: taken from a much longer one.
fn buf_of(n: usize) -> Vec<varlit::RecordIndexEntry> {
    vec![varlit::RecordIndexEntry::default(); n]
}

fn foreign_cursor() -> Result<varlit::PageCursor, Box<dyn std::error::Error>> {
    let (_d, path) = paged_probe(400)?;
    let file = ProbeFormat::spec().open_readonly(&path)?;
    let mut buf = vec![varlit::RecordIndexEntry::default(); 300];
    Ok(file
        .index_entries(&mut buf)?
        .next
        .ok_or("a long probe pages")?)
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
    contract!("macros/modules/format_emit.rs::format_emit::generator:expand_format@fa4db551a394b7d8", generated "check_file",
        valid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("ok"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 1 })?; w.sync()?; } let r = QualificationProbe::check_file_with_resource_limits(&path, varlit::ReadLimits::STANDARD)?; assert!(r.passed(), "{r:#?}"); let r = QualificationProbe::check_file(&path)?; assert!(r.passed()); let file = QualificationProbe::open_readonly(&path)?; let entry = crate::pages::all_entries!(file)[0].clone(); let mut stored = Vec::new(); file.read_payload_into(&entry, &mut stored)?; assert_eq!(QualificationProbe::read_payload(&entry, &path)?, stored); assert_eq!(QualificationProbe::read_payload_limited(&entry, &path, entry.payload_len)?, stored); let mut logical = Vec::new(); file.read_logical_payload_into(&entry, &mut logical)?; assert_eq!(QualificationProbe::read_logical_payload(&entry, &path)?, logical); assert_eq!(QualificationProbe::read_logical_payload_limited(&entry, &path, entry.payload_len, logical.len() as u64)?, logical); assert_eq!(QualificationProbe::read_logical_payload_with_resource_limits(&entry, &path, varlit::ReadLimits::STANDARD)?, logical); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let r = QualificationProbe::check_file(dir.path().join("missing"))?; assert!(!r.passed()); let r = QualificationProbe::check_file_with_resource_limits(dir.path().join("missing"), varlit::ReadLimits::STANDARD)?; assert!(!r.passed()); let path = dir.path().join("ok"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 1 })?; w.sync()?; } let entry = crate::pages::all_entries!(QualificationProbe::open_readonly(&path)?)[0].clone(); assert!(entry.payload_len > 0); assert!(matches!(QualificationProbe::read_payload_limited(&entry, &path, entry.payload_len - 1), Err(varlit::Error::LimitExceeded { .. }))); assert!(matches!(QualificationProbe::read_logical_payload_limited(&entry, &path, entry.payload_len, 0), Err(varlit::Error::LimitExceeded { .. }))); assert!(QualificationProbe::read_payload(&entry, dir.path().join("missing")).is_err()); assert!(QualificationProbe::read_logical_payload(&entry, dir.path().join("missing")).is_err()); Ok(()) }
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
    contract!("macros/modules/block_derive.rs::block_derive::generator:expand_varlit_block@578b4426d69cbe12", generated "decode_value_into",
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
    contract!("core/format.rs::format::ReadLimits::const:STANDARD@c6d2cab306ba52f2",
        valid {
            // The resident directory and the keyed tails are finite by
            // default: an attacker-sized file cannot grow either without a
            // declared ceiling.
            let standard = varlit::ReadLimits::STANDARD;
            assert_eq!(standard.max_index_bytes, varlit::ReadLimit::Finite(1 << 30));
            assert_eq!(standard.max_keyed_tail_bytes, varlit::ReadLimit::Finite(256 << 20));
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
    contract!("macros/modules/format_emit.rs::format_emit::generator:high_cardinality_api_tokens@2ab09424b04a38fb", generated "get_indexed_entry",
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
        valid { let (_directory, reader, _offset) = probe(false)?; let entries = crate::pages::all_entries!(reader); let blocks = reader.with_directory(&entries)?.blocks::<Probe>()?; let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(blocks.get(0, &mut out)?); assert_eq!(out, Probe { value: 19 }); assert!(!blocks.get(1, &mut out)?); assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(true)?; let entries = crate::pages::all_entries!(reader); let blocks = reader.with_directory(&entries)?.blocks::<Probe>()?; let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(blocks.get(0, &mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/collections.rs::collections::BlockVec < T >::fn:get_into@ff6c17469d86c610",
        valid { let (_directory, reader, _offset) = probe(false)?; let entries = crate::pages::all_entries!(reader); let blocks = reader.with_directory(&entries)?.blocks::<Probe>()?; let (mut payload, mut out) = (Vec::new(), <Probe as varlit::VarlitEmpty>::empty()); assert!(blocks.get_into(0, &mut payload, &mut out)?); assert_eq!(out, Probe { value: 19 }); assert!(!blocks.get_into(1, &mut payload, &mut out)?); assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(true)?; let entries = crate::pages::all_entries!(reader); let blocks = reader.with_directory(&entries)?.blocks::<Probe>()?; let (mut payload, mut out) = (Vec::new(), <Probe as varlit::VarlitEmpty>::empty()); assert!(matches!(blocks.get_into(0, &mut payload, &mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/collections.rs::collections::BlockVec < T >::fn:cursor@36d86d7d49fea490",
        valid { let (_directory, reader, _offset) = probe(false)?; let entries = crate::pages::all_entries!(reader); let blocks = reader.with_directory(&entries)?.blocks::<Probe>()?; let mut cursor = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(cursor.next_into(&mut out)?); assert_eq!(out, Probe { value: 19 }); assert!(!cursor.next_into(&mut out)?); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(true)?; let entries = crate::pages::all_entries!(reader); let blocks = reader.with_directory(&entries)?.blocks::<Probe>()?; let mut cursor = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(cursor.next_into(&mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/collections.rs::collections::BlockCursor < '_ , T >::fn:next_into@3543e49de9ffc7d2",
        valid { let (_directory, reader, _offset) = probe(false)?; let entries = crate::pages::all_entries!(reader); let blocks = reader.with_directory(&entries)?.blocks::<Probe>()?; let mut cursor = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(cursor.next_into(&mut out)?); assert_eq!(out, Probe { value: 19 }); assert!(!cursor.next_into(&mut out)?); assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(true)?; let entries = crate::pages::all_entries!(reader); let blocks = reader.with_directory(&entries)?.blocks::<Probe>()?; let mut cursor = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(cursor.next_into(&mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/collections.rs::collections::struct:BlockCursor@4866f029fb7abaa7",
        valid { let (_directory, reader, _offset) = probe(false)?; let entries = crate::pages::all_entries!(reader); let blocks = reader.with_directory(&entries)?.blocks::<Probe>()?; let mut cursor: varlit::BlockCursor<'_, Probe> = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(cursor.next_into(&mut out)?); assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(true)?; let entries = crate::pages::all_entries!(reader); let blocks = reader.with_directory(&entries)?.blocks::<Probe>()?; let mut cursor: varlit::BlockCursor<'_, Probe> = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(cursor.next_into(&mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
    ),
    contract!("core/lib.rs::::export:BlockCursor@9ee06a732cea7753",
        valid { let (_directory, reader, _offset) = probe(false)?; let entries = crate::pages::all_entries!(reader); let blocks = reader.with_directory(&entries)?.blocks::<Probe>()?; let mut cursor: varlit::BlockCursor<'_, Probe> = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(cursor.next_into(&mut out)?); assert_eq!(out, Probe { value: 19 }); Ok(()) },
        invalid { let (_directory, reader, _offset) = probe(true)?; let entries = crate::pages::all_entries!(reader); let blocks = reader.with_directory(&entries)?.blocks::<Probe>()?; let mut cursor: varlit::BlockCursor<'_, Probe> = blocks.cursor(); let mut out = <Probe as varlit::VarlitEmpty>::empty(); assert!(matches!(cursor.next_into(&mut out), Err(varlit::Error::ChecksumMismatch { .. }))); Ok(()) }
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
        invalid { let (directory, _file, _offset) = probe(false)?; let path = directory.path().join("contract.varlit"); let file = varlit::VarlitFile::open_readonly_without_directory(ProbeFormat::spec(), &path, &mut Vec::new())?; assert_eq!(file.record_count()?, 1); { let mut writer = ProbeFormat::spec().open(&path)?; writer.push(&Probe { value: 7 })?; writer.flush()?; } assert_eq!(file.record_count()?, 1); assert_ne!(file.record_count()?, ProbeFormat::spec().open_readonly(&path)?.record_count()?); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:record_count@a4d70077dd9c37f9",
        valid { let (directory, _file, _offset) = probe(false)?; let reader = ProbeFormat::spec().open_reader(directory.path().join("contract.varlit"))?; assert_eq!(reader.record_count()?, 1); Ok(()) },
        invalid { let (directory, _file, _offset) = probe(false)?; let path = directory.path().join("contract.varlit"); let reader = varlit::VarlitReader::open_without_directory(ProbeFormat::spec(), &path, &mut Vec::new())?; assert_eq!(reader.record_count()?, 1); { let mut writer = ProbeFormat::spec().open(&path)?; writer.push(&Probe { value: 7 })?; writer.flush()?; } assert_eq!(reader.record_count()?, 1); assert_ne!(reader.record_count()?, ProbeFormat::spec().open_readonly(&path)?.record_count()?); Ok(()) }
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
    contract!("core/error.rs::error::Error::variant:MatrixSnapshotExpired@c6c8d6017c3c5e2e",
        valid { let (_directory, path) = grid_fixture()?; let spec = GridMatrix::spec().with_matrix_ring_bytes(80 * 1024); let _ = spec; let error = varlit::Error::MatrixSnapshotExpired { snapshot: 1, checkpointed: 2 }; assert!(error.to_string().contains("follow")); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let mut reader = GridMatrix::spec().open_reader(&path)?; reader.follow()?; let mut out = <GridCell as varlit::VarlitEmpty>::empty(); assert!(!matches!(reader.read_matrix_cell::<GridCell>(varlit::MatrixKey::new(0, 0), &mut out), Err(varlit::Error::MatrixSnapshotExpired { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::const:MATRIX_TILE_BLOCK_ID@b881bce266fa8769",
        valid { let (_directory, path) = grid_fixture()?; let file = GridMatrix::spec().open_readonly(&path)?; let mut out = vec![varlit::RecordIndexEntry::default(); 4]; let page = file.index_entries(&mut out)?; assert!(page.filled > 0); assert_eq!(out[0].block_id, varlit::MATRIX_TILE_BLOCK_ID); Ok(()) },
        invalid { assert_ne!(varlit::MATRIX_TILE_BLOCK_ID, varlit::MATRIX_CHUNK_BLOCK_ID); assert!(varlit::MATRIX_TILE_BLOCK_ID >= 0xFFFF_FF00); Ok(()) }
    ),
    contract!("core/lib.rs::::export:MATRIX_TILE_BLOCK_ID@672f089cc43cba8d",
        valid { assert_eq!(varlit::MATRIX_TILE_BLOCK_ID, 0xFFFF_FFF3); Ok(()) },
        invalid { assert_ne!(varlit::MATRIX_TILE_BLOCK_ID, varlit::INDEX_BLOCK_ID); Ok(()) }
    ),
    contract!("core/file/scan.rs::file::scan::macro:thread_local@30d8e54fefd68dbd", generated "records_framed",
        valid {
            #[cfg(feature = "scalable-fault-injection")]
            {
                let before = varlit::VarlitFile::records_framed();
                let (_directory, path) = grid_fixture()?;
                let _file = GridMatrix::spec().open_readonly(&path)?;
                assert!(varlit::VarlitFile::records_framed() > before);
            }
            #[cfg(not(feature = "scalable-fault-injection"))]
            assert!(true);
            Ok(())
        },
        invalid {
            #[cfg(feature = "scalable-fault-injection")]
            {
                let before = varlit::VarlitFile::records_framed();
                assert_eq!(varlit::VarlitFile::records_framed(), before);
            }
            #[cfg(not(feature = "scalable-fault-injection"))]
            assert!(true);
            Ok(())
        }
    ),
    contract!("core/format.rs::format::FormatSpec::field:matrix_ring_bytes@0504e5cb6c5993db",
        valid { let spec = GridMatrix::spec().with_matrix_ring_bytes(1 << 20); assert_eq!(spec.matrix_ring_bytes, Some(1 << 20)); Ok(()) },
        invalid { assert_eq!(GridMatrix::spec().matrix_ring_bytes, None); Ok(()) }
    ),
    contract!("core/format.rs::format::FormatSpec::fn:matrix_ring_bytes@3153345a828f3350",
        valid { assert_eq!(GridMatrix::spec().with_matrix_ring_bytes(80 * 1024).matrix_ring_bytes(), Some(80 * 1024)); Ok(()) },
        invalid { assert_eq!(GridMatrix::spec().matrix_ring_bytes(), None); Ok(()) }
    ),
    contract!("core/format.rs::format::FormatSpec::fn:with_matrix_ring_bytes@bb18ded6eafee17d",
        valid { let directory = private_tempdir::private_tempdir()?; let spec = GridMatrix::spec().with_matrix_ring_bytes(80 * 1024); let mut writer = spec.create_writer_with_dims(directory.path().join("ring"), varlit::MatrixDimensions::from_pairs([("row", 2), ("col", 2)]))?; writer.write_matrix_cell(varlit::MatrixKey::new(0, 0), &GridCell { value: 3 })?; writer.commit_matrix_cell::<GridCell>(varlit::MatrixKey::new(0, 0))?; writer.sync()?; assert!(std::fs::metadata(directory.path().join("ring"))?.len() > 80 * 1024); Ok(()) },
        invalid { let directory = private_tempdir::private_tempdir()?; let spec = GridMatrix::spec().with_matrix_ring_bytes(1 << 40).with_read_limits(GridMatrix::spec().read_limits.with_max_record_payload_len(1 << 20)); assert!(spec.create_writer_with_dims(directory.path().join("ring"), varlit::MatrixDimensions::from_pairs([("row", 2), ("col", 2)])).is_err()); Ok(()) }
    ),
    contract!("core/format.rs::format::MatrixBlockDescriptor::field:tile@c050a6f49669bcf3",
        valid { let block = GridMatrix::spec().matrix_blocks[0]; assert_eq!(block.tile, <GridCell as varlit::VarlitMatrixBlock>::TILE); Ok(()) },
        invalid { let mut block = GridMatrix::spec().matrix_blocks[0]; block.tile = [0, 0]; assert_eq!(block.effective_tile(), varlit::matrix_default_tile(block.slot_stride)); Ok(()) }
    ),
    contract!("core/format.rs::format::MatrixBlockDescriptor::fn:effective_tile@d71091fed53bd8ec",
        valid { let mut block = GridMatrix::spec().matrix_blocks[0]; block.tile = [2, 16]; assert_eq!(block.effective_tile(), [2, 16]); Ok(()) },
        invalid { let mut block = GridMatrix::spec().matrix_blocks[0]; block.tile = [0, 0]; assert_eq!(block.effective_tile(), varlit::matrix_default_tile(8)); Ok(()) }
    ),
    contract!("core/format.rs::format::const:MATRIX_DEFAULT_TILE_BYTES@b9a2af4bbecf07a5",
        valid { let [rows, cols] = varlit::matrix_default_tile(8); assert!(rows * cols * 12 <= varlit::MATRIX_DEFAULT_TILE_BYTES); Ok(()) },
        invalid { let [rows, cols] = varlit::matrix_default_tile(8); assert!(2 * rows * cols * 12 > varlit::MATRIX_DEFAULT_TILE_BYTES); Ok(()) }
    ),
    contract!("core/lib.rs::::export:MATRIX_DEFAULT_TILE_BYTES@28b761db8ba65074",
        valid { assert_eq!(varlit::MATRIX_DEFAULT_TILE_BYTES, 64 * 1024); Ok(()) },
        invalid { assert_ne!(varlit::MATRIX_DEFAULT_TILE_BYTES, 0); Ok(()) }
    ),
    contract!("core/format.rs::format::const:MATRIX_MAX_TILE_BYTES@3758a23cbceb72d3",
        valid { assert_eq!(varlit::MATRIX_MAX_TILE_BYTES, 64 << 20); Ok(()) },
        invalid { let spec = GridMatrix::spec(); let mut block = spec.matrix_blocks[0]; block.tile = [1 << 10, 1 << 10]; block.slot_stride = 4096; let blocks: &'static [varlit::MatrixBlockDescriptor] = Box::leak(Box::new([block])); assert!(block.tile[0] * block.tile[1] * (block.slot_stride + 4) > varlit::MATRIX_MAX_TILE_BYTES); assert!(spec.with_matrix_spec(spec.matrix_dimensions, spec.matrix_commits, blocks).validate().is_err()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:MATRIX_MAX_TILE_BYTES@a26404714d801561",
        valid { assert!(varlit::MATRIX_MAX_TILE_BYTES >= varlit::MATRIX_DEFAULT_TILE_BYTES); Ok(()) },
        invalid { assert_ne!(varlit::MATRIX_MAX_TILE_BYTES, 0); Ok(()) }
    ),
    contract!("core/format.rs::format::const:MATRIX_MAX_TILE_CELLS@0e019ae78f1f5728",
        valid { assert_eq!(varlit::MATRIX_MAX_TILE_CELLS, 1 << 20); Ok(()) },
        invalid { let [rows, cols] = varlit::matrix_default_tile(1); assert!(rows * cols <= varlit::MATRIX_MAX_TILE_CELLS); Ok(()) }
    ),
    contract!("core/lib.rs::::export:MATRIX_MAX_TILE_CELLS@bf6f68e4d7f4aad3",
        valid { assert!(varlit::MATRIX_MAX_TILE_CELLS > 0); Ok(()) },
        invalid { assert_ne!(varlit::MATRIX_MAX_TILE_CELLS, u64::MAX); Ok(()) }
    ),
    contract!("core/format.rs::format::fn:matrix_default_tile@0ae457ac2f48df49",
        valid { assert_eq!(varlit::matrix_default_tile(1024), [4, 8]); assert_eq!(varlit::matrix_default_tile(4), [64, 128]); Ok(()) },
        invalid { assert_eq!(varlit::matrix_default_tile(1 << 20), [1, 1]); Ok(()) }
    ),
    contract!("core/lib.rs::::export:matrix_default_tile@ffe6e4b444ef60e8",
        valid { assert_eq!(varlit::matrix_default_tile(8), <GridCell as varlit::VarlitMatrixBlock>::TILE); Ok(()) },
        invalid { assert_ne!(varlit::matrix_default_tile(8), [0, 0]); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::struct:MatrixStorageDiagnostics@cbf29ce484222325",
        valid { let (_directory, path) = grid_fixture()?; let file = GridMatrix::spec().open_readonly(&path)?; let diagnostics: varlit::MatrixStorageDiagnostics = file.matrix_storage_diagnostics().expect("a matrix file"); assert_eq!(diagnostics.sparse_file, Some(false)); Ok(()) },
        invalid { let directory = private_tempdir::private_tempdir()?; let file = ProbeFormat::spec().create(directory.path().join("plain"))?; let none: Option<varlit::MatrixStorageDiagnostics> = file.matrix_storage_diagnostics(); assert!(none.is_none()); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixStorageDiagnostics::field:sparse_file@df5b26aaaeca973a",
        valid { let (_directory, path) = grid_fixture()?; let file = GridMatrix::spec().open_readonly(&path)?; assert_eq!(file.matrix_storage_diagnostics().expect("a matrix file").sparse_file, Some(false)); Ok(()) },
        invalid { assert_eq!(varlit::MatrixStorageDiagnostics::default().sparse_file, None); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixStorageDiagnostics::field:allocation_map_available@cd2fd49bc6b014bd",
        valid { let (_directory, path) = grid_fixture()?; let file = GridMatrix::spec().open_readonly(&path)?; assert!(!file.matrix_storage_diagnostics().expect("a matrix file").allocation_map_available); Ok(()) },
        invalid { assert!(!varlit::MatrixStorageDiagnostics::default().allocation_map_available); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixStorageDiagnostics::field:hole_punch_api_available@cd2fd49bc6b014bd",
        valid { let (_directory, path) = grid_fixture()?; let file = GridMatrix::spec().open_readonly(&path)?; assert!(!file.matrix_storage_diagnostics().expect("a matrix file").hole_punch_api_available); Ok(()) },
        invalid { assert!(!varlit::MatrixStorageDiagnostics::default().hole_punch_api_available); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixStorageDiagnostics::field:sparse_mark_failed@cd2fd49bc6b014bd",
        valid { let (_directory, path) = grid_fixture()?; let file = GridMatrix::spec().open_readonly(&path)?; assert!(!file.matrix_storage_diagnostics().expect("a matrix file").sparse_mark_failed); Ok(()) },
        invalid { assert!(!varlit::MatrixStorageDiagnostics::default().sparse_mark_failed); Ok(()) }
    ),
    contract!("core/traits.rs::traits::VarlitMatrixBlock::const:TILE@3ef62d55ffd757ff",
        valid { assert_eq!(<GridCell as varlit::VarlitMatrixBlock>::TILE, varlit::matrix_default_tile(8)); Ok(()) },
        invalid { assert_ne!(<GridCell as varlit::VarlitMatrixBlock>::TILE, [0, 0]); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:inline_block_tokens@5aafd049dd8b1b82", generated "write_grid_cell",
        valid { let directory = private_tempdir::private_tempdir()?; let mut writer = GridMatrix::create_writer_with_dims(directory.path().join("g"), GridMatrixDims { row: 2, col: 2 })?; writer.write_grid_cell(GridCellKey { row: 1, col: 1 }, &GridCell { value: 5 })?; writer.commit_grid_cell(GridCellKey { row: 1, col: 1 })?; assert_eq!(writer.grid_cell_status(GridCellKey { row: 1, col: 1 })?, varlit::MatrixCellStatus::Committed); Ok(()) },
        invalid { let directory = private_tempdir::private_tempdir()?; let mut writer = GridMatrix::create_writer_with_dims(directory.path().join("g"), GridMatrixDims { row: 2, col: 2 })?; assert!(matches!(writer.write_grid_cell(GridCellKey { row: 2, col: 0 }, &GridCell { value: 5 }), Err(varlit::Error::MatrixKeyOutOfBounds { .. }))); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:matrix_spec_tokens@f37c7c67c1bc42df", generated "spec",
        valid { let spec = GridMatrix::spec(); assert_eq!(spec.matrix_blocks[0].effective_tile(), <GridCell as varlit::VarlitMatrixBlock>::TILE); spec.validate()?; Ok(()) },
        invalid { let spec = GridMatrix::spec(); assert_eq!(spec.matrix_blocks.len(), 1); assert!(spec.with_chunk_compression(varlit::VariableCompression::zstd(varlit::CompressionHeaderMode::RecordExplicit)).validate().is_err()); Ok(()) }
    ),
    contract!("macros/modules/format_parse.rs::format_parse::generator:parse_inline_block@f87283ae4de49669", generated "spec",
        valid { assert_eq!(GridMatrix::spec().matrix_blocks[0].category, "data"); Ok(()) },
        invalid { assert_ne!(GridMatrix::spec().matrix_blocks[0].tile, [0, 0]); Ok(()) }
    ),
    contract!("core/matrix_bulk.rs::file::matrix_bulk::VarlitFile::fn:read_matrix_region_into@4da15e3526589c4b",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_readonly(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 4]; let page = handle.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out)?; assert_eq!(page.filled, 4); assert_eq!(out[3].value, 12); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let handle = GridMatrix::spec().open_readonly(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 4]; assert!(matches!(handle.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/matrix_bulk.rs::file::matrix_bulk::VarlitFile::fn:read_matrix_region_from@c481feaf33f56499",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_readonly(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 1]; let page = handle.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::row(1, 0..2), &mut out)?; let page = handle.read_matrix_region_from::<GridCell>(page.next.expect("one more"), &mut out)?; assert_eq!((page.filled, page.next), (1, None)); assert_eq!(out[0].value, 12); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let handle = GridMatrix::spec().open_readonly(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 1]; let page = handle.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::row(0, 0..2), &mut out)?; assert!(matches!(handle.read_matrix_region_from::<GridCell>(page.next.expect("one more"), &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/matrix_bulk.rs::file::matrix_bulk::VarlitFile::fn:read_matrix_region_bytes_into@bdafe560b173747a",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_readonly(&path)?; let mut out = [0u8; 16]; let page = handle.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::column(1, 0..2), &mut out)?; assert_eq!(page.filled, 2); assert_eq!(u64::from_le_bytes(out[8..].try_into().unwrap()), 12); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_readonly(&path)?; let mut out = [0u8; 7]; assert!(handle.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::column(1, 0..2), &mut out).is_err()); Ok(()) }
    ),
    contract!("core/matrix_bulk.rs::file::matrix_bulk::VarlitFile::fn:read_matrix_region_bytes_from@4a865dcde776b98c",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_readonly(&path)?; let mut out = [0u8; 8]; let page = handle.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::column(0, 0..2), &mut out)?; let page = handle.read_matrix_region_bytes_from::<GridCell>(page.next.expect("one more"), &mut out)?; assert_eq!(page.filled, 1); assert_eq!(u64::from_le_bytes(out), 11); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_readonly(&path)?; let mut out = [0u8; 8]; let page = handle.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::column(0, 0..2), &mut out)?; let mut short = [0u8; 3]; assert!(handle.read_matrix_region_bytes_from::<GridCell>(page.next.expect("one more"), &mut short).is_err()); Ok(()) }
    ),
    contract!("core/matrix_bulk.rs::file::matrix_bulk::VarlitFile::fn:read_matrix_cells_into@8dfe438a869a865c",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_readonly(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 3]; handle.read_matrix_cells_into::<GridCell>(&[varlit::MatrixKey::new(1, 1), varlit::MatrixKey::new(0, 0), varlit::MatrixKey::new(1, 1)], &mut out)?; assert_eq!((out[0].value, out[1].value, out[2].value), (12, 1, 12)); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let handle = GridMatrix::spec().open_readonly(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 2]; assert!(matches!(handle.read_matrix_cells_into::<GridCell>(&[varlit::MatrixKey::new(0, 0), varlit::MatrixKey::new(1, 1)], &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/matrix_bulk.rs::file::matrix_bulk::VarlitFile::fn:read_matrix_cells_bytes_into@d762fe311ef72bc1",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_readonly(&path)?; let mut out = [0u8; 16]; handle.read_matrix_cells_bytes_into::<GridCell>(&[varlit::MatrixKey::new(1, 0), varlit::MatrixKey::new(0, 1)], &mut out)?; assert_eq!(u64::from_le_bytes(out[..8].try_into()?), 11); assert_eq!(u64::from_le_bytes(out[8..].try_into()?), 2); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_readonly(&path)?; let mut out = [0u8; 8]; assert!(matches!(handle.read_matrix_cells_bytes_into::<GridCell>(&[varlit::MatrixKey::new(0, 0), varlit::MatrixKey::new(1, 1)], &mut out), Err(varlit::Error::MatrixSizeMismatch { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:read_matrix_cells_into@8dfe438a869a865c",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_reader(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 3]; handle.read_matrix_cells_into::<GridCell>(&[varlit::MatrixKey::new(1, 1), varlit::MatrixKey::new(0, 0), varlit::MatrixKey::new(1, 1)], &mut out)?; assert_eq!((out[0].value, out[1].value, out[2].value), (12, 1, 12)); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let handle = GridMatrix::spec().open_reader(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 2]; assert!(matches!(handle.read_matrix_cells_into::<GridCell>(&[varlit::MatrixKey::new(0, 0), varlit::MatrixKey::new(1, 1)], &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:read_matrix_cells_bytes_into@d762fe311ef72bc1",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 16]; handle.read_matrix_cells_bytes_into::<GridCell>(&[varlit::MatrixKey::new(1, 0), varlit::MatrixKey::new(0, 1)], &mut out)?; assert_eq!(u64::from_le_bytes(out[..8].try_into()?), 11); assert_eq!(u64::from_le_bytes(out[8..].try_into()?), 2); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 8]; assert!(matches!(handle.read_matrix_cells_bytes_into::<GridCell>(&[varlit::MatrixKey::new(0, 0), varlit::MatrixKey::new(1, 1)], &mut out), Err(varlit::Error::MatrixSizeMismatch { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:read_matrix_cells_into@8dfe438a869a865c",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_writer(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 3]; handle.read_matrix_cells_into::<GridCell>(&[varlit::MatrixKey::new(1, 1), varlit::MatrixKey::new(0, 0), varlit::MatrixKey::new(1, 1)], &mut out)?; assert_eq!((out[0].value, out[1].value, out[2].value), (12, 1, 12)); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let handle = GridMatrix::spec().open_writer(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 2]; assert!(matches!(handle.read_matrix_cells_into::<GridCell>(&[varlit::MatrixKey::new(0, 0), varlit::MatrixKey::new(1, 1)], &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:read_matrix_cells_bytes_into@d762fe311ef72bc1",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_writer(&path)?; let mut out = [0u8; 16]; handle.read_matrix_cells_bytes_into::<GridCell>(&[varlit::MatrixKey::new(1, 0), varlit::MatrixKey::new(0, 1)], &mut out)?; assert_eq!(u64::from_le_bytes(out[..8].try_into()?), 11); assert_eq!(u64::from_le_bytes(out[8..].try_into()?), 2); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_writer(&path)?; let mut out = [0u8; 8]; assert!(matches!(handle.read_matrix_cells_bytes_into::<GridCell>(&[varlit::MatrixKey::new(0, 0), varlit::MatrixKey::new(1, 1)], &mut out), Err(varlit::Error::MatrixSizeMismatch { .. }))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedReader::fn:matrix_cell_status@6f3305a58db69ad7",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_reader(&path)?; assert_eq!(handle.matrix_cell_status::<IdxCell>(varlit::MatrixKey::new(1, 1))?, varlit::MatrixCellStatus::Committed); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_reader(&path)?; assert!(matches!(handle.matrix_cell_status::<IdxCell>(varlit::MatrixKey::new(2, 0)), Err(varlit::Error::MatrixKeyOutOfBounds { .. }))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedReader::fn:read_matrix_cell@b1223a9f317f6615",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_reader(&path)?; let mut out = <IdxCell as varlit::VarlitEmpty>::empty(); handle.read_matrix_cell::<IdxCell>(varlit::MatrixKey::new(1, 0), &mut out)?; assert_eq!(out.value, 11); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(false)?; let handle = idx_reader(&path)?; let mut out = <IdxCell as varlit::VarlitEmpty>::empty(); assert!(matches!(handle.read_matrix_cell::<IdxCell>(varlit::MatrixKey::new(1, 1), &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedReader::fn:read_matrix_cells_into@8dfe438a869a865c",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_reader(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 2]; handle.read_matrix_cells_into::<IdxCell>(&[varlit::MatrixKey::new(1, 1), varlit::MatrixKey::new(0, 1)], &mut out)?; assert_eq!((out[0].value, out[1].value), (12, 2)); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_reader(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 1]; assert!(matches!(handle.read_matrix_cells_into::<IdxCell>(&[varlit::MatrixKey::new(1, 1), varlit::MatrixKey::new(0, 1)], &mut out), Err(varlit::Error::MatrixSizeMismatch { .. }))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedReader::fn:read_matrix_region_into@4da15e3526589c4b",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_reader(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 4]; let page = handle.read_matrix_region_into::<IdxCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out)?; assert_eq!(page.filled, 4); assert_eq!(out[3].value, 12); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(false)?; let handle = idx_reader(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 4]; assert!(matches!(handle.read_matrix_region_into::<IdxCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedReader::fn:read_matrix_region_from@c481feaf33f56499",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_reader(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 1]; let page = handle.read_matrix_region_into::<IdxCell>(varlit::MatrixRegion::row(1, 0..2), &mut out)?; let page = handle.read_matrix_region_from::<IdxCell>(page.next.expect("one more"), &mut out)?; assert_eq!((page.filled, page.next), (1, None)); assert_eq!(out[0].value, 12); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(false)?; let handle = idx_reader(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 1]; let page = handle.read_matrix_region_into::<IdxCell>(varlit::MatrixRegion::row(0, 0..2), &mut out)?; assert!(matches!(handle.read_matrix_region_from::<IdxCell>(page.next.expect("one more"), &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedReader::fn:read_matrix_row_into@7986f0c60d7e4056",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_reader(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 2]; handle.read_matrix_row_into::<IdxCell>(1, &mut out)?; assert_eq!((out[0].value, out[1].value), (11, 12)); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_reader(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 3]; assert!(matches!(handle.read_matrix_row_into::<IdxCell>(1, &mut out), Err(varlit::Error::MatrixSizeMismatch { .. }))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedWriter::fn:matrix_cell_status@6f3305a58db69ad7",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_writer(&path)?; assert_eq!(handle.matrix_cell_status::<IdxCell>(varlit::MatrixKey::new(1, 1))?, varlit::MatrixCellStatus::Committed); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_writer(&path)?; assert!(matches!(handle.matrix_cell_status::<IdxCell>(varlit::MatrixKey::new(2, 0)), Err(varlit::Error::MatrixKeyOutOfBounds { .. }))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedWriter::fn:read_matrix_cell@b1223a9f317f6615",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_writer(&path)?; let mut out = <IdxCell as varlit::VarlitEmpty>::empty(); handle.read_matrix_cell::<IdxCell>(varlit::MatrixKey::new(1, 0), &mut out)?; assert_eq!(out.value, 11); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(false)?; let handle = idx_writer(&path)?; let mut out = <IdxCell as varlit::VarlitEmpty>::empty(); assert!(matches!(handle.read_matrix_cell::<IdxCell>(varlit::MatrixKey::new(1, 1), &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedWriter::fn:read_matrix_cells_into@8dfe438a869a865c",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_writer(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 2]; handle.read_matrix_cells_into::<IdxCell>(&[varlit::MatrixKey::new(1, 1), varlit::MatrixKey::new(0, 1)], &mut out)?; assert_eq!((out[0].value, out[1].value), (12, 2)); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_writer(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 1]; assert!(matches!(handle.read_matrix_cells_into::<IdxCell>(&[varlit::MatrixKey::new(1, 1), varlit::MatrixKey::new(0, 1)], &mut out), Err(varlit::Error::MatrixSizeMismatch { .. }))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedWriter::fn:read_matrix_region_into@4da15e3526589c4b",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_writer(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 4]; let page = handle.read_matrix_region_into::<IdxCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out)?; assert_eq!(page.filled, 4); assert_eq!(out[3].value, 12); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(false)?; let handle = idx_writer(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 4]; assert!(matches!(handle.read_matrix_region_into::<IdxCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedWriter::fn:read_matrix_region_from@c481feaf33f56499",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_writer(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 1]; let page = handle.read_matrix_region_into::<IdxCell>(varlit::MatrixRegion::row(1, 0..2), &mut out)?; let page = handle.read_matrix_region_from::<IdxCell>(page.next.expect("one more"), &mut out)?; assert_eq!((page.filled, page.next), (1, None)); assert_eq!(out[0].value, 12); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(false)?; let handle = idx_writer(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 1]; let page = handle.read_matrix_region_into::<IdxCell>(varlit::MatrixRegion::row(0, 0..2), &mut out)?; assert!(matches!(handle.read_matrix_region_from::<IdxCell>(page.next.expect("one more"), &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedWriter::fn:read_matrix_row_into@7986f0c60d7e4056",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_writer(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 2]; handle.read_matrix_row_into::<IdxCell>(1, &mut out)?; assert_eq!((out[0].value, out[1].value), (11, 12)); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_writer(&path)?; let mut out = vec![<IdxCell as varlit::VarlitEmpty>::empty(); 3]; assert!(matches!(handle.read_matrix_row_into::<IdxCell>(1, &mut out), Err(varlit::Error::MatrixSizeMismatch { .. }))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedReader::fn:matrix_generation@7b3a03dde266f443",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let handle = idx_reader(&path)?; assert!(handle.matrix_generation()? >= 1); Ok(()) },
        invalid { let directory = private_tempdir::private_tempdir()?; let path = directory.path().join("nomx.varlit"); let spec = IndexedNoMatrix::spec(); const BLOCKS: &[varlit::DiskIndexedBlock] = &[varlit::DiskIndexedBlock::of::<NoMxEntry>()]; let plan = varlit::DiskIndexPlan::canonical(spec, BLOCKS)?; drop(varlit::VarlitIndexedWriter::create(spec, &path, varlit::DiskIndexOptions::default(), plan)?); let handle = varlit::VarlitIndexedReader::open(spec, &path, varlit::DiskIndexOptions::default(), plan)?; assert!(matches!(handle.matrix_generation(), Err(varlit::Error::MatrixLayoutMissing))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedWriter::fn:write_matrix_cell@a97005569bd68189",
        valid { let (_d, path) = indexed_grid_fixture(false)?; let mut handle = idx_writer(&path)?; let key = varlit::MatrixKey::new(1, 1); handle.write_matrix_cell(key, &IdxCell { value: 9 })?; assert_eq!(handle.matrix_cell_status::<IdxCell>(key)?, varlit::MatrixCellStatus::NotCommitted); handle.commit_matrix_cell::<IdxCell>(key)?; let mut out = <IdxCell as varlit::VarlitEmpty>::empty(); handle.read_matrix_cell::<IdxCell>(key, &mut out)?; assert_eq!(out.value, 9); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(false)?; let mut handle = idx_writer(&path)?; assert!(matches!(handle.write_matrix_cell(varlit::MatrixKey::new(0, 2), &IdxCell { value: 9 }), Err(varlit::Error::MatrixKeyOutOfBounds { .. }))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedWriter::fn:commit_matrix_cell@519dad70962e62c4",
        valid { let (_d, path) = indexed_grid_fixture(false)?; let mut handle = idx_writer(&path)?; let key = varlit::MatrixKey::new(0, 1); handle.write_matrix_cell(key, &IdxCell { value: 4 })?; handle.commit_matrix_cell::<IdxCell>(key)?; handle.sync()?; drop(handle); let reader = idx_reader(&path)?; let mut out = <IdxCell as varlit::VarlitEmpty>::empty(); reader.read_matrix_cell::<IdxCell>(key, &mut out)?; assert_eq!(out.value, 4); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(false)?; let mut handle = idx_writer(&path)?; assert!(matches!(handle.commit_matrix_cell::<IdxCell>(varlit::MatrixKey::new(1, 1)), Err(varlit::Error::MatrixCellNotWritten { .. } | varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedWriter::fn:clear_matrix_cell@487215310c8ecb86",
        valid { let (_d, path) = indexed_grid_fixture(true)?; let mut handle = idx_writer(&path)?; let key = varlit::MatrixKey::new(1, 0); handle.clear_matrix_cell::<IdxCell>(key)?; assert_eq!(handle.matrix_cell_status::<IdxCell>(key)?, varlit::MatrixCellStatus::NotCommitted); Ok(()) },
        invalid { let (_d, path) = indexed_grid_fixture(true)?; let mut handle = idx_writer(&path)?; assert!(matches!(handle.clear_matrix_cell::<IdxCell>(varlit::MatrixKey::new(5, 0)), Err(varlit::Error::MatrixKeyOutOfBounds { .. }))); Ok(()) }
    ),
    contract!("core/indexed/matrix.rs::indexed::matrix::VarlitIndexedWriter::fn:create_with_dims@8a94461c3dd494b4",
        valid { let directory = private_tempdir::private_tempdir()?; let path = directory.path().join("made.varlit"); let spec = IndexedGridMatrix::spec(); let writer = varlit::VarlitIndexedWriter::create_with_dims(spec, &path, varlit::DiskIndexOptions::default(), idx_plan(spec)?, idx_dims())?; assert_eq!(writer.matrix_cell_status::<IdxCell>(varlit::MatrixKey::new(1, 1))?, varlit::MatrixCellStatus::NotCommitted); drop(writer); let reader = idx_reader(&path)?; assert!(reader.matrix_generation().is_ok()); Ok(()) },
        invalid { let directory = private_tempdir::private_tempdir()?; let path = directory.path().join("nomx.varlit"); let spec = IndexedNoMatrix::spec(); const BLOCKS: &[varlit::DiskIndexedBlock] = &[varlit::DiskIndexedBlock::of::<NoMxEntry>()]; let plan = varlit::DiskIndexPlan::canonical(spec, BLOCKS)?; assert!(matches!(varlit::VarlitIndexedWriter::create_with_dims(spec, &path, varlit::DiskIndexOptions::default(), plan, idx_dims()), Err(varlit::Error::MatrixLayoutMissing))); Ok(()) }
    ),
    contract!("core/file.rs::file::RecordIndexEntry::fn:is_chunked@4bce4029df21c02e",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; assert!(entries[0].is_chunked()); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; assert!(!entries[1].is_chunked()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:decode_field@d5ee0e20ba2e2979",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let view = h.view_block::<ChunkBlob>(&e)?; assert_eq!(h.decode_field::<u64>(&e, view.id)?, 1); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let view = h.view_block::<ChunkBlob>(&e)?; assert!(matches!(h.decode_field::<String>(&e, view.id), Err(varlit::Error::WireTypeMismatch { .. }))); assert!(h.decode_field::<Vec<u8>>(&e, view.bytes).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:read_record_from@2f15527b6011740f",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; let page = h.read_record_from(page.next.expect("more"), &mut out)?; assert_eq!(page.filled, 4096); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; assert!(matches!(h.read_record_from(page.next.expect("more"), &mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:read_record_into@884eefd26c76923f",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; assert_eq!(page.filled, 4096); assert!(page.next.is_some()); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); assert!(matches!(h.read_record_into(&e, &mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:read_span_into@b70f6ce39bf190ae",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let mut out = vec![0u8; 1000]; let page = h.read_span_into(&e, view.bytes.data()?, &mut out)?; assert_eq!(page.filled, 1000); assert_eq!(out[999], (999 % 251) as u8); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let view = h.view_block::<ChunkBlob>(&e)?; assert!(matches!(h.read_span_into(&e, view.bytes, &mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:view_block@4650df2a0bb91c7c",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let view: ChunkBlobView = h.view_block::<ChunkBlob>(&e)?; assert_eq!(h.decode_field::<u64>(&e, view.id)?, 1); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut wrong = e.clone(); wrong.block_id = 5; assert!(h.view_block::<ChunkBlob>(&wrong).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:decode_field@d5ee0e20ba2e2979",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_writer(&path)?; let e = entries[0].clone(); let view = h.view_block::<ChunkBlob>(&e)?; assert_eq!(h.decode_field::<u64>(&e, view.id)?, 1); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_writer(&path)?; let e = entries[0].clone(); let view = h.view_block::<ChunkBlob>(&e)?; assert!(matches!(h.decode_field::<String>(&e, view.id), Err(varlit::Error::WireTypeMismatch { .. }))); assert!(h.decode_field::<Vec<u8>>(&e, view.bytes).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:read_record_from@2f15527b6011740f",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_writer(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; let page = h.read_record_from(page.next.expect("more"), &mut out)?; assert_eq!(page.filled, 4096); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_writer(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; assert!(matches!(h.read_record_from(page.next.expect("more"), &mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:read_record_into@884eefd26c76923f",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_writer(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; assert_eq!(page.filled, 4096); assert!(page.next.is_some()); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_writer(&path)?; let e = entries[0].clone(); assert!(matches!(h.read_record_into(&e, &mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:read_span_into@b70f6ce39bf190ae",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_writer(&path)?; let e = entries[0].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let mut out = vec![0u8; 1000]; let page = h.read_span_into(&e, view.bytes.data()?, &mut out)?; assert_eq!(page.filled, 1000); assert_eq!(out[999], (999 % 251) as u8); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_writer(&path)?; let e = entries[0].clone(); let view = h.view_block::<ChunkBlob>(&e)?; assert!(matches!(h.read_span_into(&e, view.bytes, &mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:view_block@4650df2a0bb91c7c",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_writer(&path)?; let e = entries[0].clone(); let view: ChunkBlobView = h.view_block::<ChunkBlob>(&e)?; assert_eq!(h.decode_field::<u64>(&e, view.id)?, 1); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_writer(&path)?; let e = entries[0].clone(); let mut wrong = e.clone(); wrong.block_id = 5; assert!(h.view_block::<ChunkBlob>(&wrong).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::export:FIELD_INLINE@fdc86922091ac192",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(view.id.len() as usize <= varlit::FIELD_INLINE && view.id.inline_bytes().is_some()); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(view.bytes.len() as usize > varlit::FIELD_INLINE && view.bytes.inline_bytes().is_none()); Ok(()) }
    ),
    contract!("core/file.rs::file::export:FieldSpan@ceb6c48bf51dc57e",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let span: varlit::FieldSpan = view.id; assert_eq!(h.decode_field::<u64>(&e, span)?, 1); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let span: varlit::FieldSpan = view.bytes; assert!(h.decode_field::<u64>(&e, span).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::export:FieldWalk@66de5867d4edeaaf",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let id = view.id; let bytes = view.bytes; let mut walk = |visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { visit(1, id)?; visit(2, bytes) }; let w: varlit::FieldWalk<'_> = &mut walk; assert_eq!(<ChunkBlob as varlit::VarlitView>::view_from_fields(w)?, view); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let id = view.id; let mut walk = |visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { visit(1, id)?; visit(1, id) }; let w: varlit::FieldWalk<'_> = &mut walk; assert!(<ChunkBlob as varlit::VarlitView>::view_from_fields(w).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::export:RecordCursor@e8b43d67da8633b0",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let cursor: varlit::RecordCursor = h.read_record_into(&e, &mut out)?.next.expect("more"); assert_eq!(cursor.offset(), 4096); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let cursor: varlit::RecordCursor = h.read_record_into(&e, &mut out)?.next.expect("more"); assert!(matches!(h.read_record_from(cursor, &mut []), Err(varlit::Error::BufferTooSmall { .. }))); assert_eq!(cursor.offset(), 4096); Ok(()) }
    ),
    contract!("core/file.rs::file::export:RecordPage@422233ae43e3fe39",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page: varlit::RecordPage = h.read_record_into(&e, &mut out)?; assert_eq!(page.filled, 4096); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let s = entries[1].clone(); let page: varlit::RecordPage = h.read_record_into(&s, &mut out)?; assert!(page.filled < 4096); assert!(page.next.is_none()); Ok(()) }
    ),
    contract!("core/file.rs::file::export:VarlitView@77de35f0a56fcdb5",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let again: <ChunkBlob as varlit::VarlitView>::View = h.view_block::<ChunkBlob>(&e)?; assert_eq!(again, view); let (id, bytes) = (view.id, view.bytes); let mut walk = |visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { visit(1, id)?; visit(2, bytes) }; assert_eq!(<ChunkBlob as varlit::VarlitView>::view_from_fields(&mut walk)?, view); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let mut walk = |_visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { Ok(()) }; let refused: varlit::Result<<ChunkBlob as varlit::VarlitView>::View> = <ChunkBlob as varlit::VarlitView>::view_from_fields(&mut walk); assert!(matches!(refused, Err(varlit::Error::MissingField { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::export:view_set@9b4a65a3eb812b94",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let mut slot = None; varlit::view_set(&mut slot, view.id)?; assert_eq!(slot, Some(view.id)); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let mut slot = Some(view.id); assert!(varlit::view_set(&mut slot, view.id).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::export:view_wire_mismatch@946433594595f7e6",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(matches!(varlit::view_wire_mismatch("id", varlit::WireType::U64, varlit::WireType::Bytes), varlit::Error::WireTypeMismatch { field: "id", .. })); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let made = varlit::view_wire_mismatch("field", varlit::WireType::String, varlit::WireType::U64); let got = h.decode_field::<String>(&e, view.id).expect_err("refused"); assert_eq!(got.to_string(), made.to_string()); Ok(()) }
    ),
    contract!("core/file/record_read.rs::file::record_read::RecordCursor::fn:end@21d37496e7dbbeb4",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let cursor = h.read_record_into(&e, &mut out)?.next.expect("more"); assert_eq!(cursor.end(), u64::MAX); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let view = h.view_block::<ChunkBlob>(&e)?; let data = view.bytes.data()?; let cursor = h.read_span_into(&e, data, &mut out)?.next.expect("more"); assert_eq!(cursor.end(), data.offset() + data.len()); Ok(()) }
    ),
    contract!("core/file/record_read.rs::file::record_read::RecordCursor::fn:offset@eeb5edcd950f95fa",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let cursor: varlit::RecordCursor = h.read_record_into(&e, &mut out)?.next.expect("more"); assert_eq!(cursor.offset(), 4096); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let cursor: varlit::RecordCursor = h.read_record_into(&e, &mut out)?.next.expect("more"); assert!(matches!(h.read_record_from(cursor, &mut []), Err(varlit::Error::BufferTooSmall { .. }))); assert_eq!(cursor.offset(), 4096); Ok(()) }
    ),
    contract!("core/file/record_read.rs::file::record_read::RecordCursor::fn:record_offset@be378c80bd931ee8",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let cursor = h.read_record_into(&e, &mut out)?.next.expect("more"); assert_eq!(cursor.record_offset(), e.record_offset); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let cursor = h.read_span_into(&e, view.bytes.data()?, &mut out)?.next.expect("more"); assert_ne!(cursor.record_offset(), s.record_offset); Ok(()) }
    ),
    contract!("core/file/record_read.rs::file::record_read::RecordPage::field:end@4d35d3193e8d66f2",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; assert!(page.end > 200_000); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let view = h.view_block::<ChunkBlob>(&e)?; let page = h.read_span_into(&e, view.id, &mut out)?; assert_eq!(page.end, view.id.offset() + view.id.len()); Ok(()) }
    ),
    contract!("core/file/record_read.rs::file::record_read::RecordPage::field:filled@d978be71270b16c1",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page: varlit::RecordPage = h.read_record_into(&e, &mut out)?; assert_eq!(page.filled, 4096); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let s = entries[1].clone(); let page: varlit::RecordPage = h.read_record_into(&s, &mut out)?; assert!(page.filled < 4096); assert!(page.next.is_none()); Ok(()) }
    ),
    contract!("core/file/record_read.rs::file::record_read::RecordPage::field:next@cc5364f0b6d8a819",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; assert!(page.next.is_some()); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let s = entries[1].clone(); let page = h.read_record_into(&s, &mut out)?; assert!(page.next.is_none()); Ok(()) }
    ),
    contract!("core/file/record_read.rs::file::record_read::VarlitFile::fn:read_record_from@2f15527b6011740f",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_readonly(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; let page = h.read_record_from(page.next.expect("more"), &mut out)?; assert_eq!(page.filled, 4096); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_readonly(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; assert!(matches!(h.read_record_from(page.next.expect("more"), &mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file/record_read.rs::file::record_read::VarlitFile::fn:read_record_into@884eefd26c76923f",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_readonly(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; assert_eq!(page.filled, 4096); assert!(page.next.is_some()); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_readonly(&path)?; let e = entries[0].clone(); assert!(matches!(h.read_record_into(&e, &mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file/record_read.rs::file::record_read::struct:RecordCursor@cbf29ce484222325",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let cursor: varlit::RecordCursor = h.read_record_into(&e, &mut out)?.next.expect("more"); assert_eq!(cursor.offset(), 4096); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let cursor: varlit::RecordCursor = h.read_record_into(&e, &mut out)?.next.expect("more"); assert!(matches!(h.read_record_from(cursor, &mut []), Err(varlit::Error::BufferTooSmall { .. }))); assert_eq!(cursor.offset(), 4096); Ok(()) }
    ),
    contract!("core/file/record_read.rs::file::record_read::struct:RecordPage@cbf29ce484222325",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page: varlit::RecordPage = h.read_record_into(&e, &mut out)?; assert_eq!(page.filled, 4096); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let s = entries[1].clone(); let page: varlit::RecordPage = h.read_record_into(&s, &mut out)?; assert!(page.filled < 4096); assert!(page.next.is_none()); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::FieldSpan::fn:data@667e96fcaefbfeec",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert_eq!(view.bytes.data()?.len(), 200_000); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(view.id.data().is_err()); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::FieldSpan::fn:inline_bytes@576d43629dcdecd4",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert_eq!(view.id.inline_bytes(), Some(&1u64.to_le_bytes()[..])); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(view.bytes.inline_bytes().is_none()); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::FieldSpan::fn:is_empty@2db41bc361487a64",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(short.bytes.data()?.is_empty()); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(!view.bytes.data()?.is_empty()); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::FieldSpan::fn:len@0f492946bc24be78",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert_eq!(view.id.len(), 8); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert_eq!(view.bytes.len(), 200_008); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::FieldSpan::fn:offset@2aa8e0d8f7cb616c",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(view.bytes.offset() > view.id.offset()); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert_eq!(view.bytes.data()?.offset(), view.bytes.offset() + 8); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::FieldSpan::fn:wire_type@cec14d111df8ea19",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert_eq!(view.id.wire_type(), varlit::WireType::U64); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert_eq!(view.bytes.wire_type(), varlit::WireType::Bytes); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::VarlitFile::fn:decode_field@d5ee0e20ba2e2979",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_readonly(&path)?; let e = entries[0].clone(); let view = h.view_block::<ChunkBlob>(&e)?; assert_eq!(h.decode_field::<u64>(&e, view.id)?, 1); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_readonly(&path)?; let e = entries[0].clone(); let view = h.view_block::<ChunkBlob>(&e)?; assert!(matches!(h.decode_field::<String>(&e, view.id), Err(varlit::Error::WireTypeMismatch { .. }))); assert!(h.decode_field::<Vec<u8>>(&e, view.bytes).is_err()); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::VarlitFile::fn:read_span_into@7ac6f8c73d15a26d",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_readonly(&path)?; let e = entries[0].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let mut out = vec![0u8; 1000]; let page = h.read_span_into(&e, view.bytes.data()?, &mut out)?; assert_eq!(page.filled, 1000); assert_eq!(out[999], (999 % 251) as u8); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_readonly(&path)?; let e = entries[0].clone(); let view = h.view_block::<ChunkBlob>(&e)?; assert!(matches!(h.read_span_into(&e, view.bytes, &mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::VarlitFile::fn:view_block@4650df2a0bb91c7c",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_readonly(&path)?; let e = entries[0].clone(); let view: ChunkBlobView = h.view_block::<ChunkBlob>(&e)?; assert_eq!(h.decode_field::<u64>(&e, view.id)?, 1); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_readonly(&path)?; let e = entries[0].clone(); let mut wrong = e.clone(); wrong.block_id = 5; assert!(h.view_block::<ChunkBlob>(&wrong).is_err()); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::VarlitView::fn:view_from_fields@c3920b0384088ef1",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let again: <ChunkBlob as varlit::VarlitView>::View = h.view_block::<ChunkBlob>(&e)?; assert_eq!(again, view); let (id, bytes) = (view.id, view.bytes); let mut walk = |visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { visit(1, id)?; visit(2, bytes) }; assert_eq!(<ChunkBlob as varlit::VarlitView>::view_from_fields(&mut walk)?, view); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let mut walk = |_visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { Ok(()) }; let refused: varlit::Result<<ChunkBlob as varlit::VarlitView>::View> = <ChunkBlob as varlit::VarlitView>::view_from_fields(&mut walk); assert!(matches!(refused, Err(varlit::Error::MissingField { .. }))); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::VarlitView::type:View@fd2fbe088eee5ef7",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let again: <ChunkBlob as varlit::VarlitView>::View = h.view_block::<ChunkBlob>(&e)?; assert_eq!(again, view); let (id, bytes) = (view.id, view.bytes); let mut walk = |visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { visit(1, id)?; visit(2, bytes) }; assert_eq!(<ChunkBlob as varlit::VarlitView>::view_from_fields(&mut walk)?, view); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let mut walk = |_visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { Ok(()) }; let refused: varlit::Result<<ChunkBlob as varlit::VarlitView>::View> = <ChunkBlob as varlit::VarlitView>::view_from_fields(&mut walk); assert!(matches!(refused, Err(varlit::Error::MissingField { .. }))); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::const:FIELD_INLINE@90ada0a7acd81e58",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(view.id.len() as usize <= varlit::FIELD_INLINE && view.id.inline_bytes().is_some()); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(view.bytes.len() as usize > varlit::FIELD_INLINE && view.bytes.inline_bytes().is_none()); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::fn:view_set@5bce7c591d3cb2c4",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let mut slot = None; varlit::view_set(&mut slot, view.id)?; assert_eq!(slot, Some(view.id)); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let mut slot = Some(view.id); assert!(varlit::view_set(&mut slot, view.id).is_err()); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::fn:view_wire_mismatch@d99af80b447d0bb3",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(matches!(varlit::view_wire_mismatch("id", varlit::WireType::U64, varlit::WireType::Bytes), varlit::Error::WireTypeMismatch { field: "id", .. })); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let made = varlit::view_wire_mismatch("field", varlit::WireType::String, varlit::WireType::U64); let got = h.decode_field::<String>(&e, view.id).expect_err("refused"); assert_eq!(got.to_string(), made.to_string()); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::struct:FieldSpan@cbf29ce484222325",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let span: varlit::FieldSpan = view.id; assert_eq!(h.decode_field::<u64>(&e, span)?, 1); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let span: varlit::FieldSpan = view.bytes; assert!(h.decode_field::<u64>(&e, span).is_err()); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::trait:VarlitView@cbf29ce484222325",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let again: <ChunkBlob as varlit::VarlitView>::View = h.view_block::<ChunkBlob>(&e)?; assert_eq!(again, view); let (id, bytes) = (view.id, view.bytes); let mut walk = |visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { visit(1, id)?; visit(2, bytes) }; assert_eq!(<ChunkBlob as varlit::VarlitView>::view_from_fields(&mut walk)?, view); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let mut walk = |_visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { Ok(()) }; let refused: varlit::Result<<ChunkBlob as varlit::VarlitView>::View> = <ChunkBlob as varlit::VarlitView>::view_from_fields(&mut walk); assert!(matches!(refused, Err(varlit::Error::MissingField { .. }))); Ok(()) }
    ),
    contract!("core/file/record_view.rs::file::record_view::type:FieldWalk@e84110449fdafe05",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let id = view.id; let bytes = view.bytes; let mut walk = |visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { visit(1, id)?; visit(2, bytes) }; let w: varlit::FieldWalk<'_> = &mut walk; assert_eq!(<ChunkBlob as varlit::VarlitView>::view_from_fields(w)?, view); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let id = view.id; let mut walk = |visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { visit(1, id)?; visit(1, id) }; let w: varlit::FieldWalk<'_> = &mut walk; assert!(<ChunkBlob as varlit::VarlitView>::view_from_fields(w).is_err()); Ok(()) }
    ),
    contract!("core/indexed.rs::indexed::VarlitIndexedReader::fn:decode_field@0d2f8961b9ff7f4e",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = chunk_indexed(&indexed)?; let e = h.lookup_entry::<ChunkBlob>(&1)?.expect("present"); let view = h.view_block::<ChunkBlob>(&e)?; assert_eq!(h.decode_field::<u64>(&e, view.id)?, 1); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = chunk_indexed(&indexed)?; let e = h.lookup_entry::<ChunkBlob>(&1)?.expect("present"); let view = h.view_block::<ChunkBlob>(&e)?; assert!(matches!(h.decode_field::<String>(&e, view.id), Err(varlit::Error::WireTypeMismatch { .. }))); assert!(h.decode_field::<Vec<u8>>(&e, view.bytes).is_err()); Ok(()) }
    ),
    contract!("core/indexed.rs::indexed::VarlitIndexedReader::fn:lookup_entry@745cc9d2c7bd6cee",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = chunk_indexed(&indexed)?; assert!(h.lookup_entry::<ChunkBlob>(&1)?.expect("present").is_chunked()); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = chunk_indexed(&indexed)?; assert!(h.lookup_entry::<ChunkBlob>(&99)?.is_none()); Ok(()) }
    ),
    contract!("core/indexed.rs::indexed::VarlitIndexedReader::fn:read_record_from@f7c9b95778031fc9",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = chunk_indexed(&indexed)?; let e = h.lookup_entry::<ChunkBlob>(&1)?.expect("present"); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; let page = h.read_record_from(page.next.expect("more"), &mut out)?; assert_eq!(page.filled, 4096); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = chunk_indexed(&indexed)?; let e = h.lookup_entry::<ChunkBlob>(&1)?.expect("present"); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; assert!(matches!(h.read_record_from(page.next.expect("more"), &mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/indexed.rs::indexed::VarlitIndexedReader::fn:read_record_into@ff9e6c55ae68bf76",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = chunk_indexed(&indexed)?; let e = h.lookup_entry::<ChunkBlob>(&1)?.expect("present"); let mut out = vec![0u8; 4096]; let page = h.read_record_into(&e, &mut out)?; assert_eq!(page.filled, 4096); assert!(page.next.is_some()); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = chunk_indexed(&indexed)?; let e = h.lookup_entry::<ChunkBlob>(&1)?.expect("present"); assert!(matches!(h.read_record_into(&e, &mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/indexed.rs::indexed::VarlitIndexedReader::fn:read_span_into@773c79ad0ec79f5c",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = chunk_indexed(&indexed)?; let e = h.lookup_entry::<ChunkBlob>(&1)?.expect("present"); let view = h.view_block::<ChunkBlob>(&e)?; let mut out = vec![0u8; 1000]; let page = h.read_span_into(&e, view.bytes.data()?, &mut out)?; assert_eq!(page.filled, 1000); assert_eq!(out[999], (999 % 251) as u8); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = chunk_indexed(&indexed)?; let e = h.lookup_entry::<ChunkBlob>(&1)?.expect("present"); let view = h.view_block::<ChunkBlob>(&e)?; assert!(matches!(h.read_span_into(&e, view.bytes, &mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/indexed.rs::indexed::VarlitIndexedReader::fn:view_block@42f0e967d95d7deb",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = chunk_indexed(&indexed)?; let e = h.lookup_entry::<ChunkBlob>(&1)?.expect("present"); let view: ChunkBlobView = h.view_block::<ChunkBlob>(&e)?; assert_eq!(h.decode_field::<u64>(&e, view.id)?, 1); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = chunk_indexed(&indexed)?; let e = h.lookup_entry::<ChunkBlob>(&1)?.expect("present"); let mut wrong = e.clone(); wrong.block_id = 5; assert!(h.view_block::<ChunkBlob>(&wrong).is_err()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:FIELD_INLINE@24dc1e46edff0431",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(view.id.len() as usize <= varlit::FIELD_INLINE && view.id.inline_bytes().is_some()); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(view.bytes.len() as usize > varlit::FIELD_INLINE && view.bytes.inline_bytes().is_none()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:FieldSpan@7f1b462764053117",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let span: varlit::FieldSpan = view.id; assert_eq!(h.decode_field::<u64>(&e, span)?, 1); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let span: varlit::FieldSpan = view.bytes; assert!(h.decode_field::<u64>(&e, span).is_err()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:FieldWalk@2af1b24601792e9e",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let id = view.id; let bytes = view.bytes; let mut walk = |visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { visit(1, id)?; visit(2, bytes) }; let w: varlit::FieldWalk<'_> = &mut walk; assert_eq!(<ChunkBlob as varlit::VarlitView>::view_from_fields(w)?, view); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let id = view.id; let mut walk = |visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { visit(1, id)?; visit(1, id) }; let w: varlit::FieldWalk<'_> = &mut walk; assert!(<ChunkBlob as varlit::VarlitView>::view_from_fields(w).is_err()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:RecordCursor@cd539acc0915f4da",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let cursor: varlit::RecordCursor = h.read_record_into(&e, &mut out)?.next.expect("more"); assert_eq!(cursor.offset(), 4096); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let cursor: varlit::RecordCursor = h.read_record_into(&e, &mut out)?.next.expect("more"); assert!(matches!(h.read_record_from(cursor, &mut []), Err(varlit::Error::BufferTooSmall { .. }))); assert_eq!(cursor.offset(), 4096); Ok(()) }
    ),
    contract!("core/lib.rs::::export:RecordPage@1403639b9b072173",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let page: varlit::RecordPage = h.read_record_into(&e, &mut out)?; assert_eq!(page.filled, 4096); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let mut out = vec![0u8; 4096]; let s = entries[1].clone(); let page: varlit::RecordPage = h.read_record_into(&s, &mut out)?; assert!(page.filled < 4096); assert!(page.next.is_none()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:VarlitView@63b03dd6a50f9fa2",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let again: <ChunkBlob as varlit::VarlitView>::View = h.view_block::<ChunkBlob>(&e)?; assert_eq!(again, view); let (id, bytes) = (view.id, view.bytes); let mut walk = |visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { visit(1, id)?; visit(2, bytes) }; assert_eq!(<ChunkBlob as varlit::VarlitView>::view_from_fields(&mut walk)?, view); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let mut walk = |_visit: &mut dyn FnMut(u32, varlit::FieldSpan) -> varlit::Result<()>| -> varlit::Result<()> { Ok(()) }; let refused: varlit::Result<<ChunkBlob as varlit::VarlitView>::View> = <ChunkBlob as varlit::VarlitView>::view_from_fields(&mut walk); assert!(matches!(refused, Err(varlit::Error::MissingField { .. }))); Ok(()) }
    ),
    contract!("core/lib.rs::::export:view_set@c915b9bbaa3df7c7",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let mut slot = None; varlit::view_set(&mut slot, view.id)?; assert_eq!(slot, Some(view.id)); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let mut slot = Some(view.id); assert!(varlit::view_set(&mut slot, view.id).is_err()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:view_wire_mismatch@34f4d9a0f81773e5",
        valid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; assert!(matches!(varlit::view_wire_mismatch("id", varlit::WireType::U64, varlit::WireType::Bytes), varlit::Error::WireTypeMismatch { field: "id", .. })); Ok(()) },
        invalid { let (_d, path, indexed) = chunk_fixture()?; let entries = chunk_entries(&path)?; let _ = &indexed; let h = ChunkFmt::spec().open_reader(&path)?; let e = entries[0].clone(); let s = entries[1].clone(); let view = h.view_block::<ChunkBlob>(&e)?; let short = h.view_block::<ChunkBlob>(&s)?; let made = varlit::view_wire_mismatch("field", varlit::WireType::String, varlit::WireType::U64); let got = h.decode_field::<String>(&e, view.id).expect_err("refused"); assert_eq!(got.to_string(), made.to_string()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:read_matrix_region_into@4da15e3526589c4b",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_reader(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 4]; let page = handle.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out)?; assert_eq!(page.filled, 4); assert_eq!(out[3].value, 12); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let handle = GridMatrix::spec().open_reader(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 4]; assert!(matches!(handle.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:read_matrix_region_from@c481feaf33f56499",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_reader(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 1]; let page = handle.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::row(1, 0..2), &mut out)?; let page = handle.read_matrix_region_from::<GridCell>(page.next.expect("one more"), &mut out)?; assert_eq!((page.filled, page.next), (1, None)); assert_eq!(out[0].value, 12); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let handle = GridMatrix::spec().open_reader(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 1]; let page = handle.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::row(0, 0..2), &mut out)?; assert!(matches!(handle.read_matrix_region_from::<GridCell>(page.next.expect("one more"), &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:read_matrix_region_bytes_into@bdafe560b173747a",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 16]; let page = handle.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::column(1, 0..2), &mut out)?; assert_eq!(page.filled, 2); assert_eq!(u64::from_le_bytes(out[8..].try_into().unwrap()), 12); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 7]; assert!(handle.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::column(1, 0..2), &mut out).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:read_matrix_region_bytes_from@4a865dcde776b98c",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 8]; let page = handle.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::column(0, 0..2), &mut out)?; let page = handle.read_matrix_region_bytes_from::<GridCell>(page.next.expect("one more"), &mut out)?; assert_eq!(page.filled, 1); assert_eq!(u64::from_le_bytes(out), 11); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 8]; let page = handle.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::column(0, 0..2), &mut out)?; let mut short = [0u8; 3]; assert!(handle.read_matrix_region_bytes_from::<GridCell>(page.next.expect("one more"), &mut short).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:read_matrix_region_into@4da15e3526589c4b",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_writer(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 4]; let page = handle.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out)?; assert_eq!(page.filled, 4); assert_eq!(out[3].value, 12); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let handle = GridMatrix::spec().open_writer(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 4]; assert!(matches!(handle.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:read_matrix_region_from@c481feaf33f56499",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_writer(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 1]; let page = handle.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::row(1, 0..2), &mut out)?; let page = handle.read_matrix_region_from::<GridCell>(page.next.expect("one more"), &mut out)?; assert_eq!((page.filled, page.next), (1, None)); assert_eq!(out[0].value, 12); Ok(()) },
        invalid { let (_directory, path) = grid_fixture()?; let handle = GridMatrix::spec().open_writer(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 1]; let page = handle.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::row(0, 0..2), &mut out)?; assert!(matches!(handle.read_matrix_region_from::<GridCell>(page.next.expect("one more"), &mut out), Err(varlit::Error::MatrixNotCommitted))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:read_matrix_region_bytes_into@bdafe560b173747a",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_writer(&path)?; let mut out = [0u8; 16]; let page = handle.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::column(1, 0..2), &mut out)?; assert_eq!(page.filled, 2); assert_eq!(u64::from_le_bytes(out[8..].try_into().unwrap()), 12); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_writer(&path)?; let mut out = [0u8; 7]; assert!(handle.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::column(1, 0..2), &mut out).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:read_matrix_region_bytes_from@4a865dcde776b98c",
        valid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_writer(&path)?; let mut out = [0u8; 8]; let page = handle.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::column(0, 0..2), &mut out)?; let page = handle.read_matrix_region_bytes_from::<GridCell>(page.next.expect("one more"), &mut out)?; assert_eq!(page.filled, 1); assert_eq!(u64::from_le_bytes(out), 11); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let handle = GridMatrix::spec().open_writer(&path)?; let mut out = [0u8; 8]; let page = handle.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::column(0, 0..2), &mut out)?; let mut short = [0u8; 3]; assert!(handle.read_matrix_region_bytes_from::<GridCell>(page.next.expect("one more"), &mut short).is_err()); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::struct:MatrixRegion@cbf29ce484222325",
        valid { let region: varlit::MatrixRegion = varlit::MatrixRegion::new(1..3, 2..5); assert_eq!(region.cells(), Some(6)); Ok(()) },
        invalid { let region: varlit::MatrixRegion = varlit::MatrixRegion::new(3..3, 0..1); assert!(region.is_empty()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:MatrixRegion@350f1d5f47059edf",
        valid { assert_eq!(varlit::MatrixRegion::row(2, 0..4).cells(), Some(4)); Ok(()) },
        invalid { assert_eq!(varlit::MatrixRegion::new(0..0, 0..9).cells(), Some(0)); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixRegion::field:row_start@4d35d3193e8d66f2",
        valid { assert_eq!(varlit::MatrixRegion::new(1..3, 2..5).row_start, 1); Ok(()) },
        invalid { assert_ne!(varlit::MatrixRegion::column(4, 0..2).row_start, 4); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixRegion::field:row_end@4d35d3193e8d66f2",
        valid { assert_eq!(varlit::MatrixRegion::new(1..3, 2..5).row_end, 3); Ok(()) },
        invalid { assert_eq!(varlit::MatrixRegion::row(7, 0..1).row_end, 8); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixRegion::field:col_start@4d35d3193e8d66f2",
        valid { assert_eq!(varlit::MatrixRegion::new(1..3, 2..5).col_start, 2); Ok(()) },
        invalid { assert_ne!(varlit::MatrixRegion::row(4, 0..2).col_start, 4); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixRegion::field:col_end@4d35d3193e8d66f2",
        valid { assert_eq!(varlit::MatrixRegion::new(1..3, 2..5).col_end, 5); Ok(()) },
        invalid { assert_eq!(varlit::MatrixRegion::column(7, 0..1).col_end, 8); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixRegion::fn:new@373d177e82b24490",
        valid { let region = varlit::MatrixRegion::new(1..3, 2..5); assert!(region.contains(varlit::MatrixKey::new(2, 4))); Ok(()) },
        invalid { let region = varlit::MatrixRegion::new(1..3, 2..5); assert!(!region.contains(varlit::MatrixKey::new(3, 4))); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixRegion::fn:row@0880e25407281221",
        valid { assert_eq!(varlit::MatrixRegion::row(4, 1..3), varlit::MatrixRegion::new(4..5, 1..3)); Ok(()) },
        invalid { assert!(varlit::MatrixRegion::row(4, 3..3).is_empty()); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixRegion::fn:column@ec52338497b1c667",
        valid { assert_eq!(varlit::MatrixRegion::column(4, 1..3), varlit::MatrixRegion::new(1..3, 4..5)); Ok(()) },
        invalid { assert!(varlit::MatrixRegion::column(4, 3..1).is_empty()); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixRegion::fn:is_empty@2db41bc361487a64",
        valid { assert!(!varlit::MatrixRegion::new(0..1, 0..1).is_empty()); Ok(()) },
        invalid { assert!(varlit::MatrixRegion::new(0..1, 1..1).is_empty()); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixRegion::fn:contains@82d4a926cc60385e",
        valid { assert!(varlit::MatrixRegion::new(0..2, 0..2).contains(varlit::MatrixKey::new(1, 1))); Ok(()) },
        invalid { assert!(!varlit::MatrixRegion::new(0..2, 0..2).contains(varlit::MatrixKey::new(1, 2))); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixRegion::fn:cells@b2111673dbaca2d3",
        valid { assert_eq!(varlit::MatrixRegion::new(0..3, 0..4).cells(), Some(12)); Ok(()) },
        invalid { assert_eq!(varlit::MatrixRegion::new(0..u64::MAX, 0..u64::MAX).cells(), None); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixRegion::fn:first@3bae251c3b893b1c",
        valid { assert_eq!(varlit::MatrixRegion::new(3..4, 5..6).first(), varlit::MatrixKey::new(3, 5)); Ok(()) },
        invalid { assert_ne!(varlit::MatrixRegion::new(3..4, 5..6).first(), varlit::MatrixKey::new(0, 0)); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::struct:MatrixCursor@cbf29ce484222325",
        valid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 1]; let cursor: varlit::MatrixCursor = reader.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::row(0, 0..2), &mut out)?.next.expect("one more"); assert_eq!(cursor.next_key(), varlit::MatrixKey::new(0, 1)); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = vec![<GridCell as varlit::VarlitEmpty>::empty(); 2]; let next: Option<varlit::MatrixCursor> = reader.read_matrix_region_into::<GridCell>(varlit::MatrixRegion::row(0, 0..2), &mut out)?.next; assert!(next.is_none()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:MatrixCursor@5028a4937a026b65",
        valid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 8]; let cursor: Option<varlit::MatrixCursor> = reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::row(0, 0..2), &mut out)?.next; assert!(cursor.is_some()); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 16]; let cursor: Option<varlit::MatrixCursor> = reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::row(0, 0..2), &mut out)?.next; assert!(cursor.is_none()); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixCursor::fn:next_key@18ebeabf320e997f",
        valid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 8]; let cursor = reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out)?.next.expect("more"); assert_eq!(cursor.next_key(), varlit::MatrixKey::new(0, 1)); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 16]; let cursor = reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out)?.next.expect("more"); assert_ne!(cursor.next_key(), varlit::MatrixKey::new(0, 1)); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixCursor::fn:region@842e78e33c7c4223",
        valid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 8]; let region = varlit::MatrixRegion::new(0..2, 0..2); let cursor = reader.read_matrix_region_bytes_into::<GridCell>(region, &mut out)?.next.expect("more"); assert_eq!(cursor.region(), region); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 8]; let cursor = reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out)?.next.expect("more"); assert_ne!(cursor.region(), varlit::MatrixRegion::row(0, 0..2)); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::struct:MatrixPage@cbf29ce484222325",
        valid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 32]; let page: varlit::MatrixPage = reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out)?; assert_eq!(page.filled, 4); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 0]; let page: varlit::MatrixPage = reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::new(0..2, 0..2), &mut out)?; assert_eq!(page.filled, 0); Ok(()) }
    ),
    contract!("core/lib.rs::::export:MatrixPage@be2a572cf08275fc",
        valid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 8]; let page: varlit::MatrixPage = reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::row(1, 1..2), &mut out)?; assert!(page.next.is_none()); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 8]; let page: varlit::MatrixPage = reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::row(1, 0..2), &mut out)?; assert!(page.next.is_some()); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixPage::field:filled@d978be71270b16c1",
        valid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 16]; assert_eq!(reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::row(1, 0..2), &mut out)?.filled, 2); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 8]; assert_ne!(reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::row(1, 0..2), &mut out)?.filled, 2); Ok(()) }
    ),
    contract!("core/matrix.rs::matrix::MatrixPage::field:next@942956136eb7d815",
        valid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 8]; assert!(reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::row(1, 0..2), &mut out)?.next.is_some()); Ok(()) },
        invalid { let (_directory, path) = full_grid_fixture()?; let reader = GridMatrix::spec().open_reader(&path)?; let mut out = [0u8; 16]; assert!(reader.read_matrix_region_bytes_into::<GridCell>(varlit::MatrixRegion::row(1, 0..2), &mut out)?.next.is_none()); Ok(()) }
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
    contract!("core/scalable_fault.rs::scalable_fault::const:REQUIRED_POINTS@4f413771e59774b2",
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
    contract!("core/disk_index.rs::disk_index::struct:DiskIndexConfirmPolicy@cbf29ce484222325",
        valid { let policy: varlit::DiskIndexConfirmPolicy = varlit::DiskIndexConfirmPolicy::EVERY_SYNC; assert_eq!(confirmations(varlit::DiskIndexOptions { confirm: policy, ..varlit::DiskIndexOptions::default() }, 3)?, 3); Ok(()) },
        invalid { let policy: varlit::DiskIndexConfirmPolicy = varlit::DiskIndexConfirmPolicy { max_bytes: u64::MAX, max_records: u64::MAX }; assert_eq!(confirmations(varlit::DiskIndexOptions { confirm: policy, ..varlit::DiskIndexOptions::default() }, 3)?, 0); Ok(()) }
    ),
    contract!("core/disk_index.rs::disk_index::DiskIndexConfirmPolicy::const:EVERY_SYNC@c48f72f43b915edb",
        valid { assert_eq!(confirmations(varlit::DiskIndexOptions { confirm: varlit::DiskIndexConfirmPolicy::EVERY_SYNC, ..varlit::DiskIndexOptions::default() }, 4)?, 4); Ok(()) },
        invalid { assert_ne!(varlit::DiskIndexConfirmPolicy::default(), varlit::DiskIndexConfirmPolicy::EVERY_SYNC); assert_eq!(confirmations(varlit::DiskIndexOptions::default(), 4)?, 0); Ok(()) }
    ),
    contract!("core/disk_index.rs::disk_index::DiskIndexConfirmPolicy::field:max_bytes@4d35d3193e8d66f2",
        valid { let confirm = varlit::DiskIndexConfirmPolicy { max_bytes: 1, max_records: u64::MAX }; assert_eq!(confirmations(varlit::DiskIndexOptions { confirm, ..varlit::DiskIndexOptions::default() }, 2)?, 2); Ok(()) },
        invalid { let confirm = varlit::DiskIndexConfirmPolicy { max_bytes: u64::MAX, max_records: u64::MAX }; assert_eq!(confirmations(varlit::DiskIndexOptions { confirm, ..varlit::DiskIndexOptions::default() }, 2)?, 0); Ok(()) }
    ),
    contract!("core/disk_index.rs::disk_index::DiskIndexConfirmPolicy::field:max_records@4d35d3193e8d66f2",
        valid { let confirm = varlit::DiskIndexConfirmPolicy { max_bytes: u64::MAX, max_records: 1 }; assert_eq!(confirmations(varlit::DiskIndexOptions { confirm, ..varlit::DiskIndexOptions::default() }, 2)?, 2); Ok(()) },
        invalid { let confirm = varlit::DiskIndexConfirmPolicy { max_bytes: u64::MAX, max_records: 5 }; assert_eq!(confirmations(varlit::DiskIndexOptions { confirm, ..varlit::DiskIndexOptions::default() }, 2)?, 0); Ok(()) }
    ),
    contract!("core/disk_index.rs::disk_index::DiskIndexOptions::field:confirm@9972f89bd550d59a",
        valid { let options = varlit::DiskIndexOptions { confirm: varlit::DiskIndexConfirmPolicy::EVERY_SYNC, ..varlit::DiskIndexOptions::default() }; assert_eq!(confirmations(options, 2)?, 2); Ok(()) },
        invalid { let options = varlit::DiskIndexOptions { confirm: varlit::DiskIndexConfirmPolicy { max_bytes: u64::MAX, max_records: u64::MAX }, ..varlit::DiskIndexOptions::default() }; assert_eq!(confirmations(options, 2)?, 0); Ok(()) }
    ),
    contract!("core/lib.rs::::export:DiskIndexConfirmPolicy@252f0bb080294882",
        valid { let policy = varlit::DiskIndexConfirmPolicy::EVERY_SYNC; assert_eq!(confirmations(varlit::DiskIndexOptions { confirm: policy, ..varlit::DiskIndexOptions::default() }, 1)?, 1); Ok(()) },
        invalid { let policy = varlit::DiskIndexConfirmPolicy::default(); assert!(policy.max_bytes > 0 && policy.max_records > 0); assert_eq!(confirmations(varlit::DiskIndexOptions { confirm: policy, ..varlit::DiskIndexOptions::default() }, 1)?, 0); Ok(()) }
    ),
    contract!("core/disk_index.rs::disk_index::DiskIndexOptions::field:max_tail_records@4d35d3193e8d66f2",
        valid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("tail.varlit"); let mut w = IntoIndex::create_indexed_writer(&path, varlit::DiskIndexOptions::default())?; w.push_indexed_entry(&IndexedEntry { id: 1, value: 10 })?; w.sync()?; let r = IntoIndex::open_indexed_reader(&path, varlit::DiskIndexOptions { max_tail_records: 2, ..varlit::DiskIndexOptions::default() })?; let mut out = <IndexedEntry as varlit::VarlitEmpty>::empty(); assert!(r.get_indexed_entry(&1, &mut out)?); assert_eq!(out.value, 10); drop(w); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("tail.varlit"); let mut w = IntoIndex::create_indexed_writer(&path, varlit::DiskIndexOptions::default())?; w.push_indexed_entry(&IndexedEntry { id: 1, value: 10 })?; w.sync()?; let opened = IntoIndex::open_indexed_reader(&path, varlit::DiskIndexOptions { max_tail_records: 1, ..varlit::DiskIndexOptions::default() }); assert!(opened.is_err()); drop(w); Ok(()) }
    ),
    contract!("core/disk_index.rs::disk_index::DiskIndexOptions::field:reclaim_after@4d35d3193e8d66f2",
        valid { let options = varlit::DiskIndexOptions { reclaim_after: 1_000, ..varlit::DiskIndexOptions::default() }; assert!(stale_lookup(options.reclaim_after, 60)?.is_none()); Ok(()) },
        invalid { let options = varlit::DiskIndexOptions { reclaim_after: 0, ..varlit::DiskIndexOptions::default() }; assert!(stale_lookup(options.reclaim_after, 60)?.is_some()); Ok(()) }
    ),
    contract!("core/disk_index.rs::disk_index::DiskIndexError::variant:PageReclaimed@b31125477191296c",
        valid { let outcome = stale_lookup(1_000, 60)?; assert!(!matches!(&outcome, Some(varlit::Error::DiskIndex(e)) if matches!(**e, varlit::DiskIndexError::PageReclaimed))); assert!(outcome.is_none()); Ok(()) },
        invalid { let outcome = stale_lookup(0, 60)?; assert!(matches!(&outcome, Some(varlit::Error::DiskIndex(e)) if matches!(**e, varlit::DiskIndexError::PageReclaimed)), "{outcome:?}"); Ok(()) }
    ),
    contract!("core/sync_marker.rs::sync_marker::const:SYNC_BLOCK_ID@b743b3c893debb89",
        valid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("marker.varlit"); let mut w = IntoIndex::create_indexed_writer(&path, varlit::DiskIndexOptions::default())?; w.push_indexed_entry(&IndexedEntry { id: 1, value: 1 })?; w.sync()?; let r = IntoIndex::open_indexed_reader(&path, varlit::DiskIndexOptions::default())?; let ids: Vec<u32> = r.events()?.map(|e| e.map(|e| e.block_id)).collect::<varlit::Result<_>>()?; assert_eq!(ids.last(), Some(&varlit::SYNC_BLOCK_ID)); drop(w); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("marker.varlit"); let mut w = IntoIndex::create_indexed_writer(&path, varlit::DiskIndexOptions::default())?; w.push_indexed_entry(&IndexedEntry { id: 1, value: 1 })?; w.flush()?; let r = IntoIndex::open_indexed_reader(&path, varlit::DiskIndexOptions::default())?; let ids: Vec<u32> = r.events()?.map(|e| e.map(|e| e.block_id)).collect::<varlit::Result<_>>()?; assert!(!ids.contains(&varlit::SYNC_BLOCK_ID)); assert!(!ids.contains(&75)); drop(w); Ok(()) }
    ),
    contract!("core/lib.rs::::export:SYNC_BLOCK_ID@9b93dc73b3174862",
        valid { const { assert!(varlit::SYNC_BLOCK_ID >= 0xFFFF_FF00) }; let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("marker.varlit"); let mut w = IntoIndex::create_indexed_writer(&path, varlit::DiskIndexOptions::default())?; w.push_indexed_entry(&IndexedEntry { id: 1, value: 1 })?; w.sync()?; let r = IntoIndex::open_indexed_reader(&path, varlit::DiskIndexOptions::default())?; assert!(r.events()?.any(|e| e.is_ok_and(|e| e.block_id == varlit::SYNC_BLOCK_ID))); drop(w); Ok(()) },
        invalid { let dir = private_tempdir::private_tempdir()?; let path = dir.path().join("plain.varlit"); let mut f = QualificationProbe::create(&path)?; f.push(&Probe { value: 1 })?; f.sync()?; drop(f); let f = QualificationProbe::open_readonly(&path)?; let entries = crate::pages::all_entries!(f); assert!(!entries.iter().any(|e| e.block_id == varlit::SYNC_BLOCK_ID)); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:record_map_from@db621a5c990f1f13",
        valid { let (_d, path, start, end) = resume_probe()?; let f = varlit::VarlitFile::open_readonly_lazy(ResumeProbe::spec(), &path)?; let mut buffer = Vec::new(); let mut map = f.record_map_from(&mut buffer, start)?; map.fill()?; assert_eq!(map.resume_offset(), end); Ok(()) },
        invalid { let (_d, path, _start, end) = resume_probe()?; let f = varlit::VarlitFile::open_readonly_lazy(ResumeProbe::spec(), &path)?; let mut buffer = Vec::new(); assert!(f.record_map_from(&mut buffer, end + 1).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:record_map_from@db621a5c990f1f13",
        valid { let (_d, path, start, end) = resume_probe()?; let r = ResumeProbe::spec().open_reader(&path)?; let mut buffer = Vec::new(); let mut map = r.record_map_from(&mut buffer, start)?; map.fill()?; assert_eq!(map.resume_offset(), end); Ok(()) },
        invalid { let (_d, path, start, _end) = resume_probe()?; let r = ResumeProbe::spec().open_reader(&path)?; let mut buffer = Vec::new(); assert!(r.record_map_from(&mut buffer, start - 1).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:record_map_from@db621a5c990f1f13",
        valid { let (_d, path, start, end) = resume_probe()?; let w = ResumeProbe::spec().open_writer(&path)?; let mut buffer = Vec::new(); let mut map = w.record_map_from(&mut buffer, start)?; map.fill()?; assert_eq!(map.resume_offset(), end); Ok(()) },
        invalid { let (_d, path, _start, end) = resume_probe()?; let w = ResumeProbe::spec().open_writer(&path)?; let mut buffer = Vec::new(); assert!(w.record_map_from(&mut buffer, end + 1).is_err()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::Anchor::field:k@4d35d3193e8d66f2",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let a = file.anchor(1)?.expect("an intact anchor"); assert_eq!(a.k, 1); assert_eq!(file.anchor(2)?.unwrap().k, 2); Ok(()) },
        invalid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let past = file.anchor_layout().unwrap().anchors_before(u64::MAX / 2); assert!(file.anchor(past)?.map(|a| a.k).is_none()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::Anchor::field:logical@4d35d3193e8d66f2",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let a = file.anchor(1)?.expect("an intact anchor"); assert_eq!(Some(a.logical), file.anchor_layout().unwrap().anchor_logical(1)); Ok(()) },
        invalid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let _file = ProbeFormat::spec().open_readonly(&path)?; damage_anchor(&path, 1)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert!(file.anchor(1)?.map(|a| a.logical).is_none()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::Anchor::field:record_end@4d35d3193e8d66f2",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let a = file.anchor(1)?.expect("an intact anchor"); assert!(a.logical < a.record_end); let e = crate::pages::all_entries!(file).iter().find(|e| e.record_offset == a.record_start).cloned().unwrap(); assert!(e.payload_offset + e.payload_len <= a.record_end); Ok(()) },
        invalid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert!(file.anchor(1)?.map(|a| a.record_end).is_none()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::Anchor::field:record_start@4d35d3193e8d66f2",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let a = file.anchor(1)?.expect("an intact anchor"); assert!(a.record_start <= a.logical); assert!(crate::pages::all_entries!(file).iter().any(|e| e.record_offset == a.record_start)); Ok(()) },
        invalid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert!(file.anchor(1)?.map(|a| a.record_start).is_none()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::Anchor::field:sequence@4d35d3193e8d66f2",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let a = file.anchor(1)?.expect("an intact anchor"); let e = crate::pages::all_entries!(file).iter().find(|e| e.record_offset == a.record_start).cloned().unwrap(); assert_eq!(e.sequence, a.sequence); Ok(()) },
        invalid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let _file = ProbeFormat::spec().open_readonly(&path)?; damage_anchor(&path, 1)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert!(file.anchor(1)?.map(|a| a.sequence).is_none()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorLayout::fn:anchor_logical@de2a075cc78f3660",
        valid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; assert_eq!(l.anchor_logical(1), Some(100 + 8192)); assert_eq!(l.anchor_logical(2), Some(100 + 8192 + 8192 - 256)); Ok(()) },
        invalid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; assert_eq!(l.anchor_logical(0), None); assert_eq!(l.anchor_logical(u64::MAX), None); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorLayout::fn:anchor_physical@691475448f9c147c",
        valid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; assert_eq!(l.anchor_physical(1), Some(100 + 8192)); assert_eq!(l.anchor_physical(3), Some(100 + 3 * 8192)); Ok(()) },
        invalid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; assert_eq!(l.anchor_physical(0), None); assert_eq!(l.anchor_physical(u64::MAX), None); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorLayout::fn:anchors_before@08b298d8c970b962",
        valid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; let first = l.anchor_logical(1).unwrap(); assert_eq!(l.anchors_before(first + 1), 1); assert_eq!(l.anchors_before(l.anchor_logical(5).unwrap() + 1), 5); Ok(()) },
        invalid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; assert_eq!(l.anchors_before(0), 0); assert_eq!(l.anchors_before(l.anchor_logical(1).unwrap()), 0); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorLayout::fn:block@117dee6ffe3aa276",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert_eq!(file.anchor_layout().unwrap().block(), 8192); Ok(()) },
        invalid { assert!(varlit::AnchorLayout::new(0, 256, 256).map(|l| l.block()).is_err()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorLayout::fn:hole@44b59bbbaea5b15d",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert_eq!(file.anchor_layout().unwrap().hole(), 256); Ok(()) },
        invalid { assert!(varlit::AnchorLayout::new(0, 8192, 32).map(|l| l.hole()).is_err()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorLayout::fn:logical_len@88cbf278b022703f",
        valid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; assert_eq!(l.logical_len(100 + 8192 + 256 + 10), 100 + 8192 + 10); assert_eq!(l.logical_len(50), 50); Ok(()) },
        invalid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; assert_eq!(l.logical_len(100 + 8192 + 100), 100 + 8192); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorLayout::fn:new@d0b8b2826fc0933c",
        valid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; assert_eq!((l.start(), l.block(), l.hole()), (100, 8192, 256)); Ok(()) },
        invalid { assert!(matches!(varlit::AnchorLayout::new(0, 300, 200), Err(varlit::Error::InvalidFormatSpec(_)))); assert!(varlit::AnchorLayout::new(0, 8192, 63).is_err()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorLayout::fn:physical@0e8187266f25807e",
        valid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; assert_eq!(l.physical(50), Some(50)); assert_eq!(l.physical(100 + 8192), Some(100 + 8192 + 256)); Ok(()) },
        invalid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; assert_eq!(l.physical(u64::MAX), None); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorLayout::fn:physical_end@d033ab84519d462e",
        valid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; assert_eq!(l.physical_end(100 + 8192), Some(100 + 8192)); assert_eq!(l.physical_end(100 + 8192 + 1), Some(100 + 8192 + 256 + 1)); Ok(()) },
        invalid { let l = varlit::AnchorLayout::new(100, 8192, 256)?; assert_eq!(l.physical_end(u64::MAX), None); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorLayout::fn:start@8edeeff79f3dcb2b",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let l = file.anchor_layout().unwrap(); assert!(l.start() > 0); assert_eq!(l.anchor_physical(1), Some(l.start() + l.block())); Ok(()) },
        invalid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert!(file.anchor_layout().map(|l| l.start()).is_none()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorPolicy::const:DEFAULT@15807fa821590e4f",
        valid { assert_eq!(varlit::AnchorPolicy::DEFAULT.spacing(), Some((4 << 20, 4096))); assert_eq!(ProbeFormat::spec().anchors, varlit::AnchorPolicy::DEFAULT); Ok(()) },
        invalid { assert!(varlit::AnchorPolicy::DEFAULT.is_anchored()); assert_ne!(varlit::AnchorPolicy::DEFAULT, varlit::AnchorPolicy::NONE); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorPolicy::const:NONE@78d95eadedf6c3f2",
        valid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert!(file.anchor_layout().is_none()); assert!(!varlit::AnchorPolicy::NONE.is_anchored()); Ok(()) },
        invalid { assert_eq!(varlit::AnchorPolicy::NONE.spacing(), None); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorPolicy::fn:every@ee8f8a5aaa42af84",
        valid { assert_eq!(varlit::AnchorPolicy::every(13, true).spacing(), Some((8192, 256))); assert_eq!(varlit::AnchorPolicy::every(20, false).spacing(), Some((1 << 20, 4096))); Ok(()) },
        invalid { assert!(varlit::AnchorPolicy::every(12, false).validate().is_err()); assert!(varlit::AnchorPolicy::every(63, false).validate().is_err()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorPolicy::fn:is_anchored@754dbb54430469bb",
        valid { assert!(SMALL_ANCHORS.is_anchored()); Ok(()) },
        invalid { assert!(!varlit::AnchorPolicy::NONE.is_anchored()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorPolicy::fn:spacing@fe9d7d83d9fad873",
        valid { assert_eq!(SMALL_ANCHORS.spacing(), Some((8192, 256))); Ok(()) },
        invalid { assert_eq!(varlit::AnchorPolicy::NONE.spacing(), None); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::AnchorPolicy::fn:validate@31c51d8c81855d4e",
        valid { assert_eq!(SMALL_ANCHORS.validate()?, SMALL_ANCHORS); assert!(varlit::AnchorPolicy::NONE.validate().is_ok()); Ok(()) },
        invalid { assert!(matches!(varlit::AnchorPolicy::every(12, false).validate(), Err(varlit::Error::InvalidFormatSpec(_)))); let d = private_tempdir::private_tempdir()?; assert!(ProbeFormat::spec().with_anchors(varlit::AnchorPolicy::every(63, false)).create(d.path().join("bad")).is_err()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::struct:Anchor@cbf29ce484222325",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let a = file.anchor(1)?.expect("an intact anchor"); let a: varlit::Anchor = a; assert_eq!(a.k, 1); Ok(()) },
        invalid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let file = ProbeFormat::spec().open_readonly(&path)?; let a: Option<varlit::Anchor> = file.anchor(1)?; assert!(a.is_none()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::struct:AnchorLayout@cbf29ce484222325",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let l: varlit::AnchorLayout = file.anchor_layout().unwrap(); assert_eq!(l.block(), 8192); Ok(()) },
        invalid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let file = ProbeFormat::spec().open_readonly(&path)?; let l: Option<varlit::AnchorLayout> = file.anchor_layout(); assert!(l.is_none()); Ok(()) }
    ),
    contract!("core/anchor.rs::anchor::struct:AnchorPolicy@cbf29ce484222325",
        valid { let p: varlit::AnchorPolicy = ProbeFormat::spec().anchors; assert!(p.is_anchored()); Ok(()) },
        invalid { let p: varlit::AnchorPolicy = varlit::AnchorPolicy::every(12, false); assert!(p.validate().is_err()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:Anchor@01f5cf9e18b1bde3",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let a = file.anchor(1)?.expect("an intact anchor"); let a: varlit::Anchor = a; assert!(a.record_start <= a.logical); Ok(()) },
        invalid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let file = ProbeFormat::spec().open_readonly(&path)?; let a: Option<varlit::Anchor> = file.anchor(1)?; assert!(a.is_none()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:AnchorLayout@6f2cda96913ad0a9",
        valid { let l: varlit::AnchorLayout = varlit::AnchorLayout::new(0, 8192, 256)?; assert_eq!(l.hole(), 256); Ok(()) },
        invalid { let l: varlit::Result<varlit::AnchorLayout> = varlit::AnchorLayout::new(0, 100, 64); assert!(l.is_err()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:AnchorPolicy@0fa98147337a8645",
        valid { let p: varlit::AnchorPolicy = varlit::AnchorPolicy::DEFAULT; assert!(p.is_anchored()); Ok(()) },
        invalid { let p: varlit::AnchorPolicy = varlit::AnchorPolicy::NONE; assert!(!p.is_anchored()); Ok(()) }
    ),
    contract!("core/format.rs::format::FormatSpec::field:anchors@7ad1baa29f8db78f",
        valid { let spec = ProbeFormat::spec().with_anchors(SMALL_ANCHORS); assert_eq!(spec.anchors, SMALL_ANCHORS); Ok(()) },
        invalid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert_eq!(ProbeFormat::spec().with_anchors(varlit::AnchorPolicy::NONE).anchors, varlit::AnchorPolicy::NONE); assert!(file.anchor_layout().is_none()); Ok(()) }
    ),
    contract!("core/format.rs::format::FormatSpec::fn:effective_anchors@4f2b04e202a245e6",
        valid { assert_eq!(ProbeFormat::spec().with_anchors(SMALL_ANCHORS).effective_anchors(), SMALL_ANCHORS); Ok(()) },
        invalid { assert_eq!(ProbeFormat::spec().with_anchors(varlit::AnchorPolicy::NONE).effective_anchors(), varlit::AnchorPolicy::NONE); Ok(()) }
    ),
    contract!("core/format.rs::format::FormatSpec::fn:with_anchors@61a2928b117b660b",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert_eq!(ProbeFormat::spec().with_anchors(SMALL_ANCHORS).anchors, SMALL_ANCHORS); assert_eq!(file.anchor_layout().unwrap().block(), 8192); Ok(()) },
        invalid { let d = private_tempdir::private_tempdir()?; assert!(matches!(ProbeFormat::spec().with_anchors(varlit::AnchorPolicy::every(12, false)).create(d.path().join("bad")), Err(varlit::Error::InvalidFormatSpec(_)))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:anchor@54bf9838287ea892",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let a = file.anchor(1)?.expect("an intact anchor"); assert!(a.record_start <= a.logical && a.logical < a.record_end); Ok(()) },
        invalid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let _file = ProbeFormat::spec().open_readonly(&path)?; damage_anchor(&path, 1)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert!(file.anchor(1)?.is_none()); assert!(file.anchor(0)?.is_none()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:anchor_layout@bb110be404740daa",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert_eq!(file.anchor_layout().map(|l| (l.block(), l.hole())), Some((8192, 256))); Ok(()) },
        invalid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert!(file.anchor_layout().is_none()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:seek_sequence@88571b7aa6e03a73",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let from = file.seek_sequence(1_500)?; let mut buffer = Vec::new(); let mut map = file.record_map_from(&mut buffer, from)?; let first = map.advance()?.unwrap(); assert!(first.sequence <= 1_500 && first.sequence > 0); Ok(()) },
        invalid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert!(file.anchor(1)?.is_some()); damage_all_anchors(&path)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert!(matches!(file.seek_sequence(1_999), Err(varlit::Error::InvalidCanonicalEncoding(_)))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:anchor@54bf9838287ea892",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let reader = ProbeFormat::spec().open_reader(&path)?; let a = reader.anchor(1)?.unwrap(); assert_eq!(Some(a), file.anchor(1)?); Ok(()) },
        invalid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let _file = ProbeFormat::spec().open_readonly(&path)?; let reader = ProbeFormat::spec().open_reader(&path)?; assert!(reader.anchor(1)?.is_none()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:anchor_layout@bb110be404740daa",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let reader = ProbeFormat::spec().open_reader(&path)?; assert_eq!(reader.anchor_layout(), file.anchor_layout()); assert!(reader.anchor_layout().is_some()); Ok(()) },
        invalid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let _file = ProbeFormat::spec().open_readonly(&path)?; let reader = ProbeFormat::spec().open_reader(&path)?; assert!(reader.anchor_layout().is_none()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:seek_sequence@88571b7aa6e03a73",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let reader = ProbeFormat::spec().open_reader(&path)?; assert_eq!(reader.seek_sequence(1_500)?, file.seek_sequence(1_500)?); assert!(reader.seek_sequence(1_500)? > reader.seek_sequence(0)?); Ok(()) },
        invalid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let _file = ProbeFormat::spec().open_readonly(&path)?; damage_all_anchors(&path)?; let reader = ProbeFormat::spec().open_reader(&path)?; assert!(reader.seek_sequence(1_999).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:anchor@54bf9838287ea892",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let writer = ProbeFormat::spec().open_writer(&path)?; assert_eq!(writer.anchor(1)?, file.anchor(1)?); assert!(writer.anchor(1)?.is_some()); Ok(()) },
        invalid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let _file = ProbeFormat::spec().open_readonly(&path)?; let writer = ProbeFormat::spec().open_writer(&path)?; assert!(writer.anchor(1)?.is_none()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:anchor_layout@bb110be404740daa",
        valid { let d = private_tempdir::private_tempdir()?; let writer = ProbeFormat::spec().with_anchors(SMALL_ANCHORS).create_writer(d.path().join("w"))?; assert_eq!(writer.anchor_layout().map(|l| l.block()), Some(8192)); Ok(()) },
        invalid { let (_d, path) = anchored_probe(varlit::AnchorPolicy::NONE, 200)?; let _file = ProbeFormat::spec().open_readonly(&path)?; let writer = ProbeFormat::spec().with_anchors(SMALL_ANCHORS).open_writer(&path)?; assert!(writer.anchor_layout().is_none()); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:seek_sequence@88571b7aa6e03a73",
        valid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let file = ProbeFormat::spec().open_readonly(&path)?; let writer = ProbeFormat::spec().open_writer(&path)?; assert_eq!(writer.seek_sequence(1_500)?, file.seek_sequence(1_500)?); Ok(()) },
        invalid { let (_d, path) = anchored_probe(SMALL_ANCHORS, 2_000)?; let _file = ProbeFormat::spec().open_readonly(&path)?; damage_all_anchors(&path)?; let writer = ProbeFormat::spec().open_writer(&path)?; assert!(writer.seek_sequence(1_999).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::RecordIndexEntry::fn:__read_payload@2503a8ab6b803d9b",
        valid { let d = private_tempdir::private_tempdir()?; let path = d.path().join("p"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 7 })?; w.sync()?; } let file = QualificationProbe::open_readonly(&path)?; let entry = crate::pages::all_entries!(file)[0].clone(); let mut out = Vec::new(); file.read_payload_into(&entry, &mut out)?; assert_eq!(entry.__read_payload(QualificationProbe::spec(), &path)?, out); Ok(()) },
        invalid { let d = private_tempdir::private_tempdir()?; let path = d.path().join("p"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 7 })?; w.sync()?; } let file = QualificationProbe::open_readonly(&path)?; let entry = crate::pages::all_entries!(file)[0].clone(); assert!(entry.__read_payload(QualificationProbe::spec(), &d.path().join("missing")).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::RecordIndexEntry::fn:__read_payload_limited@24ccdebab9deb34e",
        valid { let d = private_tempdir::private_tempdir()?; let path = d.path().join("p"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 7 })?; w.sync()?; } let file = QualificationProbe::open_readonly(&path)?; let entry = crate::pages::all_entries!(file)[0].clone(); assert_eq!(entry.__read_payload_limited(QualificationProbe::spec(), &path, entry.payload_len)?.len() as u64, entry.payload_len); Ok(()) },
        invalid { let d = private_tempdir::private_tempdir()?; let path = d.path().join("p"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 7 })?; w.sync()?; } let file = QualificationProbe::open_readonly(&path)?; let entry = crate::pages::all_entries!(file)[0].clone(); assert!(matches!(entry.__read_payload_limited(QualificationProbe::spec(), &path, entry.payload_len - 1), Err(varlit::Error::LimitExceeded { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::RecordIndexEntry::fn:__read_logical_payload_limited@e7510136fe9c9a60",
        valid { let d = private_tempdir::private_tempdir()?; let path = d.path().join("p"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 7 })?; w.sync()?; } let file = QualificationProbe::open_readonly(&path)?; let entry = crate::pages::all_entries!(file)[0].clone(); let mut out = Vec::new(); file.read_logical_payload_into(&entry, &mut out)?; assert_eq!(entry.__read_logical_payload_limited(QualificationProbe::spec(), &path, u64::MAX, u64::MAX)?, out); Ok(()) },
        invalid { let d = private_tempdir::private_tempdir()?; let path = d.path().join("p"); { let mut w = QualificationProbe::create(&path)?; w.push(&Probe { value: 7 })?; w.sync()?; } let file = QualificationProbe::open_readonly(&path)?; let entry = crate::pages::all_entries!(file)[0].clone(); assert!(matches!(entry.__read_logical_payload_limited(QualificationProbe::spec(), &path, u64::MAX, 0), Err(varlit::Error::LimitExceeded { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:take_header_region_writes@e6db8c2392065419",
        valid { #[cfg(feature = "scalable-fault-injection")] { let d = private_tempdir::private_tempdir()?; let _ = varlit::VarlitFile::take_header_region_writes(); { let mut w = TailProbe::create(d.path().join("t"))?; w.push(&Probe { value: 1 })?; w.flush()?; } assert!(varlit::VarlitFile::take_header_region_writes() >= 1); assert_eq!(varlit::VarlitFile::take_header_region_writes(), 0); } Ok(()) },
        invalid { #[cfg(feature = "scalable-fault-injection")] { let d = private_tempdir::private_tempdir()?; let _ = varlit::VarlitFile::take_header_region_writes(); { let mut w = ProbeFormat::create(d.path().join("p"))?; for value in 0..100 { w.push(&Probe { value })?; } w.flush()?; } assert_eq!(varlit::VarlitFile::take_header_region_writes(), 0); } Ok(()) }
    ),
    contract!("core/file/record_file.rs::file::record_file::macro:thread_local@6dda689fe20cb010", generated "take_header_region_writes",
        valid { #[cfg(feature = "scalable-fault-injection")] { let d = private_tempdir::private_tempdir()?; let _ = varlit::VarlitFile::take_header_region_writes(); { let mut w = TailProbe::create(d.path().join("t"))?; w.push(&Probe { value: 1 })?; w.flush()?; } assert!(varlit::VarlitFile::take_header_region_writes() >= 1); } Ok(()) },
        invalid { #[cfg(feature = "scalable-fault-injection")] { let d = private_tempdir::private_tempdir()?; let _ = varlit::VarlitFile::take_header_region_writes(); { let mut w = ProbeFormat::create(d.path().join("p"))?; w.push(&Probe { value: 1 })?; w.flush()?; } assert_eq!(varlit::VarlitFile::take_header_region_writes(), 0); } Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:index_entries@ae1f38692dde64d3",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let page = file.index_entries(&mut buf)?; assert_eq!(page.filled, 7); assert!(page.next.is_some()); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; assert!(matches!(file.index_entries(&mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:index_entries_from@58ec573972cf248d",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let mut page = file.index_entries(&mut buf)?; let mut total = page.filled; let mut probes = buf[..page.filled].iter().filter(|e| e.block_id == <Probe as varlit::VarlitBlock>::ID).count(); while let Some(next) = page.next { page = file.index_entries_from(next, &mut buf)?; total += page.filled; probes += buf[..page.filled].iter().filter(|e| e.block_id == <Probe as varlit::VarlitBlock>::ID).count(); } assert!(total >= 20); assert_eq!(probes, 20); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let far = foreign_cursor()?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; assert!(matches!(file.index_entries_from(far, &mut buf), Err(varlit::Error::InvalidFollowCursor(_)))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:blocks@0bc920bb4f105377",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut out: Vec<Probe> = (0..7).map(|_| <Probe as varlit::VarlitEmpty>::empty()).collect(); let page = file.blocks::<Probe>(&mut Vec::new(), &mut out)?; assert_eq!(page.filled, 7); assert_eq!(out[3], Probe { value: 3 }); assert!(page.next.is_some()); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut out: [Probe; 0] = []; assert!(matches!(file.blocks::<Probe>(&mut Vec::new(), &mut out), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitFile::fn:blocks_from@9e94b153177022d7",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut out: Vec<Probe> = (0..7).map(|_| <Probe as varlit::VarlitEmpty>::empty()).collect(); let mut payload = Vec::new(); let mut page = file.blocks::<Probe>(&mut payload, &mut out)?; let mut values: Vec<u64> = out[..page.filled].iter().map(|p| p.value).collect(); while let Some(next) = page.next { page = file.blocks_from::<Probe>(next, &mut payload, &mut out)?; values.extend(out[..page.filled].iter().map(|p| p.value)); } assert_eq!(values, (0..20).collect::<Vec<_>>()); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let far = foreign_cursor()?; let mut out: Vec<Probe> = (0..7).map(|_| <Probe as varlit::VarlitEmpty>::empty()).collect(); assert!(matches!(file.blocks_from::<Probe>(far, &mut Vec::new(), &mut out), Err(varlit::Error::InvalidFollowCursor(_)))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:index_entries@ae1f38692dde64d3",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_reader(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let page = file.index_entries(&mut buf)?; assert_eq!(page.filled, 7); assert!(page.next.is_some()); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_reader(&path)?; assert!(matches!(file.index_entries(&mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:index_entries_from@58ec573972cf248d",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_reader(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let mut page = file.index_entries(&mut buf)?; let mut total = page.filled; let mut probes = buf[..page.filled].iter().filter(|e| e.block_id == <Probe as varlit::VarlitBlock>::ID).count(); while let Some(next) = page.next { page = file.index_entries_from(next, &mut buf)?; total += page.filled; probes += buf[..page.filled].iter().filter(|e| e.block_id == <Probe as varlit::VarlitBlock>::ID).count(); } assert!(total >= 20); assert_eq!(probes, 20); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_reader(&path)?; let far = foreign_cursor()?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; assert!(matches!(file.index_entries_from(far, &mut buf), Err(varlit::Error::InvalidFollowCursor(_)))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:blocks@0bc920bb4f105377",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_reader(&path)?; let mut out: Vec<Probe> = (0..7).map(|_| <Probe as varlit::VarlitEmpty>::empty()).collect(); let page = file.blocks::<Probe>(&mut Vec::new(), &mut out)?; assert_eq!(page.filled, 7); assert_eq!(out[3], Probe { value: 3 }); assert!(page.next.is_some()); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_reader(&path)?; let mut out: [Probe; 0] = []; assert!(matches!(file.blocks::<Probe>(&mut Vec::new(), &mut out), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitReader::fn:blocks_from@9e94b153177022d7",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_reader(&path)?; let mut out: Vec<Probe> = (0..7).map(|_| <Probe as varlit::VarlitEmpty>::empty()).collect(); let mut payload = Vec::new(); let mut page = file.blocks::<Probe>(&mut payload, &mut out)?; let mut values: Vec<u64> = out[..page.filled].iter().map(|p| p.value).collect(); while let Some(next) = page.next { page = file.blocks_from::<Probe>(next, &mut payload, &mut out)?; values.extend(out[..page.filled].iter().map(|p| p.value)); } assert_eq!(values, (0..20).collect::<Vec<_>>()); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_reader(&path)?; let far = foreign_cursor()?; let mut out: Vec<Probe> = (0..7).map(|_| <Probe as varlit::VarlitEmpty>::empty()).collect(); assert!(matches!(file.blocks_from::<Probe>(far, &mut Vec::new(), &mut out), Err(varlit::Error::InvalidFollowCursor(_)))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:index_entries@ae1f38692dde64d3",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_writer(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let page = file.index_entries(&mut buf)?; assert_eq!(page.filled, 7); assert!(page.next.is_some()); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_writer(&path)?; assert!(matches!(file.index_entries(&mut []), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
    ),
    contract!("core/file.rs::file::VarlitWriter::fn:index_entries_from@58ec573972cf248d",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_writer(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let mut page = file.index_entries(&mut buf)?; let mut total = page.filled; let mut probes = buf[..page.filled].iter().filter(|e| e.block_id == <Probe as varlit::VarlitBlock>::ID).count(); while let Some(next) = page.next { page = file.index_entries_from(next, &mut buf)?; total += page.filled; probes += buf[..page.filled].iter().filter(|e| e.block_id == <Probe as varlit::VarlitBlock>::ID).count(); } assert!(total >= 20); assert_eq!(probes, 20); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_writer(&path)?; let far = foreign_cursor()?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; assert!(matches!(file.index_entries_from(far, &mut buf), Err(varlit::Error::InvalidFollowCursor(_)))); Ok(()) }
    ),
    contract!("core/file.rs::file::Page::field:filled@d978be71270b16c1",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let page: varlit::Page = file.index_entries(&mut buf)?; assert_eq!(page.filled, 7); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 4096]; let page = file.index_entries(&mut buf)?; assert!(page.filled < 4096); assert!(page.next.is_none()); Ok(()) }
    ),
    contract!("core/file.rs::file::Page::field:next@32063ebaab08af5f",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let page: varlit::Page = file.index_entries(&mut buf)?; assert!(page.next.is_some()); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 4096]; let page: varlit::Page = file.index_entries(&mut buf)?; assert!(page.next.is_none()); Ok(()) }
    ),
    contract!("core/file.rs::file::PageCursor::fn:offset@eeb5edcd950f95fa",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let page = file.index_entries(&mut buf)?; let next = page.next.unwrap(); let following = file.index_entries_from(next, &mut buf)?; assert!(following.filled > 0); assert_eq!(buf[0].record_offset, next.offset()); let mut scratch = Vec::new(); let mut map = file.record_map_from(&mut scratch, next.offset())?; assert!(map.advance()?.is_some()); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let far = foreign_cursor()?; assert!(file.record_map_from(&mut Vec::new(), far.offset()).is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::struct:Page@cbf29ce484222325",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let page: varlit::Page = file.index_entries(&mut buf)?; assert_eq!(page.filled, 7); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let page: varlit::Result<varlit::Page> = file.index_entries(&mut []); assert!(page.is_err()); Ok(()) }
    ),
    contract!("core/file.rs::file::struct:PageCursor@cbf29ce484222325",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let page: varlit::Page = file.index_entries(&mut buf)?; let next: varlit::PageCursor = page.next.unwrap(); assert!(file.index_entries_from(next, &mut buf)?.filled > 0); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let far: varlit::PageCursor = foreign_cursor()?; assert!(file.index_entries_from(far, &mut buf_of(7)).is_err()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:Page@7171dd646821ceda",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let page: varlit::Page = file.index_entries(&mut buf)?; assert!(page.next.is_some()); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 4096]; let page: varlit::Page = file.index_entries(&mut buf)?; assert!(page.next.is_none()); Ok(()) }
    ),
    contract!("core/lib.rs::::export:PageCursor@dcbd7132087d2ce8",
        valid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let mut buf = vec![varlit::RecordIndexEntry::default(); 7]; let page: varlit::Page = file.index_entries(&mut buf)?; let next: varlit::PageCursor = page.next.unwrap(); assert!(next.offset() > 0); Ok(()) },
        invalid { let (_d, path) = paged_probe(20)?; let file = ProbeFormat::spec().open_readonly(&path)?; let far: varlit::PageCursor = foreign_cursor()?; assert!(file.index_entries_from(far, &mut buf_of(7)).is_err()); Ok(()) }
    ),
    contract!("macros/modules/format_emit.rs::format_emit::generator:reader_block_methods@88a30e018712ca37", generated "tallies",
        valid { let (_d, path) = paged_dsl(20)?; let reader = PagedDsl::open_reader(&path)?; let mut out: Vec<Tally> = (0..7).map(|_| <Tally as varlit::VarlitEmpty>::empty()).collect(); let mut payload = Vec::new(); let page = reader.tallies(&mut payload, &mut out)?; assert_eq!(page.filled, 7); let page = reader.tallies_from(page.next.unwrap(), &mut payload, &mut out)?; assert_eq!(out[0], Tally { value: 7 }); assert!(page.filled > 0); Ok(()) },
        invalid { let (_d, path) = paged_dsl(20)?; let reader = PagedDsl::open_reader(&path)?; let mut empty: [Tally; 0] = []; assert!(matches!(reader.tallies(&mut Vec::new(), &mut empty), Err(varlit::Error::BufferTooSmall { .. }))); Ok(()) }
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
