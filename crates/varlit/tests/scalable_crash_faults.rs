#![allow(unexpected_cfgs)]
#[path = "support/pages.rs"]
mod pages;

#[cfg(feature = "scalable-fault-injection")]
mod enabled {

    use std::collections::{BTreeMap, BTreeSet};
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitStatus, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};

    use varlit::scalable_fault::{
        ArmGuard, FAULT_ENV, REQUIRED_POINTS, TRACE_ENV, arm_from_env, fault_point,
    };
    use varlit::{
        BatchOptions, BlockDescriptor, BlockKind, Decoder, Encoder, Endian, FormatSpec,
        IndexPolicy, IntegrityPolicy, ManifestPolicy, ReadLimits, RecoveryPolicy,
        Result as VarlitResult, StreamOptions, VarlitBlock, VarlitDecode, VarlitEmpty,
        VarlitEncode, VarlitStreamReader, VarlitStreamWriter, WireType, WriterLockBreakPolicy,
        clear_stale_writer_lock,
    };

    const CHILD_ENV: &str = "VARLIT_SCALABLE_FAULT_CHILD";
    const SCENARIO_ENV: &str = "VARLIT_SCALABLE_FAULT_SCENARIO";
    const ROOT_ENV: &str = "VARLIT_SCALABLE_FAULT_ROOT";
    const READY_FILE: &str = "ready";
    const BASE_EOF_FILE: &str = "base-eof";

    const SCENARIOS: &[Scenario] = &[Scenario::Create, Scenario::Append, Scenario::Restore];

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Scenario {
        Create,
        Append,
        Restore,
    }

    impl Scenario {
        const fn name(self) -> &'static str {
            match self {
                Self::Create => "create-stream",
                Self::Append => "append-stream",
                Self::Restore => "restore-stream",
            }
        }

        fn parse(value: &str) -> Self {
            SCENARIOS
                .iter()
                .copied()
                .find(|scenario| scenario.name() == value)
                .unwrap_or_else(|| panic!("unknown scalable fault scenario {value:?}"))
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct TraceEvent {
        point: String,
        occurrence: u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct StreamRecord(u64);

    impl VarlitEmpty for StreamRecord {
        fn empty() -> Self {
            Self(0)
        }
    }

    impl VarlitEncode for StreamRecord {
        const WIRE_TYPE: WireType = WireType::U64;

        fn encode_varlit(&self, encoder: &mut Encoder) -> VarlitResult<()> {
            self.0.encode_varlit(encoder)
        }
    }

    impl VarlitDecode for StreamRecord {
        const WIRE_TYPE: WireType = WireType::U64;

        fn decode_varlit(decoder: &mut Decoder<'_>) -> VarlitResult<Self> {
            Ok(Self(u64::decode_varlit(decoder)?))
        }
    }

    impl VarlitBlock for StreamRecord {
        const ID: u32 = 41;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const SCHEMA_FINGERPRINT: u64 = 0xF159E1E9A0866168;
        const IS_KEYED: bool = false;
    }

    static STREAM_BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: StreamRecord::ID,
        name: "ScalableCrashStreamRecord",
        version: StreamRecord::VERSION,
        kind: StreamRecord::KIND,
        fields: &[],
    }];

    #[test]
    fn scalable_crash_fault_matrix() {
        let root_dir = unique_root("matrix");
        let root = root_dir.path().to_path_buf();
        let mut discovered = Vec::new();
        let mut catalog = BTreeSet::new();

        for scenario in SCENARIOS.iter().copied() {
            let run = root.join(format!("trace-{}", scenario.name()));
            let trace = run.join("trace.tsv");
            fs::create_dir_all(&run).expect("create trace scenario root");
            let status = run_child(scenario, &run, "trace", &trace);
            assert!(
                status.success(),
                "trace child {scenario:?} failed: {status}"
            );
            let events = parse_trace(&trace);
            assert_complete_before_after_pairs(scenario, &events);
            for event in events {
                catalog.insert(event.point.clone());
                discovered.push((scenario, event));
            }
        }

        let required: BTreeSet<_> = REQUIRED_POINTS
            .iter()
            .map(|point| (*point).to_owned())
            .collect();
        assert_eq!(
            catalog, required,
            "fault hooks are not fully wired; the trace catalog must exactly match REQUIRED_POINTS"
        );
        assert!(
            !discovered.is_empty(),
            "fault trace contained no wired points"
        );

        let fault_cases = discovered.len();
        for (index, (scenario, event)) in discovered.into_iter().enumerate() {
            let run = root.join(format!(
                "abort-{index:04}-{}-{}-{}",
                scenario.name(),
                event.point,
                event.occurrence
            ));
            let trace = run.join("trace.tsv");
            fs::create_dir_all(&run).expect("create abort scenario root");
            let selection = format!("abort:{}:{}", event.point, event.occurrence);
            let status = run_child(scenario, &run, &selection, &trace);
            assert!(
                !status.success(),
                "selected fault did not abort: {scenario:?} {event:?}"
            );
            assert!(
                run.join(READY_FILE).is_file(),
                "child aborted before explicit arming: {scenario:?} {event:?}"
            );
            assert!(
                parse_trace(&trace).contains(&event),
                "aborting trace did not reach its selected event: {scenario:?} {event:?}"
            );
            assert_boundary_oracle(scenario, &run, &event);
        }

        root_dir.close().expect("remove crash matrix root");
        eprintln!(
            "CRASH_MATRIX scenarios={} fault_points={} aborts={fault_cases} cleanup=verified",
            SCENARIOS.len(),
            REQUIRED_POINTS.len()
        );
    }

    #[test]
    fn scalable_fault_environment_is_inert_until_armed() {
        let root_dir = unique_root("inert");
        let trace = root_dir.path().join("trace.tsv");
        let status = Command::new(env::current_exe().expect("locate integration test executable"))
            .args([
                "--ignored",
                "--exact",
                "enabled::scalable_fault_inert_child",
            ])
            .env(CHILD_ENV, "1")
            .env(FAULT_ENV, "abort:sync.native_sync:1")
            .env(TRACE_ENV, &trace)
            .status()
            .expect("run inert child");
        assert!(
            status.success(),
            "environment alone activated a fault: {status}"
        );
        assert!(!trace.exists(), "an unarmed hook created a trace file");
        root_dir.close().expect("remove inert test root");
    }

    #[test]
    #[ignore = "invoked only as a scalable crash-test subprocess"]
    fn scalable_fault_child() {
        if env::var_os(CHILD_ENV).is_none() {
            return;
        }
        suppress_interactive_fault_reporting();
        let scenario = Scenario::parse(&env::var(SCENARIO_ENV).expect("child scenario"));
        let root = PathBuf::from(env::var_os(ROOT_ENV).expect("child root"));
        run_scenario(scenario, &root);
    }

    #[test]
    #[ignore = "invoked only to prove environment variables do not arm hooks"]
    fn scalable_fault_inert_child() {
        if env::var_os(CHILD_ENV).is_none() {
            return;
        }
        suppress_interactive_fault_reporting();
        fault_point("sync.native_sync");
    }

    /// Keeps a deliberately aborting child process non-interactive.
    ///
    /// The crash matrix aborts one child per traced fault boundary. Without
    /// this, Windows starts `WerFault.exe` for every one of them, which
    /// dominates the suite wall time and can raise error dialogs on a
    /// developer machine. Suppression must happen inside the child, before it
    /// induces the fault, because the abort is `std::process::abort` in the
    /// injected fault point itself.
    #[cfg(windows)]
    fn suppress_interactive_fault_reporting() {
        const SEM_FAILCRITICALERRORS: u32 = 0x0001;
        const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
        const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;
        const WER_FAULT_REPORTING_FLAG_NOHEAP: u32 = 0x0001;
        const WER_FAULT_REPORTING_NO_UI: u32 = 0x0020;
        const WER_FAULT_REPORTING_FLAG_DISABLE_SNAPSHOT_CRASH: u32 = 0x0040;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetErrorMode(mode: u32) -> u32;
            fn SetThreadErrorMode(mode: u32, old_mode: *mut u32) -> i32;
            fn WerSetFlags(flags: u32) -> i32;
        }

        let mode = SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX;
        let mut previous_thread_mode = 0_u32;
        // SAFETY: both error-mode setters take documented flag bit sets, and
        // the out parameter points at a live local. `WerSetFlags` only takes a
        // documented flag bit set. All three are process/thread-local settings
        // with no memory effects.
        unsafe {
            SetErrorMode(mode);
            SetThreadErrorMode(mode, &raw mut previous_thread_mode);
            WerSetFlags(
                WER_FAULT_REPORTING_NO_UI
                    | WER_FAULT_REPORTING_FLAG_NOHEAP
                    | WER_FAULT_REPORTING_FLAG_DISABLE_SNAPSHOT_CRASH,
            );
        }
    }

    /// The Linux counterpart. Where `core_pattern` pipes to a crash reporter
    /// (Ubuntu's apport), the kernel runs it for every aborted child and holds
    /// the exit until it returns — about a second each, measured at 1.07 s
    /// apart in `apport.log`, which made the matrix's 494 aborts take nearly
    /// nine minutes. A non-dumpable process skips the core-dump path entirely,
    /// so the reporter is never started. This touches only the child's own
    /// process flag, not any system setting.
    #[cfg(target_os = "linux")]
    fn suppress_interactive_fault_reporting() {
        const PR_SET_DUMPABLE: i32 = 4;

        unsafe extern "C" {
            fn prctl(option: i32, ...) -> i32;
        }

        // SAFETY: `PR_SET_DUMPABLE` takes one integer argument (0 or 1) and
        // only changes this process's dumpable flag.
        let result = unsafe { prctl(PR_SET_DUMPABLE, 0_u64) };
        assert_eq!(result, 0, "PR_SET_DUMPABLE failed");
    }

    /// The macOS counterpart. ReportCrash learns of a crash through the Mach
    /// `EXC_CRASH` exception, and writes a report for every aborted child —
    /// the same per-child cost as `WerFault.exe` and apport. Clearing this
    /// task's `EXC_CRASH` port keeps the exception from reaching it; LLVM
    /// disables its crash reports the same way (`LLVM_DISABLE_CRASH_REPORT`
    /// in `Support/Unix/Signals.inc`). Only this child's own task is changed.
    #[cfg(target_os = "macos")]
    fn suppress_interactive_fault_reporting() {
        type MachPort = u32;
        const MACH_PORT_NULL: MachPort = 0;
        const EXC_MASK_CRASH: u32 = 1 << 10;
        const EXCEPTION_STATE_IDENTITY: i32 = 3;
        const MACH_EXCEPTION_CODES: i32 = 0x8000_0000_u32 as i32;
        #[cfg(target_arch = "aarch64")]
        const THREAD_STATE_NONE: i32 = 5;
        #[cfg(target_arch = "x86_64")]
        const THREAD_STATE_NONE: i32 = 13;

        unsafe extern "C" {
            static mach_task_self_: MachPort;
            fn task_set_exception_ports(
                task: MachPort,
                exception_mask: u32,
                new_port: MachPort,
                behavior: i32,
                new_flavor: i32,
            ) -> i32;
        }

        // SAFETY: `mach_task_self_` is the process's own task port, which
        // libSystem initializes before `main`. The call takes only plain
        // integers and changes this task's crash exception port.
        let result = unsafe {
            task_set_exception_ports(
                mach_task_self_,
                EXC_MASK_CRASH,
                MACH_PORT_NULL,
                EXCEPTION_STATE_IDENTITY | MACH_EXCEPTION_CODES,
                THREAD_STATE_NONE,
            )
        };
        assert_eq!(result, 0, "task_set_exception_ports failed: {result}");
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    fn suppress_interactive_fault_reporting() {}

    fn run_child(scenario: Scenario, root: &Path, selection: &str, trace: &Path) -> ExitStatus {
        Command::new(env::current_exe().expect("locate integration test executable"))
            .args(["--ignored", "--exact", "enabled::scalable_fault_child"])
            .env(CHILD_ENV, "1")
            .env(SCENARIO_ENV, scenario.name())
            .env(ROOT_ENV, root)
            .env(FAULT_ENV, selection)
            .env(TRACE_ENV, trace)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap_or_else(|error| panic!("run {scenario:?} child: {error}"))
    }

    fn run_scenario(scenario: Scenario, root: &Path) {
        match scenario {
            Scenario::Create => {
                let _fault_guard = mark_ready_and_arm(root);
                write_stream_generation(root, false);
            }
            Scenario::Append => {
                write_stream_base(root);
                let _fault_guard = mark_ready_and_arm(root);
                append_stream_generation(root);
            }
            Scenario::Restore => {
                write_dirty_stream(root);
                let _fault_guard = mark_ready_and_arm(root);
                let writer =
                    VarlitStreamWriter::open(stream_spec(), native_path(root), stream_options())
                        .expect("recover stream");
                drop(writer);
            }
        }
    }

    fn mark_ready_and_arm(root: &Path) -> ArmGuard {
        fs::write(root.join(READY_FILE), b"ready\n").expect("write ready marker");
        arm_from_env().expect("explicitly arm scalable faults")
    }

    fn write_stream_base(root: &Path) {
        let path = native_path(root);
        let mut writer = VarlitStreamWriter::create(stream_spec(), &path, stream_options())
            .expect("create stream base");
        for value in base_stream_values() {
            writer.push_info(&value).expect("append stream base value");
        }
        writer.sync().expect("sync stream base");
        drop(writer);
        write_base_eof(root);
    }

    fn write_stream_generation(root: &Path, include_base: bool) {
        let path = native_path(root);
        let mut writer = VarlitStreamWriter::create(stream_spec(), &path, stream_options())
            .expect("create stream fixture");
        let values = if include_base {
            base_stream_values()
        } else {
            full_stream_values()
        };
        writer
            .push_iter::<StreamRecord, _>(values, batch_options())
            .map_err(|error| error.source)
            .expect("append deterministic stream chunks");
        writer.sync().expect("sync stream fixture");
    }

    fn append_stream_generation(root: &Path) {
        let path = native_path(root);
        let mut writer = VarlitStreamWriter::open(stream_spec(), &path, stream_options())
            .expect("open stream base");
        writer
            .push_iter::<StreamRecord, _>(new_stream_values(), batch_options())
            .map_err(|error| error.source)
            .expect("append deterministic stream chunks");
        writer.sync().expect("publish stream generation");
    }

    fn write_dirty_stream(root: &Path) {
        write_stream_base(root);
        let path = native_path(root);
        let mut writer = VarlitStreamWriter::open(stream_spec(), &path, stream_options())
            .expect("open stream for dirty tail");
        writer
            .push_iter::<StreamRecord, _>(new_stream_values(), batch_options())
            .map_err(|error| error.source)
            .expect("append dirty stream tail");
    }

    fn assert_boundary_oracle(scenario: Scenario, root: &Path, event: &TraceEvent) {
        clear_stale_writer_lock(
            native_path(root),
            WriterLockBreakPolicy::BreakIfProcessAbsent,
        )
        .unwrap_or_else(|error| {
            panic!("explicit stale-lock recovery failed at {scenario:?} {event:?}: {error}")
        });
        match scenario {
            Scenario::Create => assert_interrupted_stream_creation(root, event),
            Scenario::Append => assert_stream_append_outcome(root, event),
            Scenario::Restore => assert_stream_restore_outcome(root, event),
        }
    }

    fn assert_interrupted_stream_creation(root: &Path, event: &TraceEvent) {
        if let Ok(values) = read_stream(root) {
            assert!(
                values.is_empty() || values == full_stream_values(),
                "partial clean stream at {event:?}: {values:?}"
            );
        }
    }

    /// Opens (and so recovers) the writer and closes it again.
    fn reopen_writer(root: &Path, event: &TraceEvent) {
        drop(
            VarlitStreamWriter::open(stream_spec(), native_path(root), stream_options())
                .unwrap_or_else(|error| {
                    panic!("stream writer reopen failed at {event:?}: {error}")
                }),
        );
    }

    fn assert_stream_append_outcome(root: &Path, event: &TraceEvent) {
        let base_eof = read_base_eof(root);
        let values = read_stream(root)
            .unwrap_or_else(|error| panic!("confirmed stream unreadable at {event:?}: {error}"));
        assert!(
            values == base_stream_values() || values == full_stream_values(),
            "partial stream at {event:?}"
        );
        // Recovery keeps exactly what a reader saw: every verified sync, and
        // nothing after the last one.
        reopen_writer(root, event);
        assert_eq!(
            read_stream(root).unwrap(),
            values,
            "recovery changed the confirmed stream at {event:?}"
        );
        if values == base_stream_values() {
            assert_eq!(fs::metadata(native_path(root)).unwrap().len(), base_eof);
        }
    }

    fn assert_stream_restore_outcome(root: &Path, event: &TraceEvent) {
        assert_eq!(read_stream(root).unwrap(), base_stream_values());
        assert!(
            fs::metadata(native_path(root)).unwrap().len() >= read_base_eof(root),
            "interrupted recovery truncated below its base EOF at {event:?}"
        );
        // A repeated recovery finishes what the interrupted one began.
        reopen_writer(root, event);
        assert_eq!(
            fs::metadata(native_path(root)).unwrap().len(),
            read_base_eof(root)
        );
        assert_eq!(read_stream(root).unwrap(), base_stream_values());
    }

    fn parse_trace(path: &Path) -> Vec<TraceEvent> {
        let contents = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read fault trace {}: {error}", path.display()));
        let required: BTreeSet<_> = REQUIRED_POINTS.iter().copied().collect();
        let mut next = BTreeMap::<String, u64>::new();
        let mut events = Vec::new();
        for (line_number, line) in contents.lines().enumerate() {
            let fields: Vec<_> = line.split('\t').collect();
            assert_eq!(
                fields.len(),
                3,
                "malformed trace line {}: {line:?}",
                line_number + 1
            );
            assert_eq!(
                fields[0],
                "v1",
                "unknown trace version on line {}",
                line_number + 1
            );
            assert!(
                required.contains(fields[1]),
                "unknown point on line {}: {line:?}",
                line_number + 1
            );
            let occurrence = fields[2].parse::<u64>().unwrap_or_else(|_| {
                panic!("invalid occurrence on line {}: {line:?}", line_number + 1)
            });
            let expected = next.entry(fields[1].to_owned()).or_insert(1);
            assert_eq!(
                occurrence,
                *expected,
                "non-consecutive occurrence on line {}",
                line_number + 1
            );
            *expected += 1;
            events.push(TraceEvent {
                point: fields[1].to_owned(),
                occurrence,
            });
        }
        events
    }

    fn assert_complete_before_after_pairs(scenario: Scenario, events: &[TraceEvent]) {
        let mut counts = BTreeMap::<&str, u64>::new();
        for event in events {
            counts.insert(&event.point, event.occurrence);
        }
        for (point, count) in counts {
            assert_eq!(
                count % 2,
                0,
                "trace scenario {scenario:?} did not record both sides of the last {point:?} primitive"
            );
        }
    }

    fn read_stream(root: &Path) -> VarlitResult<Vec<StreamRecord>> {
        let mut blocks =
            VarlitStreamReader::open(stream_spec(), native_path(root), stream_options())?
                .blocks::<StreamRecord>()?;
        let mut record = StreamRecord::empty();
        let mut records = Vec::new();
        while blocks.next_into(&mut record)? {
            records.push(record);
        }
        Ok(records)
    }

    fn stream_spec() -> FormatSpec {
        FormatSpec::new(
            b"VFSTRM",
            1,
            Endian::Little,
            0,
            IndexPolicy::BlockOffsetChain,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            STREAM_BLOCKS,
        )
        .with_block_identities(&[StreamRecord::IDENTITY])
        .with_read_limits(ReadLimits::STANDARD)
    }

    fn stream_options() -> StreamOptions {
        StreamOptions::default()
    }

    const fn batch_options() -> BatchOptions {
        BatchOptions {
            max_records: 2,
            max_bytes: usize::MAX,
        }
    }

    fn base_stream_values() -> Vec<StreamRecord> {
        vec![StreamRecord(10), StreamRecord(20)]
    }

    fn new_stream_values() -> Vec<StreamRecord> {
        (100..105).map(StreamRecord).collect()
    }

    fn full_stream_values() -> Vec<StreamRecord> {
        base_stream_values()
            .into_iter()
            .chain(new_stream_values())
            .collect()
    }

    fn native_path(root: &Path) -> PathBuf {
        root.join("fixture.varlit")
    }

    fn write_base_eof(root: &Path) {
        let eof = fs::metadata(native_path(root))
            .expect("base native metadata")
            .len();
        fs::write(root.join(BASE_EOF_FILE), eof.to_string()).expect("write base EOF");
    }

    fn read_base_eof(root: &Path) -> u64 {
        fs::read_to_string(root.join(BASE_EOF_FILE))
            .expect("read base EOF")
            .parse()
            .expect("parse base EOF")
    }

    fn unique_root(label: &str) -> tempfile::TempDir {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        tempfile::Builder::new()
            .prefix(&format!(
                "varlit-scalable-fault-{label}-{}-{nonce}-",
                std::process::id()
            ))
            .tempdir()
            .expect("create crash test root")
    }
}
