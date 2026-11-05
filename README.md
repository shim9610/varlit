# Varlit

Version: **0.13.0** · Rust **1.95+** · MIT OR Apache-2.0

Varlit defines typed binary block formats in Rust. Declare the block layouts and
policies, append application-sized blocks, and let independent readers follow
published data. The library provides codecs, bounded working buffers, disk
indexes, explicit durability, recovery and optional compression or memory mapping.

```toml
[dependencies]
varlit = { version = "0.13.0", features = ["integrity"] }
```

Start with the [quickstart](https://github.com/shim9610/varlit/blob/main/docs/quickstart.md), which writes grouped samples
through the stream API. See [API changes](https://github.com/shim9610/varlit/blob/main/docs/api-changes.md) when upgrading.

## Choose the storage path

| Workload | Start with | Layout choice |
| --- | --- | --- |
| Append/follow logging | Stream reader/writer | Pack related samples into a block; buffer several blocks per publication |
| Keyed block lookup | Indexed reader/writer | Use a block key; bounded declared key domains use finite-key tables |
| Direct grid/cell access | Matrix blocks | Choose dimensions, cell size and generation policy for the access pattern |
| Small files, direct native record access | Native reader/writer | Open may scan all records and spill directory rows |

Block packing, buffered append batches and publication are separate decisions.
A batch does not pack independent input records into one encoded block. For
high-rate acquisition, choose the physical block size first and amortize `sync()`
over a publication batch. See [batch sizing and routing](https://github.com/shim9610/varlit/blob/main/docs/scalable-io.md#choose-the-physical-block-before-choosing-the-batch-size).

One writer owns mutations. Readers have independent snapshots and caches; use a
reader per thread and explicitly follow new publications. Multiple producers
can feed the writer through a bounded queue. Durability and visibility depend on
the selected policy; [the durability model](https://github.com/shim9610/varlit/blob/main/docs/durability-model.md) explains
`flush`, `sync`, transaction markers and matrix generations.

## Features and contracts

| Cargo feature | Use |
| --- | --- |
| `integrity` | Stored CRC creation and verification |
| `compression-zstd` | Zstandard compression and decoding |
| `mmap` | Mapped byte views with stable backing-file requirements |
| `zero-copy` | Typed raw views; includes `mmap` |
| `experimental-matrix-reuse` | Explicit opt-in experimental matrix page reuse |
| `scalable-fault-injection` | Failure hooks for development and qualification |

Default features are empty. Stream/indexed APIs and finite keys are available
without extra feature flags. The experimental page-reuse format has its own
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
| [Scalable I/O](https://github.com/shim9610/varlit/blob/main/docs/scalable-io.md) | Batching, key routing, follow and bounded caches |
| [Format author guide](https://github.com/shim9610/varlit/blob/main/docs/format-author-guide.md) | Schema, compression and matrix declarations |
| [Custom codecs](https://github.com/shim9610/varlit/blob/main/docs/custom-codec-guide.md) | Codec identity and allocation accounting |
| [Durability](https://github.com/shim9610/varlit/blob/main/docs/durability-model.md) / [recovery](https://github.com/shim9610/varlit/blob/main/docs/recovery-model.md) | Publication, failure results and repair |
| [Update and compact](https://github.com/shim9610/varlit/blob/main/docs/update-compact-guide.md) | Replacement, keyed updates and reclamation |
| [Chunk range streaming](https://github.com/shim9610/varlit/blob/main/docs/chunk-streaming.md) | Read decoded ranges with fixed caller scratch |
| [Wire specification](https://github.com/shim9610/varlit/blob/main/docs/spec.md) | Persistent bytes and compatibility obligations |
| [Migration](https://github.com/shim9610/varlit/blob/main/docs/migration-guide.md) | Version upgrades and explicit conversions |
| [Self-check](https://github.com/shim9610/varlit/blob/main/docs/self-check-guide.md) / [verification](https://github.com/shim9610/varlit/blob/main/docs/verification.md) | Diagnostics and qualification commands |

## Performance and verification

The [current workload comparison](https://github.com/shim9610/varlit/blob/main/docs/performance/workload-storage-20261104/README.md)
contains grouped logging, arrays, table-oriented cases, concurrent reader progress
and CPU/memory/storage measurements. Read-policy measurements are included in the
same report. Results apply to their recorded workloads and environments.

CI covers native Linux, Windows and Intel macOS, feature interactions, external
API consumers, package archives, CodeQL, Kani and deliberate-defect controls.
Sanitizers separately runs Miri, ASan/leaks and bounded fuzz smoke. Their scopes
and exclusions are in the [verification guide](https://github.com/shim9610/varlit/blob/main/docs/verification.md).

## Contributing

See [CONTRIBUTING.md](https://github.com/shim9610/varlit/blob/main/CONTRIBUTING.md), [CODE_OF_CONDUCT.md](https://github.com/shim9610/varlit/blob/main/CODE_OF_CONDUCT.md)
and [SECURITY.md](https://github.com/shim9610/varlit/blob/main/SECURITY.md). Source and documentation history are retained in
Git. Varlit was renamed from Varve in 0.12.0; old versioned wire markers remain.
