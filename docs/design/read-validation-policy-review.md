# Independent read checks

This API is available in 0.13.0. The [public API reference](../api-reference.md#read-validation)
and [upgrade notes](../api-changes.md#0130) cover application setup.

`ReadValidation` selects optional data CRC and additional metadata consistency
checks independently. This is an explicit read-side policy. Ordinary opens retain
their previous behavior, and write-time checksums, schema fingerprints and file
bytes are unchanged.

## API and modes

```rust
use varlit::{DiskIndexOptions, ReadLimits, ReadValidation};

let limits = ReadLimits::MISSING.with_read_validation(ReadValidation::TRUSTED);
// Given a declared `spec` and input `path`:
// let reader = spec.open_reader_with_resource_limits(path, limits)?;

let options = DiskIndexOptions {
    limits: ReadLimits::MISSING.with_read_validation(ReadValidation::CRC_ONLY),
    ..DiskIndexOptions::default()
};
// VarlitIndexedReader::open(spec, path, options, plan)?;
```

| Mode | Additional structure checks | Data CRC |
|---|---|---|
| `FULL` | On | On |
| `CRC_ONLY` | Off | On |
| `STRUCTURE_ONLY` | On | Off |
| `TRUSTED` | Off | Off |

`ReadValidation { data_crc, structure }` also accepts the two booleans directly.
`ReadLimits::read_validation = None` is the compatibility default. Missing
runtime declarations preserve a format's selection; `overlay` replaces it only
with an explicit selection, and `tighten` retains an existing declaration. This
policy neither authorizes `TrustedUnbounded` nor increases a resource ceiling,
cache, buffer or scratch allowance.

An explicit CRC-on selection requires a CRC-bearing format. A format declaring
`IntegrityPolicy::None` reports `InvalidFormatSpec` for `FULL` or `CRC_ONLY`;
there is no stored checksum to verify. Reading a CRC-bearing format still
requires the existing `integrity` Cargo feature. The stored `IntegrityPolicy`
is never rewritten to simulate an unchecked read.

## I/O contract

For an already captured `RecordIndexEntry`, structure-off payload access reads
only the payload. CRC-only hashes those bytes with the captured header/footer
fields; it does not reread those fields. Consequently, a later header mutation
can go undetected in CRC-only mode: the captured metadata is trusted. Both CRC32
policies, including `Crc32WithHeader`, retain their original checksum calculation.

Indexed uncompressed point reads in `TRUSTED` derive the payload span directly
from the confirmed sidecar's physical descriptor. They do not read the native
header or footer. CRC-only additionally reads a footer once where the stored CRC
covers links absent from the sidecar. Compressed candidates need one native
frame read because the sidecar does not store their uncompressed-length hint;
payload access does not repeat it. Trusted pointer/tombstone lookup does not
materialize a payload just to recheck a key.

Typed stream/follow cursors parse each incoming frame once and then use its
captured metadata for the returned payload. Trust does not make a stream
self-indexing: identifying the next record still requires its framing. Native
resident/demand-directory access may also need an initial frame read to learn
fields absent from an offset-only directory. The one-payload-read promise is
about captured entries and uncompressed indexed candidates, not every possible
open or query.

Structure-off skips optional disk-index page/key CRC checks and padding/prefix
comparisons, and omits the eager whole-matrix metadata reporting pass. Required
page shape, key extents, child descent, traversal fuel and bounded cache admission
remain. Index writer/maintenance readers retain metadata checks. Matrix scalar,
run, column, growing chunk, compressed-slot and mapped payload paths honor the
data-CRC selection without changing stored layout or cell width. Immutable mmap
payload certificates are allocated only when an optional check is selected.

`IntegrityVerification::{AtOpen, OnDemand}` schedules selected record checks.
With both checks off, `AtOpen` does not force a payload-verification scan or
exclude the existing digest/segment open routes. With a check selected it keeps
its admission behavior. Explicit `verify_all`, recovery admission and index
reconstruction/source validation do not inherit a trusted payload-read choice.

## Guards that remain

Every mode retains checked arithmetic, snapshot/slice bounds, legal Rust value
construction, decoder output/workspace limits, fallible reservations on supported
paths, finite traversal and committed-publication boundaries. Actual truncated
owned reads return an error. No mode upgrades this to a process-wide OOM proof;
see [allocation limitations](../known-limitations.md#72-some-allocations-abort-instead-of-returning-an-error).

Confirmed heads and generation/commit pages still use the checksums required by
the publication protocol. Cooperating writers can be mid-publication even in a
trusted deployment. The data switch therefore does not mean that every CRC
instruction in the library disappears. Standalone `ChunkedBytes` methods and
compression-container checks keep their own documented contracts; a file-reader
option is not carried inside a returned standalone value.

A skipped check cannot promise detection. Incorrect but well-formed metadata
may return wrong data when the trust assumption is violated. A retained check
still returns its ordinary error. Trust does not relax mmap immutability or
prevent SIGBUS after external truncation; see [SECURITY.md](../../SECURITY.md).

## Baseline review

The [pre-PR main snapshot](https://github.com/shim9610/varlit/tree/ba541cc3082cecdf7f385d06897a58a3889e1294)
already reread headers/footers, compared them with index entries and recomputed
CRC in `file/scan.rs::verify_snapshot_record`. It already returned early for
`IntegrityPolicy::None`. PR head `82e1077` shared that work with mapped reads but
still coupled the optional checks. Not all of this cost originated in security
hardening. The earlier `38c37d0` is already a PR snapshot, not the pre-PR baseline.

## Validation and performance method

Regression contracts exercise all four modes, independent metadata/payload
corruption, exact positional-read counts, resource refusal before I/O, real short
reads, allocation refusal/retry, compression header variants, matrix runs/columns
across compressed chunks and eight independent readers around pending/confirmed
publication. Index tests combine trusted/checked metadata with hostile heights,
cyclic children, short reads, intermediate I/O failure, allocator refusal,
cleanup and retry. Public additions have executable valid/invalid contracts;
the frozen legacy inventory is unchanged.

The comparison harness uses identical logical work, grouping, key order,
publication size and query locality across snapshots/modes. Pre-PR main and the
pre-change PR are separate baselines. Identical pre-change binaries provide A/A
controls. Timing samples are separate from sampled resource probes; CPU, memory,
temporary storage and reader drain remain charged. The existing **4 CPU / 16 GiB /
32 GiB** resource caps remain unchanged. No timing qualification or universal
speedup is implied by an option name. Whole-owner initialization proofs remain
in the separately planned library, as agreed.
