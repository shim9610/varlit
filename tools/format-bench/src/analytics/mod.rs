//! Scenario `analytics`: an append-only sales table queried analytically.
//!
//! Situation (the only thing this module decides):
//! - 200,000,000 rows (`FB_SMALL=1`: 4,194,304) x 12 columns arrive in time
//!   order, in batches of 65,536 rows: ts i64 (sorted), user i64, category
//!   (32 names, skewed), region (8 names), qty i32, price f64, discount f32,
//!   flags u8, m0..m3 f64.
//! - Then, from a cold cache:
//!   Q1: SELECT category, SUM(price*qty) WHERE qty > 50 GROUP BY category
//!   Q2: 20 x SELECT COUNT(*), SUM(price) WHERE ts BETWEEN a AND b (~0.05 %
//!       of the time range each).
//! - Objective: Q1 + Q2 time. Write time and size are reported. Runs under
//!   the harness memory cap.
//!
//! Chunking, encodings, compression, indexes and statistics are each adapter's
//! choice (`candidates`).
use crate::common::*;
use std::path::{Path, PathBuf};

mod h5;
mod pq;
mod sq;
mod vl;

pub const NCAT: usize = 32;
pub const NREG: usize = 8;
pub const Q2: usize = 20;
pub const BATCH: usize = 65_536;
pub const TS0: i64 = 1_700_000_000_000;

pub fn rows() -> usize {
    if std::env::var_os("FB_SMALL").is_some() {
        4 << 20
    } else {
        200_000_000
    }
}
fn q2_span() -> i64 {
    (rows() as i64 * 10) / 2000
}
pub fn cat_name(i: usize) -> String {
    format!("category_{i:02}")
}
pub fn region_name(i: usize) -> String {
    format!("region_{i}")
}

/// A batch of consecutive rows, column-wise.
#[derive(Default, Clone)]
pub struct Batch {
    pub ts: Vec<i64>,
    pub user: Vec<i64>,
    pub cat: Vec<u8>,
    pub region: Vec<u8>,
    pub qty: Vec<i32>,
    pub price: Vec<f64>,
    pub discount: Vec<f32>,
    pub flags: Vec<u8>,
    pub m: [Vec<f64>; 4],
}
impl Batch {
    pub fn len(&self) -> usize {
        self.ts.len()
    }
    pub fn clear(&mut self) {
        *self = Batch::default();
    }
    /// Rows `a..b` of `src` appended to `self`.
    pub fn extend(&mut self, src: &Batch, a: usize, b: usize) {
        self.ts.extend_from_slice(&src.ts[a..b]);
        self.user.extend_from_slice(&src.user[a..b]);
        self.cat.extend_from_slice(&src.cat[a..b]);
        self.region.extend_from_slice(&src.region[a..b]);
        self.qty.extend_from_slice(&src.qty[a..b]);
        self.price.extend_from_slice(&src.price[a..b]);
        self.discount.extend_from_slice(&src.discount[a..b]);
        self.flags.extend_from_slice(&src.flags[a..b]);
        for k in 0..4 {
            self.m[k].extend_from_slice(&src.m[k][a..b]);
        }
    }
}
fn gen_batch(first: usize, n: usize) -> Batch {
    let mut t = Batch::default();
    for i in first..first + n {
        let mut s = splitmix(i as u64 ^ 0x7AB1E);
        let mut r = || {
            s = splitmix(s);
            s
        };
        t.ts.push(TS0 + i as i64 * 10 + (r() % 10) as i64);
        t.user.push((r() % 100_000) as i64);
        let c = r() % 1024;
        t.cat.push(((c * c) / (1024 * 1024 / NCAT as u64)) as u8);
        t.region.push((r() % NREG as u64) as u8);
        t.qty.push((r() % 100) as i32 + 1);
        t.price.push((r() % 100_000) as f64 / 100.0 + 1.0);
        t.discount.push((r() % 31) as f32 / 100.0);
        t.flags.push((r() & 0x7) as u8);
        for k in 0..4 {
            t.m[k].push((r() >> 11) as f64 / (1u64 << 53) as f64);
        }
    }
    t
}
/// Every batch in arrival order.
pub fn batches() -> impl Iterator<Item = Batch> {
    (0..rows())
        .step_by(BATCH)
        .map(|a| gen_batch(a, BATCH.min(rows() - a)))
}

pub type Q1 = [f64; NCAT];
pub trait Writer {
    fn append(&mut self, b: &Batch) -> Result<()>;
    /// All rows appended; durable when it returns.
    fn finish(&mut self) -> Result<()>;
}
pub trait Analytics {
    fn q1(&mut self) -> Result<Q1>;
    fn q2(&mut self, a: i64, b: i64) -> Result<(u64, f64)>;
}

pub fn candidates(engine: &str) -> Result<&'static [&'static str]> {
    Ok(match engine {
        "varlit" => vl::CANDIDATES,
        "sqlite" => sq::CANDIDATES,
        "hdf5" => h5::CANDIDATES,
        "parquet" => pq::CANDIDATES,
        _ => return Err(format!("unknown engine {engine}").into()),
    })
}
pub fn adapter_files(engine: &str, _cand: &str) -> &'static str {
    match engine {
        "varlit" => "src/analytics/vl.rs",
        "sqlite" => "src/analytics/sq.rs",
        "hdf5" => "src/analytics/h5.rs",
        "parquet" => "src/analytics/pq.rs",
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
    let p: PathBuf = dir.join("sales");
    let mut m = Metrics::default();
    m.set("candidate", cand.as_str());
    m.set("rows", rows() as u64);

    let mut w: Box<dyn Writer> = match engine {
        "varlit" => vl::create(&p, &cand)?,
        "sqlite" => sq::create(&p, &cand)?,
        "hdf5" => h5::create(&p, &cand)?,
        "parquet" => pq::create(&p, &cand)?,
        _ => return Err("engine".into()),
    };
    // Oracle accumulated while generating (generation is not timed).
    let mut q1 = [0.0; NCAT];
    let mut write_s = 0.0;
    let mut first_ts = 0;
    for b in batches() {
        if first_ts == 0 {
            first_ts = b.ts[0];
        }
        for i in 0..b.len() {
            if b.qty[i] > 50 {
                q1[b.cat[i] as usize] += b.price[i] * b.qty[i] as f64;
            }
        }
        let t = Timer::start();
        w.append(&b)?;
        write_s += t.secs();
    }
    let t = Timer::start();
    w.finish()?;
    write_s += t.secs();
    drop(w);
    m.set("write_s", write_s);
    m.set("bytes", dir_bytes(dir));
    m.set("writer_peak_rss_mb", peak_rss_mb());

    let mut s = 0xC0FFEEu64;
    let span = q2_span();
    let ranges: Vec<(i64, i64)> = (0..Q2)
        .map(|_| {
            s = splitmix(s);
            let a = first_ts + (s % (rows() as u64 * 10 - span as u64)) as i64;
            (a, a + span)
        })
        .collect();
    // Q2 oracle: rows whose ts falls in a range are found by index (ts is
    // i*10 + [0, 10)), regenerated in row order.
    let q2: Vec<(u64, f64)> = ranges
        .iter()
        .map(|&(a, b)| {
            let lo = (((a - TS0) / 10 - 1).max(0)) as usize;
            let hi = ((((b - TS0) / 10) + 1) as usize).min(rows());
            let g = gen_batch(lo, hi - lo);
            let (mut n, mut s) = (0u64, 0.0);
            for i in 0..g.len() {
                if g.ts[i] >= a && g.ts[i] <= b {
                    n += 1;
                    s += g.price[i];
                }
            }
            (n, s)
        })
        .collect();

    evict(dir);
    let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 * a.abs().max(1.0);
    let t = Timer::start();
    let mut r: Box<dyn Analytics> = match engine {
        "varlit" => vl::open(&p, &cand)?,
        "sqlite" => sq::open(&p, &cand)?,
        "hdf5" => h5::open(&p, &cand)?,
        _ => pq::open(&p, &cand)?,
    };
    m.set("cold_open_ms", t.secs() * 1e3);
    let t = Timer::start();
    let got = r.q1()?;
    let q1_s = t.secs();
    m.set("cold_q1_s", q1_s);
    for k in 0..NCAT {
        if !close(got[k], q1[k]) {
            return Err(format!("q1 mismatch cat {k}: {} vs {}", got[k], q1[k]).into());
        }
    }
    let t = Timer::start();
    let mut res = Vec::new();
    for &(a, b) in &ranges {
        res.push(r.q2(a, b)?);
    }
    let q2_s = t.secs();
    m.set("cold_q2_s", q2_s);
    for (g, e) in res.iter().zip(&q2) {
        if !(g.0 == e.0 && close(g.1, e.1)) {
            return Err(format!("q2 mismatch {g:?} vs {e:?}").into());
        }
    }
    m.set("query_s", q1_s + q2_s);
    m.set("reader_peak_rss_mb", peak_rss_mb());
    m.set("verified", true);
    Ok(m)
}
