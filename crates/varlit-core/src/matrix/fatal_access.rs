use super::{
    Error, FormatSpec, MatrixAuxLayout, MatrixBlockLayout, MatrixCommitLayout,
    MatrixCorruptionSeverity, MatrixDimensionValue, MatrixLayout, MatrixRecoveryFinding,
    MatrixStorageDiagnostics, ReadLimits, Result,
};

/// Witness that no `Fatal` recovery finding blocks this access.
///
/// Zero-sized, private field, single constructor. Held by reference by the
/// resolvers, because one check legitimately covers the whole operation
/// that follows it.
#[derive(Debug)]
pub struct FatalAccessAllowed(());

/// Whether safe access to this layout's state is blocked by a `Fatal`
/// recovery finding, and the sole source of [`FatalAccessAllowed`].
///
/// **Deliberately not `Clone` (round 16).** Re-verification found that
/// `layout.fatal_access = donor.fatal_access.clone();` — one line, with any
/// `&mut MatrixLayout` in scope — copied an unblocked gate from a healthy
/// layout onto a blocked one, undoing `FatalAccessGate::block`. A gate
/// that can be copied out of one layout is a gate that can be installed
/// into another, so the derive is the bypass. `MatrixLayout` is therefore
/// not `Clone` either; nothing in the crate cloned it.
#[derive(Debug)]
pub struct FatalAccessGate {
    blocked: bool,
}

impl FatalAccessGate {
    /// The only constructor, and it **is** the classification.
    ///
    /// Round 14 had `new(blocked: bool)`, which let any line in `matrix.rs`
    /// choose the answer. The decision is now made here, from the recovery
    /// findings the open path actually collected and the spec's declared
    /// forensic opt-in, so there is no signature anywhere that accepts
    /// "this gate permits" as an argument.
    ///
    /// `findings` must be every finding the layout will carry — the CRC
    /// findings and the per-commit quarantine findings.
    ///
    /// **Private to this module (round 16).** `pub(super)` made it
    /// file-visible, and a file-visible function returning a
    /// `FatalAccessGate` is a file-visible way to *write* one:
    /// `layout.fatal_access = FatalAccessGate::evaluate(clean.iter(), spec);`
    /// is a single field assignment — `MatrixLayout`'s fields are
    /// file-private — and it undoes [`FatalAccessGate::block`]. The rule
    /// that closes it is structural rather than a call-site count: **no
    /// expression of type `FatalAccessGate` exists outside this module**.
    /// The only caller is [`assemble_layout`], which builds the whole
    /// layout around the gate it derives, so a gate can be created only
    /// together with the state it speaks for.
    fn evaluate<'a>(
        findings: impl IntoIterator<Item = &'a MatrixRecoveryFinding>,
        spec: FormatSpec,
    ) -> Self {
        let has_fatal_finding = findings
            .into_iter()
            .any(|finding| finding.severity == MatrixCorruptionSeverity::Fatal);
        Self {
            blocked: has_fatal_finding && !spec.matrix_fatal_forensics,
        }
    }

    /// Blocks safe access from here on. Not reversible: a rebuild that was
    /// interrupted after marking the persisted page index leaves state no
    /// safe accessor may consume, and reopening with forensic access is the
    /// deliberate, explicitly named route to reading it anyway.
    pub(crate) fn block(&mut self) {
        self.blocked = true;
    }

    /// Reads the decision without addressing a layout. Test-only, named to
    /// say so, and compiled out of every shipped build: the production
    /// route to the answer is [`allow_for`], which needs the layout.
    #[cfg(test)]
    pub(super) fn is_blocked_for_tests(&self) -> bool {
        self.blocked
    }
}

/// The fail-closed refusal, and the only way to obtain the witness.
///
/// Takes the layout rather than `&self` on the gate on purpose: the witness
/// then speaks for the state the caller is about to address, and a gate
/// built anywhere else has nothing to say. This mirrors
/// `GuardedWriter::writer_permit` in `writer_permit.rs`, which was given
/// the same shape in round 14 for the same reason.
pub(super) fn allow_for(layout: &MatrixLayout) -> Result<FatalAccessAllowed> {
    if layout.fatal_access.blocked {
        return Err(Error::MatrixFatalCorruption);
    }
    Ok(FatalAccessAllowed(()))
}

/// The one place a [`MatrixLayout`] — and therefore the one place a
/// [`FatalAccessGate`] — comes into existence.
///
/// Round 15 left the classification inside this module but the *literal*
/// outside it, which meant a gate value crossed the module boundary and
/// could be assigned to any layout's field. The literal moves in here
/// instead: `evaluate` is private, this function returns a whole layout
/// rather than a gate, and the gate it installs is derived from the
/// findings that same layout is being built with. There is no signature in
/// the crate that yields a `FatalAccessGate` on its own, so
/// `layout.fatal_access = ...` has nothing to assign.
///
/// The findings consulted are every finding the layout will carry: the CRC
/// findings vector it is given, and the quarantine finding of each commit
/// category, which by this point holds every entry the caller's
/// `commit_findings` map had (`layout_from_parts` refuses an undrained
/// map).
///
/// The residual, stated rather than implied: a caller inside `matrix.rs`
/// can still *call this function*, and gets back a layout whose gate is
/// unblocked when it passes no findings. That buys nothing it did not
/// already have — it must supply the twelve fields of a whole layout,
/// including the blocks and commits whose state it wants to address, which
/// is to say it must supply the state itself. What it cannot do is take the
/// gate off a layout it built and put it on one it did not.
#[allow(clippy::too_many_arguments)]
pub(super) fn assemble_layout(
    dimensions: Vec<MatrixDimensionValue>,
    commits: Vec<MatrixCommitLayout>,
    blocks: Vec<MatrixBlockLayout>,
    aux: Vec<MatrixAuxLayout>,
    crc_findings: Vec<MatrixRecoveryFinding>,
    append_log_start: u64,
    read_limits: ReadLimits,
    resident_bitmap_bytes: u64,
    resident_page_index_bytes: u64,
    spec: FormatSpec,
) -> MatrixLayout {
    let fatal_access = FatalAccessGate::evaluate(
        crc_findings.iter().chain(
            commits
                .iter()
                .filter_map(|commit| commit.quarantine_finding.as_ref()),
        ),
        spec,
    );
    let layout = MatrixLayout {
        storage: MatrixStorageDiagnostics::default(),
        dimensions,
        commits,
        blocks,
        aux,
        crc_findings,
        append_log_start,
        read_limits,
        resident_bitmap_bytes,
        resident_page_index_bytes,
        fatal_access,
    };
    super::record_open_resident_bitmap_bytes(&layout);
    layout
}

/// Copies a whole layout, decision included — the mmap snapshot path
/// ([`RecordFile::mmap_matrix`](crate::file::RecordFile)) needs a layout it
/// owns.
///
/// This is what `#[derive(Clone)]` used to do, minus the part that made it
/// a bypass. The derive required `FatalAccessGate: Clone`, and a clonable
/// gate can be lifted off a healthy layout and dropped onto a blocked one.
/// Copying a *layout* cannot launder anything: the result carries the
/// blocks, commits and findings it was copied from, so its gate still
/// speaks for the state beside it. The only thing a caller can do with this
/// is address the copy, which is addressing the original.
///
/// Partial moves are what would break that, and they are refused by the
/// `Drop` impl on `MatrixLayout`: `donor.clone().fatal_access` — moving the
/// gate out of the temporary — is E0509, not a one-liner.
pub(super) fn clone_layout(layout: &MatrixLayout) -> MatrixLayout {
    MatrixLayout {
        storage: layout.storage,
        dimensions: layout.dimensions.clone(),
        commits: layout.commits.clone(),
        blocks: layout.blocks.clone(),
        aux: layout.aux.clone(),
        crc_findings: layout.crc_findings.clone(),
        append_log_start: layout.append_log_start,
        read_limits: layout.read_limits,
        resident_bitmap_bytes: layout.resident_bitmap_bytes,
        resident_page_index_bytes: layout.resident_page_index_bytes,
        fatal_access: FatalAccessGate {
            blocked: layout.fatal_access.blocked,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{FatalAccessGate, MatrixRecoveryFinding};
    use crate::matrix::{MatrixCorruptionKind, MatrixCorruptionSeverity, bypass_catalogue};

    /// The gate the layout carries is the one `evaluate` derived from that
    /// layout's findings — not a value any caller chose.
    ///
    /// Lives inside the module because `evaluate` is private to it, which
    /// is the property under test: a test that could call it from file
    /// scope would be a test that the bypass is still open.
    #[test]
    fn the_gate_is_derived_from_the_findings_not_supplied() {
        let spec = bypass_catalogue::probe_spec();
        let clean: [MatrixRecoveryFinding; 0] = [];
        assert!(!FatalAccessGate::evaluate(clean.iter(), spec).is_blocked_for_tests());
        let fatal = [MatrixRecoveryFinding {
            kind: MatrixCorruptionKind::CommitMap,
            severity: MatrixCorruptionSeverity::Fatal,
            message: String::new(),
        }];
        assert!(FatalAccessGate::evaluate(fatal.iter(), spec).is_blocked_for_tests());
        assert!(
            !FatalAccessGate::evaluate(fatal.iter(), spec.with_matrix_fatal_forensics())
                .is_blocked_for_tests(),
            "forensic access is the declared opt-in and must still relax the gate"
        );
    }
}
