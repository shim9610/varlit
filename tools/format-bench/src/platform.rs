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
/// read the disk. Every supported platform can, without privileges:
///
/// - Linux, Android, FreeBSD: `posix_fadvise(POSIX_FADV_DONTNEED)` on the
///   synced (clean) file;
/// - macOS, iOS: map the file and `msync(MS_INVALIDATE)` the mapping, which
///   drops its cached pages (what `vmtouch -e` does there);
/// - Windows: open the file once without buffering (`FILE_FLAG_NO_BUFFERING`),
///   which makes the cache manager flush and purge that file's cached data.
pub const EVICTS: bool = true;

/// How [`evict`] drops pages on this platform; recorded with every result.
pub const EVICTION: &str = if cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd"
)) {
    "posix_fadvise(DONTNEED)"
} else if cfg!(any(target_os = "macos", target_os = "ios")) {
    "mmap + msync(MS_INVALIDATE)"
} else if cfg!(windows) {
    "FILE_FLAG_NO_BUFFERING open"
} else {
    "none"
};

/// Evict every file under `root` from the page cache. Files are synced first,
/// so only clean pages remain to drop. See [`EVICTS`].
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
            evict_file(&p, &f);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
unsafe extern "C" {
    fn posix_fadvise(fd: c_int, offset: i64, len: i64, advice: c_int) -> c_int;
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
fn evict_file(_path: &Path, f: &std::fs::File) {
    use std::os::fd::AsRawFd;
    const DONTNEED: c_int = 4; // POSIX_FADV_DONTNEED on Linux and FreeBSD
    unsafe { posix_fadvise(f.as_raw_fd(), 0, 0, DONTNEED) };
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut u8;
    fn msync(addr: *mut u8, len: usize, flags: c_int) -> c_int;
    fn munmap(addr: *mut u8, len: usize) -> c_int;
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn evict_file(_path: &Path, f: &std::fs::File) {
    use std::os::fd::AsRawFd;
    const PROT_READ: c_int = 0x1;
    const MAP_SHARED: c_int = 0x1;
    const MS_INVALIDATE: c_int = 0x2;
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return;
    };
    let Ok(len) = usize::try_from(len) else {
        return;
    };
    if len == 0 {
        return;
    }
    // SAFETY: a fresh read-only shared mapping of an open file, never
    // dereferenced here; it is invalidated and unmapped before returning.
    unsafe {
        let addr = mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ,
            MAP_SHARED,
            f.as_raw_fd(),
            0,
        );
        if addr as isize == -1 {
            return;
        }
        msync(addr, len, MS_INVALIDATE);
        munmap(addr, len);
    }
}

#[cfg(windows)]
fn evict_file(path: &Path, _f: &std::fs::File) {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
    // Opening (and closing) a non-cached handle purges the file's cached
    // pages; nothing is read through it.
    let _ = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(path);
}

#[cfg(not(any(unix, windows)))]
fn evict_file(_path: &Path, _f: &std::fs::File) {}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn evict_file(_path: &Path, _f: &std::fs::File) {}

/// Fraction of `path`'s pages resident in memory, measured with `mincore`
/// over a fresh mapping; `None` where the platform has no `mincore`
/// (Windows). Lets CI check that [`evict`] really drops pages.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "macos",
    target_os = "ios"
))]
pub fn resident_fraction(path: &Path) -> Option<f64> {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        #[link_name = "mmap"]
        fn mmap2(
            addr: *mut u8,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            off: i64,
        ) -> *mut u8;
        #[link_name = "munmap"]
        fn munmap2(addr: *mut u8, len: usize) -> c_int;
        fn mincore(addr: *mut u8, len: usize, vec: *mut u8) -> c_int;
        fn getpagesize() -> c_int;
    }
    const PROT_READ: c_int = 0x1;
    const MAP_SHARED: c_int = 0x1;
    let f = std::fs::File::open(path).ok()?;
    let len = usize::try_from(f.metadata().ok()?.len()).ok()?;
    if len == 0 {
        return Some(0.0);
    }
    let page = unsafe { getpagesize() } as usize;
    let pages = len.div_ceil(page);
    let mut vec = vec![0u8; pages];
    // SAFETY: a read-only mapping that is never dereferenced; mincore writes
    // one byte per page into `vec`, which has exactly that many.
    unsafe {
        let addr = mmap2(
            std::ptr::null_mut(),
            len,
            PROT_READ,
            MAP_SHARED,
            f.as_raw_fd(),
            0,
        );
        if addr as isize == -1 {
            return None;
        }
        let ok = mincore(addr, len, vec.as_mut_ptr()) == 0;
        munmap2(addr, len);
        if !ok {
            return None;
        }
    }
    Some(vec.iter().filter(|b| **b & 1 != 0).count() as f64 / pages as f64)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "macos",
    target_os = "ios"
)))]
pub fn resident_fraction(_path: &Path) -> Option<f64> {
    None
}

/// Writes a file, reads it back, evicts it and reports how much of it stayed
/// resident before and after: `evict-check <dir>` prints one JSON line.
pub fn evict_check(dir: &Path) -> std::io::Result<serde_json::Value> {
    use std::io::{Read, Write};
    std::fs::create_dir_all(dir)?;
    let path = dir.join("evict-check.bin");
    {
        let mut f = std::fs::File::create(&path)?;
        let block = vec![0x5au8; 1 << 20];
        for _ in 0..64 {
            f.write_all(&block)?;
        }
        f.sync_all()?;
    }
    let mut sink = Vec::new();
    std::fs::File::open(&path)?.read_to_end(&mut sink)?;
    let before = resident_fraction(&path);
    evict(dir);
    let after = resident_fraction(&path);
    std::fs::remove_file(&path)?;
    Ok(
        serde_json::json!({ "eviction": EVICTION, "resident_before": before, "resident_after": after }),
    )
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
        "eviction": EVICTION,
        "peak_rss_measured": PEAK_RSS,
        "process_cpu_measured": PROCESS_CPU,
        "sqlite_fullfsync": !SQLITE_DURABILITY.is_empty(),
    })
}
