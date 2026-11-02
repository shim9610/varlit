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
* Python gate regressions: **54 passed**. Clippy 1.95.0 workspace/all-target/all-feature,
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
