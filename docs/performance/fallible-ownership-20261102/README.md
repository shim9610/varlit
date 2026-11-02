# Fallible cloning and owned allocation

Baseline: `2f2d8180f8e2de55afb02824e200b52da4a6f7c8`, the latest head of
PR #18 when this change started. The preceding decompression, spill policy,
metadata/resource verification and range-streaming commits were reviewed first.
This change preserves their limits, existing public signatures and file formats.

## Behavior and compatibility

1. `TryClone` explicitly propagates allocation refusal for supported owned
   containers and Varlit layouts. There is no blanket `T: Clone` implementation.
   Partially copied output is released and the source remains usable. Scalar
   byte arrays retain bulk copying. Existing public `Clone` remains available.
2. `try_box` returns an allocation error, including for fixed-size ownership and
   error storage. Durable matrix writes reserve their event and source boxes
   before mutation; a later hook/durability failure fills the reserved storage.
   Other publication paths retain inline `AllocationFailureContext` facts when
   detailed error storage is refused. Callers must inspect those facts before
   repeating an append. Fallible path formatting prevents a hidden allocation
   abort while constructing these particular error wrappers.
3. Private shared owners use pinned `triomphe 0.1.17` with only its `std` feature.
   Stable Rust 1.95 does not provide stable `std::sync::Arc::try_new`.
   Public standard-library Arc types are unchanged. Copy-on-write separation
   completes before persistence, and the commit step only accesses unique
   storage. `ScanCancellationToken::try_new` adds a fallible constructor; its
   existing `new`/`default` contract remains unchanged. The Windows read pool
   owns a fallibly allocated `Box<File>` and retries a refused initialization.

On this x86-64 build, `Error` and `MatrixCommitEvent` each remain 56 bytes and
`BatchAppendError` remains 120 bytes. A successful durable matrix cell write now
performs two extra allocations requesting 112 bytes in total, excluding allocator
metadata. These reservations preserve the existing post-commit error shapes.

`BTreeMap` allocation is explicitly deferred. Existing `FormatSelfTest` fluent
builders still box their callbacks infallibly, and public deep `Clone` retains
its original behavior. Other convenience/report builders, formatting, application
code and dependency internals have not all been converted.
An arbitrary user `TryClone`, hash/equality callback or destructor must uphold its
own contract. Allocators that abort instead of returning null, reference-count
overflow and OS OOM kills are outside this work. **This is not a whole-library
OOM-freedom proof.** See [allocation limitations](../../known-limitations.md#72-some-allocations-abort-instead-of-returning-an-error).

## Regression qualification

The three implementation units were checked separately, then together. Stage 1
adds fallible clones, stage 2 adds boxes/error storage, and stage 3 adds shared
owners and the remaining copy-on-write integration. Each performance comparison
uses the immediately preceding stage; it is not a comparison against `master`.

| Check | Result and scope |
| --- | --- |
| Stage 1 workspace | 1,339 passed, 30 ignored; after bulk-copy specialization, core regressions (288 passed, 7 ignored), API coverage and Clippy reran |
| Stage 2 isolated workspace | 1,342 passed, 30 ignored; independent source snapshot |
| Final workspace, all features | 1,352 passed, 30 ignored, including doctests and compile-fail tests |
| Default core features | 258 passed, 3 ignored |
| Required storage contracts | 53 passed, including explicit runs of selected ignored resource tests |
| Allocation refusal | 47 clone allocation points, 4 nested-box points and 21 mmap layout-copy points; partial cleanup, source preservation and retry checked |
| Publication refusal | Both durable preflight reservations fail before mutation; a late hook error requires no post-commit allocation; detailed-error refusal retains publication facts |
| Ownership invariants | Alignment/ZST, input destruction on refusal, shared COW refusal, last-owner threaded destruction, blocked-layout state and CRC evidence preservation |
| Miri | 8 targeted helper/integration tests passed with default alias checks; not the complete Miri suite |
| Loom | Both cancellation ordering models passed; these do not model triomphe's reference-count implementation |
| ASan codec fuzzing | 1,242,365 executions in 31 seconds, no finding; unchanged 4 GiB RSS limit; not exhaustive fuzz coverage |
| Python checks | 46 passed |
| Dependency checks | Runtime-dependency policy, deny, root/auxiliary lockfile audits and vendored redb identity/advisory controls passed |
| API checks | Executable contracts registered without refreshing the frozen legacy inventory; public API and renamed-dependency fixtures passed |
| Windows | GNU-target cross-check passed for integrity/mmap/zero-copy/experimental reuse; no native Windows execution or zstd cross-build |

Kani verified all 35 existing harnesses in both default and all-feature builds.
All 17 resource and 11 publication mutation controls were detected by the
expected assertions; compilation errors/timeouts are not counted as detections.
Source fingerprints matched before and after the proof runs.
The Kani harnesses verify the existing bounded metadata/resource/publication
models, with their documented assumptions and production-binding checks. They do
not establish absence of arbitrary heap failure or prove the entire Arc/Box
implementation. Allocation-refusal sweeps and Miri provide separate evidence for
the new ownership paths.

The intermediate stage-2 run that overlapped source edits is not qualification.
Its replacement ran against an isolated snapshot. An earlier debug build filled
the workspace filesystem; build caches were cleaned with `cargo clean`, and the
same checks reran with debug information/incremental compilation disabled. CPU,
memory and disk budgets were not increased. Release benchmark profiles were
unchanged. Failed and superseded logs are retained alongside successful evidence.

## Performance method

Four-CPU and 16 GiB cgroup limits were unchanged. Timed measurements ran without
concurrent builds, tests or proofs. Direct/native workloads use the same pinned
CPU; concurrent workloads use the same full affinity under that quota. Order
alternates before/after. Direct/native/matrix cases start with five pairs; any
measured metric above +5% triggers 15 more pairs. **All 20 pairs are pooled**,
including the original adverse samples. Time/CPU use medians and RSS uses the
maximum per side. Indexed SWMR compares the write phase using three pairs per
size and verifies matching work counters, snapshots and trace hashes.

Workloads include 64 MiB and 1 GiB owned byte copies; nested owned strings;
2 million records/200,000 marks; a 4 GiB writer/4,096 reverse marks; complex
maintenance; full chunk encode/decode; 1/4 GiB indexed SWMR with eight followers
and one pinned reader; and matrix COW/compaction with eight followers and one
pinned reader. The matrix fixture holds 8,192 rows of 4,096 bytes, updates 64
rows for each of 64 epochs, compacts every 16 epochs, and checks 512 complete
cells per reader per epoch. It measures ordinary and durable writes separately.

Native, indexed SWMR and the qualified matrix tests use explicit paths on the
workspace's disk-backed overlay filesystem. An initial matrix run used `/tmp`,
which is tmpfs on this host. That run is retained as supplementary evidence and
is **not used as disk durability performance evidence**. Resource probes that
used default `/tmp` are identified separately. RSS does not include all tmpfs or
filesystem cache memory; the probes also record whole-cgroup memory and open or
deleted file lengths/allocated blocks. These are 5 ms sampled peaks, not formal
upper bounds.

The [per-unit tables](measurements.md) and [machine-readable aggregates](measurements.json)
retain each comparison. The stage-1 nested-string case has a measured **+11.07%
operation-time and +9.93% CPU regression across all 20 pairs**. It is not a
performance-gate pass. An earlier generic per-element implementation caused more
than 2× byte-copy time; bulk copying fixed that regression before qualification.
Those initial adverse samples remain in the archive. The nested-string cost was
not removed by that optimization and must be considered by callers doing many
small owned copies. These host measurements do not establish general speedups
or a zero-overhead guarantee.

Stage 3's short 4 GiB reverse-mark operation is close to the threshold: the
initial five pairs measured +6.99%, the additional 15 measured +7.81%, and all
20 pooled measured +4.94%. Whole-process CPU changed -1.04%; that does not erase
the operation-time signal. The two-million-record case pools to +1.87% after an
initial +8.95% signal. All samples are retained, and neither result supports a
general claim that shared-owner conversion is free.

Stage 2's disk-backed durable matrix comparison measured +3.61% operation time
and +3.69% CPU. Stage 3's initial +5.60% operation signal prompted 15 additional
pairs (-3.33%); all 20 pooled measured -1.89% operation time and +1.70% CPU.
Both indexed SWMR sizes pass the existing resource gate in every stage. The
final stage changes 1/4 GiB write-phase wall time by -3.75%/-3.27%, CPU by
-3.85%/-3.22%, and maximum RSS by +0.11%/-0.02%.

The [sampled resource tables](resources.md) retain RSS and file allocation
separately. Complex maintenance retains the same 4,194,304-byte private spill
allocation in all three comparisons. The disk matrix probes retain
148,422,656 logical bytes of deleted regular files, including old generations;
that storage cost must not be described as zero just because private spill is
zero. Large owned cloning still needs both the source and the copy (about 2 GiB
RSS for a 1 GiB input). Cgroup OOM, OOM-kill and group-kill counters remain zero;
the pre-existing limit/reclaim events are retained in the evidence.

## Reproduction and evidence

`bench_stage.py` and `matrix_bench.py` record workloads, confirmation rules and
exact commands. They reuse the repository's audit-followup benchmark/resource
drivers and `measure-child.c`. `build.sh` records the release/all-feature builds.
Paths reflect this session; map `/workspace/varlit` and the baseline worktree to
your checkouts. The baseline uses `baseline_ownership_load.rs` (infallible clone)
and the same `matrix_ownership_load.rs` fixture as the candidate. Stage snapshots
and untracked source overlays are retained so intermediate implementations can
be reconstructed without relying on a mutable working directory. For each stage,
start a separate checkout at the baseline, apply `snapshots/stageN/tracked.patch`,
then copy its `crates/` overlay into that checkout. Point the build script at that
checkout and use a separate Cargo target directory for each version. Stage 1
retains the runtime/API snapshot and the common benchmark fixture; its allocation
sweep was subsequently extended by stages 2 and 3 in the final test source.

Source/binary hashes, commands, raw measurements, qualification logs and failed
attempts are retained in the evidence archive with a member SHA-256 index.
Build products, executables and generated database fixtures are excluded.
The qualified primary observations total 496: 120 direct ownership, 270 native,
36 indexed SWMR and 70 disk matrix observations, plus 16 resource probes.
Supplementary tmpfs matrix measurements and superseded attempts are retained
separately and are not pooled into those totals.
Earlier PR performance costs remain relevant; this report does not supersede
the earlier baseline comparisons.
