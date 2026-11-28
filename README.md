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
How Varlit compares with SQLite, HDF5 and Parquet, and why it is measured that
way, is on the [benchmarks](https://github.com/shim9610/varlit/blob/main/docs/benchmarks.md) page.

## Examples

Each example declares its own format with `varlit_format!`. A declaration is a
file format of its own: its files are read by the reader generated from the same
declaration, and a reader generated from another declaration refuses them. The
examples below are complete programs; CI runs them
(`python3 scripts/check_docs.py --quickstart`).

### Append and follow a stream

Records are appended in batches; `sync()` makes them durable and visible, and a
reader in another thread or process follows only what each sync published.

```rust
use varlit::{BatchOptions, StreamOptions, VarlitEmpty, varlit_format};

varlit_format! {
    pub format Capture {
        magic: b"CAPTURE";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        blocks {
            variable Frame(id = 1) {
                first_sample: u64,
                samples: Vec<u32>,
            }
        }
    }
}

fn main() -> varlit::Result<()> {
    let path = "capture.varlit";
    let mut writer = Capture::create_stream_writer(path, StreamOptions::default())?;
    let frame = |n: u64| Frame { first_sample: n * 1024, samples: vec![7; 1024] };

    // Append a batch of frames; sync() makes them durable and visible.
    writer.push_frames((0..4).map(frame), BatchOptions::default())
        .map_err(|failure| failure.source)?;
    writer.sync()?;

    // A reader in another thread or process sees what was synced.
    let mut reader = Capture::open_stream_reader(path, StreamOptions::default())?;
    let mut frames = reader.frames()?;
    let mut out = Frame::empty(); // decoded into, reused for every record
    let mut seen = 0;
    while frames.next_into(&mut out)? {
        seen += 1;
    }
    assert_eq!(seen, 4);

    // More data later: the reader follows only what the next sync published.
    writer.push_frames((4..6).map(frame), BatchOptions::default())
        .map_err(|failure| failure.source)?;
    writer.sync()?;
    reader.follow_blocks(&mut frames)?;
    while frames.next_into(&mut out)? {
        seen += 1;
    }
    assert_eq!(seen, 6);
    assert_eq!(out.first_sample, 5 * 1024);
    Ok(())
}
```

### Update records by key in place

`keyed_storage: in_place` keeps one record per key in a B-tree inside the file.
`put_*` and `remove_*` change it in place, `sync()` publishes the changes
atomically, and a lookup reads only the pages on its path.

```rust
use varlit::{VarlitEmpty, varlit_format};

varlit_format! {
    pub format Accounts {
        magic: b"ACCOUNTS";
        version: 1;
        schema_hash: computed;
        integrity: crc32;
        keyed_storage: in_place;
        blocks {
            fixed Account(id = 1, key = [id]) { id: u64, balance: i64, }
        }
    }
}

fn main() -> varlit::Result<()> {
    let path = "accounts.varlit";
    let mut writer = Accounts::create_writer(path)?;
    for id in 0..1000 {
        writer.put_account(&Account { id, balance: 100 })?;
    }
    // Updating a key rewrites its record in place; the file does not grow
    // with the number of updates.
    writer.put_account(&Account { id: 7, balance: 250 })?;
    writer.remove_account(&8)?;
    writer.sync()?; // publishes every change above as one generation

    let reader = Accounts::open_reader(path)?;
    let mut scratch = Vec::new();
    let mut out = Account::empty();
    assert!(reader.get_account(&7, &mut scratch, &mut out)?);
    assert_eq!(out.balance, 250);
    assert!(!reader.get_account(&8, &mut scratch, &mut out)?);
    Ok(())
}
```

### Overwrite cells of a matrix

Matrix blocks address fixed-size cells by their dimensions. Cells are
overwritten in place through a write-ahead ring inside the file (16 MiB by
default); a reader keeps the generation it opened until it calls `follow()`.

```rust
use varlit::{MatrixDimensions, MatrixKey, VarlitEmpty, varlit_format};

varlit_format! {
    pub format Grid {
        magic: b"GRIDDEMO";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks {
            matrix Cell(id = 1, dims = [row, col], category = data) {
                value: f64,
            }
        }
    }
}

fn main() -> varlit::Result<()> {
    let path = "grid.varlit";
    let dims = MatrixDimensions::from_pairs([("row", 1024), ("col", 1024)]);
    let mut writer = Grid::spec().create_writer_with_dims(path, dims)?;

    // Write a cell, then mark it committed.
    let key = MatrixKey::new(3, 5);
    writer.write_matrix_cell(key, &Cell { value: 1.5 })?;
    writer.commit_matrix_cell::<Cell>(key)?;
    writer.sync()?; // publishes the cells as one generation

    let mut reader = Grid::spec().open_reader(path)?;
    let mut out = Cell::empty();
    reader.read_matrix_cell::<Cell>(key, &mut out)?;
    assert_eq!(out.value, 1.5);

    // Overwrites go through the write-ahead ring, in place; a reader keeps
    // its generation until it follows.
    writer.write_matrix_cell(key, &Cell { value: 2.5 })?;
    writer.commit_matrix_cell::<Cell>(key)?;
    writer.sync()?;
    reader.read_matrix_cell::<Cell>(key, &mut out)?;
    assert_eq!(out.value, 1.5);
    reader.follow()?;
    reader.read_matrix_cell::<Cell>(key, &mut out)?;
    assert_eq!(out.value, 2.5);
    Ok(())
}
```

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

[Benchmarks](https://github.com/shim9610/varlit/blob/main/docs/benchmarks.md) states the purpose, the method and the latest
results, including where Varlit is behind.
[`tools/format-bench`](https://github.com/shim9610/varlit/blob/main/tools/format-bench/README.md) compares Varlit with SQLite,
HDF5 and Parquet on grid, OLTP, analytics, mixed-stream and cell workloads, and
its `ab.py` compares two Varlit revisions. The manual *Performance A/B*
workflow runs that comparison on hosted runners; a change is flagged only when
the run ranges do not overlap and the medians differ by 5 % or more. Hosted
runners vary, so treat those numbers as comparisons, not absolute figures.
[`tools/storage-bench`](https://github.com/shim9610/varlit/blob/main/tools/storage-bench/README.md) keeps the older storage
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
