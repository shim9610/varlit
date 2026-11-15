//! Pending scans remain private, advance only over new complete frames, and
//! are revalidated before publication. Certificates retain exact record starts
//! for nonresident logging without retaining full record descriptors.

#![cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
thread_local! {
    static REFUSAL: Cell<(usize, usize, usize)> = const { Cell::new((usize::MAX, usize::MAX, 0)) };
}
struct RefusingAllocator;
fn refuse(size: usize) -> bool {
    REFUSAL
        .try_with(|state| {
            let (minimum, nth, count) = state.get();
            if size < minimum {
                return false;
            }
            state.set((minimum, nth, count + 1));
            count + 1 >= nth
        })
        .unwrap_or(false)
}
// SAFETY: successful requests and deallocations use System with the original
// layouts; refusal returns null. Thread-local bookkeeping does not allocate.
unsafe impl GlobalAlloc for RefusingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if refuse(layout.size()) {
            std::ptr::null_mut()
        } else {
            unsafe { System.alloc(layout) }
        }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if refuse(layout.size()) {
            std::ptr::null_mut()
        } else {
            unsafe { System.alloc_zeroed(layout) }
        }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if refuse(size) {
            std::ptr::null_mut()
        } else {
            unsafe { System.realloc(ptr, layout, size) }
        }
    }
}
#[global_allocator]
static ALLOCATOR: RefusingAllocator = RefusingAllocator;
struct RefusalWindow;
impl Drop for RefusalWindow {
    fn drop(&mut self) {
        REFUSAL.with(|s| s.set((usize::MAX, usize::MAX, 0)));
    }
}
fn with_refusal<T>(nth: usize, run: impl FnOnce() -> T) -> T {
    REFUSAL.with(|s| s.set((1024, nth, 0)));
    let window = RefusalWindow;
    let result = run();
    drop(window);
    result
}

use varlit::{VarlitBlock, VarlitEmpty, varlit_format};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 10, version = 1, kind = "fixed")]
struct Reading {
    value: u64,
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 11, version = 1, kind = "variable")]
struct Label {
    #[varlit(field_id = 1)]
    text: String,
}

varlit_format! {
    pub struct FollowFormat {
        magic: b"VFOLLOW1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks: [Reading, Label];
    }
}

// The same declaration with no commit marker, so "visible" means "framed"
// rather than "committed". The boundary rules differ and both are pinned.
varlit_format! {
    pub struct FollowNoMarker {
        magic: b"VFOLLOW2";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: none;
        blocks: [Reading, Label];
    }
}

fn spec() -> varlit::FormatSpec {
    FollowFormat::spec()
}

fn records(file: &varlit::VarlitFile) -> varlit::Result<usize> {
    let mut buffer = Vec::new();
    let mut walk = file.record_map(&mut buffer)?;
    let mut count = 0;
    while walk.advance()?.is_some() {
        count += 1;
    }
    Ok(count)
}

fn explicit() -> varlit::FormatSpec {
    spec().with_commit_policy(varlit::CommitPolicy::TransactionMarker(
        varlit::TransactionMarkerMode::Explicit,
    ))
}
fn fixture(path: &std::path::Path) -> varlit::Result<(varlit::VarlitFile, varlit::VarlitFile)> {
    let mut writer = explicit().create(path)?;
    for value in 0..128 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;
    let reader = varlit::VarlitFile::open_readonly(explicit(), path)?;
    Ok((writer, reader))
}
#[test]
fn idle_pending_polls_do_not_reframe_a_complete_prefix() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut writer, mut reader) = fixture(&directory.path().join("idle"))?;
    let held = records(&reader)?;
    for value in 128..4224 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    let before = varlit::VarlitFile::records_framed();
    assert_eq!(reader.follow()?, 0);
    let first = varlit::VarlitFile::records_framed() - before;
    assert_eq!(first, 4096);
    for _ in 0..64 {
        let before = varlit::VarlitFile::records_framed();
        assert_eq!(reader.follow()?, 0);
        assert_eq!(
            varlit::VarlitFile::records_framed() - before,
            0,
            "an unchanged pending prefix was scanned again"
        );
    }
    assert_eq!(records(&reader)?, held);
    writer.commit()?;
    assert!(reader.follow()? > 0);
    assert_eq!(records(&reader)?, held + 4097);
    Ok(())
}
#[test]
fn growing_pending_polls_parse_only_new_complete_records() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut writer, mut reader) = fixture(&directory.path().join("growing"))?;
    let held = records(&reader)?;
    let mut value = 128;
    for batch in 0..32 {
        for _ in 0..64 {
            writer.push(&Reading { value })?;
            value += 1;
        }
        writer.flush()?;
        let before = varlit::VarlitFile::records_framed();
        assert_eq!(reader.follow()?, 0);
        let framed = varlit::VarlitFile::records_framed() - before;
        assert_eq!(framed, 64, "batch {batch} replayed old pending records");
    }
    assert_eq!(records(&reader)?, held);
    writer.commit()?;
    let before = varlit::VarlitFile::records_framed();
    assert!(reader.follow()? > 0);
    assert!(
        varlit::VarlitFile::records_framed() - before <= 2052,
        "publication may revalidate the pending run once, not once per poll"
    );
    assert_eq!(records(&reader)?, held + 2049);
    Ok(())
}
#[test]
fn writer_recovery_discards_the_previous_pending_cursor() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("recovery");
    let (mut writer, mut reader) = fixture(&path)?;
    let held = records(&reader)?;
    for value in 0..256 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    assert_eq!(reader.follow()?, 0);
    drop(writer);
    // The truncate and regrowth both occur between reader polls. The replacement
    // run is larger, so checking the current length alone cannot detect recovery.
    let mut writer = varlit::VarlitFile::open(explicit(), &path)?;
    for value in 1000..1512 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;
    assert!(reader.follow()? > 0);
    let mut entries = Vec::new();
    reader.index_entries_into(&mut entries)?;
    let mut scratch = Vec::new();
    let mut reading = Reading::empty();
    let values: Vec<_> = entries
        .iter()
        .filter(|e| e.block_id == 10)
        .map(|e| {
            reader
                .decode_block_into::<Reading>(e, &mut scratch, &mut reading)
                .map(|()| reading.value)
        })
        .collect::<varlit::Result<_>>()?;
    assert_eq!(values.len(), 640);
    assert_eq!(&values[128..], &(1000..1512).collect::<Vec<_>>());
    assert_eq!(records(&reader)?, held + 513);
    Ok(())
}
#[test]
fn cached_pending_headers_are_revalidated_before_publication() -> varlit::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("changed");
    let (mut writer, mut reader) = fixture(&path)?;
    let held = records(&reader)?;
    let pending = writer.push_info(&Reading { value: 1000 })?;
    writer.flush()?;
    assert_eq!(reader.follow()?, 0);
    let mut raw = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?;
    raw.seek(SeekFrom::Start(pending.record_offset + 6))?;
    let mut original = [0; 2];
    raw.read_exact(&mut original)?;
    raw.seek(SeekFrom::Start(pending.record_offset + 6))?;
    raw.write_all(&[0x80, 0])?;
    writer.commit()?;
    assert!(
        reader.follow().is_err(),
        "a cached parse authorized a changed header"
    );
    assert_eq!(records(&reader)?, held);
    raw.seek(SeekFrom::Start(pending.record_offset + 6))?;
    raw.write_all(&original)?;
    assert!(
        reader.follow()? > 0,
        "repair at the same EOF must permit retry"
    );
    assert_eq!(records(&reader)?, held + 2);
    Ok(())
}

#[test]
fn eight_readers_reuse_pending_progress_and_publish_the_same_commit() -> varlit::Result<()> {
    use std::sync::mpsc;
    use std::time::Duration;
    #[derive(Clone, Copy)]
    enum Command {
        Pending,
        Committed,
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("eight-readers");
    let (mut writer, reader) = fixture(&path)?;
    let held = records(&reader)?;
    drop(reader);
    std::thread::scope(|scope| -> varlit::Result<()> {
        let (answers, replies) = mpsc::channel();
        let mut commands = Vec::new();
        let mut joins = Vec::new();
        for id in 0..8 {
            let (send, receive) = mpsc::channel();
            commands.push(send);
            let answers = answers.clone();
            let path = &path;
            joins.push(scope.spawn(move || -> varlit::Result<()> {
                let mut reader = varlit::VarlitFile::open_readonly(explicit(), path)?;
                answers.send((id, 0)).unwrap();
                for command in receive {
                    match command {
                        Command::Pending => {
                            let before = varlit::VarlitFile::records_framed();
                            assert_eq!(reader.follow()?, 0);
                            let framed = varlit::VarlitFile::records_framed() - before;
                            assert_eq!(framed, 64, "reader {id} repeated a prefix");
                            for _ in 0..8 {
                                let before = varlit::VarlitFile::records_framed();
                                assert_eq!(reader.follow()?, 0);
                                assert_eq!(varlit::VarlitFile::records_framed() - before, 0);
                            }
                            assert_eq!(records(&reader)?, held);
                            answers.send((id, framed)).unwrap();
                        }
                        Command::Committed => {
                            assert!(reader.follow()? > 0);
                            assert_eq!(records(&reader)?, held + 1025);
                            let mut entries = Vec::new();
                            reader.index_entries_into(&mut entries)?;
                            let mut scratch = Vec::new();
                            let mut reading = Reading::empty();
                            let values = entries
                                .iter()
                                .filter(|e| e.block_id == 10)
                                .map(|e| {
                                    reader
                                        .decode_block_into::<Reading>(e, &mut scratch, &mut reading)
                                        .map(|()| reading.value)
                                })
                                .collect::<varlit::Result<Vec<_>>>()?;
                            assert_eq!(values, (0..1152).collect::<Vec<_>>());
                            answers.send((id, 1025)).unwrap();
                        }
                    }
                }
                Ok(())
            }));
        }
        drop(answers);
        for _ in 0..8 {
            assert_eq!(replies.recv_timeout(Duration::from_secs(15)).unwrap().1, 0);
        }
        for batch in 0..16 {
            for value in 128 + batch * 64..128 + (batch + 1) * 64 {
                writer.push(&Reading { value })?;
            }
            writer.flush()?;
            for send in &commands {
                send.send(Command::Pending).unwrap();
            }
            let mut seen = [false; 8];
            for _ in 0..8 {
                let (id, framed) = replies.recv_timeout(Duration::from_secs(15)).unwrap();
                assert!(!seen[id]);
                seen[id] = true;
                assert_eq!(framed, 64);
            }
        }
        writer.commit()?;
        for send in &commands {
            send.send(Command::Committed).unwrap();
        }
        for _ in 0..8 {
            assert_eq!(
                replies.recv_timeout(Duration::from_secs(15)).unwrap().1,
                1025
            );
        }
        drop(commands);
        for join in joins {
            join.join().unwrap()?;
        }
        Ok(())
    })
}

fn nonresident() -> varlit::FormatSpec {
    explicit()
        .with_liveness_policy(varlit::LivenessPolicy::FooterFlags)
        .with_block_residency(&[varlit::BlockResidencyDescriptor {
            block_id: 10,
            resident: false,
        }])
}
#[test]
fn nonresident_open_reuses_verified_boundaries_without_a_second_scan() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("nonresident");
    let spec = nonresident();
    let mut writer = spec.create(&path)?;
    let mut offsets = Vec::new();
    for value in 0..4096 {
        offsets.push(writer.push_info(&Reading { value })?.record_offset);
    }
    writer.commit()?;
    drop(writer);
    // Full scan is deliberate: this test requires its result to survive open.
    let spec = spec.with_read_limits(
        spec.read_limits
            .with_integrity_verification(varlit::IntegrityVerification::AtOpen),
    );
    let mut writer = spec.open(&path)?;
    varlit::VarlitFile::take_record_entry_faults();
    for sample in 0..512 {
        let at = offsets[(sample * 31337 + 4095) % offsets.len()];
        writer.mark_record_dead(at)?;
    }
    assert_eq!(
        varlit::VarlitFile::take_record_entry_faults(),
        512,
        "open discarded boundary certificates and scanned records again"
    );
    Ok(())
}
#[test]
fn nonresident_appends_extend_certificates_without_scanning_the_prefix() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("appended-boundaries");
    let mut writer = nonresident().create(&path)?;
    for value in 0..1024 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;
    varlit::VarlitFile::take_record_entry_faults();
    for value in 1024..1152 {
        let info = writer.push_info(&Reading { value })?;
        writer.mark_record_dead(info.record_offset)?;
    }
    assert_eq!(
        varlit::VarlitFile::take_record_entry_faults(),
        128,
        "a fresh append lost its already-known boundary"
    );
    Ok(())
}

#[test]
fn an_incomplete_first_record_waits_and_resumes_from_its_start() -> varlit::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("partial");
    let (mut writer, mut reader) = fixture(&path)?;
    let held = records(&reader)?;
    let info = writer.push_info(&Label {
        text: "x".repeat(50_000),
    })?;
    writer.flush()?;
    let mut raw = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?;
    raw.seek(SeekFrom::Start(info.record_offset))?;
    let mut complete = Vec::new();
    raw.read_to_end(&mut complete)?;
    raw.set_len(info.payload_offset + 100)?;
    assert_eq!(reader.follow()?, 0);
    for _ in 0..32 {
        let before = varlit::VarlitFile::records_framed();
        assert_eq!(reader.follow()?, 0);
        assert_eq!(
            varlit::VarlitFile::records_framed() - before,
            0,
            "an unchanged partial first frame was retried"
        );
    }
    assert_eq!(records(&reader)?, held);
    raw.seek(SeekFrom::Start(info.record_offset))?;
    raw.write_all(&complete)?;
    assert_eq!(reader.follow()?, 0);
    writer.commit()?;
    assert!(reader.follow()? > 0);
    assert_eq!(records(&reader)?, held + 2);
    Ok(())
}

#[test]
fn pending_allocation_refusal_preserves_the_snapshot_and_allows_retry() -> varlit::Result<()> {
    for nth in 1..=4 {
        let directory = tempfile::tempdir()?;
        let (mut writer, mut reader) = fixture(&directory.path().join("refusal"))?;
        let held = records(&reader)?;
        for value in 128..2176 {
            writer.push(&Reading { value })?;
        }
        writer.flush()?;
        let refused = with_refusal(nth, || reader.follow());
        assert!(refused.is_err(), "allocation stage {nth} was not exercised");
        assert_eq!(records(&reader)?, held);
        assert_eq!(reader.follow()?, 0);
        writer.commit()?;
        assert!(reader.follow()? > 0);
        assert_eq!(records(&reader)?, held + 2049);
    }
    Ok(())
}

#[test]
fn nonresident_metadata_refusal_does_not_refuse_logging() -> varlit::Result<()> {
    const CHILD: &str = "VARLIT_TEST_CACHE_NO_SPILL";
    if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new("1")) {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "nonresident_metadata_refusal_does_not_refuse_logging",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("VARLIT_SPILL_MAX_BYTES", "0")
            .status()?;
        assert!(status.success(), "a refused optional cache broke logging");
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("no-private-spill");
    let spec = nonresident();
    let mut writer = spec.create(&path)?;
    for value in 0..4096 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;
    drop(writer);
    let mut writer = spec.open(&path)?;
    for value in 4096..8192 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;
    let reader = varlit::VarlitFile::open_readonly(spec, &path)?;
    let mut count = 0;
    for entry in reader.block_chain(10)? {
        entry?;
        count += 1;
    }
    assert_eq!(count, 8192, "refused metadata caused data loss");
    Ok(())
}

#[test]
fn writer_keeps_ingesting_while_eight_readers_decode_queued_commits() -> varlit::Result<()> {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("independent-stream");
    let (mut writer, pinned) = fixture(&path)?;
    let held = records(&pinned)?;
    let label = Label {
        text: "stream".repeat(8192),
    };
    std::thread::scope(|scope| -> varlit::Result<()> {
        let (answers, receive) = mpsc::channel();
        let mut sends = Vec::new();
        let mut joins = Vec::new();
        for id in 0..8 {
            let path = &path;
            let answers = answers.clone();
            let (send, chunks) = mpsc::channel::<Arc<Vec<(u64, u64)>>>();
            sends.push(send);
            joins.push(scope.spawn(move || -> varlit::Result<()> {
                let mut reader = varlit::VarlitFile::open_readonly(explicit(), path)?;
                answers.send((id, 0, 0)).unwrap();
                let mut scratch = Vec::new();
                let mut reading = Reading::empty();
                let (mut count, mut trace) = (0, 0u64);
                for chunk in chunks {
                    for _ in 0..8 {
                        reader.follow()?;
                    }
                    for &(offset, expected) in chunk.iter() {
                        let entry = reader.record_entry_at(offset)?;
                        reader.decode_block_into::<Reading>(&entry, &mut scratch, &mut reading)?;
                        assert_eq!(reading.value, expected);
                        count += 1;
                        trace = trace
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(reading.value);
                    }
                }
                answers.send((id, count, trace)).unwrap();
                Ok(())
            }));
        }
        drop(answers);
        for _ in 0..8 {
            assert_eq!(receive.recv_timeout(Duration::from_secs(30)).unwrap().1, 0);
        }
        // No reader acknowledgement is awaited in this loop. Variable records
        // exercise partial framing while readers decode preceding fixed records.
        for phase in 0..64 {
            let mut chunk = Vec::new();
            for n in 0..128 {
                let value = 128 + phase * 128 + n;
                let info = writer.push_info(&Reading { value })?;
                chunk.push((info.record_offset, value));
            }
            writer.push(&label)?;
            writer.commit()?;
            let chunk = Arc::new(chunk);
            for send in &sends {
                send.send(Arc::clone(&chunk)).unwrap();
            }
        }
        sends.clear();
        let mut seen = [false; 8];
        let mut trace = None;
        for _ in 0..8 {
            let (id, count, actual) = receive.recv_timeout(Duration::from_secs(30)).unwrap();
            assert!(!seen[id]);
            seen[id] = true;
            assert_eq!(count, 8192);
            assert_eq!(*trace.get_or_insert(actual), actual);
        }
        for join in joins {
            join.join().unwrap()?;
        }
        Ok(())
    })?;
    assert_eq!(records(&pinned)?, held);
    Ok(())
}

#[test]
fn publication_allocation_refusal_preserves_the_snapshot_and_retries() -> varlit::Result<()> {
    for nth in 1..=4 {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("publication-refusal");
        let (mut writer, mut reader) = fixture(&path)?;
        let held = records(&reader)?;
        for value in 128..4224 {
            writer.push(&Reading { value })?;
        }
        writer.flush()?;
        assert_eq!(reader.follow()?, 0);
        writer.commit()?;
        let result = with_refusal(nth, || reader.follow());
        assert!(
            result.is_err(),
            "publication allocation site {nth} did not refuse"
        );
        assert_eq!(records(&reader)?, held);
        assert!(
            reader.follow()? > 0,
            "publication retry failed at site {nth}"
        );
        assert_eq!(records(&reader)?, held + 4097);
    }
    Ok(())
}

#[test]
fn publication_replay_reuses_spilled_staging_without_stale_rows() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("spilled-replay");
    let spec =
        explicit().with_resource_limits(varlit::ReadLimits::STANDARD.with_max_index_bytes(2048));
    let mut writer = spec.create(&path)?;
    let first = writer.push_info(&Reading { value: 41 })?;
    writer.commit()?;
    let pinned = varlit::VarlitFile::open_readonly(spec, &path)?;
    let mut reader = varlit::VarlitFile::open_readonly(spec, &path)?;
    let mut expected = Vec::new();
    for value in 0..512 {
        let info = writer.push_info(&Reading { value })?;
        expected.push((info.record_offset, value));
    }
    writer.flush()?;
    assert_eq!(reader.follow()?, 0);
    assert!(reader.record_entry_at(expected[0].0).is_err());
    writer.commit()?;
    assert!(reader.follow()? > 0);
    let mut scratch = Vec::new();
    let mut reading = Reading::empty();
    for &(offset, value) in &expected {
        let entry = reader.record_entry_at(offset)?;
        reader.decode_block_into::<Reading>(&entry, &mut scratch, &mut reading)?;
        assert_eq!(reading.value, value);
    }
    assert!(pinned.record_entry_at(expected[0].0).is_err());
    let entry = pinned.record_entry_at(first.record_offset)?;
    pinned.decode_block_into::<Reading>(&entry, &mut scratch, &mut reading)?;
    assert_eq!(reading.value, 41);
    Ok(())
}
