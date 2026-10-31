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
