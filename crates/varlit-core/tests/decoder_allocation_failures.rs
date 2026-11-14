//! Fail every observed Rust allocation, then keep refusing until the operation
//! returns. BTreeMap and custom allocating codecs are deliberately not fixtures.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use varlit_core::{Decoder, Endian, Error, Result, VarlitDecode, encode_to_vec, read_field_header};

#[derive(Clone, Copy, Default, Debug)]
struct State {
    enabled: bool,
    fail_at: usize,
    calls: usize,
    denied: usize,
    live: isize,
    peak: usize,
}
thread_local! { static STATE: Cell<State> = const { Cell::new(State {
    enabled: false, fail_at: usize::MAX, calls: 0, denied: 0, live: 0, peak: 0,
}) }; }
struct Meter;
fn refuse() -> bool {
    STATE
        .try_with(|cell| {
            let mut s = cell.get();
            if !s.enabled {
                return false;
            }
            s.calls += 1;
            let denied = s.calls >= s.fail_at;
            s.denied += usize::from(denied);
            cell.set(s);
            denied
        })
        .unwrap_or(false)
}
fn account(delta: isize) {
    let _ = STATE.try_with(|cell| {
        let mut s = cell.get();
        if s.enabled {
            s.live += delta;
            s.peak = s.peak.max(s.live.max(0) as usize);
            cell.set(s);
        }
    });
}
// SAFETY: System receives unchanged pointers/layouts. Refusal returns null and
// leaves realloc's old allocation intact. Constant TLS bookkeeping never allocates.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if refuse() {
            return std::ptr::null_mut();
        }
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            account(layout.size() as isize);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if refuse() {
            return std::ptr::null_mut();
        }
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            account(layout.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        account(-(layout.size() as isize));
        unsafe { System.dealloc(p, layout) };
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if refuse() {
            return std::ptr::null_mut();
        }
        let next = unsafe { System.realloc(p, layout, size) };
        if !next.is_null() {
            account(size as isize - layout.size() as isize);
        }
        next
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;

fn measure(fail_at: usize, body: impl FnOnce() -> Result<()>) -> (Result<()>, State) {
    STATE.with(|s| {
        s.set(State {
            enabled: true,
            fail_at,
            ..State::default()
        })
    });
    // A panic fails this test; this guard disables refusal during unwinding.
    struct Disable;
    impl Drop for Disable {
        fn drop(&mut self) {
            STATE.with(|s| {
                let mut v = s.get();
                v.enabled = false;
                s.set(v);
            });
        }
    }
    let guard = Disable;
    let result = body();
    drop(guard);
    (result, STATE.with(Cell::get))
}
fn sweep(name: &str, body: impl Fn() -> Result<()>) -> usize {
    let (result, baseline) = measure(usize::MAX, &body);
    result.unwrap();
    assert!(baseline.calls > 0, "empty allocation fixture: {name}");
    assert_eq!(
        baseline.live, 0,
        "successful operation retained memory: {name}"
    );
    for fail_at in 1..=baseline.calls {
        let (result, state) = measure(fail_at, &body);
        assert!(
            matches!(result, Err(Error::AllocationFailed { .. })),
            "{name} allocation {fail_at}: {result:?}"
        );
        assert!(state.denied > 0);
        assert_eq!(state.live, 0, "{name} leaked at allocation {fail_at}");
        // The same immutable input must remain usable after every refusal.
        body().unwrap();
    }
    println!(
        "ALLOCATION_SWEEP {name} points={} peak={}",
        baseline.calls, baseline.peak
    );
    baseline.calls
}
fn typed<T: VarlitDecode>(bytes: &[u8], endian: Endian) -> Result<()> {
    Decoder::decode_from_slice_limited::<T>(bytes, endian, 16 * 1024 * 1024).map(drop)
}

#[test]
fn every_builtin_decoder_allocation_failure_returns_and_releases() {
    let mut points = 0;
    let mut fixtures = 0;
    for endian in [Endian::Little, Endian::Big] {
        macro_rules! fixture {
            ($name:literal, $ty:ty, $value:expr) => {{
                let value: $ty = $value;
                let bytes = encode_to_vec(&value, endian).unwrap();
                let decoded =
                    Decoder::decode_from_slice_limited::<$ty>(&bytes, endian, 16 * 1024 * 1024)
                        .unwrap();
                assert_eq!(decoded, value);
                points += sweep($name, || typed::<$ty>(&bytes, endian));
                fixtures += 1;
            }};
        }
        fixture!("bytes", Vec<u8>, vec![7; 65536]);
        fixture!("string", String, "z".repeat(4096));
        fixture!("sequence", Vec<u64>, (0..2048).collect());
        fixture!("nested-sequence", Vec<String>, vec!["a".repeat(4096); 8]);
        fixture!(
            "stack-string-array",
            [String; 4],
            std::array::from_fn(|i| "a".repeat(128 + i))
        );
        fixture!("heap-array", [u64; 2048], std::array::from_fn(|i| i as u64));
        fixture!(
            "nested-option-tuple",
            (Option<String>, Vec<u8>),
            (Some("a".repeat(256)), vec![1; 256])
        );
        fixture!("hash-map-growth", HashMap<u64, u64>, (0..4096).map(|i| (i, i)).collect());
        fixture!("hash-map-nested", HashMap<u64, Vec<u8>>, (0..32).map(|i| (i, vec![9; 512])).collect());
        let mut fields = Vec::new();
        for id in 64u32..160 {
            let id = match endian {
                Endian::Little => id.to_le_bytes(),
                Endian::Big => id.to_be_bytes(),
            };
            fields.extend_from_slice(&id);
            fields.extend_from_slice(&[0; 12]); // Unit type, no flags, empty payload
        }
        points += sweep("large-field-id-set", || {
            let mut decoder = Decoder::new_limited(&fields, endian, 65536);
            for _ in 64..160 {
                read_field_header(&mut decoder)?;
            }
            Ok(())
        });
        fixtures += 1;
    }
    println!("DECODER_OOM fixtures={fixtures} refusal_points={points} all_retried=true");
}

#[cfg(all(feature = "compression-zstd", feature = "integrity"))]
#[test]
fn chunk_output_allocation_failure_returns_and_retries() {
    let source = vec![7; 1024 * 1024];
    let chunks = varlit_core::ChunkedBytes::from_zstd_chunks(
        &source,
        65536,
        varlit_core::CompressionLevel::Fast,
    )
    .unwrap();
    let points = sweep("chunk-output", || {
        chunks.decode_to_vec_limited(source.len() as u64).map(drop)
    });
    assert_eq!(
        chunks.decode_to_vec_limited(source.len() as u64).unwrap(),
        source
    );
    println!("CHUNK_OOM refusal_points={points} all_retried=true");
}

#[test]
fn hash_map_rehash_peak_fits_charged_materialization() {
    for len in [1024u64, 1025, 1792, 1793, 3584, 3585, 4096] {
        let value: HashMap<u64, u64> = (0..len).map(|i| (i, i)).collect();
        let bytes = encode_to_vec(&value, Endian::Little).unwrap();
        let budget = 16 * 1024 * 1024;
        let mut charged = 0;
        let (result, state) = measure(usize::MAX, || {
            let mut decoder = Decoder::new_limited(&bytes, Endian::Little, budget);
            let value = HashMap::<u64, u64>::decode_varlit(&mut decoder)?;
            charged = budget - decoder.materialization_remaining();
            drop(value);
            Ok(())
        });
        result.unwrap();
        assert_eq!(state.live, 0);
        println!(
            "HASH_REHASH entries={len} peak={} charged={charged}",
            state.peak
        );
        assert!(
            state.peak as u64 <= charged,
            "rehash overlap exceeded the charged memory budget"
        );
        let (result, exact) = measure(usize::MAX, || {
            Decoder::decode_from_slice_limited::<HashMap<u64, u64>>(&bytes, Endian::Little, charged)
                .map(drop)
        });
        result.unwrap();
        assert!(exact.peak as u64 <= charged);
        let (result, refused) = measure(usize::MAX, || {
            Decoder::decode_from_slice_limited::<HashMap<u64, u64>>(
                &bytes,
                Endian::Little,
                charged - 1,
            )
            .map(drop)
        });
        assert!(matches!(result, Err(Error::LimitExceeded { .. })));
        assert_eq!(
            refused.calls, 0,
            "insufficient aggregate budget reached allocator"
        );
    }
}
