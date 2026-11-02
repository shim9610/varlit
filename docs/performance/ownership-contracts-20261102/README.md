# Ownership allocation verification and performance

Baseline: `213e2349406bf5ad4de1f052672b40c892bd634a`, the latest PR #18 head
when this follow-up started. The preceding commit history and allocation changes
were reviewed before modification. Existing API signatures and file formats
remain compatible; the two Box helpers are additive.

Box cloning now reserves its owner before invoking the payload clone. New
`try_box_uninit` and `try_box_with` APIs separate reservation from construction.
Private Arc initialization uses uniquely owned uninitialized storage, and
copy-on-write reserves its replacement owner before cloning the payload. An
allocation refusal can reach the caller without first running that deferred
initializer. The caller chooses recovery.

The [verification report](../../verification/ownership-20261102/README.md)
defines the exact preconditions, proof bounds, allocator/empty-vector models,
cleanup properties and exclusions. It distinguishes allocation-refusal proofs
from unconditional OOM freedom. It also records the Kani representation issue
and failed verifier attempts rather than counting them as successful checks.

## Per-unit qualification

Two units change runtime behavior: Box reservation/cloning, then shared owner
reservation/copy-on-write. Other `TryClone` implementations receive verification
without an additional runtime change. Each benchmark compares a unit with its
immediate predecessor, not with `master`.

| Check | Result |
| --- | --- |
| Box isolated workspace, all features | 1,355 passed, 30 ignored |
| Arc workspace, all features | 1,356 passed, 30 ignored |
| Kani, standard checks | 50/50 passed under default features and 50/50 under all features; identical source/manifest fingerprints before and after both runs |
| Negative controls | 5/5 ownership, 17/17 resource and 11/11 publication mutations detected at their required assertions |
| Required release contracts | 57/57 passed, including allocation refusal, zstd native workspace refusal, large cardinality and retry/state preservation |
| Targeted Miri | 6 helper and 6 integration tests passed; includes refusal, partial cleanup, shared destruction and large in-place Box initialization |
| Extra Kani initialization/value instrumentation | Not qualified: Kani 0.68 compiler errors for pointer-pattern ABI and unsupported function-pointer resolution; retained logs and scope are explained in the verification report |
| Python checks | 46 passed |
| API and Clippy | Registered executable contracts for both new Box functions/exports; workspace all-feature/all-target Clippy passed on Rust 1.95.0 and 1.99.1 |
| Concurrency and source checks | Both production cancellation Loom models passed; formatting, actionlint 1.7.11 and diff whitespace checks passed |

The earlier Box workspace run overlapped later source edits; the isolated
replacement above is the qualified Box run.
An intermediate Python run omitted the toolchain PATH and failed to find Cargo;
the correctly configured rerun passed. Failed/superseded logs remain archived.
The first ownership mutation run correctly rejected its Arc mutation as an
invalid control: its generated closure lacked a Result error-type annotation.
That compiler failure is not a detected contract violation. The final control
adds the annotation and requires the named runtime assertion to fail.

The baseline's remote stable-Clippy job failed on Rust 1.99.1's
`byte_char_slices` lint in the existing zstd allocation-refusal test. The
equivalent `*b"12"` spelling fixes that test-only lint without suppressing it;
the 1.99.1 all-workspace Clippy run above includes the fix.

## Measurement method

The four-CPU and 16 GiB cgroup limits and release profiles are unchanged. Timed
runs execute without concurrent builds, tests or proofs. Direct/native/owner
microbenchmarks use the same pinned CPU; concurrent cases use the same affinity
under the four-CPU quota. Before/after order alternates. Five initial pairs are
followed by 15 additional pairs whenever a measured metric exceeds +5%; all 20
pairs are pooled. Time/CPU use medians; RSS uses the maximum per side. Native operation timers
exclude fixture setup; process CPU and RSS include the entire child process. Indexed
single-writer/multiple-reader (SWMR) comparisons use three pairs per size and
require equal work counters, snapshots and trace hashes.

The retained set contains 524 primary before/after observations, 60 qualified
diagnostic observations, 30 superseded diagnostic observations and 12 separate
resource probes. Box/Arc primary benchmark
cgroup event counters are unchanged between their start and end snapshots.

The fixed workloads include:

- 10 million Box creations, 10 million shared-owner creation/clone/cancel
  operations, and 8,000 clones of a boxed vector containing 1,024 strings.
- 64 MiB and 1 GiB byte clones, and nested owned strings.
- Two million records/200,000 marks, a 4 GiB writer with 4,096 reverse marks,
  complex maintenance, and full 64 MiB chunk encoding/decoding.
- 1 GiB and 4 GiB indexed SWMR with eight followers and one pinned reader.
- Ordinary and durable matrix writes with 8,192 rows of 4,096 bytes, 64 epochs
  of 64 updates, compaction every 16 epochs, eight followers and one pinned
  reader; each reader checks 512 complete cells per epoch.

Native, indexed SWMR and matrix workloads use the workspace's disk-backed overlay
filesystem. Matrix resource probes set an explicit disk-backed TMPDIR. The
complex-maintenance and large-clone resource probes retain their default `/tmp`
setting, which is tmpfs on this host. Resource evidence records process RSS,
whole-cgroup memory, file lengths/allocated blocks (including deleted open files)
and writes. Those peaks are sampled every 5 ms, not formal upper bounds; RSS
alone does not include all filesystem cache/tmpfs memory.

## Results and reproduction

The [evidence archive](evidence.tar.gz) contains 1,364 files (85,353,874 bytes
before compression). Its SHA-256 is
`bc0601c59cc2a99a77cfdf23d54e5c3e27e2f152931e970f9ff9ada73759e4f1`.
The [member manifest](evidence-members.sha256), [source hashes](source-sha256.json)
and [binary hashes](binary-sha256.json) make the snapshots independently checkable.
Qualified checks are identified by `box-isolated-workspace.log`,
`arc-workspace.log`, `kani-{default,all}-contracts.json`,
`ownership-mutations-final/report.json`, the resource/protocol mutation
`summary.json` files, and `storage-contracts/summary.json` inside the archive.
Failed/superseded logs are retained alongside these authoritative results.

The [complete measurement tables](measurements.md) include both per-unit
comparisons and a direct final-versus-baseline owner comparison. The latter
shows a **Box creation regression of +8.77% operation time and +9.11% CPU**
over 20 alternating pairs. Nested Box cloning improves by 18.52% time/18.50%
CPU, while shared owner operations change by +0.86%/+0.98%. The Box regression
is not a passing performance gate. Its operation medians are 99.43 → 108.15 ms
for 10 million creations; process CPU medians are 100.44 → 109.58 ms.

The original owner benchmark selects its operation inside the repeated loop.
Disassembly shows the Box allocation/write/free operations inline in both
binaries, with differences in surrounding loop/register code. A separate
diagnostic moves dispatch outside the loop and retains the same operation counts
and payloads. Its first 30 observations are not qualified: a baseline rebuild
after formatting used default features while the after binary used all features.
That +3.45%/+3.70% Box result is retained as a superseded, mismatched comparison.
The replacement builds both sides with identical all-feature settings.
Across its retained samples, Box time/CPU change by -1.09%/-1.14% (five pairs),
nested Box by -12.86%/-12.84% (five pairs), and shared ownership by
-0.25%/-0.13% (20 pooled pairs). These distinct caller contexts produce distinct
measurements; they do not establish one cause for the original regression.
Neither diagnostic replaces the adverse original result. No production
code was tuned to the diagnostic. A final release rebuild is byte-identical to
the Arc-stage binary used for the primary benchmarks.

Both 1 GiB and 4 GiB indexed SWMR gates pass for each unit. Matrix workloads
preserve all work counters and reader sums. Sampled complex-maintenance temporary
allocation stays at 4 MiB; sampled open regular allocation stays at 87.70 MiB.
Large-clone RSS remains about 2,049 MiB and matrix open regular allocation about
173.87 MiB. Full CPU, RSS, writes, cgroup memory and temporary-file observations
are retained rather than inferred from elapsed time alone.

The Box-stage 4 GiB reverse-mark workload initially measured +15.34% time.
The prescribed 15 confirmation pairs brought the pooled 20-pair result to
+1.95% time/-1.94% CPU. The initial samples remain included. No resource limit
or workload was relaxed to obtain these results.

The per-unit tables, raw samples, resource peaks, binary hashes and source
snapshots are retained with this report. The final aggregate includes all initial
and confirmation observations, including adverse measurements. Host-specific
observations are not universal speedup claims.

The archive contains the executed drivers and the Box/Arc source patches. To
reproduce, make separate checkouts from the baseline, apply the corresponding
patch/saved sources, copy both new ownership benchmark examples unchanged into
the baseline checkout, and run the same release builds with `build.sh`. Its optional
second argument selects an explicit source checkout. `bench_stage.py`,
`owner_bench.py` and `matrix_bench.py` take the current and preceding unit names;
`diagnostic-matched.sh` builds and runs the matched-feature diagnostic;
`summarize.py` pools the retained samples. Absolute workspace paths in the
drivers must be adapted consistently on another host.
