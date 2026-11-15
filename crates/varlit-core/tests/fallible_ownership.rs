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

#[test]
fn reserved_boxes_skip_initialization_on_refusal_and_release_failed_initializers() {
    let runs = Cell::new(0);
    let (result, state) = measure(1, || {
        let _ = varlit_core::try_box_with(|| {
            runs.set(runs.get() + 1);
            Ok([19u8; 64])
        })?;
        Ok(())
    });
    assert!(matches!(result, Err(Error::AllocationFailed { .. })));
    assert_eq!(runs.get(), 0);
    assert_eq!(state.live, 0);
    let (result, state) = measure(usize::MAX, || {
        let _ = varlit_core::try_box_with::<[u8; 64]>(|| {
            runs.set(runs.get() + 1);
            Err(Error::InvalidMatrixLayout)
        })?;
        Ok(())
    });
    assert!(matches!(result, Err(Error::InvalidMatrixLayout)));
    assert_eq!(runs.get(), 1);
    assert_eq!(state.live, 0);
    assert_eq!(*varlit_core::try_box_with(|| Ok(17u64)).unwrap(), 17);
}

#[test]
fn boxed_clone_reserves_owner_before_cloning_payload() {
    struct Probe<'a>(&'a Cell<usize>);
    impl TryClone for Probe<'_> {
        fn try_clone(&self) -> Result<Self> {
            self.0.set(self.0.get() + 1);
            Ok(Self(self.0))
        }
    }
    let calls = Cell::new(0);
    let source = Box::new(Probe(&calls));
    let (result, state) = measure(1, || {
        let _ = source.try_clone()?;
        Ok(())
    });
    assert!(matches!(result, Err(Error::AllocationFailed { .. })));
    assert_eq!(calls.get(), 0);
    assert_eq!(state.live, 0);
    drop(source.try_clone().unwrap());
    assert_eq!(calls.get(), 1);
}

#[test]
fn uninitialized_large_box_can_be_filled_on_a_small_stack() {
    const LENGTH: usize = 16 * 1024 * 1024;
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(|| {
            let mut storage = varlit_core::try_box_uninit::<[u8; LENGTH]>().unwrap();
            // SAFETY: the allocation holds LENGTH bytes; u8 has no invalid bit
            // pattern. Every array element is initialized before assume_init.
            unsafe {
                storage.as_mut_ptr().cast::<u8>().write_bytes(23, LENGTH);
            }
            let owner = unsafe { storage.assume_init() };
            assert_eq!(owner[0], 23);
            assert_eq!(owner[LENGTH - 1], 23);
        })
        .unwrap()
        .join()
        .unwrap();
    let (result, state) = measure(1, || {
        let _ = varlit_core::try_box_uninit::<[u8; LENGTH]>()?;
        Ok(())
    });
    assert!(matches!(result, Err(Error::AllocationFailed { .. })));
    assert_eq!(state.live, 0);
}

#[test]
fn callback_errors_clean_partial_deque_clones_and_allow_retry() {
    use std::collections::VecDeque;
    struct Item<'a> {
        value: usize,
        calls: &'a Cell<usize>,
        fail_at: &'a Cell<usize>,
        live_clones: &'a Cell<usize>,
        cloned: bool,
    }
    impl TryClone for Item<'_> {
        fn try_clone(&self) -> Result<Self> {
            let call = self.calls.get() + 1;
            self.calls.set(call);
            if call == self.fail_at.get() {
                return Err(Error::InvalidMatrixLayout);
            }
            self.live_clones.set(self.live_clones.get() + 1);
            Ok(Self {
                cloned: true,
                ..*self
            })
        }
    }
    impl Drop for Item<'_> {
        fn drop(&mut self) {
            if self.cloned {
                self.live_clones.set(self.live_clones.get() - 1);
            }
        }
    }
    let calls = Cell::new(0);
    let fail_at = Cell::new(usize::MAX);
    let live_clones = Cell::new(0);
    let mut source: VecDeque<_> = (0..8)
        .map(|value| Item {
            value,
            calls: &calls,
            fail_at: &fail_at,
            live_clones: &live_clones,
            cloned: false,
        })
        .collect();
    // Exercise a rotated deque, including its logical order after a failure.
    source.rotate_left(3);
    for refusal in 1..=source.len() {
        calls.set(0);
        fail_at.set(refusal);
        let (result, state) = measure(usize::MAX, || source.try_clone().map(drop));
        assert!(matches!(result, Err(Error::InvalidMatrixLayout)));
        assert_eq!(calls.get(), refusal);
        assert_eq!(live_clones.get(), 0);
        assert_eq!(state.live, 0);
        assert_eq!(state.denied, 0);
        assert!(
            source
                .iter()
                .map(|item| item.value)
                .eq([3, 4, 5, 6, 7, 0, 1, 2])
        );
        fail_at.set(usize::MAX);
        calls.set(0);
        let copy = source.try_clone().unwrap();
        assert!(
            copy.iter()
                .map(|item| item.value)
                .eq(source.iter().map(|item| item.value))
        );
        assert_eq!(live_clones.get(), source.len());
        drop(copy);
        assert_eq!(live_clones.get(), 0);
    }
    fail_at.set(usize::MAX);
    sweep("rotated deque", || source.try_clone().map(drop));
    assert_eq!(live_clones.get(), 0);
}

#[test]
#[allow(clippy::mutable_key_type)] // The counter tracks ownership; Eq/Hash never read it.
fn map_value_clone_error_drops_the_already_cloned_key() {
    use std::hash::{Hash, Hasher};
    struct Key<'a>(&'a Cell<usize>);
    impl PartialEq for Key<'_> {
        fn eq(&self, _: &Self) -> bool {
            true
        }
    }
    impl Eq for Key<'_> {}
    impl Hash for Key<'_> {
        fn hash<H: Hasher>(&self, state: &mut H) {
            0u8.hash(state);
        }
    }
    impl TryClone for Key<'_> {
        fn try_clone(&self) -> Result<Self> {
            self.0.set(self.0.get() + 1);
            Ok(Self(self.0))
        }
    }
    impl Drop for Key<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() - 1);
        }
    }
    struct Value<'a>(&'a Cell<bool>);
    impl TryClone for Value<'_> {
        fn try_clone(&self) -> Result<Self> {
            if self.0.get() {
                Err(Error::InvalidMatrixLayout)
            } else {
                Ok(Self(self.0))
            }
        }
    }
    let owners = Cell::new(1);
    let reject = Cell::new(true);
    let source: HashMap<_, _> = [(Key(&owners), Value(&reject))].into();
    for _ in 0..3 {
        let (result, state) = measure(usize::MAX, || source.try_clone().map(drop));
        assert!(matches!(result, Err(Error::InvalidMatrixLayout)));
        assert_eq!(owners.get(), 1);
        assert_eq!(state.live, 0);
        assert_eq!(source.len(), 1);
        reject.set(false);
        let copy = source.try_clone().unwrap();
        assert_eq!(owners.get(), 2);
        drop(copy);
        assert_eq!(owners.get(), 1);
        reject.set(true);
    }
    drop(source);
    assert_eq!(owners.get(), 0);
}
