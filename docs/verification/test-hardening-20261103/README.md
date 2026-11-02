# Test hardening and CodeQL diagnosis

Baseline: PR #18 at `cd7b0446da3e731fba867802bef5f4b7b3fc328d`.

## CodeQL failure

The Rust, Python and Actions analysis jobs succeeded. The separate
[CodeQL result check](https://github.com/shim9610/varlit/runs/125413465199)
failed on **30 high-severity `rust/cleartext-logging` findings**, all in test
diagnostics. The complete location inventory is retained in
`codeql-findings-before.json`.

The reported origins were six generated `__varlit_field_user_id` fields in
synthetic fixtures, and the expression at `format.rs:2555` inspecting the
private `trusted_api` boolean. CodeQL's
[SensitiveData heuristics](https://github.com/github/codeql/blob/6f546d6e76b6e1a7b64db1f479d50565a19abfdb/shared/concepts/codeql/concepts/internal/SensitiveDataHeuristics.qll)
classify `user_id` as account data and `trusted` as secret data. The Rust
[SensitiveFieldAccess model](https://github.com/github/codeql/blob/6f546d6e76b6e1a7b64db1f479d50565a19abfdb/rust/ql/lib/codeql/rust/security/SensitiveData.qll)
can classify a parent expression of that field access as a source, beyond the
boolean itself. Findings therefore included error diagnostics, file sizes and
memory-residency measurements. These locations use generated test records and
read-policy state, not real user credentials.

The private boolean now explicitly reads `is_trusted_api`; synthetic fixture
keys are named `fixture_key`. Numeric values, field IDs, assertions and resource
measurements are retained. No query, severity or source-path exclusion was added.
Public signatures and persisted production formats are unchanged. The private
field label in derived Debug output changes; it is not an on-disk field.

The CI aggregate previously required successful analysis/upload jobs, which
could be green while the separate CodeQL alert check was red. Each language now
also checks its SARIF for high/critical security findings and error results,
rejects malformed/failed analysis, and archives the raw SARIF. Existing findings
are not silently waived by this gate. Low/medium warning results remain visible
to GitHub code scanning; this script is not an all-severity zero-alert claim.

## Added regression contracts

* Deque TryClone: callback-returned errors at every element of a rotated deque,
  partial-clone drops, unchanged source order, repeated retry, and real allocator
  refusal through cleanup.
* HashMap TryClone: failure of a value clone after its key was cloned, exactly
  accounted key ownership, released temporary table, preserved source and retries.
* ReadLimits::STANDARD: successful ordinary reads and explicit payload-limit
  refusal. The source inventory detected the private field rename inside the
  public constant; an executable contract was added without refreshing the
  frozen legacy inventory. A test that depended on the first contract now selects
  its intended API explicitly.
* Kani result readers: reject duplicate manifest entries, contradictory failed
  properties, repeated completion summaries, signal termination and timeout or
  unexpected exit codes. A negative control requires exit 1 and exactly one
  completed expected counterexample.
* Miri accounting: every selected test must pass, with zero ignored tests and
  exactly one completion summary. Three previously ignored native-only probes
  are explicitly documented exclusions, not executed Miri coverage. The ownership
  integration suite is now also run by CI using this checked selection mechanism.
* SARIF gate tests: clean analysis, high/critical findings despite successful
  upload, missing evidence, analyzer errors, invalid severity, unknown rules and
  suppressions that must not silently waive the required check.

## Local qualification

* Workspace/all-feature library and integration suite: **1,354 passed, 27 ignored**;
  nested UI snapshots explicitly skipped here and retained in ordinary CI.
* Ownership integration suite: **8/8 default native and 8/8 Miri**.
* Miri unit selection: **205 selected / 260 enumerated, 55 explicit exclusions**.
  This selection check is not a claim that all 205 were rerun locally in this unit.
* Python gate regressions: **55 passed**. Clippy 1.95.0 workspace/all-target/all-feature,
  formatting, actionlint and API validation contracts passed.
* Actual scoped Kani memory run: **2 scalar validity + 1 refusal initialization**
  and **6 calibration controls** passed. Strengthened ordinary-log parsing also
  accepted both archived 52-harness logs; those archived runs are not relabelled
  as new proofs of changed source. CI reruns the ordinary proofs on the new head.
* Full ownership initialization/value-validity remains unqualified, as recorded
  in the preceding report. This work does not establish universal OOM freedom.

Initial failing API-contract experiments and the test-only Clippy failure are
retained with corrected passing logs. No assertion or resource bound was relaxed.
Performance results and exact-source CI status are recorded with the PR update.

At `e9537e9d4770dc61dc7c55680ee387b008b18db0`, GitHub's separate CodeQL
result check completed successfully with **no new alerts**, confirming removal
of the 30 reported false positives. The newly added local-SARIF step initially
failed because the action defaults to `../results`, not `results`. Its follow-up
uses the pinned action's documented `sarif-output` output for both validation
and archiving; no path assumption or optional missing-file fallback remains.


The real CodeQL 2.27.2 SARIF then exposed a second integration issue: its query
rules are stored under `tool.extensions` rather than `tool.driver.rules`.
The validator now resolves pack-scoped rule identities, rejects invalid or
ambiguous references, and is tested against actual clean Actions/Python SARIF
plus an injected high-severity finding in the actual Actions report. Missing
inventories still fail closed. The prior integration failures are retained.

## Equal-work performance

Limits remain four CPUs and 16 GiB; no additional OOM/kill events occurred.
There are **182 qualified timing observations and four separate resource
probes**, plus **50 retained, unqualified native observations**. A binary
inspection command overlapped the initial native timing series; that entire
series was retained as diagnostic and replaced. Final native cases 0 and 4
triggered the predeclared 15-pair confirmation and combine all 20 pairs. The
1-GiB clone likewise combines its initial and confirmation pairs. SWMR uses
three pairs at each size, with eight following readers plus a pinned reader.

| Workload | Pairs | Elapsed | CPU |
| --- | ---: | ---: | ---: |
| Clone bytes-67108864 | 5 | -2.61% | -2.32% |
| Clone bytes-1073741824 | 20 | +2.23% | +0.59% |
| Clone nested-16384 | 5 | -6.87% | -6.46% |
| Single writer, 2M fixed records | 20 | +12.24% | +5.29% |
| Single writer, large reverse updates | 20 | +8.74% | -1.66% |
| Complex maintenance, 65,536 × 512 B | 5 | -1.10% | -3.12% |
| Compression, 64 MiB | 5 | -2.67% | -2.11% |
| Decoding, 64 MiB | 5 | +1.11% | +2.30% |
| SWMR 1 GiB | 3 | +4.89% | +6.24% |
| SWMR 4 GiB | 3 | +2.11% | +0.87% |

**Performance is not an all-pass result.** Fixed-record writer elapsed +12.24%
and CPU +5.29%, reverse-writer elapsed +8.74%, and 1-GiB SWMR CPU +6.24%
exceed the unchanged 5% threshold. These remain failed gates; no favorable
subset or earlier diagnostic series replaces them. 4-GiB SWMR passes.

All six binaries have identical `.text` and `.rodata` sections. The only
allocated-section differences are build IDs and, in four binaries, ten
`format.rs` source-location line numbers incremented by the added comment.
`location-metadata.json` records their decoded file/line/column values.
Thus these measurements do not establish a new algorithmic cost from this
change; nor do binary comparisons turn the observed timing gates green.
The two core examples were rebuilt at the baseline with matching default
features; the other four baseline executables retain the preceding qualified
builds and identical machine-code/constant sections. Exact hashes are retained.

The maximum final native peak-RSS increase is 0.56%. Large-clone peak RSS stays
about 2,049 MiB. Complex temporary allocation stays **4,194,304 bytes**, and
open regular-file allocation **91,955,200 bytes**, on both sides; large cloning
uses no temporary files. Sampled resource-probe times are recorded separately
from the timing comparisons, without pooling them into favorable results.

`results.json` contains the qualified and failed metrics. `evidence.tar.xz`
retains the completed/failed local logs, actual SARIF reports, all timing and
resource observations, source snapshots, benchmark scripts and binary hashes;
executable binaries, Cargo caches and workload files are excluded.
