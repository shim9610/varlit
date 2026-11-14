//! Scenario `oltp`: bank accounts with durable read-modify-write transfers.
//!
//! Situation (the only thing this module decides):
//! - 200,000,000 accounts (`FB_SMALL=1`: 1,000,000) arrive in id order:
//!   `id u64, branch u32, balance i64, name [u8; 24], created u64`; branch is
//!   a random attribute (one branch per ~100 accounts).
//! - Then 1000 transactions, each 10 transfers between accounts chosen by the
//!   access distribution (`FB_DIST`: `uniform` or `zipf`, s ~= 1 over
//!   scrambled ids). A transaction reads the current balances and writes the
//!   new ones; it must be atomic and durable when it returns.
//! - Then, from a cold cache: 20,000 point lookups (same distribution) and 200
//!   branch aggregates (sum and count of balances of one branch).
//! - Objective: transactions + cold lookups + cold aggregates time.
//!   Loading time and size are reported. Runs under the harness memory cap.
//!
//! Layout, secondary index, grouping, caching and maintenance are each
//! adapter's choice (`candidates`).
use crate::common::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub mod csr;
mod h5;
pub mod pack;
mod pq;
mod sq;
mod vl_append;
mod vl_grouped;
mod vl_indexed;
mod vl_packed;

pub const TXNS: usize = 1000;
pub const TRANSFERS: usize = 10;
pub const POINTS: usize = 20_000;
pub const BRANCH_Q: usize = 200;
pub const START: i64 = 1000;

pub fn accounts() -> u64 {
    if std::env::var_os("FB_SMALL").is_some() {
        1_000_000
    } else {
        200_000_000
    }
}
pub fn branches() -> u32 {
    (accounts() / 100) as u32
}
pub fn branch_of(id: u64) -> u32 {
    (splitmix(id ^ 0xB0B) % branches() as u64) as u32
}
pub fn name_of(id: u64) -> [u8; 24] {
    let mut n = [0u8; 24];
    fill_payload(id ^ 0xAA, &mut n);
    n
}
pub fn created_of(id: u64) -> u64 {
    id.wrapping_mul(7) + 1_700_000_000
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    pub id: u64,
    pub branch: u32,
    pub balance: i64,
    pub name: [u8; 24],
    pub created: u64,
}
pub fn initial_row(id: u64) -> Row {
    Row {
        id,
        branch: branch_of(id),
        balance: START,
        name: name_of(id),
        created: created_of(id),
    }
}

pub trait Oltp {
    /// Load every account (`initial_row(id)` for id in 0..accounts()) and
    /// build whatever the adapter needs; durable when it returns.
    fn load(&mut self) -> Result<()>;
    /// Apply `(id, delta)` (distinct ids) as one atomic, durable transaction
    /// that reads the current balances from storage.
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()>;
    /// Optional maintenance after the workload (timed separately).
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}
pub trait OltpRead {
    fn point(&mut self, id: u64) -> Result<Row>;
    /// (sum of balances, number of accounts) of a branch.
    fn branch(&mut self, b: u32) -> Result<(i64, u64)>;
}

pub fn candidates(engine: &str) -> Result<&'static [&'static str]> {
    Ok(match engine {
        "varlit" => &["indexed", "grouped", "append", "packed-matrix"],
        "sqlite" => sq::CANDIDATES,
        "hdf5" => h5::CANDIDATES,
        "parquet" => pq::CANDIDATES,
        _ => return Err(format!("unknown engine {engine}").into()),
    })
}
/// Effort accounting: every source file the (engine, candidate) adapter uses.
pub fn adapter_files(engine: &str, cand: &str) -> &'static str {
    match (engine, cand) {
        ("varlit", "indexed") => "src/oltp/vl_indexed.rs,src/oltp/csr.rs",
        ("varlit", "grouped") => "src/oltp/vl_grouped.rs,src/oltp/pack.rs,src/oltp/csr.rs",
        ("varlit", "append") => {
            "src/oltp/vl_append.rs,src/oltp/vl_grouped.rs,src/oltp/pack.rs,src/oltp/csr.rs"
        }
        ("varlit", _) => "src/oltp/vl_packed.rs,src/oltp/pack.rs,src/oltp/csr.rs",
        ("sqlite", _) => "src/oltp/sq.rs",
        ("hdf5", _) => "src/oltp/h5.rs,src/oltp/csr.rs",
        ("parquet", _) => "src/oltp/pq.rs,src/oltp/csr.rs",
        _ => "",
    }
}
fn create(engine: &str, cand: &str, p: &Path) -> Result<Box<dyn Oltp>> {
    match (engine, cand) {
        ("varlit", "indexed") => vl_indexed::create(p),
        ("varlit", "grouped") => vl_grouped::create(p),
        ("varlit", "append") => vl_append::create(p),
        ("varlit", "packed-matrix") => vl_packed::create(p),
        ("sqlite", c) => sq::create(p, c),
        ("hdf5", c) => h5::create(p, c),
        ("parquet", c) => pq::create(p, c),
        _ => Err(format!("unknown {engine}/{cand}").into()),
    }
}
fn open(engine: &str, cand: &str, p: &Path) -> Result<Box<dyn OltpRead>> {
    match (engine, cand) {
        ("varlit", "indexed") => vl_indexed::open(p),
        ("varlit", "grouped") => vl_grouped::open(p),
        ("varlit", "append") => vl_append::open(p),
        ("varlit", "packed-matrix") => vl_packed::open(p),
        ("sqlite", c) => sq::open(p, c),
        ("hdf5", c) => h5::open(p, c),
        ("parquet", c) => pq::open(p, c),
        _ => Err(format!("unknown {engine}/{cand}").into()),
    }
}

/// Access distribution over account ids.
struct Dist {
    zipf: bool,
    rng: u64,
    n: u64,
}
impl Dist {
    fn new(seed: u64) -> Self {
        let zipf = match std::env::var("FB_DIST").as_deref() {
            Ok("zipf") => true,
            Ok("uniform") | Err(_) => false,
            Ok(o) => panic!("FB_DIST={o}"),
        };
        Self {
            zipf,
            rng: seed,
            n: accounts(),
        }
    }
    fn raw(&mut self) -> u64 {
        self.rng = splitmix(self.rng);
        self.rng
    }
    fn id(&mut self) -> u64 {
        if !self.zipf {
            return self.raw() % self.n;
        }
        // Zipf s ~= 1: rank = n^u (inverse of the continuous CDF ln k / ln n),
        // scattered over the id space by a bijection (odd prime stride mod n).
        let u = (self.raw() >> 11) as f64 / (1u64 << 53) as f64;
        let rank = ((self.n as f64).powf(u) as u64)
            .saturating_sub(1)
            .min(self.n - 1);
        ((rank as u128 * 1_000_000_007u128) % self.n as u128) as u64
    }
}

pub fn run(engine: &str, dir: &Path) -> Result<Metrics> {
    let cand = cand();
    let cand = if cand.is_empty() {
        candidates(engine)?[0].to_string()
    } else {
        cand
    };
    let p: PathBuf = dir.join("bank");
    let n = accounts();

    // Plan + sparse oracle (untimed).
    let mut d = Dist::new(0xBA4C);
    let mut amt = 0x5EEDu64;
    let mut delta: HashMap<u64, i64> = HashMap::new();
    let mut plan: Vec<Vec<(u64, i64)>> = Vec::with_capacity(TXNS);
    for _ in 0..TXNS {
        let mut net: std::collections::BTreeMap<u64, i64> = Default::default();
        for _ in 0..TRANSFERS {
            let (a, b) = (d.id(), d.id());
            amt = splitmix(amt);
            let x = (amt % 500) as i64 + 1;
            *net.entry(a).or_default() -= x;
            *net.entry(b).or_default() += x;
        }
        net.retain(|_, v| *v != 0);
        for (&id, &v) in &net {
            *delta.entry(id).or_default() += v;
        }
        plan.push(net.into_iter().collect());
    }
    let pts: Vec<u64> = (0..POINTS).map(|_| d.id()).collect();
    let brs: Vec<u32> = (0..BRANCH_Q)
        .map(|_| (d.raw() % branches() as u64) as u32)
        .collect();

    let mut m = Metrics::default();
    m.set("candidate", cand.as_str());
    m.set("accounts", n);
    let mut w = create(engine, &cand, &p)?;
    let t = Timer::start();
    w.load()?;
    m.set("load_s", t.secs());
    let mut lat = Vec::with_capacity(TXNS);
    let t = Timer::start();
    for d in &plan {
        let a = Timer::start();
        w.txn(d)?;
        lat.push(a.secs() * 1e3);
    }
    let txn_s = t.secs();
    m.set("txn_s", txn_s);
    lat.sort_by(f64::total_cmp);
    m.set("txn_p50_ms", percentile(&lat, 0.5));
    m.set("txn_p99_ms", percentile(&lat, 0.99));
    let t = Timer::start();
    w.finish()?;
    m.set("maint_s", t.secs());
    drop(w);
    m.set("bytes", dir_bytes(dir));
    m.set("writer_peak_rss_mb", peak_rss_mb());

    evict(dir);
    let t = Timer::start();
    let mut r = open(engine, &cand, &p)?;
    m.set("cold_open_ms", t.secs() * 1e3);
    let t = Timer::start();
    let mut rows = Vec::with_capacity(POINTS);
    for &id in &pts {
        rows.push(r.point(id)?);
    }
    let pts_s = t.secs();
    m.set("cold_points_s", pts_s);
    let t = Timer::start();
    let mut got = Vec::with_capacity(BRANCH_Q);
    for &b in &brs {
        got.push(r.branch(b)?);
    }
    let br_s = t.secs();
    m.set("cold_branch_s", br_s);
    m.set("total_s", txn_s + pts_s + br_s);
    drop(r);

    // Verify (untimed).
    for row in rows {
        let mut want = initial_row(row.id);
        want.balance += delta.get(&row.id).copied().unwrap_or(0);
        if row != want {
            return Err(format!("point mismatch: {row:?} != {want:?}").into());
        }
    }
    let q: HashMap<u32, usize> = brs.iter().enumerate().map(|(i, &b)| (b, i)).collect();
    let mut want = vec![(0i64, 0u64); BRANCH_Q];
    for id in 0..n {
        if let Some(&i) = q.get(&branch_of(id)) {
            want[i].0 += START;
            want[i].1 += 1;
        }
    }
    for (&id, &v) in &delta {
        if let Some(&i) = q.get(&branch_of(id)) {
            want[i].0 += v;
        }
    }
    for (i, (&b, g)) in brs.iter().zip(got).enumerate() {
        let w = want[q[&b]];
        let _ = i;
        if g != w {
            return Err(format!("branch {b} mismatch: {g:?} != {w:?}").into());
        }
    }
    m.set("verified", true);
    Ok(m)
}
