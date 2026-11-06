//! Write authority can finish independently of the protection held by readers.
use super::*;
use std::io;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Writer lifetime within one pinned append-only session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AppendOnlyWriterState {
    /// No writer handle has been issued yet.
    NotStarted,
    /// Exactly one writer handle is active.
    Active,
    /// The writer ended; the session may issue another handle.
    Closed,
    /// Recovery excludes new reader registration and writer issuance.
    Recovering,
    /// Write authority ended permanently; retained readers can finish draining.
    Finished,
}

/// Allocation-free facts about the last reported operation error.
///
/// This summarizes the returned error, not every nested cause. In particular,
/// an append rollback error describes the failed rollback. `None` for a commit
/// sequence does not prove that no publication occurred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendOnlyFailure {
    pub operation: &'static str,
    pub category: &'static str,
    pub io_kind: Option<io::ErrorKind>,
    pub raw_os_error: Option<i32>,
    pub committed_sequence: Option<u64>,
    pub writer_epoch: u64,
}

/// A coherent snapshot of writer lifetime and its most recent failure.
///
/// Errors remain latched across reconnect/recovery. They are not an event queue:
/// later errors replace earlier ones. `revision` saturates rather than wrapping;
/// waiters compare the entire snapshot. Successful appends/commits do not update
/// this status; follow publications through the reader's existing `follow` API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendOnlySessionStatus {
    pub writer: AppendOnlyWriterState,
    pub recovery_required: bool,
    pub writer_epoch: u64,
    pub revision: u64,
    pub last_error: Option<AppendOnlyFailure>,
}

#[derive(Debug)]
pub(super) struct Protection {
    pub(super) spec: FormatSpec,
    pub(super) path: PathBuf,
    pub(super) identity: [u8; 16],
    gate: reader_gate::ReaderGate,
    signal: Mutex<AppendOnlySessionStatus>,
    changed: Condvar,
    // The final protection drops only after files and all mapped views.
    _lock: Option<WriterLock>,
}

impl Protection {
    pub(super) fn status(&self) -> Result<AppendOnlySessionStatus> {
        self.signal
            .lock()
            .map(|s| *s)
            .map_err(|_| Error::AppendOnlySessionPoisoned)
    }

    pub(super) fn wait(
        &self,
        observed: AppendOnlySessionStatus,
        timeout: Duration,
    ) -> Result<AppendOnlySessionStatus> {
        std::time::Instant::now().checked_add(timeout).ok_or(
            Error::ResourceArithmeticOverflow {
                resource: "append-only wait deadline",
            },
        )?;
        let state = self
            .signal
            .lock()
            .map_err(|_| Error::AppendOnlySessionPoisoned)?;
        self.changed
            .wait_timeout_while(state, timeout, |state| *state == observed)
            .map(|(state, _)| *state)
            .map_err(|_| Error::AppendOnlySessionPoisoned)
    }

    fn publish(&self, change: impl FnOnce(&mut AppendOnlySessionStatus)) -> Result<()> {
        let mut state = self
            .signal
            .lock()
            .map_err(|_| Error::AppendOnlySessionPoisoned)?;
        change(&mut state);
        state.revision = state.revision.saturating_add(1);
        self.changed.notify_all();
        Ok(())
    }

    pub(super) fn reader(protection: &Arc<Self>) -> Result<Arc<ReaderProtection>> {
        if !protection.gate.try_reader() {
            return Err(Error::AppendOnlySessionBusy);
        }
        // Dropping this value also unregisters when its fallible allocation fails.
        Ok(try_arc(ReaderProtection {
            session: Arc::clone(protection),
        })?)
    }
}

#[derive(Debug)]
pub(super) struct ReaderProtection {
    pub(super) session: Arc<Protection>,
}

impl Drop for ReaderProtection {
    fn drop(&mut self) {
        self.session.gate.release_reader();
    }
}

struct RecoveryExclusion<'a>(&'a reader_gate::ReaderGate);
impl Drop for RecoveryExclusion<'_> {
    fn drop(&mut self) {
        self.0.end_recovery();
    }
}

struct RecoveryAttempt<'a> {
    authority: &'a WriteAuthority,
    completed: bool,
    _exclusion: RecoveryExclusion<'a>,
}

impl Drop for RecoveryAttempt<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut slot = self
            .authority
            .slot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        slot.recovering = false;
        slot.recovery_required = true;
        let epoch = slot.epoch;
        let _ = self.authority.protection.publish(|status| {
            status.writer = AppendOnlyWriterState::Closed;
            status.recovery_required = true;
            status.last_error = Some(AppendOnlyFailure {
                operation: "recovery",
                category: "recovery interrupted",
                io_kind: None,
                raw_os_error: None,
                committed_sequence: None,
                writer_epoch: epoch,
            });
        });
    }
}

#[derive(Debug)]
struct WriterSlot {
    file: Option<VarlitFile>,
    active: bool,
    recovering: bool,
    finished: bool,
    recovery_required: bool,
    epoch: u64,
    parent_sync_pending: bool,
}

#[derive(Debug)]
pub(super) struct WriteAuthority {
    slot: Mutex<WriterSlot>,
    pub(super) protection: Arc<Protection>,
}

impl WriteAuthority {
    fn slot(&self) -> Result<MutexGuard<'_, WriterSlot>> {
        self.slot
            .lock()
            .map_err(|_| Error::AppendOnlySessionPoisoned)
    }

    pub(super) fn failure(
        &self,
        operation: &'static str,
        error: &Error,
        poisoned: bool,
    ) -> Result<()> {
        let (mut slot, coordination_poisoned) = match self.slot.lock() {
            Ok(slot) => (slot, false),
            Err(error) => (error.into_inner(), true),
        };
        slot.recovery_required |= poisoned || coordination_poisoned;
        let epoch = slot.epoch;
        let recovery_required = slot.recovery_required;
        let failure = summarize(operation, error, epoch);
        self.protection.publish(|status| {
            status.recovery_required = recovery_required;
            status.last_error = Some(failure);
        })?;
        if coordination_poisoned {
            Err(Error::AppendOnlySessionPoisoned)
        } else {
            Ok(())
        }
    }

    pub(super) fn return_writer(&self, mut file: VarlitFile) -> Result<()> {
        let unwinding = std::thread::panicking();
        if unwinding {
            file.poison.poison();
        }
        let pending = file.has_uncommitted_since_last_commit();
        let poisoned = file.poison.is_refusing();
        let mut slot = match self.slot.lock() {
            Ok(slot) => slot,
            Err(error) => {
                let mut slot = error.into_inner();
                slot.recovery_required = true;
                slot.active = false;
                slot.file = Some(file);
                let _ = self.protection.publish(|status| {
                    status.writer = AppendOnlyWriterState::Closed;
                    status.recovery_required = true;
                });
                return Err(Error::AppendOnlySessionPoisoned);
            }
        };
        slot.active = false;
        slot.parent_sync_pending = file.pending_pathname_parent_sync;
        slot.recovery_required |= poisoned || pending;
        slot.file = Some(file);
        let recovery_required = slot.recovery_required;
        let epoch = slot.epoch;
        self.protection.publish(|status| {
            status.writer = AppendOnlyWriterState::Closed;
            status.recovery_required = recovery_required;
            if unwinding || (pending && !poisoned) {
                status.last_error = Some(AppendOnlyFailure {
                    operation: "writer close",
                    category: if unwinding {
                        "writer panicked"
                    } else {
                        "uncommitted tail"
                    },
                    io_kind: None,
                    raw_os_error: None,
                    committed_sequence: None,
                    writer_epoch: epoch,
                });
            }
        })?;
        if recovery_required {
            Err(Error::AppendOnlyRecoveryRequired)
        } else {
            Ok(())
        }
    }
}

impl Drop for WriteAuthority {
    fn drop(&mut self) {
        let slot = self
            .slot
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        // No writing authority remains. Free its index/encoding buffers even if
        // read-only tokens or mappings retain the much smaller protection.
        let pending = slot.file.as_ref().is_some_and(|file| {
            file.poison.is_refusing() || file.has_uncommitted_since_last_commit()
        });
        slot.recovery_required |= pending;
        let failure = slot.file.take().and_then(|file| finish_file(file).err());
        slot.recovery_required |= failure.is_some();
        let facts = failure
            .as_ref()
            .map(|error| summarize("session drop", error, slot.epoch));
        let recovery_required = slot.recovery_required;
        // No observer can appear after the last write authority disappears.
        // A unique protection needs no notification. In particular, a failed
        // authority allocation must not lazily allocate a pthread mutex during
        // cleanup on platforms whose std mutex initializes on its first lock.
        if Arc::get_mut(&mut self.protection).is_some() {
            return;
        }
        let _ = self.protection.publish(|status| {
            status.writer = AppendOnlyWriterState::Finished;
            status.recovery_required = recovery_required;
            if let Some(failure) = facts {
                status.last_error = Some(failure);
            }
        });
    }
}

/// Pinned native file authority independent of any active writer handle.
///
/// Create readers before the first writer, retain their views across healthy
/// writer close/reconnect, and observe failure/termination separately from data.
/// Clones share one writer slot; reader tokens grant no write authority.
#[derive(Clone, Debug)]
pub struct AppendOnlySession {
    authority: Arc<WriteAuthority>,
}

pub(super) fn session_from_writer(authority: &Arc<WriteAuthority>) -> AppendOnlySession {
    AppendOnlySession {
        authority: Arc::clone(authority),
    }
}

impl AppendOnlySession {
    /// Admits an exclusively controlled, already opened native writer.
    ///
    /// Initial native open/recovery must complete before capabilities are issued.
    /// The file is parked until `open_writer`; no active writer is required to
    /// open the session's protected readers.
    ///
    /// # Safety
    /// Exclude nonparticipating writes, truncation and storage invalidation for
    /// the lifetime of every derived reader, token, mapping and borrowed slice,
    /// including after all session/writer handles are dropped. Advisory locks
    /// coordinate participating Varlit entry points only.
    pub unsafe fn new(mut file: VarlitFile) -> Result<Self> {
        validate_admission(&file)?;
        let identity = file.file.object_identity_array()?;
        let mut protection = try_arc(Protection {
            spec: file.spec,
            path: file.path.try_clone()?,
            identity,
            gate: reader_gate::ReaderGate::new(),
            signal: Mutex::new(AppendOnlySessionStatus {
                writer: AppendOnlyWriterState::NotStarted,
                recovery_required: false,
                writer_epoch: 0,
                revision: 0,
                last_error: None,
            }),
            changed: Condvar::new(),
            _lock: None,
        })?;
        Arc::get_mut(&mut protection)
            .expect("new protection is unique")
            ._lock = file._lock.take();
        let parent_sync_pending = file.pending_pathname_parent_sync;
        let authority = try_arc(WriteAuthority {
            slot: Mutex::new(WriterSlot {
                file: Some(file),
                active: false,
                recovering: false,
                finished: false,
                recovery_required: false,
                epoch: 0,
                parent_sync_pending,
            }),
            protection,
        })?;
        Ok(Self { authority })
    }

    /// Grants read-only capability before, during or after a writer's lifetime.
    pub fn reader_token(&self) -> AppendOnlyReaderToken {
        AppendOnlyReaderToken {
            protection: Arc::clone(&self.authority.protection),
        }
    }

    /// Acquires the single writer slot without reopening or rescanning the file.
    /// Refuses active writers, unfinished transactions and recovery failures.
    pub fn open_writer(&self) -> Result<AppendOnlyWriter> {
        let mut slot = self.authority.slot()?;
        if slot.finished {
            return Err(Error::AppendOnlySessionClosed);
        }
        if slot.recovering {
            return Err(Error::AppendOnlySessionBusy);
        }
        if slot.active {
            return Err(Error::AppendOnlyWriterActive);
        }
        if slot.recovery_required {
            return Err(Error::AppendOnlyRecoveryRequired);
        }
        let epoch = slot
            .epoch
            .checked_add(1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "append-only writer epoch",
            })?;
        let file = slot.file.take().ok_or(Error::AppendOnlyRecoveryRequired)?;
        slot.active = true;
        slot.epoch = epoch;
        if let Err(error) = self.authority.protection.publish(|status| {
            status.writer = AppendOnlyWriterState::Active;
            status.writer_epoch = epoch;
        }) {
            slot.active = false;
            slot.file = Some(file);
            slot.recovery_required = true;
            return Err(error);
        }
        Ok(AppendOnlyWriter {
            file: Some(file),
            authority: Arc::clone(&self.authority),
        })
    }

    /// Snapshot writer lifetime and the last reported failure without file I/O.
    pub fn status(&self) -> Result<AppendOnlySessionStatus> {
        self.authority.protection.status()
    }

    /// Wait for a different status snapshot or timeout; does not wait for data.
    pub fn wait_for_change(
        &self,
        observed: AppendOnlySessionStatus,
        timeout: Duration,
    ) -> Result<AppendOnlySessionStatus> {
        self.authority.protection.wait(observed, timeout)
    }

    /// Rebuilds writer state under the existing native object lock.
    ///
    /// This may truncate an unpublished tail. It refuses while any protected
    /// reader or derived mapping remains, without waiting for them. Tokens and
    /// session handles may remain; new reader registration is excluded atomically
    /// until recovery finishes. A failed attempt remains retryable/observable.
    pub fn recover(&self) -> Result<()> {
        self.recover_using(|spec, path, file| {
            let mut scratch = BufferedScan::new(spec);
            VarlitFile::open_writer_file_with_scratch(spec, path, file, &mut scratch)
        })
    }

    fn recover_using(
        &self,
        rebuild: impl FnOnce(FormatSpec, PathBuf, File) -> Result<VarlitFile>,
    ) -> Result<()> {
        let protection = &self.authority.protection;
        let mut slot = self.authority.slot()?;
        if slot.finished {
            return Err(Error::AppendOnlySessionClosed);
        }
        if slot.active {
            return Err(Error::AppendOnlyWriterActive);
        }
        if slot.recovering {
            return Err(Error::AppendOnlySessionBusy);
        }
        if !protection.gate.try_recovery() {
            return Err(Error::AppendOnlyReadersActive);
        }
        let exclusion = RecoveryExclusion(&protection.gate);
        let was_required = slot.recovery_required;
        let parent_sync_pending = slot.parent_sync_pending;
        slot.recovering = true;
        slot.recovery_required = true;
        let notification = protection.publish(|status| {
            status.writer = AppendOnlyWriterState::Recovering;
            status.recovery_required = true;
        });
        drop(slot);
        let mut attempt = RecoveryAttempt {
            authority: &self.authority,
            completed: false,
            _exclusion: exclusion,
        };
        notification?;
        let mut native_started = false;
        let result = (|| {
            let native = protection
                ._lock
                .as_ref()
                .and_then(|lock| lock.native_guard.as_ref())
                .ok_or(Error::AppendOnlySessionPoisoned)?;
            let file = native.try_clone()?;
            if super::super::publication::opened_file_identity_array(&file)? != protection.identity
            {
                return Err(Error::InvalidFormatSpec(
                    "append-only recovery backing object changed",
                ));
            }
            let path = protection.path.try_clone()?;
            let old = self.authority.slot()?.file.take();
            native_started = true;
            // Preserve dirty-tail evidence, rather than clearing it while
            // destroying the superseded writer state before recovery.
            if let Some(mut old) = old {
                old.mode = OpenMode::ReadOnly;
                drop(old);
            }
            rebuild(protection.spec, path, file)
        })();
        let mut slot = self.authority.slot()?;
        slot.recovering = false;
        let outcome = match result {
            Ok(mut file) => {
                file.pending_pathname_parent_sync |= parent_sync_pending;
                slot.file = Some(file);
                slot.recovery_required = false;
                protection.publish(|status| {
                    status.writer = AppendOnlyWriterState::Closed;
                    status.recovery_required = false;
                })
            }
            Err(error) => {
                slot.recovery_required = was_required || native_started;
                let required = slot.recovery_required;
                let failure = summarize("recovery", &error, slot.epoch);
                let notification = protection.publish(|status| {
                    status.writer = AppendOnlyWriterState::Closed;
                    status.recovery_required = required;
                    status.last_error = Some(failure);
                });
                notification.and(Err(error))
            }
        };
        drop(slot);
        // A typed native failure has already restored a stable, retryable slot.
        // Coordination poisoning still needs the fail-closed unwind guard.
        if !matches!(outcome, Err(Error::AppendOnlySessionPoisoned)) {
            attempt.completed = true;
        }
        outcome
    }

    /// Permanently revokes writer issuance. Does not commit pending data.
    /// Existing readers/maps remain valid and retain native writer exclusion.
    pub fn close(&self) -> Result<()> {
        let mut slot = self.authority.slot()?;
        if slot.active {
            return Err(Error::AppendOnlyWriterActive);
        }
        if slot.recovering {
            return Err(Error::AppendOnlySessionBusy);
        }
        if slot.finished {
            return Ok(());
        }
        slot.finished = true;
        let pending = slot.file.as_ref().is_some_and(|file| {
            file.poison.is_refusing() || file.has_uncommitted_since_last_commit()
        });
        slot.recovery_required |= pending;
        let required = slot.recovery_required;
        let file = slot.file.take();
        self.authority.protection.publish(|status| {
            status.writer = AppendOnlyWriterState::Finished;
            status.recovery_required = required;
        })?;
        drop(slot);
        if let Some(file) = file
            && let Err(error) = finish_file(file)
        {
            let _ = self.authority.failure("session close", &error, true);
            return Err(error);
        }
        Ok(())
    }
}

// Native Drop is best effort. Perform its close explicitly so retained readers
// can observe cleanup failure even after the last write authority disappears.
fn finish_file(mut file: VarlitFile) -> Result<()> {
    let result = file.mark_writer_closed();
    file.mode = OpenMode::ReadOnly;
    drop(file);
    result
}

fn validate_admission(file: &VarlitFile) -> Result<()> {
    if file.mode != OpenMode::ReadWrite
        || file._lock.as_ref().is_none_or(|l| l.native_guard.is_none())
    {
        return Err(Error::InvalidFormatSpec(
            "append-only admission requires a locked writer",
        ));
    }
    if file.poison.is_refusing() {
        return Err(Error::WriterPoisoned("append-only admission"));
    }
    if file.matrix.is_some() || file.spec.has_matrix_blocks() || file.spec.growing_matrix.is_some()
    {
        return Err(Error::InvalidFormatSpec(
            "append-only mappings do not support matrix storage",
        ));
    }
    if !file.spec.commit_policy.is_transaction_marker() {
        return Err(Error::InvalidFormatSpec(
            "append-only mappings require transaction markers",
        ));
    }
    let native = file
        ._lock
        .as_ref()
        .and_then(|l| l.native_guard.as_ref())
        .unwrap();
    if super::super::publication::opened_file_identity_array(native)?
        != file.file.object_identity_array()?
    {
        return Err(Error::InvalidFormatSpec(
            "append-only writer lock protects another object",
        ));
    }
    Ok(())
}

fn summarize(operation: &'static str, error: &Error, epoch: u64) -> AppendOnlyFailure {
    let mut facts = AppendOnlyFailure {
        operation,
        category: "operation failed",
        io_kind: None,
        raw_os_error: None,
        committed_sequence: None,
        writer_epoch: epoch,
    };
    let mut current = error;
    // Public encoders can return arbitrarily nested errors; do not recurse.
    for _ in 0..8 {
        match current {
            Error::Io(source) => {
                facts.io_kind = Some(source.kind());
                facts.raw_os_error = source.raw_os_error();
                break;
            }
            Error::WriteRollbackFailed { source, .. } => {
                facts.category = "append rollback failed";
                facts.io_kind = Some(source.kind());
                facts.raw_os_error = source.raw_os_error();
                break;
            }
            Error::CommittedButDurabilityUnproven { sequence, source } => {
                facts.category = "commit durability unconfirmed";
                facts.committed_sequence = Some(*sequence);
                current = source;
            }
            Error::PublishedButParentSyncPending { source, .. } => {
                facts.category = "parent-directory durability unconfirmed";
                current = source;
            }
            Error::ErrorContextAllocationFailed {
                context: crate::AllocationFailureContext::Committed { sequence },
            } => {
                facts.category = "commit durability unconfirmed";
                facts.committed_sequence = Some(*sequence);
                break;
            }
            Error::AllocationFailed { .. } => {
                facts.category = "allocation refused";
                break;
            }
            Error::LimitExceeded { .. } => {
                facts.category = "resource limit exceeded";
                break;
            }
            _ => break,
        }
    }
    facts
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
