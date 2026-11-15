//! Shared control flow for publication. I/O implementations retain ownership of
//! encoding, validation and system calls; verification supplies adversarial I/O.
//! Generic callbacks are statically dispatched and allocate no protocol state.

#[derive(Clone, Copy)]
pub(crate) enum CheckpointKind {
    Confirmed,
    Working,
    Dirty,
}

#[derive(Clone, Copy)]
pub(crate) enum CheckpointStep {
    Body,
    /// A barrier between a body and the head naming it: the two-barrier
    /// confirmation of a page-reusing sidecar.
    BodySync,
    SelectHead,
    Head,
    HeadSync,
}

#[inline]
pub(crate) fn checkpoint<E>(
    kind: CheckpointKind,
    mut io: impl FnMut(CheckpointStep) -> Result<(), E>,
) -> Result<(), E> {
    // One barrier per confirmed publication. The confirmed head names the
    // checksum of every sidecar byte appended since the previous confirmed
    // checkpoint, so a head that reached storage without its generation is
    // detected and the other (older, durable) confirmed slot is used. Dirty and
    // working checkpoints are not barriers: a crashed generation is recovered
    // from the newest confirmed checkpoint, whatever reached storage after it.
    io(CheckpointStep::Body)?;
    io(CheckpointStep::SelectHead)?;
    io(CheckpointStep::Head)?;
    if matches!(kind, CheckpointKind::Confirmed) {
        io(CheckpointStep::HeadSync)?;
    }
    Ok(())
}

/// [`checkpoint`] for a sidecar whose confirmed body is not covered by a
/// checksum in its head: a confirmation syncs the body before the head is
/// written, so a head on storage always names a body on storage. Working and
/// dirty publications remain barrier-free.
#[inline]
pub(crate) fn checkpoint_with_body_barrier<E>(
    kind: CheckpointKind,
    mut io: impl FnMut(CheckpointStep) -> Result<(), E>,
) -> Result<(), E> {
    io(CheckpointStep::Body)?;
    if matches!(kind, CheckpointKind::Confirmed) {
        io(CheckpointStep::BodySync)?;
    }
    io(CheckpointStep::SelectHead)?;
    io(CheckpointStep::Head)?;
    if matches!(kind, CheckpointKind::Confirmed) {
        io(CheckpointStep::HeadSync)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum CommitStep {
    Prepare,
    PrimarySync,
    Publish,
    Poison,
    Finish,
}

#[inline]
pub(crate) fn commit<E>(mut io: impl FnMut(CommitStep) -> Result<(), E>) -> Result<(), E> {
    io(CommitStep::Prepare)?;
    io(CommitStep::PrimarySync)?;
    if let Err(error) = io(CommitStep::Publish) {
        // The publication may already be visible even though I/O reported an
        // error. Poison and Finish are infallible in the production adapter.
        let _ = io(CommitStep::Poison);
        return Err(error);
    }
    io(CommitStep::Finish)
}

#[inline]
pub(crate) fn restore<E, T>(
    truncate: impl FnOnce() -> Result<(), E>,
    sync: impl FnOnce() -> Result<(), E>,
    publish: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    truncate()?;
    sync()?;
    publish()
}

#[inline]
pub(crate) fn follow<C, T, E, R>(
    current: &mut C,
    prepare: impl FnOnce(&C) -> Result<T, E>,
    adopt: impl FnOnce(&mut C, T) -> R,
) -> Result<R, E> {
    let next = prepare(current)?;
    Ok(adopt(current, next))
}

/// Preserve the newest prior anchor while replacing the older copy first.
#[inline]
pub(crate) fn older_head(first: u64, second: u64) -> usize {
    usize::from(first > second)
}

/// Matching valid copies imply that the first copy was synced before the second
/// was written. Otherwise a restarted writer must reconcile uncertain durability.
#[inline]
pub(crate) fn resume<E>(
    first: Option<(u64, u64)>,
    second: Option<(u64, u64)>,
    sync: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    if first.is_none() || first != second {
        sync()?;
    }
    Ok(())
}

#[cfg(kani)]
mod proofs;
