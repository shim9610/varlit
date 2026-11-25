# Varlit

Version: **0.13.0-alpha.2** (pre-release) · Rust **1.95+** · MIT OR Apache-2.0

Varlit defines typed binary block formats in Rust. Declare the block layouts and
policies, append application-sized blocks, and let independent readers follow
published data. The library provides codecs, reads into caller-owned buffers,
in-place keyed storage, tiled matrices, explicit durability, recovery and
optional compression or memory mapping.

```toml
[dependencies]
varlit = { version = "0.13.0-alpha.2", features = ["integrity"] }
```

0.13.0 is not published yet. `0.13.0-alpha.2` is a pre-release on the main
branch: depend on it from git (`varlit = { git = "https://github.com/shim9610/varlit", features = ["integrity"] }`),
and expect the API and the file layouts new in 0.13 to change before the
release. The last published version is 0.12.1.

Start with the [quickstart](https://github.com/shim9610/varlit/blob/main/docs/quickstart.md), which writes grouped samples
through the stream API. See [API changes](https://github.com/shim9610/varlit/blob/main/docs/api-changes.md) when upgrading.

## Choose the storage path

| Workload | Start with | Layout choice |
| --- | --- | --- |
| Append/follow logging | Stream reader/writer | Pack related samples into a block; buffer several blocks per publication |
| Keyed block lookup | Native reader/writer with `keyed_storage: in_place` | One record per key, updated in place, so the file follows the live data rather than the number of updates; `put_*` / `get_*` / `remove_*` / `range_*`. Without it, keyed blocks are appended and lookup is resident |
| Direct grid/cell access | Matrix blocks | Choose dimensions, cell size and tile shape; cells are overwritten in place through a write-ahead ring |
| Small files, direct native record access | Native reader/writer | Open may scan all records and spill directory rows |

Block packing, buffered append batches and publication are separate decisions.
A batch does not pack independent input records into one encoded block. For
high-rate acquisition, choose the physical block size first and amortize `sync()`
over a publication batch. See [batch sizing and routing](https://github.com/shim9610/varlit/blob/main/docs/scalable-io.md#choose-the-physical-block-before-choosing-the-batch-size).

One writer owns mutations. Readers have independent snapshots and caches; use a
reader per thread and explicitly follow new publications. Multiple producers
can feed the writer through a bounded queue. Durability and visibility depend on
the selected policy; [the durability model](https://github.com/shim9610/varlit/blob/main/docs/durability-model.md) explains
`flush`, `sync`, transaction markers and matrix and keyed-storage publication.

## Features and contracts

| Cargo feature | Use |
| --- | --- |
| `integrity` | Stored CRC creation and verification |
| `compression-zstd` | Zstandard compression and decoding |
| `mmap` | Mapped byte views with stable backing-file requirements |
| `zero-copy` | Typed raw views; includes `mmap` |
| `experimental-matrix-reuse` | Explicit opt-in experimental matrix page reuse |
| `scalable-fault-injection` | Failure hooks for development and qualification |

Default features are empty. Stream APIs and finite keys are available without
extra feature flags. The experimental page-reuse format has its own
compatibility rules.

Read validation can select CRC and additional structure checks independently.
Bounds, resource/decoder limits, finite traversal and publication guards remain
required. Ordinary opens preserve their existing validation policy. See the
[read-policy table](https://github.com/shim9610/varlit/blob/main/docs/api-reference.md#read-validation) and [security contract](https://github.com/shim9610/varlit/blob/main/SECURITY.md).

Resource limits bound specified resources rather than total process memory.
Some public convenience allocations and application code remain infallible.
Mapped backing files must remain stable for the view lifetime. Platform sparse
allocation and durability depend on the actual filesystem. See
[known limitations](https://github.com/shim9610/varlit/blob/main/docs/known-limitations.md) before adoption.

## Documentation

| Guide | Purpose |
| --- | --- |
| [Quickstart](https://github.com/shim9610/varlit/blob/main/docs/quickstart.md) | Grouped append and reading published data |
| [How it works](https://github.com/shim9610/varlit/blob/main/docs/how-it-works.md) | Blocks, generated APIs and storage model |
| [API reference](https://github.com/shim9610/varlit/blob/main/docs/api-reference.md) | Public operations, errors and resource policies |
| [Scalable I/O](https://github.com/shim9610/varlit/blob/main/docs/scalable-io.md) | Stream handles: batching, follow and bounded caches |
| [Format author guide](https://github.com/shim9610/varlit/blob/main/docs/format-author-guide.md) | Schema, compression and matrix declarations |
| [Custom codecs](https://github.com/shim9610/varlit/blob/main/docs/custom-codec-guide.md) | Codec identity and allocation accounting |
| [Durability](https://github.com/shim9610/varlit/blob/main/docs/durability-model.md) / [recovery](https://github.com/shim9610/varlit/blob/main/docs/recovery-model.md) | Publication, failure results and repair |
| [Update and compact](https://github.com/shim9610/varlit/blob/main/docs/update-compact-guide.md) | Replacement, keyed updates and reclamation |
| [Chunk range streaming](https://github.com/shim9610/varlit/blob/main/docs/chunk-streaming.md) | Read decoded ranges with fixed caller scratch |
| [Wire specification](https://github.com/shim9610/varlit/blob/main/docs/spec.md) | Persistent bytes and compatibility obligations |
| [Migration](https://github.com/shim9610/varlit/blob/main/docs/migration-guide.md) | Version upgrades and explicit conversions |
| [Self-check](https://github.com/shim9610/varlit/blob/main/docs/self-check-guide.md) / [verification](https://github.com/shim9610/varlit/blob/main/docs/verification.md) | Diagnostics and qualification commands |

## Performance and verification

[`tools/format-bench`](tools/format-bench/README.md) compares Varlit with SQLite,
HDF5 and Parquet on grid, OLTP, analytics, mixed-stream and cell workloads, and
its `ab.py` compares two Varlit revisions. The manual *Performance A/B*
workflow runs that comparison on hosted runners; a change is flagged only when
the run ranges do not overlap and the medians differ by 5 % or more. Hosted
runners vary, so treat those numbers as comparisons, not absolute figures.
[`tools/storage-bench`](tools/storage-bench/README.md) keeps the older storage
workload definitions.

CI covers native Linux, Windows and Intel macOS, 32-bit x86 Linux, s390x
big-endian Linux under QEMU, feature interactions, external API consumers,
package archives, CodeQL, Kani, Verus, Loom, coverage and deliberate-defect
controls. The Sanitizers workflow separately runs Miri (including 32-bit and
big-endian targets), ASan/leak checks and a 90-second fuzz smoke run per target,
starting from the saved corpus and the committed seed corpus. Scheduled
workflows grow the fuzz corpus and run longer qualification profiles. Scopes and
exclusions are in the [verification guide](https://github.com/shim9610/varlit/blob/main/docs/verification.md).

## Contributing

See [CONTRIBUTING.md](https://github.com/shim9610/varlit/blob/main/CONTRIBUTING.md), [CODE_OF_CONDUCT.md](https://github.com/shim9610/varlit/blob/main/CODE_OF_CONDUCT.md)
and [SECURITY.md](https://github.com/shim9610/varlit/blob/main/SECURITY.md). Source and documentation history are retained in
Git. Varlit was renamed from Varve in 0.12.0; old versioned wire markers remain.
