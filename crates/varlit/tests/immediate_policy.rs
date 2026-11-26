//! Durability boundaries are tested through fresh snapshots and recovery,
//! including boundaries inside one iterator call.
//!
//! A stream writer publishes at its durability boundaries, so a fresh reader
//! sees exactly what the last boundary covered. A native writer makes each
//! write visible at once; its boundaries decide only what is synced.

use varlit::{BatchOptions, Error, ImmediatePolicy, StreamOptions, varlit_format};

varlit_format! {
    pub format Boundaries {
        magic: b"IMMD";
        version: 1;
        schema_hash: computed;
        index: keyed_offset_chain;
        blocks {
            fixed Item(id = 1, key = [key]) { key: u64, value: u64 }
            fixed Durable(id = 2, key = [key], durability = immediate) { key: u64 }
            fixed Marker(id = 3, durability = immediate) { value: u64 }
            fixed Conditional(id = 4, key = [key], immediate_if = checkpoint) { key: u64, value: u64 }
        }
    }
}

fn checkpoint(value: &Conditional) -> bool {
    value.value == 2 || value.value == 5
}

varlit_format! {
    pub format StreamBoundaries {
        magic: b"IMMS";
        version: 1;
        schema_hash: computed;
        index: block_offset_chain;
        blocks {
            fixed Sample(id = 11, immediate_if = event_checkpoint) { value: u64 }
            fixed Barrier(id = 12, durability = immediate) { value: u64 }
        }
    }
}

fn event_checkpoint(value: &Sample) -> bool {
    value.value == 2 || value.value == 5
}

/// The current value of `key`, read through the native reader's key map.
fn get<T>(reader: &varlit::VarlitReader, key: &T::Key) -> varlit::Result<Option<T>>
where
    T: varlit::VarlitKeyedBlock + varlit::VarlitEmpty,
{
    let mut out = T::empty();
    Ok(reader
        .keyed_blocks::<T>()?
        .get(key, &mut out)?
        .then_some(out))
}

fn stream_options() -> StreamOptions {
    StreamOptions::default()
}

fn visible_samples(path: &std::path::Path) -> varlit::Result<usize> {
    Ok(
        collect_blocks(StreamBoundaries::open_stream_reader(path, stream_options())?.samples()?)?
            .len(),
    )
}

fn visible_barriers(path: &std::path::Path) -> varlit::Result<usize> {
    Ok(
        collect_blocks(StreamBoundaries::open_stream_reader(path, stream_options())?.barriers()?)?
            .len(),
    )
}

#[test]
fn mandatory_blocks_cover_prior_writes() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("blocks.varlit");
    let mut writer = StreamBoundaries::create_stream_writer(&path, stream_options())?;
    let pinned = StreamBoundaries::open_stream_reader(&path, stream_options())?;
    writer.push_sample(&Sample { value: 0 })?;
    assert_eq!(visible_samples(&path)?, 0);
    writer.push_barrier(&Barrier { value: 7 })?;
    assert_eq!(visible_samples(&path)?, 1);
    assert_eq!(visible_barriers(&path)?, 1);
    assert_eq!(collect_blocks(pinned.samples()?)?.len(), 0);
    Ok(())
}

#[test]
fn value_conditions_cut_batches_at_the_record_and_recovery_keeps_the_boundary() -> varlit::Result<()>
{
    for chunk_bytes in [1, 64, 4096] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("values.varlit");
        let mut writer = StreamBoundaries::create_stream_writer(&path, stream_options())?;
        let values = (0..8).map(|value| {
            // Values 2 and 5 are boundaries: by the time 3 and 6 are pulled,
            // everything up to the boundary is published and nothing after it.
            if value == 3 || value == 6 {
                assert_eq!(visible_samples(&path).unwrap(), value as usize);
            }
            Sample { value }
        });
        let info = writer
            .push_samples(
                values,
                BatchOptions {
                    max_records: 64,
                    max_bytes: chunk_bytes,
                },
            )
            .map_err(|e| e.source)?;
        assert_eq!(info.records, 8);
        assert_eq!(visible_samples(&path)?, 6);
        drop(writer);
        let mut writer = StreamBoundaries::open_stream_writer(&path, stream_options())?;
        assert!(visible_samples(&path)? >= 6);
        writer.push_sample(&Sample { value: 9 })?;
        writer.immediate()?;
        assert!(visible_samples(&path)? >= 7);
    }
    Ok(())
}

#[test]
fn stream_scalar_and_batch_writes_honor_declarations() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("stream.varlit");
    let mut writer = StreamBoundaries::create_stream_writer(&path, StreamOptions::default())?;
    writer.push_sample(&Sample { value: 0 })?;
    assert_eq!(
        collect_blocks(
            StreamBoundaries::open_stream_reader(&path, StreamOptions::default())?.samples()?
        )?
        .len(),
        0
    );
    writer.push_barrier(&Barrier { value: 0 })?;
    StreamBoundaries::open_stream_reader(&path, StreamOptions::default())?;
    let values = (0..8).map(|value| {
        if value == 3 || value == 6 {
            StreamBoundaries::open_stream_reader(&path, StreamOptions::default()).unwrap();
        }
        Sample { value }
    });
    writer
        .push_samples(values, BatchOptions::default())
        .map_err(|e| e.source)?;
    assert_eq!(
        collect_blocks(
            StreamBoundaries::open_stream_reader(&path, StreamOptions::default())?.samples()?
        )?
        .len(),
        7
    );
    writer.immediate()?;
    writer.set_immediate_policy(ImmediatePolicy::new().after_records(2))?;
    writer.push_sample(&Sample { value: 10 })?;
    assert_eq!(
        collect_blocks(
            StreamBoundaries::open_stream_reader(&path, StreamOptions::default())?.samples()?
        )?
        .len(),
        9
    );
    writer.push_sample(&Sample { value: 11 })?;
    StreamBoundaries::open_stream_reader(&path, StreamOptions::default())?;

    Ok(())
}

/// On a native writer every write is visible at once; this checks that the
/// policy API installs, validates and keeps conditions.
#[test]
fn runtime_conditions_and_explicit_immediate_share_the_same_counters() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("policy.varlit");
    let mut writer = Boundaries::create_writer(&path)?;
    writer.set_immediate_policy(ImmediatePolicy::new().after_records(3))?;
    writer.push_item(&Item { key: 0, value: 0 })?;
    writer.immediate()?;
    for value in 1..=6 {
        if value == 4 {
            assert!(get::<Item>(&Boundaries::open_reader(&path)?.into_inner(), &3)?.is_some());
        }
        writer.push_item(&Item { key: value, value })?;
    }
    assert!(get::<Item>(&Boundaries::open_reader(&path)?.into_inner(), &6)?.is_some());
    writer.set_immediate_policy(ImmediatePolicy::new().when(|event| {
        event.block_id == 1 && event.operation == varlit::ImmediateOperation::Delete
    }))?;
    writer.push_item(&Item { key: 7, value: 7 })?;
    writer.delete_item(&6)?;
    let reader = Boundaries::open_reader(&path)?.into_inner();
    assert!(get::<Item>(&reader, &6)?.is_none());
    assert!(get::<Item>(&reader, &7)?.is_some());
    writer.set_immediate_policy(ImmediatePolicy::new().after_records(1000).after_bytes(1))?;
    writer.push_item(&Item { key: 8, value: 8 })?;
    assert!(get::<Item>(&Boundaries::open_reader(&path)?.into_inner(), &8)?.is_some());
    assert!(matches!(
        writer.set_immediate_policy(ImmediatePolicy::new().after_records(0)),
        Err(Error::InvalidImmediatePolicy)
    ));
    // Invalid installation keeps the old byte condition.
    writer.push_item(&Item { key: 9, value: 9 })?;
    assert!(get::<Item>(&Boundaries::open_reader(&path)?.into_inner(), &9)?.is_some());
    Ok(())
}

#[test]
fn process_exit_after_immediate_preserves_acknowledged_records() -> varlit::Result<()> {
    for mode in ["explicit", "block", "policy", "value"] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("crash.varlit");
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "immediate_crash_child", "--ignored"])
            .env("VARLIT_IMMEDIATE_CHILD_PATH", &path)
            .env("VARLIT_IMMEDIATE_CHILD_MODE", mode)
            .status()?;
        assert_eq!(status.code(), Some(73), "child mode {mode}");
        varlit::clear_stale_writer_lock(
            &path,
            varlit::WriterLockBreakPolicy::BreakIfProcessAbsent,
        )?;
        let _writer = StreamBoundaries::open_stream_writer(&path, stream_options())?;
        let expected = if mode == "value" { 4 } else { 3 };
        assert_eq!(visible_samples(&path)?, expected, "mode {mode}");
        assert_eq!(
            visible_barriers(&path)?,
            usize::from(mode == "block"),
            "mode {mode}"
        );
    }
    Ok(())
}

#[test]
#[ignore = "subprocess entry point; exits without running writer destructors"]
fn immediate_crash_child() -> varlit::Result<()> {
    let path = std::env::var_os("VARLIT_IMMEDIATE_CHILD_PATH").expect("child path");
    let mode = std::env::var("VARLIT_IMMEDIATE_CHILD_MODE").expect("child mode");
    let mut writer = StreamBoundaries::create_stream_writer(path, stream_options())?;
    if mode == "policy" {
        writer.set_immediate_policy(ImmediatePolicy::new().after_records(3))?;
    }
    writer
        .push_samples(
            (10..13).map(|value| Sample { value }),
            BatchOptions::default(),
        )
        .map_err(|e| e.source)?;
    match mode.as_str() {
        "explicit" => writer.immediate()?,
        "block" => {
            writer.push_barrier(&Barrier { value: 1 })?;
        }
        "value" => {
            writer.push_sample(&Sample { value: 2 })?;
        }
        "policy" => {}
        _ => panic!("unknown child mode"),
    }
    writer.set_immediate_policy(ImmediatePolicy::default())?;
    writer
        .push_samples(
            (20..26).map(|value| Sample { value }),
            BatchOptions {
                max_records: 1,
                max_bytes: 1,
            },
        )
        .map_err(|e| e.source)?;
    writer.flush()?;
    // Exiting skips the writer's destructor, which would otherwise publish.
    std::process::exit(73)
}

/// Drains a streaming walk into a `Vec`; the first error ends it.
fn collect_blocks<T: varlit::VarlitBlock + varlit::VarlitEmpty + Clone>(
    mut blocks: varlit::StreamingBlocks<T>,
) -> varlit::Result<Vec<T>> {
    let mut out = T::empty();
    let mut all = Vec::new();
    while blocks.next_into(&mut out)? {
        all.push(out.clone());
    }
    Ok(all)
}
