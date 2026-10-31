//! Bounds live Rust allocations during repeated decoding. The required Linux
//! release job also checks process peak RSS, including native codec allocations.
#![cfg(all(feature = "integrity", feature = "compression-zstd"))]
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use varlit::*;
struct Meter;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn add(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Relaxed) + bytes;
    PEAK.fetch_max(live, Relaxed);
}
// SAFETY: forwards every allocation and its original layout to System; the
// counters never affect pointers, allocation sizes or allocator decisions.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(p, layout) };
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(p, layout, size) };
        if !next.is_null() {
            if size >= layout.size() {
                add(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, Relaxed);
            }
        }
        next
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;
fn measure<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    let baseline = LIVE.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    let value = f();
    (
        value,
        PEAK.load(Relaxed).saturating_sub(baseline),
        LIVE.load(Relaxed).saturating_sub(baseline),
    )
}

// One test owns the process-wide measurement window; no sibling test can
// allocate concurrently and invalidate the measured bound.
#[test]
fn decoded_memory_is_bounded_and_released_across_sizes_and_failures() -> Result<()> {
    const MIB: usize = 1024 * 1024;
    const CHUNK: usize = 64 * 1024;
    for length in [MIB, 8 * MIB, 32 * MIB] {
        let source: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
        let chunks = ChunkedBytes::from_zstd_chunks(&source, CHUNK, CompressionLevel::Fast)?;
        for _ in 0..4 {
            let (refused, peak, _) = measure(|| chunks.decode_to_vec_limited((length - 1) as u64));
            assert!(matches!(
                refused,
                Err(Error::DecompressedLengthLimitExceeded { .. })
            ));
            assert!(peak < CHUNK, "refusal allocated {peak} bytes");
            let (decoded, peak, retained) = measure(|| -> Result<()> {
                let value = chunks.decode_to_vec_limited(length as u64)?;
                assert_eq!(value, source);
                Ok(())
            });
            decoded?;
            assert!(
                peak <= length + 2 * CHUNK + CHUNK,
                "live Rust heap {peak} for {length} bytes"
            );
            assert!(retained < CHUNK, "decode retained {retained} bytes");
        }
    }
    // Individually small nested values share one cumulative decode budget.
    for endian in [Endian::Little, Endian::Big] {
        let values: std::collections::BTreeMap<u64, Vec<u8>> =
            (0..16).map(|id| (id, vec![9u8; CHUNK])).collect();
        let mut encoded = Encoder::new(endian);
        values.encode_varlit(&mut encoded)?;
        let bytes = encoded.into_inner();
        let (result, peak, retained) = measure(|| {
            Decoder::decode_from_slice_limited::<std::collections::BTreeMap<u64, Vec<u8>>>(
                &bytes,
                endian,
                8 * CHUNK as u64,
            )
        });
        assert!(
            matches!(result, Err(Error::LimitExceeded { .. })),
            "{result:?}"
        );
        assert!(
            peak <= 9 * CHUNK,
            "nested decoder escaped its shared budget: {peak}"
        );
        assert!(retained < CHUNK);
        assert_eq!(
            Decoder::decode_from_slice_limited::<std::collections::BTreeMap<u64, Vec<u8>>>(
                &bytes,
                endian,
                2 * MIB as u64
            )?,
            values
        );
    }
    println!("decoded sizes=1,8,32 MiB; repeated success/refusal; shared nested budget");
    // Opted in by the required Linux release job, where no sanitizer/compiler
    // instrumentation distorts process memory. Ordinary portable tests retain
    // the Rust-allocation contracts above.
    if std::env::var_os("VARLIT_ENFORCE_DECODE_RSS").is_some() {
        let status = std::fs::read_to_string("/proc/self/status")?;
        let kib: usize = status
            .lines()
            .find_map(|line| line.strip_prefix("VmHWM:"))
            .expect("Linux reports peak resident memory")
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        // 32 MiB source + 32 MiB decoded output, with a fixed 32 MiB allowance
        // for the executable, allocator and native codec state; no file-size
        // proportional cache is permitted in addition to the two values.
        assert!(
            kib <= 96 * 1024,
            "decode process RSS exceeded 96 MiB: {kib} KiB"
        );
        println!("DECODE_RSS peak_kib={kib} limit_kib={}", 96 * 1024);
    }
    Ok(())
}
