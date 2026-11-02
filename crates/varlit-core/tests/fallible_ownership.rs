//! Refuse every allocation point, retain refusal through cleanup, and retry.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use varlit_core::{Error, Result, TryClone};

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

#[test]
fn fallible_clones_release_partial_output_and_preserve_sources() {
    let nested = vec![vec![String::from("alpha"), String::from("beta")]; 4];
    let map: HashMap<_, _> = [
        (String::from("first"), nested.clone()),
        (String::from("second"), nested.clone()),
    ]
    .into();
    let path = std::path::PathBuf::from("a-directory/a-file");
    let layout = varlit_core::LayoutValue::Bytes(vec![19; 4096]);
    let dims = varlit_core::MatrixDimensions::from_pairs([("scan", 17), ("ch", 31)]);
    let mut points = 0;
    macro_rules! trial {
        ($name:literal, $source:expr) => {{
            let source = &$source;
            let saved = source.clone();
            points += sweep($name, || {
                let cloned = source.try_clone()?;
                assert_eq!(&cloned, source);
                Ok(())
            });
            assert_eq!(source, &saved);
        }};
    }
    trial!("nested", nested);
    trial!("map", map);
    trial!("path", path);
    trial!("layout", layout);
    trial!("dimensions", dims);
    let shared = std::sync::Arc::new(vec![1; 4096]);
    let (result, state) = measure(1, || {
        let copy = shared.try_clone()?;
        assert!(std::sync::Arc::ptr_eq(&shared, &copy));
        Ok(())
    });
    result.unwrap();
    assert_eq!(state.calls, 0);
    println!("TRY_CLONE refusal_points={points} partial_cleanup=true source_unchanged=true");
}

#[test]
fn boxes_preserve_alignment_drop_once_and_clean_up_partial_clones() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Counted<'a>(&'a AtomicUsize);
    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    #[repr(align(256))]
    struct Aligned([u8; 257]);
    let aligned = varlit_core::try_box(Aligned([9; 257])).unwrap();
    assert_eq!((&*aligned as *const Aligned as usize) % 256, 0);
    assert_eq!(aligned.0, [9; 257]);
    let drops = AtomicUsize::new(0);
    let (result, state) = measure(1, || varlit_core::try_box(Counted(&drops)).map(drop));
    assert!(matches!(result, Err(Error::AllocationFailed { .. })));
    assert_eq!(state.denied, 1);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    varlit_core::try_box(Counted(&drops)).map(drop).unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    #[repr(align(256))]
    struct Zero;
    let (result, state) = measure(1, || {
        let zero = varlit_core::try_box(Zero)?;
        assert_eq!((&*zero as *const Zero as usize) % 256, 0);
        Ok(())
    });
    result.unwrap();
    assert_eq!(state.calls, 0);
    let source = Box::new(vec![String::from("one"), String::from("two")]);
    sweep("boxed nested clone", || {
        let copy = source.try_clone()?;
        assert_eq!(copy, source);
        Ok(())
    });
}

#[test]
fn cancellation_token_refusal_and_retry() {
    let (result, state) = measure(1, || {
        varlit_core::ScanCancellationToken::try_new().map(drop)
    });
    assert!(matches!(result, Err(Error::Io(ref e)) if e.kind() == std::io::ErrorKind::OutOfMemory));
    assert_eq!(state.denied, 1);
    let token = varlit_core::ScanCancellationToken::try_new().unwrap();
    assert!(!token.is_cancelled());
    let (result, state) = measure(1, || {
        token.clone().cancel();
        Ok(())
    });
    result.unwrap();
    assert_eq!(state.calls, 0);
    assert!(token.is_cancelled());
}
