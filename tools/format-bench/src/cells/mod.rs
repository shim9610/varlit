//! Scenario `cells`: a bounded grid of fixed-size cells updated in place
//! while several readers follow.
//!
//! Situation (the only thing this module decides):
//! - 16,777,216 cells of 1 KiB (16 GiB; `FB_SMALL=1`: 262,144) that exist
//!   before the run (initial load is timed separately).
//! - The application overwrites 64 random cells as one atomic update (a reader
//!   must never see part of it), then records the update number and its
//!   publication time in cell 0. 2000 updates.
//! - Loss bound (`FB_LOSS_MS`): 0 = every visible update durable; otherwise a
//!   durable point at least that often.
//! - Four follower processes read cell 0, the 64 cells of that update and 64
//!   random cells (verified against the deterministic payload).
//! - Objective: updates per second. Runs under the harness memory cap.
//!
//! How cells are stored, grouped, indexed and published is each adapter's
//! choice (`candidates`). An engine without in-place update fails.
use crate::common::*;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod h5;
mod sq;
pub mod tile_proto;
mod varlit_indexed;
mod varlit_matrix;

pub const CELL: usize = 1024;
pub const BATCH: usize = 64;
const POLL: Duration = Duration::from_micros(100);

pub fn cells() -> usize {
    if std::env::var_os("FB_SMALL").is_some() {
        1 << 18
    } else {
        1 << 24
    }
}
pub fn batches() -> u64 {
    std::env::var("FB_BATCHES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000)
}
pub fn loss_ms() -> u64 {
    std::env::var("FB_LOSS_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}
pub fn readers() -> usize {
    std::env::var("FB_READERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4)
}

/// Cells written by update `b` (distinct, never cell 0).
pub fn batch_cells(b: u64) -> Vec<u32> {
    let n = cells() as u64;
    let mut v: Vec<u32> = Vec::with_capacity(BATCH);
    let mut x = b.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    while v.len() < BATCH {
        x = splitmix(x);
        let c = 1 + (x % (n - 1)) as u32;
        if !v.contains(&c) {
            v.push(c);
        }
    }
    v
}
pub fn fill(id: u32, version: u64, extra: u64, out: &mut [u8]) {
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

/// What an adapter implements. The harness calls `load` once with every cell
/// in id order (contiguous runs), then `begin`/`write`/`publish` per update.
pub trait CellWriter {
    /// Initial grid: `data` holds cells `first..first + data.len() / CELL`.
    fn load(&mut self, first: u32, data: &[u8]) -> Result<()> {
        for (i, c) in data.chunks_exact(CELL).enumerate() {
            self.write(first + i as u32, c)?;
        }
        Ok(())
    }
    /// After the whole initial grid was loaded; must leave it durable.
    fn loaded(&mut self) -> Result<()> {
        self.publish(true)
    }
    fn begin(&mut self, _durable: bool) -> Result<()> {
        Ok(())
    }
    fn write(&mut self, id: u32, cell: &[u8]) -> Result<()>;
    fn publish(&mut self, durable: bool) -> Result<()>;
}
pub trait CellReader {
    /// Adopt the latest publication; reads until `end` see one snapshot where
    /// the engine provides snapshots.
    fn refresh(&mut self) -> Result<()>;
    fn read(&mut self, id: u32, out: &mut [u8]) -> Result<()>;
    fn end(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Storage configurations each engine offers for this situation.
pub fn candidates(engine: &str) -> Result<&'static [&'static str]> {
    Ok(match engine {
        "varlit" => &["matrix", "indexed"],
        "sqlite" => sq::CANDIDATES,
        "hdf5" => h5::CANDIDATES,
        "parquet" => &["-"],
        "tile" => tile_proto::CANDIDATES,
        _ => return Err(format!("unknown engine {engine}").into()),
    })
}

fn target(engine: &str, dir: &Path) -> PathBuf {
    dir.join(format!("cells.{engine}"))
}
fn create(engine: &str, cand: &str, p: &Path) -> Result<Box<dyn CellWriter>> {
    match (engine, cand) {
        ("varlit", "matrix") => varlit_matrix::create(p),
        ("varlit", "indexed") => varlit_indexed::create(p),
        ("sqlite", c) => sq::create(p, c),
        ("tile", c) => tile_proto::create(p, c),
        ("hdf5", c) => h5::create(p, c),
        ("parquet", _) => {
            Err("parquet: files are immutable; no in-place cell update (unsupported)".into())
        }
        _ => Err(format!("unknown {engine}/{cand}").into()),
    }
}
fn open(engine: &str, cand: &str, p: &Path) -> Result<Box<dyn CellReader>> {
    match (engine, cand) {
        ("varlit", "matrix") => varlit_matrix::open(p),
        ("varlit", "indexed") => varlit_indexed::open(p),
        ("sqlite", c) => sq::open(p, c),
        ("tile", c) => tile_proto::open(p, c),
        ("hdf5", c) => h5::open(p, c),
        _ => Err(format!("unknown {engine}/{cand}").into()),
    }
}

/// Effort accounting: the adapter source file for (engine, candidate).
pub fn adapter_file(engine: &str, cand: &str) -> &'static str {
    match (engine, cand) {
        ("varlit", "matrix") => "src/cells/varlit_matrix.rs",
        ("varlit", "indexed") => "src/cells/varlit_indexed.rs",
        ("sqlite", _) => "src/cells/sq.rs",
        ("hdf5", _) => "src/cells/h5.rs",
        ("tile", _) => "src/cells/tile_proto.rs",
        _ => "",
    }
}

pub fn run(engine: &str, dir: &Path) -> Result<Metrics> {
    let cand = cand();
    let cand = if cand.is_empty() {
        candidates(engine)?[0].to_string()
    } else {
        cand
    };
    let p = target(engine, dir);
    let mut m = Metrics::default();
    m.set("candidate", cand.as_str());
    m.set("cells", cells() as u64);

    let t = Timer::start();
    let mut w = create(engine, &cand, &p)?;
    const RUN: usize = 4096;
    let mut buf = vec![0u8; RUN * CELL];
    for first in (0..cells()).step_by(RUN) {
        for (i, c) in buf.chunks_exact_mut(CELL).enumerate() {
            fill((first + i) as u32, 0, 0, c);
        }
        w.load(first as u32, &buf)?;
    }
    w.loaded()?;
    m.set("init_s", t.secs());
    let init_bytes = dir_bytes(dir);
    m.set("init_bytes", init_bytes);

    let exe = std::env::current_exe()?;
    let t = Timer::start();
    let mut kids = (0..readers())
        .map(|i| {
            std::process::Command::new(&exe)
                .args([
                    "follow-cells",
                    engine,
                    dir.to_str().unwrap(),
                    &i.to_string(),
                ])
                .env("FB_CAND", &cand)
                .spawn()
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(1800);
    while !(0..readers()).all(|i| dir.join(format!("ready-{i}")).exists()) {
        let dead = kids.iter_mut().any(|k| matches!(k.try_wait(), Ok(Some(_))));
        if dead || std::time::Instant::now() > deadline {
            for k in kids.iter_mut() {
                let _ = k.kill();
                let _ = k.wait();
            }
            return Err("followers did not start".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    m.set("reader_open_s", t.secs());

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
    pubs.sort_by(f64::total_cmp);
    m.set("publish_p50_ms", percentile(&pubs, 0.5));
    m.set("publish_p99_ms", percentile(&pubs, 0.99));
    m.set("durable_publications", ndur);
    let (mut p50, mut p99, mut torn, mut obs, mut cpu, mut rss) =
        (0f64, 0f64, 0u64, 0u64, 0f64, 0u64);
    let (mut corrupt, mut first_err) = (0u64, None::<String>);
    for i in 0..readers() {
        let f = dir.join(format!("cells-{i}.json"));
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&f)?)?;
        let mut l: Vec<f64> = v["lags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap())
            .collect();
        l.sort_by(f64::total_cmp);
        p50 = p50.max(percentile(&l, 0.5));
        p99 = p99.max(percentile(&l, 0.99));
        torn += v["torn"].as_u64().unwrap();
        obs += v["observations"].as_u64().unwrap();
        cpu += v["cpu_s"].as_f64().unwrap();
        rss = rss.max(v["peak_rss_mb"].as_u64().unwrap_or(0));
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
    m.set("reader_peak_rss_mb", rss);
    m.set("writer_peak_rss_mb", peak_rss_mb());
    let bytes = dir_bytes(dir);
    m.set("bytes", bytes);
    m.set("growth_bytes", bytes as i64 - init_bytes as i64);
    m.set("loss_ms", loss_ms());
    m.set("readers", readers() as u64);
    m.set("batches", batches());
    Ok(m)
}

pub fn follower_main(engine: &str, dir: &Path, idx: usize) -> Result<()> {
    let cand = cand();
    let p = target(engine, dir);
    let mut r = open(engine, &cand, &p)?;
    // Adopt the initial grid before the writer starts (readers that build
    // their own index pay for it here; `reader_open_s`).
    r.refresh()?;
    r.end()?;
    std::fs::write(dir.join(format!("ready-{idx}")), b"")?;
    let cpu0 = process_cpu_s();
    let n = cells() as u64;
    let (mut last, mut torn, mut obs, mut corrupt) = (0u64, 0u64, 0u64, 0u64);
    let mut first_err: Option<String> = None;
    let mut lags = Vec::new();
    let mut buf = vec![0u8; CELL];
    let mut rng = splitmix(idx as u64 + 1);
    // A read that errors or returns bytes matching no whole version counts as
    // `corrupt` (and the observation as torn) instead of aborting.
    let cell = |r: &mut Box<dyn CellReader>,
                id: u32,
                buf: &mut [u8],
                corrupt: &mut u64,
                first_err: &mut Option<String>|
     -> Option<(u64, u64)> {
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
            let id = 1 + (rng % (n - 1)) as u32;
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
        "corrupt": corrupt, "first_error": first_err, "peak_rss_mb": peak_rss_mb(),
    });
    std::fs::write(dir.join(format!("cells-{idx}.json")), out.to_string())?;
    Ok(())
}
