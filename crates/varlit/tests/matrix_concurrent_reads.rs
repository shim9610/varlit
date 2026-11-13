//! Matrix readers own private caches and are Send + !Sync. Each reading
//! thread opens its own reader; regular reads remain &self. Tests cover
//! concurrent correctness, cache eviction without I/O under a cache borrow,
//! and separately reported throughput measurements.

use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::sync::atomic::{AtomicU64, Ordering};

use varlit::{
    BlockDescriptor, BlockKind, Endian, FormatSpec, IndexPolicy, MatrixAuxDescriptor,
    MatrixBlockDescriptor, MatrixCommitDescriptor, MatrixCommitKind, MatrixDimensionDescriptor,
    MatrixDimensions, MatrixKey, ReadLimits, VarlitBlock, VarlitEmpty, VarlitFile,
    VarlitMatrixBlock, VarlitReader, VarlitWriter,
};

/// Owned-result shim over the caller-owned-output `read_matrix_cell`.
trait ReadShared {
    fn read_shared(&self, key: MatrixKey) -> varlit::Result<SharedCell>;
}

macro_rules! read_shared_impl {
    ($($handle:ty),+) => {$(
        impl ReadShared for $handle {
            fn read_shared(&self, key: MatrixKey) -> varlit::Result<SharedCell> {
                let mut out = SharedCell::empty();
                self.read_matrix_cell::<SharedCell>(key, &mut out)?;
                Ok(out)
            }
        }
    )+};
}
read_shared_impl!(VarlitReader, VarlitWriter, VarlitFile);

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, VarlitBlock)]
#[varlit(id = 780, version = 1, kind = "matrix")]
struct SharedCell {
    value: u32,
}

impl VarlitMatrixBlock for SharedCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

fn spec(integrity: varlit::IntegrityPolicy) -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: SharedCell::ID,
        name: "SharedCell",
        version: SharedCell::VERSION,
        kind: BlockKind::Matrix,
        fields: SharedCell::FIELDS,
    }];
    static DIMENSIONS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: SharedCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: SharedCell::ID,
        dimensions: SharedCell::DIMENSIONS,
        category: SharedCell::CATEGORY,
        slot_stride: SharedCell::SLOT_STRIDE,
        tile: [0, 0],
    }];
    static AUX: &[MatrixAuxDescriptor] = &[MatrixAuxDescriptor {
        name: "thumbnail",
        byte_len: 32,
    }];

    FormatSpec::new(
        b"MCON",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        integrity,
        varlit::RecoveryPolicy::Strict,
        varlit::ManifestPolicy::None,
        BLOCKS,
    )
    .with_block_identities(&[SharedCell::IDENTITY])
    .with_matrix_spec(DIMENSIONS, COMMITS, MATRIX_BLOCKS)
    .with_matrix_aux(AUX)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

struct TempMatrix {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TempMatrix {
    fn new(name: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = tempfile::tempdir().expect("create per-test temp directory");
        let path = dir
            .path()
            .join(format!("varlit-{name}-{}-{id}.vrv", std::process::id()));
        Self { path, _dir: dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

const SIDE: u64 = 16;

fn cell_value(scan: u64, ch: u64) -> u32 {
    (scan * SIDE + ch) as u32 + 1
}

/// A `SIDE x SIDE` matrix with every cell written and committed.
fn populated(
    fixture: &TempMatrix,
    integrity: varlit::IntegrityPolicy,
) -> varlit::Result<FormatSpec> {
    let spec = spec(integrity);
    let dimensions = MatrixDimensions::from_pairs([("scan", SIDE), ("ch", SIDE)]);
    let mut writer = spec.create_with_dims(fixture.path(), dimensions)?;
    for scan in 0..SIDE {
        for ch in 0..SIDE {
            let key = MatrixKey::new(scan, ch);
            writer.write_matrix_cell(
                key,
                &SharedCell {
                    value: cell_value(scan, ch),
                },
            )?;
            writer.commit_matrix_cell::<SharedCell>(key)?;
        }
    }
    writer.write_matrix_aux("thumbnail", 0, &[7u8; 32])?;
    writer.sync()?;
    drop(writer);
    Ok(spec)
}

/// Criterion (C), part 1: the handle crosses a thread boundary by shared
/// reference.
///
/// Ownership can move to a thread. Cross-thread sharing is covered by the
/// compile-fail fixtures in compile.rs.
#[test]
fn the_reader_and_writer_handles_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<varlit::VarlitReader>();
    assert_send::<varlit::VarlitFile>();
    assert_send::<varlit::VarlitWriter>();
}

/// Two live bits on each of three bitmap pages make eviction observable:
/// clearing one bit must retain its neighbour even after a dirty page faults in.
fn owned_writer_fixture(
    fixture: &TempMatrix,
    integrity: varlit::IntegrityPolicy,
) -> varlit::Result<FormatSpec> {
    let spec = spec(integrity);
    let mut writer = spec.create_writer_with_dims(
        fixture.path(),
        MatrixDimensions::from_pairs([("scan", 3), ("ch", 4096 * 8)]),
    )?;
    for page in 0..3 {
        for bit in 0..2 {
            let key = MatrixKey::new(page, bit);
            writer.write_matrix_cell(key, &SharedCell { value: 10 })?;
            writer.commit_matrix_cell::<SharedCell>(key)?;
        }
    }
    writer.sync()?;
    drop(writer);
    Ok(spec.with_read_limits(
        ReadLimits::finite_all(u64::MAX)
            .with_matrix_metadata_residency(varlit::MatrixMetadataResidency::Lazy {
                cache_bytes: 4096,
            })
            .with_matrix_generation_cache_bytes(0),
    ))
}

fn owned_writer_integrities() -> &'static [varlit::IntegrityPolicy] {
    &[
        varlit::IntegrityPolicy::None,
        #[cfg(feature = "integrity")]
        varlit::IntegrityPolicy::Crc32,
    ]
}

#[test]
fn dirty_writer_pages_survive_eviction_thread_moves_sync_and_compaction() -> varlit::Result<()> {
    for &integrity in owned_writer_integrities() {
        let fixture = TempMatrix::new("owned-writer");
        let spec = owned_writer_fixture(&fixture, integrity)?;
        let mut old = spec.open_reader(fixture.path())?;
        let mut writer = spec.open_writer(fixture.path())?;
        writer.clear_matrix_cell::<SharedCell>(MatrixKey::new(0, 0))?;
        // Fault page 1, evicting the modified page 0 from the one-page cache.
        assert_eq!(writer.read_shared(MatrixKey::new(1, 0))?.value, 10);

        let mut writer = std::thread::spawn(move || -> varlit::Result<_> {
            assert_eq!(
                writer.matrix_cell_status::<SharedCell>(MatrixKey::new(0, 0))?,
                varlit::MatrixCellStatus::NotCommitted,
            );
            assert_eq!(writer.read_shared(MatrixKey::new(0, 1))?.value, 10);
            assert_eq!(
                writer.matrix_resume_signal(SharedCell::CATEGORY)?,
                varlit::MatrixResumeSignal::Partial {
                    committed: 5,
                    total: 3 * 4096 * 8
                },
            );
            assert!(
                writer
                    .verify_matrix_metadata()?
                    .findings
                    .iter()
                    .all(|finding| {
                        finding.severity == varlit::MatrixCorruptionSeverity::Advisory
                    })
            );
            writer.write_matrix_cell(MatrixKey::new(0, 2), &SharedCell { value: 20 })?;
            writer.commit_matrix_cell::<SharedCell>(MatrixKey::new(0, 2))?;
            writer.read_shared(MatrixKey::new(2, 0))?;
            assert_eq!(writer.read_shared(MatrixKey::new(0, 2))?.value, 20);
            writer.sync()?;
            writer.compact_matrix()?;
            Ok(writer)
        })
        .join()
        .expect("writer thread panicked")?;

        // The pre-publication reader still sees its own generation.
        assert_eq!(old.read_shared(MatrixKey::new(0, 0))?.value, 10);
        old.follow()?;
        assert_eq!(old.read_shared(MatrixKey::new(0, 2))?.value, 20);

        writer.clear_matrix_cell::<SharedCell>(MatrixKey::new(0, 1))?;
        writer.read_shared(MatrixKey::new(1, 0))?;
        assert_eq!(writer.read_shared(MatrixKey::new(0, 2))?.value, 20);
        assert_eq!(
            writer.matrix_cell_status::<SharedCell>(MatrixKey::new(0, 1))?,
            varlit::MatrixCellStatus::NotCommitted,
        );
        assert_eq!(
            writer.matrix_resume_signal(SharedCell::CATEGORY)?,
            varlit::MatrixResumeSignal::Partial {
                committed: 5,
                total: 3 * 4096 * 8
            },
        );
        assert!(
            writer
                .verify_matrix_metadata()?
                .findings
                .iter()
                .all(|finding| { finding.severity == varlit::MatrixCorruptionSeverity::Advisory })
        );
        writer.sync()?;
        old.follow()?;
        assert_eq!(
            old.matrix_cell_status::<SharedCell>(MatrixKey::new(0, 1))?,
            varlit::MatrixCellStatus::NotCommitted,
        );
        assert_eq!(old.read_shared(MatrixKey::new(0, 2))?.value, 20);
    }
    Ok(())
}

/// Criterion (C), part 2: the read entry points compile against a shared
/// borrow.
///
/// This would not compile at all before this round. It is written as a
/// free-standing function taking `&VarlitReader` precisely so that reintroducing
/// `&mut self` on any of these entry points is a build failure, not a
/// silently-accepted regression.
fn read_everything(reader: &varlit::VarlitReader, key: MatrixKey) -> varlit::Result<u32> {
    let typed = reader.read_shared(key)?;
    let payload = reader.matrix_cell_payload::<SharedCell>(key)?;
    let aux = reader.read_matrix_aux("thumbnail", 0, 8)?;
    let aux_len = reader.matrix_aux_len("thumbnail")?;
    let status = reader.matrix_cell_status::<SharedCell>(key)?;
    assert_eq!(payload.len(), 4);
    assert_eq!(aux, vec![7u8; 8]);
    assert_eq!(aux_len, 32);
    assert_eq!(status, varlit::MatrixCellStatus::Committed);
    Ok(typed.value)
}

/// The same five entry points on the *writer*, which is the handle no test
/// pinned before: a `&mut self` reintroduced there would not be caught by the
/// reader-only function above.
fn read_everything_through_writer(
    writer: &varlit::VarlitWriter,
    key: MatrixKey,
) -> varlit::Result<u32> {
    let typed = writer.read_shared(key)?;
    let payload = writer.matrix_cell_payload::<SharedCell>(key)?;
    let aux = writer.read_matrix_aux("thumbnail", 0, 8)?;
    let aux_len = writer.matrix_aux_len("thumbnail")?;
    let status = writer.matrix_cell_status::<SharedCell>(key)?;
    assert_eq!(payload.len(), 4);
    assert_eq!(aux, vec![7u8; 8]);
    assert_eq!(aux_len, 32);
    assert_eq!(status, varlit::MatrixCellStatus::Committed);
    Ok(typed.value)
}

/// And on the general handle.
fn read_everything_through_file(file: &varlit::VarlitFile, key: MatrixKey) -> varlit::Result<u32> {
    let typed = file.read_shared(key)?;
    let payload = file.matrix_cell_payload::<SharedCell>(key)?;
    let aux = file.read_matrix_aux("thumbnail", 0, 8)?;
    let aux_len = file.matrix_aux_len("thumbnail")?;
    let status = file.matrix_cell_status::<SharedCell>(key)?;
    assert_eq!(payload.len(), 4);
    assert_eq!(aux, vec![7u8; 8]);
    assert_eq!(aux_len, 32);
    assert_eq!(status, varlit::MatrixCellStatus::Committed);
    Ok(typed.value)
}

#[test]
fn every_matrix_read_entry_point_takes_a_shared_borrow() -> varlit::Result<()> {
    let fixture = TempMatrix::new("shared-borrow");
    let spec = populated(&fixture, varlit::IntegrityPolicy::None)?;
    let reader = spec.open_reader(fixture.path())?;
    // Note: `reader` is not `mut`. Two shared borrows are alive at once.
    let first = &reader;
    let second = &reader;
    assert_eq!(
        read_everything(first, MatrixKey::new(0, 0))?,
        cell_value(0, 0)
    );
    assert_eq!(
        read_everything(second, MatrixKey::new(3, 5))?,
        cell_value(3, 5)
    );
    drop(reader);

    // 5 entry points x 3 handle types = 15 signatures, all `&self`.
    let file = spec.open_readonly(fixture.path())?;
    assert_eq!(
        read_everything_through_file(&file, MatrixKey::new(1, 1))?,
        cell_value(1, 1)
    );
    drop(file);

    let writer = spec.open_writer(fixture.path())?;
    assert_eq!(
        read_everything_through_writer(&writer, MatrixKey::new(2, 4))?,
        cell_value(2, 4)
    );
    Ok(())
}

/// Each thread owns a reader and verifies every cell at interleaved offsets.
#[test]
fn independent_readers_read_every_cell_correctly() -> varlit::Result<()> {
    // `Crc32` is only constructible into a working spec when the `integrity`
    // feature is on; without it every checksummed read fails closed with
    // `IntegrityFeatureDisabled`, so the default-feature run covers `None` only.
    let policies: &[varlit::IntegrityPolicy] = if cfg!(feature = "integrity") {
        &[
            varlit::IntegrityPolicy::None,
            varlit::IntegrityPolicy::Crc32,
        ]
    } else {
        &[varlit::IntegrityPolicy::None]
    };
    for &integrity in policies {
        let fixture = TempMatrix::new("concurrent-cells");
        let spec = populated(&fixture, integrity)?;

        const THREADS: usize = 4;
        let barrier = Barrier::new(THREADS);
        let barrier = &barrier;

        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                let reader = spec
                    .open_reader(fixture.path())
                    .expect("independent reader");
                scope.spawn(move || {
                    barrier.wait();
                    // Each thread walks the matrix from a different starting
                    // offset so no two threads are ever addressing the same
                    // slot at the same moment.
                    for step in 0..(SIDE * SIDE) {
                        let ordinal = (step + (thread as u64) * 7) % (SIDE * SIDE);
                        let (scan, ch) = (ordinal / SIDE, ordinal % SIDE);
                        let key = MatrixKey::new(scan, ch);
                        let cell = reader.read_shared(key).expect("concurrent typed read");
                        assert_eq!(
                            cell.value,
                            cell_value(scan, ch),
                            "thread {thread} read the wrong slot for ({scan},{ch}); \
                             a shared file cursor would produce exactly this"
                        );
                        let payload = reader
                            .matrix_cell_payload::<SharedCell>(key)
                            .expect("concurrent payload read");
                        assert_eq!(
                            u32::from_le_bytes(payload.try_into().expect("4-byte slot")),
                            cell_value(scan, ch)
                        );
                    }
                });
            }
        });
    }
    Ok(())
}

/// Bytes in one commit-map page. Not a public constant, and deliberately not
/// derived from one: the fixture below is built to straddle exactly this
/// boundary, so if the internal page size ever changes the fixture stops being
/// several pages wide and the warm-up assertion says so.
#[cfg(feature = "scalable-fault-injection")]

/// Cells per commit-map page: the map holds one bit per cell.
#[cfg(feature = "scalable-fault-injection")]

/// Live commit-map pages in the paged fixture. Four is enough for a one-page
/// cache to miss on essentially every read while keeping the fixture cheap to
/// build: four writes and four commits.
#[cfg(feature = "scalable-fault-injection")]

/// Fixed-work scaling with one independent reader per thread. Timing is
/// printed rather than asserted on shared CI hardware; every value is checked.
#[test]
fn report_the_scaling_of_independent_handles() -> varlit::Result<()> {
    let Some((serial, parallel, threads)) = measure_independent_reader_scaling()? else {
        println!("single-core host: nothing to compare, skipping the measurement");
        return Ok(());
    };
    let ratio = parallel.as_secs_f64() / serial.as_secs_f64();
    println!(
        "{TOTAL_READS} reads through independent handles: 1 thread {:.3}s, {threads} threads {:.3}s \
         (ratio {ratio:.2}x, lower is better; MEASUREMENT ONLY — not a gate, see the module \
         header)",
        serial.as_secs_f64(),
        parallel.as_secs_f64()
    );
    Ok(())
}

/// The strict scaling threshold, for a machine whose cores are actually yours.
///
/// `#[ignore]`d because it measures the host as much as the code: it is the gate
/// that CI could not keep. Run it deliberately —
/// `cargo test -p varlit --test matrix_concurrent_reads -- --ignored --nocapture`
/// — on an idle host when changing the matrix read path, and expect well under
/// 1.0x. A number above 1.0x on an idle host means N threads owning separate readers
/// are slower than one thread, which is a convoy worth investigating even though
/// it cannot be distinguished from load on a shared runner.
#[test]
#[ignore = "wall-clock scaling: measures the host, so it is a manual benchmark rather than a gate"]
fn independent_reader_scaling_beats_one_thread_on_an_idle_host() -> varlit::Result<()> {
    let Some((serial, parallel, threads)) = measure_independent_reader_scaling()? else {
        println!("single-core host: nothing to compare, skipping");
        return Ok(());
    };
    let ratio = parallel.as_secs_f64() / serial.as_secs_f64();
    println!(
        "{TOTAL_READS} reads through independent handles: 1 thread {:.3}s, {threads} threads {:.3}s \
         (ratio {ratio:.2}x)",
        serial.as_secs_f64(),
        parallel.as_secs_f64()
    );
    assert!(
        ratio <= 1.0,
        "{threads} threads with independent readers took {ratio:.2}x as long as one thread for the \
         same {TOTAL_READS} reads. On an idle host that is a convoy; on a shared runner it may \
         only be the runner, which is why this test is `#[ignore]`d"
    );
    Ok(())
}

/// Total work is held constant; only the number of threads it is split across
/// changes. Anything else compares two different amounts of work.
const TOTAL_READS: u64 = 24_000;

/// `(one thread, N threads, N)`, or `None` on a single-core host.
fn measure_independent_reader_scaling()
-> varlit::Result<Option<(std::time::Duration, std::time::Duration, usize)>> {
    let threads = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(4);
    if threads < 2 {
        return Ok(None);
    }
    let fixture = TempMatrix::new("scaling");
    let spec = populated(&fixture, varlit::IntegrityPolicy::None)?;
    // Best-of-three on each configuration: taking the minimum removes some
    // scheduler noise without inventing statistics.
    let serial = best_of_three(spec, fixture.path(), 1, TOTAL_READS);
    let parallel = best_of_three(spec, fixture.path(), threads, TOTAL_READS);
    Ok(Some((serial, parallel, threads)))
}

/// Wall-clock time for `total_reads` cell reads split across `threads` threads,
/// each owning its reader; the fastest of three attempts.
fn best_of_three(
    spec: FormatSpec,
    path: &Path,
    threads: usize,
    total_reads: u64,
) -> std::time::Duration {
    (0..3)
        .map(|_| timed_reads(spec, path, threads, total_reads))
        .min()
        .expect("three attempts")
}

fn timed_reads(
    spec: FormatSpec,
    path: &Path,
    threads: usize,
    total_reads: u64,
) -> std::time::Duration {
    let per_thread = total_reads / threads as u64;
    let ready = Barrier::new(threads + 1);
    let ready = &ready;
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|thread| {
                let reader = spec.open_reader(path).expect("independent reader");
                scope.spawn(move || {
                    // Every thread is spawned and warm before the clock starts,
                    // so thread creation is not charged to the parallel run.
                    ready.wait();
                    for step in 0..per_thread {
                        let ordinal = (step + (thread as u64) * 13) % (SIDE * SIDE);
                        let key = MatrixKey::new(ordinal / SIDE, ordinal % SIDE);
                        let cell = reader.read_shared(key).expect("concurrent read");
                        assert_eq!(cell.value, cell_value(ordinal / SIDE, ordinal % SIDE));
                    }
                })
            })
            .collect();
        ready.wait();
        let start = std::time::Instant::now();
        for worker in workers {
            worker.join().expect("reader thread");
        }
        start.elapsed()
    })
}
