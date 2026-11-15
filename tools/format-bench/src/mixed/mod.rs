//! Scenario `mixed`: mixed-type acquisition records at several rates,
//! followed live, sustained, then read back from the start.
//!
//! Three record types with exact, heterogeneous field types:
//!   A  t:u64, a:[i8;10], f:[bool;3], x:[u16;500]   100 kHz  (~1 KB/record)
//!   B  t:u64, x:[f32;32], status:u16               10 kHz
//!   C  t:u64, code:u32, msg:String (variable)      1 kHz
//! Producer threads (one per type) build one block per 10 ms window and feed a
//! single writer through bounded queues.
//!
//! The application states two requirements, not a sync schedule:
//!   freshness `FB_FRESH_MS`: a follower must see every window within this
//!     time of the window completing (10 ms: high refresh; 100 ms: logging);
//!   loss bound `FB_LOSS_MS`: a power loss may lose at most this much of the
//!     most recent data (0: everything a follower has seen is durable).
//! Each engine meets them with its own cheapest mechanisms: visibility every
//! freshness interval, durability every loss interval (or with every
//! visibility when the bound is 0).
//! Follower processes: one reads every type and checks that each observation
//! shows all types at the same publication boundary; one subscribes to B only.
//! `FB_PACE=1` emits windows in real time (latency run); otherwise producers
//! run flat out (throughput run). The sweep tunes on a flat-out run and
//! measures a 10-minute real-time run (~60 GB), after which a late reader
//! process reads everything from the start from a cold cache (`catchup_s`).
//! Records arrive in 10 ms bursts per type (the device buffer); how bursts
//! are grouped into stored blocks, published and indexed is each adapter's
//! choice (`candidates`). Runs under the harness memory cap.
use crate::common::*;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::Duration;

pub const NA: usize = 1000; // records per 10 ms window
pub const NB: usize = 100;
pub const NC: usize = 10;
const WIN_NS: u64 = 10_000_000;
const POLL: Duration = Duration::from_micros(100);

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().map_or(d, |v| v.parse().unwrap())
}
pub fn windows() -> u64 {
    env_u64("FB_WINDOWS", 100)
}
/// Freshness requirement in ms (multiple of the 10 ms window).
pub fn fresh_ms() -> u64 {
    let v = env_u64("FB_FRESH_MS", 10);
    assert!(
        v >= 10 && v % 10 == 0,
        "FB_FRESH_MS must be a multiple of 10"
    );
    v
}
/// Loss bound in ms; 0 makes every publication durable.
pub fn loss_ms() -> u64 {
    let v = env_u64("FB_LOSS_MS", 0);
    assert!(
        v == 0 || (v >= fresh_ms() && v % fresh_ms() == 0),
        "FB_LOSS_MS must be 0 or a multiple of FB_FRESH_MS"
    );
    v
}
/// Windows per publication.
pub fn pubw() -> u64 {
    fresh_ms() / 10
}
/// Publications per durable publication.
pub fn durable_every() -> u64 {
    if loss_ms() == 0 {
        1
    } else {
        loss_ms() / fresh_ms()
    }
}
/// `FB_VERIFY=0`: followers fold every decoded field into a checksum instead
/// of regenerating the expected records, so reader CPU reflects decoding.
fn verify() -> bool {
    std::env::var("FB_VERIFY").map_or(true, |v| v != "0")
}
/// Number of followers that read every record type (`FB_ALL_READERS`, default 1).
fn all_readers() -> usize {
    env_u64("FB_ALL_READERS", 1) as usize
}
fn paced() -> bool {
    std::env::var_os("FB_PACE").is_some()
}

fn h(i: u64, j: u64) -> u64 {
    i.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(j.wrapping_mul(0xBF58_476D_1CE4_E5B9))
        .rotate_left(23)
        ^ i
}

#[derive(Clone, Copy, PartialEq, Debug, hdf5::H5Type, varlit::VarlitBlock)]
#[varlit(id = 60, version = 1, kind = "fixed")]
#[repr(C)]
pub struct RecA {
    pub t: u64,
    pub a: [i8; 10],
    pub f: [bool; 3],
    pub x: [u16; 500],
}
#[derive(Clone, Copy, PartialEq, Debug, hdf5::H5Type, varlit::VarlitBlock)]
#[varlit(id = 61, version = 1, kind = "fixed")]
#[repr(C)]
pub struct RecB {
    pub t: u64,
    pub x: [f32; 32],
    pub status: u16,
}
#[derive(Clone, PartialEq, Debug)]
pub struct RecC {
    pub t: u64,
    pub code: u32,
    pub msg: String,
}
pub fn gen_a(i: u64) -> RecA {
    RecA {
        t: i * 10_000,
        a: std::array::from_fn(|j| h(i, j as u64) as i8),
        f: std::array::from_fn(|j| h(i, 16 + j as u64) & 1 == 1),
        x: std::array::from_fn(|j| h(i, 32 + j as u64) as u16),
    }
}
pub fn gen_b(i: u64) -> RecB {
    RecB {
        t: i * 100_000,
        x: std::array::from_fn(|j| (h(i, j as u64) % 1_000_000) as f32 / 7.0),
        status: h(i, 99) as u16,
    }
}
pub fn gen_c(i: u64) -> RecC {
    RecC {
        t: i * 1_000_000,
        code: h(i, 0) as u32,
        msg: format!("evt-{i}-{}", "x".repeat((h(i, 1) % 24) as usize)),
    }
}

/// One window's block of one type; `t0` = wall time the block was completed.
pub struct Blk<R> {
    pub w: u64,
    pub t0: u64,
    pub recs: Vec<R>,
}
pub type BA = Blk<RecA>;
pub type BB = Blk<RecB>;
pub type BC = Blk<RecC>;
pub fn bytes_per_window() -> u64 {
    (NA * (8 + 10 + 3 + 1000) + NB * (8 + 128 + 2) + NC * 24) as u64
}

pub trait AcqWriter {
    /// Blocks are handed over by value so a writer can move them into its
    /// own block type without copying.
    fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()>;
    /// Make everything appended visible to followers; `durable` also makes
    /// it survive power loss.
    fn publish(&mut self, durable: bool) -> Result<()>;
    /// Announced before the first append of each publication: whether that
    /// publication will be durable (SQLite fixes the sync level at BEGIN).
    fn will_publish(&mut self, _durable: bool) {}
}

/// Follower-side sink: verification, lag and cross-type consistency.
pub struct Sink {
    pub want: [bool; 3],
    pub seen: [u64; 3],
    pub lags: Vec<f64>,
    pub inconsistent: u64,
    pub observations: u64,
    pub sum: u64,
    pub verify: bool,
}
impl Sink {
    pub fn a(&mut self, w: u64, t0: u64, recs: impl ExactSizeIterator<Item = RecA>) {
        assert_eq!(w, self.seen[0], "A window order");
        assert_eq!(recs.len(), NA);
        for (k, r) in recs.enumerate() {
            if self.verify {
                assert!(
                    r == gen_a(w * NA as u64 + k as u64),
                    "A mismatch w={w} k={k}"
                );
            } else {
                let mut x = r.t;
                x =
                    r.a.iter()
                        .fold(x, |s, &v| s.wrapping_mul(31).wrapping_add(v as u64));
                x =
                    r.f.iter()
                        .fold(x, |s, &v| s.wrapping_mul(31).wrapping_add(v as u64));
                x = r.x.iter().fold(x, |s, &v| s.wrapping_add(v as u64));
                self.sum = self.sum.wrapping_add(x);
            }
        }
        self.done(0, t0);
    }
    pub fn b(&mut self, w: u64, t0: u64, recs: impl ExactSizeIterator<Item = RecB>) {
        assert_eq!(w, self.seen[1], "B window order");
        assert_eq!(recs.len(), NB);
        for (k, r) in recs.enumerate() {
            if self.verify {
                assert!(
                    r == gen_b(w * NB as u64 + k as u64),
                    "B mismatch w={w} k={k}"
                );
            } else {
                let x = r.x.iter().fold(r.t ^ r.status as u64, |s, &v| {
                    s.wrapping_add(v.to_bits() as u64)
                });
                self.sum = self.sum.wrapping_add(x);
            }
        }
        self.done(1, t0);
    }
    pub fn c(&mut self, w: u64, t0: u64, recs: impl ExactSizeIterator<Item = RecC>) {
        assert_eq!(w, self.seen[2], "C window order");
        assert_eq!(recs.len(), NC);
        for (k, r) in recs.enumerate() {
            if self.verify {
                assert!(
                    r == gen_c(w * NC as u64 + k as u64),
                    "C mismatch w={w} k={k}"
                );
            } else {
                self.sum = self
                    .sum
                    .wrapping_add(r.t ^ r.code as u64 ^ r.msg.len() as u64);
            }
        }
        self.done(2, t0);
    }
    fn done(&mut self, i: usize, t0: u64) {
        self.lags.push((mono_ns().saturating_sub(t0)) as f64 / 1e6);
        self.seen[i] += 1;
    }
    /// Called after a poll that returned data: every wanted type must stand
    /// at the same window boundary (a publication covers all types).
    pub fn observe(&mut self) {
        self.observations += 1;
        let v: Vec<u64> = (0..3)
            .filter(|&i| self.want[i])
            .map(|i| self.seen[i])
            .collect();
        if v.iter().any(|&x| x != v[0]) {
            self.inconsistent += 1;
        }
    }
    pub fn finished(&self) -> bool {
        (0..3).all(|i| !self.want[i] || self.seen[i] >= windows())
    }
}

pub trait AcqReader {
    /// Consume everything newly published; return true if anything arrived.
    fn poll(&mut self, s: &mut Sink) -> Result<bool>;
}

mod vl_native;
mod vl_native_raw;
mod vl_native_rec;
mod vl_stream;
use vl_stream as vl;

mod sq;

mod h5;

mod pq;

// ---------------------------------------------------------------- driver
fn target(fmt: &str, dir: &Path) -> PathBuf {
    match fmt {
        "parquet" => dir.join("pq"),
        f => dir.join(format!("mixed.{f}")),
    }
}

fn producer<R: Send + 'static>(
    n: usize,
    make: fn(u64) -> R,
    start: std::time::Instant,
) -> Receiver<Blk<R>> {
    let (tx, rx) = sync_channel(16);
    std::thread::spawn(move || {
        for w in 0..windows() {
            if paced() {
                // A window's block is complete at the end of that window.
                let due = start + Duration::from_nanos((w + 1) * WIN_NS);
                if let Some(d) = due.checked_duration_since(std::time::Instant::now()) {
                    std::thread::sleep(d);
                }
            }
            let recs = (0..n as u64).map(|k| make(w * n as u64 + k)).collect();
            if tx
                .send(Blk {
                    w,
                    t0: mono_ns(),
                    recs,
                })
                .is_err()
            {
                return;
            }
        }
    });
    rx
}

pub fn candidates(engine: &str) -> Result<&'static [&'static str]> {
    Ok(match engine {
        "varlit" => &["native", "native-rec", "native-raw", "stream"],
        "sqlite" | "hdf5" | "parquet" => &["default"],
        _ => return Err(format!("unknown engine {engine}").into()),
    })
}
pub fn adapter_files(engine: &str, cand: &str) -> &'static str {
    match (engine, cand) {
        ("varlit", "stream") => "src/mixed/vl_stream.rs",
        ("varlit", "native") => "src/mixed/vl_native.rs,src/mixed/vl_stream.rs",
        ("varlit", "native-rec") => {
            "src/mixed/vl_native_rec.rs,src/mixed/vl_native.rs,src/mixed/vl_stream.rs"
        }
        ("varlit", "native-raw") => {
            "src/mixed/vl_native_raw.rs,src/mixed/vl_native.rs,src/mixed/vl_stream.rs"
        }
        ("varlit", _) => "src/mixed/vl_stream.rs",
        ("sqlite", _) => "src/mixed/sq.rs",
        ("hdf5", _) => "src/mixed/h5.rs",
        ("parquet", _) => "src/mixed/pq.rs",
        _ => "",
    }
}
fn create(engine: &str, cand: &str, p: &Path) -> Result<Box<dyn AcqWriter>> {
    match (engine, cand) {
        ("varlit", "stream") => vl::create(p),
        ("varlit", "native") => vl_native::create(p),
        ("varlit", "native-rec") => vl_native_rec::create(p),
        ("varlit", "native-raw") => vl_native_raw::create(p),
        ("sqlite", _) => sq::create(p),
        ("hdf5", _) => h5::create(p),
        ("parquet", _) => pq::create(p),
        _ => Err(format!("unknown {engine}/{cand}").into()),
    }
}
fn open(engine: &str, cand: &str, p: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
    match (engine, cand) {
        ("varlit", "stream") => vl::open(p, want),
        ("varlit", "native") => vl_native::open(p, want),
        ("varlit", "native-rec") => vl_native_rec::open(p, want),
        ("varlit", "native-raw") => vl_native_raw::open(p, want),
        ("sqlite", _) => sq::open(p, want),
        ("hdf5", _) => h5::open(p, want),
        ("parquet", _) => pq::open(p, want),
        _ => Err(format!("unknown {engine}/{cand}").into()),
    }
}
fn the_cand(engine: &str) -> Result<String> {
    let c = cand();
    Ok(if c.is_empty() {
        candidates(engine)?[0].to_string()
    } else {
        c
    })
}

pub fn run(fmt: &str, dir: &Path) -> Result<Metrics> {
    let cand = the_cand(fmt)?;
    let p = target(fmt, dir);
    let mut w: Box<dyn AcqWriter> = create(fmt, &cand, &p)?;
    let exe = std::env::current_exe()?;
    let mut roles = vec!["all"; all_readers()];
    roles.push("b");
    let mut kids = roles
        .iter()
        .enumerate()
        .map(|(i, r)| {
            std::process::Command::new(&exe)
                .args([
                    "follow-mixed",
                    fmt,
                    dir.to_str().unwrap(),
                    r,
                    &i.to_string(),
                ])
                .env("FB_CAND", &cand)
                .spawn()
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !(0..roles.len()).all(|i| dir.join(format!("ready-{i}")).exists()) {
        assert!(
            std::time::Instant::now() < deadline,
            "followers did not start"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let start = std::time::Instant::now();
    let (ra, rb, rc) = (
        producer(NA, gen_a, start),
        producer(NB, gen_b, start),
        producer(NC, gen_c, start),
    );
    let mut pubs = Vec::new();
    let (mut t_wait, mut t_append) = (0.0, 0.0);
    let (mut npub, mut ndurable) = (0u64, 0u64);
    let durable_pub = |n: u64, k: u64| n % durable_every() == 0 || k + 1 == windows();
    // A writer error must not leave followers waiting forever.
    let mut write = || -> Result<()> {
        for k in 0..windows() {
            if k % pubw() == 0 {
                let last = (k + pubw()).min(windows()) - 1;
                w.will_publish(durable_pub(npub + 1, last));
            }
            let t = Timer::start();
            let (a, b, c) = (ra.recv()?, rb.recv()?, rc.recv()?);
            t_wait += t.secs();
            let t = Timer::start();
            w.append(a, b, c)?;
            t_append += t.secs();
            if (k + 1) % pubw() == 0 || k + 1 == windows() {
                npub += 1;
                // The last publication is always durable.
                let durable = durable_pub(npub, k);
                let t = Timer::start();
                w.publish(durable)?;
                pubs.push(t.secs() * 1e3);
                ndurable += u64::from(durable);
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
    let mut m = Metrics::default();
    m.set("candidate", cand.as_str());
    m.set("writer_peak_rss_mb", peak_rss_mb());
    let data_s = windows() as f64 * WIN_NS as f64 / 1e9;
    m.set("write_s", wall);
    m.set(
        "MBps",
        windows() as f64 * bytes_per_window() as f64 / wall / 1e6,
    );
    m.set("realtime_factor", data_s / wall);
    pubs.sort_by(f64::total_cmp);
    m.set("wait_s", t_wait);
    m.set("append_s", t_append);
    m.set("publish_s", pubs.iter().sum::<f64>() / 1e3);
    m.set("publish_p50_ms", percentile(&pubs, 0.5));
    m.set("publish_p99_ms", percentile(&pubs, 0.99));
    // Per role: worst reader's lag percentiles, summed inconsistencies,
    // mean reader CPU, and decode throughput per reader CPU second.
    for role in ["all", "b"] {
        let (mut p50, mut p99, mut inc, mut obs, mut cpu, mut n) =
            (0f64, 0f64, 0u64, 0u64, 0f64, 0f64);
        let (mut late, mut seen, mut rss) = (0usize, 0usize, 0u64);
        for (i, r) in roles.iter().enumerate() {
            if *r != role {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
                dir.join(format!("mixed-{i}.json")),
            )?)?;
            let mut l: Vec<f64> = v["lags"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_f64().unwrap())
                .collect();
            l.sort_by(f64::total_cmp);
            late += l.iter().filter(|&&x| x > fresh_ms() as f64).count();
            seen += l.len();
            p50 = p50.max(percentile(&l, 0.5));
            p99 = p99.max(percentile(&l, 0.99));
            inc += v["inconsistent"].as_u64().unwrap();
            obs += v["observations"].as_u64().unwrap();
            cpu += v["cpu_s"].as_f64().unwrap();
            rss = rss.max(v["peak_rss_mb"].as_u64().unwrap_or(0));
            n += 1.0;
            std::fs::remove_file(dir.join(format!("mixed-{i}.json")))?;
            std::fs::remove_file(dir.join(format!("ready-{i}")))?;
        }
        let bytes = if role == "b" {
            (NB * 138) as f64 * windows() as f64
        } else {
            (windows() * bytes_per_window()) as f64
        };
        m.set(&format!("{role}_lag_p50_ms"), p50);
        m.set(&format!("{role}_lag_p99_ms"), p99);
        m.set(
            &format!("{role}_late_frac"),
            late as f64 / seen.max(1) as f64,
        );
        m.set(&format!("{role}_inconsistent"), inc);
        m.set(&format!("{role}_observations"), obs);
        m.set(&format!("{role}_cpu_s"), cpu / n);
        m.set(&format!("{role}_MB_per_cpu_s"), bytes / (cpu / n) / 1e6);
        m.set(&format!("{role}_peak_rss_mb"), rss);
    }
    m.set("all_readers", all_readers() as u64);
    m.set("verify", verify());
    m.set("bytes", dir_bytes(dir));
    m.set("pubw", pubw());
    m.set("fresh_ms", fresh_ms());
    m.set("loss_ms", loss_ms());
    m.set("publications", npub);
    m.set("durable_publications", ndurable);
    m.set("paced", paced());

    // A reader that starts after the run and reads everything from the start,
    // from a cold cache, in its own process.
    evict(dir);
    let t = Timer::start();
    let st = std::process::Command::new(&exe)
        .args(["follow-mixed", fmt, dir.to_str().unwrap(), "catchup", "99"])
        .env("FB_CAND", &cand)
        .status()?;
    if !st.success() {
        return Err("catch-up reader failed".into());
    }
    let catchup = t.secs();
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("mixed-99.json"))?)?;
    std::fs::remove_file(dir.join("mixed-99.json"))?;
    std::fs::remove_file(dir.join("ready-99"))?;
    m.set("catchup_s", catchup);
    m.set(
        "catchup_MBps",
        (windows() * bytes_per_window()) as f64 / catchup / 1e6,
    );
    m.set(
        "catchup_peak_rss_mb",
        v["peak_rss_mb"].as_u64().unwrap_or(0),
    );
    Ok(m)
}

pub fn follower_main(fmt: &str, dir: &Path, role: &str, idx: usize) -> Result<()> {
    let p = target(fmt, dir);
    let want = if role == "b" {
        [false, true, false]
    } else {
        [true, true, true]
    };
    let mut r = open(fmt, &the_cand(fmt)?, &p, want)?;
    std::fs::write(dir.join(format!("ready-{idx}")), b"")?;
    let mut s = Sink {
        want,
        seen: [0; 3],
        lags: Vec::new(),
        inconsistent: 0,
        observations: 0,
        sum: 0,
        verify: verify(),
    };
    let cpu0 = process_cpu_s();
    while !s.finished() {
        if r.poll(&mut s)? {
            s.observe();
        } else {
            std::thread::sleep(POLL);
        }
    }
    let cpu_s = process_cpu_s() - cpu0;
    std::hint::black_box(s.sum);
    let out = serde_json::json!({ "lags": s.lags, "inconsistent": s.inconsistent, "observations": s.observations,
        "cpu_s": cpu_s, "peak_rss_mb": peak_rss_mb() });
    std::fs::write(dir.join(format!("mixed-{idx}.json")), out.to_string())?;
    Ok(())
}

pub fn codec_probe() -> Result<()> {
    use varlit::{Endian, encode_to_vec};
    let blk = Blk {
        w: 0,
        t0: 0,
        recs: (0..NA as u64).map(gen_a).collect(),
    };
    let typed = vl::to_a(&blk);
    let raw: Vec<u8> = typed.x.iter().flat_map(|v| v.to_le_bytes()).collect();
    let n = 200;
    let t = Timer::start();
    let mut sz = 0;
    for _ in 0..n {
        sz += encode_to_vec(&typed.x, Endian::Little)?.len();
    }
    let tv = t.secs();
    let t = Timer::start();
    for _ in 0..n {
        sz += encode_to_vec(&raw, Endian::Little)?.len();
    }
    let tr = t.secs();
    let encoded = encode_to_vec(&typed.x, Endian::Little)?;
    let t = Timer::start();
    for _ in 0..n {
        let v: Vec<u16> = varlit::decode_from_slice(&encoded, Endian::Little)?;
        sz += v.len();
    }
    let td = t.secs();
    let mb = (raw.len() * n) as f64 / 1e6;
    println!("decode Vec<u16>: {:.0} MB/s", mb / td);
    // Whole A block (t, a, f, x): encode, then CRC32 over the encoded bytes.
    let t = Timer::start();
    let mut whole = Vec::new();
    for _ in 0..n {
        whole = encode_to_vec(&typed, Endian::Little)?;
    }
    let te = t.secs();
    let t = Timer::start();
    let mut c = 0u32;
    for _ in 0..n {
        c ^= crc32fast::hash(&whole);
    }
    let tc = t.secs();
    let wmb = (whole.len() * n) as f64 / 1e6;
    println!(
        "ABlock encode: {:.0} MB/s; crc32: {:.0} MB/s [{c}]",
        wmb / te,
        wmb / tc
    );
    println!(
        "Vec<u16> 500k elems: {:.0} MB/s ({:.2} ns/elem); same bytes as Vec<u8>: {:.0} MB/s  [{}]",
        mb / tv,
        tv * 1e9 / (n * typed.x.len()) as f64,
        mb / tr,
        sz
    );
    Ok(())
}
