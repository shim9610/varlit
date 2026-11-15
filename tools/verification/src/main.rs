//! Run with RUSTFLAGS="--cfg varlit_loom" cargo run --locked.
#![allow(dead_code)]

#[cfg(not(varlit_loom))]
compile_error!("Use --cfg varlit_loom: a std-only run would not explore interleavings");

#[path = "../../../crates/varlit-core/src/scan_control.rs"]
mod scan_control;
#[path = "../../../crates/varlit-core/src/matrix_tiles/reader_lock.rs"]
mod reader_lock;

use loom::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use scan_control::{ScanCancellationToken, ScanOptions, ScanProgressDriver};

fn main() {
    // Cancellation publishes prior writes and is monotonic across token clones.
    loom::model(|| {
        let token = ScanCancellationToken::new();
        let payload = Arc::new(AtomicUsize::new(0));
        let sender = token.clone();
        let data = payload.clone();
        let worker = loom::thread::spawn(move || {
            data.store(17, Ordering::Relaxed);
            sender.cancel();
            sender.cancel();
        });
        if token.is_cancelled() {
            assert_eq!(payload.load(Ordering::Relaxed), 17);
            assert!(token.is_cancelled());
        }
        worker.join().unwrap();
        assert!(token.is_cancelled());
    });
    // Exercise the real progress driver's entry/callback/exit checks while a
    // second thread cancels. No unbounded spin or unsupported fairness premise.
    loom::model(|| {
        let token = ScanCancellationToken::new();
        let sender = token.clone();
        let worker = loom::thread::spawn(move || sender.cancel());
        let mut driver = ScanProgressDriver::new(
            ScanOptions {
                cancellation: Some(&token),
                ..Default::default()
            },
            0,
            64,
        );
        let mut observer = |_| {};
        let _ = driver.start(&mut observer);
        worker.join().unwrap();
        assert!(driver.record_completed(32, &mut observer).is_err());
        assert!(driver.complete(&mut observer).is_err());
    });
    // The tile reader's lock admits one guard at a time. The guarded value
    // is a Loom cell, so two guards alive together, or an access not ordered
    // after the previous holder's release, fails the model.
    loom::model(|| {
        let lock = Arc::new(reader_lock::ReaderLock::new(loom::cell::UnsafeCell::new(0u32)));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let lock = lock.clone();
                loom::thread::spawn(move || {
                    let guard = lock.lock().ok().expect("not poisoned");
                    // SAFETY: the guard is exclusive; Loom checks that claim.
                    guard.with_mut(|value| unsafe { *value += 1 });
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let guard = lock.lock().ok().expect("not poisoned");
        // SAFETY: as above.
        assert_eq!(guard.with(|value| unsafe { *value }), 2);
    });
    // `try_lock` never waits and never hands out a second guard: it either
    // gets exclusive access or nothing, whatever the holder is doing.
    loom::model(|| {
        let lock = Arc::new(reader_lock::ReaderLock::new(loom::cell::UnsafeCell::new(0u32)));
        let other = lock.clone();
        let worker = loom::thread::spawn(move || {
            if let Some(guard) = other.try_lock() {
                // SAFETY: the guard is exclusive; Loom checks that claim.
                guard.with_mut(|value| unsafe { *value += 1 });
                true
            } else {
                false
            }
        });
        {
            let guard = lock.lock().ok().expect("not poisoned");
            // SAFETY: as above.
            guard.with_mut(|value| unsafe { *value += 10 });
        }
        let taken = worker.join().unwrap();
        let guard = lock.lock().ok().expect("not poisoned");
        // SAFETY: as above.
        let value = guard.with(|value| unsafe { *value });
        assert_eq!(value, if taken { 11 } else { 10 });
    });
    println!("Cancellation, progress and reader-lock models passed");
}
