//! Scenario 5: mixed-type acquisition records at several rates.
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
//! run flat out (throughput run).
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
    assert!(v >= 10 && v % 10 == 0, "FB_FRESH_MS must be a multiple of 10");
    v
}
/// Loss bound in ms; 0 makes every publication durable.
pub fn loss_ms() -> u64 {
    let v = env_u64("FB_LOSS_MS", 0);
    assert!(v == 0 || (v >= fresh_ms() && v % fresh_ms() == 0), "FB_LOSS_MS must be 0 or a multiple of FB_FRESH_MS");
    v
}
/// Windows per publication.
pub fn pubw() -> u64 {
    fresh_ms() / 10
}
/// Publications per durable publication.
pub fn durable_every() -> u64 {
    if loss_ms() == 0 { 1 } else { loss_ms() / fresh_ms() }
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
                assert!(r == gen_a(w * NA as u64 + k as u64), "A mismatch w={w} k={k}");
            } else {
                let mut x = r.t;
                x = r.a.iter().fold(x, |s, &v| s.wrapping_mul(31).wrapping_add(v as u64));
                x = r.f.iter().fold(x, |s, &v| s.wrapping_mul(31).wrapping_add(v as u64));
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
                assert!(r == gen_b(w * NB as u64 + k as u64), "B mismatch w={w} k={k}");
            } else {
                let x = r.x.iter().fold(r.t ^ r.status as u64, |s, &v| s.wrapping_add(v.to_bits() as u64));
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
                assert!(r == gen_c(w * NC as u64 + k as u64), "C mismatch w={w} k={k}");
            } else {
                self.sum = self.sum.wrapping_add(r.t ^ r.code as u64 ^ r.msg.len() as u64);
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
        let v: Vec<u64> = (0..3).filter(|&i| self.want[i]).map(|i| self.seen[i]).collect();
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

// ---------------------------------------------------------------- Varlit (stream API)
mod vl {
    use super::*;
    use varlit::{BatchOptions, StreamOptions, StreamingBlocks, VarlitEmpty, varlit_format};

    // One block type per record type; fields keep their exact element types
    // (struct-of-arrays inside the block, N records per block).
    varlit_format! {
        pub format Mixed {
            magic: b"MIXEDACQ";
            version: 1;
            limits {
                record_payload: 67_108_864;
                logical_payload: 268_435_456;
                materialized_bytes: 1_073_741_824;
            }
            endian: little;
            schema_hash: computed;
            integrity: crc32_with_header;
            blocks {
                variable ABlock(id = 1) {
                    w: u64, t0: u64,
                    t: Vec<u64>, a: Vec<i8>, f: Vec<bool>, x: Vec<u16>,
                }
                variable BBlock(id = 2) {
                    w: u64, t0: u64,
                    t: Vec<u64>, x: Vec<f32>, status: Vec<u16>,
                }
                variable CBlock(id = 3) {
                    w: u64, t0: u64,
                    t: Vec<u64>, code: Vec<u32>, msg: Vec<String>,
                }
            }
        }
    }
    pub fn to_a(b: &BA) -> ABlock {
        ABlock {
            w: b.w,
            t0: b.t0,
            t: b.recs.iter().map(|r| r.t).collect(),
            a: b.recs.iter().flat_map(|r| r.a).collect(),
            f: b.recs.iter().flat_map(|r| r.f).collect(),
            x: b.recs.iter().flat_map(|r| r.x).collect(),
        }
    }
    pub fn to_b(b: &BB) -> BBlock {
        BBlock {
            w: b.w,
            t0: b.t0,
            t: b.recs.iter().map(|r| r.t).collect(),
            x: b.recs.iter().flat_map(|r| r.x).collect(),
            status: b.recs.iter().map(|r| r.status).collect(),
        }
    }
    pub fn to_c(b: &BC) -> CBlock {
        CBlock {
            w: b.w,
            t0: b.t0,
            t: b.recs.iter().map(|r| r.t).collect(),
            code: b.recs.iter().map(|r| r.code).collect(),
            msg: b.recs.iter().map(|r| r.msg.clone()).collect(),
        }
    }
    pub fn sink_a(s: &mut Sink, v: &ABlock) {
        s.a(v.w, v.t0, (0..v.t.len()).map(|k| RecA {
            t: v.t[k],
            a: v.a[k * 10..k * 10 + 10].try_into().unwrap(),
            f: v.f[k * 3..k * 3 + 3].try_into().unwrap(),
            x: v.x[k * 500..k * 500 + 500].try_into().unwrap(),
        }));
    }
    pub fn sink_b(s: &mut Sink, v: &BBlock) {
        sink_b_parts(s, v.w, v.t0, &v.t, &v.x, &v.status);
    }
    pub fn sink_b_parts(s: &mut Sink, w: u64, t0: u64, t: &[u64], x: &[f32], status: &[u16]) {
        s.b(w, t0, (0..t.len()).map(|k| RecB {
            t: t[k],
            x: x[k * 32..k * 32 + 32].try_into().unwrap(),
            status: status[k],
        }));
    }
    pub fn sink_c(s: &mut Sink, v: &CBlock) {
        sink_c_parts(s, v.w, v.t0, &v.t, &v.code, &v.msg);
    }
    pub fn sink_c_parts(s: &mut Sink, w: u64, t0: u64, t: &[u64], code: &[u32], msg: &[String]) {
        s.c(w, t0, t.iter().zip(code).zip(msg).map(|((&t, &code), msg)| RecC { t, code, msg: msg.clone() }));
    }

    pub struct W(MixedStreamWriter);
    pub fn create(p: &Path) -> Result<Box<dyn AcqWriter>> {
        let mut w = Mixed::create_stream_writer(p, StreamOptions::default())?;
        w.sync()?;
        Ok(Box::new(W(w)))
    }
    impl AcqWriter for W {
        fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
            let (a, b, c) = (&a, &b, &c);
            self.0.push_a_block(&to_a(a))?;
            self.0.push_b_block(&to_b(b))?;
            self.0.push_c_block(&to_c(c))?;
            let _ = BatchOptions::default();
            Ok(())
        }
        fn publish(&mut self, _durable: bool) -> Result<()> {
            // Stream followers advance on the sidecar generation `sync`
            // publishes; `flush` alone is not visible to them, so meeting the
            // freshness bound needs a sync every publication.
            self.0.sync()?;
            Ok(())
        }
    }

    pub struct R {
        r: MixedStreamReader,
        a: Option<StreamingBlocks<ABlock>>,
        b: Option<StreamingBlocks<BBlock>>,
        c: Option<StreamingBlocks<CBlock>>,
        // Caller-owned decode targets, reused for every record.
        oa: ABlock,
        ob: BBlock,
        oc: CBlock,
    }
    pub fn open(p: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
        let r = Mixed::open_stream_reader(p, vl_stream(StreamOptions::default()))?;
        let a = if want[0] { Some(r.a_blocks()?) } else { None };
        let b = if want[1] { Some(r.b_blocks()?) } else { None };
        let c = if want[2] { Some(r.c_blocks()?) } else { None };
        Ok(Box::new(R { r, a, b, c, oa: ABlock::empty(), ob: BBlock::empty(), oc: CBlock::empty() }))
    }
    impl AcqReader for R {
        fn poll(&mut self, s: &mut Sink) -> Result<bool> {
            // follow_blocks follows the reader each call: repeat the pass until
            // no later cursor advanced, so every cursor shares one generation.
            loop {
                let mut later = 0;
                let mut first = true;
                if let Some(c) = &mut self.a {
                    self.r.follow_blocks(c)?;
                    first = false;
                }
                if let Some(c) = &mut self.b {
                    let n = self.r.follow_blocks(c)?;
                    if !first {
                        later += n;
                    }
                    first = false;
                }
                if let Some(c) = &mut self.c {
                    let n = self.r.follow_blocks(c)?;
                    if !first {
                        later += n;
                    }
                }
                if later == 0 {
                    break;
                }
            }
            let mut got = false;
            if let Some(c) = &mut self.a {
                while c.next_into(&mut self.oa)? {
                    sink_a(s, &self.oa);
                    got = true;
                }
            }
            if let Some(c) = &mut self.b {
                while c.next_into(&mut self.ob)? {
                    sink_b(s, &self.ob);
                    got = true;
                }
            }
            if let Some(c) = &mut self.c {
                while c.next_into(&mut self.oc)? {
                    sink_c(s, &self.oc);
                    got = true;
                }
            }
            Ok(got)
        }
    }
}

// ---------------------------------------------------------------- Varlit native
mod vn {
    use super::*;
    use varlit::{RecordIndexEntry, VarlitBlock, VarlitFile, varlit_format};
    use varlit::VarlitEmpty;
    use vl::{ABlock as VA, BBlock as VB, CBlock as VC};
    /// Caller-owned decode targets, reused for every record.
    type Outs = (VA, VB, VC);

    // Same block declarations, native append log with explicit commits.
    varlit_format! {
        pub struct MixedNative {
            magic: b"MIXEDNAT";
            version: 1;
            limits {
                record_payload: 67_108_864;
                logical_payload: 268_435_456;
                materialized_bytes: 1_073_741_824;
            }
            endian: little;
            integrity: crc32_with_header;
            index: block_offset_chain;
            commit: transaction_marker(explicit);
            blocks: [VA, VB, VC];
        }
    }
    // Identical, but `commit_durable` uses one storage barrier.
    varlit_format! {
        pub struct MixedNativeSb {
            magic: b"MIXEDNSB";
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
            blocks: [VA, VB, VC];
        }
    }
    /// The single-barrier commit is this layout's best durable choice (it
    /// already carries `crc32_with_header`); `FB_TWO_BARRIER=1` selects the
    /// default two-barrier commit for comparison.
    pub fn native_spec() -> varlit::FormatSpec {
        if std::env::var_os("FB_TWO_BARRIER").is_some() {
            MixedNative::spec()
        } else {
            MixedNativeSb::spec()
        }
    }

    pub struct W(VarlitFile);
    pub fn create(p: &Path) -> Result<Box<dyn AcqWriter>> {
        let mut f = VarlitFile::create(native_spec(), p)?;
        f.commit_durable()?;
        Ok(Box::new(W(f)))
    }
    impl AcqWriter for W {
        fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
            let (a, b, c) = (&a, &b, &c);
            let t = mono_ns();
            let (xa, xb, xc) = (vl::to_a(a), vl::to_b(b), vl::to_c(c));
            let t2 = mono_ns();
            self.0.push_info(&xa)?;
            self.0.push_info(&xb)?;
            self.0.push_info(&xc)?;
            let t3 = mono_ns();
            if std::env::var_os("FB_PROFILE").is_some() {
                eprintln!("PROF convert_ns={} push_ns={}", t2 - t, t3 - t2);
            }
            Ok(())
        }
        fn publish(&mut self, durable: bool) -> Result<()> {
            if durable { self.0.commit_durable()?; } else { self.0.commit()?; }
            Ok(())
        }
    }
    pub struct R {
        f: VarlitFile,
        want: [bool; 3],
        seen: [Option<u64>; 3],
        scratch: Vec<u8>,
        outs: Outs,
    }
    pub fn open(p: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
        Ok(Box::new(R {
            f: VarlitFile::open_readonly(vl_spec(native_spec()), p)?,
            want,
            seen: [None; 3],
            scratch: Vec::new(),
            outs: Outs::empty(),
        }))
    }
    impl R {
        fn fresh(&mut self, i: usize, id: u32) -> Result<Vec<RecordIndexEntry>> {
            let tail = self.f.block_tail_offset(id);
            let mut v = Vec::new();
            if tail != self.seen[i] {
                for e in self.f.block_chain(id)? {
                    let e = e?;
                    if Some(e.record_offset) <= self.seen[i] {
                        break;
                    }
                    v.push(e);
                }
                v.reverse();
                self.seen[i] = tail;
            }
            Ok(v)
        }
    }
    impl AcqReader for R {
        fn poll(&mut self, s: &mut Sink) -> Result<bool> {
            // One follow() fixes one committed snapshot for every block type.
            self.f.follow()?;
            let mut got = false;
            if self.want[0] {
                for e in self.fresh(0, VA::ID)? {
                    self.f.decode_block_into(&e, &mut self.scratch, &mut self.outs.0)?;
                    vl::sink_a(s, &self.outs.0);
                    got = true;
                }
            }
            if self.want[1] {
                for e in self.fresh(1, VB::ID)? {
                    self.f.decode_block_into(&e, &mut self.scratch, &mut self.outs.1)?;
                    vl::sink_b(s, &self.outs.1);
                    got = true;
                }
            }
            if self.want[2] {
                for e in self.fresh(2, VC::ID)? {
                    self.f.decode_block_into(&e, &mut self.scratch, &mut self.outs.2)?;
                    vl::sink_c(s, &self.outs.2);
                    got = true;
                }
            }
            Ok(got)
        }
    }
}

// ---------------------------------------------------------------- Varlit native, raw u16 bytes
// Diagnostic only: identical to `vn` except the 500 x u16 field is stored as
// little-endian bytes (Vec<u8>), to estimate what a bulk encode path would give.
mod vr {
    use super::*;
    use varlit::{RecordIndexEntry, VarlitBlock, VarlitFile, varlit_format};
    use varlit::VarlitEmpty;
    use vl::{BBlock as VB, CBlock as VC};
    type Outs = (ARaw, BRaw, CRaw);

    varlit_format! {
        pub format MixedRaw {
            magic: b"MIXEDRAW";
            version: 1;
            limits {
                record_payload: 67_108_864;
                logical_payload: 268_435_456;
                materialized_bytes: 1_073_741_824;
            }
            endian: little;
            integrity: crc32_with_header;
            index: block_offset_chain;
            commit: transaction_marker(explicit);
            blocks {
                variable ARaw(id = 1) {
                    w: u64, t0: u64,
                    t: Vec<u64>, a: Vec<i8>, f: Vec<bool>, x: Vec<u8>,
                }
                variable BRaw(id = 2) { w: u64, t0: u64, t: Vec<u64>, x: Vec<f32>, status: Vec<u16>, }
                variable CRaw(id = 3) { w: u64, t0: u64, t: Vec<u64>, code: Vec<u32>, msg: Vec<String>, }
            }
        }
    }
    fn to_raw(b: &BA) -> ARaw {
        let mut x = Vec::with_capacity(b.recs.len() * 1000);
        for r in &b.recs {
            for v in r.x {
                x.extend_from_slice(&v.to_le_bytes());
            }
        }
        ARaw {
            w: b.w,
            t0: b.t0,
            t: b.recs.iter().map(|r| r.t).collect(),
            a: b.recs.iter().flat_map(|r| r.a).collect(),
            f: b.recs.iter().flat_map(|r| r.f).collect(),
            x,
        }
    }
    pub struct W(VarlitFile);
    pub fn create(p: &Path) -> Result<Box<dyn AcqWriter>> {
        let mut f = VarlitFile::create(MixedRaw::spec(), p)?;
        f.commit_durable()?;
        Ok(Box::new(W(f)))
    }
    impl AcqWriter for W {
        fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
            let (a, b, c) = (&a, &b, &c);
            self.0.push_info(&to_raw(a))?;
            let (b, c) = (vl::to_b(b), vl::to_c(c));
            self.0.push_info(&BRaw { w: b.w, t0: b.t0, t: b.t, x: b.x, status: b.status })?;
            self.0.push_info(&CRaw { w: c.w, t0: c.t0, t: c.t, code: c.code, msg: c.msg })?;
            Ok(())
        }
        fn publish(&mut self, durable: bool) -> Result<()> {
            if durable { self.0.commit_durable()?; } else { self.0.commit()?; }
            Ok(())
        }
    }
    pub struct R {
        f: VarlitFile,
        want: [bool; 3],
        seen: [Option<u64>; 3],
        scratch: Vec<u8>,
        outs: Outs,
    }
    pub fn open(p: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
        Ok(Box::new(R { f: VarlitFile::open_readonly(vl_spec(MixedRaw::spec()), p)?, want, seen: [None; 3], scratch: Vec::new(), outs: Outs::empty() }))
    }
    impl R {
        fn fresh(&mut self, i: usize, id: u32) -> Result<Vec<RecordIndexEntry>> {
            let tail = self.f.block_tail_offset(id);
            let mut v = Vec::new();
            if tail != self.seen[i] {
                for e in self.f.block_chain(id)? {
                    let e = e?;
                    if Some(e.record_offset) <= self.seen[i] {
                        break;
                    }
                    v.push(e);
                }
                v.reverse();
                self.seen[i] = tail;
            }
            Ok(v)
        }
    }
    impl AcqReader for R {
        fn poll(&mut self, s: &mut Sink) -> Result<bool> {
            self.f.follow()?;
            let mut got = false;
            if self.want[0] {
                for e in self.fresh(0, ARaw::ID)? {
                    self.f.decode_block_into(&e, &mut self.scratch, &mut self.outs.0)?;
                    let v = &self.outs.0;
                    s.a(v.w, v.t0, (0..v.t.len()).map(|k| RecA {
                        t: v.t[k],
                        a: v.a[k * 10..k * 10 + 10].try_into().unwrap(),
                        f: v.f[k * 3..k * 3 + 3].try_into().unwrap(),
                        x: std::array::from_fn(|j| u16::from_le_bytes(v.x[k * 1000 + 2 * j..k * 1000 + 2 * j + 2].try_into().unwrap())),
                    }));
                    got = true;
                }
            }
            if self.want[1] {
                for e in self.fresh(1, VB::ID)? {
                    self.f.decode_block_into(&e, &mut self.scratch, &mut self.outs.1)?;
                    let v = &self.outs.1;
                    vl::sink_b_parts(s, v.w, v.t0, &v.t, &v.x, &v.status);
                    got = true;
                }
            }
            if self.want[2] {
                for e in self.fresh(2, VC::ID)? {
                    self.f.decode_block_into(&e, &mut self.scratch, &mut self.outs.2)?;
                    let v = &self.outs.2;
                    vl::sink_c_parts(s, v.w, v.t0, &v.t, &v.code, &v.msg);
                    got = true;
                }
            }
            Ok(got)
        }
    }
}

// ---------------------------------------------------------------- Varlit native, record sequences
// A and B records go into their window blocks as `Vec<RecA>` / `Vec<RecB>`:
// the producer's records are moved in, no per-field split, no copy.
mod vq {
    use super::*;
    use varlit::{RecordIndexEntry, VarlitBlock, VarlitFile, varlit_format};
    use varlit::VarlitEmpty;
    use vl::CBlock as VC;
    type Outs = (AWin, BWin, VC);

    #[derive(Clone, Debug, PartialEq, varlit::VarlitBlock)]
    #[varlit(id = 1, version = 1, kind = "variable")]
    pub struct AWin {
        #[varlit(field_id = 1)]
        pub w: u64,
        #[varlit(field_id = 2)]
        pub t0: u64,
        #[varlit(field_id = 3)]
        pub recs: Vec<RecA>,
    }
    #[derive(Clone, Debug, PartialEq, varlit::VarlitBlock)]
    #[varlit(id = 2, version = 1, kind = "variable")]
    pub struct BWin {
        #[varlit(field_id = 1)]
        pub w: u64,
        #[varlit(field_id = 2)]
        pub t0: u64,
        #[varlit(field_id = 3)]
        pub recs: Vec<RecB>,
    }
    varlit_format! {
        pub struct MixedRec {
            magic: b"MIXEDREC";
            version: 1;
            limits {
                record_payload: 67_108_864;
                logical_payload: 268_435_456;
                materialized_bytes: 1_073_741_824;
            }
            endian: little;
            integrity: crc32_with_header;
            index: block_offset_chain;
            commit: transaction_marker(explicit);
            blocks: [AWin, BWin, VC];
        }
    }

    pub struct W(VarlitFile);
    pub fn create(p: &Path) -> Result<Box<dyn AcqWriter>> {
        let mut f = VarlitFile::create(MixedRec::spec(), p)?;
        f.commit_durable()?;
        Ok(Box::new(W(f)))
    }
    impl AcqWriter for W {
        fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
            let t = mono_ns();
            let c = vl::to_c(&c);
            let (a, b) = (AWin { w: a.w, t0: a.t0, recs: a.recs }, BWin { w: b.w, t0: b.t0, recs: b.recs });
            let t2 = mono_ns();
            self.0.push_info(&a)?;
            self.0.push_info(&b)?;
            self.0.push_info(&c)?;
            if std::env::var_os("FB_PROFILE").is_some() {
                eprintln!("PROF convert_ns={} push_ns={}", t2 - t, mono_ns() - t2);
            }
            Ok(())
        }
        fn publish(&mut self, durable: bool) -> Result<()> {
            if durable { self.0.commit_durable()?; } else { self.0.commit()?; }
            Ok(())
        }
    }
    pub struct R {
        f: VarlitFile,
        want: [bool; 3],
        seen: [Option<u64>; 3],
        scratch: Vec<u8>,
        outs: Outs,
    }
    pub fn open(p: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
        Ok(Box::new(R { f: VarlitFile::open_readonly(vl_spec(MixedRec::spec()), p)?, want, seen: [None; 3], scratch: Vec::new(), outs: Outs::empty() }))
    }
    impl R {
        fn fresh(&mut self, i: usize, id: u32) -> Result<Vec<RecordIndexEntry>> {
            let tail = self.f.block_tail_offset(id);
            let mut v = Vec::new();
            if tail != self.seen[i] {
                for e in self.f.block_chain(id)? {
                    let e = e?;
                    if Some(e.record_offset) <= self.seen[i] {
                        break;
                    }
                    v.push(e);
                }
                v.reverse();
                self.seen[i] = tail;
            }
            Ok(v)
        }
    }
    impl AcqReader for R {
        fn poll(&mut self, s: &mut Sink) -> Result<bool> {
            self.f.follow()?;
            let mut got = false;
            if self.want[0] {
                for e in self.fresh(0, AWin::ID)? {
                    self.f.decode_block_into(&e, &mut self.scratch, &mut self.outs.0)?;
                    let v = &self.outs.0;
                    s.a(v.w, v.t0, v.recs.iter().copied());
                    got = true;
                }
            }
            if self.want[1] {
                for e in self.fresh(1, BWin::ID)? {
                    self.f.decode_block_into(&e, &mut self.scratch, &mut self.outs.1)?;
                    let v = &self.outs.1;
                    s.b(v.w, v.t0, v.recs.iter().copied());
                    got = true;
                }
            }
            if self.want[2] {
                for e in self.fresh(2, VC::ID)? {
                    self.f.decode_block_into(&e, &mut self.scratch, &mut self.outs.2)?;
                    vl::sink_c(s, &self.outs.2);
                    got = true;
                }
            }
            Ok(got)
        }
    }
}

// ---------------------------------------------------------------- SQLite
// One table per type, one row per block holding the packed records
// (little-endian, exact field widths); one transaction per publication.
mod sq {
    use super::*;
    use rusqlite::{Connection, OpenFlags, params};

    fn pack_a(b: &BA, out: &mut Vec<u8>) {
        out.clear();
        for r in &b.recs {
            out.extend_from_slice(&r.t.to_le_bytes());
            out.extend(r.a.iter().map(|&v| v as u8));
            out.extend(r.f.iter().map(|&v| v as u8));
            for v in r.x {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    fn pack_b(b: &BB, out: &mut Vec<u8>) {
        out.clear();
        for r in &b.recs {
            out.extend_from_slice(&r.t.to_le_bytes());
            for v in r.x {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&r.status.to_le_bytes());
        }
    }
    fn pack_c(b: &BC, out: &mut Vec<u8>) {
        out.clear();
        for r in &b.recs {
            out.extend_from_slice(&r.t.to_le_bytes());
            out.extend_from_slice(&r.code.to_le_bytes());
            out.extend_from_slice(&(r.msg.len() as u32).to_le_bytes());
            out.extend_from_slice(r.msg.as_bytes());
        }
    }
    fn u64le(b: &[u8]) -> u64 {
        u64::from_le_bytes(b[..8].try_into().unwrap())
    }
    fn unpack_a(d: &[u8]) -> impl ExactSizeIterator<Item = RecA> + '_ {
        d.chunks_exact(1021).map(|c| RecA {
            t: u64le(c),
            a: std::array::from_fn(|j| c[8 + j] as i8),
            f: std::array::from_fn(|j| c[18 + j] != 0),
            x: std::array::from_fn(|j| u16::from_le_bytes(c[21 + 2 * j..23 + 2 * j].try_into().unwrap())),
        })
    }
    fn unpack_b(d: &[u8]) -> impl ExactSizeIterator<Item = RecB> + '_ {
        d.chunks_exact(138).map(|c| RecB {
            t: u64le(c),
            x: std::array::from_fn(|j| f32::from_le_bytes(c[8 + 4 * j..12 + 4 * j].try_into().unwrap())),
            status: u16::from_le_bytes(c[136..138].try_into().unwrap()),
        })
    }
    fn unpack_c(mut d: &[u8]) -> Vec<RecC> {
        let mut v = Vec::new();
        while !d.is_empty() {
            let n = u32::from_le_bytes(d[12..16].try_into().unwrap()) as usize;
            v.push(RecC {
                t: u64le(d),
                code: u32::from_le_bytes(d[8..12].try_into().unwrap()),
                msg: String::from_utf8(d[16..16 + n].to_vec()).unwrap(),
            });
            d = &d[16 + n..];
        }
        v
    }

    pub struct W {
        c: Connection,
        buf: Vec<u8>,
        open: bool,
        full: bool,
        want_full: bool,
        wal_bytes: usize,
    }
    pub fn create(p: &Path) -> Result<Box<dyn AcqWriter>> {
        let c = Connection::open(p)?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE a(w INTEGER PRIMARY KEY, t0 INTEGER, data BLOB);
             CREATE TABLE b(w INTEGER PRIMARY KEY, t0 INTEGER, data BLOB);
             CREATE TABLE c(w INTEGER PRIMARY KEY, t0 INTEGER, data BLOB);",
        )?;
        Ok(Box::new(W { c, buf: Vec::new(), open: false, full: false, want_full: false, wal_bytes: 0 }))
    }
    impl AcqWriter for W {
        fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
            let (a, b, c) = (&a, &b, &c);
            if !self.open {
                // WAL + NORMAL: a commit is visible without syncing the WAL;
                // FULL syncs the WAL at commit. The level is per connection
                // and cannot change inside a transaction.
                if self.want_full != self.full {
                    self.c.execute_batch(if self.want_full { "PRAGMA synchronous=FULL" } else { "PRAGMA synchronous=NORMAL" })?;
                    self.full = self.want_full;
                }
                self.c.execute_batch("BEGIN")?;
                self.open = true;
            }
            pack_a(a, &mut self.buf);
            self.c.prepare_cached("INSERT INTO a VALUES (?1,?2,?3)")?.execute(params![a.w as i64, a.t0 as i64, &self.buf])?;
            self.wal_bytes += self.buf.len();
            pack_b(b, &mut self.buf);
            self.c.prepare_cached("INSERT INTO b VALUES (?1,?2,?3)")?.execute(params![b.w as i64, b.t0 as i64, &self.buf])?;
            self.wal_bytes += self.buf.len();
            pack_c(c, &mut self.buf);
            self.c.prepare_cached("INSERT INTO c VALUES (?1,?2,?3)")?.execute(params![c.w as i64, c.t0 as i64, &self.buf])?;
            self.wal_bytes += self.buf.len();
            Ok(())
        }
        fn will_publish(&mut self, durable: bool) {
            self.want_full = durable;
        }
        fn publish(&mut self, durable: bool) -> Result<()> {
            assert_eq!(durable, self.full, "durability announced at BEGIN");
            if self.open {
                self.c.execute_batch("COMMIT")?;
                self.open = false;
            }
            // Automatic checkpoints (every ~4 MB of WAL) sync the database
            // at arbitrary commits; keep the same WAL budget but checkpoint
            // only at commits that already pay a sync.
            if durable && self.wal_bytes >= 4 << 20 {
                self.c.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
                self.wal_bytes = 0;
            }
            Ok(())
        }
    }
    pub struct R {
        c: Connection,
        want: [bool; 3],
    }
    pub fn open(p: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
        Ok(Box::new(R { c: Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY)?, want }))
    }
    impl AcqReader for R {
        fn poll(&mut self, s: &mut Sink) -> Result<bool> {
            // One read transaction = one consistent snapshot of all tables.
            self.c.execute_batch("BEGIN")?;
            let mut got = false;
            for (i, t) in ["a", "b", "c"].into_iter().enumerate() {
                if !self.want[i] {
                    continue;
                }
                let mut st = self.c.prepare_cached(&format!("SELECT w, t0, data FROM {t} WHERE w >= ?1 ORDER BY w"))?;
                let mut rows = st.query([s.seen[i] as i64])?;
                while let Some(r) = rows.next()? {
                    let (w, t0, d) = (r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get_ref(2)?.as_blob()?);
                    match i {
                        0 => s.a(w, t0, unpack_a(d)),
                        1 => s.b(w, t0, unpack_b(d)),
                        _ => s.c(w, t0, unpack_c(d).into_iter()),
                    }
                    got = true;
                }
            }
            self.c.execute_batch("COMMIT")?;
            Ok(got)
        }
    }
}

// ---------------------------------------------------------------- HDF5 (compound + SWMR)
// Each record type is an HDF5 compound type with fixed-size array members, in
// its own extendible chunked dataset, plus a per-window t0 dataset.
mod h5 {
    use super::*;
    use hdf5::{File, OpenMode, types::FixedAscii};
    use ndarray::{Array1, Array2, s};

    // SWMR does not support variable-length types (global heap), so the
    // message is a fixed-length string, as HDF5 SWMR applications must do.
    #[derive(Clone, hdf5::H5Type)]
    #[repr(C)]
    struct H5C {
        t: u64,
        code: u32,
        msg: FixedAscii<64>,
    }

    pub struct W {
        f: File,
        a: hdf5::Dataset,
        b: hdf5::Dataset,
        c: hdf5::Dataset,
        t0: hdf5::Dataset,
        p: PathBuf,
        n: usize,
    }
    pub fn create(p: &Path) -> Result<Box<dyn AcqWriter>> {
        let f = File::with_options().with_fapl(|a| a.libver_latest()).create(p)?;
        let a = f.new_dataset::<RecA>().chunk(NA).shape(0..).create("A")?;
        let b = f.new_dataset::<RecB>().chunk(NB * 10).shape(0..).create("B")?;
        let c = f.new_dataset::<H5C>().chunk(NC * 100).shape(0..).create("C")?;
        let t0 = f.new_dataset::<u64>().chunk((256, 3)).shape((0.., 3)).create("t0")?;
        f.start_swmr()?;
        Ok(Box::new(W { f, a, b, c, t0, p: p.to_path_buf(), n: 0 }))
    }
    impl AcqWriter for W {
        fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
            let (a, b, c) = (&a, &b, &c);
            let w = self.n;
            self.a.resize((w + 1) * NA)?;
            self.a.write_slice(&Array1::from(a.recs.clone()), s![w * NA..(w + 1) * NA])?;
            self.b.resize((w + 1) * NB)?;
            self.b.write_slice(&Array1::from(b.recs.clone()), s![w * NB..(w + 1) * NB])?;
            let cr: Vec<H5C> = c.recs.iter().map(|r| H5C { t: r.t, code: r.code, msg: FixedAscii::from_ascii(r.msg.as_bytes()).unwrap() }).collect();
            self.c.resize((w + 1) * NC)?;
            self.c.write_slice(&Array1::from(cr), s![w * NC..(w + 1) * NC])?;
            // t0 last: a window is complete when its t0 row is visible.
            self.t0.resize((w + 1, 3))?;
            self.t0.write_slice(&Array2::from_shape_vec((1, 3), vec![a.t0, b.t0, c.t0])?, s![w..w + 1, ..])?;
            self.n += 1;
            Ok(())
        }
        fn publish(&mut self, durable: bool) -> Result<()> {
            // SWMR readers see what H5Fflush wrote; fsync adds durability.
            self.f.flush()?;
            if durable {
                fsync_file(&self.p)?;
            }
            Ok(())
        }
    }
    pub struct R {
        _f: File,
        a: hdf5::Dataset,
        b: hdf5::Dataset,
        c: hdf5::Dataset,
        t0: hdf5::Dataset,
        want: [bool; 3],
    }
    pub fn open(p: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
        let f = File::open_as(p, OpenMode::ReadSWMR)?;
        Ok(Box::new(R { a: f.dataset("A")?, b: f.dataset("B")?, c: f.dataset("C")?, t0: f.dataset("t0")?, _f: f, want }))
    }
    impl AcqReader for R {
        fn poll(&mut self, s: &mut Sink) -> Result<bool> {
            // SWMR refreshes each dataset separately; each type takes the
            // windows visible in both its own dataset and t0.
            self.t0.refresh()?;
            let tw = self.t0.shape()[0] as u64;
            let mut got = false;
            for i in 0..3 {
                if !self.want[i] {
                    continue;
                }
                let (ds, per) = match i {
                    0 => (&self.a, NA),
                    1 => (&self.b, NB),
                    _ => (&self.c, NC),
                };
                ds.refresh()?;
                let vis = (ds.shape()[0] / per) as u64;
                let upto = vis.min(tw);
                let from = s.seen[i];
                if upto <= from {
                    continue;
                }
                let t0: Array2<u64> = self.t0.read_slice(s![from as usize..upto as usize, ..])?;
                let (a, b) = (from as usize * per, upto as usize * per);
                match i {
                    0 => {
                        let v: Array1<RecA> = ds.read_slice_1d(a..b)?;
                        for (k, w) in (from..upto).enumerate() {
                            s.a(w, t0[[k, 0]], v.slice(s![k * per..(k + 1) * per]).iter().copied().collect::<Vec<_>>().into_iter());
                        }
                    }
                    1 => {
                        let v: Array1<RecB> = ds.read_slice_1d(a..b)?;
                        for (k, w) in (from..upto).enumerate() {
                            s.b(w, t0[[k, 1]], v.slice(s![k * per..(k + 1) * per]).iter().copied().collect::<Vec<_>>().into_iter());
                        }
                    }
                    _ => {
                        let v: Array1<H5C> = ds.read_slice_1d(a..b)?;
                        for (k, w) in (from..upto).enumerate() {
                            let recs: Vec<RecC> = v
                                .slice(s![k * per..(k + 1) * per])
                                .iter()
                                .map(|r| RecC { t: r.t, code: r.code, msg: r.msg.as_str().to_string() })
                                .collect();
                            s.c(w, t0[[k, 2]], recs.into_iter());
                        }
                    }
                }
                got = true;
            }
            Ok(got)
        }
    }
}

// ---------------------------------------------------------------- Parquet
// One schema per file: per publication one file per type (FixedSizeList
// columns keep exact element types), then an empty marker renamed into place
// last so a publication becomes visible for all types at once.
mod pq {
    use super::*;
    use arrow_array::builder::{BooleanBuilder, FixedSizeListBuilder, Float32Builder, Int8Builder, UInt16Builder};
    use arrow_array::{Array, ArrayRef, FixedSizeListArray, RecordBatch, StringArray, UInt16Array, UInt32Array, UInt64Array};
    use parquet::arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder};
    use parquet::file::properties::WriterProperties;
    use std::sync::Arc;

    pub struct W {
        dir: PathBuf,
        pend: [Vec<RecordBatch>; 3],
        n: u64,
        /// Files and markers published since the last durable publication.
        unsynced: Vec<PathBuf>,
    }
    pub fn create(dir: &Path) -> Result<Box<dyn AcqWriter>> {
        std::fs::create_dir_all(dir)?;
        Ok(Box::new(W { dir: dir.to_path_buf(), pend: Default::default(), n: 0, unsynced: Vec::new() }))
    }
    impl W {
        fn sync_unsynced(&mut self) -> Result<()> {
            for p in self.unsynced.drain(..) {
                fsync_file(&p)?;
            }
            fsync_dir(&self.dir)
        }
    }
    fn fsl<B: arrow_array::builder::ArrayBuilder>(b: B, n: i32) -> FixedSizeListBuilder<B> {
        FixedSizeListBuilder::new(b, n)
    }
    fn batch_a(b: &BA) -> Result<RecordBatch> {
        let mut a = fsl(Int8Builder::new(), 10);
        let mut f = fsl(BooleanBuilder::new(), 3);
        let mut x = fsl(UInt16Builder::new(), 500);
        for r in &b.recs {
            a.values().append_slice(&r.a);
            a.append(true);
            for v in r.f {
                f.values().append_value(v);
            }
            f.append(true);
            x.values().append_slice(&r.x);
            x.append(true);
        }
        let n = b.recs.len();
        Ok(RecordBatch::try_from_iter([
            ("w", Arc::new(UInt64Array::from(vec![b.w; n])) as ArrayRef),
            ("t0", Arc::new(UInt64Array::from(vec![b.t0; n]))),
            ("t", Arc::new(UInt64Array::from_iter_values(b.recs.iter().map(|r| r.t)))),
            ("a", Arc::new(a.finish())),
            ("f", Arc::new(f.finish())),
            ("x", Arc::new(x.finish())),
        ])?)
    }
    fn batch_b(b: &BB) -> Result<RecordBatch> {
        let mut x = fsl(Float32Builder::new(), 32);
        for r in &b.recs {
            x.values().append_slice(&r.x);
            x.append(true);
        }
        let n = b.recs.len();
        Ok(RecordBatch::try_from_iter([
            ("w", Arc::new(UInt64Array::from(vec![b.w; n])) as ArrayRef),
            ("t0", Arc::new(UInt64Array::from(vec![b.t0; n]))),
            ("t", Arc::new(UInt64Array::from_iter_values(b.recs.iter().map(|r| r.t)))),
            ("x", Arc::new(x.finish())),
            ("status", Arc::new(UInt16Array::from_iter_values(b.recs.iter().map(|r| r.status)))),
        ])?)
    }
    fn batch_c(b: &BC) -> Result<RecordBatch> {
        let n = b.recs.len();
        Ok(RecordBatch::try_from_iter([
            ("w", Arc::new(UInt64Array::from(vec![b.w; n])) as ArrayRef),
            ("t0", Arc::new(UInt64Array::from(vec![b.t0; n]))),
            ("t", Arc::new(UInt64Array::from_iter_values(b.recs.iter().map(|r| r.t)))),
            ("code", Arc::new(UInt32Array::from_iter_values(b.recs.iter().map(|r| r.code)))),
            ("msg", Arc::new(StringArray::from_iter_values(b.recs.iter().map(|r| r.msg.as_str())))),
        ])?)
    }
    impl AcqWriter for W {
        fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
            let (a, b, c) = (&a, &b, &c);
            self.pend[0].push(batch_a(a)?);
            self.pend[1].push(batch_b(b)?);
            self.pend[2].push(batch_c(c)?);
            Ok(())
        }
        fn publish(&mut self, durable: bool) -> Result<()> {
            if self.pend[0].is_empty() {
                if durable {
                    self.sync_unsynced()?;
                }
                return Ok(());
            }
            let props = WriterProperties::builder()
                .set_compression(parquet::basic::Compression::UNCOMPRESSED)
                .set_dictionary_enabled(false)
                .build();
            for (i, t) in ["a", "b", "c"].into_iter().enumerate() {
                let batches = std::mem::take(&mut self.pend[i]);
                let p = self.dir.join(format!("{t}_{:06}.parquet", self.n));
                let mut w = ArrowWriter::try_new(std::fs::File::create(&p)?, batches[0].schema(), Some(props.clone()))?;
                for b in &batches {
                    w.write(b)?;
                }
                w.into_inner()?;
                self.unsynced.push(p);
            }
            // A file is immutable once its footer is written; the marker
            // makes this publication's files visible together. Durability:
            // sync the data files before the marker that names them.
            if durable {
                self.sync_unsynced()?;
            }
            let tmp = self.dir.join("tmp.done");
            let marker = std::fs::File::create(&tmp)?;
            let done = self.dir.join(format!("{:06}.done", self.n));
            if durable {
                marker.sync_all()?;
            }
            std::fs::rename(&tmp, &done)?;
            if durable {
                fsync_dir(&self.dir)?;
            } else {
                self.unsynced.push(done);
            }
            self.n += 1;
            Ok(())
        }
    }
    pub struct R {
        dir: PathBuf,
        want: [bool; 3],
        n: u64,
    }
    pub fn open(dir: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
        Ok(Box::new(R { dir: dir.to_path_buf(), want, n: 0 }))
    }
    fn col<'a, T: 'static>(b: &'a RecordBatch, n: &str) -> &'a T {
        b.column_by_name(n).unwrap().as_any().downcast_ref::<T>().unwrap()
    }
    fn list_vals<'a, T: 'static>(b: &'a RecordBatch, n: &str) -> (usize, &'a T) {
        let l = col::<FixedSizeListArray>(b, n);
        (l.value_length() as usize, l.values().as_any().downcast_ref::<T>().unwrap())
    }
    impl AcqReader for R {
        fn poll(&mut self, s: &mut Sink) -> Result<bool> {
            let mut got = false;
            while self.dir.join(format!("{:06}.done", self.n)).exists() {
                for (i, t) in ["a", "b", "c"].into_iter().enumerate() {
                    if !self.want[i] {
                        continue;
                    }
                    let p = self.dir.join(format!("{t}_{:06}.parquet", self.n));
                    let mut rows: Vec<(u64, u64, usize, RecordBatch)> = Vec::new();
                    for b in ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(p)?)?.build()? {
                        let b = b?;
                        for k in 0..b.num_rows() {
                            rows.push((col::<UInt64Array>(&b, "w").value(k), col::<UInt64Array>(&b, "t0").value(k), k, b.clone()));
                        }
                    }
                    // Group consecutive rows by window.
                    let mut k = 0;
                    while k < rows.len() {
                        let w = rows[k].0;
                        let t0 = rows[k].1;
                        let end = (k..rows.len()).find(|&j| rows[j].0 != w).unwrap_or(rows.len());
                        let it = rows[k..end].iter().map(|(_, _, r, b)| (*r, b));
                        match i {
                            0 => s.a(w, t0, it.map(|(r, b)| {
                                let (_, a) = list_vals::<arrow_array::Int8Array>(b, "a");
                                let (_, f) = list_vals::<arrow_array::BooleanArray>(b, "f");
                                let (_, x) = list_vals::<UInt16Array>(b, "x");
                                RecA {
                                    t: col::<UInt64Array>(b, "t").value(r),
                                    a: std::array::from_fn(|j| a.value(r * 10 + j)),
                                    f: std::array::from_fn(|j| f.value(r * 3 + j)),
                                    x: std::array::from_fn(|j| x.value(r * 500 + j)),
                                }
                            }).collect::<Vec<_>>().into_iter()),
                            1 => s.b(w, t0, it.map(|(r, b)| {
                                let (_, x) = list_vals::<arrow_array::Float32Array>(b, "x");
                                RecB {
                                    t: col::<UInt64Array>(b, "t").value(r),
                                    x: std::array::from_fn(|j| x.value(r * 32 + j)),
                                    status: col::<UInt16Array>(b, "status").value(r),
                                }
                            }).collect::<Vec<_>>().into_iter()),
                            _ => s.c(w, t0, it.map(|(r, b)| RecC {
                                t: col::<UInt64Array>(b, "t").value(r),
                                code: col::<UInt32Array>(b, "code").value(r),
                                msg: col::<StringArray>(b, "msg").value(r).to_string(),
                            }).collect::<Vec<_>>().into_iter()),
                        }
                        k = end;
                    }
                }
                self.n += 1;
                got = true;
            }
            Ok(got)
        }
    }
}

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
            if tx.send(Blk { w, t0: mono_ns(), recs }).is_err() {
                return;
            }
        }
    });
    rx
}

pub fn run(fmt: &str, dir: &Path) -> Result<Metrics> {
    let p = target(fmt, dir);
    let mut w: Box<dyn AcqWriter> = match fmt {
        "varlit" => vl::create(&p)?,
        "varlit-native" => vn::create(&p)?,
        "varlit-native-raw" => vr::create(&p)?,
        "varlit-native-rec" => vq::create(&p)?,
        "sqlite" => sq::create(&p)?,
        "hdf5" => h5::create(&p)?,
        "parquet" => pq::create(&p)?,
        _ => return Err("format".into()),
    };
    let exe = std::env::current_exe()?;
    let mut roles = vec!["all"; all_readers()];
    roles.push("b");
    let mut kids = roles
        .iter()
        .enumerate()
        .map(|(i, r)| {
            std::process::Command::new(&exe)
                .args(["follow-mixed", fmt, dir.to_str().unwrap(), r, &i.to_string()])
                .spawn()
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !(0..roles.len()).all(|i| dir.join(format!("ready-{i}")).exists()) {
        assert!(std::time::Instant::now() < deadline, "followers did not start");
        std::thread::sleep(Duration::from_millis(1));
    }
    let start = std::time::Instant::now();
    let (ra, rb, rc) = (producer(NA, gen_a, start), producer(NB, gen_b, start), producer(NC, gen_c, start));
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
        assert!(k.wait()?.success(), "follower failed");
    }
    drop(w);
    let mut m = Metrics::default();
    let data_s = windows() as f64 * WIN_NS as f64 / 1e9;
    m.set("write_s", wall);
    m.set("MBps", windows() as f64 * bytes_per_window() as f64 / wall / 1e6);
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
        let (mut p50, mut p99, mut inc, mut obs, mut cpu, mut n) = (0f64, 0f64, 0u64, 0u64, 0f64, 0f64);
        let (mut late, mut seen) = (0usize, 0usize);
        for (i, r) in roles.iter().enumerate() {
            if *r != role {
                continue;
            }
            let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join(format!("mixed-{i}.json")))?)?;
            let mut l: Vec<f64> = v["lags"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
            l.sort_by(f64::total_cmp);
            late += l.iter().filter(|&&x| x > fresh_ms() as f64).count();
            seen += l.len();
            p50 = p50.max(percentile(&l, 0.5));
            p99 = p99.max(percentile(&l, 0.99));
            inc += v["inconsistent"].as_u64().unwrap();
            obs += v["observations"].as_u64().unwrap();
            cpu += v["cpu_s"].as_f64().unwrap();
            n += 1.0;
            std::fs::remove_file(dir.join(format!("mixed-{i}.json")))?;
            std::fs::remove_file(dir.join(format!("ready-{i}")))?;
        }
        let bytes = if role == "b" { (NB * 138) as f64 * windows() as f64 } else { (windows() * bytes_per_window()) as f64 };
        m.set(&format!("{role}_lag_p50_ms"), p50);
        m.set(&format!("{role}_lag_p99_ms"), p99);
        m.set(&format!("{role}_late_frac"), late as f64 / seen.max(1) as f64);
        m.set(&format!("{role}_inconsistent"), inc);
        m.set(&format!("{role}_observations"), obs);
        m.set(&format!("{role}_cpu_s"), cpu / n);
        m.set(&format!("{role}_MB_per_cpu_s"), bytes / (cpu / n) / 1e6);
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
    Ok(m)
}

pub fn follower_main(fmt: &str, dir: &Path, role: &str, idx: usize) -> Result<()> {
    let p = target(fmt, dir);
    let want = if role == "b" { [false, true, false] } else { [true, true, true] };
    let mut r: Box<dyn AcqReader> = match fmt {
        "varlit" => vl::open(&p, want)?,
        "varlit-native" => vn::open(&p, want)?,
        "varlit-native-raw" => vr::open(&p, want)?,
        "varlit-native-rec" => vq::open(&p, want)?,
        "sqlite" => sq::open(&p, want)?,
        "hdf5" => h5::open(&p, want)?,
        _ => pq::open(&p, want)?,
    };
    std::fs::write(dir.join(format!("ready-{idx}")), b"")?;
    let mut s = Sink { want, seen: [0; 3], lags: Vec::new(), inconsistent: 0, observations: 0, sum: 0, verify: verify() };
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
    let out = serde_json::json!({ "lags": s.lags, "inconsistent": s.inconsistent, "observations": s.observations, "cpu_s": cpu_s });
    std::fs::write(dir.join(format!("mixed-{idx}.json")), out.to_string())?;
    Ok(())
}

/// Codec probe: encode cost of one A window as typed Vec<u16> vs raw bytes.
/// Reader-side allocation probe: decode one A window (as the bulk codec does:
/// one bounds-checked slice, element conversion into a Vec) with a fresh Vec per
/// block vs a reused one. No library change; estimates a `decode_into` gain.
pub fn reuse_probe() -> Result<()> {
    use varlit::{Endian, encode_to_vec};
    let blk = Blk { w: 0, t0: 0, recs: (0..NA as u64).map(gen_a).collect() };
    let typed = vl::to_a(&blk);
    let bytes = encode_to_vec(&typed.x, Endian::Little)?;
    let payload = &bytes[8..];
    let n = 400;
    let decode = |out: &mut Vec<u16>| {
        out.clear();
        out.reserve_exact(payload.len() / 2);
        out.extend(payload.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)));
    };
    let t = Timer::start();
    let mut sink = 0u64;
    for _ in 0..n {
        let mut v = Vec::new();
        decode(&mut v);
        sink = sink.wrapping_add(v[v.len() - 1] as u64);
    }
    let fresh = t.secs();
    let t = Timer::start();
    let mut v = Vec::new();
    for _ in 0..n {
        decode(&mut v);
        sink = sink.wrapping_add(v[v.len() - 1] as u64);
    }
    let reused = t.secs();
    let mb = (payload.len() * n) as f64 / 1e6;
    println!("decode Vec<u16> 1 MB: fresh Vec {:.0} MB/s, reused Vec {:.0} MB/s [{sink}]", mb / fresh, mb / reused);
    // Full library decode of the whole A block (all fields), for scale.
    let whole = encode_to_vec(&typed, Endian::Little)?;
    let t = Timer::start();
    for _ in 0..n {
        let b: vl::ABlock = varlit::decode_from_slice(&whole, Endian::Little)?;
        sink = sink.wrapping_add(b.x.len() as u64);
    }
    println!("library ABlock decode (fresh): {:.0} MB/s [{sink}]", (whole.len() * n) as f64 / 1e6 / t.secs());
    let mut b = <vl::ABlock as varlit::VarlitEmpty>::empty();
    let t = Timer::start();
    for _ in 0..n {
        varlit::decode_from_slice_into(&whole, Endian::Little, &mut b)?;
        sink = sink.wrapping_add(b.x.len() as u64);
    }
    println!("library ABlock decode (into, reused): {:.0} MB/s [{sink}]", (whole.len() * n) as f64 / 1e6 / t.secs());
    // File-level: decode every A window of a written file through the
    // caller-owned-output read, fresh `out` per record (the old API's cost)
    // versus one reused `out`.
    let dir = std::env::temp_dir().join(format!("fb-reuse-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("m.vl");
    {
        let mut w = vn::create(&path)?;
        let wins = 200u64;
        for wi in 0..wins {
            let a = Blk { w: wi, t0: wi, recs: (0..NA as u64).map(|k| gen_a(wi * NA as u64 + k)).collect() };
            let b = Blk { w: wi, t0: wi, recs: (0..NB as u64).map(|k| gen_b(wi * NB as u64 + k)).collect() };
            let c = Blk { w: wi, t0: wi, recs: (0..NC as u64).map(|k| gen_c(wi * NC as u64 + k)).collect() };
            w.append(a, b, c)?;
            w.publish(true)?;
        }
    }
    let f = varlit::VarlitFile::open_readonly(vl_spec(vn::native_spec()), &path)?;
    let mut entries = Vec::new();
    f.block_entries_into::<vl::ABlock>(&mut entries)?;
    let mut scratch = Vec::new();
    let bytes: u64 = entries.iter().map(|e| e.payload_len as u64).sum();
    for round in 0..3 {
        let t = Timer::start();
        for e in &entries {
            let mut v = <vl::ABlock as varlit::VarlitEmpty>::empty();
            f.decode_block_into(e, &mut scratch, &mut v)?;
            sink = sink.wrapping_add(v.x.len() as u64);
        }
        let fresh = t.secs();
        let mut v = <vl::ABlock as varlit::VarlitEmpty>::empty();
        let t = Timer::start();
        for e in &entries {
            f.decode_block_into(e, &mut scratch, &mut v)?;
            sink = sink.wrapping_add(v.x.len() as u64);
        }
        let reused = t.secs();
        println!(
            "file A-window reads ({} x {:.1} MB) round {round}: fresh out {:.0} MB/s, reused out {:.0} MB/s [{sink}]",
            entries.len(),
            bytes as f64 / entries.len() as f64 / 1e6,
            bytes as f64 / 1e6 / fresh,
            bytes as f64 / 1e6 / reused
        );
    }
    // Decode straight from a read-only mapping (no copy into scratch).
    for round in 0..3 {
        // Fresh mapping per pass (what a follower remapping each generation pays).
        let t = Timer::start();
        // SAFETY: the probe file is closed for writing and not modified.
        let map = unsafe { f.mmap_payloads()? };
        let mut v = <vl::ABlock as varlit::VarlitEmpty>::empty();
        for e in &entries {
            map.decode_block_into(e, &mut scratch, &mut v)?;
            sink = sink.wrapping_add(v.x.len() as u64);
        }
        let cold = t.secs();
        let t = Timer::start();
        for e in &entries {
            map.decode_block_into(e, &mut scratch, &mut v)?;
            sink = sink.wrapping_add(v.x.len() as u64);
        }
        let warm = t.secs();
        println!(
            "mmap A-window decode round {round}: new mapping {:.0} MB/s, warm mapping {:.0} MB/s [{sink}]",
            bytes as f64 / 1e6 / cold,
            bytes as f64 / 1e6 / warm
        );
    }
    macro_rules! probe {
        ($ty:ty, $name:expr) => {{
            let mut entries = Vec::new();
            f.block_entries_into::<$ty>(&mut entries)?;
            let bytes: u64 = entries.iter().map(|e| e.payload_len as u64).sum();
            let reps = 50;
            let t = Timer::start();
            for _ in 0..reps {
                for e in &entries {
                    let mut v = <$ty as varlit::VarlitEmpty>::empty();
                    f.decode_block_into(e, &mut scratch, &mut v)?;
                    sink = sink.wrapping_add(v.t.len() as u64);
                }
            }
            let fresh = t.secs();
            let mut v = <$ty as varlit::VarlitEmpty>::empty();
            let t = Timer::start();
            for _ in 0..reps {
                for e in &entries {
                    f.decode_block_into(e, &mut scratch, &mut v)?;
                    sink = sink.wrapping_add(v.t.len() as u64);
                }
            }
            let reused = t.secs();
            let recs = (entries.len() * reps) as f64;
            println!(
                "file {} reads ({} x {} B): fresh out {:.2} us/rec, reused out {:.2} us/rec [{sink}]",
                $name, entries.len(), bytes / entries.len() as u64,
                fresh * 1e6 / recs, reused * 1e6 / recs
            );
        }};
    }
    probe!(vl::BBlock, "B");
    probe!(vl::CBlock, "C");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

pub fn codec_probe() -> Result<()> {
    use varlit::{Endian, encode_to_vec};
    let blk = Blk { w: 0, t0: 0, recs: (0..NA as u64).map(gen_a).collect() };
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
    println!("ABlock encode: {:.0} MB/s; crc32: {:.0} MB/s [{c}]", wmb / te, wmb / tc);
    println!("Vec<u16> 500k elems: {:.0} MB/s ({:.2} ns/elem); same bytes as Vec<u8>: {:.0} MB/s  [{}]", mb / tv, tv * 1e9 / (n * typed.x.len()) as f64, mb / tr, sz);
    Ok(())
}
