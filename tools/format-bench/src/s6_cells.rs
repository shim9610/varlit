//! Scenario 6 (matrix-favored): a bounded grid of fixed-size cells updated in
//! place while several readers follow.
//!
//! 262,144 cells of 1 KiB (256 MiB), as an instrument or analysis grid whose
//! cells are recomputed and rewritten. The writer applies batches of 64
//! random cells; every batch must become visible to followers as a whole
//! (a reader never sees part of a batch) and the latest batch number and
//! its publication time live in cell 0. Loss bound (`FB_LOSS_MS`): 0 = every
//! visible batch durable; otherwise a durable publication at most that often.
//!
//! Followers (separate processes) poll: read cell 0 (latest batch B), then the
//! cells of batch B (all must carry version B) and 64 random cells (version
//! <= B, and a cell can only carry version v if batch v wrote it). Any
//! violation is a torn observation. Every cell's payload is verified.
//!
//! Each engine uses its best in-place mechanism. Parquet files are immutable
//! and have no in-place cell update, so the parquet row fails.
use crate::common::*;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const ROWS: usize = 1024;
pub const COLS: usize = 256;
pub const CELLS: usize = ROWS * COLS;
pub const CELL: usize = 1024;
pub const BATCH: usize = 64;
const POLL: Duration = Duration::from_micros(100);

pub fn batches() -> u64 {
    std::env::var("FB_BATCHES").ok().and_then(|v| v.parse().ok()).unwrap_or(2000)
}
pub fn loss_ms() -> u64 {
    std::env::var("FB_LOSS_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0)
}
pub fn readers() -> usize {
    std::env::var("FB_READERS").ok().and_then(|v| v.parse().ok()).unwrap_or(4)
}

/// Cells written by batch `b` (distinct, never cell 0).
pub fn batch_cells(b: u64) -> Vec<u32> {
    let mut v: Vec<u32> = Vec::with_capacity(BATCH);
    let mut x = b.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    while v.len() < BATCH {
        x = splitmix(x);
        let c = 1 + (x % (CELLS as u64 - 1)) as u32;
        if !v.contains(&c) {
            v.push(c);
        }
    }
    v
}
fn fill(id: u32, version: u64, extra: u64, out: &mut [u8]) {
    out[0..8].copy_from_slice(&version.to_le_bytes());
    out[8..16].copy_from_slice(&(id as u64).to_le_bytes());
    out[16..24].copy_from_slice(&extra.to_le_bytes());
    let mut x = splitmix(((id as u64) << 32) ^ version);
    for c in out[24..].chunks_exact_mut(8) {
        x = splitmix(x);
        c.copy_from_slice(&x.to_le_bytes());
    }
}
/// Checks a cell's payload; returns (version, extra).
fn check(id: u32, b: &[u8]) -> Result<(u64, u64)> {
    let v = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let e = u64::from_le_bytes(b[16..24].try_into().unwrap());
    let mut want = [0u8; CELL];
    fill(id, v, e, &mut want);
    if b != &want[..] {
        return Err(format!("cell {id} payload mismatch at version {v}").into());
    }
    Ok((v, e))
}

pub trait CellWriter {
    /// Called before the first write of a batch: whether it will be durable.
    fn begin(&mut self, _durable: bool) -> Result<()> {
        Ok(())
    }
    fn write(&mut self, id: u32, cell: &[u8]) -> Result<()>;
    fn publish(&mut self, durable: bool) -> Result<()>;
}
pub trait CellReader {
    /// Adopt the latest publication; reads until the next call see one snapshot
    /// where the engine provides snapshots.
    fn refresh(&mut self) -> Result<()>;
    fn read(&mut self, id: u32, out: &mut [u8]) -> Result<()>;
    fn end(&mut self) -> Result<()> {
        Ok(())
    }
}

fn target(fmt: &str, dir: &Path) -> PathBuf {
    dir.join(format!("cells.{fmt}"))
}

// ---------------------------------------------------------------- Varlit matrix
// One matrix cell per grid cell; `sync()` publishes a new generation (the
// matrix has no publication without a storage barrier).
mod vm {
    use super::*;
    use varlit::{MatrixKey, VarlitFile, VarlitReader, varlit_format};

    varlit_format! {
        pub format CellGrid {
            magic: b"CELLGRID";
            version: 1;
            limits {
                record_payload: 67_108_864;
                logical_payload: 268_435_456;
                materialized_bytes: 1_073_741_824;
                matrix_dimension: 16_000_000;
                matrix_cells: 16_000_000;
                matrix_bitmap: 64_000_000;
                matrix_crc: 128_000_000;
                matrix_metadata: 268_435_456;
                matrix_slot_region: 8_589_934_592;
                sidecar: 268_435_456;
            }
            endian: little;
            schema_hash: computed;
            integrity: crc32_with_header;
            dims { r: u32, c: u32, }
            commit: cell_bitmap { keyspace = [r, c]; categories = [data]; };
            blocks {
                matrix Cell(id = 1, dims = [r, c], category = data) { v: [u8; 1024], }
            }
        }
    }
    fn key(id: u32) -> MatrixKey {
        MatrixKey::new((id as usize / COLS) as u64, (id as usize % COLS) as u64)
    }
    pub struct W(VarlitFile);
    pub fn create(p: &Path) -> Result<Box<dyn CellWriter>> {
        let f = CellGrid::spec().create_with_dims(
            p,
            varlit::MatrixDimensions::from_pairs([("r", ROWS as u64), ("c", COLS as u64)]),
        )?;
        Ok(Box::new(W(f)))
    }
    impl CellWriter for W {
        fn write(&mut self, id: u32, cell: &[u8]) -> Result<()> {
            self.0.write_matrix_cell_payload::<Cell>(key(id), cell)?;
            self.0.commit_matrix_cell::<Cell>(key(id))?;
            Ok(())
        }
        fn publish(&mut self, _durable: bool) -> Result<()> {
            self.0.sync()?;
            Ok(())
        }
    }
    pub struct R(VarlitReader);
    pub fn open(p: &Path) -> Result<Box<dyn CellReader>> {
        Ok(Box::new(R(vl_spec(CellGrid::spec()).open_reader(p)?)))
    }
    impl CellReader for R {
        fn refresh(&mut self) -> Result<()> {
            self.0.follow()?;
            Ok(())
        }
        fn read(&mut self, id: u32, out: &mut [u8]) -> Result<()> {
            self.0.read_matrix_run_bytes_into::<Cell>(key(id), out)?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- Varlit native keyed records
// Each cell version is an appended record; a transaction marker publishes a
// batch atomically without a storage barrier (`commit`), `commit_durable` adds
// one (single-barrier). Readers keep their own cell -> newest record map,
// updated from the records each follow adds.
mod vn {
    use super::*;
    use varlit::{RecordIndexEntry, VarlitBlock, VarlitEmpty, VarlitFile, varlit_format};

    varlit_format! {
        pub format CellLog {
            magic: b"CELLLOG1";
            version: 1;
            limits {
                record_payload: 67_108_864;
                logical_payload: 268_435_456;
                materialized_bytes: 1_073_741_824;
            }
            endian: little;
            integrity: crc32_with_header;
            index: block_offset_chain;
            commit: transaction_marker(explicit, single_barrier);
            blocks {
                variable CellRec(id = 1) { id: u32, data: Vec<u8>, }
            }
        }
    }
    pub struct W {
        f: VarlitFile,
        rec: CellRec,
    }
    pub fn create(p: &Path) -> Result<Box<dyn CellWriter>> {
        let mut f = VarlitFile::create(CellLog::spec(), p)?;
        f.commit_durable()?;
        Ok(Box::new(W { f, rec: CellRec { id: 0, data: Vec::with_capacity(CELL) } }))
    }
    impl CellWriter for W {
        fn write(&mut self, id: u32, cell: &[u8]) -> Result<()> {
            self.rec.id = id;
            self.rec.data.clear();
            self.rec.data.extend_from_slice(cell);
            self.f.push_info(&self.rec)?;
            Ok(())
        }
        fn publish(&mut self, durable: bool) -> Result<()> {
            if durable {
                self.f.commit_durable()?;
            } else {
                self.f.commit()?;
            }
            Ok(())
        }
    }
    pub struct R {
        f: VarlitFile,
        map: Vec<Option<RecordIndexEntry>>,
        seen: Option<u64>,
        scratch: Vec<u8>,
        out: CellRec,
    }
    pub fn open(p: &Path) -> Result<Box<dyn CellReader>> {
        Ok(Box::new(R {
            f: VarlitFile::open_readonly(vl_spec(CellLog::spec()), p)?,
            map: vec![None; CELLS],
            seen: None,
            scratch: Vec::new(),
            out: CellRec::empty(),
        }))
    }
    impl CellReader for R {
        fn refresh(&mut self) -> Result<()> {
            self.f.follow()?;
            let tail = self.f.block_tail_offset(CellRec::ID);
            if tail == self.seen {
                return Ok(());
            }
            // Newest first: the first entry met for a cell is its newest.
            let mut fresh: Vec<RecordIndexEntry> = Vec::new();
            for e in self.f.block_chain(CellRec::ID)? {
                let e = e?;
                if Some(e.record_offset) <= self.seen {
                    break;
                }
                fresh.push(e);
            }
            for e in fresh.into_iter().rev() {
                self.f.decode_block_into(&e, &mut self.scratch, &mut self.out)?;
                self.map[self.out.id as usize] = Some(e);
            }
            self.seen = tail;
            Ok(())
        }
        fn read(&mut self, id: u32, out: &mut [u8]) -> Result<()> {
            let e = self.map[id as usize].clone().ok_or("cell missing")?;
            self.f.decode_block_into(&e, &mut self.scratch, &mut self.out)?;
            if self.out.id != id {
                return Err("cell id mismatch".into());
            }
            out.copy_from_slice(&self.out.data);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- SQLite
// One row per cell; a batch is one transaction. WAL with synchronous=NORMAL
// makes a commit visible without a WAL sync; durable batches run at FULL;
// checkpoints only at durable commits once ~4 MB of WAL accumulated.
mod sq {
    use super::*;
    use rusqlite::{Connection, OpenFlags, params};

    pub struct W {
        c: Connection,
        full: bool,
        wal: usize,
    }
    pub fn create(p: &Path) -> Result<Box<dyn CellWriter>> {
        let c = Connection::open(p)?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE cells(id INTEGER PRIMARY KEY, data BLOB NOT NULL);",
        )?;
        Ok(Box::new(W { c, full: false, wal: 0 }))
    }
    impl CellWriter for W {
        fn begin(&mut self, durable: bool) -> Result<()> {
            if durable != self.full {
                self.c.execute_batch(if durable { "PRAGMA synchronous=FULL" } else { "PRAGMA synchronous=NORMAL" })?;
                self.full = durable;
            }
            self.c.execute_batch("BEGIN")?;
            Ok(())
        }
        fn write(&mut self, id: u32, cell: &[u8]) -> Result<()> {
            self.c
                .prepare_cached("INSERT OR REPLACE INTO cells VALUES (?1, ?2)")?
                .execute(params![id as i64, cell])?;
            self.wal += cell.len();
            Ok(())
        }
        fn publish(&mut self, durable: bool) -> Result<()> {
            self.c.execute_batch("COMMIT")?;
            if durable && self.wal >= 4 << 20 {
                self.c.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
                self.wal = 0;
            }
            Ok(())
        }
    }
    pub struct R(Connection);
    pub fn open(p: &Path) -> Result<Box<dyn CellReader>> {
        Ok(Box::new(R(Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY)?)))
    }
    impl CellReader for R {
        fn refresh(&mut self) -> Result<()> {
            // One read transaction = one snapshot for the whole poll.
            self.0.execute_batch("BEGIN")?;
            Ok(())
        }
        fn read(&mut self, id: u32, out: &mut [u8]) -> Result<()> {
            let mut st = self.0.prepare_cached("SELECT data FROM cells WHERE id = ?1")?;
            let mut rows = st.query([id as i64])?;
            let r = rows.next()?.ok_or("cell missing")?;
            out.copy_from_slice(r.get_ref(0)?.as_blob()?);
            Ok(())
        }
        fn end(&mut self) -> Result<()> {
            self.0.execute_batch("COMMIT")?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- HDF5 SWMR
// A (cells, 1024) u8 dataset, chunked 16 cells; cells rewritten in place,
// SWMR `H5Fflush` publishes, fsync adds durability. SWMR has no multi-write
// atomicity, so a reader can see part of a batch.
mod h5 {
    use super::*;
    use hdf5::{File, OpenMode};
    use ndarray::{ArrayView2, s};

    pub struct W {
        f: File,
        d: hdf5::Dataset,
        p: PathBuf,
    }
    impl CellWriter for W {
        fn write(&mut self, id: u32, cell: &[u8]) -> Result<()> {
            let a = ArrayView2::from_shape((1, CELL), cell)?;
            self.d.write_slice(&a, s![id as usize..id as usize + 1, ..])?;
            Ok(())
        }
        fn publish(&mut self, durable: bool) -> Result<()> {
            self.f.flush()?;
            if durable {
                fsync_file(&self.p)?;
            }
            Ok(())
        }
    }
    pub struct R {
        _f: File,
        d: hdf5::Dataset,
    }
    pub fn open(p: &Path) -> Result<Box<dyn CellReader>> {
        let f = File::open_as(p, OpenMode::ReadSWMR)?;
        let d = f.dataset("cells")?;
        Ok(Box::new(R { _f: f, d }))
    }
    impl CellReader for R {
        fn refresh(&mut self) -> Result<()> {
            self.d.refresh()?;
            Ok(())
        }
        fn read(&mut self, id: u32, out: &mut [u8]) -> Result<()> {
            let v = self.d.read_slice_1d::<u8, _>(s![id as usize, ..])?;
            out.copy_from_slice(v.as_slice().ok_or("layout")?);
            Ok(())
        }
    }
    impl W {
        pub fn swmr(&mut self) -> Result<()> {
            self.f.start_swmr()?;
            Ok(())
        }
    }
    pub fn create_swmr(p: &Path, init: &mut dyn FnMut(&mut dyn CellWriter) -> Result<()>) -> Result<Box<dyn CellWriter>> {
        let f = File::with_options().with_fapl(|a| a.libver_latest()).create(p)?;
        let d = f.new_dataset::<u8>().chunk((16, CELL)).shape((CELLS, CELL)).create("cells")?;
        let mut w = W { f, d, p: p.to_path_buf() };
        init(&mut w)?;
        w.swmr()?;
        Ok(Box::new(w))
    }
}

// ---------------------------------------------------------------- driver
fn initial(w: &mut dyn CellWriter) -> Result<()> {
    let mut buf = vec![0u8; CELL];
    w.begin(true)?;
    for id in 0..CELLS as u32 {
        fill(id, 0, 0, &mut buf);
        w.write(id, &buf)?;
    }
    w.publish(true)
}

pub fn run(fmt: &str, dir: &Path) -> Result<Metrics> {
    let p = target(fmt, dir);
    let t = Timer::start();
    let mut w: Box<dyn CellWriter> = match fmt {
        "varlit-matrix" => vm::create(&p)?,
        "varlit-native" => vn::create(&p)?,
        "sqlite" => sq::create(&p)?,
        "hdf5" => h5::create_swmr(&p, &mut |w| initial(w))?,
        "parquet" => {
            return Err("parquet: files are immutable; no in-place cell update (unsupported)".into());
        }
        _ => return Err("format".into()),
    };
    if fmt != "hdf5" {
        initial(w.as_mut())?;
    }
    let mut m = Metrics::default();
    m.set("init_s", t.secs());
    let init_bytes = dir_bytes(dir);

    let exe = std::env::current_exe()?;
    let mut kids = (0..readers())
        .map(|i| {
            std::process::Command::new(&exe)
                .args(["follow-cells", fmt, dir.to_str().unwrap(), &i.to_string()])
                .spawn()
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !(0..readers()).all(|i| dir.join(format!("ready-{i}")).exists()) {
        if std::time::Instant::now() > deadline {
            for k in kids.iter_mut() {
                let _ = k.kill();
            }
            return Err("followers did not start".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let mut pubs = Vec::new();
    let mut ndur = 0u64;
    let start = std::time::Instant::now();
    let mut write = || -> Result<()> {
        let mut buf = vec![0u8; CELL];
        let mut last_durable = std::time::Instant::now();
        for b in 1..=batches() {
            let durable = loss_ms() == 0
                || b == batches()
                || last_durable.elapsed() >= Duration::from_millis(loss_ms());
            w.begin(durable)?;
            for id in batch_cells(b) {
                fill(id, b, 0, &mut buf);
                w.write(id, &buf)?;
            }
            // Cell 0: latest batch and its publication start time.
            fill(0, b, mono_ns(), &mut buf);
            w.write(0, &buf)?;
            let t = Timer::start();
            w.publish(durable)?;
            pubs.push(t.secs() * 1e3);
            if durable {
                ndur += 1;
                last_durable = std::time::Instant::now();
            }
        }
        Ok(())
    };
    if let Err(e) = write() {
        for k in kids.iter_mut() {
            let _ = k.kill();
            let _ = k.wait();
        }
        return Err(e);
    }
    let wall = start.elapsed().as_secs_f64();
    for k in kids.iter_mut() {
        if !k.wait()?.success() {
            return Err("follower failed".into());
        }
    }
    drop(w);
    m.set("write_s", wall);
    m.set("batches_per_s", batches() as f64 / wall);
    m.set("cells_per_s", (batches() as usize * (BATCH + 1)) as f64 / wall);
    pubs.sort_by(f64::total_cmp);
    m.set("publish_p50_ms", percentile(&pubs, 0.5));
    m.set("publish_p99_ms", percentile(&pubs, 0.99));
    m.set("durable_publications", ndur);
    let (mut p50, mut p99, mut torn, mut obs, mut cpu) = (0f64, 0f64, 0u64, 0u64, 0f64);
    let (mut corrupt, mut first_err) = (0u64, None::<String>);
    for i in 0..readers() {
        let f = dir.join(format!("cells-{i}.json"));
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&f)?)?;
        let mut l: Vec<f64> = v["lags"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
        l.sort_by(f64::total_cmp);
        p50 = p50.max(percentile(&l, 0.5));
        p99 = p99.max(percentile(&l, 0.99));
        torn += v["torn"].as_u64().unwrap();
        obs += v["observations"].as_u64().unwrap();
        cpu += v["cpu_s"].as_f64().unwrap();
        corrupt += v["corrupt"].as_u64().unwrap_or(0);
        if first_err.is_none() {
            first_err = v["first_error"].as_str().map(str::to_owned);
        }
        std::fs::remove_file(f)?;
        std::fs::remove_file(dir.join(format!("ready-{i}")))?;
    }
    m.set("lag_p50_ms", p50);
    m.set("lag_p99_ms", p99);
    m.set("torn", torn);
    m.set("corrupt", corrupt);
    if let Some(e) = first_err {
        eprintln!("first corrupt read: {e}");
    }
    m.set("observations", obs);
    m.set("reader_cpu_s", cpu / readers() as f64);
    m.set("bytes", dir_bytes(dir));
    m.set("growth_bytes", dir_bytes(dir) as i64 - init_bytes as i64);
    m.set("loss_ms", loss_ms());
    m.set("readers", readers() as u64);
    m.set("batches", batches());
    Ok(m)
}

pub fn follower_main(fmt: &str, dir: &Path, idx: usize) -> Result<()> {
    let p = target(fmt, dir);
    let mut r: Box<dyn CellReader> = match fmt {
        "varlit-matrix" => vm::open(&p)?,
        "varlit-native" => vn::open(&p)?,
        "sqlite" => sq::open(&p)?,
        _ => h5::open(&p)?,
    };
    // Adopt the initial grid (readers that index records build their map
    // here) before the writer starts.
    r.refresh()?;
    r.end()?;
    std::fs::write(dir.join(format!("ready-{idx}")), b"")?;
    let cpu0 = process_cpu_s();
    let (mut last, mut torn, mut obs, mut corrupt) = (0u64, 0u64, 0u64, 0u64);
    let mut first_err: Option<String> = None;
    let mut lags = Vec::new();
    let mut buf = vec![0u8; CELL];
    let mut rng = splitmix(idx as u64 + 1);
    // A read that errors or returns a cell whose bytes do not match any
    // whole version is counted as `corrupt` (and the observation as torn)
    // instead of aborting the follower, so the result records it.
    let cell = |r: &mut Box<dyn CellReader>, id: u32, buf: &mut [u8],
                    corrupt: &mut u64, first_err: &mut Option<String>| -> Option<(u64, u64)> {
        match r.read(id, buf).and_then(|()| check(id, buf)) {
            Ok(x) => Some(x),
            Err(e) => {
                *corrupt += 1;
                first_err.get_or_insert_with(|| e.to_string());
                None
            }
        }
    };
    while last < batches() {
        if let Err(e) = r.refresh() {
            corrupt += 1;
            first_err.get_or_insert_with(|| format!("refresh: {e}"));
            let _ = r.end();
            std::thread::sleep(POLL);
            continue;
        }
        let Some((b, ts)) = cell(&mut r, 0, &mut buf, &mut corrupt, &mut first_err) else {
            let _ = r.end();
            std::thread::sleep(POLL);
            continue;
        };
        if b == last {
            r.end()?;
            std::thread::sleep(POLL);
            continue;
        }
        let now = mono_ns();
        let mut bad = false;
        for id in batch_cells(b) {
            match cell(&mut r, id, &mut buf, &mut corrupt, &mut first_err) {
                Some((v, _)) => bad |= v != b,
                None => bad = true,
            }
        }
        for _ in 0..BATCH {
            rng = splitmix(rng);
            let id = 1 + (rng % (CELLS as u64 - 1)) as u32;
            match cell(&mut r, id, &mut buf, &mut corrupt, &mut first_err) {
                Some((v, _)) => bad |= v > b || (v > 0 && !batch_cells(v).contains(&id)),
                None => bad = true,
            }
        }
        r.end()?;
        torn += u64::from(bad);
        obs += 1;
        lags.push(now.saturating_sub(ts) as f64 / 1e6);
        last = b;
    }
    let out = serde_json::json!({
        "lags": lags, "torn": torn, "observations": obs, "cpu_s": process_cpu_s() - cpu0,
        "corrupt": corrupt, "first_error": first_err,
    });
    std::fs::write(dir.join(format!("cells-{idx}.json")), out.to_string())?;
    Ok(())
}
