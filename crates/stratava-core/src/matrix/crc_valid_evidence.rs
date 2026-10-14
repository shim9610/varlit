use super::{
    BitmapByteUpdate, BitmapRead, Error, PageResidencyDelta, PreparedByteWrite, Result,
    SparseBitmap, page_index::PageIndexMirror, page_index_enumeration::PageIndexEnumeration,
    prepare_bitmap_update,
};

/// A block's CRC-validity bitmap together with whether it is complete.
#[derive(Clone, Debug)]
pub struct CrcValidEvidence {
    bits: SparseBitmap,
    /// False when this block's validity page index could not be enumerated
    /// in full, so the bitmap is a *subset* of the published evidence and a
    /// clear bit outside the loaded pages means nothing.
    complete: bool,
}

/// Proof that the validity bitmap it borrows was enumerated in full.
///
/// The only way to obtain one is `CrcValidEvidence::complete`, which is
/// the refusal. There is no other constructor, and the borrowed field is
/// private, so this witness cannot be fabricated — see
/// `crates/stratava/tests/ui/fail_fabricated_crc_valid_completeness.rs`.
#[must_use = "the witness is the completeness proof; obtaining and dropping \
              it discards the only reason a clear bit may be read as absence"]
pub struct CompleteCrcValidEvidence<'a> {
    bits: &'a SparseBitmap,
}

impl CrcValidEvidence {
    /// Binds a loaded validity bitmap to the enumeration that produced it.
    ///
    /// Round 14 spelled this `new(bits, complete: bool)` with `pub(super)`
    /// visibility, so any line in `matrix.rs` could declare a bitmap
    /// complete and immediately mint the F-04 witness from it. The
    /// completeness argument is now a [`PageIndexEnumeration`], whose
    /// constructors are private to `mod page_index_enumeration` and are
    /// reached only by the enumerator itself. There is no signature in the
    /// crate that accepts completeness as a `bool`.
    pub(super) fn from_page_index_enumeration(
        bits: SparseBitmap,
        enumeration: PageIndexEnumeration,
    ) -> Self {
        Self {
            complete: enumeration.is_complete(),
            bits,
        }
    }

    /// Evidence for a bitmap that has published nothing yet.
    ///
    /// Used at creation, and — with `bit_count` zero — for a format without
    /// integrity, where no validity bitmap exists and no recovery path reads
    /// one. Completeness is vacuous rather than asserted: this constructor
    /// takes a *count*, not a bitmap, so it builds the empty map itself and
    /// cannot be used to launder a bitmap that came from somewhere else.
    pub(super) fn newly_created(bit_count: u64) -> Result<Self> {
        Ok(Self {
            bits: SparseBitmap::new(bit_count)?,
            complete: true,
        })
    }

    /// Test-only escape, named at the call site so it cannot be mistaken
    /// for the production route, and compiled out of every shipped build.
    /// It exists because the type's own contract
    /// (`crc_valid_evidence_tests`) has to be exercisable without a file.
    #[cfg(test)]
    pub(super) fn from_parts_for_tests_only(bits: SparseBitmap, complete: bool) -> Self {
        Self { bits, complete }
    }

    /// Proves the evidence complete, or refuses.
    ///
    /// This is the F-04 refusal, and it is deliberately unconditional —
    /// exactly like `poison_interrupted_rebuild`. `matrix_fatal_forensics`
    /// relaxes *reading* fatal state; consuming absent evidence as proof is
    /// a destructive republication, not a read. Discarding unverifiable
    /// visibility on purpose is a different operation from recovering it and
    /// has to be asked for by name rather than obtained by default.
    ///
    /// The refusal reuses [`Error::MatrixFatalCorruption`] because that is
    /// what this is — a fatal recovery finding blocking an operation. Its
    /// message's pointer at forensic access is unhelpful for a caller that
    /// already enabled it; a dedicated variant belongs in `error.rs`
    /// (checklist open item 16).
    pub(super) fn complete(&self, read: BitmapRead<'_>) -> Result<CompleteCrcValidEvidence<'_>> {
        if !self.complete {
            return Err(Error::MatrixFatalCorruption);
        }
        // OnDemand admission has not enumerated the directory. A destructive
        // recovery witness is issued only after streaming and cross-checking
        // all forward/reverse links in the active view.
        self.bits.visit_index(read, |_, _| Ok(()))?;
        Ok(CompleteCrcValidEvidence { bits: &self.bits })
    }

    pub(super) fn verify_directory_report(
        &self,
        read: BitmapRead<'_>,
        report: &mut super::MatrixRecoveryReport,
    ) -> Result<()> {
        super::verify_directory_report(&self.bits, read, report)
    }

    /// Fail-closed read: `Ok(())` when the cell's CRC is known meaningful,
    /// otherwise the caller's refusal.
    ///
    /// Safe on incomplete evidence *because it yields nothing*: an omitted
    /// page reads as "not established", which for this shape of caller means
    /// refuse, never publish. Callers that need the bit as data must go
    /// through [`Self::complete`].
    pub(super) fn require_meaningful(
        &self,
        ordinal: u64,
        absent: impl FnOnce() -> Error,
        read: BitmapRead<'_>,
    ) -> Result<()> {
        if self.bits.get(ordinal, read)? {
            Ok(())
        } else {
            Err(absent())
        }
    }

    /// Fail-closed range check: no validity bits escape as recovery evidence.
    pub(super) fn require_meaningful_range(
        &self,
        first: u64,
        count: u64,
        absent: impl FnOnce(u64) -> Result<Error>,
        read: BitmapRead<'_>,
    ) -> Result<()> {
        match self.bits.first_unset(first, count, read)? {
            Some(ordinal) => Err(absent(ordinal)?),
            None => Ok(()),
        }
    }

    /// Fail-closed selected-cell check; never exposes absence as recovery evidence.
    pub(super) fn require_meaningful_strided(
        &self,
        first: u64,
        step: u64,
        count: u64,
        absent: impl FnOnce(u64) -> Result<Error>,
        read: BitmapRead<'_>,
    ) -> Result<()> {
        match self.bits.first_unset_strided(first, step, count, read)? {
            Some(ordinal) => Err(absent(ordinal)?),
            None => Ok(()),
        }
    }

    pub(super) fn byte_len(&self) -> u64 {
        self.bits.byte_len
    }

    pub(super) fn page_count(&self) -> u64 {
        self.bits.page_count
    }

    pub(super) fn resident_bytes(&self) -> u64 {
        self.bits.resident_bytes()
    }

    pub(super) fn resident_index_bytes(&self) -> u64 {
        self.bits.resident_index_bytes()
    }

    /// Drops every page and the persisted-index tracking, mirroring what a
    /// whole-map clear does on disk.
    ///
    /// Completeness is deliberately *not* reset: a cleared map is not
    /// evidence that was re-enumerated, and a session that opened on a
    /// damaged validity index still has no basis for reading absence as
    /// proof. The state is fail-closed until the next open (checklist open
    /// item 24).
    pub(super) fn clear(&mut self) {
        self.bits.clear();
    }
    pub(super) fn rebind_backing(&mut self, source: &super::Source) {
        self.bits.rebind_backing(source);
    }

    pub(super) fn prepare_follow(
        &mut self,
        index: u64,
        changes: &super::generation_follow::Changes<'_>,
        budget: &mut super::ResidentBitmapBudget,
    ) -> Result<super::generation_follow::BitmapDelta> {
        if !self.complete {
            return Err(Error::MatrixFatalCorruption);
        }
        super::generation_follow::BitmapDelta::prepare(
            &mut self.bits,
            index,
            changes,
            budget,
            false,
        )
    }
    pub(super) fn apply_follow(&mut self, delta: super::generation_follow::BitmapDelta) {
        delta.apply(&mut self.bits);
    }

    pub(super) fn prepare_update(
        &mut self,
        bit_count: u64,
        base_offset: u64,
        ordinal: u64,
        value: bool,
        read: BitmapRead<'_>,
    ) -> Result<BitmapByteUpdate> {
        prepare_bitmap_update(&mut self.bits, bit_count, base_offset, ordinal, value, read)
    }

    pub(super) fn materialisation_cost(
        &mut self,
        index: u64,
        value: u8,
        read: BitmapRead<'_>,
    ) -> Result<u64> {
        self.bits.materialisation_cost(index, value, read)
    }

    pub(super) fn prepare_byte_write(
        &mut self,
        index: u64,
        value: u8,
        read: BitmapRead<'_>,
    ) -> Result<PreparedByteWrite> {
        self.bits.prepare_byte_write(index, value, read)
    }

    pub(super) fn commit_byte_write(&mut self, prepared: PreparedByteWrite) -> PageResidencyDelta {
        self.bits.commit_byte_write(prepared)
    }

    /// The one named escape, narrowed to a compiler refusal (round 16).
    ///
    /// It used to return `&mut SparseBitmap`, held to one call site by a
    /// source gate. That is not the same thing as holding it to one
    /// *operation*: re-verification wrote
    /// `*evidence.page_index_mirror_mut() = attacker_bits;` — a whole-map
    /// replacement that leaves `complete` true, so
    /// `CompleteCrcValidEvidence::get` then reports bits no enumeration
    /// produced — and `.page_index_mirror_mut().set(ordinal, true)` in
    /// place, both from inside this file, and ran the first one to a bit
    /// read back through the F-04 witness.
    ///
    /// The return type is now [`PageIndexMirror`](super::page_index::PageIndexMirror),
    /// whose borrow is private to `mod page_index`. Assignment through it
    /// is a type error and `SparseBitmap`'s mutators are not reachable on
    /// it; the only things it can do are the two index-maintenance
    /// operations that made the escape necessary. The call-site count is
    /// kept as a second line of defence, not as the property.
    pub(super) fn page_index_mirror_mut(&mut self) -> PageIndexMirror<'_> {
        PageIndexMirror::new(&mut self.bits)
    }
}

impl CompleteCrcValidEvidence<'_> {
    /// Reads a validity bit as evidence, which the witness makes sound.
    pub(super) fn get(&self, ordinal: u64, read: BitmapRead<'_>) -> Result<bool> {
        self.bits.get(ordinal, read)
    }
}
