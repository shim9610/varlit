//! Shared helpers: deterministic data, timing, file accounting, metric output.
use std::{
    collections::BTreeMap,
    ffi::{c_int, c_long},
    path::Path,
    time::Instant,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[repr(C)]
struct Timespec {
    sec: c_long,
    nsec: c_long,
}
unsafe extern "C" {
    fn clock_gettime(id: c_int, t: *mut Timespec) -> c_int;
}

/// CLOCK_MONOTONIC, comparable across processes on the same host.
pub fn mono_ns() -> u64 {
    let mut t = Timespec { sec: 0, nsec: 0 };
    assert_eq!(unsafe { clock_gettime(1, &mut t) }, 0);
    t.sec as u64 * 1_000_000_000 + t.nsec as u64
}

pub fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Deterministic, incompressible payload for `seq`.
pub fn fill_payload(seq: u64, out: &mut [u8]) {
    for (i, c) in out.chunks_mut(8).enumerate() {
        let v = splitmix(seq.wrapping_mul(0x1_0000_0001).wrapping_add(i as u64)).to_le_bytes();
        c.copy_from_slice(&v[..c.len()]);
    }
}

pub fn fsync_dir(dir: &Path) -> Result<()> {
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

pub fn fsync_file(path: &Path) -> Result<()> {
    std::fs::OpenOptions::new().read(true).open(path)?.sync_all()?;
    Ok(())
}

/// Total *allocated* bytes of all files under `root` (sidecars included;
/// sparse preallocation is not counted).
pub fn dir_bytes(root: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let m = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if m.is_dir() {
                n += dir_bytes(&e.path());
            } else {
                n += m.blocks() * 512;
            }
        }
    }
    n
}

pub struct Timer(Instant);
impl Timer {
    pub fn start() -> Self {
        Self(Instant::now())
    }
    pub fn secs(&self) -> f64 {
        self.0.elapsed().as_secs_f64()
    }
}

/// Ordered metric map printed as one JSON line.
#[derive(Default)]
pub struct Metrics(pub BTreeMap<String, serde_json::Value>);
impl Metrics {
    pub fn set(&mut self, k: &str, v: impl Into<serde_json::Value>) {
        self.0.insert(k.to_string(), v.into());
    }
    pub fn print(&self) {
        println!("{}", serde_json::to_string(&self.0).unwrap());
    }
}

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

unsafe extern "C" {
    fn posix_fadvise(fd: c_int, offset: i64, len: i64, advice: c_int) -> c_int;
}

/// Evict every file under `root` from the page cache (clean pages only, so
/// files are fsynced first). Gives cold-read numbers without root privileges.
pub fn evict(root: &Path) {
    use std::os::fd::AsRawFd;
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            evict(&p);
        } else if let Ok(f) = std::fs::OpenOptions::new().read(true).open(&p) {
            let _ = f.sync_all();
            unsafe { posix_fadvise(f.as_raw_fd(), 0, 0, 4) }; // POSIX_FADV_DONTNEED
        }
    }
}

/// `<format>+trusted` selects `ReadValidation::TRUSTED` for every Varlit
/// reader (set once in main, inherited by follower processes via env).
pub fn trusted() -> bool {
    std::env::var_os("FB_TRUSTED").is_some()
}
pub fn vl_limits() -> varlit::ReadLimits {
    if trusted() {
        varlit::ReadLimits::MISSING.with_read_validation(varlit::ReadValidation::TRUSTED)
    } else {
        varlit::ReadLimits::MISSING
    }
}
pub fn vl_spec(spec: varlit::FormatSpec) -> varlit::FormatSpec {
    spec.with_resource_limits(vl_limits())
}
pub fn vl_dio(mut o: varlit::DiskIndexOptions) -> varlit::DiskIndexOptions {
    o.limits = o.limits.overlay(vl_limits());
    o
}
pub fn vl_stream(mut o: varlit::StreamOptions) -> varlit::StreamOptions {
    o.limits = o.limits.overlay(vl_limits());
    o.state = vl_dio(o.state);
    o
}

/// CPU seconds consumed by this process (all threads).
pub fn process_cpu_s() -> f64 {
    let mut t = Timespec { sec: 0, nsec: 0 };
    assert_eq!(unsafe { clock_gettime(2, &mut t) }, 0); // CLOCK_PROCESS_CPUTIME_ID
    t.sec as f64 + t.nsec as f64 / 1e9
}

/// A keyed read into a fresh caller-owned value; `what` names a missing key.
pub fn got<T: varlit::VarlitEmpty>(
    read: impl FnOnce(&mut T) -> varlit::Result<bool>,
    what: &'static str,
) -> Result<T> {
    let mut out = T::empty();
    if read(&mut out)? { Ok(out) } else { Err(what.into()) }
}
