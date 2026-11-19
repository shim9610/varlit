//! Thread ownership, explicit snapshot retention, and cold-open writer admission.

use std::sync::{
    Barrier,
    atomic::{AtomicBool, Ordering},
};
use varlit::{BatchOptions, DiskIndexOptions, Error, StreamOptions, VarlitEmpty, varlit_format};

varlit_format! {
    pub format Feed {
        magic: b"MGFD";
        version: 1;
        schema_hash: computed;
        index: block_offset_chain;
        blocks { fixed Sample(id = 1) { value: u64 } }
    }
}

/// Every sample a fresh walk of `reader` yields, in order.
fn samples(reader: &FeedStreamReader) -> varlit::Result<Vec<u64>> {
    let mut cursor = reader.samples()?;
    let mut sample = Sample::empty();
    let mut values = Vec::new();
    while cursor.next_into(&mut sample)? {
        values.push(sample.value);
    }
    Ok(values)
}

#[test]
fn moving_reader_with_live_local_cursors_keeps_positions_independent() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("positions.varlit");
    let options = StreamOptions::default();
    let mut writer = Feed::create_stream_writer(&path, options)?;
    writer
        .push_samples(
            (0..10000).map(|value| Sample { value: value * 3 }),
            BatchOptions::default(),
        )
        .map_err(|e| e.source)?;
    writer.immediate()?;
    let reader = Feed::open_stream_reader(&path, options)?;
    let mut cursors = (0..8)
        .map(|_| reader.samples())
        .collect::<varlit::Result<Vec<_>>>()?;
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| -> varlit::Result<()> {
        let worker = scope.spawn(|| -> varlit::Result<()> {
            // Move the reader, never a cursor or a shared reader reference.
            let reader = reader;
            barrier.wait();
            let values = samples(&reader)?;
            assert_eq!(
                values,
                (0..10000).map(|value| value * 3).collect::<Vec<_>>()
            );
            reader.verify_all()?;
            Ok(())
        });
        barrier.wait();
        let mut sample = Sample::empty();
        for value in 0..10000 {
            for cursor in &mut cursors {
                assert!(cursor.next_into(&mut sample)?);
                assert_eq!(sample, Sample { value: value * 3 });
            }
        }
        for cursor in &mut cursors {
            assert!(!cursor.next_into(&mut sample)?);
        }
        worker.join().expect("reader panicked")
    })
}

#[test]
fn stream_release_preserves_cursor_and_follow_releases_old_generation() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("stream.varlit");
    // Lag is counted in confirmed sidecar generations, so confirm each sync.
    let options = StreamOptions {
        state: DiskIndexOptions {
            confirm: varlit::DiskIndexConfirmPolicy::EVERY_SYNC,
            ..DiskIndexOptions::default()
        },
        ..StreamOptions::default()
    };
    let mut writer = Feed::create_stream_writer(&path, options)?;
    writer.push_sample(&Sample { value: 1 })?;
    writer.immediate()?;
    let mut reader = Feed::open_stream_reader(&path, options)?;
    let mut cursor = reader.samples()?;
    assert!(reader.release_snapshot());
    let mut sample = Sample::empty();
    assert!(cursor.next_into(&mut sample)?);
    assert_eq!(sample, Sample { value: 1 });
    writer.push_sample(&Sample { value: 2 })?;
    writer.immediate()?;
    writer.push_sample(&Sample { value: 3 })?;
    // The sample and its sync marker.
    assert_eq!(reader.snapshot_status()?.records_behind, 2);
    assert_eq!(reader.follow_blocks(&mut cursor)?, 2);
    assert!(cursor.next_into(&mut sample)?);
    assert_eq!(sample, Sample { value: 2 });
    assert!(!cursor.next_into(&mut sample)?);
    assert!(reader.release_snapshot());
    assert_eq!(reader.follow()?, 0);
    assert!(reader.snapshot_status()?.pinned);
    Ok(())
}

#[test]
fn cold_reader_retries_never_reject_or_interrupt_the_single_writer() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let options = StreamOptions::default();
    for trial in 0..32 {
        let path = dir.path().join(format!("cold-{trial}.varlit"));
        let mut writer = Feed::create_stream_writer(&path, options)?;
        writer.push_sample(&Sample { value: 0 })?;
        writer.immediate()?;
        drop(writer);
        let start = Barrier::new(9);
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| -> varlit::Result<()> {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| -> varlit::Result<()> {
                        start.wait();
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(10);
                        loop {
                            match Feed::open_stream_reader(&path, options) {
                                Ok(mut reader) => {
                                    reader.follow()?;
                                    let values = samples(&reader)?;
                                    assert!(!values.is_empty() && values.len() <= 17);
                                    assert!(values.iter().enumerate().all(|(i, v)| *v == i as u64));
                                }
                                Err(Error::IndexBusy) if std::time::Instant::now() < deadline => {
                                    std::thread::yield_now();
                                    continue;
                                }
                                Err(error) => return Err(error),
                            }
                            if done.load(Ordering::Acquire) {
                                return Ok(());
                            }
                        }
                    })
                })
                .collect();
            start.wait();
            let result = (|| -> varlit::Result<()> {
                // No application retry around writer open, append, or sync.
                let mut writer = Feed::open_stream_writer(&path, options)?;
                for value in 1..=16 {
                    writer.push_sample(&Sample { value })?;
                    writer.immediate()?;
                }
                Ok(())
            })();
            done.store(true, Ordering::Release);
            for worker in workers {
                worker.join().expect("reader panicked")?;
            }
            result
        })?;
    }
    Ok(())
}

/// Samples are written in pairs of equal value, one pair per durable
/// boundary, so a reader that ever sees an odd count or an unequal pair has
/// observed a torn generation.
#[test]
#[ignore = "subprocess entry point for independent_process_reads_dirty_writer_and_follows_compaction"]
fn independent_process_reader_child() -> varlit::Result<()> {
    let path = std::path::PathBuf::from(std::env::var_os("VARLIT_STREAM_READER_CHILD").unwrap());
    let options = StreamOptions::default();
    let mut reader = Feed::open_stream_reader(&path, options)?;
    let pinned = Feed::open_stream_reader(&path, options)?;
    assert_eq!(samples(&reader)?, [1, 1]);
    std::fs::write(path.with_extension("ready"), b"ready")?;
    // Bound a stalled publication, not all 256 durable commits on a busy disk.
    let mut deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut last_len = 2;
    let mut checks = 0;
    loop {
        reader.follow()?;
        let values = samples(&reader)?;
        assert_eq!(values.len() % 2, 0, "mixed confirmed generations");
        assert!(values.chunks(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(samples(&pinned)?, [1, 1]);
        if values.len() != last_len {
            assert!(
                values.len() > last_len,
                "confirmed generation moved backwards"
            );
            last_len = values.len();
            deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        }
        checks += 1;
        if path.with_extension("done").exists() {
            reader.follow()?;
            assert_eq!(samples(&reader)?.len(), 2 * 257);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "writer made no confirmed progress for 20 seconds"
        );
        std::thread::yield_now();
    }
    assert!(checks > 0);
    reader.verify_all()?;
    Ok(())
}

#[test]
fn independent_process_reads_dirty_writer_and_follows_compaction() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("process.varlit");
    let options = StreamOptions::default();
    let mut writer = Feed::create_stream_writer(&path, options)?;
    for _ in 0..2 {
        writer.push_sample(&Sample { value: 1 })?;
    }
    writer.immediate()?;
    writer.push_sample(&Sample { value: 2 })?; // Child opens during Dirty.
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--ignored",
            "--exact",
            "independent_process_reader_child",
            "--nocapture",
        ])
        .env("VARLIT_STREAM_READER_CHILD", &path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !path.with_extension("ready").exists() {
        if let Some(status) = child.try_wait()? {
            panic!("reader exited before open: {status}");
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("reader open blocked");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    writer.push_sample(&Sample { value: 2 })?;
    writer.immediate()?;
    for value in 3..=257 {
        for _ in 0..2 {
            writer.push_sample(&Sample { value })?;
        }
        writer.immediate()?;
        if value % 64 == 0 {
            writer.compact_index()?;
        }
    }
    std::fs::write(path.with_extension("done"), b"done")?;
    // Startup, active publication, and final acknowledgement are distinct phases.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait()? {
            let output = child.wait_with_output()?;
            assert!(
                status.success(),
                "child failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("reader did not finish");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    Ok(())
}
