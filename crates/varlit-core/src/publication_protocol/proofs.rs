//! The environment model may fail before, during or after any I/O. A successful
//! sync persists preceding complete writes. Slot checksums reject torn writes;
//! they do not authenticate arbitrary replacement bytes. Old committed data is
//! immutable.
//!
//! A commit appends the sync marker that closes its records, syncs the
//! primary, then writes the end it reached into the published-end slot after
//! the newest. That write is not synced: the next barrier makes it durable,
//! and until then either slot may be what storage holds.
use super::*;
use core::cell::Cell;

#[derive(Clone, Copy, PartialEq, Eq, kani::Arbitrary)]
enum Slot {
    Old,
    Torn,
    New,
}

struct Fault {
    at: u8,
    effect: u8,
    calls: u8,
}
impl Fault {
    fn arbitrary() -> Self {
        let effect = kani::any();
        kani::assume(effect <= 2); // no effect, partial effect, complete effect + error
        Self {
            at: kani::any(),
            effect,
            calls: 0,
        }
    }
    fn begin(&mut self) -> (bool, bool, bool) {
        let failed = self.calls == self.at;
        self.calls += 1;
        (
            !failed || self.effect != 0,
            !failed || self.effect == 2,
            failed,
        )
    }
}

struct Medium {
    primary_synced: bool,
    /// The sequence number of the newest valid slot before this commit.
    newest: u64,
    /// Whether the adapter reports a failed slot write (the stream writer
    /// does not: a failed write leaves the earlier end, which is safe).
    strict: bool,
    slots: [Slot; 2],
    persisted: [Slot; 2],
    poisoned: bool,
    finished: bool,
    publication_failed: bool,
    fault: Fault,
}
impl Medium {
    fn new(primary_synced: bool, poisoned: bool) -> Self {
        Self {
            primary_synced,
            newest: kani::any(),
            strict: kani::any(),
            slots: [Slot::Old; 2],
            persisted: [Slot::Old; 2],
            poisoned,
            finished: false,
            publication_failed: false,
            fault: Fault::arbitrary(),
        }
    }
    fn newest_slot(&self) -> usize {
        (self.newest % 2) as usize
    }
    /// The new end as a reader or the next open would adopt it, from memory
    /// or after power loss.
    fn published_new(&self) -> bool {
        self.slots.contains(&Slot::New) || self.persisted.contains(&Slot::New)
    }
    fn check(&self) {
        // A reader can run after each partial effect, including before an
        // error return. Power loss can expose either persisted slot. The
        // newest prior end is never the slot written, so it survives every
        // effect, in memory and on storage.
        let newest = self.newest_slot();
        for slots in [self.slots, self.persisted] {
            assert!(
                slots[newest] == Slot::Old,
                "newest published end was destroyed"
            );
        }
        if self.published_new() {
            assert!(
                self.primary_synced,
                "end published before primary durability"
            );
        }
    }
    fn commit_step(&mut self, step: CommitStep) -> Result<(), ()> {
        match step {
            CommitStep::Prepare => {
                if self.poisoned {
                    return Err(());
                }
                let (_, _, failed) = self.fault.begin();
                if failed {
                    return Err(());
                }
            }
            CommitStep::PrimarySync => {
                let (effect, complete, failed) = self.fault.begin();
                if effect && (complete || kani::any()) {
                    self.primary_synced = true;
                }
                if failed {
                    return Err(());
                }
            }
            CommitStep::Publish => {
                let slot = slot_after(self.newest);
                let (effect, complete, failed) = self.fault.begin();
                if effect {
                    self.slots[slot] = if complete { Slot::New } else { Slot::Torn };
                    // Background writeback may persist none, a torn prefix,
                    // or the complete write; a partial write cannot invent
                    // completion.
                    let persisted: Slot = kani::any();
                    kani::assume(complete || persisted != Slot::New);
                    self.persisted[slot] = persisted;
                }
                self.check();
                if failed && self.strict {
                    self.publication_failed = true;
                    return Err(());
                }
                return Ok(());
            }
            CommitStep::Poison => self.poisoned = true,
            CommitStep::Finish => {
                // The marker that closes the commit is in the synced primary.
                assert!(self.primary_synced, "acknowledged an undurable commit");
                self.finished = true;
            }
        }
        self.check();
        Ok(())
    }
}

#[kani::proof]
#[kani::unwind(10)]
fn commit_survives_every_io_failure_effect() {
    let mut medium = Medium::new(kani::any(), false);
    let result = commit(|step| medium.commit_step(step));
    medium.check();
    assert_eq!(result.is_ok(), medium.finished);
    if medium.publication_failed {
        assert!(medium.poisoned, "uncertain publication left writer usable");
    }
}

#[kani::proof]
#[kani::unwind(10)]
fn successful_commit_is_reachable_and_durable() {
    let mut medium = Medium::new(false, false);
    medium.fault.at = u8::MAX;
    assert!(commit(|step| medium.commit_step(step)).is_ok());
    assert!(medium.finished && medium.primary_synced);
    let written = slot_after(medium.newest);
    assert!(medium.slots[written] == Slot::New);
    assert!(medium.slots[1 - written] == Slot::Old);
}

#[kani::proof]
fn recovery_never_publishes_before_truncate_and_sync() {
    let truncated = Cell::new(false);
    let synced = Cell::new(false);
    let published = Cell::new(false);
    let fail_at: u8 = kani::any();
    let fail_after: bool = kani::any();
    let result = restore(
        &mut (),
        |_| {
            if fail_at != 0 || fail_after {
                truncated.set(true);
            }
            if fail_at == 0 { Err(()) } else { Ok(()) }
        },
        |_| {
            assert!(truncated.get());
            if fail_at != 1 || fail_after {
                synced.set(true);
            }
            if fail_at == 1 { Err(()) } else { Ok(()) }
        },
        |_| {
            assert!(
                truncated.get() && synced.get(),
                "recovery published before native sync"
            );
            if fail_at != 2 || fail_after {
                published.set(true);
            }
            if fail_at == 2 { Err(()) } else { Ok(()) }
        },
    );
    assert!(!published.get() || (truncated.get() && synced.get()));
    if result.is_ok() {
        assert!(published.get());
    }
}

#[kani::proof]
fn follow_error_preserves_the_entire_reader() {
    let old: (u64, u64) = kani::any();
    let candidate: (u64, u64) = kani::any();
    let reject: bool = kani::any();
    let mut reader = old;
    let result = follow(
        &mut reader,
        |_| if reject { Err(()) } else { Ok(candidate) },
        |current, next| *current = next,
    );
    if result.is_err() {
        assert_eq!(reader, old);
    } else {
        assert_eq!(reader, candidate);
    }
}

// An abstract frontier invariant with no history-length bound. These are byte
// frontiers, not a proof of payload decoding or a replacement for the I/O model.
//
// `marked` is the end of the primary's verified sync-marker prefix: what
// recovery keeps and readers expose. It bounds what was acknowledged; the
// published end (`published`, and `persisted` on storage) trails it.
#[derive(kani::Arbitrary)]
struct Frontiers {
    published: u64,
    persisted: u64,
    marked: u64,
    acknowledged: u64,
    working: u64,
    durable: u64,
    reader: u64,
    poisoned: bool,
}
impl Frontiers {
    fn invariant(&self) -> bool {
        self.acknowledged <= self.marked
            && self.persisted <= self.published
            && self.published <= self.marked
            && self.marked <= self.working
            && self.marked <= self.durable
            && self.reader <= self.marked
    }
}

#[kani::proof]
#[kani::unwind(10)]
fn frontier_invariant_is_preserved_by_an_arbitrary_transition() {
    let mut state: Frontiers = kani::any();
    kani::assume(state.invariant());
    let action: u8 = kani::any();
    match action {
        0 => {
            // Append, overwrite or delete extends the log, not the confirmed view.
            if !state.poisoned {
                if let Some(next) = state.working.checked_add(kani::any()) {
                    state.working = next;
                }
            }
        }
        1 => {
            // The production commit control flow, with arbitrary failure.
            if !state.poisoned {
                let mut medium = Medium::new(state.durable >= state.working, false);
                let result = commit(|step| medium.commit_step(step));
                if medium.primary_synced {
                    // The marker closing everything written is now durable.
                    state.durable = state.working;
                    state.marked = state.working;
                }
                if medium.slots.contains(&Slot::New) {
                    state.published = state.working;
                }
                if medium.persisted.contains(&Slot::New) {
                    state.persisted = state.working;
                }
                state.poisoned = medium.poisoned;
                if result.is_ok() {
                    state.acknowledged = state.marked;
                }
            }
        }
        2 => {
            // Process death leaves the page cache and other readers alive.
            state.poisoned = true;
        }
        3 => {
            // Power loss also destroys reader handles; synced data survives,
            // the verified marker prefix included, and the published end is
            // whichever slot reached storage.
            state.published = state.persisted;
            state.working = state.durable;
            state.reader = 0;
            state.poisoned = true;
        }
        4 => {
            // Recovery cuts the primary back to its verified marker prefix,
            // never further, makes it durable, then publishes its end.
            let fail: u8 = kani::any();
            let mut cut = (state.working, state.durable, state.published);
            let result = restore(
                &mut cut,
                |cut| {
                    cut.0 = state.marked;
                    if fail == 0 { Err(()) } else { Ok(()) }
                },
                |cut| {
                    cut.1 = state.marked;
                    if fail == 1 { Err(()) } else { Ok(()) }
                },
                |cut| {
                    cut.2 = state.marked;
                    if fail == 2 { Err(()) } else { Ok(()) }
                },
            );
            (state.working, state.durable, state.published) = cut;
            state.poisoned = result.is_err();
        }
        _ => {
            // Open/follow a reader; failed follow retains its old frontier.
            let fail: bool = kani::any();
            // Readers expose the verified marker prefix past the published end.
            let _ = follow(
                &mut state.reader,
                |_| if fail { Err(()) } else { Ok(state.marked) },
                |reader, next| *reader = next,
            );
        }
    }
    assert!(state.invariant());
}

#[kani::proof]
fn initial_frontier_satisfies_the_inductive_invariant() {
    let frontier: u64 = kani::any();
    let state = Frontiers {
        published: frontier,
        persisted: frontier,
        marked: frontier,
        acknowledged: frontier,
        working: frontier,
        durable: frontier,
        reader: frontier,
        poisoned: false,
    };
    assert!(state.invariant());
}

#[kani::proof]
#[kani::unwind(10)]
fn poisoned_writer_never_reaches_publication() {
    let mut medium = Medium::new(false, true);
    let result = commit(|step| medium.commit_step(step));
    assert!(result.is_err(), "poisoned writer accepted a commit");
    assert_eq!(medium.fault.calls, 0, "poisoned writer performed I/O");
    assert!(medium.slots == [Slot::Old; 2] && !medium.finished);
}
