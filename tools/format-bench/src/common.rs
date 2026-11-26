//! Shared helpers: deterministic data, timing, file accounting, metric output.
use std::{collections::BTreeMap, path::Path, time::Instant};

pub use crate::platform::{evict, mono_ns, peak_rss_mb, process_cpu_s};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

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
    std::fs::OpenOptions::new()
        .read(true)
        .open(path)?
        .sync_all()?;
    Ok(())
}

/// Total *allocated* bytes of all files under `root` (sidecars included;
/// sparse preallocation is not counted).
pub fn dir_bytes(root: &Path) -> u64 {
    #[cfg(unix)]
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
                #[cfg(unix)]
                {
                    n += m.blocks() * 512;
                }
                // Off Unix the file length: sparse space counts as allocated.
                #[cfg(not(unix))]
                {
                    n += m.len();
                }
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
        let mut out = self.0.clone();
        out.entry("platform".to_string())
            .or_insert_with(crate::platform::caveats);
        println!("{}", serde_json::to_string(&out).unwrap());
    }
}

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
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
pub fn vl_stream(mut o: varlit::StreamOptions) -> varlit::StreamOptions {
    o.limits = o.limits.overlay(vl_limits());
    o
}

/// A keyed read into a fresh caller-owned value; `what` names a missing key.
pub fn got<T: varlit::VarlitEmpty>(
    read: impl FnOnce(&mut T) -> varlit::Result<bool>,
    what: &'static str,
) -> Result<T> {
    let mut out = T::empty();
    if read(&mut out)? {
        Ok(out)
    } else {
        Err(what.into())
    }
}

/// The storage configuration an engine adapter was asked to use (one of the
/// names its scenario module lists as candidates). Empty = the first.
pub fn cand() -> String {
    std::env::var("FB_CAND").unwrap_or_default()
}

/// Disk-health probe: 1000 x (4 KiB pwrite + fdatasync) on a fresh file.
/// Returns seconds. Independent of every scenario's configuration.
pub fn probe(dir: &Path) -> Result<f64> {
    #[cfg(unix)]
    use std::os::unix::fs::FileExt;
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;
    std::fs::create_dir_all(dir)?;
    let p = dir.join("probe.bin");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&p)?;
    let buf = [0x5au8; 4096];
    let t = Timer::start();
    for i in 0..1000u64 {
        #[cfg(unix)]
        f.write_all_at(&buf, i * 4096)?;
        #[cfg(windows)]
        f.seek_write(&buf, i * 4096)?;
        f.sync_data()?;
    }
    let s = t.secs();
    drop(f);
    std::fs::remove_file(&p)?;
    Ok(s)
}

/// Logical data size every scenario is scaled to, in GB (10^9 bytes), from
/// `FB_DATA_GB`; unset keeps each scenario's own default size. `FB_SMALL`
/// still selects the small smoke shapes and takes precedence.
pub fn data_gb() -> Option<f64> {
    if std::env::var_os("FB_SMALL").is_some() {
        return None;
    }
    std::env::var("FB_DATA_GB")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
}
