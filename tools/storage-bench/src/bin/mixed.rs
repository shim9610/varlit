// Handles live on worker stacks; avoid an extra benchmark-only allocation/indirection.
#![allow(clippy::large_enum_variant)]
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use varlit_storage_bench::{
    Result, Stopwatch, file_usage, hdf, matrix_table, model::*, monotonic_ns, native, packed_table,
    sqlite, thread_cpu,
};
type ProducerMetrics = (f64, f64, f64, Vec<(u64, u64)>, Vec<(u64, u64, u64)>);
enum Writer {
    Packed(packed_table::Matrix),
    Matrix(matrix_table::Matrix),
    Native(native::Writer),
    Sql(sqlite::Sql),
    Hdf(hdf::Hdf),
}
impl Writer {
    fn create(e: &str, p: &Path, c: &Config) -> Result<Self> {
        Ok(match e {
            "varlit-matrix-packed" => Self::Packed(packed_table::Matrix::open(p, c, true)?),
            "varlit-matrix" => Self::Matrix(matrix_table::Matrix::open(p, c, true)?),
            "varlit-indexed" | "varlit-stream" => {
                let mut w = native::Writer::create(p, c, e == "varlit-indexed")?;
                w.initial()?;
                Self::Native(w)
            }
            "sqlite" => Self::Sql(sqlite::Sql::open(p, c, true, true)?),
            "hdf5" => Self::Hdf(hdf::Hdf::open(p, c, true)?),
            _ => return Err("unknown engine".into()),
        })
    }
    fn append(&mut self, t: &Transaction, h: &mut Head, c: &Config) -> Result<()> {
        match self {
            Self::Packed(w) => w.append(t, h)?,
            Self::Matrix(w) => w.append(t, h)?,
            Self::Native(w) => w.append(t, h, c)?,
            Self::Sql(w) => *h = w.append(t),
            Self::Hdf(w) => w.append(t, h),
        }
        Ok(())
    }
    fn maintain(&mut self, p: &Path, c: &Config, last: &mut u64) -> Result<Option<(u64, u64)>> {
        if c.compact_bytes == 0 {
            return Ok(None);
        }
        if let Self::Native(native::Writer::Indexed(w)) = self {
            let bytes = std::fs::metadata(varlit::disk_index_sidecar_path(p))?.len();
            if bytes >= c.compact_bytes.max(last.saturating_mul(2)) {
                let result = w.compact_index()?;
                *last = result.after_bytes;
                return Ok(Some((result.before_bytes, result.after_bytes)));
            }
        }
        if let Self::Matrix(w) = self {
            let before = w.generation_bytes()?;
            if before >= c.compact_bytes.max(last.saturating_mul(2)) {
                w.compact()?;
                let after = w.generation_bytes()?;
                *last = after;
                return Ok(Some((before, after)));
            }
        }
        if let Self::Packed(w) = self {
            let before = w.generation_bytes()?;
            if before >= c.compact_bytes.max(last.saturating_mul(2)) {
                w.compact()?;
                let after = w.generation_bytes()?;
                *last = after;
                return Ok(Some((before, after)));
            }
        }
        Ok(None)
    }
    fn finish(&mut self) {
        match self {
            Self::Sql(w) => w.finish(),
            Self::Hdf(w) => w.finish(),
            _ => {}
        }
    }
}
enum Reader {
    Packed(packed_table::Matrix),
    Matrix(matrix_table::Matrix),
    Native(native::Reader),
    Sql(sqlite::Sql),
    Hdf(hdf::Hdf),
}
impl Reader {
    fn open(e: &str, p: &Path, c: &Config) -> Result<Self> {
        Ok(match e {
            "varlit-matrix-packed" => Self::Packed(packed_table::Matrix::open(p, c, false)?),
            "varlit-matrix" => Self::Matrix(matrix_table::Matrix::open(p, c, false)?),
            "varlit-indexed" | "varlit-stream" => {
                Self::Native(native::Reader::open(p, c, e == "varlit-indexed")?)
            }
            "sqlite" => {
                let r = sqlite::Sql::open(p, c, false, false)?;
                r.exec("BEGIN");
                Self::Sql(r)
            }
            "hdf5" => Self::Hdf(hdf::Hdf::open(p, c, false)?),
            _ => return Err("unknown engine".into()),
        })
    }
    fn refresh(&mut self) -> Result<Head> {
        Ok(match self {
            Self::Packed(r) => r.refresh()?,
            Self::Matrix(r) => r.refresh()?,
            Self::Native(r) => r.refresh()?,
            Self::Sql(r) => r.refresh(),
            Self::Hdf(r) => r.refresh(),
        })
    }
    fn consume(&mut self, old: &Head, h: &Head, o: &mut Oracle, c: &Config) -> Result<()> {
        match self {
            Self::Packed(r) => r.consume(old, h, o, c)?,
            Self::Matrix(r) => r.consume(old, h, o, c)?,
            Self::Native(r) => r.consume(h, o, c)?,
            Self::Sql(r) => r.consume(old, h, o, c),
            Self::Hdf(r) => r.consume(old, h, o, c),
        }
        Ok(())
    }
}
fn read(e: &str, root: &Path, c: &Config, id: usize) -> Result<()> {
    let all = Stopwatch::start();
    let t = Instant::now();
    let mut r = Reader::open(e, &root.join("data/store"), c)?;
    let open = t.elapsed().as_secs_f64();
    let mut old = r.refresh()?;
    assert_eq!(old.txns, 0, "writer began before readers opened");
    let mut oracle = Oracle::new(c);
    let mut consume_intervals = Vec::new();
    std::fs::write(root.join(format!("ready-{id}")), b"ready")?;
    let (mut refresh, mut consume, mut polls, mut max_pending) = (0.0, 0.0, 0u64, 0u64);
    let deadline = Instant::now() + Duration::from_secs(1800);
    loop {
        if Instant::now() > deadline {
            return Err("reader timeout".into());
        }
        let interval_start = if c.telemetry { monotonic_ns() } else { 0 };
        let t = Instant::now();
        let h = r.refresh()?;
        refresh += t.elapsed().as_secs_f64();
        polls += 1;
        if std::env::var_os("VARLIT_BENCH_VISIBILITY_GATE").is_some()
            && root.join("staged").exists()
            && !root.join(format!("observed-{id}")).exists()
        {
            assert_eq!(h.txns, 0, "unpublished transaction became visible");
            assert_eq!(h.frames + h.rows + h.metas, 0);
            std::fs::write(root.join(format!("observed-{id}")), b"previous-prefix")?;
        }
        assert!(
            h.txns >= old.txns
                && h.frames >= old.frames
                && h.rows >= old.rows
                && h.metas >= old.metas
        );
        max_pending = max_pending.max(h.frames + h.rows - old.frames - old.rows);
        let t = Instant::now();
        if h.txns > old.txns {
            let before_bytes = oracle.bytes;
            r.consume(&old, &h, &mut oracle, c)?;
            if c.telemetry {
                consume_intervals.push((
                    interval_start,
                    monotonic_ns(),
                    old.txns,
                    h.txns,
                    oracle.bytes - before_bytes,
                ));
            }
        } else {
            assert_eq!(
                (h.frames, h.rows, h.metas),
                (old.frames, old.rows, old.metas)
            );
        }
        consume += t.elapsed().as_secs_f64();
        old = h;
        if old.txns as usize == c.transactions() {
            break;
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    oracle.finish();
    if c.lookups && c.group_items != 0 {
        assert_eq!(oracle.point_items, c.records * c.group_items);
    }
    if c.lookups {
        assert_eq!(oracle.lookup_count, c.records);
    }
    let (seconds, cpu) = all.stop();
    std::fs::write(
        root.join(format!("reader-{id}.json")),
        serde_json::to_vec(
            &serde_json::json!({"id":id,"seconds":seconds,"cpu_seconds":cpu,"open_seconds":open,"refresh_seconds":refresh,"consume_seconds":consume,"polls":polls,"max_observed_pending_records":max_pending,"records":oracle.data_count,"logical_records":oracle.data_count*c.group_items.max(1),"point_items":oracle.point_items,"consume_intervals":consume_intervals,"metadata":oracle.meta_count,"payload_bytes":oracle.bytes,"lookups":oracle.lookup_count,"order_hash":oracle.order_hash,"metadata_hash":oracle.metadata_hash,"verified":true}),
        )?,
    )?;
    Ok(())
}
struct Children(Vec<Child>);
impl Drop for Children {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
fn run(e: &str, root: &Path, c: &Config, config: &Path) -> Result<()> {
    if root.exists() {
        return Err(
            "output directory already exists; use a fresh path to retain failed attempts".into(),
        );
    }
    if c.lookups
        && (e == "varlit-stream" || (e == "hdf5" && c.scenario == "mixed" && c.group_items == 0))
    {
        return Err("requested lookup semantics are unsupported by this adapter".into());
    }
    c.validate()?;
    std::fs::create_dir_all(root.join("data"))?;
    let all = Stopwatch::start();
    let t = Instant::now();
    let mut writer = Writer::create(e, &root.join("data/store"), c)?;
    let initialize = t.elapsed().as_secs_f64();
    let mut children = Children(Vec::new());
    for id in 0..c.readers {
        children.0.push(
            Command::new(std::env::current_exe()?)
                .args(["reader", e])
                .arg(root)
                .arg(config)
                .arg(id.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?,
        );
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while !(0..c.readers).all(|id| root.join(format!("ready-{id}")).exists()) {
        for ch in &mut children.0 {
            if let Some(s) = ch.try_wait()? {
                return Err(format!("reader failed to initialize: {s}").into());
            }
        }
        if Instant::now() > deadline {
            return Err("reader initialization timeout".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let t = Instant::now();
    let mut backend = 0.0;
    let mut writer_cpu = 0.0;
    let mut producer_cpu = 0.0;
    let mut queue_send = 0.0;
    let mut head = Head::default();
    let (mut compact_count, mut compact_wall, mut compact_cpu, mut compact_after) =
        (0u64, 0.0, 0.0, 0u64);
    let mut commits = Vec::with_capacity(c.transactions());
    let mut publication_spans = Vec::new();
    let writer_active_start_ns = monotonic_ns();
    if e == "sqlite" && c.producers > 1 {
        // Each producer owns a separate native connection. SQLite serializes transactions.
        std::thread::scope(|s| -> Result<()> {
            let mut joins = Vec::new();
            for source in 0..c.producers {
                joins.push(s.spawn(move || -> Result<ProducerMetrics> {
                    let w = sqlite::Sql::open(&root.join("data/store"), c, true, false)?;
                    let mut b = 0.0;
                    let cpu = thread_cpu();
                    let mut generation = 0.0;
                    let mut order = Vec::new();
                    let mut spans = Vec::new();
                    for local in 0..c.txns_per_producer() {
                        let t = Instant::now();
                        let tx = c.transaction(source, local);
                        generation += t.elapsed().as_secs_f64();
                        let t = Instant::now();
                        let start_ns = if c.telemetry { monotonic_ns() } else { 0 };
                        let h = w.append(&tx);
                        if c.telemetry {
                            spans.push((h.txns, start_ns, monotonic_ns()));
                        }
                        order.push((h.txns, tx.txid));
                        b += t.elapsed().as_secs_f64();
                    }
                    Ok((b, thread_cpu() - cpu, generation, order, spans))
                }));
            }
            for j in joins {
                let (b, cpu, generation, order, spans) =
                    j.join().map_err(|_| "SQLite producer panicked")??;
                commits.extend(order);
                publication_spans.extend(spans);
                backend += b;
                producer_cpu += cpu;
                queue_send += generation;
            }
            Ok(())
        })?;
    } else {
        // Bounded MPSC is the sole handoff into a non-shared writer handle.
        let (tx, rx) = mpsc::sync_channel::<Transaction>(2);
        std::thread::scope(|s| -> Result<()> {
            let mut joins = Vec::new();
            for source in 0..c.producers {
                let tx = tx.clone();
                joins.push(s.spawn(move || -> Result<(f64, f64)> {
                    let cpu = thread_cpu();
                    let mut send = 0.0;
                    for local in 0..c.txns_per_producer() {
                        let t = c.transaction(source, local);
                        let start = Instant::now();
                        tx.send(t)?;
                        send += start.elapsed().as_secs_f64();
                    }
                    Ok((thread_cpu() - cpu, send))
                }));
            }
            drop(tx);
            for t in rx {
                let start = Stopwatch::start();
                let start_ns = if c.telemetry { monotonic_ns() } else { 0 };
                writer.append(&t, &mut head, c)?;
                if c.telemetry {
                    publication_spans.push((head.txns, start_ns, monotonic_ns()));
                }
                let maintenance = Stopwatch::start();
                if writer
                    .maintain(&root.join("data/store"), c, &mut compact_after)?
                    .is_some()
                {
                    let (w, cpu) = maintenance.stop();
                    compact_count += 1;
                    compact_wall += w;
                    compact_cpu += cpu;
                }
                commits.push((head.txns, t.txid));
                let (w, cpu) = start.stop();
                backend += w;
                writer_cpu += cpu;
            }
            for j in joins {
                let (cpu, send) = j.join().map_err(|_| "MPSC producer panicked")??;
                producer_cpu += cpu;
                queue_send += send;
            }
            Ok(())
        })?;
    }
    let writer_seconds = t.elapsed().as_secs_f64();
    let writer_active_end_ns = monotonic_ns();
    let readers_start = Instant::now();
    for ch in &mut children.0 {
        let status = ch.wait()?;
        if !status.success() {
            return Err(format!("reader failed: {status}").into());
        }
    }
    let drain = readers_start.elapsed().as_secs_f64();
    let t = Instant::now();
    let actual_sqlite_settings = if let Writer::Sql(w) = &writer {
        Some(w.settings())
    } else {
        None
    };
    writer.finish();
    drop(writer);
    let finalize = t.elapsed().as_secs_f64();

    let (logical, allocated) = file_usage(&root.join("data"))?;
    let mut final_files = Vec::new();
    for entry in std::fs::read_dir(root.join("data"))? {
        use std::os::unix::fs::MetadataExt;
        let entry = entry?;
        let m = entry.metadata()?;
        final_files.push(serde_json::json!({"name":entry.file_name().to_string_lossy(),"logical_bytes":m.len(),"allocated_bytes":m.blocks()*512}));
    }
    let readers = (0..c.readers)
        .map(|i| -> Result<serde_json::Value> {
            Ok(serde_json::from_slice(&std::fs::read(
                root.join(format!("reader-{i}.json")),
            )?)?)
        })
        .collect::<Result<Vec<_>>>()?;
    commits.sort_unstable();
    assert_eq!(commits.len(), c.transactions());
    let (mut expected_order, mut expected_metadata) = (0, 0);
    for (i, &(seq, txid)) in commits.iter().enumerate() {
        assert_eq!(seq, i as u64 + 1);
        let source = txid as usize % c.producers;
        let local = txid as usize / c.producers;
        let start = local * c.per_tx;
        let n = c.per_tx.min(c.records / c.producers - start);
        for j in 0..n {
            expected_order = ordered_hash(
                expected_order,
                (source * (c.records / c.producers) + start + j) as u64,
            );
        }
        for flag in 0..2 {
            expected_metadata = ordered_hash(ordered_hash(expected_metadata, txid), flag);
        }
    }
    for r in &readers {
        assert_eq!(r["order_hash"].as_u64(), Some(expected_order));
        assert_eq!(r["metadata_hash"].as_u64(), Some(expected_metadata));
    }
    let (seconds, coordinator_cpu) = all.stop();
    let result = serde_json::json!({"schema":1,"scenario":c.scenario,"engine":e,"config":c,"writer_active_start_ns":writer_active_start_ns,"writer_active_end_ns":writer_active_end_ns,"publication_spans":publication_spans,"initialization_seconds":initialize,"writer_seconds":writer_seconds,"writer_backend_sum_seconds":backend,"storage_compactions":compact_count,"storage_compaction_seconds":compact_wall,"storage_compaction_cpu_seconds":compact_cpu,"writer_thread_cpu_seconds":writer_cpu,"producer_thread_cpu_seconds":producer_cpu,"mpsc_send_blocked_and_handoff_sum_seconds":if e=="sqlite"&&c.producers>1{None}else{Some(queue_send)},"sqlite_multiwriter_input_generation_sum_seconds":if e=="sqlite"&&c.producers>1{Some(queue_send)}else{None},"reader_drain_seconds":drain,"finalize_seconds":finalize,"seconds":seconds,"coordinator_thread_cpu_seconds":coordinator_cpu,"file_bytes":logical,"allocated_bytes":allocated,"final_files":final_files,"readers":readers,"sqlite":sqlite::facts(),"sqlite_settings":actual_sqlite_settings,"hdf5":hdf::facts(),"verified":true});
    println!("{}", result);
    Ok(())
}
fn main() -> Result<()> {
    let a = std::env::args().collect::<Vec<_>>();
    if a.len() < 5 {
        return Err("usage: mixed run|reader ENGINE ROOT CONFIG.json [READER_ID]".into());
    }
    let root = PathBuf::from(&a[3]);
    let config = std::fs::canonicalize(&a[4])?;
    let c: Config = serde_json::from_slice(&std::fs::read(&config)?)?;
    c.validate()?;
    match a[1].as_str() {
        "run" => run(&a[2], &root, &c, &config),
        "reader" => read(&a[2], &root, &c, a[5].parse()?),
        _ => Err("unknown operation".into()),
    }
}
