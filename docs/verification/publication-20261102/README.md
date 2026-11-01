# Publication and recovery verification

Baseline: `521b9f6aa7485e0a6184f32af52a6572f1f0c090`. Qualification on Linux
x86-64, 2026-11-01 UTC (2026-11-02 KST), Rust 1.95.0 and Kani 0.68.0.
This follow-up adds verification only: no production method body, public API,
persisted format, dependency or configured resource budget changes.

## Observable contract

For the indexed SWMR API, a successful reader observation must equal one whole
confirmed generation. It must not expose staged writes or combine generations.
A pinned reader retains its generation; `follow()` may advance it. Failed
`follow()` must preserve the prior handle. Independent readers can legitimately
observe different confirmed generations.

`push_info()` and `delete_info()` stage working state. `flush()` is not clean
publication. `sync()` orders stream synchronization before clean index
publication. If publication returns an error, its effects may already be visible;
callers cannot assume the commit did not happen or blindly repeat an operation.
The tests accept the old or newly confirmed complete state at such boundaries.
The poisoned writer must be recovered before further mutation.

This is a data-safety contract, not unconditional availability: damaged
publication anchors may require an explicit error. Process termination does not
drop the OS page cache. Power-loss durability additionally depends on the
filesystem/device honoring synchronization and ordering, and is not established
by a child-process abort. No claim is made for hostile concurrent edits outside
the supported SWMR/mmap contract or for CRC authentication.

## Added checks and their exact scope

| Check | Coverage | Required result |
| --- | --- | --- |
| Four Kani harnesses | Actual metadata/checkpoint validators; symbolic full-width offsets, counts, generations, sequence options, identities and digest bytes | Accepted metadata has consistent frontiers; accepted restore preserves the confirmed frontier; consistent clean and restore families are accepted |
| Joint torn heads | Both 64-byte confirmed heads independently cut at every byte boundary; new checkpoint body present or absent: 65 × 65 × 2 = 8,450 images | Exactly old/new metadata and end, or rejection; complete old/new images must succeed |
| Short operation histories | Every length-three sequence over eight operations: 512 histories | Pinned, following and fresh readers match an independent logical map after each operation |
| Longer histories | 16 reproducible seeds × 48 operations | Same oracle, including repeated keys, failures, recovery and compaction |
| Live-reader crash history | One 15-operation mixed history, aborted at all 244 discovered hook occurrences | Whole confirmed state, stable old pins, non-regressing successful observations, valid recovery and subsequent write |

The eight-operation alphabet is put key 0, put key 1, delete key 0, delete key 1,
sync, reopen/restore, sync then compact, and injected append-plus-rollback failure
then recovery. The 528 short/long histories execute 2,304 operations. Their oracle
is a separate logical map, not output read back from the implementation.

The crash history includes overwrite, delete, sync, compaction, a failed append
and rollback, recovery, reopen, and a final unpublished append. Discovery requires
commit, partial checkpoint-record/head, clean-publication, native-truncate and
atomic-replacement hooks. Each child must reach its selected occurrence before
abnormal exit; it has a 30-second deadline and is reaped even on parent failure.
Readers stay open while that child runs. After each crash, the oracle also uses
the interrupted operation number recorded by the child: only its preceding
commit, or its new commit if interrupted inside sync, is permitted. A future
commit from later operations cannot satisfy this check. Recovery must preserve
the confirmed state, permit a subsequent committed append, and preserve the
pre-recovery pin.

The Kani harnesses use unwind 35 for fixed 32-byte comparisons, with unwinding
assertions enabled. The two existing arithmetic/membership harnesses retain
default unwind 5. Validator errors own no allocation; harnesses forget their
result to exclude unrelated recursive error destruction. Actual validation
executes unchanged. Acceptance proofs cover consistent StateOnly metadata with
an empty primary witness; rejection/implication proofs explore both modes and
arbitrary witnesses. These proofs do not establish filesystem ordering.

## Negative controls

In an isolated source copy, removing the production clean-frontier equality
check makes Kani fail `m.committed == m.working`. Selecting the working head in
`Store::confirmed()` makes the history test fail on an unexpected committed-reader
generation error. Both mutants compiled and were detected. Neither mutation is
present in this change.

## Automation and remaining proof boundary

All six registered Kani harnesses run in required CI; name checks prevent removal
from silently passing. The three added release contracts make 24 required
storage/resource cases. Native and ASan suites discover the new tests. The new
8,450-image filesystem sweep exceeded a measured 300-second Miri trial, so its exact name and reason
are in `scripts/miri-exclusions.json`; the full native/ASan sweep remains selected.
The Miri trial is a timeout, not a pass or evidence of a memory-safety defect.
New API operations still need an operation model and logical oracle.

This is not an exhaustive proof over unlimited histories, all keys/payload types,
all thread schedules, every error return, every platform or all storage failures.
The crash sweep is one history, not the Cartesian product of every history and
cut. Joint-head images model prefix tears, not every sector permutation, arbitrary
bit corruption or checksum collision. The existing Loom models cover cancellation;
no whole-filesystem Loom proof is claimed. Existing broader fault, corruption,
fuzz and resource suites remain necessary. Owner-deferred BTreeMap OOM behavior
is unchanged. These results do not establish safety-critical certification.

## Reproduction

```sh
cargo kani -p varlit-core --default-unwind 5
python3 scripts/check_storage_contracts.py
cargo run --locked -p varlit-test-runner -- test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
python3 -m unittest discover -s scripts/tests
rustfmt --edition 2024 --check crates/varlit/tests/support/publication_histories.rs
```

The explicit rustfmt command is needed because the history support file is
included with `include!`. See `evidence.json` for measured qualification results
and `performance/` for the equal-work large-file comparison.

## Local qualification results

- Workspace and doctests: **1,311 passed, zero failed, 30 ignored** (including
  subprocess entry points). Test artifact cleanup verified empty.
- Required release storage/resource contracts: **24 passed**.
- Kani: **six harnesses verified**, zero failures; four are new.
- Warnings-denied Clippy, **33 Python tests**, formatting and actionlint passed.
- Both negative controls were detected. The new Miri image sweep timed out at
  300 seconds; validated Miri selection is 197 tests with 51 documented exclusions.
- Remote CI/ASan/Miri/fuzz results for the publication commit are separate from
  these local results.

The baseline remote CI, ASan and fuzz jobs passed. Its Miri job
[124899646500](https://github.com/shim9610/varlit/actions/runs/41323882631/job/124899646500)
failed in `stream::tests::confirmed_partial_record_is_an_error_for_both_cursors`.
The exact test reproduces unsupported directory `fsync` locally with
nightly-2026-10-12. Its documented exclusion retains native/ASan coverage; the
two remaining selected stream tests pass individually under Miri. The full
Miri suite must rerun on this commit; these focused results do not replace it.

## Equal-work performance check

Three alternating pairs per size (12 measurements) compare the current PR baseline
with the verification candidate, without concurrent builds/tests. Both use eight
following readers and one pinned reader, 16,384 checks per reader per mutation
epoch, unchanged cache/batch limits and matching snapshots/trace hashes. Wall and
CPU values are medians; RSS is the maximum observed across each side. The existing
5% gate also checks bytes written. Complete report objects and binary hashes are
retained in `performance/`; this is not a statistical zero-overhead guarantee.

| Payload | Write-phase wall | CPU | Maximum RSS | Gate |
| --- | ---: | ---: | ---: | --- |
| 1 GiB | +2.43% | +1.79% | +0.28% | passed |
| 4 GiB | +2.09% | +1.14% | -0.03% | passed |
