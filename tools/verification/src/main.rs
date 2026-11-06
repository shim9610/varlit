//! Run with RUSTFLAGS="--cfg varlit_loom" cargo run --locked.
#![allow(dead_code)]

#[cfg(not(varlit_loom))]
compile_error!("Use --cfg varlit_loom: a std-only run would not explore interleavings");

#[path = "../../../crates/varlit-core/src/scan_control.rs"]
mod scan_control;

#[path = "../../../crates/varlit-core/src/file/append_only/reader_gate.rs"]
mod reader_gate;

use loom::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use scan_control::{ScanCancellationToken, ScanOptions, ScanProgressDriver};

fn main() {
    // A registration races recovery; the winner excludes the other until its
    // complete mapped-reader lifetime ends. This is the production atomic gate.
    loom::model(|| {
        let gate = Arc::new(reader_gate::ReaderGate::new());
        let active = Arc::new(AtomicUsize::new(0));
        let recovery = Arc::new(AtomicUsize::new(0));
        let g = gate.clone();
        let a = active.clone();
        let r = recovery.clone();
        let reader = loom::thread::spawn(move || {
            if g.try_reader() {
                a.fetch_add(1, Ordering::SeqCst);
                assert_eq!(r.load(Ordering::SeqCst), 0);
                // A view can outlive its reader; registration ends only when
                // the last owner has stopped using the captured bytes.
                loom::thread::yield_now();
                assert_eq!(r.load(Ordering::SeqCst), 0);
                a.fetch_sub(1, Ordering::SeqCst);
                g.release_reader();
            }
        });
        if gate.try_recovery() {
            recovery.store(1, Ordering::SeqCst);
            assert_eq!(active.load(Ordering::SeqCst), 0);
            loom::thread::yield_now();
            assert_eq!(active.load(Ordering::SeqCst), 0);
            recovery.store(0, Ordering::SeqCst);
            gate.end_recovery();
        }
        reader.join().unwrap();
        assert!(gate.try_recovery());
        assert!(!gate.try_reader());
        gate.end_recovery();
        assert!(gate.try_reader());
        gate.release_reader();
    });
    // Final reader release publishes all preceding use/unmap work before a
    // successful exclusive recovery claim. No join supplies that ordering.
    loom::model(|| {
        let gate = Arc::new(reader_gate::ReaderGate::new());
        assert!(gate.try_reader());
        let payload = Arc::new(AtomicUsize::new(0));
        let g = gate.clone();
        let p = payload.clone();
        let reader = loom::thread::spawn(move || {
            p.store(17, Ordering::Relaxed);
            g.release_reader();
        });
        if gate.try_recovery() {
            assert_eq!(payload.load(Ordering::Relaxed), 17);
            gate.end_recovery();
        }
        reader.join().unwrap();
        assert!(gate.try_recovery());
        assert_eq!(payload.load(Ordering::Relaxed), 17);
        gate.end_recovery();
    });
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
    println!("Production reader/recovery exclusion and both cancellation models passed");
}
