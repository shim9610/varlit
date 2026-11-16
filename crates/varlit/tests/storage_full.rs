//! Storage that fills up under concurrent maintenance jobs and readers.
//!
//! A child process runs with a file-size limit (`ulimit -f`) and SIGXFSZ
//! ignored, so every write past the limit fails with an error instead of
//! killing the process: the same path a full disk takes through the library.
//! Several keyed merges run at once, each with its own output and scratch,
//! while readers keep reading the inputs. Every job must fail with an error,
//! leave no published output and no scratch behind, and leave the inputs
//! unchanged; the same jobs must then succeed once space is available again.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

use varlit::{VarlitBlock, VarlitMerge, merge_keyed_files, varlit_format};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 2, version = 1, kind = "variable", key = "fixture_key")]
struct Entry {
    #[varlit(field_id = 1)]
    fixture_key: u64,
    #[varlit(field_id = 2)]
    payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 3, version = 1, kind = "variable")]
struct Replace {
    #[varlit(field_id = 1)]
    payload: Vec<u8>,
}

impl VarlitMerge for Entry {
    type Op = Replace;

    fn apply_op(&mut self, op: Self::Op) -> varlit::Result<()> {
        self.payload = op.payload;
        Ok(())
    }
}

varlit_format! {
    pub struct FullDisk {
        magic: b"TFULLD";
        version: 1;
        endian: little;
        blocks: [Entry, Replace];
    }
}

const CHILD: &str = "VARLIT_STORAGE_FULL_CHILD";
const JOBS: usize = 4;
const RECORDS: u64 = 2_000;
const PAYLOAD: usize = 512;
/// The child's file-size limit, in `ulimit -f` blocks of 1024 bytes (bash) or
/// 512 bytes (dash). Either way it is far below one merged output, about
/// 1 MiB, and above nothing the child needs before the merges start.
const LIMIT_BLOCKS: u64 = 256;

fn payload(key: u64, generation: u8) -> Vec<u8> {
    (0..PAYLOAD)
        .map(|i| (key as usize + i) as u8 ^ generation)
        .collect()
}

fn write_inputs(directory: &Path) -> varlit::Result<(PathBuf, PathBuf)> {
    let base = directory.join("base.vlt");
    let delta = directory.join("delta.vlt");
    let mut file = FullDisk::create(&base)?;
    for key in 0..RECORDS {
        file.push(&Entry {
            fixture_key: key,
            payload: payload(key, 0),
        })?;
    }
    file.flush()?;
    drop(file);
    let mut file = FullDisk::create(&delta)?;
    for key in (0..RECORDS).step_by(3) {
        file.push_op::<Entry>(
            &key,
            &Replace {
                payload: payload(key, 1),
            },
        )?;
    }
    for key in (1..RECORDS).step_by(7) {
        file.delete::<Entry>(&key)?;
    }
    file.flush()?;
    Ok((base, delta))
}

fn output(directory: &Path, job: usize) -> PathBuf {
    directory.join(format!("merged-{job}.vlt"))
}

/// Run every merge at once while readers walk the inputs. Returns each
/// merge's result.
fn merge_concurrently(directory: &Path) -> Vec<varlit::Result<()>> {
    let base = directory.join("base.vlt");
    let delta = directory.join("delta.vlt");
    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| -> varlit::Result<u64> {
                    let mut seen = 0;
                    for _ in 0..4 {
                        let file = FullDisk::open_readonly(&base)?;
                        seen += file.keyed_blocks::<Entry>()?.len() as u64;
                    }
                    Ok(seen)
                })
            })
            .collect();
        let jobs: Vec<_> = (0..JOBS)
            .map(|job| {
                let (base, delta) = (&base, &delta);
                scope.spawn(move || {
                    merge_keyed_files::<Entry, _>(
                        FullDisk::spec(),
                        base,
                        &[delta],
                        &output(directory, job),
                    )
                })
            })
            .collect();
        for reader in readers {
            assert_eq!(
                reader
                    .join()
                    .expect("reader panicked")
                    .expect("reader failed"),
                4 * RECORDS
            );
        }
        jobs.into_iter()
            .map(|job| job.join().expect("a merge panicked"))
            .collect()
    })
}

fn digest(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

#[test]
fn child_merges_on_a_full_disk() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let directory = PathBuf::from(std::env::var_os("VARLIT_STORAGE_FULL_DIR").unwrap());
    for (job, result) in merge_concurrently(&directory).into_iter().enumerate() {
        let error = result.expect_err("a merge succeeded past the file-size limit");
        println!("job {job}: {error}");
    }
}

#[test]
fn concurrent_merges_fail_cleanly_on_a_full_disk_and_succeed_after() -> varlit::Result<()> {
    if std::env::var_os(CHILD).is_some() {
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let spill = tempfile::tempdir()?;
    let (base, delta) = write_inputs(directory.path())?;
    let before = (digest(&base), digest(&delta));

    let status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "trap '' XFSZ; ulimit -f {LIMIT_BLOCKS}; exec \"$0\" \"$@\""
        ))
        .arg(std::env::current_exe()?)
        .args(["--exact", "child_merges_on_a_full_disk", "--nocapture"])
        .env(CHILD, "1")
        .env("VARLIT_STORAGE_FULL_DIR", directory.path())
        .env("VARLIT_SPILL_DIR", spill.path())
        .status()?;
    assert!(status.success(), "the child failed or was killed: {status}");

    // Nothing was published, no scratch was left, and the inputs are intact.
    for job in 0..JOBS {
        assert!(
            !output(directory.path(), job).exists(),
            "job {job} published an output despite failing"
        );
    }
    let leftovers: Vec<_> = std::fs::read_dir(spill.path())?.collect();
    assert!(leftovers.is_empty(), "scratch left behind: {leftovers:?}");
    assert_eq!((digest(&base), digest(&delta)), before);
    let mut names: Vec<_> = std::fs::read_dir(directory.path())?
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert!(
        names
            .iter()
            .all(|name| name == "base.vlt" || name == "delta.vlt" || name.ends_with(".lock")),
        "a failed merge left files beside its output: {names:?}"
    );

    // With space again, the same jobs succeed and agree.
    for result in merge_concurrently(directory.path()) {
        result?;
    }
    let merged = digest(&output(directory.path(), 0));
    for job in 1..JOBS {
        assert_eq!(digest(&output(directory.path(), job)), merged);
    }
    let reader = FullDisk::open_readonly(output(directory.path(), 0))?;
    let entries = reader.keyed_blocks::<Entry>()?;
    let expected = (0..RECORDS).filter(|key| key % 7 != 1).count();
    assert_eq!(entries.len(), expected);
    Ok(())
}
