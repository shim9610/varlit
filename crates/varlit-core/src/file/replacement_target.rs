use super::{Error, KeyedTails, Result, VarlitBlock};

#[derive(Clone, Copy, Debug)]
pub struct ReplacementTarget {
    position: usize,
}

impl ReplacementTarget {
    /// Selects the `ordinal`-th record of `T::ID` and refuses a stored
    /// `block_version` that is not `T::VERSION`.
    ///
    /// This is the only way to build a [`ReplacementTarget`].
    /// Takes the index itself, not a slice of it.
    ///
    /// What this needs is a forward scan that stops at the nth match and
    /// then one positional read — both of which a demand-filled index can
    /// serve. Asking for `&[RecordIndexEntry]` asked for every entry at
    /// once, which is the shape that keeps the index resident.
    pub(super) fn resolve<T: VarlitBlock>(
        index: &super::ResidentIndex,
        ordinal: usize,
    ) -> Result<Self> {
        Self::resolve_internal(index, T::ID, T::VERSION, ordinal)
    }

    /// The same selection and the same version refusal, for a block varlit
    /// writes itself and no `VarlitBlock` type names.
    ///
    /// [`Self::resolve`] is this function with `T::ID` and `T::VERSION`
    /// substituted, so there is one implementation of "find the nth record
    /// of a block and refuse a stored version that is not the expected
    /// one", not two that could drift.
    ///
    /// **Why a second constructor is not a second write route.** The
    /// property `ReplacementTarget` exists to enforce is that the bytes of
    /// an already-indexed record are reachable only by consuming one of
    /// these, and only through `RecordFile::overwrite_indexed_record`. That
    /// is unchanged: this returns the same opaque token, having done the
    /// same refusal. What it does not do is decide *which* internal blocks
    /// may be rewritten — its one caller today is the matrix chunk, and a
    /// caller for anything else has to argue for itself, exactly as this
    /// one did.
    pub(super) fn resolve_internal(
        index: &super::ResidentIndex,
        block_id: u32,
        block_version: u16,
        ordinal: usize,
    ) -> Result<Self> {
        let mut found = None;
        let mut matches_seen = 0usize;
        for (candidate, entry) in index.iter().enumerate() {
            if entry?.block_id != block_id {
                continue;
            }
            if matches_seen == ordinal {
                found = Some(candidate);
                break;
            }
            matches_seen += 1;
        }
        let position = found.ok_or(Error::UnexpectedEof)?;
        let actual = index.entry_at(position)?.block_version;
        if actual != block_version {
            return Err(Error::BlockVersionMismatch {
                block_id,
                expected: block_version,
                actual,
            });
        }
        Ok(Self { position })
    }

    /// The resident-index position of the version-checked target.
    pub(super) fn position(self) -> usize {
        self.position
    }
}

/// Mechanical enforcement for F-02: the permission to rewrite the bytes of
/// an already-indexed record, which cannot be obtained without dropping the
/// block's resident keyed-tail map first.
///
/// The cache maps canonical key bytes to tail offsets and maintained keyed
/// appends read their predecessor from it, so a record whose stored bytes
/// change may no longer carry the key the cache filed it under. Round 12
/// put that invalidation inside the writing function, which made it a
/// property of *today's* single caller rather than of the operation: a new
/// seek/write pair elsewhere in the file skipped it and compiled.
///
/// Now the invalidation happens in the constructor of the permission, and
/// the permission is consumed by value by the only function that can write
/// those bytes. It is infallible and allocation-free (a `HashMap::remove`),
/// so it adds no fallible step in either direction of invariant 3, and it
/// runs strictly before the first byte, so it also covers a half-completed
/// write, a poisoned writer inspected afterwards, and a caller that
/// violates the same-key contract of the `unsafe` in-place entry point.
#[derive(Debug)]
pub struct RecordOverwrite {
    record_offset: u64,
    payload_offset: u64,
}

impl RecordOverwrite {
    /// Consumes a version-checked [`ReplacementTarget`], drops the target
    /// block's keyed-tail map, and yields the write permission.
    ///
    /// This is the only way to build a [`RecordOverwrite`], and a
    /// [`ReplacementTarget`] is the only way to call it.
    /// One positional read, so it takes the index rather than a slice.
    pub(super) fn prepare(
        target: ReplacementTarget,
        index: &super::ResidentIndex,
        keyed_tails: &mut KeyedTails,
    ) -> Result<Self> {
        let entry = index.entry_at(target.position())?;
        let record_offset = entry.record_offset;
        let payload_offset = entry.payload_offset;
        keyed_tails.invalidate(entry.block_id);
        Ok(Self {
            record_offset,
            payload_offset,
        })
    }

    pub(super) fn record_offset(&self) -> u64 {
        self.record_offset
    }

    pub(super) fn payload_offset(&self) -> u64 {
        self.payload_offset
    }
}
