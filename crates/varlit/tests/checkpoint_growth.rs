#[path = "support/pages.rs"]
mod pages;

// Writers no longer write index checkpoints. A checkpoint serialized the whole
// resident index into one record, built in memory, which is exactly the
// per-record growth a writer must not have: a billion-record log carried a
// multi-gigabyte checkpoint payload and the directory behind it. Opening fast
// is the segment chain's and the anchors' job now.
//
// What stays is reading: a file written by an earlier build, checkpoints and
// all, opens and reads every record. `fixtures/checkpointed-v0.13-pre.varlit`
// is such a file — 64 records, each flushed, written with the build before
// this change, holding three checkpoints.

use std::fs::remove_file;
use std::path::{Path, PathBuf};

use varlit::{INDEX_BLOCK_ID, VarlitBlock, varlit_format};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 40, version = 1, kind = "fixed")]
struct GrowthBlock {
    value: u32,
}

varlit_format! {
    pub struct GrowthFormat {
        magic: b"GROWTH00";
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
        index: checkpoint_on_flush;
        blocks: [GrowthBlock];
    }
}

/// Writes `records` blocks, flushing after every single append, and returns the
/// resulting file size in bytes.
fn write_flush_every_record(path: &PathBuf, records: u32) -> varlit::Result<u64> {
    {
        let mut file = GrowthFormat::create(path)?;
        for value in 0..records {
            file.push(&GrowthBlock { value })?;
            file.flush()?;
        }
    }
    Ok(std::fs::metadata(path)?.len())
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/checkpointed-v0.13-pre.varlit")
}

#[test]
fn a_checkpoint_format_writer_writes_no_checkpoint() -> varlit::Result<()> {
    let path = temp_path("no_checkpoint");
    cleanup(&path);
    write_flush_every_record(&path, 256)?;
    let file = GrowthFormat::open_readonly(&path)?;
    let entries = crate::pages::all_entries!(file);
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.block_id == INDEX_BLOCK_ID)
            .count(),
        0,
        "a writer must not serialize its index into the file"
    );
    let values: Vec<u32> = crate::pages::all_blocks!(file, GrowthBlock)?
        .iter()
        .map(|block| block.value)
        .collect();
    assert_eq!(values, (0..256).collect::<Vec<_>>());
    Ok(())
}

#[test]
fn the_file_grows_linearly_with_flushes() -> varlit::Result<()> {
    // With no checkpoint, flushing after every record costs the same bytes per
    // record at any size: doubling the records doubles the file.
    let small = temp_path("linear_small");
    let large = temp_path("linear_large");
    cleanup(&small);
    cleanup(&large);
    let at_1k = write_flush_every_record(&small, 1024)?;
    let at_2k = write_flush_every_record(&large, 2048)?;
    assert!(
        at_2k <= at_1k * 2 + 4096,
        "2,048 records took {at_2k} bytes against {at_1k} for 1,024"
    );
    Ok(())
}

#[test]
fn a_file_written_with_checkpoints_still_opens_and_reads() -> varlit::Result<()> {
    let file = GrowthFormat::open_readonly(fixture())?;
    let entries = crate::pages::all_entries!(file);
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.block_id == INDEX_BLOCK_ID)
            .count(),
        3,
        "the fixture is the checkpointed file it claims to be"
    );
    let values: Vec<u32> = crate::pages::all_blocks!(file, GrowthBlock)?
        .iter()
        .map(|block| block.value)
        .collect();
    assert_eq!(values, (0..64).collect::<Vec<_>>());
    Ok(())
}

#[test]
fn a_damaged_old_checkpoint_falls_back_to_a_scan_without_hiding_data() -> varlit::Result<()> {
    let path = temp_path("damaged_old_checkpoint");
    cleanup(&path);
    std::fs::copy(fixture(), &*path)?;
    let checkpoint = crate::pages::all_entries!(GrowthFormat::open_readonly(&path)?)
        .into_iter()
        .find(|entry| entry.block_id == INDEX_BLOCK_ID)
        .expect("the fixture holds a checkpoint");
    let mut bytes = std::fs::read(&*path)?;
    bytes[checkpoint.payload_offset as usize + 30] ^= 0x5a;
    std::fs::write(&*path, bytes)?;
    let file = GrowthFormat::open_readonly(&path)?;
    let values: Vec<u32> = crate::pages::all_blocks!(file, GrowthBlock)?
        .iter()
        .map(|block| block.value)
        .collect();
    assert_eq!(values, (0..64).collect::<Vec<_>>());
    Ok(())
}

#[test]
fn appending_to_a_checkpointed_file_adds_no_checkpoint() -> varlit::Result<()> {
    let path = temp_path("append_old");
    cleanup(&path);
    std::fs::copy(fixture(), &*path)?;
    {
        let mut file = GrowthFormat::open(&path)?;
        for value in 64..128 {
            file.push(&GrowthBlock { value })?;
            file.flush()?;
        }
    }
    let file = GrowthFormat::open_readonly(&path)?;
    let entries = crate::pages::all_entries!(file);
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.block_id == INDEX_BLOCK_ID)
            .count(),
        3,
        "the old checkpoints stay and no new one is written"
    );
    let values: Vec<u32> = crate::pages::all_blocks!(file, GrowthBlock)?
        .iter()
        .map(|block| block.value)
        .collect();
    assert_eq!(values, (0..128).collect::<Vec<_>>());
    Ok(())
}

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
        "varlit_{name}_{}_{}.vrv",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    TempPath { path, _dir: dir }
}

fn cleanup(path: &PathBuf) {
    match remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", path.display()),
    }
    let lock = path.with_extension("vrv.lock");
    match remove_file(&lock) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", lock.display()),
    }
}
