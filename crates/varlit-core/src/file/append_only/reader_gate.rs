//! Registration is ordered against recovery, not against ordinary append.
#[cfg(varlit_loom)]
use loom::sync::atomic::{AtomicUsize, Ordering};
#[cfg(not(varlit_loom))]
use std::sync::atomic::{AtomicUsize, Ordering};

const EXCLUSIVE: usize = 1 << (usize::BITS - 1);

#[derive(Debug)]
pub(super) struct ReaderGate(AtomicUsize);

impl ReaderGate {
    pub(super) fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    pub(super) fn try_reader(&self) -> bool {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state < EXCLUSIVE - 1).then(|| state + 1)
            })
            .is_ok()
    }

    pub(super) fn release_reader(&self) {
        let before = self.0.fetch_sub(1, Ordering::Release);
        debug_assert!(before > 0 && before < EXCLUSIVE);
    }

    pub(super) fn try_recovery(&self) -> bool {
        self.0
            .compare_exchange(0, EXCLUSIVE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn end_recovery(&self) {
        let before = self.0.swap(0, Ordering::Release);
        debug_assert_eq!(before, EXCLUSIVE);
    }
}
