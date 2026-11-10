# Contributing to Varlit

## Issues and proposals

Use the issue forms for reproducible bugs and feature proposals. Include the
Varlit commit/version, feature flags, OS/filesystem and a small reproduction.
For a potential security problem, follow [SECURITY.md](SECURITY.md).
Read the [Code of Conduct](CODE_OF_CONDUCT.md) before participating.

## Pull requests

Create a topic branch or fork and submit a PR targeting `main`. Explain the
problem, resulting behavior and validation. Keep a change focused; update public
documentation when an API, file format, supported option or limitation changes.
Add a regression when fixing behavior that previous tests missed.

Preserve the single-writer/independent-reader contract, caller-selected durability,
explicit compaction, checked address arithmetic and bounded working memory.
Readers have private caches and are `Send + !Sync`; querying uses a shared borrow.
Experimental matrix reuse stays opt-in. Discuss changes to these contracts before
implementing them. Performance claims need the workload, platform and measured data.

The PR templates and checks apply to contributions from the maintainer too.
Normal updates use PRs rather than direct pushes to `main`. An external
contributor does not need write permission or repository credentials to run tests.

## Local checks

Use Rust 1.95 or newer. Keep dependency changes explicit and preserve lockfiles.
For a code change, start with:

```sh
export CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=4
cargo fmt --all -- --check
rustfmt --edition 2024 --check crates/varlit-core/src/file/*.rs
cargo run --locked -p varlit-test-runner -- test --locked --workspace
cargo run --locked -p varlit-test-runner -- test --locked --workspace --all-features
cargo clippy --locked --all-targets --workspace --all-features -- -D warnings
```

Run targeted tests while iterating. See the [self-check guide](docs/self-check-guide.md)
for fuzzing, Miri, fault injection and native I/O qualification. Documentation-only
changes need valid links, compilable API examples and agreement with the documented
revision. Release PRs must include the target version in workspace metadata,
sibling dependencies, standalone lockfiles, installation examples, changelog
and API changes before merge. Complete these updates and package verification
in the release PR. Keep public documentation in English and preserve the source,
workload and uncertainty attached to recorded measurements.

The [verification guide](docs/verification.md) contains the current Kani/Loom,
coverage, mutation-control and advisory commands. Read-check policy changes must
preserve required bounds and publication guards across every optional mode.

New APIs, policy variants and Cargo features must join the executable
[validation contracts](docs/validation-contracts.md). The gate reports missing
registrations; its legacy inventory is frozen rather than a snapshot to refresh.
Runtime fixes must also satisfy the documented performance qualification: paired
release measurements across file sizes, key counts and concurrent workloads,
with unchanged resource budgets and recorded memory/I/O costs.

## CI and merging

CI and Sanitizers automatically run only for PRs targeting `main`. They also
support explicit manual dispatch. Updating a PR cancels obsolete runs; merging
does not duplicate the PR checks. All feature configurations, native Linux/
Windows/macOS tests, packaging, dependency checks and CodeQL feed the stable
`ci / required` gate. A failed, cancelled, missing or skipped job cannot pass it.
Separate required checks cover ASan, portable Miri and fuzz smoke.

Keep the PR current with `main`, address review conversations, and wait for the
required checks before merging. CodeQL findings remain visible in the Security
tab. Repository protection settings are enforced by GitHub after they are enabled.

Contributions are provided under the project's existing MIT OR Apache-2.0 license.

## Maintaining code and documentation

Keep native file operations in the focused implementation files under
`crates/varlit-core/src/file/`: lifecycle, append/publication, directory reads,
follow, mutation/rewrite, matrix access, mapped access and header slots. Keep
unit tests beside their owning module. The implementation files share the native
file module through literal includes, preserving its private boundary and compiler
partitioning. The source auditors resolve those includes recursively; dynamic,
missing or out-of-crate paths cannot bypass validation. Preserve public signatures
and the frozen API inventory when reorganizing implementations.

Extend a shared fixture or parameter table when a regression adds a case to an
existing contract. Separate tests are appropriate for different failure oracles,
platform behavior, ownership lifetimes and feature selections. Empty placeholder
tests and duplicated assertions do not add coverage. New failure tests should
check recovery/retry and retained-reader state where relevant.

Keep the root README and quickstart focused on recommended application patterns.
Maintain current contracts in the reference guides. Performance results are
pending a controlled local rerun; keep the benchmark harness and its execution
instructions without publishing cloud measurements. Historical release notes,
audit narratives and superseded raw evidence belong in Git history. `scripts/check_docs.py --quickstart` rejects repeated
report sections, checks local links and executes the example taken directly from
the guide.
