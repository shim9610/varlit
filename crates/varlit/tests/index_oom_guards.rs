//! Every allocation in a public tree lookup is refused in turn.
#![cfg(all(feature = "integrity", not(miri)))]
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use varlit::*;
struct Meter;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static DENY_GE: AtomicUsize = AtomicUsize::new(usize::MAX);
static DENY_NTH: AtomicUsize = AtomicUsize::new(1);
static MATCHES: AtomicUsize = AtomicUsize::new(0);
fn denied(size: usize) -> bool {
    size >= DENY_GE.load(Relaxed) && MATCHES.fetch_add(1, Relaxed) + 1 >= DENY_NTH.load(Relaxed)
}
fn add(size: usize) {
    let live = LIVE.fetch_add(size, Relaxed) + size;
    PEAK.fetch_max(live, Relaxed);
}
// SAFETY: successful requests use System with unchanged layouts. Refusal returns
// null as GlobalAlloc requires; bookkeeping uses atomics and never allocates.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if denied(layout.size()) {
            return std::ptr::null_mut();
        }
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            add(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if denied(layout.size()) {
            return std::ptr::null_mut();
        }
        let p = unsafe { System.alloc_zeroed(layout) };
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
        if denied(size) {
            return std::ptr::null_mut();
        }
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
struct Window;
impl Drop for Window {
    fn drop(&mut self) {
        DENY_GE.store(usize::MAX, Relaxed);
    }
}
fn measure<T>(threshold: usize, nth: usize, op: impl FnOnce() -> T) -> (T, usize) {
    let baseline = LIVE.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    MATCHES.store(0, Relaxed);
    DENY_NTH.store(nth, Relaxed);
    DENY_GE.store(threshold, Relaxed);
    let guard = Window;
    let result = op();
    drop(guard);
    (result, PEAK.load(Relaxed).saturating_sub(baseline))
}

varlit_format! {
    pub format OomIndex {
        magic:b"VIXOOM01";version:1;schema_hash:computed;index:keyed_offset_chain;integrity:crc32;
        blocks {
            fixed Number(id=1,key=[id],key_index=disk) {id:u64,value:u64}
            variable Text(id=2,key=[id],key_index=disk) {id:String,value:u64}
        }
    }
}
fn allocation_error<T>(result: &Result<T>) -> bool {
    matches!(result, Err(Error::AllocationFailed { .. }))
        || matches!(result, Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::OutOfMemory)
}
// A single test owns the allocator window; no sibling can race it.
#[test]
fn public_index_reads_return_oom_and_remain_usable_after_each_refusal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("index.varlit");
    let options = DiskIndexOptions::default();
    let mut writer = OomIndex::create_indexed_writer(&path, options)?;
    writer
        .push_numbers(
            (0..3000).map(|id| Number { id, value: id }),
            BatchOptions::default(),
        )
        .map_err(|e| e.source)?;
    let prefix = "x".repeat(256);
    writer
        .push_texts(
            (0..100).map(|id| Text {
                id: format!("{prefix}-{id:04}"),
                value: id,
            }),
            BatchOptions::default(),
        )
        .map_err(|e| e.source)?;
    writer
        .push_texts(
            (0..100).map(|id| Text {
                id: format!("{id:03}-{prefix}"),
                value: id,
            }),
            BatchOptions::default(),
        )
        .map_err(|e| e.source)?;
    writer.sync()?;
    let common = format!("{prefix}-0050");
    let distinct = format!("050-{prefix}");
    let mut total = 0;
    for case in 0..3 {
        let long = case != 0;
        let key = if case == 2 { &distinct } else { &common };
        let read = OomIndex::open_indexed_reader(&path, options)?;
        let (result, _) = measure(1, usize::MAX, || -> Result<u64> {
            if long {
                Ok(read.get_text(key)?.unwrap().value)
            } else {
                Ok(read.get_number(&1500)?.unwrap().value)
            }
        });
        assert_eq!(result?, if long { 50 } else { 1500 });
        let allocations = MATCHES.load(Relaxed);
        assert!(
            allocations > 2,
            "cold lookup did not exercise node/cache growth"
        );
        for nth in 1..=allocations {
            let read = OomIndex::open_indexed_reader(&path, options)?;
            let (result, _) = measure(1, nth, || -> Result<u64> {
                if long {
                    Ok(read.get_text(key)?.unwrap().value)
                } else {
                    Ok(read.get_number(&1500)?.unwrap().value)
                }
            });
            assert!(
                allocation_error(&result),
                "lookup did not return an allocation error at {nth}/{allocations}, long={long}"
            );
            let retried = if long {
                read.get_text(key)?.unwrap().value
            } else {
                read.get_number(&1500)?.unwrap().value
            };
            assert_eq!(retried, if long { 50 } else { 1500 });
            total += 1;
        }
    }
    println!("INDEX_OOM every cold lookup allocation refused and retried; trials={total}");
    // Two equal-sized batches establish reuse; a one-off large batch is not
    // retained. Then refuse any further chunk-sized allocation, including
    // after an intervening single put, without relying on in-place growth.
    let batch_prefix = prefix.as_str();
    let records = |extra: u64| {
        (0..5000u64).map(move |id| Text {
            id: format!("batch-{batch_prefix}-{id:04}"),
            value: id + extra,
        })
    };
    writer
        .push_texts(records(0), BatchOptions::default())
        .map_err(|e| e.source)?;
    writer
        .push_texts(records(0), BatchOptions::default())
        .map_err(|e| e.source)?;
    writer.push_number(&Number { id: 4000, value: 8 })?;
    let (reused, _) = measure(1024 * 1024, 1, || {
        writer.push_texts(records(1), BatchOptions::default())
    });
    let large_allocations = MATCHES.load(Relaxed);
    reused.map_err(|e| e.source)?;
    assert_eq!(large_allocations, 0, "batch staging was allocated again");
    writer.sync()?;
    let read = OomIndex::open_indexed_reader(&path, options)?;
    assert_eq!(
        read.get_text(&format!("batch-{prefix}-0500"))?
            .unwrap()
            .value,
        501
    );
    assert_eq!(read.get_number(&4000)?.unwrap().value, 8);
    println!("INDEX_BATCH_REUSE repeated batch and single put share staging");
    scan_tail_allocation_refusal_returns_an_error()?;
    Ok(())
}

varlit_format! {
    pub struct ScanOom {
        magic: b"VSCANOM1";
        version: 1;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks: [Number];
    }
}
fn scan_tail_allocation_refusal_returns_an_error() -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("scan.varlit");
    let spec = ScanOom::spec();
    let mut writer = spec.create(&path)?;
    let mut offsets = Vec::new();
    for id in 0..512 {
        offsets.push(writer.push_info(&Number { id, value: id })?.record_offset);
    }
    writer.flush()?;
    drop(writer);
    let mut file = std::fs::OpenOptions::new().write(true).open(&path)?;
    for (ordinal, offset) in offsets.into_iter().enumerate() {
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&(1000 + ordinal as u32).to_le_bytes())?;
    }
    drop(file);
    let (opened, _) = measure(4096, 1, || VarlitFile::open_readonly(spec, &path));
    assert!(
        allocation_error(&opened),
        "scan tail allocation did not return an error"
    );
    let limited = spec.with_read_limits(spec.read_limits.with_max_materialized_bytes(770));
    assert!(matches!(
        VarlitFile::open_readonly(limited, &path),
        Err(Error::LimitExceeded { .. })
    ));
    let opened = VarlitFile::open_readonly(spec, &path)?;
    assert!(opened.block_tail_offset(1511).is_some());
    Ok(())
}
