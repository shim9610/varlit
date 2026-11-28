#![forbid(unsafe_code)]
//! Format-first, append-friendly custom binary formats for Rust.
//!
//! Varlit turns a *declaration* of a binary format into the format: you write
//! the header identity, resource limits, durability and integrity policies, and
//! the block set once, and the macro generates the on-disk contract together
//! with the typed Rust API that reads and writes it. Each declaration is a
//! file format of its own: its files are read by the reader generated from
//! the same declaration, and a reader generated from another declaration
//! refuses them.
//!
//! A declaration chooses its storage paths. Records are appended to a log,
//! and readers take a snapshot at open and follow new publications; stream
//! handles append and follow at a cost that does not grow with the file; keyed
//! storage keeps one record per key in a B-tree inside the file, updated in
//! place; matrix blocks address fixed-size cells in tiles overwritten in place.
//! Matrix and keyed-storage changes go through a write-ahead ring inside the
//! file and are published atomically.
//!
//! The README is deliberately not inlined here: it is packaged alongside this
//! crate, but its path differs between the repository checkout and the
//! published `.crate`, so `include_str!` would break one of the two.
//!
//! # Crate layout
//!
//! `varlit` is a facade. It re-exports the entire runtime from
//! [`varlit_core`](https://docs.rs/varlit-core) and the two procedural macros
//! from [`varlit_macros`](https://docs.rs/varlit-macros). Depend on `varlit`
//! alone; the other two crates are implementation packages and their direct
//! use is not a supported configuration.
//!
//! | Entry point | Purpose |
//! | --- | --- |
//! | [`varlit_format!`] | declare a whole format: header identity, limits, policies, blocks, and the typed API generated from them |
//! | [`VarlitBlock`] (derive) | declare one stored record type outside a format declaration |
//! | [`FormatSpec`] | the resolved, immutable format contract a handle is opened against |
//! | [`VarlitFile`], [`VarlitReader`], [`VarlitWriter`] | untyped handles; the generated wrappers sit on top of these |
//! | [`VarlitEncode`] / [`VarlitDecode`] | field and block payload codecs, including custom ones |
//!
//! # Getting started
//!
//! ```
//! use varlit::{VarlitEmpty, varlit_format};
//!
//! varlit_format! {
//!     pub format AppFormat {
//!         magic: b"APPF";
//!         version: 1;
//!         endian: little;
//!         schema_hash: computed;
//!         manifest: embedded;
//!         index: [scan_on_open, block_offset_chain, keyed_offset_chain];
//!         commit: transaction_marker(on_flush);
//!
//!         blocks {
//!             fixed Point(id = 1) {
//!                 x: u32,
//!                 y: u32,
//! }
//!
//!             variable User(id = 2, key = [id]) {
//!                 id: u64,
//!                 name: String,
//! }
//! }
//! }
//! }
//!
//! # fn main() -> varlit::Result<()> {
//! # let dir = std::env::temp_dir().join(format!("varlit-doctest-{}", std::process::id()));
//! # std::fs::create_dir_all(&dir).unwrap();
//! # let path = dir.join("app.appf");
//! {
//!     let mut writer = AppFormat::create_writer(&path)?;
//!     writer.push_point(&Point { x: 1, y: 2 })?;
//!     writer.push_user(&User { id: 7, name: "Ada".to_string() })?;
//!     writer.flush()?;
//! }
//!
//! let reader = AppFormat::open_reader(&path)?;
//! let mut points: Vec<Point> = Vec::new();
//! reader.points_decoded_into(&mut points)?; // decodes into a Vec you own
//! assert_eq!(points, vec![Point { x: 1, y: 2 }]);
//! let mut user = User::empty(); // reads decode into a value you own
//! assert!(reader.users()?.get(&7, &mut user)?);
//! assert_eq!(user.name, "Ada");
//! # std::fs::remove_dir_all(&dir).unwrap();
//! # Ok(())
//! # }
//! ```
//!
//! Before wiring a new format into application logic, run
//! [`FormatSelfTest`]: it exercises the generated APIs, codecs, keyed lookup,
//! and a file round-trip for representative values, and reports which layer to
//! look at when something disagrees.
//!
//! # Cargo features
//!
//! All features are off by default; the default build is the pure-Rust core
//! with no optional dependencies. Stream handles, finite keys and scan control
//! are part of this default API; no Cargo feature is required. The former
//! `high-cardinality-dev` feature has been removed.
//!
//! | Feature | Adds | Stability |
//! | --- | --- | --- |
//! | `integrity` | record and matrix cell/commit-map CRC evidence | stable |
//! | `compression-zstd` | zstd compression of variable-block payloads and of [`ChunkedBytes`] | stable |
//! | `mmap` | read-only memory-mapped payload and matrix windows | stable, `unsafe` entry points |
//! | `zero-copy` | raw typed views over mapped bytes; implies `mmap` | stable, `unsafe` entry points |
//! | `experimental-matrix-reuse` | opt-in page slot reuse and automatic-follow owned matrix reads | **experimental**, disabled by default |
//! | `scalable-fault-injection` | fault-injection counters and hooks used by the scalable-path tests | **test infrastructure**: not a production feature, and the counters it exposes are `#[doc(hidden)]` |
//!
//! Requesting a feature-gated operation without its feature is a typed error
//! ([`Error::CompressionFeatureDisabled`], [`Error::IntegrityFeatureDisabled`])
//! rather than a silent fallback.
//!
//! # Guides
//!
//! The repository carries the long-form documentation; these are the entry
//! points, under `docs/` at
//! <https://github.com/shim9610/varlit>:
//!
//! | Guide | Covers |
//! | --- | --- |
//! | `quickstart.md` | shortest path from declaration to a file |
//! | `format-author-guide.md` | declaring a format clause by clause, and what each clause generates |
//! | `spec.md` | the normative wire contract, including streams, tiled matrices and keyed storage, and the `varlit_format!` macro contract |
//! | `how-it-works.md` | how a declaration becomes a file: the storage paths, the published end, streams, matrices and keyed storage, with diagrams |
//! | `api-reference.md` | the complete public surface, feature by feature |
//! | `custom-codec-guide.md` | writing a codec, and the mandatory `SCHEMA_ID` identity contract |
//! | `durability-model.md` | what `flush`, `sync`, and `commit_durable` promise |
//! | `recovery-model.md` | how damaged files are classified and what may be rebuilt |
//! | `scalable-io.md` | stream handles: append and follow without rescanning the file |
//! | `known-limitations.md` | the scale contracts, resource budgets, and what is not verified |
//! | `migration-guide.md` | version-to-version behaviour changes |
//!
//! [`ChunkedBytes`]: crate::ChunkedBytes
//! [`Error::CompressionFeatureDisabled`]: crate::Error::CompressionFeatureDisabled
//! [`Error::IntegrityFeatureDisabled`]: crate::Error::IntegrityFeatureDisabled
//! [`FormatSelfTest`]: crate::FormatSelfTest
//! [`FormatSpec`]: crate::FormatSpec
//! [`VarlitDecode`]: crate::VarlitDecode
//! [`VarlitEncode`]: crate::VarlitEncode
//! [`VarlitFile`]: crate::VarlitFile
//! [`VarlitReader`]: crate::VarlitReader
//! [`VarlitWriter`]: crate::VarlitWriter

pub use varlit_core::*;
pub use varlit_macros::{VarlitBlock, varlit_format};

#[doc(hidden)]
pub mod __core {
    pub use varlit_core::*;
}
