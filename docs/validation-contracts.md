# Validation contracts for new features

New source-level interfaces must have executable validation contracts before the
all-feature test suite can pass. The gate discovers public functions, methods,
fields, enum variants, types, constants and exports in workspace library source
trees, including newly added library crates. It also tracks Cargo feature
declarations and changes to procedural-macro generator functions and item-level
macro definitions/invocations.

The initial inventory in `crates/varlit/tests/support/validation_legacy.txt` records the surface
at commit `65edbde`; it does not certify complete coverage of existing APIs. Its
fingerprint is frozen. Add new contracts instead of refreshing that inventory.
The existing positional-read API is deliberately outside the legacy inventory
and exercises the same registration and execution requirements as future APIs.

## Add an API or format field

Run the gate after changing the interface:

```sh
cargo test --locked --workspace --all-features --test validation_coverage
```

The failure lists each new or changed interface, including its signature
fingerprint. Add a `contract!` entry to `CONTRACTS` in
`crates/varlit/tests/validation_coverage.rs`, using that exact identifier. Follow
the existing `read_block_at` example:

- The `valid` callback constructs a supported input and asserts the result.
- The `invalid` callback constructs a malformed, unsupported or otherwise
  invalid input and asserts the intended response. CRC tests should also cover
  structurally invalid records with consistent checksums when applicable.
- Both callbacks must reference the registered interface and contain an
  assertion. Function/method contracts must actually call it; a function-pointer
  reference alone does not qualify. The runner executes both callbacks, and
  either failure fails the test.

Each registration binds to the interface fingerprint. Renamed, removed or
changed signatures make existing registrations stale and require review.
Generator and item-macro contracts additionally use `generated "method_name",`
after the identifier to name the generated API called by both callbacks. This
checks generated behaviour without pretending to resolve arbitrary macro output
from its unexpanded syntax.

## Add a policy variant

The shared domains in `crates/varlit/tests/support/validation_cases.rs` supply the lifecycle
and read-route runners. For a type already represented there, add its new variant
and a representative value. Source discovery checks the complete variant set,
including `non_exhaustive` enums. Each label is checked against its actual value.

Update the case generator when a variant carries parameters. The generator
checks require every registered shape and compression/transaction parameter
combination to remain represented. Supported cases run through the lifecycle;
transaction-marker formats exercise the stream/index APIs' explicit refusal.
The shared runners consume the resulting cases automatically.

A new policy type or field also needs the executable interface contracts above.
Choose boundary cases for numeric parameters; the case registry does not
enumerate every integer, payload, sequence of operations or feature subset.

## Add a Cargo feature

`scripts/feature_matrix.py` reads Cargo metadata, including implicit features from
optional dependencies and workspace membership. New features automatically
join Clippy and native singleton test jobs. Direct facade forwards avoid duplicate
dependency-feature jobs; a core-only or macro-only feature gets its own job.
Every pair of those independent feature selections also runs the workspace
tests on Linux, starting with the other features off. Cargo still enables each
selection's required dependencies. This does not enumerate all feature subsets.
Default and all-feature jobs remain, with native platform I/O coverage separately.
The interface gate also requires valid/invalid contracts for new or changed
feature declarations. Their callbacks must call an API that exercises the feature
and assert the intended behaviour. Use workspace all-features when qualifying a
core-only feature; its `cfg` is not a feature of the facade test crate. Merely
adding a CI matrix entry is insufficient.

## What the gate establishes

This is an omission check and an executable regression contract, not a proof that
the assertions are sufficient. Review their expected results and hostile inputs.
The inventory uses Rust syntax, not compiler type resolution; implementation-only
changes behind an unchanged interface still need ordinary regression review.
Unsupported `include!` expansion and conditional module paths are rejected until
their source traversal is implemented.
Tests and build-time tooling add no library runtime work.

## Storage failures and resource growth

The required `storage-contracts` CI job runs
`python3 scripts/check_storage_contracts.py`. It checks exact test names, pass
counts and execution markers so renamed or feature-disabled cases cannot pass
by running zero tests. The contracts cover:

- Real Linux partial writes and `EFBIG`, followed by rollback, retry and reopen;
  process resource limits are confined to a child process.
- Failure/recovery histories with overwrite, delete, reinsert, pinned readers,
  follow, index compaction and writer reopen, checked against an independent map.
- CRC-valid inconsistent checkpoint frontiers and record-chain links.
- Torn publication-head prefixes and missing durable checkpoint bytes. These
  are explicit storage images, not a claim of physical power-loss qualification.
- Repeated 1/8/32 MiB decompression and cumulative nested decode budgets, with
  peak/retained Rust heap checks. The required Linux release run also checks
  process peak RSS, including native codec allocations, against a fixed 96 MiB
  ceiling for this workload (32 MiB input, 32 MiB output, 32 MiB overhead).
- The existing 10k/100k/1m-key allocator growth test, now required in release mode
  with the same fixed cache and batch budgets at each size.

CI also executes the load example with actual file bytes and eight readers, to
catch broken benchmark fixtures. Its small smoke run verifies the harness;
throughput conclusions require the larger paired measurements below.

## Performance qualification when fixing runtime defects

Use the same release workload, machine, feature set, payloads, cache limits and
batch limits for the parent and patched implementations. Preserve the baseline
binary before changing runtime code. Alternate before/after runs and report
variability; a single timing is insufficient.

The default concurrent load is a saturation study: readers run until the writer
finishes, so a delayed writer causes additional reads. Its wall times do not
compare equal work. For before/after regression checks, also use
`--reader-checks-per-epoch` with the same count on both sides. Each reader then
executes the same keys against the same pinned/published snapshots, without
sleeping; the driver rejects a different operation count or snapshot trace.
Record the writer's append/sync time separately from reader-completion waiting,
along with total CPU and cgroup throttling. Run a same-binary control when the
environment is noisy; unresolved differences block performance qualification.

Include increasing real file sizes and key cardinalities, repeated mutations,
reopen/recovery/compaction, and a writer with multiple independent readers.
Record throughput, latency distributions, peak RSS/heap and I/O. Confirm content
against an independent oracle throughout. Sparse large-offset probes supplement
these runs but cannot stand in for writing real data. Do not increase resource
budgets to make a failed check pass, or claim terabyte-scale throughput from a
smaller run. A runtime fix is not qualified until its relevant before/after
measurements and resource-growth checks pass.
