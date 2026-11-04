# Independent read checks — proposal, not implemented

This is a read-path review and proposed contract. It does not add a trusted
mode to the library. Ordinary opens, public API defaults and file bytes remain
unchanged.

## Baseline and current behavior

Compare the [pre-PR main snapshot](https://github.com/shim9610/varlit/tree/ba541cc3082cecdf7f385d06897a58a3889e1294)
with PR #18. The
`38c37d0` worktree is an earlier PR snapshot and is not the pre-PR baseline.

The baseline's `file/scan.rs::verify_snapshot_record` already reread a record
header and footer, compared them with the index entry and recomputed its CRC.
It also already returned early under `IntegrityPolicy::None`. The current
implementation shares that work with mapped reads through
`verify_record_bytes`; it retains the same coupling. It is incorrect to
attribute all of these reads/checks to newly added security hardening.

Current controls are not four independent read modes:

- `IntegrityPolicy` declares stored record integrity (`None`, `Crc32`, or
  `Crc32WithHeader`) and participates in the format contract.
- `IntegrityVerification::{AtOpen, OnDemand}` schedules checking; `OnDemand`
  still verifies returned records.
- `MatrixMetadataVerification::OnDemand` defers whole-map verification but
  still authenticates accessed metadata pages.
- `trusted_unbounded` authorizes resource ceilings to be absent. It does not
  mean unverified file access.
- Disk-index pages and confirmed publication heads have their own checksums.
  Turning off record CRC does not turn those protocol checks off.

## Proposed independent axes

| Additional structural consistency checks | Data CRC verification | Intended behavior |
|---|---|---|
| On | On | Full additional consistency checks and stored-data CRC checking |
| Off | On | Trusted structure; check stored-data CRC without redundant index/header cross-checks |
| On | Off | Check structure/identity relationships without data CRC calculation |
| Off | Off | Trusted-data fast path with required parsing, bounds and publication guards |

These axes concern optional data verification. They must not mean an unchecked
memory access or an infallible allocator. Bounds, overflow checks, legal Rust
value construction, decoder output limits and finite traversal remain enforced.
Failure detected by a retained check returns an error. A skipped check cannot
promise detection: a wrong but well-formed value can be returned if the trust
assumption is violated.

CRC-on still reads and hashes whatever the stored checksum covers. In particular,
`Crc32WithHeader` cannot skip header bytes merely because additional structural
comparisons are off; footer bytes covered by the format remain checksum input.
This split removes eligible duplicate comparisons and scheduling work, not all
framing I/O. Its speedup must be measured rather than inferred from the toggle.

Publication fencing is separate. In single-writer/multiple-reader use, even a
cooperating writer may be in the middle of publishing a head. Checksums used to
reject torn or incomplete publication cannot be disabled while still claiming
the same committed-prefix contract. The UI/API must call the data toggle
"data CRC", not promise that all metadata/protocol CRC work disappears.

## Compatibility and implementation boundaries

Use an explicit read-side policy, independent of the stored integrity policy.
Opening a CRC-bearing file with CRC verification disabled must not pretend the
file was written with `IntegrityPolicy::None`. No header, schema fingerprint,
file format or write-time checksum policy changes are required.

Default admission must preserve existing behavior. A new explicit structure-on
option can request the header/index checks even for a file with no data CRC;
silently adding those checks to all existing no-CRC opens would change their
cost. Likewise, selecting trust must not imply `TrustedUnbounded` or enlarge
buffers or caches. Policy composition must preserve an explicit choice rather
than silently ignoring it under a declared `AtOpen`/`OnDemand` policy.

A data-CRC-on request needs an actual stored checksum. For a no-CRC file, an
explicit requirement must report that no checksum exists; it cannot manufacture
corruption detection by hashing bytes against themselves.

Before implementation, trace native owned/buffered reads, stream/indexed point
and follow routes, matrix payload/metadata/generation reads, compression/range
decoding and immutable mapped views. Distinguish cached evidence, optional
consistency checks, resource guards and publication protocol at each route.
Do not advertise a library-wide mode after changing only one read helper.

## Required validation

Measure all four combinations against the pre-PR snapshot and current defaults
using identical logical data, physical grouping, key order, publication sizes,
query locality and successful reader work. Keep the same 4 CPU / 16 GiB / 32 GiB
benchmark limits and record CPU, memory, scratch storage and reader lag. Separate
the benchmark's own oracle/copying work from library verification attribution;
the existing end-to-end timings do not prove verification is the entire gap.

Test data corruption and independently corrupted metadata in each combination,
plus short reads, I/O errors, cyclic/overflowing extents, allocation refusal and
concurrent publication. Check the documented detection differences and preserve
termination, cleanup and fallible error behavior. Ordinary safe-mode regressions
and registered default proof contracts must remain intact. Whole-owner
initialization proofs remain in the separately planned library, as agreed.
