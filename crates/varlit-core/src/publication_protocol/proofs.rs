//! The environment model may fail before, during or after any I/O. A successful
//! sync persists preceding complete writes. Checksums reject torn heads; they do
//! not authenticate arbitrary replacement bytes. Old committed data is immutable.
//!
//! A commit acknowledges once the primary — the records and the sync marker
//! that closes them — is durable. Whether it also confirms the sidecar is the
//! writer's confirm policy; when it does, the confirmation is either the
//! single-barrier append-only form (version 5) or the two-barrier page-reusing
//! form (version 6), and either must leave the new generation durable before
//! the acknowledgement.
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
    /// Whether this commit confirms the sidecar, and in which form.
    confirm: bool,
    two_barrier: bool,
    body_complete: bool,
    /// Whether the generation's sidecar bytes reached storage. A persisted new
    /// head is adopted only when this holds: the head names their checksum.
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
            confirm: kani::any(),
            two_barrier: kani::any(),
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
    /// The new generation as a reader or a recovery would adopt it: a new head
    /// whose generation verifies. In memory that needs the complete body; on
    /// storage it needs the body to have reached storage too.
    fn visible_new(&self) -> bool {
        self.heads.contains(&Head::New) && self.body_complete
    }
    fn durable_new(&self) -> bool {
        self.persisted.contains(&Head::New) && self.body_complete && self.body_synced
    }
    fn check(&self) {
        // A reader can run after each partial effect, including before an error
        // return. Power loss can expose either persisted head independently.
        // The newest prior anchor is never the slot written, so it survives
        // every effect, in memory and on storage.
        let newest = self.old_offsets[0].max(self.old_offsets[1]);
        for heads in [self.heads, self.persisted] {
            assert!(
                (heads[0] == Head::Old && self.old_offsets[0] == newest)
                    || (heads[1] == Head::Old && self.old_offsets[1] == newest),
                "newest prior anchor was destroyed"
            );
        }
        if self.visible_new() || self.durable_new() {
            assert!(
                self.primary_synced,
                "generation adopted before primary durability"
            );
        }
    }
    fn checkpoint_step(&mut self, step: CheckpointStep) -> Result<(), ()> {
        let (effect, complete, failed) = self.fault.begin();
        if effect {
            match step {
                CheckpointStep::Body => {
                    self.body_complete = complete;
                    // Background writeback may persist the body at any time.
                    if complete && kani::any() {
                        self.body_synced = true;
                    }
                }
                CheckpointStep::SelectHead => {
                    self.first = older_head(self.old_offsets[0], self.old_offsets[1]);
                    self.selected = complete;
                }
                CheckpointStep::Head => {
                    assert!(self.selected);
                    let slot = self.first;
                    self.heads[slot] = if complete { Head::New } else { Head::Torn };
                    // Background writeback may persist none, a torn prefix, or
                    // the complete write; partial writes cannot invent completion.
                    let persisted: Head = kani::any();
                    kani::assume(complete || persisted != Head::New);
                    self.persisted[slot] = persisted;
                }
                CheckpointStep::BodySync => {
                    // Before any head of this publication is written.
                    assert!(!self.selected);
                    if complete || kani::any() {
                        self.body_synced = self.body_complete;
                    }
                }
                CheckpointStep::HeadSync => {
                    if complete || kani::any() {
                        // sync_data covers the whole sidecar: body and head.
                        self.body_synced = self.body_complete;
                        self.persisted = self.heads;
                    }
                }
            }
            if matches!(step, CheckpointStep::Head) && self.two_barrier {
                // The two-barrier form only names a body already on storage.
                assert!(self.body_synced, "head written before its body barrier");
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
                if !self.confirm {
                    // The sidecar trails: recovery and readers take this commit
                    // from the primary's verified marker prefix.
                    return Ok(());
                }
                let result = if self.two_barrier {
                    checkpoint_with_body_barrier(CheckpointKind::Confirmed, |step| {
                        self.checkpoint_step(step)
                    })
                } else {
                    checkpoint(CheckpointKind::Confirmed, |step| self.checkpoint_step(step))
                };
                self.publication_failed = result.is_err();
                return result;
            }
            CommitStep::Poison => self.poisoned = true,
            CommitStep::Finish => {
                // The marker that closes the commit is in the synced primary.
                assert!(self.primary_synced, "acknowledged an undurable commit");
                if self.confirm {
                    assert!(self.durable_new(), "acknowledged an undurable confirmation");
                }
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
    if medium.confirm {
        assert!(medium.body_synced);
        assert!(
            medium.heads[medium.first] == Head::New && medium.persisted[medium.first] == Head::New
        );
        assert!(medium.heads[1 - medium.first] == Head::Old);
    } else {
        assert!(
            medium.heads == [Head::Old; 2],
            "an unconfirmed commit wrote a head"
        );
    }
}

#[kani::proof]
#[kani::unwind(10)]
fn every_two_barrier_publication_syncs_its_body_before_its_head() {
    let choice: u8 = kani::any();
    let kind = match choice % 3 {
        0 => CheckpointKind::Working,
        1 => CheckpointKind::Dirty,
        _ => CheckpointKind::Confirmed,
    };
    let confirmed = matches!(kind, CheckpointKind::Confirmed);
    let (mut body, mut body_synced, mut heads, mut syncs) = (false, false, 0u8, 0u8);
    let result: Result<(), ()> = checkpoint_with_body_barrier(kind, |step| {
        match step {
            CheckpointStep::Body => body = true,
            CheckpointStep::BodySync => {
                assert!(body && heads == 0);
                body_synced = true;
                syncs += 1;
            }
            CheckpointStep::SelectHead => assert!(body && (body_synced || !confirmed)),
            CheckpointStep::Head => {
                assert!(body && (body_synced || !confirmed));
                heads += 1;
            }
            CheckpointStep::HeadSync => {
                assert!(heads == 1 && body_synced);
                syncs += 1;
            }
        }
        Ok(())
    });
    assert!(result.is_ok());
    assert_eq!(heads, 1);
    assert_eq!(syncs, if confirmed { 2 } else { 0 });
}

#[kani::proof]
#[kani::unwind(10)]
fn every_publication_writes_one_head_and_only_confirmed_syncs_once() {
    let choice: u8 = kani::any();
    let kind = match choice % 3 {
        0 => CheckpointKind::Working,
        1 => CheckpointKind::Dirty,
        _ => CheckpointKind::Confirmed,
    };
    let mut body = false;
    let mut heads = 0;
    let mut syncs = 0;
    let result: Result<(), ()> = checkpoint(kind, |step| {
        match step {
            CheckpointStep::Body => body = true,
            CheckpointStep::SelectHead => assert!(body),
            CheckpointStep::Head => {
                assert!(body);
                heads += 1;
            }
            CheckpointStep::HeadSync => {
                assert!(heads == 1);
                syncs += 1;
            }
            CheckpointStep::BodySync => panic!("single-barrier publication synced its body"),
        }
        Ok(())
    });
    assert!(result.is_ok());
    assert_eq!(heads, 1);
    assert_eq!(syncs, u8::from(matches!(kind, CheckpointKind::Confirmed)));
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
//
// `marked` is the end of the primary's verified sync-marker prefix: what
// recovery keeps and readers expose. It, not the sidecar, bounds what was
// acknowledged; the sidecar (`confirmed`, `persisted`) trails it.
#[derive(kani::Arbitrary)]
struct Frontiers {
    confirmed: u64,
    persisted: u64,
    marked: u64,
    acknowledged: u64,
    working: u64,
    durable: u64,
    pinned: u64,
    poisoned: bool,
}
impl Frontiers {
    fn invariant(&self) -> bool {
        self.acknowledged <= self.marked
            && self.persisted <= self.confirmed
            && self.confirmed <= self.marked
            && self.marked <= self.working
            && self.marked <= self.durable
            && self.pinned <= self.marked
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
                    // The marker closing everything written is now durable.
                    state.durable = state.working;
                    state.marked = state.working;
                }
                state.confirmed = if medium.visible_new() {
                    state.working
                } else {
                    old
                };
                state.persisted = if medium.durable_new() {
                    state.working
                } else {
                    old
                };
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
            // the verified marker prefix included.
            state.confirmed = state.persisted;
            state.working = state.durable;
            state.pinned = 0;
            state.poisoned = true;
        }
        4 => {
            // Recovery cuts the primary back to its verified marker prefix —
            // never further — republishes the confirmed base, then replays the
            // kept tail and confirms it.
            let fail: u8 = kani::any();
            let working = Cell::new(state.working);
            let durable = Cell::new(state.durable);
            let persisted = Cell::new(state.persisted);
            let result = restore(
                || {
                    working.set(state.marked);
                    if fail == 0 { Err(()) } else { Ok(()) }
                },
                || {
                    durable.set(state.marked);
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
            if result.is_ok() && kani::any() {
                // The replay's confirmation succeeded.
                state.confirmed = state.marked;
                state.persisted = state.marked;
            }
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
            // Readers expose the verified marker prefix past the confirmed EOF.
            let _ = follow(
                &mut state.pinned,
                |_| if fail { Err(()) } else { Ok(state.marked) },
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
        marked: frontier,
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
    let visible_new = medium.visible_new();
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
    let persisted = Cell::new(medium.durable_new());
    let ready = resume(first, second, || {
        if !failed || kani::any() {
            // sync_data covers every byte the visible generation names.
            persisted.set(visible_new);
        }
        if failed { Err(()) } else { Ok(()) }
    });
    // A clean/no-write sync may acknowledge the view only after successful resume.
    if ready.is_ok() && visible_new {
        assert!(persisted.get(), "resumed writer acknowledged volatile head");
    }
}
