# Compound failures and Miri exclusion audit

Baseline: PR #18 at `598ec8b31dcf09d718955d901fa7bed4d2360eb2`.

This unit covers storage failure composition, live snapshot behavior and the
Miri exclusion inventory. The separately planned replacement ownership library
is outside this unit: Box/Arc/TryClone implementations and their universal
initialization proofs are not being reimplemented here.

## Added executable contracts

* A CRC-valid hostile tree child is combined with 31-byte/full-page read
  requests, early/mid-read EOF or I/O error, and persistent allocator refusal
  at positions 1–4 (plus an unrestricted control). All **80 combinations** must
  reject, keep bounded cache accounting, recover on the same reader after the
  rejected child is repaired, and release the reader's file ownership. The
  thread-local syscall seam exists only under `cfg(test)`; real positional
  reads still run, and EOF exercises their zero-progress result branch.
* A single indexed writer fails a partial batch while **eight reader threads**
  each perform 256 rounds of old-generation queries. The writer is poisoned;
  its confirmed checkpoint can be restored while those readers and their
  cursors remain alive. Readers subsequently follow the recovered publication,
  while old cursors remain readable even after their originating reader is
  dropped. This is a concrete concurrent execution, not an exhaustive scheduler
  proof. A separate storage test checks failed resume, released writer locks,
  retained snapshot owners and consuming the final actual File owner.
* The existing head parser is extracted without changing its checks. A portable
  test runs 130 torn-prefix transitions, CRC-valid invalid extents and damaged
  reserved/checksum bytes through that **production parser**. The native
  8,450-image paired-head/durability sweep is retained.
* A memory-only trace exercises the actual page-store recency links, eviction,
  cached/resident byte accounting and shared page lifetimes. It includes a
  writer page that eviction must preserve. It supplements the file-backed
  scaling tests; it does not replace them.

The read-work bound counts non-interrupted attempts: each successful read makes
at least one byte of progress, and there can be one terminal error. The OS must
not return Interrupted forever. An initial test incorrectly assumed each syscall
filled its requested 31 bytes; Miri supplied shorter reads and rejected that
assumption. The corrected bound is based on bytes, not requested chunk count.
Both the failing and corrected logs are retained. An accidentally empty exact
filter run is diagnostic only and is not counted as executed coverage.

## Miri exclusions

`scripts/miri-exclusion-audit.json` covers all **55** exclusions individually:
**46 unsupported OS interactions, 6 workload-cost exclusions, 3 opt-in probes**.
All 52 ordinary alternatives were matched by exact name against passing local
default, all-feature and ASan core logs; they also remain in the CI suites. The three ignored
probes are explicitly *not required execution*, not silently counted as tested.

The checked runner now rejects missing/duplicate audit entries, invalid coverage
routes, and a portable counterpart that is not actually selected for Miri.
Each entry records the remaining gap. Native/ASan do not supply Miri's aliasing
semantics; portable parsing/recency samples do not establish filesystem durability
or replace their complete native workloads. Directory fsync, pathname races and
unsupported syscalls have not been stubbed out to manufacture Miri coverage.

## Qualification

Local results: **264 passed / 3 ignored** with default core features;
**278 passed / 3 ignored** with all core features; **22 public regressions**
(including API validation contracts); **56 Python tests**; workspace/all-target/all-feature Clippy and
formatting. ASan with leak detection passed the full **278 core tests / 3 ignored**
and **13 public failure tests**, including the new compound paths. All four new targeted core tests passed
under Miri; the 80-case compound test took 155.15 seconds in that interpreter.
These are executed checks, not whole-library formal proofs.

Final logs and equal-work benchmark results accompany this report. The default
Miri inventory selects **209 / 264 tests**, retaining all 55 explicit exclusions;
this inventory is not a claim that all selected tests were rerun in this unit.
Four targeted new core contracts were run under Miri with default alias/validity
checks. Native default/all-feature suites and the public failure suites are
checked separately. No public API, file format or resource limit was changed.

## Separate CodeQL status

The baseline's final Rust analysis failed: its SARIF contains **144
cleartext-logging findings and 9 hard-coded-cryptographic-value findings**.
GitHub reports 40 new high-severity alerts. The new SARIF gate correctly rejects
these results; successful Actions/Python jobs or an earlier interim aggregate
are not a clean Rust result. The raw Rust SARIF and location inventory are
retained. Most logging origins point to the private `clear_trusted_api()` calls;
nonce findings include noncryptographic file-identity fields. These need their
own reviewed disposition; this test unit does not waive them or claim CI green.

## Performance provenance

The baseline executables were rebuilt at `598ec8b` with the same Rust 1.95.0,
lockfile, release profile and all-feature selection as the changed tree.
The last test-only syscall instrumentation edit landed immediately after an
initial benchmark build. A final rebuild retained identical code/constant
sections but changed the build ID and `.data.rel.ro`. To qualify the exact
final binaries, the entire measurement sequence was repeated. The initial
**62 timing observations + 2 resource probes** remain diagnostic evidence and
are not pooled into the final results.

Each final native workload starts with five alternating pairs. A metric above
the unchanged 5% threshold triggers 15 additional pairs for that workload, with
all 20 pairs pooled. SWMR uses three alternating pairs at both 1 and 4 GiB, with
eight following readers and a pinned reader, fixed checks per epoch and matching
trace/work assertions. Resource probes run separately from timing samples.
Budgets remain four CPUs and 16 GiB. No test/build runs overlap the final timing
sequence. Existing performance regressions from previous PR units remain in the
PR description; passing this comparison would not erase them.

## Final measured results

**92 qualified timing observations and 2 separate resource probes.**

| Workload | Pairs | Operation/wall time | Process CPU |
| --- | ---: | ---: | ---: |
| Fixed writer, 2M records | 5 | -1.74% | +2.72% |
| Large reverse writer | 20 | +14.83% | +0.49% |
| Complex maintenance, 65,536 × 512 B | 5 | -2.80% | -3.68% |
| Compression, 64 MiB | 5 | +1.62% | +0.08% |
| Decoding, 64 MiB | 5 | -0.88% | -1.38% |
| SWMR 1 GiB, eight following readers + pin | 3 | -4.91% | -4.63% |
| SWMR 4 GiB, eight following readers + pin | 3 | -1.62% | -1.78% |

**The reverse-writer operation-time gate fails:** 14.256 ms → 16.371 ms,
+14.83% across all 20 pairs; process CPU is +0.49%. No passing subset replaces
this result. Other configured workload gates pass. This is a measured comparison,
not proof that the parser extraction caused the timing change.

Sampled complex-work temporary allocation is 4,194,304 → 4,194,304 bytes;
open regular-file allocation is 91,955,200 → 91,955,200 bytes.
Maximum native peak-RSS increase is +0.69%; SWMR peak RSS does not increase.
The unchanged cgroup budgets are `400000 100000` CPU and `17179869184` memory.
Memory event counters do not change: no additional OOM/kill events occurred.

`results.json` holds final metrics and resource readings. `evidence.tar.xz`
retains initial/failed/final logs, both benchmark sequences, real Rust SARIF,
source snapshots and build/binary provenance. Build caches, executable binaries
and generated workload data are excluded. `evidence.sha256` verifies the archive.
