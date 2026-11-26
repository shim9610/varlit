#![cfg(feature = "integrity")]

//! Stabilization gates for stream handles: strict model histories, pinned
//! concurrent snapshots, and repeated external process termination with a
//! live reader across each crash. No operation error is discarded.
//! Reproduce a campaign with VARLIT_QUAL_SEED and VARLIT_QUAL_EPOCHS.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use varlit::{
    BatchOptions, StreamOptions, VarlitEmpty, WriterLockBreakPolicy, clear_stale_writer_lock,
    varlit_format,
};

varlit_format! {
    pub format Qualification {
        magic: b"QUAL";
        version: 1;
        schema_hash: computed;
        index: block_offset_chain;
        integrity: crc32;
        blocks {
            variable Item(id = 1) {
                tenant: u32,
                key: u64,
                payload: Vec<u8>,
            }
            fixed Marker(id = 2) {
                value: u64,
            }
        }
    }
}

const KEYS: u64 = 257;
const WAIT: Duration = Duration::from_secs(60);

/// What a reader of the committed generation must see, in file order.
#[derive(Clone, Debug, Default, PartialEq)]
struct Model {
    items: Vec<Item>,
    markers: Vec<u64>,
}

fn options() -> StreamOptions {
    StreamOptions::default()
}

fn item(number: u64, value: u64) -> Item {
    // Composite identities include large integers and the zero boundary.
    let tenant = (number % 5) as u32;
    let key = number.wrapping_mul(0x0100_0000_0000_0001);
    let mut payload = vec![(value ^ number) as u8; [0, 1, 31, 255, 4097][value as usize % 5]];
    payload.extend_from_slice(&value.to_le_bytes());
    Item {
        tenant,
        key,
        payload,
    }
}

/// Blanks the file's published-end slots: what a file written before the
/// block existed says, so the next open frames from the header.
fn blank_published_end(path: &Path) -> varlit::Result<()> {
    let mut bytes = fs::read(path)?;
    let block = bytes[..4096.min(bytes.len())]
        .windows(4)
        .position(|window| window == b"VPUB")
        .expect("a stream file carries the published end");
    // Magic and length, then the version word before the two slots.
    bytes[block + 16..block + 16 + 48].fill(0);
    fs::write(path, bytes)?;
    Ok(())
}

/// Reads everything `reader` sees through fresh walks.
fn snapshot(reader: &QualificationStreamReader) -> varlit::Result<Model> {
    let mut model = Model::default();
    let mut items = reader.items()?;
    let mut item = Item::empty();
    while items.next_into(&mut item)? {
        model.items.push(item.clone());
    }
    let mut markers = reader.markers()?;
    let mut marker = Marker::empty();
    while markers.next_into(&mut marker)? {
        model.markers.push(marker.value);
    }
    Ok(model)
}

fn check(reader: &QualificationStreamReader, model: &Model) -> varlit::Result<()> {
    let seen = snapshot(reader)?;
    assert_eq!(seen.items.len(), model.items.len(), "item count");
    assert_eq!(seen, *model, "reader disagrees with the committed model");
    Ok(())
}

// SplitMix64 is specified here, so seed replay is independent of dependency
// PRNG changes. The oracle is an ordinary vector, not another Varlit read path.
fn random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn configured(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("invalid {name}={value:?}")),
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => panic!("{name}: {error}"),
    }
}

fn check_dirty_reader(path: &Path, committed: &Model) {
    // Reader access does not recover or publish the unfinished generation.
    let mut reader = Qualification::open_stream_reader(path, options()).unwrap();
    check(&reader, committed).unwrap();
    reader.verify_all().unwrap();
    assert_eq!(reader.follow().unwrap(), 0);
}

fn history(seed: u64, epochs: u64) -> varlit::Result<()> {
    assert!(
        epochs >= 4,
        "at least four epochs must exercise all lifecycle paths"
    );
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("history.varlit");
    let mut writer = Qualification::create_stream_writer(&path, options())?;
    writer.sync()?;
    let mut committed = Model::default();
    let mut state = seed;
    let mut operations = [0u64; 5];

    for epoch in 0..epochs {
        eprintln!("qualification-history seed={seed} epoch={epoch}/{epochs}");
        let old = Qualification::open_stream_reader(&path, options())?;
        let old_eof = fs::metadata(&path)?.len();
        let mut pending = committed.clone();
        for step in 0..128 {
            // Each epoch necessarily covers every operation, then randomizes.
            let operation = if step < 5 {
                step
            } else {
                random(&mut state) % 5
            };
            operations[operation as usize] += 1;
            let number = random(&mut state) % KEYS;
            let value = random(&mut state);
            match operation {
                0 | 1 => {
                    let item = item(number, value);
                    writer.push_item(&item)?;
                    pending.items.push(item);
                }
                2 => {
                    // Many records split across small native chunks.
                    let items: Vec<_> = (0..19)
                        .map(|offset| {
                            item((number + offset % 11) % KEYS, value.wrapping_add(offset))
                        })
                        .collect();
                    let report = writer
                        .push_items(
                            &items,
                            BatchOptions {
                                max_records: 3,
                                max_bytes: 512,
                            },
                        )
                        .map_err(|error| error.source)?;
                    assert_eq!(report.records, items.len() as u64);
                    pending.items.extend(items);
                }
                3 => {
                    writer.push_marker(&Marker { value })?;
                    pending.markers.push(value);
                }
                4 => writer.flush()?,
                _ => unreachable!(),
            }
        }
        // Readers opened before the mutation must still see the old
        // generation, although the new bytes are already in the file.
        check(&old, &committed)?;
        drop(old);
        if epoch % 3 == 2 {
            writer.flush()?;
            drop(writer);
            check_dirty_reader(&path, &committed);
            writer = Qualification::open_stream_writer(&path, options())?;
            assert_eq!(
                fs::metadata(&path)?.len(),
                old_eof,
                "restore EOF seed={seed} epoch={epoch}"
            );
        } else {
            writer.sync()?;
            committed = pending;
            drop(writer);
            writer = Qualification::open_stream_writer(&path, options())?;
        }
        let reader = Qualification::open_stream_reader(&path, options())?;
        check(&reader, &committed)?;
        reader.verify_all()?;
        drop(reader);
        if epoch % 4 == 3 {
            // A file whose published end is gone recovers from its markers.
            drop(writer);
            blank_published_end(&path)?;
            check(
                &Qualification::open_stream_reader(&path, options())?,
                &committed,
            )?;
            writer = Qualification::open_stream_writer(&path, options())?;
        }
    }
    assert!(operations.into_iter().all(|count| count > 0));
    eprintln!("QUAL_HISTORY seed={seed} epochs={epochs} operations={operations:?}");
    Ok(())
}

#[test]
fn model_history_regression() -> varlit::Result<()> {
    for seed in [0, 1, 0xdead_beef, u64::MAX] {
        history(seed, 4)?;
    }
    Ok(())
}

#[test]
#[ignore = "long deterministic model campaign; scripts/qualify_scalable.py"]
fn model_history_stress() -> varlit::Result<()> {
    history(
        configured("VARLIT_QUAL_SEED", 42),
        configured("VARLIT_QUAL_EPOCHS", 64),
    )
}

#[test]
fn concurrent_snapshots_survive_many_publications() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("concurrent.varlit");
    let mut writer = Qualification::create_stream_writer(&path, options())?;
    let initial: Vec<_> = (0..KEYS).map(|number| item(number, 0)).collect();
    writer
        .push_items(&initial, BatchOptions::default())
        .map_err(|e| e.source)?;
    writer.sync()?;
    let original = Model {
        items: initial,
        markers: Vec::new(),
    };
    let mut current = original.clone();
    std::thread::scope(|scope| -> varlit::Result<()> {
        let mut workers = Vec::new();
        let mut channels = Vec::new();
        for _ in 0..4 {
            let reader = Qualification::open_stream_reader(&path, options())?;
            let model = &original;
            let (start, receive) = mpsc::channel();
            let (done, ack) = mpsc::channel();
            workers.push(scope.spawn(move || -> varlit::Result<()> {
                while receive.recv_timeout(WAIT).is_ok() {
                    check(&reader, model)?;
                    done.send(())
                        .expect("coordinator dropped acknowledgement receiver");
                }
                Ok(())
            }));
            channels.push((start, ack));
        }
        for epoch in 1..=24 {
            for (start, _) in &channels {
                start.send(()).unwrap();
            }
            let next: Vec<_> = (0..KEYS).map(|number| item(number, epoch)).collect();
            writer
                .push_items(&next, BatchOptions::default())
                .map_err(|e| e.source)?;
            writer.sync()?;
            current.items.extend(next);
            check(
                &Qualification::open_stream_reader(&path, options())?,
                &current,
            )?;
            for (_, ack) in &channels {
                ack.recv_timeout(WAIT)
                    .expect("snapshot worker failed or timed out");
            }
        }
        drop(channels);
        for worker in workers {
            worker.join().expect("snapshot worker panicked")?;
        }
        Ok(())
    })
}

// External kill deliberately bypasses Drop and fault hooks. The ready file is
// written and fsynced only AFTER flush/sync returned. Its contents distinguish
// acknowledged durable generations from deliberately unacknowledged appends.
#[test]
#[ignore = "subprocess entry point, only invoked by kill/recovery tests"]
fn qualification_kill_child() -> varlit::Result<()> {
    let root = std::env::var_os("VARLIT_QUAL_CHILD_ROOT").expect("parent root required");
    let root = Path::new(&root);
    let epoch = configured("VARLIT_QUAL_CHILD_EPOCH", 0);
    assert!(epoch > 0);
    let mut writer = Qualification::open_stream_writer(root.join("killed.varlit"), options())?;
    let items: Vec<_> = (0..KEYS).map(|number| item(number, epoch)).collect();
    writer
        .push_items(&items, BatchOptions::default())
        .map_err(|e| e.source)?;
    writer.push_marker(&Marker { value: epoch })?;
    if epoch.is_multiple_of(2) {
        writer.sync()?;
    } else {
        writer.flush()?;
    }
    let mut ready = File::create(root.join("ready"))?;
    writeln!(ready, "{epoch}")?;
    ready.sync_all()?;
    loop {
        std::thread::park();
    }
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn kill_cycles(cycles: u64) -> varlit::Result<()> {
    assert!(cycles >= 4);
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let path = root.join("killed.varlit");
    Qualification::create_stream_writer(&path, options())?.sync()?;
    let mut expected = Model::default();
    for epoch in 1..=cycles {
        let old_eof = fs::metadata(&path)?.len();
        // A reader that lives across the crash, the recovery and the next
        // publication: it keeps its snapshot, then follows to the new one.
        let mut live = Qualification::open_stream_reader(&path, options())?;
        check(&live, &expected)?;
        let log = File::create(root.join(format!("child-{epoch}.log")))?;
        let mut child = KillOnDrop(
            Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "qualification_kill_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("VARLIT_QUAL_CHILD_ROOT", root)
                .env("VARLIT_QUAL_CHILD_EPOCH", epoch.to_string())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log))
                .spawn()?,
        );
        let started = Instant::now();
        while !root.join("ready").exists()
            || fs::read_to_string(root.join("ready"))? != format!("{epoch}\n")
        {
            assert!(
                child.0.try_wait()?.is_none(),
                "kill child exited: {}",
                fs::read_to_string(root.join(format!("child-{epoch}.log")))?
            );
            assert!(
                started.elapsed() < WAIT,
                "kill child timeout at epoch={epoch}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        child.0.kill()?;
        assert!(
            !child.0.wait()?.success(),
            "child must be terminated externally"
        );
        check(&live, &expected)?;
        clear_stale_writer_lock(&path, WriterLockBreakPolicy::BreakIfProcessAbsent)?;
        let before = expected.clone();
        if epoch.is_multiple_of(2) {
            expected
                .items
                .extend((0..KEYS).map(|number| item(number, epoch)));
            expected.markers.push(epoch);
        } else {
            check_dirty_reader(&path, &expected);
            drop(Qualification::open_stream_writer(&path, options())?);
            assert_eq!(fs::metadata(&path)?.len(), old_eof);
        }
        // The live reader still holds the generation it opened, and following
        // adopts exactly the acknowledged one.
        check(&live, &before)?;
        live.follow()?;
        check(&live, &expected)?;
        live.verify_all()?;
        drop(live);
        let reader = Qualification::open_stream_reader(&path, options())?;
        check(&reader, &expected)?;
        reader.verify_all()?;
        drop(reader);
        fs::remove_file(root.join("ready"))?;
        eprintln!(
            "QUAL_KILL epoch={epoch} durable={}",
            epoch.is_multiple_of(2)
        );
    }
    Ok(())
}

#[test]
fn repeated_external_kill_preserves_acknowledged_generations() -> varlit::Result<()> {
    kill_cycles(4)
}

#[test]
#[ignore = "long external kill/recovery campaign; scripts/qualify_scalable.py"]
fn external_kill_stress() -> varlit::Result<()> {
    kill_cycles(configured("VARLIT_QUAL_KILL_CYCLES", 64))
}

#[test]
fn opening_readers_does_not_interrupt_the_single_writer() -> varlit::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("reader-churn.varlit");
    let mut writer = Qualification::create_stream_writer(&path, options())?;
    writer.push_item(&item(0, 0))?;
    writer.sync()?;
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| -> varlit::Result<()> {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let path = &path;
                let stop = &stop;
                scope.spawn(move || {
                    let mut opens = 0;
                    while !stop.load(Ordering::Acquire) {
                        match Qualification::open_stream_reader(path, options()) {
                            Ok(reader) => {
                                let mut found = Item::empty();
                                assert!(
                                    reader
                                        .items()
                                        .and_then(|mut items| items.next_into(&mut found))
                                        .expect("opened snapshot must remain readable"),
                                );
                                assert_eq!(found, item(0, 0));
                                opens += 1;
                            }
                            Err(error) => panic!("unexpected reader-open failure: {error:?}"),
                        }
                    }
                    opens
                })
            })
            .collect();
        let result = (|| -> varlit::Result<()> {
            for epoch in 1..=100 {
                writer.push_item(&item(epoch % KEYS, epoch))?;
                writer.sync()?;
            }
            Ok(())
        })();
        stop.store(true, Ordering::Release);
        let opened: usize = workers
            .into_iter()
            .map(|w| w.join().expect("reader panicked"))
            .sum();
        eprintln!("concurrent reader opens={opened}, writer result={result:?}");
        assert!(opened > 0, "reader-open contention was not exercised");
        result
    })
}
