//! Confirmed generations stay readable while a writer is Dirty. Following
//! updates a handle explicitly and can resume existing parsers without replay.

use std::sync::{
    Barrier,
    atomic::{AtomicBool, Ordering},
};
use varlit::{
    BatchOptions, Error, StreamOptions, StreamingBlocks, VarlitBlock, VarlitEmpty, varlit_format,
};

/// Drains the rest of a block walk into a `Vec` through one reused value.
fn drain<T: VarlitBlock + VarlitEmpty + Clone>(
    blocks: &mut StreamingBlocks<T>,
) -> varlit::Result<Vec<T>> {
    let mut value = T::empty();
    let mut all = Vec::new();
    while blocks.next_into(&mut value)? {
        all.push(value.clone());
    }
    Ok(all)
}

varlit_format! {
    pub format Feed {
        magic: b"FEED";
        version: 1;
        schema_hash: computed;
        index: block_offset_chain;
        blocks { fixed Sample(id = 1) { value: u64 } }
    }
}

fn options() -> StreamOptions {
    StreamOptions::default()
}

/// Every sample a fresh walk of `reader` yields, in order.
fn values(reader: &FeedStreamReader) -> varlit::Result<Vec<u64>> {
    Ok(drain(&mut reader.samples()?)?
        .into_iter()
        .map(|sample| sample.value)
        .collect())
}

#[test]
fn readers_open_and_follow_inside_an_active_write_batch() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("batch.varlit");
    let mut writer = Feed::create_stream_writer(&path, options())?;
    writer.push_sample(&Sample { value: 7 })?;
    writer.immediate()?;
    let mut retained = Feed::open_stream_reader(&path, options())?;
    let batch = (0..25).map(|_| {
        let reader = Feed::open_stream_reader(&path, options()).unwrap();
        assert_eq!(values(&reader).unwrap(), [7]);
        assert_eq!(retained.follow().unwrap(), 0);
        Sample { value: 8 }
    });
    writer
        .push_samples(
            batch,
            BatchOptions {
                max_records: 1,
                max_bytes: 1,
            },
        )
        .map_err(|e| e.source)?;
    assert_eq!(retained.follow()?, 0);
    writer.immediate()?;
    // Twenty-five records and the sync marker that closed them.
    assert_eq!(retained.follow()?, 26);
    assert_eq!(values(&retained)?.len(), 26);
    Ok(())
}

#[test]
fn dirty_reopen_and_restore_do_not_invalidate_existing_readers() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("recover.varlit");
    let mut writer = Feed::create_stream_writer(&path, options())?;
    writer.push_sample(&Sample { value: 1 })?;
    writer.immediate()?;
    writer.push_sample(&Sample { value: 2 })?;
    writer.flush()?;
    drop(writer);
    let dirty_length = std::fs::metadata(&path)?.len();
    let mut reader = Feed::open_stream_reader(&path, options())?;
    assert_eq!(values(&reader)?, [1]);
    assert_eq!(std::fs::metadata(&path)?.len(), dirty_length);
    let mut cursor = reader.samples()?;
    let mut sample = Sample::empty();
    assert!(cursor.next_into(&mut sample)?);
    assert_eq!(sample.value, 1);
    assert!(!cursor.next_into(&mut sample)?);
    let mut writer = Feed::open_stream_writer(&path, options())?;
    assert!(std::fs::metadata(&path)?.len() < dirty_length);
    assert_eq!(reader.follow()?, 0);
    writer.push_sample(&Sample { value: 3 })?;
    writer.immediate()?;
    // The record and its sync marker.
    assert_eq!(reader.follow_blocks(&mut cursor)?, 2);
    assert!(cursor.next_into(&mut sample)?);
    assert_eq!(sample.value, 3);
    assert!(!cursor.next_into(&mut sample)?);
    assert_eq!(values(&reader)?, [1, 3]);
    Ok(())
}

/// Each epoch appends `KEYS` samples of the epoch's value as one durable
/// generation; a follower must only ever see whole epochs.
#[test]
fn eight_followers_observe_atomic_generations_during_single_writer_updates() -> varlit::Result<()> {
    const KEYS: u64 = 16;
    const EPOCHS: u64 = 100;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("concurrent.varlit");
    let mut writer = Feed::create_stream_writer(&path, options())?;
    writer
        .push_samples(
            (0..KEYS).map(|_| Sample { value: 0 }),
            BatchOptions::default(),
        )
        .map_err(|e| e.source)?;
    writer.immediate()?;
    let stop = AtomicBool::new(false);
    let barrier = Barrier::new(9);
    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    std::thread::scope(|scope| -> varlit::Result<()> {
        let _stop_on_panic = StopOnDrop(&stop);
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let path = &path;
                let stop = &stop;
                let barrier = &barrier;
                scope.spawn(move || -> varlit::Result<u64> {
                    let mut reader = Feed::open_stream_reader(path, options())?;
                    let mut cursor = reader.samples()?;
                    let mut parsed = 0;
                    let mut reads = 0;
                    let mut sample = Sample::empty();
                    barrier.wait();
                    loop {
                        let done = stop.load(Ordering::Acquire);
                        reader.follow_blocks(&mut cursor)?;
                        while cursor.next_into(&mut sample)? {
                            assert_eq!(sample.value, parsed / KEYS);
                            parsed += 1;
                        }
                        assert_eq!(parsed % KEYS, 0, "torn generation");
                        let fresh = values(&Feed::open_stream_reader(path, options())?)?;
                        assert_eq!(fresh.len() as u64 % KEYS, 0, "torn fresh generation");
                        reads += 1;
                        if done {
                            assert_eq!(parsed, (EPOCHS + 1) * KEYS);
                            break;
                        }
                    }
                    Ok(reads)
                })
            })
            .collect();
        barrier.wait();
        let result = (|| -> varlit::Result<()> {
            for value in 1..=EPOCHS {
                writer
                    .push_samples(
                        (0..KEYS).map(|_| Sample { value }),
                        BatchOptions {
                            max_records: 1,
                            max_bytes: 1,
                        },
                    )
                    .map_err(|e| e.source)?;
                writer.immediate()?;
            }
            Ok(())
        })();
        stop.store(true, Ordering::Release);
        let mut reads = 0;
        for worker in workers {
            reads += worker.join().expect("reader panicked")?;
        }
        result?;
        assert!(reads >= 8);
        eprintln!("FOLLOW_CONCURRENT readers=8 generations={EPOCHS} coherent_reads={reads}");
        Ok(())
    })
}

#[test]
fn stream_cursors_resume_from_eof_across_dirty_generations() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("feed.varlit");
    let opts = StreamOptions::default();
    let mut writer = Feed::create_stream_writer(&path, opts)?;
    writer.push_sample(&Sample { value: 0 })?;
    let mut reader = Feed::open_stream_reader(&path, opts)?;
    let mut blocks = reader.samples()?;
    let mut events = reader.events()?;
    let initial_events = events.by_ref().collect::<varlit::Result<Vec<_>>>()?.len();
    let mut sample = Sample::empty();
    assert!(!blocks.next_into(&mut sample)?);
    let mut all = Vec::new();
    let mut event_count = initial_events;
    for value in 0..40 {
        assert_eq!(reader.follow_blocks(&mut blocks)?, 0);
        assert!(!blocks.next_into(&mut sample)?);
        writer.immediate()?;
        writer.push_sample(&Sample { value: value + 1 })?;
        // The synced sample and its sync marker.
        assert_eq!(reader.follow_blocks(&mut blocks)?, 2);
        all.extend(drain(&mut blocks)?);
        // A cursor can catch up after follow() already advanced the reader.
        assert_eq!(reader.follow_events(&mut events)?, 0);
        event_count += events.by_ref().collect::<varlit::Result<Vec<_>>>()?.len();
    }
    assert_eq!(
        all,
        (0..40).map(|value| Sample { value }).collect::<Vec<_>>()
    );
    // Each sync adds its sample and its marker.
    assert_eq!(event_count, initial_events + 80);
    assert_eq!(reader.verify_all()? as usize, event_count);
    Ok(())
}

#[test]
fn foreign_cursor_is_rejected_before_the_reader_changes() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("foreign.varlit");
    let mut writer = Feed::create_stream_writer(&path, StreamOptions::default())?;
    let mut reader = Feed::open_stream_reader(&path, StreamOptions::default())?;
    let other = Feed::open_stream_reader(&path, StreamOptions::default())?;
    let mut cursor = other.samples()?;
    let eof = reader.committed_len();
    writer.push_sample(&Sample { value: 1 })?;
    writer.immediate()?;
    assert!(matches!(
        reader.follow_blocks(&mut cursor),
        Err(Error::InvalidFollowCursor(_))
    ));
    assert_eq!(reader.committed_len(), eof);
    Ok(())
}

#[test]
fn follow_retains_scan_limits_and_cannot_revive_a_failed_cursor() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("limit.varlit");
    let mut writer = Feed::create_stream_writer(&path, StreamOptions::default())?;
    let opts = StreamOptions {
        limits: varlit::ResourceLimits::MISSING.with_max_records(3),
    };
    let mut reader = Feed::open_stream_reader(&path, opts)?;
    let mut cursor = reader.events()?;
    let mut seen = cursor.by_ref().collect::<varlit::Result<Vec<_>>>()?.len();
    while seen < 3 {
        writer.push_sample(&Sample { value: seen as u64 })?;
        writer.immediate()?;
        reader.follow_events(&mut cursor)?;
        seen += cursor.by_ref().collect::<varlit::Result<Vec<_>>>()?.len();
    }
    writer.push_sample(&Sample { value: 4 })?;
    writer.immediate()?;
    reader.follow_events(&mut cursor)?;
    assert!(cursor.next().unwrap().is_err());
    assert!(matches!(
        reader.follow_events(&mut cursor),
        Err(Error::InvalidFollowCursor(_))
    ));
    Ok(())
}
