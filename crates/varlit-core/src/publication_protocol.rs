//! Shared control flow for publication. I/O implementations retain ownership of
//! encoding, validation and system calls; verification supplies adversarial I/O.
//! Generic callbacks are statically dispatched and allocate no protocol state.

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

/// Recovery: cut what follows the kept prefix, make the file durable, then
/// publish the kept end. Each step gets `context`, the file they all act on.
#[inline]
pub(crate) fn restore<C, E, T>(
    context: &mut C,
    truncate: impl FnOnce(&mut C) -> Result<(), E>,
    sync: impl FnOnce(&mut C) -> Result<(), E>,
    publish: impl FnOnce(&mut C) -> Result<T, E>,
) -> Result<T, E> {
    truncate(context)?;
    sync(context)?;
    publish(context)
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

/// The published-end slot a publication writes when `newest` is the
/// sequence number of the newest one: the other slot, so a torn write leaves
/// the newest end standing.
#[inline]
pub(crate) const fn slot_after(newest: u64) -> usize {
    (newest.wrapping_add(1) % 2) as usize
}

#[cfg(kani)]
mod proofs;
