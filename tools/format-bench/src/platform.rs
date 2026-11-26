//! What the harness needs from the operating system, per operating system:
//! a monotonic clock every process of a run shares, process CPU time, peak
//! resident memory, page-cache eviction for cold reads, and the SQLite
//! settings that give it the same durability as the Rust engines.
//!
//! Every function exists on every platform. Where a platform cannot provide
//! the measurement, the function says so ([`EVICTS`], [`PEAK_RSS`],
//! [`PROCESS_CPU`]) and the scenario records that next to its numbers,
//! rather than reporting a value that looks measured.

#![allow(unsafe_code)]

use std::path::Path;

#[cfg(unix)]
use std::ffi::{c_int, c_long};

#[cfg(unix)]
#[repr(C)]
struct Timespec {
    sec: c_long,
    nsec: c_long,
}

#[cfg(unix)]
unsafe extern "C" {
    fn clock_gettime(id: c_int, t: *mut Timespec) -> c_int;
    fn getrusage(who: c_int, usage: *mut c_long) -> c_int;
}

/// `CLOCK_MONOTONIC` and `CLOCK_PROCESS_CPUTIME_ID`: the constants differ by
/// operating system, so each one is named here.
#[cfg(any(target_os = "linux", target_os = "android"))]
const CLOCKS: (c_int, c_int) = (1, 2);
#[cfg(any(target_os = "macos", target_os = "ios"))]
const CLOCKS: (c_int, c_int) = (6, 12);
#[cfg(target_os = "freebsd")]
const CLOCKS: (c_int, c_int) = (4, 15);
#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    ))
))]
compile_error!("format-bench: add this platform's CLOCK_MONOTONIC and CLOCK_PROCESS_CPUTIME_ID");

#[cfg(unix)]
fn clock(id: c_int) -> (u64, u64) {
    let mut t = Timespec { sec: 0, nsec: 0 };
    assert_eq!(
        unsafe { clock_gettime(id, &mut t) },
        0,
        "clock_gettime({id})"
    );
    (t.sec as u64, t.nsec as u64)
}

/// A monotonic clock in nanoseconds, comparable across processes on the same
/// host (followers stamp what they see with it).
#[cfg(unix)]
pub fn mono_ns() -> u64 {
    let (s, ns) = clock(CLOCKS.0);
    s * 1_000_000_000 + ns
}

/// On Windows the precise wall clock: `QueryPerformanceCounter` is per boot,
/// not per process, but `SystemTime` has sub-microsecond resolution and is the
/// clock every process shares. A clock adjustment during a run would show in
/// its latencies.
#[cfg(not(unix))]
pub fn mono_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// Whether [`process_cpu_s`] measures.
pub const PROCESS_CPU: bool = cfg!(any(unix, windows));

/// CPU seconds this process has used, all threads.
#[cfg(unix)]
pub fn process_cpu_s() -> f64 {
    let (s, ns) = clock(CLOCKS.1);
    s as f64 + ns as f64 / 1e9
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FileTime {
    low: u32,
    high: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> isize;
    fn GetProcessTimes(
        process: isize,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn K32GetProcessMemoryInfo(
        process: isize,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
}

#[cfg(windows)]
pub fn process_cpu_s() -> f64 {
    let (mut c, mut e, mut k, mut u) = Default::default();
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    assert!(ok != 0, "GetProcessTimes");
    let ticks = |t: FileTime| (u64::from(t.high) << 32 | u64::from(t.low)) as f64;
    (ticks(k) + ticks(u)) / 1e7 // 100 ns units
}

#[cfg(not(any(unix, windows)))]
pub fn process_cpu_s() -> f64 {
    0.0
}

/// Whether [`peak_rss_mb`] measures.
pub const PEAK_RSS: bool = cfg!(any(unix, windows));

/// Peak resident set of this process, MiB.
#[cfg(unix)]
pub fn peak_rss_mb() -> u64 {
    // `struct rusage` is two `struct timeval`s and fourteen `long`s on every
    // supported Unix, 32- or 64-bit; `ru_maxrss` is the third `long` after the
    // timevals (index 4).
    let mut usage = [0 as c_long; 18];
    if unsafe { getrusage(0, usage.as_mut_ptr()) } != 0 {
        return 0;
    }
    let maxrss = usage[4].max(0) as u64;
    // Bytes on Apple platforms, KiB elsewhere.
    if cfg!(any(target_os = "macos", target_os = "ios")) {
        maxrss / (1024 * 1024)
    } else {
        maxrss / 1024
    }
}

#[cfg(windows)]
pub fn peak_rss_mb() -> u64 {
    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        return 0;
    }
    counters.peak_working_set_size as u64 / (1024 * 1024)
}

#[cfg(not(any(unix, windows)))]
pub fn peak_rss_mb() -> u64 {
    0
}

/// Whether [`evict`] drops files from the page cache, so that "cold" reads
/// read the disk. Linux and FreeBSD can (`posix_fadvise(DONTNEED)` on clean
/// pages, no privilege needed). macOS and Windows have no unprivileged
/// equivalent: their "cold" reads are the first read after the write and may
/// be served from the cache.
pub const EVICTS: bool = cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd"
));

#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
unsafe extern "C" {
    fn posix_fadvise(fd: c_int, offset: i64, len: i64, advice: c_int) -> c_int;
}

/// Evict every file under `root` from the page cache (clean pages only, so
/// files are synced first). See [`EVICTS`].
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
pub fn evict(root: &Path) {
    use std::os::fd::AsRawFd;
    const DONTNEED: c_int = 4; // POSIX_FADV_DONTNEED on Linux and FreeBSD
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            evict(&p);
        } else if let Ok(f) = std::fs::OpenOptions::new().read(true).open(&p) {
            let _ = f.sync_all();
            unsafe { posix_fadvise(f.as_raw_fd(), 0, 0, DONTNEED) };
        }
    }
}

/// Syncs every file under `root` so the read that follows starts from the
/// same durable state as on Linux; nothing is evicted (see [`EVICTS`]).
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
pub fn evict(root: &Path) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            evict(&p);
        } else if let Ok(f) = std::fs::OpenOptions::new().read(true).open(&p) {
            let _ = f.sync_all();
        }
    }
}

/// SQLite settings that make its durable commits match the Rust engines'.
///
/// Rust's `File::sync_data`/`sync_all` on Apple platforms are `F_FULLFSYNC`,
/// which also flushes the drive's write cache; SQLite's `fsync` there is a
/// plain `fsync(2)` unless `fullfsync` is on. Without these pragmas SQLite
/// would be measured with a weaker durability guarantee than Varlit, HDF5 and
/// Parquet on the same Mac. Elsewhere the defaults already match.
pub const SQLITE_DURABILITY: &str = if cfg!(any(target_os = "macos", target_os = "ios")) {
    "PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON;"
} else {
    ""
};

/// Opens a SQLite database for writing with [`SQLITE_DURABILITY`] applied.
pub fn sqlite_open(p: &Path) -> rusqlite::Result<rusqlite::Connection> {
    let c = rusqlite::Connection::open(p)?;
    if !SQLITE_DURABILITY.is_empty() {
        c.execute_batch(SQLITE_DURABILITY)?;
    }
    Ok(c)
}

/// One line describing what this build measures, recorded in every result.
pub fn caveats() -> serde_json::Value {
    serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cold_reads_evicted": EVICTS,
        "peak_rss_measured": PEAK_RSS,
        "process_cpu_measured": PROCESS_CPU,
        "sqlite_fullfsync": !SQLITE_DURABILITY.is_empty(),
    })
}
