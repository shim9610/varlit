// Included inside the existing crash harness to share its real format and hooks.
use std::time::{Duration, Instant};

const HISTORY_CHILD: &str = "VARLIT_PUBLICATION_HISTORY_CHILD";

#[derive(Clone, Copy, Debug)]
enum Op {
    Put(u64, u64),
    Delete(u64),
    Sync,
    Reopen,
    SyncCompact,
    FailRecover,
}

type Values = BTreeMap<u64, IndexedRecord>;

fn base() -> Values {
    base_indexed_values()
        .into_iter()
        .map(|v| (v.key, v))
        .collect()
}

fn open_reader(root: &Path) -> VarlitIndexedReader {
    VarlitIndexedReader::open(
        indexed_spec(),
        native_path(root),
        disk_options(),
        index_plan(),
    )
    .unwrap()
}

fn values(reader: &VarlitIndexedReader) -> Values {
    let mut value = IndexedRecord::empty();
    let mut values = Values::new();
    for key in 0..8 {
        if reader.get::<IndexedRecord>(&key, &mut value).unwrap() {
            values.insert(key, value.clone());
        }
    }
    values
}

fn reopen(root: &Path) -> VarlitIndexedWriter {
    match VarlitIndexedWriter::open(
        indexed_spec(),
        native_path(root),
        disk_options(),
        index_plan(),
    ) {
        Ok(writer) => writer,
        Err(varlit::Error::DiskIndex(error))
            if matches!(*error, varlit::DiskIndexError::CleanStateRequired) =>
        {
            VarlitIndexedWriter::restore_checkpoint_and_open(
                indexed_spec(),
                native_path(root),
                disk_options(),
                index_plan(),
            )
            .unwrap()
        }
        Err(error) => panic!("unexpected reopen error: {error:?}"),
    }
}

fn apply(writer: &mut Option<VarlitIndexedWriter>, root: &Path, op: Op) {
    match op {
        Op::Put(key, value) => {
            writer
                .as_mut()
                .unwrap()
                .push_info(&IndexedRecord { key, value })
                .unwrap();
        }
        Op::Delete(key) => {
            writer
                .as_mut()
                .unwrap()
                .delete_info::<IndexedRecord>(&key)
                .unwrap();
        }
        Op::Sync => writer.as_mut().unwrap().sync().unwrap(),
        Op::SyncCompact => {
            writer.as_mut().unwrap().sync().unwrap();
            writer.as_mut().unwrap().compact_index().unwrap();
        }
        Op::Reopen => {
            drop(writer.take());
            *writer = Some(reopen(root));
        }
        Op::FailRecover => {
            VarlitStreamWriter::inject_append_rollback_failures(1);
            assert!(matches!(
                writer.as_mut().unwrap().push_info(&IndexedRecord {
                    key: 0,
                    value: u64::MAX
                }),
                Err(varlit::Error::WriteRollbackFailed { .. })
            ));
            assert!(matches!(
                writer
                    .as_mut()
                    .unwrap()
                    .push_info(&IndexedRecord { key: 0, value: 1 }),
                Err(varlit::Error::WriterPoisoned(_))
            ));
            drop(writer.take());
            *writer = Some(reopen(root));
        }
    }
}

// Logical operations, independent of file offsets, index layout and return values.
fn interpret(op: Op, working: &mut Values, committed: &mut Values) {
    match op {
        Op::Put(key, value) => {
            working.insert(key, IndexedRecord { key, value });
        }
        Op::Delete(key) => {
            working.remove(&key);
        }
        Op::Sync | Op::SyncCompact => *committed = working.clone(),
        Op::Reopen | Op::FailRecover => *working = committed.clone(),
    }
}

fn check_history(ops: &[Op]) {
    let root = unique_root("operation-history");
    write_indexed_base(root.path());
    let pin = open_reader(root.path());
    let mut following = open_reader(root.path());
    let mut writer = Some(reopen(root.path()));
    let mut committed = base();
    let mut working = committed.clone();
    for (step, &op) in ops.iter().enumerate() {
        apply(&mut writer, root.path(), op);
        interpret(op, &mut working, &mut committed);
        assert_eq!(values(&pin), base(), "old pin changed at {step}: {ops:?}");
        following.follow().unwrap();
        assert_eq!(
            values(&following),
            committed,
            "follow mixed a commit at {step}: {ops:?}"
        );
        assert_eq!(
            values(&open_reader(root.path())),
            committed,
            "fresh read at {step}: {ops:?}"
        );
    }
    drop(writer);
    drop(following);
    drop(pin);
    root.close().unwrap();
}

#[test]
fn bounded_and_generated_histories_preserve_committed_snapshots() {
    let operations = [
        Op::Put(0, 101),
        Op::Put(1, 202),
        Op::Delete(0),
        Op::Delete(1),
        Op::Sync,
        Op::Reopen,
        Op::SyncCompact,
        Op::FailRecover,
    ];
    let mut histories = 0;
    // Every length-three history over the stated eight-operation alphabet.
    for first in operations {
        for second in operations {
            for third in operations {
                check_history(&[first, second, third]);
                histories += 1;
            }
        }
    }
    // Longer seeded histories revisit keys after failures, compaction and reopen.
    for seed in 1..=16u64 {
        let mut random = seed;
        let mut ops = Vec::new();
        for step in 0..48u64 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let op = match operations[random as usize % operations.len()] {
                Op::Put(key, _) => Op::Put(key, (seed << 32) | step),
                op => op,
            };
            ops.push(op);
        }
        check_history(&ops);
        histories += 1;
    }
    assert_eq!(histories, 528);
    println!("PUBLICATION_HISTORIES exhaustive=512 seeded=16 steps=2304");
}

fn crash_history() -> Vec<Op> {
    vec![
        Op::Put(0, 101),
        Op::Put(2, 202),
        Op::Delete(1),
        Op::Sync,
        Op::SyncCompact,
        Op::Put(0, 303),
        Op::FailRecover,
        Op::Put(1, 404),
        Op::Delete(2),
        Op::Sync,
        Op::Reopen,
        Op::Put(2, 505),
        Op::Put(0, 606),
        Op::Sync,
        Op::Put(0, 707),
    ]
}

fn allowed_commits() -> Vec<Values> {
    let mut working = base();
    let mut committed = working.clone();
    let mut allowed = vec![committed.clone()];
    for op in crash_history() {
        interpret(op, &mut working, &mut committed);
        if matches!(op, Op::Sync | Op::SyncCompact) && allowed.last() != Some(&committed) {
            allowed.push(committed.clone());
        }
    }
    allowed
}

fn allowed_at_interrupted_operation(root: &Path) -> Vec<Values> {
    let step: usize = fs::read_to_string(root.join("operation.txt"))
        .unwrap()
        .parse()
        .unwrap();
    let ops = crash_history();
    assert!(step < ops.len());
    let mut working = base();
    let mut committed = working.clone();
    for &op in &ops[..step] {
        interpret(op, &mut working, &mut committed);
    }
    let mut allowed = vec![committed.clone()];
    // An interrupted sync may have published before it could return. Other
    // operations cannot expose the commit of a future sync in this history.
    if matches!(ops[step], Op::Sync | Op::SyncCompact) {
        allowed.push(working);
    }
    allowed
}

#[test]
#[ignore = "invoked only by the publication-history crash parent"]
fn history_child() {
    if env::var_os(HISTORY_CHILD).is_none() {
        return;
    }
    suppress_interactive_fault_reporting();
    let root = PathBuf::from(env::var_os(ROOT_ENV).unwrap());
    let mut writer = Some(reopen(&root));
    let _fault = mark_ready_and_arm(&root);
    for (step, op) in crash_history().into_iter().enumerate() {
        // Written before entering the operation, outside the fault hooks.
        // The parent reads this only after the child has exited.
        fs::write(root.join("operation.txt"), step.to_string()).unwrap();
        apply(&mut writer, &root, op);
    }
}

struct ObservedChild(std::process::Child);
impl Drop for ObservedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn observed_child(root: &Path, selection: &str, trace: &Path) -> (ExitStatus, usize) {
    let pin = open_reader(root);
    let mut following = open_reader(root);
    let allowed = allowed_commits();
    let mut child = ObservedChild(
        Command::new(env::current_exe().unwrap())
            .args(["--ignored", "--exact", "enabled::histories::history_child"])
            .env(HISTORY_CHILD, "1")
            .env(ROOT_ENV, root)
            .env(FAULT_ENV, selection)
            .env(TRACE_ENV, trace)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut observations = 0;
    let mut previous_commit = 0;
    loop {
        if Instant::now() >= deadline {
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            panic!("publication child exceeded 30 seconds: {selection}");
        }
        assert_eq!(
            values(&pin),
            base(),
            "pinned generation changed: {selection}"
        );
        let previous = values(&following);
        if following.follow().is_err() {
            assert_eq!(
                values(&following),
                previous,
                "failed follow changed its handle"
            );
        }
        let visible = values(&following);
        let generation = allowed
            .iter()
            .position(|state| state == &visible)
            .unwrap_or_else(|| panic!("mixed/unconfirmed snapshot: {selection}: {visible:?}"));
        assert!(generation >= previous_commit, "reader moved backward");
        previous_commit = generation;
        observations += 1;
        if let Some(status) = child.0.try_wait().unwrap() {
            return (status, observations);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn every_mixed_history_crash_boundary_preserves_live_readers_and_recovery() {
    let root = unique_root("live-reader-crashes");
    let discovery = root.path().join("discovery");
    fs::create_dir(&discovery).unwrap();
    write_indexed_base(&discovery);
    let trace = discovery.join("trace.tsv");
    let (status, mut observations) = observed_child(&discovery, "trace", &trace);
    assert!(status.success(), "trace history failed: {status}");
    let events = parse_trace(&trace);
    assert!(!events.is_empty());
    let event_count = events.len();
    let points: BTreeSet<_> = events.iter().map(|event| event.point.as_str()).collect();
    for required in [
        "generation.commit",
        "checkpoint.record_prefix",
        "checkpoint.head_prefix",
        "publish.clean_commit",
        "restore.native_truncate",
        "replace.atomic",
    ] {
        assert!(
            points.contains(required),
            "history did not reach {required}"
        );
    }
    for (index, event) in events.into_iter().enumerate() {
        let run = root.path().join(format!("cut-{index}"));
        fs::create_dir(&run).unwrap();
        write_indexed_base(&run);
        let trace = run.join("trace.tsv");
        let selection = format!("abort:{}:{}", event.point, event.occurrence);
        let (status, reads) = observed_child(&run, &selection, &trace);
        observations += reads;
        assert!(!status.success());
        assert!(
            parse_trace(&trace).contains(&event),
            "child did not reach selected cut"
        );
        clear_stale_writer_lock(
            native_path(&run),
            WriterLockBreakPolicy::BreakIfProcessAbsent,
        )
        .unwrap();
        let pinned_after_crash = open_reader(&run);
        let confirmed = values(&pinned_after_crash);
        assert!(
            allowed_at_interrupted_operation(&run).contains(&confirmed),
            "crash exposed an uncommitted edit"
        );
        let mut recovered = reopen(&run);
        assert_eq!(
            values(&open_reader(&run)),
            confirmed,
            "recovery changed the visible commit"
        );
        recovered
            .push_info(&IndexedRecord {
                key: 7,
                value: 7777,
            })
            .unwrap();
        recovered.sync().unwrap();
        let mut expected = confirmed.clone();
        expected.insert(
            7,
            IndexedRecord {
                key: 7,
                value: 7777,
            },
        );
        assert_eq!(values(&open_reader(&run)), expected);
        assert_eq!(
            values(&pinned_after_crash),
            confirmed,
            "recovery invalidated a pinned reader"
        );
        drop(recovered);
        drop(pinned_after_crash);
    }
    root.close().unwrap();
    println!(
        "PUBLICATION_CRASHES boundaries={event_count} reader_observations={observations} recovery=verified"
    );
}
