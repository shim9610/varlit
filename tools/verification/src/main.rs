//! Run with RUSTFLAGS="--cfg varlit_loom" cargo run --locked.
#![allow(dead_code)]

#[cfg(not(varlit_loom))]
compile_error!("Use --cfg varlit_loom: a std-only run would not explore interleavings");

#[path = "../../../crates/varlit-core/src/scan_control.rs"]
mod scan_control;

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
    println!("Both production cancellation models passed");
}
