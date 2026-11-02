//! A late allocation refusal must not turn a committed operation into a retryable refusal.
#![cfg(not(miri))]
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use varlit::*;

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

varlit_format! {
    pub format GeneratedMatrixFormat {
        magic: b"GMTX";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;
        dims {
            scan: u32,
            ch: u32,
        }
        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
            singles = [master_grid];
            per_channel = [threshold];
        };
        aux {
            thumbnail: 16,
        }
        blocks {
            matrix GeneratedCell(id = 300, dims = [scan, ch], category = analysis) {
                value: u32,
            }
        }
    }
}

#[test]
fn failed_error_boxes_preserve_the_committed_matrix_event() -> Result<()> {
    for fail_at in [1, 2] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("published");
        let spec = GeneratedMatrixFormat::spec();
        let mut writer = spec.create_writer_with_dims(
            &path,
            MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]),
        )?;
        let key = MatrixKey::new(1, 1);
        let (result, state) = measure(usize::MAX, || {
            writer.write_matrix_cell_durable(key, &GeneratedCell { value: 57 }, |_| {
                STATE.with(|s| {
                    s.set(State {
                        enabled: true,
                        fail_at,
                        ..State::default()
                    })
                });
                Err(Error::InvalidFormatSpec("injected notification failure"))
            })
        });
        match result {
            Err(Error::MatrixCommittedButHookFailed { event, .. }) => {
                assert_eq!(event.key, key);
                assert_eq!(event.block_id, 300);
                assert_eq!(event.slot_len, 4);
            }
            other => panic!("publication fact lost after box refusal: {other:?}"),
        }
        assert_eq!(
            state.calls, 0,
            "late error construction allocated after commit"
        );
        assert_eq!(state.denied, 0);
        drop(writer);
        let reader = spec.open_reader(&path)?;
        assert_eq!(reader.read_matrix_cell::<GeneratedCell>(key)?.value, 57);
        assert_eq!(
            reader.matrix_cell_status::<GeneratedCell>(key)?,
            MatrixCellStatus::Committed
        );
    }
    Ok(())
}

#[test]
fn error_reservation_refusal_happens_before_matrix_mutation() -> Result<()> {
    for fail_at in [1, 2] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("preflight");
        let spec = GeneratedMatrixFormat::spec();
        let mut writer = spec.create_writer_with_dims(
            &path,
            MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]),
        )?;
        let key = MatrixKey::new(1, 1);
        let (result, state) = measure(fail_at, || {
            writer.write_matrix_cell_durable(key, &GeneratedCell { value: 57 }, |_| {
                panic!("hook ran after preflight refusal")
            })
        });
        assert!(matches!(result, Err(Error::AllocationFailed { .. })));
        assert!(state.denied > 0);
        assert_eq!(
            writer.matrix_cell_status::<GeneratedCell>(key)?,
            MatrixCellStatus::NotCommitted
        );
        writer.write_matrix_cell_durable(key, &GeneratedCell { value: 57 }, |_| Ok(()))?;
        assert_eq!(writer.read_matrix_cell::<GeneratedCell>(key)?.value, 57);
    }
    Ok(())
}

#[test]
#[cfg(feature = "mmap")]
fn mmap_layout_clone_refusal_preserves_the_reader_and_retries() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("immutable");
    let spec = GeneratedMatrixFormat::spec();
    let mut writer = spec.create_writer_with_dims(
        &path,
        MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]),
    )?;
    let key = MatrixKey::new(1, 1);
    writer.write_matrix_cell_durable(key, &GeneratedCell { value: 57 }, |_| Ok(()))?;
    drop(writer);
    let reader = spec.open_reader(&path)?;
    assert_eq!(reader.read_matrix_cell::<GeneratedCell>(key)?.value, 57);
    let clone = || -> Result<()> {
        // SAFETY: this test owns the directory; the only writer was closed.
        // Nothing mutates/truncates the file while these mappings exist.
        let mapped = unsafe { reader.mmap_matrix()? };
        assert_eq!(mapped.cell_numeric::<GeneratedCell, u32>(key)?, 57);
        Ok(())
    };
    let (result, baseline) = measure(usize::MAX, clone);
    result?;
    assert!(baseline.calls > 0);
    assert_eq!(baseline.live, 0);
    for at in 1..=baseline.calls {
        let (result, state) = measure(at, clone);
        assert!(
            matches!(
                result,
                Err(Error::AllocationFailed { .. }) | Err(Error::Io(_))
            ),
            "allocation {at}: {result:?}"
        );
        assert!(state.denied > 0);
        assert_eq!(state.live, 0, "partial clone leaked at {at}");
        assert_eq!(reader.read_matrix_cell::<GeneratedCell>(key)?.value, 57);
        clone()?;
    }
    println!(
        "MMAP_CLONE refusal_points={} original_readable=true all_retried=true",
        baseline.calls
    );
    Ok(())
}

varlit_format! {
    pub format OwnershipHeader {
        magic: b"OWHD";
        version: 1;
        preset: none;
        layout {
            file_header Header { bytes magic = b"OWHD"; u16 kind; }
            segment Data repeat until_eof {
                lead_in Lead { bytes magic = b"DATA";
                    i64 next = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw = finalize(target = raw_region_start, relative_to = after_lead_in);
                }
                metadata Meta;
                raw_region Raw;
            }
        }
    }
}
#[test]
fn generated_layout_fields_refuse_before_creating_the_file() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("header");
    let (result, state) = measure(1, || {
        OwnershipHeader::create_layout_writer_with_typed_header(
            &path,
            OwnershipHeaderHeaderLayoutFields { kind: 42 },
        )
        .map(drop)
    });
    assert!(matches!(
        result,
        Err(Error::AllocationFailed {
            resource: "generated layout fields",
            ..
        })
    ));
    assert_eq!(state.denied, 1);
    assert_eq!(state.live, 0);
    assert!(!path.exists());
    OwnershipHeader::create_layout_writer_with_typed_header(
        &path,
        OwnershipHeaderHeaderLayoutFields { kind: 42 },
    )?;
    assert!(path.exists());
    Ok(())
}
