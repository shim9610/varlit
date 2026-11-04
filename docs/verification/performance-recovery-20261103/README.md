> Historical recovery results through `5e480a5`; its pooled original-reference time gate failed. For the subsequent streaming changes and remaining failures, see [streaming follow](../streaming-follow-20261104/README.md).

# Storage and ownership performance recovery

This follow-up starts from PR #18 head `0e9abd7f5296636a460ddf533637ae3a01ecac90`.
It addresses the previously disclosed native mutation/rewrite costs without
removing boundary validation, raising resource allowances, or changing public
signatures or the file format. The preceding head's CI (all 71 jobs) and separate
ASan/leak, fuzz and Miri jobs completed successfully; that is separate evidence
from qualification of these new changes.

Rejected candidates and every adverse or partial measurement remain below and
in the evidence archive. Later passes do not replace those observations.

## Current bounded-refinement candidate

The working follow-up uses checked 64-bit products for ordinary position
scaling, with the original exact 128-bit arithmetic retained on overflow. Both
resident directory searches share the same calculation. The cached floor
search permits at most three further estimates, only while observed distance
shrinks by at least half. Every returned result still requires an actual key
or neighboring-key bracket; the existing bounded window/binary search remains
the fallback. No allocation, larger buffer or public/file-format change is
introduced.

A 12-group balanced four-version diagnostic (192 observations across four
workloads) selected this combination over arithmetic alone. Against the
per-case reference, operation time changes by +0.09% for short mixed marks,
-9.25% for complex maintenance, -12.83% for variable marks and -5.74% for long
mixed marks. The complex reference is `749532d`; the other three references are
original `317fc98`, so this is not a comparison to one common revision.
These diagnostics select a candidate; they do not replace qualification.

Runtime `5e480a540997cc42186e3152813dfab45c2e2378` passes 1,384 all-feature /
848 default tests and doctests (30/20 ignored), Clippy and formatting, and four
targeted Miri contracts. The exact arithmetic grid and periodic geometry are
new contracts; the existing full-width/gap and rewritten-membership contracts
are rerun. The periodic Miri fixture has 8,194 rows / 512 queried positions,
versus native 131,074 / 8,192; it exercises the same period ratios and is not
claimed as the full native size. Miri is not a whole-library/OOM proof.

Deliberate wrapping-product and unchecked-refinement defects each fail the
required value assertion; sources are restored byte-for-byte and all 298 core
tests pass afterward. Native selector calls fall from 114,368 to 78,559 for the
same actual-key queries; the independent binary oracle uses 155,648 calls.
The periodic probe-budget test also passes the predecessor, so it does not
alone detect removing this optimization.

Five full-workspace/all-feature release executables and source hashes are
frozen in `refined-provenance.json`. The first unchanged qualification case
completes all 60 groups (180 observations) and fails short mixed operation time:
+6.43%/-0.38% against the identical original labels. Same-binary median difference
is +6.85%, also outside the 5% calibration limit. CPU, RSS, writes and final
private space pass. The conservative operation-time 95% interval is
[-2.28%, +17.44%]; this is not a passing bound. The pipeline stops before later
native/SWMR/ownership/resource stages. Separate diagnostics complete 12 plain triples, 12 sampling pairs and four I/O
pairs (68 runs), using identical extra CPU-clock calls in both revisions.
Plain phase CPU is 28.21ms current versus 27.40/26.40ms original; the sampling
series instead gives 28.35/29.34ms. These release-debug diagnostic executables
are not the frozen qualification binaries. Positional reads are identical:
16,384 calls / 524,288 requested bytes. The original writes use seek/write, so
its zero interposed `pwrite64` count is not zero writes. Only 14 current main
samples are delivered, so function attribution remains coarse.

A fixed additional 180-group replication is declared before execution, with an
independent fixed seed and the same frozen binaries, work, gates and caps.
The failed initial 60 groups remain in the 240-group pooled decision; initial,
replication and pooled analyses are retained separately. This is the sole
extension for this candidate/case; a pooled failure stops qualification.
Diagnostic observations never enter that pool.

The additional 180 groups are complete. All 240 groups (720 observations)
pooled **still fail** operation time: 37.51/37.30ms original versus 39.61ms
current, +5.62%/+6.22%. The same-binary median difference falls to -0.56%,
while the conservative 95% operation interval is [+2.44%, +11.61%]. Whole-child
CPU falls 1.49%/1.21%, maximum RSS falls 5.52%, and writes/final private lengths
pass. Whole-child CPU includes fixture creation and cannot stand for mark-only
CPU. Final private length is not a peak-storage measurement. The pipeline stops;
this runtime's later native/SWMR/ownership and 23 separate resource probes do
not execute. No further extension is planned for this unchanged candidate.

The native mixed mark path adds exact private-directory membership: original
`317fc98` framing/writing is O(1) per mark; current fallback is O(log N), with
O(1) verified uniform/estimated hits. M marks are worst-case O(M log N). The
lazy/nonresident first boundary scan can separately visit O(N) frames, but the
failing writer fixture already has verified private rows, so that scan does not
explain this measurement. Coarse sampling locates current userspace samples in
cached search, but does not quantitatively attribute the full 5.62-6.22% cost.

Exact-head CI #102 finishes 68 success / two cancelled macOS jobs / failed
required aggregator. Both cancelled jobs have runner_id=0 and no steps or logs;
the aggregator rejects abandoned `test`/`platform-io` groups. The available API
does not expose GitHub's underlying abandonment reason. Kani default/all and
all three CodeQL languages pass. Sanitizers #77 passes all three jobs. A rerun
of failed/unfinished jobs was requested, preserving the original failed run and
unchanged gate. All earlier failed
or partial observations below remain retained.

## Rejected adaptive candidate

Runtime `a2c51a57f3498acbc5c1d99dac50695b2affad34` sizes ordinary copy, file CRC
and CRC-verifying source-copy scratch to 1KiB for payloads at most 1KiB, keeping
64KiB chunks for larger payloads. Read/write counts, source identity, checksum
coverage and publication order are unchanged. The proposed decoder extraction
is excluded from this candidate.

Local regressions pass: **1,382 all-feature / 846 default** tests and doctests,
30/20 ignored, whole-workspace Clippy, formatting, all three new Miri contracts
and three deliberate defect controls. The controls reject a missing copied
byte, ignored source CRC and swallowed output-write failure at the required
assertions; compilation failure or timeout cannot count as detection.

All 13 native cases pass observed gates over 544 observations. SWMR 1/4GiB
write phases also pass across 64 runs with eight following readers and a pinned
reader. The 4GiB result includes all 12 predeclared groups, including the first
four groups' +5.18% CPU failure. The following early native cases pass all
observed 5% gates over eight balanced groups each:

| Workload | Time vs `749532d` | CPU vs `749532d` | Time vs direct `6bcfe0a` |
| --- | ---: | ---: | ---: |
| Complex maintenance | -6.17% | -4.18% | -12.88% |
| Checkpoint rewrite | -12.79% | -5.80% | -16.88% |
| 2M records / 200k fixed marks | -8.81% | +0.00% | -3.21% |

The two long historical cases also pass all observed gates and same-binary
median calibration over 60 groups each (360 observations). Mixed time is
-2.33%/+0.23%, CPU +0.57%/-0.13%; rewrite-mark time is -8.29%/-7.48%,
CPU -26.56%/-26.65%. Same-binary median differences are -2.55% and -0.87%.
The conservative paired-bootstrap 95% intervals for operation time are
**[-4.27%, +5.77%]** and [-9.66%, -4.03%]. Thus the mixed result passes observed
gates but still does **not** establish a statistical 5% upper bound. All 60
groups remain included; no extra confirmation series replaces them.

The complete original-reference suite subsequently fails short mixed marks:
all 60 groups produce +13.69%/+7.26% operation time against the two identical
original-binary labels. Same-binary median calibration is +5.99%, also outside
the unchanged 5% limit. All eight cases (1,440 observations) are retained.
Qualification stops before ownership and separate resource probes; those plans
did not run for this candidate. This is a rejected performance result despite
the native/SWMR passes. Source and five release-binary hashes are frozen in
`adaptive-scratch-provenance.json`.
Exact runtime-head **CI #101 passes all 71 jobs**; **Sanitizers #76 passes all
three jobs**, including full Miri, ASan/leaks and every-target fuzz smoke.

## Rejected `6bcfe0a` qualification

Runtime `6bcfe0a6b137d7249f622883f2ed0b0f1d8966eb` passes complete local
regressions (1,379 all-feature / 845 default tests and doctests, Clippy,
formatting and targeted Miri). Final fixed 2M-record/200k-mark qualification
passes all observed 5% gates over eight balanced four-version groups:

| Metric | vs `749532d` | vs direct predecessor `6009cd8` | vs initial `0e9abd7` |
| --- | ---: | ---: | ---: |
| Mark operation time | -6.89% | -5.41% | -19.87% |
| Whole-process CPU | +3.78% | +1.34% | -0.94% |
| Maximum RSS | +1.17% | +0.27% | +1.10% |
| Final private temporary bytes | 0% | 0% | -20.00% |

Historical long mixed and rewrite mark cases also pass all observed gates over
60 groups each (360 observations). Mixed time is -1.86%/-1.22% and CPU
+1.55%/+0.27% against the two original labels; rewrite time is -6.37%/-8.55%
and CPU -14.53%/-14.47%. Maximum RSS changes -5.49% and at most +0.66%;
private temporary lengths fall to zero in both measured fixtures.

The same-binary median differences are -0.65% and +2.38%. **The mixed operation's
conservative bootstrap 95% interval is [-4.94%, +5.58%], so this does not establish
a statistical 5% upper bound**, despite passing observed gates. The rewrite
interval is [-9.74%, -1.43%]. No extra confirmation run replaces these results.

All semantic work matches. Subsequent native cases 6, 2, 11, 12, 1, 3, 4 and
5 pass. Lazy 4GiB marks initially exceeded 5% against the direct predecessor;
all 24 predeclared groups pass (+1.95% time, -0.55% CPU). The original eight
groups remain included.

**The complex-maintenance case fails** after all 24 groups: time is +9.29% and
whole-process CPU +5.28% against `749532d` (time -1.07% against direct predecessor
`6009cd8` and -14.02% against initial `0e9abd7`). RSS, writes and temporary lengths
pass. Qualification stops there. Checkpoint, chunks/decode, SWMR, the other six
historical reference cases, ownership and final resource plans did not execute.
This candidate is not all-pass; all partial observations remain in
`final-call-native-rest/`.

A new 12-group four-version diagnostic places complex maintenance and checkpoint
first, followed by fixed, long mixed and long rewrite marks. It compares
`749532d`, `6bcfe0a`, restored ordinary snapshot-read calls, and that restoration
plus the prior inline record validator with a borrowed policy. It uses ordinary
unchanged workloads, full-workspace/all-feature release builds and unchanged
resource limits. Sources are restored before timing. The read-scope diagnostic is complete (240 observations); neither restored
snapshot-read variant removes the complex/checkpoint regression. The subsequent
decode/scratch diagnostic is also retained (240 observations). No measured
child was stopped or overlapped with git compression. Exact runtime head CI run #100 passes **all 71 jobs**, including required CI,
Kani default/all and CodeQL Rust/Python/Actions. Sanitizers #75 passes all three jobs, including full Miri, ASan/leaks and fuzz.
The preceding remote `e41c4b5` passed all 71 CI jobs and all three sanitizer jobs,
but its short-prefix implementation failed performance and was reverted.

## Implementation units

1. **Resolve native decoder fields at compile time** (`7525051`). Native header
   and footer fields use offsets derived from the existing layout table, instead
   of constructing intermediate layout values and dispatching roles at runtime.
   The original decoders remain test-only differential oracles. Truncated and
   interrupted reads, mid-read errors, all byte positions/values in the footer,
   literal fields, CRC, flags and predecessor bounds remain checked.
2. **Pack native private rows within the existing allowance** (`60a9789`, `b4685c4`). The
   offset, block id and committed flag occupy **13 bytes**, without the previous
   three padding bytes. At 131,072 records, the old 16-byte rows filled the 2-MiB
   buffer; appending internal metadata then spilled it, adding directory reads
   to random marks. The packed rows retain this working set within the same
   allowance. A scaled regression fails with the original row width and passes
   with the packed representation. Sorted cache searches use the standard slice
   partition operation and continue to compare actual private rows.

   Rewrite construction and validation still use complete 62-byte rows; the
   published resident directory can compact them to the same 13-byte rows.
   Conversion borrows at most 64 KiB from the unused part of the existing index
   allowance. After releasing the full rows, optional promotion accounts for
   both copies and future read caches within the released allowance. Insufficient
   budget, allocation refusal or I/O failure retains a complete usable
   representation. This is private temporary data; persisted record encoding is
   unchanged. Completed chain remaps are released before adoption so their spill
   files do not overlap optional compaction. After the replacement file/snapshot
   opens successfully, rebind releases the previous private directory before
   constructing the replacement; retained readers keep their own snapshots.
   Boundary-budget loans round down to
   whole source rows, never exceeding half the source allowance or 16 KiB; a
   sub-16-byte loan is not used for two u64 buffers.

The initial compaction implementation used the full unused allowance during
conversion. Its measured peak RSS rose **9.09%**, failing the unchanged 5% gate.
Qualification was stopped, and its partial measurements are retained as a
superseded implementation. The 64-KiB conversion limit addresses that overlap.
A second variant still failed mixed-mark time (**+6.27%, all 20 pairs**) and
raised sampled private temporary allocation from **4,149,248 to 4,894,720 bytes**.
Releasing the completed remap and the replaced private directory restores that
temporary peak. Standard slice
search alone did not qualify the mixed case (initial five pairs: **+13.37%**);
the resident-row packing addresses the observed spill threshold. The subsequent
packed/lifetime implementation (`b4685c4`) reduced mixed-case peak RSS by 7.94%
and eliminated its private spill bytes, but mixed marks still took **7.72% longer
across all 20 pairs**. That timing gate remains a failed intermediate result.
None of these
superseded series is counted as final passing qualification.

Unit 2 local regression results: core **286 all-feature / 272 default** tests pass
(three ignored each), **47 public integration tests**, workspace/all-target/
all-feature Clippy and formatting pass. All **six new targeted Miri tests** pass
with ordinary validity/alias checks. Unit 1 separately passed 280/266 core tests,
33 public regressions, Clippy, formatting and its two new differential tests
under Miri.

The allocation-refusal sweep checks errors before publication and successful
fallback after optional directory-allocation refusal, with retained readers,
exact boundary checks and subsequent writes. The boundary-buffer refusal hook
was reduced from 8 KiB to 4 KiB because row-aligned budgets now reserve slightly
less than 8 KiB per buffer. Both refusal points must actually be reached; the
16-KiB peak assertion is unchanged. Short-I/O promotion tests preserve the old
representation and retry after repair. Full-width loan tests include `usize::MAX`.

## Fallible clone reservation

Inlining `String::try_clone` alone did not qualify nested strings: the original
20-group std control remained **7.51%** slower (the preceding implementation was
8.41% slower). Two safe API prototypes (preinitializing empty strings and mapping
into reserved vectors) also failed to remove the cost. Their raw measurements
are retained. Rust 1.95 still rejects `Vec::try_with_capacity` and
`String::try_with_capacity` as unstable.

The broad direct-reservation prototype (`46b95b8`) recovered nested-string time,
but failed the small nested-Box RSS gate: **1,208 → 1,356 KiB (+12.25%)** across
all 20 pairs. Its completed ownership/native results and incomplete SWMR run are
retained; the latter was stopped after the failure was established. Mapping-level
probes found identical **151,552-byte heaps**; extra resident executable pages
caused the increase. Outlining error construction did not qualify either
(**+6.31% RSS** against one identical-binary control).

The revised implementation confines direct empty reservation to **String bytes**.
Generic `Vec<T>` clones keep `try_reserve_exact`, and String cloning retains its
original function-call boundary. A checked `Layout::array`, global allocation and
null check transfer exactly the requested allocation into a zero-length vector.
Ordinary vector copying initializes the bytes; the source String guarantees
valid UTF-8. Zero-length requests allocate nothing. Error variants/resource names
are unchanged. This narrows the new allocation/layout ownership boundary; it is
not a whole-owner initialization proof or a general OOM-kill guarantee. Public
Clone compatibility, the separate jzap-std scope and the BTreeMap deferral remain.

Tests cover over-alignment, full-width capacity overflow, refusal, only dropping
initialized elements, empty strings, Unicode and embedded NUL. The full-width
Kani harness directly checks the reservation helper. An additional registered
harness checks actual String cloning, refusal, unchanged source and cleanup for
ASCII inputs of up to four bytes. That bounded scope does not imply an arbitrary
String initialization proof. The Box mutation target includes its error context
so the new vector null check cannot make that existing mutation ambiguous.

## Bounded resident-key search

`578f9d8` reduces searches in resident buffers containing at least 1,024 rows.
An average-spacing estimate is checked against the actual row and adjacent
keys. When that does not bracket the answer, one correction can propose a
33-row window; its real endpoint keys must contain the requested key before
binary search is narrowed. Otherwise ordinary binary search remains. Returned
floors always come from actual private rows. No allocation, added cache, larger
budget or file-format change is involved.

The independent sorted-vector oracle now includes 1,024/1,025-row inputs,
irregular gaps, large discontinuities and full-width offsets, checking exact
keys and their neighbors. Forcing the estimated row to be accepted without a
key check makes this test fail at 1,024 rows. Restored production passes the
core default/all suites, 47 targeted public regressions, Clippy and all three
cached-floor tests under Miri (ordinary validity/alias checks).

Search-unit qualification compares `578f9d8` with both its preceding unit
(`a397b8e`) and the initial PR head (`0e9abd7`). Construction-page qualification
compares `867da61` with `578f9d8` and `0e9abd7`. Each uses randomized three-version
groups; the same candidate samples serve both comparisons. The construction-page SWMR groups
include all four versions, checking each unit and the follow-up through that build.
Five native groups expand to exactly twenty if either comparison fails any
5% metric; SWMR starts at three groups and expands to exactly nine if needed.
All observations remain, including an intermediate case that fails its gate.

## Separate rewrite construction pages

A final peak-resource probe found a remaining historical regression: the private
rewrite files peaked at 4,149,248 allocated bytes, versus 3,878,912 on `317fc98`
(about 7% higher). It was unchanged from the initial PR head but still exceeded
the historical resource gate. Full construction rows had inherited the 16-row
page size used for fast published lookup, repeating page headers and offsets.
With the separate published compact directory, `867da61` construction uses the
original maximum of 256 rows per page. Published compact pages remain at 16.
The same budget formula reserves the larger working pages from the allowance;
no resource limit or retained-cache budget is raised.

The new regression crosses both 256-row boundaries, forces allocation refusal,
checks unchanged length and prior contents, retries, and verifies exact and
non-boundary queries around both page transitions. Its initial tiny-budget
fixture selected single-row pages, so the intended refusal was not reached;
the corrected fixture explicitly selects the existing standard 2-MiB budget.
Both test logs are retained. Separate peak probes show rewrite private storage
back at **3,878,912 bytes**, exactly matching the original reference. Complex
maintenance remains at **4,194,304 bytes**. Core all-feature/default tests pass
**289/275**, with 47 targeted public regressions, workspace Clippy and formatting.
The new refusal/retry test also passes Miri with ordinary validity/alias checks.

## Adaptive mark I/O

A subsequent historical comparison of 65,536 random marks in the approximately
4-GiB mixed-record file still failed its elapsed-time gate. Pure-search
experiments did not explain the full cost; neither the u64 fast-path nor the
slower floating-point arithmetic prototype was applied. A sampling profiler
could not initialize its timer in this environment and produced no CPU samples.
Its successful process exit is not counted as profiling success.

A separate I/O interposer, enabled only around the mark loop, established that
`867da61` requested **35,638,016 bytes** in 130,648 positional reads, whereas the
original reference requested **4,194,304 bytes** in 131,072 reads. The 512-byte
prefix coalesces header/footer reads for small records but copies unused payload
bytes for large ones. `749532d` selects the existing header-only framing route
when there are at least 32 private directory rows and the file's mean extent is
greater than 512 bytes. Otherwise it preserves the 512-byte route. The hint
selects an I/O strategy only: it never authorizes a boundary, changes framing or
skips header/footer validation. There is no new allocation, cache or file cap.
Saturating arithmetic handles the full-width count without wraparound.

The candidate requests **4,194,304 bytes** in the same mixed workload. These
instrumented timings are diagnostic only; the post-change probe was not pinned
like the initial probes. The interposer counts `pread64`/`pwrite64`; the original
mutable writes use seek-plus-write, so its zero `pwrite64` count is not zero
writes. The raw sources, commands and failed sampling attempt are retained.

Two pure-memory regressions check full-width geometry against a u128 oracle and
complete framing on both I/O routes, including every actual read failure point,
repair/retry and a truncated footer. Core **291 all-feature / 277 default** tests,
47 targeted public regressions, Clippy and formatting pass. Both new contracts
pass Miri with ordinary checks. A test-only `ErrorKind` conversion mistake was
corrected before successful qualification; its compiler log remains archived.
The ownership benchmark executables are byte-identical to `867da61`, so the
completed final ownership/std comparisons remain exact-binary evidence.

The first 20 randomized four-version groups reduced time by **9.39%** against
`867da61`, but failed against one original-reference label (+11.81%, versus
-0.10% for the other). Those identical reference binaries differed by 11.92%.
The original label occupied the first position 11/20 times, the candidate 1/20.
All observations remain; this series is not reported as a passing historical gate.
A separately predeclared, Williams design balancing positions and within-group predecessor pairs then ran
20 groups with the same inputs, executables and limits. Every label occupies
each position exactly five times. It passes every 5% resource/time gate and its
same-binary calibration: candidate time is **-9.91%/-6.12%** against the reference
labels and **-0.46%** against `867da61`; reference-label time differs by 4.20%.
This does not establish a universal statistical performance bound.

Subsequent qualification balances every six-group block across all permutations
of its three labels. Native tests start with six groups and add exactly eighteen
if any gate fails; concurrent tests start with six and add six if needed. The
historical reference uses 24 groups per workload. The 5% gates, workloads and
resource limits are unchanged, and earlier adverse series remain separate.

## Verification and measurement method

Every implementation unit has separate regression and before/after measurements.
Runtime comparisons use release builds, the same workload executables and
inputs, and no simultaneous compilation, tests or other benchmarks. Native
samples run on the same CPU; concurrent workloads retain the same four-CPU
quota. The memory limit remains 16 GiB. Allocation-refusal checks return null
synthetically; they do not reproduce an operating-system OOM kill.

Early two-version native units start with five alternating pairs per case. A
metric above 5% triggers exactly fifteen further pairs; all twenty are pooled.
The search and construction-page units apply the same rule to randomized
three-version groups; the final adaptive-I/O unit uses the balanced six/twenty-four
group plan described above. Metrics are
operation time, whole-process CPU, maximum RSS, physical writes and temporary
bytes. In `record_mutation_load rewrite`, the timed operation is random marking
**after** defragmentation; the CPU total includes fixture creation and rewrite.
The `oom_maintenance_load complex/checkpoint` timer measures defragmentation
itself. These timing scopes are reported separately. Temporary bytes are a separately checked resource rather than a semantic
work-equality field, because compacting the directory intentionally reduces
them. Record counts, marks, source bytes and frame counts must still match.

Single-writer/multiple-reader runs use 1/4 GiB, eight following readers and a
pinned reader, with 16,384 checks per reader per epoch. Reader work, snapshots
and trace hashes must match. Separate 5-ms resource probes inspect private
temporary files and process/cgroup memory; their sampled time is not substituted
for the timing series. RSS alone does not account for the filesystem cache.

Historical-reference comparisons use the unchanged current mutation harness on
`317fc981e5cb78eec26f6820c9e010baf2873bcb`. Ownership controls use the original
dispatch-inside-loop harness against `213e234`. Identical-binary labels and
longer-work controls are reported separately, never pooled with a different
workload. Earlier adverse samples remain evidence of their specific builds and
measurement conditions.

## Construction-page native measurements

All 11 cases pass the unchanged 5% time, CPU, peak RSS, write-volume and
temporary-space gates against both comparison versions. Positive values are
regressions; negative values are improvements. Time is the workload-specific
operation timer; CPU includes the whole child process. All 255 observations
remain in `build-pages-native/samples.json` in the evidence archive.

| Work | Groups | Time vs preceding unit | Time vs initial PR | CPU vs initial PR | RSS vs initial PR |
| --- | ---: | ---: | ---: | ---: | ---: |
| Fixed marks | 5 | +1.81% | -11.45% | -0.09% | -0.07% |
| Variable random marks | 5 | +0.46% | -14.21% | -0.93% | -0.09% |
| Random marks after rewrite | 5 | -0.76% | -45.40% | -9.73% | -1.78% |
| Recovery open | 5 | +0.41% | -14.77% | -7.41% | -0.64% |
| Large reverse marks (~4 GiB) | 5 | -7.55% | -11.60% | +3.04% | -5.55% |
| Lazy large reverse marks | 5 | -3.15% | -8.36% | -3.52% | -5.66% |
| Mixed random marks (~4 GiB) | 20 | +4.42% | -5.18% | -0.51% | -8.28% |
| Complex defragmentation | 5 | -18.22% | -22.36% | -18.43% | -4.59% |
| Checkpoint defragmentation | 5 | -9.94% | -14.33% | -10.07% | -0.05% |
| Chunk encoding (64 MiB) | 20 | -1.16% | +0.25% | -0.09% | +0.14% |
| Decoding (64 MiB) | 5 | -3.16% | -2.04% | -3.04% | +0.09% |

## Construction/search concurrent measurements

All three comparisons pass at both sizes, with identical reader work, snapshots
and trace hashes. There are three four-version groups at 1 GiB and nine at
4 GiB (48 workload executions total). Final construction-page and preceding
search executables for this workload are byte-identical; their comparison is
also a same-binary calibration, recorded in `final-unaffected-binary-identity.json`.
The initial three 4-GiB groups exceeded the CPU gate only for these identical
executables (+5.10%). The predeclared expansion retains all nine groups; the
pooled same-binary CPU difference is +0.13%. No failing initial group was removed.

| Size | Groups | Comparison | Wall | CPU | Peak RSS | Writes | Gate |
| --- | ---: | --- | ---: | ---: | ---: | ---: | --- |
| 1 GiB | 3 | search-unit | -0.93% | -0.21% | +0.34% | -0.00% | passed |
| 1 GiB | 3 | pages-unit | -0.49% | -1.73% | -0.09% | +0.00% | passed |
| 1 GiB | 3 | overall | +0.98% | -0.19% | -0.12% | +0.00% | passed |
| 4 GiB | 9 | search-unit | +2.39% | +4.04% | +0.03% | +0.00% | passed |
| 4 GiB | 9 | pages-unit | -0.40% | +0.13% | +0.00% | -0.00% | passed |
| 4 GiB | 9 | overall | -1.85% | -2.90% | -0.09% | -0.00% | passed |

## Adaptive-I/O native measurements

All 11 workloads pass every observed 5% gate against both `867da61` and
`0e9abd7`. These are the exact `749532d` executables. All 252 observations
remain in `read-hint-native/samples.json` in the archive.

| Work | Groups | Time vs preceding unit | Time vs initial PR | CPU vs initial PR | RSS vs initial PR |
| --- | ---: | ---: | ---: | ---: | ---: |
| Fixed marks | 6 | +1.52% | -13.88% | -3.98% | -0.07% |
| Variable random marks | 6 | +4.64% | -3.12% | -2.90% | +1.36% |
| Random marks after rewrite | 6 | +4.74% | -44.18% | -5.48% | -1.64% |
| Recovery open | 6 | -1.16% | -17.75% | -10.83% | +1.38% |
| Large reverse marks (~4 GiB) | 24 | -3.10% | -9.09% | -0.04% | -7.23% |
| Lazy large reverse marks | 6 | -2.44% | -12.51% | -1.04% | -5.66% |
| Mixed random marks (~4 GiB) | 6 | +0.07% | -5.28% | -2.40% | -8.28% |
| Complex defragmentation | 6 | -5.73% | -23.42% | -17.66% | -4.60% |
| Checkpoint defragmentation | 6 | +1.70% | -15.70% | -11.82% | +0.14% |
| Chunk encoding (64 MiB) | 6 | +1.65% | -2.08% | -1.57% | +0.15% |
| Decoding (64 MiB) | 6 | +1.19% | -1.03% | -1.95% | +0.17% |

## Adaptive-I/O concurrent measurements

Both sizes pass against the preceding unit and initial PR head. Each size uses
six balanced three-version groups (36 workload executions total). All reader
work, snapshots and trace hashes match. Final logs/reports remain in
`read-hint-swmr/`; no additional groups were needed.

| Size | Comparison | Wall | CPU | Peak RSS | Writes |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 GiB | preceding unit | +1.49% | +1.36% | -0.03% | +0.00% |
| 1 GiB | initial PR | -2.59% | -4.64% | -0.05% | -0.00% |
| 4 GiB | preceding unit | +2.22% | +3.97% | -0.02% | -0.00% |
| 4 GiB | initial PR | -2.78% | -1.65% | -0.06% | +0.00% |

## Ownership measurements through the adaptive-I/O build

The two ownership executables at `749532d` are byte-identical to those used for these
completed comparisons; `read-hint-ownership-identity.json` records both hashes.
All six workloads pass the observed 5% time/CPU/RSS/write gates against `0e9abd7`:
64-MiB and 1-GiB byte cloning, nested strings, Box, nested Box and shared owners.
The 1-GiB clone's operation time changes +4.81%, CPU +2.48%, with unchanged
approximately 2-GiB peak RSS. All 60 observations remain in `final-owned/`.

The original dispatch-inside-loop ownership harness is also retained, rather
than replacing it with a favorable dispatch-outside-loop benchmark. Each row
below has 20 randomized groups with two labels of the same reference executable.
`213e234` is the Box/shared reference; `2f2d818` is the std nested-string reference.

| Work | Time vs reference A / B | CPU vs reference A / B | Maximum RSS vs reference A / B | Same-binary time B vs A |
| --- | ---: | ---: | ---: | ---: |
| 10M Box creations | +1.15% / +1.74% | +0.90% / +1.41% | -1.95% / -1.95% | -0.58% |
| 8,000 nested Box clones | -30.54% / -30.56% | -30.41% / -30.51% | -8.16% / -7.35% | +0.04% |
| 10M shared-owner operations | +1.42% / -0.01% | +1.29% / -0.19% | -7.14% / -7.14% | +1.43% |
| 16 nested-string iterations | +0.84% / +2.98% | +1.00% / +2.58% | -1.23% / -1.19% | -2.07% |
| 128 nested-string iterations | +1.05% / +1.72% | +1.05% / +1.63% | -0.82% / -0.75% | -0.66% |

All observed gates pass. The paired-group bootstrap 95% conservative time upper
bounds are +4.75%, -29.89% and +3.73% for Box/nested Box/shared respectively.
Nested strings' bounds reach +8.14% (short) and +5.72% (long), so those observed
passes are **not** statistical proof of a 5% upper bound. Longer-work observations
remain a separate workload, not extra samples pooled into the short case.

The complete 24-group historical native qualification for `749532d` passes six
of eight cases, but **fails** long mixed marks (+4.95% / +5.62% against the two
original labels) and long marks after rewrite (+10.20% / +8.14%). All CPU, peak
RSS, write-volume and temporary-space gates pass. The complete 576 observations
remain in `read-hint-reference/`; the earlier passing focused comparison does
not replace these later adverse observations. This build is not all-pass.

## Keep format copies out of ordinary entry validation

`a046bcf` moves the existing compressed-entry policy checks into a separate cold
function. Unknown flags and internal-record identity/version checks still run
before accepting ordinary records. Compressed entries retain descriptor, kind,
algorithm availability and internal-record restrictions. There is no new
allocation, unsafe block, public signature or file-format change.

The preceding release assembly copied all **664 bytes of FormatSpec** on every
ordinary mark just to call validation; uncompressed records do not use the
format settings. The now-small inline validator keeps that copy and the
compressed-policy call on the compressed branch. This is a code-generation
observation, not by itself a measured speedup. Core 291/277, all 47 targeted
public regressions, workspace Clippy, formatting and release builds pass.
The existing all-feature regression exhausts every u16 flag word on both read
routes. The first build command selected the wrong package for ownership
examples; the corrected workspace build passes and its initial error is retained.
A redundant inline attribute in the initial prototype was removed before the
qualified build; the warning log remains.

The 20-group focused mixed test passes against both original-reference labels
(-6.94% / -2.84% time), but **fails against preceding `749532d` (+7.09%)**.
All resource gates and the same-binary calibration pass. The 20-group long
post-rewrite test passes against both original labels (-2.49% / +0.50%) and the
preceding unit (-4.21%). These 160 observations remain in `entry-validation-*`;
this candidate is not all-pass. Prior ownership binaries changed at this build,
so their previous exact-binary identity does not extend to this candidate.

## Expose constant-size snapshot reads to the caller

`3b85eb7` adds an inline hint to `SnapshotFile::read_exact_at`. Its range checks,
checked offsets, partial-read loop, interrupted-read retry and errors are
unchanged. Release framing calls no longer use a separate `read_exact_at`
function or materialize its successful Result across that call boundary. Code
size changes are included in subsequent RSS measurements; inlining alone is not
evidence of speedup.

The unit passes 291/277 core tests, 47 public regressions, Clippy, formatting and
release builds. All eight snapshot contracts pass Miri with ordinary checks,
including full-width overflow, snapshot extent, fallible rebinding, range copying
and file-object identity. No new unsafe block or memory allocation is introduced.

Twenty predeclared five-version Williams groups per long workload compare
`3b85eb7` with both predecessors (`749532d`, `a046bcf`) and two labels of the
original reference. Every version occupies every position four times, and every
directed within-group predecessor pair occurs four times. These measurements
are complete. Mixed marks pass every observed implementation/reference metric,
but the identical-reference time difference is **5.14%**, exceeding calibration.
Long post-rewrite marks still fail time (+6.41% / +10.90% against the original
labels and +7.58% against `749532d`; +1.08% against `a046bcf`). The complete 200
observations remain in `inline-read-mixed/` and `inline-read-rewrite/`; neither
series is called fully qualified.

## Reduce repeated published-directory decoding

A temporary instrumented build of `3b85eb7` split the actual mark path into
boundary lookup, framing and writing. Three runs of each workload put long
post-rewrite boundary lookup at **104–122 ms** out of **386–461 ms** total;
long mixed lookup took 23–37 ms. These clocks add overhead and are diagnostic,
not qualification. The instrumentation source patch, restored-source hash and
all results remain in `stage-profile-*`. Production `file.rs` was restored
exactly before further changes.

`1006f2d` uses eight-row pages for the published compact native directory, while
construction retains its maximum of 256 rows. The same directory allowance and
promotion accounting apply; no limit or retained-cache budget increases. Smaller
pages decode fewer unrelated rows during point membership lookup. A range hint
also reuses the actual key just read when it exactly matches the requested
offset, instead of repeating a cached search. A guessed position alone never
authorizes a boundary.

The new regression tests both compact and full fallback representations, gaps,
randomized cache access, exact/neighbor offsets and successively truncated
logical prefixes. Replacing key equality with unconditional acceptance makes its
independent membership oracle fail; the restored implementation passes. Core
**292 all-feature / 278 default**, 47 public regressions, Clippy and formatting
pass. The new test also passes Miri with ordinary checks (171.24 seconds).

Nine separate 5-ms resource probes pass the unchanged 5% RSS/private temporary/
open-file-allocation comparisons. Rewrite private storage is **3,878,912 bytes**,
exactly the original reference; sampled RSS is 6,586,368 versus 6,467,584 bytes
(+1.84%). Complex private storage remains **4,194,304 bytes** and sampled RSS is
8,388,608 bytes. These sampling runs do not replace timed comparisons. The
predeclared twenty five-version groups per long workload completed. Long mixed
marks fail against one original label (+6.80%, versus -6.85% against the other);
the same reference executable differs by 14.66%, failing calibration. Long
post-rewrite marks pass all observed/calibration gates. All 200 observations
remain in `membership-mixed/` and `membership-rewrite/`.

The additional **2,000,000-record rewrite** exposes a resource regression that
the smaller fixture does not: final private storage grows from **6,944,836 to
9,878,203 bytes (+42.24%)**. The eight-row published page size is therefore
rejected. Its single diagnostic timing pair is not performance qualification.

## Reuse retained encoded pages

`d7e2247` restores published compact pages to **16 rows** and keeps construction
at a maximum of 256. Decoding borrows checked slices of immutable retained
encoded bytes directly; only spilled or cross-boundary input uses an allocated
read scratch buffer. The four existing decoded cache slots are unchanged. A
constant-size latest-slot index searches the most recently verified page first;
older pages continue to tighten bounds using actual keys. Successful exact-key
reuse from the preceding unit remains. No new unsafe block, dependency, public
API, file encoding or resource limit is introduced.

The new refusal test primes all four slots, then decodes a retained page whose
input is larger under persistent allocation refusal. It corrupts the final
varint after a decoded prefix has been initialized, checks repeated errors and
rejection of that partial prefix, repairs/retries, and checks clock wraparound,
boundary keys and gaps. Forcing a scratch copy makes the no-allocation assertion
fail; retaining partially decoded rows makes the invalid-cache assertion fail.
Both mutations are restored and their logs/patches retained.

Core **293 all-feature / 279 default** tests (three ignored each), 47 public
regressions, workspace/all-target/all-feature Clippy and formatting pass. All
five packed-row tests pass Miri with ordinary validity/alias checks (59.42 s).
The two-million-record diagnostic restores final private storage to exactly
**6,944,836 bytes**, matching the preceding 16-row implementation. Its timing is
diagnostic only. Nine separate resource probes put rewrite private peak back at
**3,878,912 bytes**, exactly the original reference; sampled RSS is 6,520,832
versus 6,508,544 bytes (+0.19%). Complex private peak remains **4,194,304 bytes**.

Final exact-binary qualification uses four versions: `749532d`, `3b85eb7`,
`0e9abd7` and `d7e2247`. Native Williams blocks start with eight groups and add
exactly sixteen if any 5% gate fails; SWMR starts with four and adds eight.
The thirteen native workloads include the two-million-record rewrite and a
mixed-record rewrite. Historical reference comparisons balance every six-group
block, with 24 groups for cases 0–5 and 60 for the two long mark cases. Ownership
comparisons rerun because the latest executable hashes changed. All planned
observations and failed intermediate builds remain evidence. That series passed four native workloads (cases 6, 2, 11 and 12; eight groups
each), then failed fixed marks (case 0) by **6.07%** against `749532d` after all
24 planned groups. Its initial eight groups had been 8.27% slower. All other
metrics/comparisons passed for that case. The pipeline stopped at the failed
gate; unexecuted SWMR/historical/ownership plans are not results. The complete
224 observations remain in `final-retained-native/`. This build is not all-pass.

Exact-head remote CI and sanitizer status is reported separately on the existing
PR. No merge or release is included.

## CI checks for the new internal representation

The first remote run on `d7e2247` exposed two test-maintenance omissions. The
source-enforcement gate still required the exact `SpillRows<16>` field spelling,
and Rust 1.95 compile-fail snapshots still described the old non-Sync type path.
Native platform/feature tests and ASan failed that structural assertion; MSRV
failed the diagnostic comparison. These are retained as failed runs, not green
validation of that head.

`aabd895` updates the private-field check to the named row-width constant and
rejects mutable `SpillRows` return types regardless of width. Public-field and
mutable-return negative controls both fail. All 16 enforcement tests and Clippy
pass. The four reader-sharing snapshots now show 13-byte rows and both private
rewrite variants; each fixture still fails compilation with E0277. All compile
contracts pass on Rust 1.95 in default/all configurations with snapshot
comparison enabled after review; no CI gate is skipped or relaxed.

The benchmark driver was paused between measured child executions while these
local checks ran. The active child completed normally before compilation began;
its observation and all earlier samples remain. Production-source hashes match
the frozen `d7e2247` binaries exactly, so timing continues on those same binaries.
The pause and all regression/mutation logs are in the evidence archive. Remote
CI on `aabd895` passed all three CodeQL languages and storage failure/resource
contracts, then found another stale structural expectation: the validation-call
catalog still required the setter to call the old prefix wrapper directly. The
updated catalog requires both setter-to-I/O-selector and selector-to-framing
edges. Removing either edge makes the syntax-tree regression fail.

The complete local workspace passes **1,379 tests/doc-tests with all features**
(30 existing ignored) and **845 with default features** (20 existing ignored).
This includes the structural gates and compile-fail contracts omitted from the
earlier targeted public subset. An initial full build exhausted the fixed
32-GiB disk with accumulated debug/incremental artifacts; that failed log is
retained. Only generated local debug/trybuild caches were removed, and these
full runs disable debug symbols and incremental compilation. Debug assertions,
code semantics, resource limits and frozen release benchmark binaries are
unchanged. No disk or memory allowance was raised. The second benchmark pause
is likewise recorded between measured children without dropping observations.
At that snapshot, remote validation of the test-catalog repair was pending.
Later runtime heads pass the complete unchanged CI catalog, as listed above.

## Short prefixes for observed small frames

`e41c4b5` uses the existing observed uniform-prefix spacing only to choose an
initial 128-byte read for offsets inside its small-frame range. Boundary
membership is still checked separately before the hint is used. Other records
keep the existing 32/512-byte selection. All routes use the same checked frame
decoder and read an additional footer when it does not fit the prefix; a false
hint does not change acceptance, allocation policy or the public/file format.

The small-frame range test includes near-u64-maximum offsets, truncation and a
gap that can receive the read hint but must not become an authorized boundary.
The actual decoder regression now covers both hint values and six lengths,
including a deliberately inaccurate hint for 4-KiB payloads. Every actual read
failure point, repaired retry and truncated footer remain checked. Disabling
the short route makes its request-size regression fail; the restored code passes.
Both targeted tests pass Miri with ordinary checks. Full workspace results are
**1,380 all-feature / 846 default tests and doc-tests**, with unchanged 30/20
ignored tests; full Clippy and formatting pass.

An instrumented copy of the same harness confirms **200,000 reads and
25,600,000 requested bytes** for 200,000 marks in the two-million-fixed-record
fixture, with 200,000 writes and unchanged semantic work. Its clocks add
substantial overhead, so that diagnostic time is excluded from qualification.
The exact uninstrumented release binaries are recorded in
`short-prefix-provenance.json`. Final comparisons retain the same workload,
5% gates and 4-CPU/16-GiB limits, replacing the unit-before label with `d7e2247`
and testing fixed marks first. That series completed all 24 fixed-mark groups and **failed time** against
`749532d` (+26.84%), `d7e2247` (+22.02%) and `0e9abd7` (+9.77%). All other
metrics passed, but qualification stopped at case 0; the remaining plans did
not execute. All 96 observations are retained. `570b64c` reverts the short-prefix
implementation rather than keeping a resource reduction that worsens time.

A subsequent 12-group factorial diagnostic keeps the same mark loop and adds
only two thread-CPU clock reads. The four variants are 512/128-byte prefixes
with and without the snapshot inline hint. Median mark wall/CPU seconds are
0.151695/0.151693 (512 inline), 0.178016/0.178010 (128 inline),
0.158740/0.158699 (512 plain call), and 0.189656/0.189632 (128 plain call).
This does not support removing snapshot inlining as the fix. Source restoration
hashes and all 48 observations remain in `prefix-inline-factorial/`.

Instrumented component probes of those exact executables observe extra CPU
inside the positional-read calls for the 128-byte variants. A separate C
control removes library parsing and holds the executable and buffer constant:
200,000 read/write pairs at 720-byte strides in a fresh 144,179,200-byte file.
Across 12 balanced four-size groups, 76/128/256/512-byte reads take median CPU
0.146129/0.145058/0.141560/0.120225 seconds respectively. Thus fewer requested
bytes do not imply faster positional I/O in this environment. These diagnostics
neither qualify library performance nor establish which kernel/copy mechanism
causes the difference. All sources, plans and observations are retained.

## Inline existing footer and membership checks

A separate 12-group diagnostic compares `749532d`, restored `d7e2247`, an inline
footer decoder, and an inline footer decoder plus the verified-boundary
predicate. All four keep the 512-byte route and identical function bodies.
Median mark wall/CPU seconds are 0.152810/0.152807, 0.154124/0.154123,
0.146738/0.146736, and 0.146493/0.146479 respectively. The combined variant is
about 4.95% faster than `d7e2247` in this diagnostic; maximum RSS is unchanged
at 5,836 KiB and final private storage is unchanged at 25,165,764 bytes.
All 48 observations and restored source hashes are retained in
`inline-boundaries-diagnostic/`. These phase-clock measurements select a
candidate; they do not replace ordinary-harness qualification.

The selected change adds only inline attributes to the existing native-footer
decoder and verified-boundary predicate. Neither function body, read size,
validation, error, allocation, signature nor file encoding changes. Complete workspace regressions pass: **1,379 all-feature / 845 default**
tests and doctests, with 30/20 ignored respectively. Whole-workspace Clippy,
formatting, and both targeted Miri tests pass (footer corruption/pointer edges
and observed-prefix membership/truncation). Ordinary-binary performance
qualification failed at fixed marks across all 24 planned groups: **+8.64%**
against `749532d`, -1.09% against `d7e2247`, and -6.81% against `0e9abd7`.
All other metrics pass; the remaining planned series did not execute. All
96 observations remain in `final-footer-native-fixed/`.

The final plan is declared before candidate timing: native comparisons use
8 balanced four-version groups, extended to 24 on any failed metric; SWMR uses
4 groups, extended to 12. Historical controls use **60 groups per case**, each
six-group block containing every permutation of the candidate and two labels
of the same original binary. Fixed marks and the previously failing long
mixed/rewrite cases run first. All samples are retained. Both observed gates
and the same-binary median calibration must stay within 5%; bootstrap
intervals are reported separately. Increased repetition does not change the
work per observation, performance threshold, or resource budget.

## Borrow policy during entry validation

Release assembly of `1994eb7` shows an unconditional **664-byte memcpy** at the
start of `write_framed_record_flags`, before checking whether the entry is
compressed. Thus the earlier cold-helper extraction did not eliminate this
copy in the final call graph. The internal `validate_record_entry` now borrows
`FormatSpec`; its eight callers pass references. The compressed branch still
receives the same policy value and performs every existing check. Public APIs,
read sizes, buffers, file encoding and error variants remain unchanged.
At `6009cd8`, all-feature 1,379/default 845 tests and doctests pass, with
30/20 ignored; Clippy, formatting and the adaptive-read/I/O-retry Miri contract
also pass. Release assembly now places the 664-byte copy **after the compressed
flag branch**, removing it from ordinary records. The assembly before and after
is retained. The final four-version unit comparison uses `1994eb7` as the direct
predecessor, with `749532d` and initial `0e9abd7` as the other references;
historical, SWMR, ownership and resource plans otherwise remain unchanged.
Ordinary-binary qualification stopped after all 24 fixed-mark groups: time
is +6.85% against `749532d`, +1.70% against direct predecessor `1994eb7`, and
-7.65% against initial `0e9abd7`. All remaining metrics pass. All 96 observations
remain in `final-borrow-native-fixed/`; this is not all-pass qualification.

Assembly also shows that the former inline mark/validation/write path became a
separate `write_framed_record_flags` call after the read routine was expanded.
A predeclared 12-group factorial diagnostic uses the **ordinary unchanged
harness** for fixed 2M/200k marks, mixed 4GiB/65,536 marks and rewritten variable
200k/200k marks. It compares current, inline-write, plain-read and both variants,
with balanced four-version order and all source/binary hashes retained. Only
attributes change; function bodies and validation remain identical. Across all 12 groups per workload (144 observations), inline-write changes
fixed/mixed/rewrite time by **-11.73%/-9.40%/-7.82%** against `6009cd8`;
whole-process CPU changes +0.13%/+3.19%/-5.56%. Maximum RSS is lower in all
three cases and temporary lengths are identical. The plain-read variant slows
mixed marks by 11.51%; combining it with inline-write still slows that case by
4.64%, so only inline-write is selected. All samples and adverse variants remain
in `call-shape-diagnostic/`. These results select a candidate and cannot
substitute for final qualification.

The selected correction adds `#[inline]` to the existing private
`write_framed_record_flags` method. Its body and all framing/flag checks are
unchanged; the final four-version unit comparison uses `6009cd8` as the direct
predecessor alongside `749532d` and initial `0e9abd7`. At `6bcfe0a`, complete regressions pass: **1,379 all-feature / 845 default**
tests and doctests, 30/20 ignored; whole-workspace Clippy, formatting and the
adaptive-read/I/O-retry Miri contract also pass. The five final release
executables and changed-source hashes are frozen in
`call-shape-provenance.json`. Its completed qualification fails complex
maintenance as detailed in the rejected `6bcfe0a` section above.

## Maintenance diagnosis and adaptive record scratch

The read-call/inline-validator diagnostic completed all 240 observations.
Neither alternative fixes maintenance: against `749532d`, complex time changes
+11.71%/+10.95%, and checkpoint time +12.77%/+8.70%. No alternative is selected.
Source and binary hashes, including the corrected frozen-baseline source
provenance, remain in `read-scope-diagnostic/`.

A separate profiler adds two CPU-clock calls around the actual maintenance
phase and requests ITIMER_PROF sampling every millisecond. Kernel delivery can
be coarser; these are diagnostic locations, not exact per-function timing.
Both builds use release debug information and the same profiler. Across 12
pairs, packed-page decoding receives 65 old / 151 current main-image samples;
external library calls dominate both. Median phase CPU is 0.47875/0.53666 s.
Memset interposition records about **7.37/7.36 GB** initialized in each phase.
That large common cost is not presented as the cause of the increase.

The next 240-observation diagnostic separates page decoding into a function,
then sizes ordinary-copy/file-CRC scratch to 1KiB for payloads at most 1KiB.
Decoder separation alone does not recover the older complex reference, though
it helps long rewritten marks. Small scratch recovers the observed complex and
checkpoint gates in that diagnostic. Its five workloads and all adverse
variants remain in `decode-scratch-diagnostic/`. The decoder extraction is not
included in the final candidate.

The final adaptive candidate applies the same policy to ordinary snapshot copy,
file CRC and the actual source-verifying rewrite copy. The source-verifying path
still checks declared identity, header/payload/footer CRC and returns its CRC
state before publication. Small records require one read/write as before;
records above 1KiB retain their existing 64KiB stream chunk. Separate private
call frames keep the large array out of the small path. No heap allocation,
new user limit, public signature or file format is introduced.

New regressions cover 0/1/1023/1024/1025/65535/65536/65537-byte payloads, short
reads/writes, zero progress, intermediate I/O failure, exact prefixes, repair
and retry, both CRC policies and both liveness policies. Wrong source identity
is rejected before output; a checksum-damaged source remains rejected and the
same snapshot succeeds after repair. Core all-feature **296 tests pass**, with
three ignored. Full workspace regressions, Clippy, three targeted Miri tests and
three defect controls pass. The adaptive qualification passes native and SWMR,
but fails the original-reference short mixed case as detailed above; ownership
and separate resource stages do not run for that candidate. All group counts,
thresholds, workloads and limits retain the declared plan.

## Scope of the recovery

These comparisons qualify the named workloads on the stated builds and resource
limits. They do not erase adverse measurements of earlier builds, establish a
statistical 5% bound for every short timer, or cover arbitrary file sizes. The
historical first lazy/nonresident boundary-discovery pass is still required:
an untrusted offset does not become an authorized record boundary without that
validation. The earlier single-first-mark fixture and its substantial discovery
cost remain documented in `docs/performance/boundary-redesign-20261101/`.
This follow-up reduces the specified mutation, rewrite-directory and clone
costs; it does not claim that initial discovery now matches pre-validation code.

The default spill policy, optional user limits, external mmap mutation contract,
public infallible compatibility APIs, BTreeMap deferral and separate jzap-std
whole-owner proof scope are unchanged. Measured files reach approximately 4 GiB;
there is no terabyte-scale qualification here.

Raw results, scripts, source snapshots and logs are retained in [evidence.tar.gz](evidence.tar.gz), with [per-file hashes](evidence-manifest.json), the [archive checksum](evidence.sha256) and [qualification status](results.json). Later stages that were not executed are listed explicitly in that status.
