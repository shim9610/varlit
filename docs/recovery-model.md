# Matrix Recovery Model

This document describes how a tiled matrix detects damage, what it reports, and
the primitive actions that repair it. Varlit reports facts and offers primitive
actions; callers choose domain policy. Crash recovery of the write-ahead ring
(which publications survive a crash) is described in the
[durability model](durability-model.md#crash-and-reopen).

## What Is Checked

A matrix lives inside the primary file as internal records: a metadata record,
the write-ahead ring, radix directory pages and tiles. Each check below is in
the code path named; nothing else is checked.

| Stored object | Check | Where | Failure |
| --- | --- | --- | --- |
| Metadata record (extents, ring position and length, creation nonce) | magic, layout version, block count, growing flag, CRC32, ring length within the format bounds | every matrix open | `InvalidMatrixLayout` |
| Ring header (two slots) | magic, version, CRC32 per slot; the valid slot with the higher sequence is used | every open, follow and read | `InvalidMatrixLayout` when neither slot is valid after retries |
| Ring frame | magic, generation sequence, length, CRC32 over the whole frame, the file length it depends on | every scan of the ring (writer open, reader open, follow, checkpoint) | the scan stops there; that generation and later ones are not adopted |
| Tile commit bitmap | CRC32 over the bitmap | every read that consults it, every commit-bit change, `matrix_resume_signal` | `InvalidMatrixLayout` |
| Tile header (magic, layout version, block id, tile coordinates) | CRC32 | `verify_matrix_metadata()` only | a `Recoverable` finding |
| Cell slot (payload and its CRC32) | CRC32 of the payload | cell reads when the read-validation policy checks data CRCs (by default unless the format declares `integrity: none`); `commit_matrix_cell()` always | `MatrixChecksumMismatch` |
| Directory pages, single and per-channel commit flags, aux bytes | none | — | — |

Every slot stores the CRC32 of its payload whatever the integrity policy, so
the per-cell evidence that `rebuild_matrix_commit_from_crc` uses is present in
every tiled matrix. An unwritten slot is all zero, and a written slot never
is: a nonzero payload is not, and the CRC32 stored after an all-zero payload
is not zero. So a written slot is told apart from an unwritten one without
extra state. `commit_matrix_cell()` refuses
an unwritten slot with `MatrixCellNotWritten` and a slot whose CRC32 does not
match with `MatrixChecksumMismatch` (`a_zero_valued_cell_can_be_committed`,
`a_cell_never_written_still_refuses_to_commit`).

## Classification

```rust
pub enum MatrixCorruptionSeverity {
    Fatal,
    Recoverable,
    Advisory,
}

pub enum MatrixCorruptionKind {
    Header,
    Layout,
    CommitMap,
    Slot,
    Sidecar,
}
```

The tiled implementation produces one kind of finding: a `Recoverable`
`CommitMap` finding from `verify_matrix_metadata()` for a tile whose header or
commit bitmap fails its check. The other kinds and severities remain in the
public enums but no current code path produces them. Damage found by an
ordinary read is returned as that read's error, not recorded as a finding.

## Recovery Report

```rust
pub struct MatrixRecoveryReport {
    pub findings: Vec<MatrixRecoveryFinding>,
    pub recommended_actions: Vec<MatrixRecoveryAction>,
}

pub enum MatrixRecoveryAction {
    ClearCell { category: String, key: MatrixKey },
    ClearCategory { category: String },
    RebuildCommitMap { category: Option<String> },
    Resume,
    Restart,
    DiscardSidecar,
}
```

An open runs no verification pass and holds no findings;
`matrix_recovery_report()` was removed in 0.13.0. `verify_matrix_metadata()` visits
every tile that exists in the handle's view, checks its header and commit
bitmap, and returns a report with one `Recoverable` `CommitMap` finding per
damaged tile and one `ClearCategory` recommendation per category that has a
damaged tile. It reports; it does not block later access, and its cost is
proportional to the written tiles. `diagnose_file` runs the same check on a
matrix file and reports each finding as `file.matrix.recovery_finding`, or
the error `file.matrix.verification_failed` if the check itself fails.

There is no quarantine. `MatrixCommitQuarantined`, `MatrixFatalCorruption` and
`FormatSpec::with_matrix_fatal_forensics()` were removed in 0.13.0.

`apply_matrix_recovery_action` runs `ClearCell` as
`clear_matrix_cell_by_category` and `ClearCategory` as `clear_matrix_category`.
It refuses `RebuildCommitMap` with `InvalidFormatSpec` because the rebuild needs
the block type: call `rebuild_matrix_commit_from_crc::<T>()`. `Resume`,
`Restart` and `DiscardSidecar` are caller decisions and do nothing.

Every action is a writer call. Its changes are staged like any other matrix
change and reach readers at the next publication (`sync()`); a reader that is
open keeps its snapshot until it follows.

## Recovering A Damaged Commit Bitmap

A tile whose commit bitmap fails its CRC32 makes every read of that tile's
cells, every write, commit or clear of one of them, and the category's
`matrix_resume_signal` fail with `InvalidMatrixLayout`. Cells in other tiles
of the category stay readable.

`clear_matrix_category(category)` reads each tile's bitmap without checking
it, sets every bit to zero, reseals the bitmap and returns how many bits were
set. It is the operation that recovers a damaged bitmap.
`rebuild_matrix_commit_from_crc::<T>()` also rewrites each tile's bitmap whole
without reading its old bits.

`damage_in_place_is_reported_and_a_category_clear_recovers` flips a bitmap
bit, checks that a read fails and that `verify_matrix_metadata()` reports a
finding, then, in one writer session with no reopen, clears the category,
rebuilds it from the slot checksums, syncs, and reads both cells back.

## Rebuild Commit Bits From Slot Checksums

`rebuild_matrix_commit_from_crc::<T>()` visits every tile of `T`'s block that
exists, reads all of its slots, and rewrites its commit bitmap: a bit is set
exactly when the slot's stored CRC32 matches its payload. It returns the
number of cells committed afterwards. Growing and fixed matrices are handled
the same way.

A matching CRC32 shows that the slot was written and is intact. It does not
show that the cell was committed before the damage. A cell written and never
committed, or committed and later cleared with `clear_matrix_cell`, keeps its
bytes and is committed by the rebuild. Clear or rewrite such cells afterwards
if that matters. A slot whose payload no longer matches its CRC32 is left
uncommitted. The CRC32 detects accidental damage, not deliberate rewriting.

## Cell And Category Clearing

Clearing a cell or a category changes only commit bits. Slot bytes stay, so
committing a cleared cell again without a new write makes its old value
visible again. Reads of a cleared cell return `NotCommitted` /
`MatrixNotCommitted`.

The writer API is `clear_matrix_cell::<T>(key)`,
`clear_matrix_cell_by_category(category, key)`, `clear_matrix_category(category)`
and `MatrixRecoveryAction::ClearCell` / `ClearCategory` through
`apply_matrix_recovery_action`. `clear_matrix_category` returns the number of
commit bits it cleared and counts zero on a second call
(`matrix_recovery_clear_category_and_cell_actions_are_applied`,
`clearing_a_category_counts_the_open_chunk_and_refuses_a_written_one`). It
reaches every tile of the category, including tiles past a growing
dimension's declared extent (`the_bulk_clear_reaches_every_written_chunk_and_reaches_the_file`).

This is the action for localized slot damage: keep the file, hide the
affected cells, and leave recomputation to the caller.

## Sidecar And Resume Signals

Sidecar support exposes signals, not domain policy:

```rust
pub enum MatrixResumeSignal {
    Clean,
    Partial { committed: u64, total: u64 },
    ResumeAvailable { committed: u64, total: u64 },
    RestartRecommended,
    DiscardRecommended,
}
```

Varlit determines the signal from:

- sidecar file presence
- committed/total progress in relevant commit maps
- optional Varlit sidecar envelope validation

The current API `matrix_sidecar_resume_signal(category, path)` intentionally
uses only sidecar presence plus commit progress. The verified path,
`matrix_verified_sidecar_resume_signal(category, path)`, reads the optional
Varlit sidecar envelope written by `write_matrix_sidecar` and checks parent
format identity, category, payload length, and payload CRC32 when the
`integrity` feature is enabled. Parent schema identity uses
`computed_schema_hash()`, not the pinned file-header `schema_hash`, so manually
pinned release hashes do not change the sidecar schema fingerprint. Generation
is stored and returned by the base read API;
`matrix_verified_sidecar_resume_signal_with_generation` and
`read_matrix_sidecar_with_generation` additionally require an exact caller
generation match. A missing verified sidecar follows normal restart/clean logic;
a corrupt or mismatched verified sidecar returns `DiscardRecommended`.

The envelope validates bytes and parent identity. Domain merge semantics,
whether a generation is recent enough, and how resume payloads are interpreted
remain caller-owned.

The caller decides whether to resume, restart, or discard.

## Tests

- `damage_in_place_is_reported_and_a_category_clear_recovers`
  (`crates/varlit/tests/matrix_tiles.rs`): a flipped slot byte is
  `MatrixChecksumMismatch` for that cell only; a flipped bitmap byte fails the
  tile's reads and is a `verify_matrix_metadata()` finding; a category clear
  followed by a CRC rebuild recovers both cells.
- `unpublished_and_torn_publications_never_reach_a_reopen`
  (`matrix_tiles.rs`): a damaged newest ring frame is not adopted, and the
  publication before it stands.
- `matrix_recovery_clear_category_and_cell_actions_are_applied`
  (`crates/varlit/tests/matrix.rs`): clear-category and `ClearCell` hide
  committed cells; the resume signal afterwards is `Clean`.
- `matrix_verified_sidecar_checks_parent_identity_and_payload_crc`
  (`matrix.rs`, `integrity` feature): verified sidecar identity and payload
  checks.
- The ring crash tests in `crates/varlit-core/src/matrix_tiles/tests.rs`,
  listed in the [durability model](durability-model.md#crash-and-reopen).
