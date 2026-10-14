# Security Policy

## Supported versions

Reports are accepted for the current `main` branch and the latest repository
release. Older releases are not actively maintained. Code on `main` can be
unreleased; use the release notes and known limitations when evaluating it.
Varlit has not yet been published to crates.io.

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

PR checks include native platform tests, dependency/license policy, Rust/Python/
Actions CodeQL analysis, Miri, ASan and a bounded fuzz smoke run. CodeQL's
build-free scan examines checked-in source; generated APIs also require the
compiler and behavioral tests. These checks do not establish exhaustive testing,
power-loss guarantees or suitability for every filesystem.

Main changes go through a PR and the configured required checks. Published
advisories are listed under [Security](https://github.com/shim9610/varlit/security/advisories).
