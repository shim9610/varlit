#![cfg(feature = "experimental-matrix-reuse")]
use stratava::experimental_matrix::{MatrixOptions, MatrixReader, MatrixReuseError, MatrixWriter};
use stratava::{FormatSpec, MatrixDimensions, MatrixKey, stratava_format};
stratava_format! {
    pub format ReuseFormat {
        magic: b"REUSETS1"; version:1; endian:little;
        dims {row:u32,col:u32,}
        commit: cell_bitmap {keyspace=[row,col];categories=[data,meta];};
        blocks {
            matrix Value(id=1,dims=[row,col],category=data) {epoch:u64,row:u64,col:u64,pad:[u8;40],}
            matrix Meta(id=2,dims=[row,col],category=meta) {epoch:u64,}
        }
    }
}
fn spec() -> FormatSpec {
    ReuseFormat::spec().with_growing_matrix_dimension("row", 4)
}
fn dims() -> MatrixDimensions {
    MatrixDimensions::from_pairs([("row", 4), ("col", 3)])
}
fn opts() -> MatrixOptions {
    MatrixOptions {
        reuse: true,
        rows_per_chunk: 4,
        ..MatrixOptions::default()
    }
}
fn key(i: u64) -> MatrixKey {
    MatrixKey::new(i / 3, i % 3)
}
fn value(i: u64, epoch: u64) -> Value {
    Value {
        epoch,
        row: i / 3,
        col: i % 3,
        pad: [epoch as u8; 40],
    }
}
fn fill(w: &mut MatrixWriter, n: u64, epoch: u64) {
    for i in 0..n {
        w.put(key(i), &value(i, epoch)).unwrap();
        w.put(key(i), &Meta { epoch }).unwrap();
    }
    w.sync().unwrap();
}
fn verify(r: &MatrixReader, n: u64, epoch: u64) {
    let mut out = vec![value(0, 0); n as usize];
    r.read_run_into(key(0), &mut out).unwrap();
    for (i, v) in out.iter().enumerate() {
        assert_eq!((v.epoch, v.row, v.col), (epoch, i as u64 / 3, i as u64 % 3));
        assert_eq!(v.pad, [epoch as u8; 40]);
    }
}
#[test]
fn reuse_is_opt_in_and_off_keeps_native_snapshots() {
    assert!(!MatrixOptions::default().reuse);
    for reuse in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let options = MatrixOptions { reuse, ..opts() };
        let mut w = MatrixWriter::create(spec(), &path, dims(), options).unwrap();
        fill(&mut w, 36, 1);
        let mut r = w.reader().unwrap();
        fill(&mut w, 36, 2);
        verify(&r, 36, if reuse { 2 } else { 1 });
        r.follow().unwrap();
        verify(&r, 36, 2);
        assert!(
            MatrixReader::open(
                spec(),
                &path,
                dims(),
                MatrixOptions {
                    reuse: !reuse,
                    ..options
                }
            )
            .is_err()
        );
    }
}
#[test]
fn repeated_updates_reuse_slots_and_dirty_readers_see_confirmed_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
    fill(&mut w, 36, 1);
    fill(&mut w, 36, 2);
    let limit = std::fs::metadata(&path).unwrap().len();
    let r = w.reader().unwrap();
    for epoch in 3..131 {
        for i in 0..36 {
            w.put(key(i), &value(i, epoch)).unwrap();
        }
        verify(&r, 36, epoch - 1);
        let dirty = MatrixReader::open(spec(), &path, dims(), opts()).unwrap();
        verify(&dirty, 36, epoch - 1);
        w.sync().unwrap();
        verify(&r, 36, epoch);
        verify(&dirty, 36, epoch);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), limit);
        // The unrelated second block survives reuse of the same chunk.
        assert_eq!(r.get::<Meta>(key(20)).unwrap().epoch, 2);
    }
    w.sync().unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), limit);
    drop(w);
    let w = MatrixWriter::open(spec(), &path, dims(), opts()).unwrap();
    verify(&w.reader().unwrap(), 36, 130);
    for suffix in [".reuse-work", ".reuse-journal"] {
        assert_eq!(
            std::fs::metadata(format!("{}{suffix}", path.display()))
                .unwrap()
                .len(),
            4096
        );
    }
}
#[test]
fn unsynced_drop_is_discarded_and_deleted_cells_stay_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
    fill(&mut w, 36, 1);
    for i in 0..36 {
        w.put(key(i), &value(i, 2)).unwrap();
    }
    drop(w);
    let mut w = MatrixWriter::open(spec(), &path, dims(), opts()).unwrap();
    let r = w.reader().unwrap();
    verify(&r, 36, 1);
    w.delete::<Value>(key(13)).unwrap();
    assert_eq!(r.get::<Value>(key(13)).unwrap().epoch, 1);
    w.sync().unwrap();
    assert!(matches!(
        r.get::<Value>(key(13)),
        Err(MatrixReuseError::Missing)
    ));
    w.put(key(35), &value(35, 7)).unwrap();
    w.sync().unwrap();
    assert!(matches!(
        r.get::<Value>(key(13)),
        Err(MatrixReuseError::Missing)
    ));
    assert_eq!(r.get::<Value>(key(0)).unwrap().epoch, 1);
}
#[test]
fn bounds_geometry_and_single_writer_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let fixed = ReuseFormat::spec();
    let mut w = MatrixWriter::create(fixed, &path, dims(), opts()).unwrap();
    assert!(MatrixWriter::open(fixed, &path, dims(), opts()).is_err());
    assert!(w.put(key(12), &value(12, 1)).is_err());
    fill(&mut w, 12, 1);
    assert!(
        MatrixReader::open(
            fixed,
            &path,
            dims(),
            MatrixOptions {
                rows_per_chunk: 8,
                ..opts()
            }
        )
        .is_err()
    );
    let r = MatrixReader::open(
        fixed,
        &path,
        dims(),
        MatrixOptions {
            cache_bytes: 0,
            ..opts()
        },
    )
    .unwrap();
    verify(&r, 12, 1);
    let mut out = vec![value(0, 0); 13];
    assert!(r.read_run_into(key(0), &mut out).is_err());
    assert_eq!(out[0].epoch, 0);
    assert!(r.get::<Value>(MatrixKey::new(u64::MAX, 0)).is_err());
    assert!(
        MatrixWriter::create(
            fixed,
            dir.path().join("small"),
            dims(),
            MatrixOptions {
                max_chunk_bytes: 1,
                ..opts()
            }
        )
        .is_err()
    );
}
#[test]
fn payload_corruption_is_not_hidden_by_an_older_slot() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
    fill(&mut w, 12, 1);
    fill(&mut w, 12, 2);
    drop(w);
    // The first value page follows the bitmap page; generation 2 is in slot 1.
    let stride = 64 + 4096;
    let at = 3 * 4096 + 3 * stride + 64;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.seek(SeekFrom::Start(at)).unwrap();
    let mut byte = [0];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 1;
    file.seek(SeekFrom::Start(at)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
    let r = MatrixReader::open(spec(), &path, dims(), opts()).unwrap();
    assert!(r.get::<Value>(key(0)).is_err());
}
#[test]
fn model_histories_match_across_evictions_deletes_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
    let mut state = [None; 96];
    let mut seed = 0xbad5eedu64;
    for epoch in 1..81 {
        for _ in 0..33 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let i = (seed >> 32) as usize % 96;
            if seed & 7 == 0 && state[i].is_some() {
                w.delete::<Value>(key(i as u64)).unwrap();
                state[i] = None;
            } else {
                w.put(key(i as u64), &value(i as u64, epoch)).unwrap();
                state[i] = Some(epoch);
            }
        }
        w.sync().unwrap();
        let r = w.reader().unwrap();
        for (i, expected) in state.iter().enumerate() {
            match expected {
                Some(e) => assert_eq!(r.get::<Value>(key(i as u64)).unwrap().epoch, *e),
                None => assert!(matches!(
                    r.get::<Value>(key(i as u64)),
                    Err(MatrixReuseError::Missing)
                )),
            }
        }
        if epoch % 7 == 0 {
            drop(w);
            w = MatrixWriter::open(spec(), &path, dims(), opts()).unwrap();
        }
    }
}
#[test]
fn independent_threads_observe_whole_generations() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
    fill(&mut w, 96, 1);
    let stop = Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        let mut joins = Vec::new();
        for _ in 0..4 {
            let reader = w.reader().unwrap();
            let stop = stop.clone();
            joins.push(scope.spawn(move || {
                let mut reads = 0;
                while !stop.load(Ordering::Acquire) {
                    let mut out = vec![value(0, 0); 96];
                    match reader.read_run_into(key(0), &mut out) {
                        Ok(()) => {
                            let epoch = out[0].epoch;
                            for (i, v) in out.iter().enumerate() {
                                assert_eq!(
                                    (v.epoch, v.row, v.col),
                                    (epoch, i as u64 / 3, i as u64 % 3)
                                );
                                assert_eq!(v.pad, [epoch as u8; 40]);
                            }
                            reads += 1;
                        }
                        Err(MatrixReuseError::Retry { .. }) => {}
                        Err(e) => panic!("{e:?}"),
                    }
                }
                reads
            }));
        }
        for epoch in 2..102 {
            fill(&mut w, 96, epoch);
        }
        stop.store(true, Ordering::Release);
        assert!(joins.into_iter().map(|j| j.join().unwrap()).sum::<usize>() > 0);
    });
}
#[cfg(feature = "scalable-fault-injection")]
mod fault_tests {
    use super::*;
    use stratava::experimental_matrix::set_test_hook;
    #[test]
    fn a_paused_reader_does_not_block_reuse_and_discards_its_old_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
        fill(&mut w, 36, 1);
        let reader = w.reader().unwrap();
        let (copied_tx, copied_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let child = std::thread::spawn(move || {
            let mut first = true;
            set_test_hook(Some(Box::new(move |point| {
                if point == "matrix_reuse.read_copied" && first {
                    first = false;
                    copied_tx.send(()).unwrap();
                    resume_rx
                        .recv_timeout(std::time::Duration::from_secs(20))
                        .unwrap();
                }
                Ok(())
            })));
            verify(&reader, 36, 33);
            set_test_hook(None);
        });
        copied_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap();
        // The reader cannot finish until all these publications complete.
        for epoch in 2..34 {
            fill(&mut w, 36, epoch);
        }
        resume_tx.send(()).unwrap();
        child.join().unwrap();
    }
    #[test]
    fn injected_sync_failure_poisoning_and_recovery_keep_a_confirmed_generation() {
        for point in [
            "matrix_reuse.intent_durable",
            "matrix_reuse.payload_written",
            "matrix_reuse.slot_written",
            "matrix_reuse.data_durable",
            "matrix_reuse.head_written",
            "matrix_reuse.head_durable",
            "matrix_reuse.reset",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("data");
            let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
            fill(&mut w, 36, 1);
            fill(&mut w, 36, 2);
            for i in 0..36 {
                w.put(key(i), &value(i, 3)).unwrap();
            }
            set_test_hook(Some(Box::new(move |p| {
                if p == point {
                    Err(std::io::Error::other("injected"))
                } else {
                    Ok(())
                }
            })));
            assert!(w.sync().is_err());
            set_test_hook(None);
            assert!(matches!(
                w.put(key(0), &value(0, 4)),
                Err(MatrixReuseError::Poisoned)
            ));
            drop(w);
            let mut w = MatrixWriter::open(spec(), &path, dims(), opts()).unwrap();
            let r = w.reader().unwrap();
            let epoch = r.get::<Value>(key(0)).unwrap().epoch;
            assert!(epoch == 2 || epoch == 3);
            verify(&r, 36, epoch);
            w.put(key(35), &value(35, 4)).unwrap();
            w.sync().unwrap();
            assert_eq!(r.get::<Value>(key(0)).unwrap().epoch, epoch);
        }
    }
    #[test]
    #[ignore = "subprocess entry point"]
    fn crash_child() {
        let path = std::env::var_os("STRATAVA_REUSE_TEST_PATH").unwrap();
        let mut w = MatrixWriter::open(spec(), PathBuf::from(path), dims(), opts()).unwrap();
        for i in 0..36 {
            w.put(key(i), &value(i, 3)).unwrap();
        }
        let _armed = stratava::scalable_fault::arm_from_env().unwrap();
        w.sync().unwrap();
        panic!("fault point did not abort");
    }
    use std::path::PathBuf;
    #[test]
    fn process_abort_matrix_recovers_and_never_resurrects_discarded_chunks() {
        for (point, occurrence) in stratava::scalable_fault::MATRIX_REUSE_POINTS
            .iter()
            .filter(|p| !p.ends_with("read_copied") && !p.ends_with("page_copied"))
            .flat_map(|point| {
                let hits = if point.ends_with("payload_written")
                    || point.ends_with("slot_written")
                    || point.contains("retirement")
                {
                    3
                } else {
                    1
                };
                (1..=hits).map(move |hit| (*point, hit))
            })
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("data");
            let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
            fill(&mut w, 36, 1);
            fill(&mut w, 36, 2);
            drop(w);
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "fault_tests::crash_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("STRATAVA_REUSE_TEST_PATH", &path)
                .env(
                    stratava::scalable_fault::FAULT_ENV,
                    format!("abort:{point}:{occurrence}"),
                )
                .env(
                    stratava::scalable_fault::TRACE_ENV,
                    dir.path().join("trace"),
                )
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(
                std::fs::read_to_string(dir.path().join("trace"))
                    .unwrap()
                    .lines()
                    .filter(|line| line.contains(point))
                    .count()
                    >= occurrence,
                "missing fault {point}/{occurrence}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let before = MatrixReader::open(spec(), &path, dims(), opts()).unwrap();
            let epoch = before.get::<Value>(key(0)).unwrap().epoch;
            assert!(epoch == 2 || epoch == 3);
            verify(&before, 36, epoch);
            let mut w = MatrixWriter::open(spec(), &path, dims(), opts()).unwrap();
            verify(&w.reader().unwrap(), 36, epoch);
            // Commit a different chunk after rollback; abandoned generation 3
            // must never become eligible just because the counter reaches 3.
            w.put(key(35), &value(35, 4)).unwrap();
            w.sync().unwrap();
            let r = w.reader().unwrap();
            assert_eq!(r.get::<Value>(key(0)).unwrap().epoch, epoch);
        }
    }
}

#[test]
fn erased_latest_header_and_truncation_do_not_return_old_values() {
    use std::io::{Seek, SeekFrom, Write};
    for truncate in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
        fill(&mut w, 12, 1);
        fill(&mut w, 12, 2);
        drop(w);
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let stride = 64 + 4096;
        if truncate {
            f.set_len(3 * 4096 + 3 * stride + 8).unwrap();
        } else {
            f.seek(SeekFrom::Start(3 * 4096 + 3 * stride)).unwrap();
            f.write_all(&[0; 64]).unwrap();
        }
        drop(f);
        match MatrixReader::open(spec(), &path, dims(), opts()) {
            Ok(r) => assert!(r.get::<Value>(key(0)).is_err()),
            Err(_) => assert!(truncate),
        }
    }
}
#[test]
fn aliased_sidecars_are_refused_without_truncating_the_primary() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
    fill(&mut w, 36, 1);
    drop(w);
    let work = dir.path().join("data.reuse-work");
    std::fs::remove_file(&work).unwrap();
    std::fs::hard_link(&path, &work).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(MatrixWriter::open(spec(), &path, dims(), opts()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
#[test]
#[ignore = "cross-process reader entry point"]
fn process_reader_child() {
    let path = std::path::PathBuf::from(std::env::var_os("STRATAVA_REUSE_READER_PATH").unwrap());
    let r = MatrixReader::open(spec(), &path, dims(), opts()).unwrap();
    let ready = path.with_extension("ready");
    let done = path.with_extension("done");
    std::fs::write(&ready, b"ready").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut reads = 0;
    while !done.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "writer did not finish"
        );
        let mut out = vec![value(0, 0); 96];
        match r.read_run_into(key(0), &mut out) {
            Ok(()) => {
                let epoch = out[0].epoch;
                assert!(
                    out.iter()
                        .all(|v| v.epoch == epoch && v.pad == [epoch as u8; 40])
                );
                reads += 1;
            }
            Err(MatrixReuseError::Retry { .. }) => {}
            Err(e) => panic!("{e:?}"),
        }
    }
    verify(&r, 96, 202);
    assert!(reads > 0);
}
#[test]
fn separately_opened_process_reader_follows_reused_slots() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
    fill(&mut w, 96, 1);
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process_reader_child",
                "--ignored",
                "--nocapture",
            ])
            .env("STRATAVA_REUSE_READER_PATH", &path)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !path.with_extension("ready").exists() {
        assert!(std::time::Instant::now() < deadline);
        assert!(child.0.try_wait().unwrap().is_none());
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    for epoch in 2..203 {
        fill(&mut w, 96, epoch);
    }
    std::fs::write(path.with_extension("done"), b"done").unwrap();
    assert!(child.0.wait().unwrap().success());
}
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn retry_budget_is_bounded_and_writer_keeps_publishing() {
    use stratava::experimental_matrix::set_test_hook;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
    fill(&mut w, 12, 1);
    let r = MatrixReader::open(
        spec(),
        &path,
        dims(),
        MatrixOptions {
            read_retries: 2,
            ..opts()
        },
    )
    .unwrap();
    let mut epoch = 1;
    set_test_hook(Some(Box::new(move |point| {
        if point == "matrix_reuse.read_copied" {
            epoch += 1;
            fill(&mut w, 12, epoch);
        }
        Ok(())
    })));
    assert!(matches!(
        r.get::<Value>(key(0)),
        Err(MatrixReuseError::Retry { attempts: 2 })
    ));
    set_test_hook(None);
    assert_eq!(r.get::<Value>(key(0)).unwrap().epoch, 3);
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(32))]
    #[test]
    fn fuzz_style_history_matches_confirmed_model(ops in proptest::collection::vec((0u8..5,0u8..64,0u16..65535),1..256)) {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("data");let mut w=MatrixWriter::create(spec(),&path,dims(),opts()).unwrap();
        let mut pending=[None;64];let mut confirmed=pending;
        for(action,index,v)in ops {let i=usize::from(index);match action {
            0=>{w.put(key(i as u64),&value(i as u64,u64::from(v))).unwrap();pending[i]=Some(u64::from(v));},
            1 if pending[i].is_some()=>{w.delete::<Value>(key(i as u64)).unwrap();pending[i]=None;},
            2=>{w.sync().unwrap();confirmed=pending;},
            3=>{drop(w);w=MatrixWriter::open(spec(),&path,dims(),opts()).unwrap();pending=confirmed;},
            _=>{let r=w.reader().unwrap();match confirmed[i]{Some(v)=>proptest::prop_assert_eq!(r.get::<Value>(key(i as u64)).unwrap().epoch,v),None=>proptest::prop_assert!(matches!(r.get::<Value>(key(i as u64)),Err(MatrixReuseError::Missing)))}}
        }}
        w.sync().unwrap();let r=w.reader().unwrap();for(i,expected)in pending.iter().enumerate(){match expected{Some(v)=>proptest::prop_assert_eq!(r.get::<Value>(key(i as u64)).unwrap().epoch,*v),None=>proptest::prop_assert!(matches!(r.get::<Value>(key(i as u64)),Err(MatrixReuseError::Missing)))}}
    }
}

#[test]
#[ignore = "bounded load test; STRATAVA_REUSE_SOAK_SECONDS and STRATAVA_REUSE_SOAK_ROOT configure it"]
fn reuse_load_campaign() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let seconds = std::env::var("STRATAVA_REUSE_SOAK_SECONDS")
        .ok()
        .map(|s| s.parse::<u64>().unwrap())
        .unwrap_or(30);
    assert!((1..=3600).contains(&seconds));
    let rows = std::env::var("STRATAVA_REUSE_SOAK_ROWS")
        .ok()
        .map(|s| s.parse::<u64>().unwrap())
        .unwrap_or(8192);
    assert!((1..=1_048_576).contains(&rows));
    let dir = match std::env::var_os("STRATAVA_REUSE_SOAK_ROOT") {
        Some(p) => tempfile::tempdir_in(p).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let path = dir.path().join("data");
    let options = MatrixOptions {
        rows_per_chunk: 4096,
        ..opts()
    };
    let spec = ReuseFormat::spec().with_growing_matrix_dimension("row", 4096);
    let dims = MatrixDimensions::from_pairs([("row", 4096), ("col", 1)]);
    let mut w = MatrixWriter::create(spec, &path, dims.clone(), options).unwrap();
    let fill = |w: &mut MatrixWriter, epoch: u64| {
        for row in 0..rows {
            w.put(
                MatrixKey::new(row, 0),
                &Value {
                    epoch,
                    row,
                    col: 0,
                    pad: [epoch as u8; 40],
                },
            )
            .unwrap();
        }
        w.sync().unwrap();
    };
    fill(&mut w, 1);
    fill(&mut w, 2);
    let bound = std::fs::metadata(&path).unwrap().len();
    let stop = Arc::new(AtomicBool::new(false));
    let start = std::time::Instant::now();
    let mut publications = 0;
    let mut successful = 0;
    let mut retries = 0;
    std::thread::scope(|scope| {
        let mut joins = Vec::new();
        for _ in 0..8 {
            let r = w.reader().unwrap();
            let stop = stop.clone();
            joins.push(scope.spawn(move || {
                let mut ok = 0;
                let mut retry = 0;
                let mut out = vec![value(0, 0); rows as usize];
                while !stop.load(Ordering::Acquire) && start.elapsed().as_secs() < seconds + 10 {
                    match r.read_run_into(MatrixKey::new(0, 0), &mut out) {
                        Ok(()) => {
                            let epoch = out[0].epoch;
                            for (row, v) in out.iter().enumerate() {
                                assert_eq!((v.epoch, v.row, v.col), (epoch, row as u64, 0));
                                assert_eq!(v.pad, [epoch as u8; 40]);
                            }
                            ok += 1;
                        }
                        Err(MatrixReuseError::Retry { .. }) => retry += 1,
                        Err(e) => panic!("{e:?}"),
                    }
                }
                (ok, retry)
            }));
        }
        while start.elapsed().as_secs() < seconds {
            publications += 1;
            fill(&mut w, publications + 2);
            assert_eq!(std::fs::metadata(&path).unwrap().len(), bound);
        }
        stop.store(true, Ordering::Release);
        for j in joins {
            let (a, b) = j.join().unwrap();
            successful += a;
            retries += b;
        }
    });
    assert!(publications > 0 && successful > 0);
    drop(w);
    let r = MatrixReader::open(spec, &path, dims, options).unwrap();
    assert_eq!(
        r.get::<Value>(MatrixKey::new(rows - 1, 0)).unwrap().epoch,
        publications + 2
    );
    println!(
        "reuse_load rows={rows} seconds={:.3} publications={publications} successful_ranges={successful} retry_exhaustions={retries} primary_bytes={bound}",
        start.elapsed().as_secs_f64()
    );
}

#[test]
fn writer_factory_does_not_rebind_to_a_replaced_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
    fill(&mut w, 12, 1);
    std::fs::rename(&path, dir.path().join("held")).unwrap();
    for suffix in [".reuse-work", ".reuse-journal"] {
        std::fs::rename(
            dir.path().join(format!("data{suffix}")),
            dir.path().join(format!("held{suffix}")),
        )
        .unwrap();
    }
    let mut replacement = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
    fill(&mut replacement, 12, 2);
    assert!(w.reader().is_err());
}
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn publication_errors_distinguish_durable_and_indeterminate() {
    use stratava::experimental_matrix::set_test_hook;
    for (point, durable) in [
        ("matrix_reuse.head_prefix", false),
        ("matrix_reuse.head_written", false),
        ("matrix_reuse.head_durable", true),
        ("matrix_reuse.reset", true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let mut w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
        fill(&mut w, 12, 1);
        w.put(key(0), &value(0, 2)).unwrap();
        set_test_hook(Some(Box::new(move |p| {
            if p == point {
                Err(std::io::Error::other("injected"))
            } else {
                Ok(())
            }
        })));
        let result = w.sync();
        set_test_hook(None);
        if durable {
            assert!(matches!(
                result,
                Err(MatrixReuseError::Published { generation: 2, .. })
            ));
        } else {
            assert!(matches!(
                result,
                Err(MatrixReuseError::PublicationIndeterminate { generation: 2, .. })
            ));
        }
    }
}

#[test]
fn pages_span_cells_and_tiny_writer_cache_survives_eviction() {
    stratava_format! {
        pub format LargeCell {
            magic: b"REUSEBIG"; version:1; endian:little;
            dims {row:u32,col:u32,}
            commit: cell_bitmap {keyspace=[row,col];categories=[data];};
            blocks { matrix Blob(id=1,dims=[row,col],category=data) { bytes:[u8;5000], } }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    for page_bytes in [4096, 8192, 65536] {
        let options = MatrixOptions {
            page_bytes,
            max_chunk_bytes: page_bytes,
            cache_bytes: 0,
            rows_per_chunk: 4096,
            ..opts()
        };
        let spec = LargeCell::spec().with_growing_matrix_dimension("row", 4096);
        let dims = MatrixDimensions::from_pairs([("row", 4096), ("col", 1)]);
        let path = dir.path().join(page_bytes.to_string());
        let mut w = MatrixWriter::create(spec, &path, dims.clone(), options).unwrap();
        for epoch in 1..5 {
            for row in [0, 4, 1, 4095, 4096, 2] {
                w.put(
                    MatrixKey::new(row, 0),
                    &Blob {
                        bytes: [epoch; 5000],
                    },
                )
                .unwrap();
            }
            w.sync().unwrap();
            let r = w.reader().unwrap();
            for row in [0, 1, 2, 4, 4095, 4096] {
                assert_eq!(
                    r.get::<Blob>(MatrixKey::new(row, 0)).unwrap().bytes,
                    [epoch; 5000]
                );
            }
            drop(w);
            w = MatrixWriter::open(spec, &path, dims.clone(), options).unwrap();
        }
    }
}

#[test]
fn logical_chunk_larger_than_working_budget_opens_and_updates_locally() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wide");
    let options = MatrixOptions {
        rows_per_chunk: 4096,
        max_chunk_bytes: 8192,
        cache_bytes: 8192,
        ..opts()
    };
    let spec = ReuseFormat::spec().with_growing_matrix_dimension("row", 4096);
    let dims = MatrixDimensions::from_pairs([("row", 4096), ("col", 640)]);
    // Logical chunk exceeds 180 MiB; neither writer nor reader materializes it.
    let mut w = MatrixWriter::create(spec, &path, dims.clone(), options).unwrap();
    let k = MatrixKey::new(4095, 639);
    for epoch in 1..5 {
        w.put(k, &value(0, epoch)).unwrap();
        w.sync().unwrap();
        assert_eq!(w.reader().unwrap().get::<Value>(k).unwrap().epoch, epoch);
    }
    drop(w);
    let r = MatrixReader::open(spec, &path, dims, options).unwrap();
    assert_eq!(r.get::<Value>(k).unwrap().epoch, 4);
    assert!(matches!(r.get::<Meta>(k), Err(MatrixReuseError::Missing)));
}

#[test]
fn old_experimental_format_and_invalid_page_geometry_are_refused() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    for page_bytes in [0, 1, 4095, 12288, 131072] {
        assert!(
            MatrixWriter::create(
                spec(),
                dir.path().join(page_bytes.to_string()),
                dims(),
                MatrixOptions {
                    page_bytes,
                    ..opts()
                }
            )
            .is_err()
        );
    }
    let path = dir.path().join("old");
    let w = MatrixWriter::create(spec(), &path, dims(), opts()).unwrap();
    drop(w);
    let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.write_all(b"STREUSE1").unwrap();
    drop(f);
    assert!(MatrixReader::open(spec(), &path, dims(), opts()).is_err());
    assert!(MatrixWriter::open(spec(), &path, dims(), opts()).is_err());
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn sparse_updates_write_one_page_and_bulk_reads_coalesce_pages() {
    use stratava::experimental_matrix::take_test_io;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let options = MatrixOptions {
        rows_per_chunk: 4096,
        ..opts()
    };
    let spec = ReuseFormat::spec().with_growing_matrix_dimension("row", 4096);
    let dims = MatrixDimensions::from_pairs([("row", 4096), ("col", 1)]);
    let mut w = MatrixWriter::create(spec, &path, dims, options).unwrap();
    for row in 0..8192 {
        w.put(MatrixKey::new(row, 0), &value(row, 1)).unwrap();
    }
    w.sync().unwrap();
    take_test_io();
    w.put(MatrixKey::new(73, 0), &value(73, 2)).unwrap();
    w.sync().unwrap();
    let io = take_test_io();
    // Includes journal, page headers, publication head and cleanup, not only payload.
    assert!(io.write_bytes < 24 * 1024, "{io:?}");
    let r = w.reader().unwrap();
    let mut out = vec![value(0, 0); 8192];
    take_test_io();
    r.read_run_into(MatrixKey::new(0, 0), &mut out).unwrap();
    let io = take_test_io();
    assert!(
        io.read_calls < 24,
        "bulk path issued page/cell-at-a-time I/O: {io:?}"
    );
    assert_eq!(out[73].epoch, 2);
    assert_eq!(out[72].epoch, 1);
    assert_eq!(out[8191].epoch, 1);
    take_test_io();
    w.delete::<Value>(MatrixKey::new(73, 0)).unwrap();
    w.sync().unwrap();
    let io = take_test_io();
    assert!(io.write_bytes < 24 * 1024, "bitmap-only delete: {io:?}");
    assert!(matches!(
        r.get::<Value>(MatrixKey::new(73, 0)),
        Err(MatrixReuseError::Missing)
    ));
    take_test_io();
    w.sync().unwrap();
    assert_eq!(take_test_io().write_bytes, 0);
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn restarting_a_scan_does_not_evict_its_own_active_page() {
    use stratava::experimental_matrix::take_test_io;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data");
    let options = MatrixOptions {
        rows_per_chunk: 4096,
        max_chunk_bytes: 16 * 1024,
        ..opts()
    };
    let spec = ReuseFormat::spec().with_growing_matrix_dimension("row", 4096);
    let dims = MatrixDimensions::from_pairs([("row", 4096), ("col", 1)]);
    let mut w = MatrixWriter::create(spec, &path, dims, options).unwrap();
    for epoch in 1..5 {
        take_test_io();
        for row in 0..512 {
            w.put(MatrixKey::new(row, 0), &value(row, epoch)).unwrap();
        }
        w.sync().unwrap();
        let io = take_test_io();
        // Eight data pages plus bitmap. Writing every cell's page on its own
        // would exceed 2 MiB; bounded eviction must not produce that amplification.
        assert!(io.write_bytes < 128 * 1024, "{io:?}");
        let r = w.reader().unwrap();
        let mut out = vec![value(0, 0); 512];
        r.read_run_into(MatrixKey::new(0, 0), &mut out).unwrap();
        assert!(out.iter().all(|v| v.epoch == epoch));
    }
}
