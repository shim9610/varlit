# Security Policy

## Supported versions

Reports are accepted for the current `main` branch and the latest repository
release. Older releases are not actively maintained. Code on `main` can be
unreleased; use the release notes and known limitations when evaluating it.
Varlit is published on [crates.io](https://crates.io/crates/varlit). Fixes on
`main` reach registry users only after a new crate version is published.

## Reporting a vulnerability

Use [GitHub's private report form](https://github.com/shim9610/varlit/security/advisories/new).
Please do not put vulnerability details or private file contents in a public issue.
If that form is unavailable, open an issue requesting a private reporting channel,
without including the details. The maintainer is [@shim9610](https://github.com/shim9610).

Include the affected version or commit, enabled features, operating system and
filesystem, a minimal reproduction, and the expected versus observed result.
A small synthetic file is preferable to production data. Retain the original
file if recovery or publication behavior is involved.

The maintainer will assess the report and coordinate a fix and disclosure through
a private advisory. There is no guaranteed response time or paid support contract.

## Scope and validation

Relevant problems include accepting invalid file structures, exceeding declared
resource limits, violating confirmed-generation boundaries, losing acknowledged
records during recovery, memory safety errors, and incorrect file-object handling.
Format declarations and user-provided codecs are trusted application code.
Documented mmap mutation exclusions and experimental feature limitations still
apply; see [known limitations](docs/known-limitations.md).

## Mapped files and external modification

Memory mapping (`mmap`) is not isolation from another handle or process that
writes or truncates the same file object. Keep mapped backing bytes stable for
the lifetime of the view and all borrowed references. An advisory writer lock
only coordinates cooperating writers. CRC checks detect corruption when checked;
they neither prevent later writes nor authenticate the file.

An external truncate can make a mapped page inaccessible and cause an operating
system fault such as `SIGBUS`, rather than a recoverable Rust error. External
writes can invalidate previously checked contents; mutation while a zero-copy
reference is live violates the existing unsafe API's immutability requirements.
The documented layout and immutability conditions on `raw_fixed` and `raw_cell`
still apply. Supported Varlit publication/snapshot operations do not authorize
uncoordinated in-place modification or truncation by other software. Use owned
reads when the application cannot maintain the mapped-file lifetime contract;
owned reads still need a coherent input and do not make concurrent external
writes transactionally consistent.

## Vendored advisory coverage

Lockfile-based `cargo audit` and `cargo deny` do not match a local path package
itself against registry advisories. The required supply-chain job also runs
`scripts/check_vendored_advisories.py`, which derives the actual names/versions
from `vendor/*/Cargo.toml` and audits temporary registry identities. Its synthetic
advisory control verifies that an affected redb identity fails and a patched
version passes. This neither replaces the lockfile audits nor certifies local
patches: vendored redb 4.4.0 remains a development-only oracle, and its local
snapshot extension still needs source review and tests.

PR checks include native platform tests, dependency/license policy, Rust/Python/
Actions CodeQL analysis, Miri, ASan and a bounded fuzz smoke run. CodeQL's
build-free scan examines checked-in source; generated APIs also require the
compiler and behavioral tests. These checks do not establish exhaustive testing,
power-loss guarantees or suitability for every filesystem.

Main changes go through a PR and the configured required checks. Published
advisories are listed under [Security](https://github.com/shim9610/varlit/security/advisories).
