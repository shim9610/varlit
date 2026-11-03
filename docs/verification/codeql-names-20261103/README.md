# CodeQL policy and file-identity naming correction

Baseline: PR #18 at `6940af6a5e3a873db8645198e1d6aebc9dc823dd`.
Source correction: `755456588ea643844350840b2e657bba2a1ec608`.

The completed baseline CI has 71 jobs: Rust CodeQL fails, which also fails the
required aggregate; the other 69 jobs pass. The separate Sanitizers workflow
passes. CodeQL analysis/upload succeeds, but the required SARIF gate rejects
**144 cleartext-logging and 9 hard-coded-cryptographic-value findings**.

## Cause and correction

* All 144 logging findings originate at two `clear_trusted_api()` calls in
  `format.rs`. The returned `ReadLimits` contains numeric limits and an opt-in
  flag. CodeQL's sensitive-name heuristic matches `trusted` in the function
  name and propagates that classification into assertion/debug output. The
  private helper is now `without_unbounded_opt_in`, describing the policy
  operation it actually performs. Its body and the opt-in checks are unchanged.
* The cryptographic rule treats arguments to any parameter named exactly
  `nonce` as cryptographic sinks, even without a cryptographic operation.
  Six findings use fixed matrix test identities, one uses a test-directory
  collision suffix, and two originate in initialized buffers used to generate
  or read file-creation identity bytes. These values bind files/sidecars or name
  test directories; they do not encrypt or authenticate data. Internal matrix
  parameters/fields now use `creation_id`; the test runner uses `suffix`.

The public `MatrixSidecarManifest::matrix_creation_nonce` field and existing
wire constants, field offsets, identity generation, CRC checks and test vectors
are preserved. No public signature, file format, dependency or resource limit
changes. No query exclusion, finding suppression, severity change or gate bypass
was introduced. The required gate still rejects the actual baseline SARIF with
all 153 findings, and its six regression tests pass.

The diagnosis uses the pinned CodeQL 2.27.2 sources:

* [Sensitive call/field classification](https://github.com/github/codeql/blob/codeql-cli/v2.27.2/rust/ql/lib/codeql/rust/security/SensitiveData.qll)
* [Sensitive-name heuristic](https://github.com/github/codeql/blob/codeql-cli/v2.27.2/shared/concepts/codeql/concepts/internal/SensitiveDataHeuristics.qll)
* [Cryptographic parameter-name heuristic](https://github.com/github/codeql/blob/codeql-cli/v2.27.2/rust/ql/lib/codeql/rust/security/HardcodedCryptographicValueExtensions.qll)

## Local verification

Release core tests pass: **264 default / 278 all features**, with three ignored
in each. **80 public integration tests** pass (four ignored), covering liveness,
API contracts, read-limit following, matrix generation/reuse and stale-sidecar
rejection after same-object file recreation. The nested process helper is not
counted as an additional independent integration test. The test runner's **12
tests**, the SARIF gate's **6 tests**, workspace/all-target/all-feature release
Clippy with warnings denied, and formatting pass.

The release benchmark executables have identical `.text`, `.rodata` and
`.data.rel.ro` sections before and after this naming change. Complete executable
hashes differ; section and binary hashes are retained. This does not relabel an
adverse timing sample as a passing result.

## Equal-work performance and resources

The existing 5% local comparison gate passes for this correction against
`6940af6`. CPU and memory budgets remain **4 CPUs / 16 GiB**. All builds and tests
finished before timing. Runs alternate before/after order and verify identical
completed work. Native CPU/time and write totals use medians; RSS uses the maximum.
The SWMR cases use eight following readers plus one pinned reader, with 16,384
checks per reader per epoch and matching work/checksum traces.

| Workload | Pairs | Time | CPU | Peak RSS | Written bytes |
|---|---:|---:|---:|---:|---:|
| Fixed writer, 2M records / 200k marks | 20 | -0.35% | -2.34% | +0.28% | +0.00% |
| 4 GiB reverse writer | 5 | -7.18% | -0.37% | +0.00% | -0.01% |
| Complex maintenance, 65,536 × 512 bytes | 5 | +4.12% | +2.15% | +0.65% | +0.00% |
| Compression, 64 MiB | 5 | -2.29% | -0.54% | -0.04% | 0 → 0 |
| Decode, 64 MiB | 5 | +1.84% | +3.60% | +0.03% | 0 → 0 |
| SWMR, 1 GiB | 3 | -0.05% | +2.00% | +0.38% | +0.01% |
| SWMR, 4 GiB | 3 | +0.99% | +2.85% | +0.14% | +0.00% |

There are **92 timing observations and two separate resource probes**. The initial
five-pair fixed-writer comparison measured **+6.53% time**. Following the runner's
confirmation rule, 15 additional pairs were collected; the table includes all 20
pairs, retaining the initial adverse samples. This is a local median/resource
gate, not a statistical guarantee across machines or an upgrade to the earlier
inconclusive performance-calibration result.

The separately instrumented complex case samples resources every 5 ms using a
filesystem-backed temporary directory. Temporary deleted-file allocation is
**4,194,304 bytes before and after**; total open regular-file allocation peaks at
91,955,200 → 91,959,296 bytes. Those are observed peaks, not an exact instantaneous
upper bound. Cgroup `memory.events` counters are unchanged throughout this run;
pre-existing OOM counters are retained in the evidence rather than presented as
zero. The additional ELF section inventory finds differences in build IDs and
unwind/exception tables; it does not claim that all loaded sections are identical.

## Evidence and remote validation

`evidence.tar.xz` retains the actual failing SARIF and compact finding inventory,
local test/build logs, all timing samples and confirmation runs, resource probes,
source and executable fingerprints, and reproduction scripts. Generated workload
files and executables are excluded. `evidence.sha256` checks the archive.

Exact-head CodeQL and required-CI results are recorded in PR #18's checks and its
latest description. A clean local build or successful analysis upload alone does
not establish a clean CodeQL result. Earlier performance calibration limitations
remain in their original report. No merge or release.
