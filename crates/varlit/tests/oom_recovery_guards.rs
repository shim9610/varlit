//! Allocation refusal uses a synthetic allocator, never host memory exhaustion.
#![cfg(all(feature = "integrity", feature = "compression-zstd", not(miri)))]
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
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
#[derive(Debug, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable")]
struct Blob {
    #[varlit(field_id = 1)]
    bytes: Vec<u8>,
}
#[derive(Debug, VarlitBlock)]
#[varlit(id = 2, version = 1, kind = "variable")]
struct MapBlock {
    #[varlit(field_id = 1)]
    values: HashMap<u64, u64>,
}
#[derive(Debug, Clone, VarlitBlock)]
#[varlit(id = 3, version = 1, kind = "fixed")]
struct Point {
    value: u64,
}
varlit_format! { struct Format {
    magic: b"VOOMTEST"; version: 1; endian: little; schema_hash: computed;
    blocks: [Blob, MapBlock, Point];
} }

// One test owns the process-wide allocator window. An infallible regression
// aborts this dedicated test binary; other test executables are unaffected.
#[test]
fn allocations_refuse_cleanly_and_rewrite_scratch_stays_bounded() -> Result<()> {
    let source: HashMap<u64, u64> = (0..3000).map(|n| (n, n)).collect();
    let encoded = encode_to_vec(&source, Endian::Little)?;
    let (result, _) = measure(60000, 1, || {
        Decoder::decode_from_slice_limited::<HashMap<u64, u64>>(
            &encoded,
            Endian::Little,
            1024 * 1024,
        )
    });
    assert!(
        matches!(result, Err(Error::AllocationFailed { .. })),
        "HashMap growth must be fallible"
    );
    let mut fields = Vec::new();
    for id in 64..100064u32 {
        fields.extend_from_slice(&id.to_le_bytes());
        fields.extend_from_slice(&(WireType::U8 as u16).to_le_bytes());
        fields.extend_from_slice(&[0; 10]);
    }
    let budget = 16 * 1024 * 1024;
    let (charged, peak) = measure(usize::MAX, 1, || -> Result<u64> {
        let mut decoder = Decoder::new_limited(&fields, Endian::Little, budget);
        while decoder.remaining() != 0 {
            read_field_header(&mut decoder)?;
        }
        Ok(budget - decoder.materialization_remaining())
    });
    assert!(
        peak as u64 <= charged?,
        "field tracking exceeded its charged memory"
    );
    let bytes = vec![9; 65536];
    let (result, _) = measure(65536, 1, || encode_to_vec(&bytes, Endian::Little));
    assert!(
        matches!(result, Err(Error::AllocationFailed { .. })),
        "result-returning encoder must be fallible"
    );
    let (result, _) = measure(65536, 1, || {
        ChunkedBytes::from_zstd_chunks(&bytes, bytes.len(), CompressionLevel::Fast)
    });
    assert!(
        matches!(result, Err(Error::AllocationFailed { .. })),
        "compression scratch must be fallible"
    );
    let chunks = ChunkedBytes::from_zstd_chunks(&bytes, bytes.len(), CompressionLevel::Fast)?;
    // A small embedded frame cannot justify a large outer output declaration.
    let small_frame = ChunkedBytes::from_zstd_chunks(&[7; 8], 8, CompressionLevel::Fast)?;
    let mut encoded = small_frame.into_encoded();
    let claimed = 64 * 1024 * 1024u64;
    encoded[8..16].copy_from_slice(&claimed.to_le_bytes());
    encoded[16..24].copy_from_slice(&claimed.to_le_bytes());
    encoded[32..36].copy_from_slice(&(claimed as u32).to_le_bytes());
    let oversized = ChunkedBytes::from_encoded(encoded)?;
    let (refused, peak) = measure(65536, 1, || oversized.decode_to_vec());
    assert!(matches!(
        refused,
        Err(Error::DecompressedLengthMismatch { .. })
    ));
    assert!(peak < 65536, "frame mismatch allocated the declared output");

    let (result, _) = measure(65536, 1, || chunks.decode_to_vec());
    assert!(
        matches!(result, Err(Error::AllocationFailed { .. })),
        "output allocation must be fallible"
    );
    let (result, peak) = measure(65536, 2, || chunks.decode_to_vec());
    assert_eq!(result?.as_slice(), bytes.as_slice());
    assert!(
        peak <= bytes.len() + 1024,
        "decompression retained a second output buffer: {peak}"
    );
    let multiple = ChunkedBytes::from_zstd_chunks(&bytes, 4096, CompressionLevel::Fast)?;
    let (refused, _) = measure(4096, 2, || multiple.decode_to_vec());
    assert!(
        matches!(refused, Err(Error::AllocationFailed { .. })),
        "chunk scratch allocation must be fallible"
    );
    let (decoded, peak) = measure(usize::MAX, 1, || multiple.decode_to_vec());
    assert!(decoded? == bytes, "multi-chunk decoded bytes differ");
    assert!(
        peak <= bytes.len() + 4096 + 1024,
        "multi-chunk scratch grew beyond one chunk: {peak}"
    );
    let (small, peak) = measure(usize::MAX, 1, || {
        ChunkedBytes::from_zstd_chunks(&bytes, 1, CompressionLevel::Fast)
    });
    let small = small?;
    assert!(
        peak < 3 * 1024 * 1024,
        "small chunks retained per-chunk capacities: {peak}"
    );
    assert_eq!(small.decode_to_vec()?, bytes);

    // Exercise the large-output paging path without raising the output or
    // scratch budget, including an early checksum error after reservation.
    let large = vec![31; 8 * 1024 * 1024];
    let large_chunks = ChunkedBytes::from_zstd_chunks(&large, 65536, CompressionLevel::Fast)?;
    let (decoded, peak) = measure(usize::MAX, 1, || large_chunks.decode_to_vec());
    assert!(decoded? == large, "large output differs");
    assert!(
        peak <= large.len() + 65536 + 1024,
        "large output retained extra memory: {peak}"
    );
    let mut invalid_chunks = large_chunks.encoded().to_vec();
    invalid_chunks[40] ^= 1;
    let invalid_chunks = ChunkedBytes::from_encoded(invalid_chunks)?;
    assert!(matches!(
        invalid_chunks.decode_to_vec(),
        Err(Error::ChunkChecksumMismatch { .. })
    ));
    assert!(
        large_chunks.decode_to_vec()? == large,
        "decode after refusal differs"
    );
    drop(large_chunks);
    drop(invalid_chunks);
    drop(large);

    let dir = tempfile::tempdir()?;
    let header_path = dir.path().join("header.varlit");
    let spec = Format::spec()
        .with_header_slots(HeaderSlots::new(
            60 * 1024,
            HeaderSlotIntegrity::Rolling,
            &[1],
        ))
        .with_computed_schema_hash();
    {
        let mut writer = spec.create(&header_path)?;
        writer.write_header_block(&Blob {
            bytes: vec![3; 32768],
        })?;
        writer.sync()?;
    }
    let reader = spec
        .with_resource_limits(ReadLimits::STANDARD.with_max_materialized_bytes(1024))
        .open_readonly(&header_path)?;
    let (usage, peak) = measure(60000, 1, || reader.header_slots_used());
    assert!(usage? > 32768);
    assert!(peak < 1024, "checksum copied the whole header: {peak}");
    assert!(matches!(
        reader.read_header_block::<Blob>(),
        Err(Error::LimitExceeded { .. })
    ));
    drop(reader);
    let mut writer = spec.open(&header_path)?;
    let old = writer.read_header_block::<Blob>()?.unwrap().bytes;
    let (refused, _) = measure(60000, 1, || {
        writer.write_header_block(&Blob { bytes: Vec::new() })
    });
    assert!(matches!(refused, Err(Error::AllocationFailed { .. })));
    assert_eq!(writer.read_header_block::<Blob>()?.unwrap().bytes, old);
    drop(writer);

    let compressed_path = dir.path().join("compressed.varlit");
    let spec = Format::spec()
        .with_compression_policy(CompressionPolicy::VariableBlocks(
            VariableCompression::zstd(CompressionHeaderMode::RecordExplicit),
        ))
        .with_computed_schema_hash();
    {
        let mut writer =
            VarlitStreamWriter::create(spec, &compressed_path, StreamOptions::default())?;
        writer.push_info(&Blob {
            bytes: bytes.clone(),
        })?;
        writer.sync()?;
    }
    let reader = VarlitStreamReader::open(spec, &compressed_path, StreamOptions::default())?;
    let (result, _) = measure(65536, 1, || reader.blocks::<Blob>()?.next().unwrap());
    assert!(
        matches!(result, Err(Error::AllocationFailed { .. })),
        "native decompression must be fallible"
    );

    let spec = Format::spec().with_resource_limits(
        ReadLimits::STANDARD
            .with_max_logical_payload_len(1024)
            .with_max_record_payload_len(1024),
    );
    let mut writer = VarlitStreamWriter::create(
        spec,
        dir.path().join("map.varlit"),
        StreamOptions::default(),
    )?;
    let map = MapBlock {
        values: (0..100000).map(|n| (n, n)).collect(),
    };
    let (result, peak) = measure(usize::MAX, 1, || writer.push_info(&map));
    assert!(matches!(result, Err(Error::LimitExceeded { .. })));
    assert!(
        peak < 1024,
        "oversized map allocated sorting scratch: {peak}"
    );
    writer.push_info(&Point { value: 7 })?;
    writer.sync()?;

    // Same budget at both cardinalities; exercise actual spills, not a larger
    // limit for the larger file. Held readers retain the old generation.
    for count in [1000, 20000] {
        let path = dir.path().join(format!("rewrite-{count}.varlit"));
        let spec = Format::spec()
            .with_commit_policy(CommitPolicy::RecordFooter)
            .with_liveness_policy(LivenessPolicy::FooterFlags)
            .with_index_policy(IndexPolicy::BlockOffsetChain)
            .with_computed_schema_hash()
            .with_resource_limits(ReadLimits::STANDARD.with_max_index_bytes(32768));
        let first;
        {
            let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
            first = writer.push_info(&Point { value: 0 })?.record_offset;
            for n in 1..count {
                writer.push_info(&Point { value: n })?;
            }
            writer.sync()?;
        }
        let mut writer = spec.open(&path)?;
        let held = spec.open_readonly(&path)?;
        let (refused, _) = measure(1024, 1, || writer.replace_block(0, &Point { value: 99 }));
        assert!(
            matches!(refused, Err(Error::AllocationFailed { .. }))
                || matches!(refused, Err(Error::Io(ref error)) if error.kind() == std::io::ErrorKind::OutOfMemory),
            "rewrite scratch refusal must be reported as an allocation error"
        );
        assert!(held.is_current()?);
        assert_eq!(writer.read_block_at::<Point>(first)?.value, 0);
        assert_eq!(
            spec.open_readonly(&path)?
                .read_block_at::<Point>(first)?
                .value,
            0
        );
        let (result, peak) = measure(usize::MAX, 1, || {
            writer.replace_block(0, &Point { value: 42 })
        });
        result?;
        assert!(
            peak < 256 * 1024,
            "rewrite scratch grew with cardinality {count}: {peak}"
        );
        assert_eq!(held.read_block_at::<Point>(first)?.value, 0);
        assert_eq!(writer.read_block_at::<Point>(first)?.value, 42);
        let appended = writer.push_info(&Point { value: 777 })?.record_offset;
        writer.sync()?;
        assert_eq!(writer.read_block_at::<Point>(appended)?.value, 777);
        let (result, peak) = measure(usize::MAX, 1, || writer.defragment());
        result?;
        assert!(
            peak < 256 * 1024,
            "defrag scratch grew with cardinality {count}: {peak}"
        );
        drop(writer);
        assert_eq!(
            spec.open_readonly(&path)?
                .read_block_at::<Point>(first)?
                .value,
            42
        );
        assert_eq!(
            spec.open_readonly(&path)?
                .read_block_at::<Point>(appended)?
                .value,
            777
        );
    }
    println!(
        "OOM_GUARDS allocation refusal, header limits, chunk scratch and bounded rewrites passed"
    );
    Ok(())
}

// Interposing GNU malloc conflicts with sanitizer allocator ownership. The
// required native storage-contracts job runs this exact ignored selection;
// ASan still runs the Rust-allocation guard and ordinary compression tests.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
#[ignore = "GNU malloc interposition; executed by required storage-contracts"]
fn zstd_context_allocation_failures_return_errors() -> Result<()> {
    if std::env::var_os("VARLIT_ZSTD_REFUSAL_FILE").is_some() {
        zstd_context_refusal_child()?;
    } else {
        zstd_context_refusal_subprocess()?;
    }
    println!("C_CONTEXT_REFUSAL compression and decompression passed");
    Ok(())
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn zstd_context_refusal_child() -> Result<()> {
    let flag = std::path::PathBuf::from(std::env::var_os("VARLIT_ZSTD_REFUSAL_FILE").unwrap());
    let bytes = vec![9; 65536];
    let encoded = ChunkedBytes::from_zstd_chunks(&bytes, bytes.len(), CompressionLevel::Fast)?;
    // The 48-byte table is below the hook's threshold. Refuse the first
    // large malloc, which creates the C compression context.
    std::fs::write(&flag, b"1")?;
    let result = ChunkedBytes::from_zstd_chunks(&bytes, bytes.len(), CompressionLevel::Fast);
    assert!(
        matches!(result, Err(Error::Io(ref error)) if error.kind() == std::io::ErrorKind::OutOfMemory)
    );
    assert!(
        !flag.exists(),
        "compression context refusal did not execute"
    );
    // Context and Rust compression scratch succeed; zstd's internal working
    // allocation then returns its memory-allocation error code.
    std::fs::write(&flag, b"3")?;
    let result = ChunkedBytes::from_zstd_chunks(&bytes, bytes.len(), CompressionLevel::Fast);
    assert!(
        matches!(result, Err(Error::Io(ref error)) if error.kind() == std::io::ErrorKind::OutOfMemory)
    );
    assert!(
        !flag.exists(),
        "compression workspace refusal did not execute"
    );
    // Permit the final Rust output, then refuse the C decompression context.
    std::fs::write(&flag, b"2")?;
    let result = encoded.decode_to_vec();
    assert!(
        matches!(result, Err(Error::Io(ref error)) if error.kind() == std::io::ErrorKind::OutOfMemory)
    );
    assert!(
        !flag.exists(),
        "decompression context refusal did not execute"
    );
    Ok(())
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn zstd_context_refusal_subprocess() -> Result<()> {
    use std::process::Command;
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("refuse.c");
    let library = directory.path().join("refuse.so");
    std::fs::write(&source, include_str!("support/zstd_malloc_refusal.c"))?;
    let compiler = Command::new("cc")
        .args(["-shared", "-fPIC", "-O2"])
        .arg(&source)
        .arg("-o")
        .arg(&library)
        .output()?;
    assert!(
        compiler.status.success(),
        "failed to build C allocator refusal hook"
    );
    let child = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "zstd_context_allocation_failures_return_errors",
            "--test-threads=1",
            "--ignored",
            "--nocapture",
        ])
        .env("LD_PRELOAD", &library)
        .env("VARLIT_ZSTD_REFUSAL_FILE", directory.path().join("armed"))
        .output()?;
    assert!(
        child.status.success()
            && String::from_utf8_lossy(&child.stdout).contains("C_CONTEXT_REFUSAL"),
        "C allocator refusal child did not execute successfully: {}",
        child.status
    );
    Ok(())
}
