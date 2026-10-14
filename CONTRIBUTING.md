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
cargo run --locked -p varlit-test-runner -- test --locked --workspace
cargo run --locked -p varlit-test-runner -- test --locked --workspace --all-features
cargo clippy --locked --all-targets --workspace --all-features -- -D warnings
```

Run targeted tests while iterating. See the [self-check guide](docs/self-check-guide.md)
for fuzzing, Miri, fault injection and native I/O qualification. Documentation-only
changes need valid links, accurate API examples and agreement with shipped behavior.

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
