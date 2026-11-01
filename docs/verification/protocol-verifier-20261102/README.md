# Publication protocol verifier and negative controls

Baseline: `795c1ab22ad664f568da9aa350aa6019250d9934` (all 74 remote checks passed).
The earlier proofs established metadata-validator properties; they did not prove
publication control flow. This follow-up connects production control flow to
symbolic failure verification and makes negative controls a required CI gate.

## What executes in production and verification

`crates/varlit-core/src/publication_protocol.rs` owns five small generic drivers.
The real indexed writer, checkpoint store, stream recovery and indexed reader
call these exact functions. The Kani environment supplies adversarial callbacks
to the same functions. There is no separately copied ordering algorithm.

| Production caller | Shared driver | Required ordering |
| --- | --- | --- |
| `VarlitIndexedWriter::sync` | `commit` | Prepare batch, sync primary, publish clean index, then acknowledge/reset; uncertain publication errors poison the writer |
| `Store::publish` | `checkpoint` | Complete checkpoint, sync its dependencies, select first head, write/sync first head, write/sync second confirmed head |
| `VarlitStreamWriter::restore_checkpointed` | `restore` | Validate target outside the driver, truncate native file, sync native file, publish restored index |
| `Store::reconcile_confirmed_heads` (both writer-open paths) | `resume` | Sync uncertain confirmed heads before admitting a resumed writer; matching valid heads already imply at least one durable copy |
| `VarlitIndexedReader::follow` | `follow` | Prepare and validate the complete next view before adopting it; preparation error does not adopt anything |

Working checkpoints still omit durability and use working heads. Dirty checkpoints
still have a separate durable anchor. The callbacks keep the original encoders,
validators, syscall arguments, fault hooks and error conversions. The only work
moved before checkpoint I/O is construction of the 64-byte head buffer. Public
signatures, file format, dependencies and resource budgets are unchanged.
Drivers use static dispatch with no new heap allocation, lock or unsafe code.
Their integration includes a production refactor and the resume durability fix
below. Qualification includes large-file equal-work benchmarks; proof execution time is not a runtime benchmark.

## Symbolic I/O model

Nine new harnesses run alongside the six existing core harnesses. The model
chooses any failing I/O position and distinguishes failure before an effect,
partial effect plus failure, and complete effect plus failure. No selected failure
is also possible. Checks run after partial effects, before the error is returned.
Background persistence may retain an old head, tear it, or persist a completed new
head. The first durable head must survive while the second is overwritten.

The model asserts that an observable new head has complete checkpoint dependencies
and durable primary data; at least one confirmed anchor survives the modeled
single publication. For the modeled dirty commit, success requires both new heads to be durable.
The clean/no-write acknowledgement after writer restart is checked by a separate
resume proof and two native tests. Arbitrary unequal prior head offsets ensure
that replacing the older copy first preserves the newest prior anchor.
Publication errors must poison the writer. A separate success harness prevents
an implementation that rejects everything from satisfying the failure proofs.
Recovery has before/after-effect failures, including failure after publication.
Follow checks an arbitrary two-field abstract reader state. It proves adoption
ordering for that state; native tests exercise real handles and their caches.

Unwind 10 covers fixed loops over two heads and their persistence states. Existing
metadata byte-array comparisons retain unwind 35. Unwinding assertions remain
enabled; these are proof execution bounds, not file-size or API operation limits.

## Counterexample found and fixed

The first model assumed every newly usable writer had identical visible and
persisted confirmed frontiers. That was too strong for writer restart. A previous
writer can finish writing the first clean head, then die or receive an I/O error
before syncing it. The next writer can read that complete head from the OS cache.
Before this change, clean writer open did not sync the sidecar, and a subsequent
no-write `sync()` could return success without making that head durable. A later
power loss could fall back to the older durable head despite this success.

This is a durability acknowledgement gap, not evidence that readers accepted a
partially encoded record: primary data and checkpoint dependencies are synced
before a new head is exposed. The evidence is the production call path and a
symbolic counterexample under the stated sync model, not a physical power-cut test.

Both indexed and stream writer open now read the two confirmed heads after clean
validation, while holding their existing exclusive writer lock. Independent readers
remain allowed. Matching valid copies prove that the first copy was synced before the
second was written. Otherwise open must sync the sidecar successfully before
returning a usable writer. Failure returns the existing I/O error. No limit,
allocation, lock, persisted byte or public API is added. Normal open reads two
extra 64-byte heads; divergent heads additionally require a sidecar sync. The
heads are not rewritten, so repeated divergent opens can repeat this sync until a
subsequent publication makes them match. Readers and normal commit sync counts
are unchanged. The whole-sidecar data sync before later publication also flushes
any prior head before it can be replaced.

The `volatile-head-resume` negative control removes this reconciliation and the
new proof fails with `resumed writer acknowledged volatile head`. Native tests
check error propagation/retry for divergent heads and no extra sync for matching
heads. Both new tests also pass under Miri.

## Inductive frontier property

An initialization/transition pair among these nine harnesses proves an abstract frontier invariant.
Frontiers are arbitrary full-width `u64` values. The transition proof starts from
any state satisfying the invariant, not just enumerated histories:

- acknowledged <= persisted confirmed <= visible confirmed;
- visible confirmed <= working and durable primary frontiers;
- every represented pinned frontier <= visible confirmed;
- a usable writer has reconciled visible and persisted confirmed frontiers.

Actions include log extension, the actual shared commit/checkpoint drivers with
arbitrary I/O failure, process death, power loss, clean writer resume, recovery
failure/success and reader follow failure/success. The clean-resume transition
uses the head/frontier correspondence established by the separate resume proof. Initialization establishes the invariant and every
represented transition preserves it. Therefore **the abstract frontier property
has no history-length bound**. This is distinct from the finite native history
sweep. Failed append outcomes that leave the committed view unchanged correspond
to a no-op or unusable-writer state; incomplete bytes beyond the working frontier
are not interpreted as records by this abstraction.

## Assumptions and what is not established

The inductive result applies to the modeled frontiers and shared control flow.
It is not a machine-checked refinement proof of every callback or of the whole
library. In particular:

- Callbacks correctly encode/decode records, validate matching primary/index
  identities and generations, and select the latest confirmed recovery target.
  Existing production-validator proofs and native tests check parts of these
  obligations; the driver proof does not assume it has proved their entirety.
- A successful sync persists preceding complete writes according to the OS/device
  contract. Failed sync may still have effects. Process death keeps the page
  cache; power loss discards volatile state and destroys old reader handles.
- Old committed data referenced by pins remains immutable under the supported
  indexed single-writer/multiple-reader (SWMR) contract. Append/update/delete use the log. Arbitrary external
  edits, native in-place liveness mutation and experimental matrix page reuse are
  not covered by this indexed protocol proof.
- A decoded head is old, new or rejected as torn. Arbitrary checksum collisions or
  intentionally forged replacement data are excluded; CRC is not authentication.
- Proofs start from an existing valid confirmed generation. Initial file/directory
  creation, compaction/path replacement, arbitrary payload parsing, allocator behavior and
  all OS scheduling behavior are not newly proved by the abstract frontier model.
  The existing native crash/compaction tests, Miri, ASan and fuzzing remain needed.

The source binding guard ensures all five named production entry points still
call the verified drivers. It is a mechanical wiring check, not a semantic Rust
parser or a proof that future callbacks cannot violate their contracts. Additional
guards check older-head selection and clean-open reconciliation. New APIs need
their own contract/model; new registered Kani harnesses run automatically.

## Required verifier negative controls

`scripts/check_publication_protocol.py` copies the exact production driver and
proof source into a dependency-free temporary crate. It first requires the
unmodified proofs to pass, then independently applies each of these mutations:

1. Omit primary synchronization.
2. Omit checkpoint synchronization.
3. Omit synchronization of the first head before writing the second.
4. Leave the writer usable after uncertain publication.
5. Swallow a publication error and report success.
6. Publish recovery before native synchronization.
7. Panic instead of returning an error on failed follow preparation.
8. Report success without publishing anything.
9. Skip the readiness check and let an already poisoned writer publish again.
10. Resume without reconciling a visible but volatile confirmed head.
11. Overwrite the newest prior anchor first, instead of the older head.

Every mutation must compile, finish verification, and fail its named expected
assertion. A compiler error, timeout, unrelated assertion or unwinding failure
is not counted as detection. The working source is never mutated. The required
Kani CI job runs this gate and retains its baseline/mutant logs and summary. Losing
a production binding, surviving a mutation, losing an existing required harness
or having a normal proof fail makes CI fail.

A passing verifier is evidence for the stated properties and assumptions, not
proof of perfection. Negative controls demonstrate sensitivity to known defect
families; they do not establish complete fault coverage. A genuine counterexample
in the unmodified implementation must be reproduced and fixed, not excluded or
converted into an accepted result to make CI green.

## Qualification evidence

Local qualification includes 1,313 passing workspace/doc tests (30 existing
ignored tests), 26 required release storage/resource contracts, 36 Python tooling
tests, warnings-denied Clippy, formatting and actionlint. Both new resume tests
pass under Miri. The final proof and negative-control results are recorded in
`evidence.json` and `negative-controls.json`, with source/log hashes and failed
assertions. Full proof logs are retained by the required Kani CI artifact.

[Performance measurements](../../performance/protocol-verifier-20261102/README.md)
retain all initial and confirmation samples. The 1 GiB concurrent workload and
pooled 4 GiB workload pass the unchanged 5% gate; normal writer open adds two
small positional reads and no sync. Divergent-head writer open adds a conditional
sync, measured at about 99–104 µs additional latency in the warm-cache fixture.
Small native-operation increases remain quantified in that report.

These are local results. Remote checks must be assessed on the final PR head;
success on the baseline is not success on this change.

## Reproduction

```sh
cargo kani -p varlit-core --default-unwind 5
python3 scripts/check_publication_protocol.py --output target/verification/protocol-mutations
python3 scripts/check_storage_contracts.py
cargo run --locked -p varlit-test-runner -- test --locked --workspace --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
python3 -m unittest discover -s scripts/tests
```
