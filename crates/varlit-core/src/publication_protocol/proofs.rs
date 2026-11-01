//! The environment model may fail before, during or after any I/O. A successful
//! sync persists preceding complete writes. Checksums reject torn heads; they do
//! not authenticate arbitrary replacement bytes. Old committed data is immutable.
use super::*;
use core::cell::Cell;

#[derive(Clone, Copy, PartialEq, Eq, kani::Arbitrary)]
enum Head {
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
    body_complete: bool,
    body_synced: bool,
    selected: bool,
    first: usize,
    old_offsets: [u64; 2],
    heads: [Head; 2],
    persisted: [Head; 2],
    poisoned: bool,
    finished: bool,
    publication_failed: bool,
    fault: Fault,
}
impl Medium {
    fn new(primary_synced: bool, poisoned: bool) -> Self {
        Self {
            primary_synced,
            body_complete: false,
            body_synced: false,
            selected: false,
            first: 0,
            old_offsets: kani::any(),
            heads: [Head::Old; 2],
            persisted: [Head::Old; 2],
            poisoned,
            finished: false,
            publication_failed: false,
            fault: Fault::arbitrary(),
        }
    }
    fn check(&self) {
        // A reader can run after each partial effect, including before an error
        // return. Power loss can expose either persisted head independently.
        assert!(self.heads.iter().any(|h| *h != Head::Torn));
        assert!(self.persisted.iter().any(|h| *h != Head::Torn));
        let newest = self.old_offsets[0].max(self.old_offsets[1]);
        for heads in [self.heads, self.persisted] {
            assert!(
                heads[0] == Head::New
                    || heads[1] == Head::New
                    || (heads[0] == Head::Old && self.old_offsets[0] == newest)
                    || (heads[1] == Head::Old && self.old_offsets[1] == newest),
                "newest prior anchor was destroyed"
            );
        }
        for head in [
            self.heads[0],
            self.heads[1],
            self.persisted[0],
            self.persisted[1],
        ] {
            if head == Head::New {
                assert!(
                    self.primary_synced,
                    "head exposed before primary durability"
                );
                assert!(
                    self.body_complete && self.body_synced,
                    "head exposed before checkpoint durability"
                );
            }
        }
    }
    fn checkpoint_step(&mut self, step: CheckpointStep) -> Result<(), ()> {
        let (effect, complete, failed) = self.fault.begin();
        if effect {
            match step {
                CheckpointStep::Body => self.body_complete = complete,
                CheckpointStep::DataSync => {
                    if complete || kani::any() {
                        self.body_synced = self.body_complete;
                        // sync_data covers the whole sidecar, including any
                        // prior head whose final sync was interrupted.
                        self.persisted = self.heads;
                    }
                }
                CheckpointStep::SelectHead => {
                    self.first = older_head(self.old_offsets[0], self.old_offsets[1]);
                    self.selected = complete;
                }
                CheckpointStep::Head(second) => {
                    assert!(self.selected);
                    let slot = if second { 1 - self.first } else { self.first };
                    self.heads[slot] = if complete { Head::New } else { Head::Torn };
                    // Background writeback may persist none, a torn prefix, or
                    // the complete write; partial writes cannot invent completion.
                    let persisted: Head = kani::any();
                    kani::assume(complete || persisted != Head::New);
                    self.persisted[slot] = persisted;
                }
                CheckpointStep::HeadSync => {
                    if complete || kani::any() {
                        self.persisted = self.heads;
                    }
                }
            }
            self.check();
        }
        if failed { Err(()) } else { Ok(()) }
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
                let result =
                    checkpoint(CheckpointKind::Confirmed, |step| self.checkpoint_step(step));
                self.publication_failed = result.is_err();
                return result;
            }
            CommitStep::Poison => self.poisoned = true,
            CommitStep::Finish => {
                assert!(
                    self.persisted == [Head::New; 2],
                    "acknowledged an undurable commit"
                );
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
    assert!(medium.finished && medium.primary_synced && medium.body_synced);
    assert!(medium.heads == [Head::New; 2] && medium.persisted == [Head::New; 2]);
}

#[kani::proof]
#[kani::unwind(10)]
fn nonconfirmed_publications_do_not_use_a_second_head() {
    let working: bool = kani::any();
    let kind = if working {
        CheckpointKind::Working
    } else {
        CheckpointKind::Dirty
    };
    let mut body = false;
    let mut synced = false;
    let mut heads = 0;
    let mut syncs = 0;
    let result: Result<(), ()> = checkpoint(kind, |step| {
        match step {
            CheckpointStep::Body => body = true,
            CheckpointStep::DataSync => {
                assert!(body);
                synced = true;
            }
            CheckpointStep::SelectHead => assert!(body),
            CheckpointStep::Head(second) => {
                assert!(!second);
                assert!(working || synced);
                heads += 1;
            }
            CheckpointStep::HeadSync => syncs += 1,
        }
        Ok(())
    });
    assert!(result.is_ok());
    assert_eq!(heads, 1);
    assert_eq!(syncs, u8::from(!working));
}

#[kani::proof]
fn recovery_never_publishes_before_truncate_and_sync() {
    let truncated = Cell::new(false);
    let synced = Cell::new(false);
    let published = Cell::new(false);
    let fail_at: u8 = kani::any();
    let fail_after: bool = kani::any();
    let result = restore(
        || {
            if fail_at != 0 || fail_after {
                truncated.set(true);
            }
            if fail_at == 0 { Err(()) } else { Ok(()) }
        },
        || {
            assert!(truncated.get());
            if fail_at != 1 || fail_after {
                synced.set(true);
            }
            if fail_at == 1 { Err(()) } else { Ok(()) }
        },
        || {
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
#[derive(kani::Arbitrary)]
struct Frontiers {
    confirmed: u64,
    persisted: u64,
    acknowledged: u64,
    working: u64,
    durable: u64,
    pinned: u64,
    poisoned: bool,
}
impl Frontiers {
    fn invariant(&self) -> bool {
        self.acknowledged <= self.persisted
            && self.persisted <= self.confirmed
            && self.confirmed <= self.working
            && self.confirmed <= self.durable
            && self.pinned <= self.confirmed
            && (self.poisoned || self.persisted == self.confirmed)
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
            // The production commit/checkpoint control flow, with arbitrary failure.
            if !state.poisoned {
                let old = state.confirmed;
                let mut medium = Medium::new(state.durable >= state.working, false);
                let result = commit(|step| medium.commit_step(step));
                if medium.primary_synced {
                    state.durable = state.working;
                }
                state.confirmed = if medium.heads.contains(&Head::New) {
                    state.working
                } else {
                    old
                };
                state.persisted = if medium.persisted.contains(&Head::New) {
                    state.working
                } else {
                    old
                };
                state.poisoned = medium.poisoned;
                if result.is_ok() {
                    state.acknowledged = state.confirmed;
                }
            }
        }
        2 => {
            // Process death leaves the page cache and other readers alive.
            state.poisoned = true;
        }
        3 => {
            // Power loss also destroys reader handles; synced data survives.
            state.confirmed = state.persisted;
            state.working = state.durable;
            state.pinned = 0;
            state.poisoned = true;
        }
        4 => {
            // Recovery validates the latest confirmed target before truncation.
            let fail: u8 = kani::any();
            let working = Cell::new(state.working);
            let durable = Cell::new(state.durable);
            let persisted = Cell::new(state.persisted);
            let result = restore(
                || {
                    working.set(state.confirmed);
                    if fail == 0 { Err(()) } else { Ok(()) }
                },
                || {
                    durable.set(state.confirmed);
                    if fail == 1 { Err(()) } else { Ok(()) }
                },
                || {
                    persisted.set(state.confirmed);
                    if fail == 2 { Err(()) } else { Ok(()) }
                },
            );
            state.working = working.get();
            state.durable = durable.get();
            state.persisted = persisted.get();
            state.poisoned = result.is_err();
        }
        5 => {
            // A clean writer may restart directly, without truncating/restoring.
            // The separate resume proof establishes the correspondence between
            // matching heads and the latest durable frontier used here.
            if state.poisoned && state.working == state.confirmed {
                let first = Some((state.confirmed, 0));
                let second = Some((state.persisted, 0));
                let fail: bool = kani::any();
                let persisted = Cell::new(state.persisted);
                let result = resume(first, second, || {
                    if !fail || kani::any() {
                        persisted.set(state.confirmed);
                    }
                    if fail { Err(()) } else { Ok(()) }
                });
                state.persisted = persisted.get();
                state.poisoned = result.is_err();
            }
        }
        _ => {
            // Open/follow a reader; failed follow retains its old frontier.
            let fail: bool = kani::any();
            let _ = follow(
                &mut state.pinned,
                |_| if fail { Err(()) } else { Ok(state.confirmed) },
                |pin, next| *pin = next,
            );
        }
    }
    assert!(state.invariant());
}

#[kani::proof]
fn initial_frontier_satisfies_the_inductive_invariant() {
    let frontier: u64 = kani::any();
    let state = Frontiers {
        confirmed: frontier,
        persisted: frontier,
        acknowledged: frontier,
        working: frontier,
        durable: frontier,
        pinned: frontier,
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
    assert!(medium.heads == [Head::Old; 2] && !medium.finished);
}

#[kani::proof]
#[kani::unwind(10)]
fn resumed_writer_cannot_acknowledge_a_volatile_head() {
    let mut medium = Medium::new(false, false);
    let _ = commit(|step| medium.commit_step(step));
    let visible_new = medium.heads.contains(&Head::New);
    // A process restart preserves visible heads, even when the old writer
    // returned no acknowledgement. Body/primary dependencies are already synced.
    let encode = |head, slot| match head {
        Head::Old => Some((medium.old_offsets[slot], 0)),
        Head::Torn => None,
        Head::New => Some((u64::MAX, 1)),
    };
    let first = encode(medium.heads[0], 0);
    let second = encode(medium.heads[1], 1);
    let failed: bool = kani::any();
    let persisted = Cell::new(medium.persisted);
    let ready = resume(first, second, || {
        if !failed || kani::any() {
            persisted.set(medium.heads);
        }
        if failed { Err(()) } else { Ok(()) }
    });
    // A clean/no-write sync may acknowledge the view only after successful resume.
    if ready.is_ok() && visible_new {
        assert!(
            persisted.get().contains(&Head::New),
            "resumed writer acknowledged volatile head"
        );
    }
}
