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

## Explicit trusted read policy

`ReadLimits::with_read_validation` independently selects optional data CRC and
additional structural consistency checks. Ordinary opens retain their existing
checks. `ReadValidation::TRUSTED` trusts captured file metadata and skips those
optional checks; wrong but well-formed data may then be returned if that trust
is violated. Retained checks return errors, but skipped damage is not guaranteed
to be detected. Snapshot bounds, checked arithmetic, resource/decoder limits,
finite traversal and committed-publication guards remain in force. This is not a
process-wide OOM guarantee. Protocol/generation checksums and standalone container
checks are distinct from optional record/cell CRCs. See the
[read-policy contract](docs/design/read-validation-policy-review.md).

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

## Allocation refusal and ownership

`TryClone` and `try_box` report refused allocations on their supported paths.
Internal shared owners and copy-on-write page separation also return errors;
`ScanCancellationToken::try_new` provides a fallible constructor. Existing public
`Clone`, `new`/`default` and convenience builders retain their original contracts.
This does not establish process-wide OOM freedom: `BTreeMap`, other unaudited
allocations, application callbacks, allocator aborts and OS OOM kills remain
outside this scope. See [allocation limitations](docs/known-limitations.md#72-some-allocations-abort-instead-of-returning-an-error).

When detailed error allocation fails after publication, inspect
`Error::ErrorContextAllocationFailed` and its `AllocationFailureContext` before
retrying: an append may already be visible or publication may be uncertain.
Durable matrix writes reserve their existing error/event boxes before mutation.

## Private spill storage

Disposable row directories, rewrite metadata, merge bytes and scratch indexes
use `VARLIT_SPILL_DIR` when set, otherwise the platform temporary directory
(including `TMPDIR` where supported). The selected directory must already exist
and be writable. An invalid explicit selection returns an error; it does not
silently fall back to another volume. Set process environment configuration
before opening handles/starting worker threads. Each spill file captures its
policy when created.

`VARLIT_SPILL_MAX_BYTES` optionally limits the logical length of **each** private
spill file, in unsigned decimal bytes. Unset means no additional capacity cap;
`0` refuses nonempty spills. A write beyond the chosen limit returns a storage-full
I/O error before that write, retaining existing authoritative state. This is not
an aggregate process/directory quota or a promise of available free space. Use a
filesystem/project quota or a dedicated volume for an aggregate capacity policy;
concurrent files, allocation granularity and other users of the volume also
consume space. Genuine filesystem-full and partial-write errors still propagate.

This policy does not cap database size or require spills beside the database.
Publication/replacement files that must be atomically renamed remain beside their
destination and are outside this disposable-spill policy. Temporary storage on
`tmpfs` consumes system memory that process resident-set size (RSS) does not fully
reflect. Choose a disk-backed directory when that distinction matters.

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
