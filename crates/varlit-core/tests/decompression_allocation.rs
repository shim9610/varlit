//! Refuse large requests; this is not an OOM-kill experiment.
#![cfg(feature = "compression-zstd")]
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use varlit_core::{CompressionLevel, Error, Result};
#[allow(dead_code)]
#[path = "../src/compression_buffers.rs"]
mod compression_buffers;
struct Meter;
static ENABLED: AtomicBool = AtomicBool::new(false);
static DENIED: AtomicUsize = AtomicUsize::new(0);
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
const MAXIMUM: usize = 4 * 1024 * 1024;
fn refuse(n: usize) -> bool {
    if ENABLED.load(Relaxed) && n >= MAXIMUM {
        DENIED.fetch_add(1, Relaxed);
        true
    } else {
        false
    }
}
// SAFETY: unchanged System layouts; refusal returns null, without allocating.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if refuse(l.size()) {
            std::ptr::null_mut()
        } else {
            unsafe { System.alloc(l) }
        }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if refuse(n) {
            std::ptr::null_mut()
        } else {
            unsafe { System.realloc(p, l, n) }
        }
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;
#[test]
fn untrusted_sizes_do_not_request_the_declared_maximum() {
    let _lock = LOCK.lock().unwrap();
    let inflated = [0x28, 0xb5, 0x2f, 0xfd, 0xa0, 0, 0, 0x40, 0, 9, 0, 0, 7];
    for input in [inflated.as_slice(), b"invalid"] {
        DENIED.store(0, Relaxed);
        ENABLED.store(true, Relaxed);
        let result = compression_buffers::decompress(input, MAXIMUM);
        ENABLED.store(false, Relaxed);
        assert!(result.is_err());
        let denied = DENIED.load(Relaxed);
        println!("input_bytes={} denied_large_requests={denied}", input.len());
        if std::env::var_os("VARLIT_BASELINE_PROBE").is_some() {
            assert!(denied > 0);
        } else {
            assert_eq!(denied, 0, "invalid input requested the declared output");
        }
    }
}

#[cfg(feature = "integrity")]
#[test]
fn chunk_headers_do_not_authorize_container_allocation() {
    let _lock = LOCK.lock().unwrap();
    // One malformed frame claims 4 MiB both in zstd and in the container.
    let frame = [0x28, 0xb5, 0x2f, 0xfd, 0xa0, 0, 0, 0x40, 0, 9, 0, 0, 7];
    let mut bytes = vec![0; 48];
    bytes[..8].copy_from_slice(b"VCHK\x01\x00\x01\x00");
    bytes[8..16].copy_from_slice(&(MAXIMUM as u64).to_le_bytes());
    bytes[16..24].copy_from_slice(&(MAXIMUM as u64).to_le_bytes());
    bytes[24..28].copy_from_slice(&1u32.to_le_bytes());
    bytes[32..36].copy_from_slice(&(MAXIMUM as u32).to_le_bytes());
    bytes[36..40].copy_from_slice(&(frame.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&frame);
    let chunks = varlit_core::ChunkedBytes::from_encoded(bytes).unwrap();
    DENIED.store(0, Relaxed);
    ENABLED.store(true, Relaxed);
    let result = chunks.decode_to_vec_limited(MAXIMUM as u64);
    ENABLED.store(false, Relaxed);
    assert!(result.is_err());
    assert_eq!(DENIED.load(Relaxed), 0);
}

#[test]
fn bounded_decode_preserves_large_unknown_concatenated_and_empty_frames() -> Result<()> {
    let _lock = LOCK.lock().unwrap();
    let data: Vec<_> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let mut encoder = zstd::bulk::Compressor::new(1)?;
    for known in [true, false] {
        encoder.include_contentsize(known)?;
        let frame = encoder.compress(&data)?;
        assert_eq!(compression_buffers::decompress(&frame, data.len())?, data);
        assert!(compression_buffers::decompress(&frame, data.len() - 1).is_err());
        for n in [0, 1, frame.len() - 1] {
            assert!(compression_buffers::decompress(&frame[..n], data.len()).is_err() || n == 0);
        }
        let mut both = frame.clone();
        both.extend_from_slice(&frame);
        let decoded = compression_buffers::decompress(&both, 2 * data.len())?;
        assert_eq!(decoded[..data.len()], data);
        assert_eq!(decoded[data.len()..], data);
        let empty = encoder.compress(&[])?;
        assert!(compression_buffers::decompress(&empty, 0)?.is_empty());
    }
    Ok(())
}

#[test]
fn upstream_legacy_and_current_frames_remain_compatible() -> Result<()> {
    let _lock = LOCK.lock().unwrap();
    // Extracted unchanged from facebook/zstd v1.5.7 tests/legacy.c, BSD license
    // retained in fixtures/zstd-LICENSE. Includes concatenated legacy + current frames.
    let encoded = include_bytes!("fixtures/zstd-legacy.bin");
    let expected = include_bytes!("fixtures/zstd-legacy.txt");
    assert_eq!(
        compression_buffers::decompress(encoded, expected.len())?,
        expected
    );
    assert!(compression_buffers::decompress(encoded, expected.len() - 1).is_err());
    Ok(())
}
