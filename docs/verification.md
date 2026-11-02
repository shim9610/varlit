# Verification tools

`ci / required` depends on the reusable `verification.yml` workflow. Its coverage,
Kani, Loom and software bill of materials (SBOM) jobs must succeed. Existing
Miri, AddressSanitizer and fuzz smoke jobs remain in `sanitizers.yml`; existing
property tests remain in the ordinary test suite. This change does not promote
the separate sanitizer workflow into the `ci / required` aggregate.

## Coverage

Install `cargo-llvm-cov 0.9.1` and Rust 1.95.0's `llvm-tools-preview` component.
The workflow instruments workspace unit and integration tests with all features,
and retains JSON, HTML and uncovered-line reports. Vendor, integration-test,
example, tool and proof-only files are excluded from the report. Inline unit-test
code inside library source files is still counted. Compile-fail snapshots are
checked by ordinary CI, not by the instrumented run. Doctests and ignored tests
are not included in this coverage measurement.

This first integration reports line/region coverage; it does not impose an
arbitrary percentage floor or claim branch coverage. A successful job requires
the tests and report generation to succeed, not every line to be covered.
New unit/integration tests join the command automatically.

Initial local qualification on Linux x86-64, 2026-11-01: 1,304 unit/integration tests passed,
26 ignored; measured source-line coverage was 46,272/52,574 (88.01%) and region
coverage 72,641/84,742 (85.72%). The exclusions and inline-test caveat above
apply. Both Kani harnesses and Loom models passed; warnings-denied Clippy,
33 Python tests and actionlint also passed. Remote CI results are separate.

```sh
VARLIT_SKIP_UI_SNAPSHOTS=1 cargo llvm-cov --workspace --all-features --lib --tests --locked --no-report
cargo llvm-cov report --show-missing-lines
```

## Kani

Install `kani-verifier 0.68.0`, run `cargo kani setup`, then:

```sh
cargo kani -p varlit-core --default-unwind 5
cargo kani -p varlit-core --all-features --default-unwind 5
```

The harnesses call production implementations directly, under `cfg(kani)`:

- `checked_extent_matches_wide_arithmetic`: arbitrary full-width `u64` offset
  and payload length, with and without a footer. Compare checked and saturating
  physical ends against independent `u128` arithmetic. Recursive destruction of
  unrelated error variants is outside this arithmetic proof; the result is
  forgotten only in the harness, and the actual overflow variant owns no heap.
- `observed_prefix_never_invents_membership`: four arbitrary 16-bit offsets,
  a 16-bit query and every truncation length from zero to four. Every accepted
  offset must correspond to an observed row that survives truncation.

The second proof is bounded and does not establish arbitrary-length or full-u64
prefix correctness. Five unwind steps cover these four-row loops; unwinding
assertions stay enabled. This is a proof bound, not a limit on accepted files.
Ordinary tests and large-file resource/performance qualification still apply.
CI checks both feature configurations against
[the contract manifest](verification/proof-contracts.json), currently 34 harnesses.
`scripts/check_proof_contracts.py` rejects missing, duplicate, unexpected or
unfinished proofs and verifies source hashes captured before and after each run.
A zero-harness result fails.

Four publication harnesses additionally call the production metadata and
checkpoint validators. They establish accepted-frontier consistency and
preservation of the confirmed frontier during checkpoint validation, and check
that consistent clean/restore examples are accepted. Offsets, counts, generations,
sequence options and digest bytes use their full symbolic widths. Their local
unwind bound is 35 for fixed-size 32-byte comparisons; the original harnesses
retain the default bound of five. Unwinding assertions remain enabled. As in the
arithmetic harness, error destruction is excluded, not validation. These are
validator proofs, not proofs of all filesystem operations or crash schedules.

## Hostile metadata and resource contracts

[Scope and retained evidence](verification/hostile-metadata-20261102/README.md)
describes thirteen additional harnesses for complete zstd block progress/output
reservations, actual chunk header/entry parsing and table bounds, limited spill
writes/retries, cumulative materialization, index visit fuel/height, and native
prefix transitions. These mix full-width arithmetic/transition proofs with
explicitly bounded I/O and prefix models. General prefix geometry induction and
whole-parser models did not complete within their trial bounds; they are not
counted as proofs. The existing 16-bit membership proof remains bounded.

The required default-feature job also runs `scripts/check_resource_mutations.py`:
ten broken variants of exact production helpers must fail their designated
assertions after the original helpers pass. Allocation refusal, CRC-valid hostile
files, actual reader preservation/retry, and concurrent live-buffer tests connect
the modeled contracts to native execution. These checks do not establish a
process-wide OOM guarantee or termination of arbitrary user callbacks/OS calls.

## Shared publication control flow

[Protocol verifier and negative controls](verification/protocol-verifier-20261102/README.md)
adds nine harnesses that call the same generic commit, checkpoint, restore, resume
and follow drivers used by production. They explore before/partial/after-effect I/O
errors, uncertain publication, poisoned-writer reuse, durable acknowledgement and
failed reader adoption. Initialization plus an arbitrary-state transition proof
establishes an abstract frontier invariant without a history-length bound.
The OS durability, immutable-prefix and callback-validation assumptions are
explicit; this is not a refinement proof of the whole library.

The required Kani job also runs `scripts/check_publication_protocol.py`. Eleven
independent mutations of the production driver must compile and fail their
expected assertions. Compiler errors, timeouts and unrelated failures do not
count as detection. All five production bindings and the clean-open reconciliation call are checked; the unmodified
protocol must pass first. New registered harnesses join the run automatically.

## Publication and recovery

[Publication qualification](verification/publication-20261102/README.md) defines
the observable contract and its assumptions. Required release tests now include
8,450 independently torn publication-head images, all 512 length-three histories
over a stated eight-operation alphabet, 16 longer seeded histories, and a mixed
history interrupted at every discovered fault-hook occurrence while readers stay
open. Recovery and subsequent publication must also work.

The oracle checks complete confirmed generations, pinned-reader stability and
failed-follow handle preservation. It does not equate `flush()` with publication,
or a returned write error with proof that publication did not occur. Native
process termination does not simulate loss of the operating system's page cache.
New tests join discovery automatically, but new API operations need an explicit
operation model and expected result; they do not automatically enter this alphabet.

## Loom

```sh
RUSTFLAGS='--cfg varlit_loom -D warnings' cargo run --locked --manifest-path tools/verification/Cargo.toml
```

The isolated runner imports the actual `scan_control.rs` module. Only its
`Arc`/atomic imports switch to Loom; normal builds use the same std types and
method bodies. Two models explore cancellation publication/monotonicity and
the progress driver's cancellation checks. No preemption bound is configured;
the finite models avoid unbounded spinning and fairness assumptions.

The runner refuses to compile without `varlit_loom`. Its dependency workspace
has its own lockfile, audit job and Dependabot directory. Loom does not enter
the published library dependency graph. A negative qualification with the
actual release store changed to relaxed in a separate source copy fails the
publication assertion (observed payload 0 instead of 17).

These models cover cancellation, not all SWMR protocols or filesystem durability.
New concurrency protocols need their own model and oracle. Filesystem, mmap,
partial-write and crash/recovery tests continue to exercise real OS behavior.

## Dependency inventory

Install `cargo-cyclonedx 0.5.9`, then:

```sh
cargo fetch --locked
python3 scripts/generate_sbom.py
```

Six CycloneDX 1.5 JSON inventories cover the three publishable crates, with
default and all-feature resolution, across all Cargo targets. Build dependencies
are included; dev dependencies are omitted by the tool. Provenance includes
the Git revision and lockfile SHA-256. The script verifies locked resolution,
runs generation offline and rejects any lockfile rewrite. These inventories
describe dependency configurations, not a particular compiled binary.

Known-advisory checking remains cargo-audit/cargo-deny. SBOM generation does not
establish that dependency source is benign. See the dated dependency review for
upstream fixes not reported by the advisory database at the time of inspection.

## Runtime and maintenance

Proofs and negative controls execute in CI or explicitly selected verification
builds. Five production entry points now delegate ordering to statically
dispatched generic drivers shared with the proofs. Public signatures, persisted
format, dependency resolution and configured budgets are unchanged. There are
no new production allocations or locks. Uncertain clean writer resumes now sync
the sidecar; matching confirmed heads avoid that extra sync. Instrumented timings are not performance
benchmarks; the protocol follow-up includes large-file equal-work qualification.

Miri and fuzzing keep their existing limitations and manifests; adding these
tools does not make all input combinations verified. Properties, model setup
and expected outcomes still need to be specified by maintainers. Verus and
Ferrocene are not introduced in this change.

[Conditional memory contract excluding BTreeMap](verification/conditional-memory-20261102/README.md)
adds decoder charge/transfer and rehash-overlap obligations. Its whole-library
acceptance target remains open; passing the registered harnesses is not a global
OOM-free claim.
