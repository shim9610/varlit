//! A followed handle must not keep a chunk directory built before the follow.
//!
//! `VarlitFile::follow` extends a handle along the append log, and a matrix
//! chunk record is an ordinary append-log record — so a run can bring new
//! chunks. The chunk directory is built once on the first chunked read and
//! cached in a `OnceLock`, which nothing in the tree invalidates. Caching it
//! forever was sound only while a handle's snapshot could not grow.
//!
//! Measured before the fix: the followed handle answered `MatrixNotCommitted`
//! for a cell that a fresh open of the same file read back as `Sample { value:
//! 80 }`. The handle had the chunk's record in its index and no entry for it in
//! its directory.

#![cfg(feature = "integrity")]
use varlit::{
    BlockDescriptor, BlockKind, Endian, FormatSpec, IndexPolicy, IntegrityPolicy,
    MatrixBlockDescriptor, MatrixCommitDescriptor, MatrixCommitKind, MatrixDimensionDescriptor,
    MatrixDimensions, MatrixKey, ReadLimits, VarlitBlock, VarlitEmpty, VarlitMatrixBlock,
    VarlitReader,
};

/// Owned-result shim over the caller-owned-output read.
trait ReadCell {
    fn read_cell(&self, key: MatrixKey) -> varlit::Result<Sample>;
}

impl ReadCell for VarlitReader {
    fn read_cell(&self, key: MatrixKey) -> varlit::Result<Sample> {
        let mut out = Sample::empty();
        self.read_matrix_cell::<Sample>(key, &mut out)?;
        Ok(out)
    }
}

const CHANNELS: u64 = 4;
const ROWS_PER_CHUNK: u64 = 4;

#[derive(Clone, Debug, PartialEq, Eq, VarlitBlock)]
#[varlit(id = 810, version = 1, kind = "matrix")]
struct Sample {
    value: u32,
}
impl VarlitMatrixBlock for Sample {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "sample";
    const SLOT_STRIDE: u64 = 4;
}

static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
    id: Sample::ID,
    name: "Sample",
    version: Sample::VERSION,
    kind: BlockKind::Matrix,
    fields: &[],
}];
static DIMS: &[MatrixDimensionDescriptor] = &[
    MatrixDimensionDescriptor { name: "scan" },
    MatrixDimensionDescriptor { name: "ch" },
];
static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
    name: Sample::CATEGORY,
    kind: MatrixCommitKind::Cell,
}];
static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
    block_id: Sample::ID,
    dimensions: Sample::DIMENSIONS,
    category: Sample::CATEGORY,
    slot_stride: Sample::SLOT_STRIDE,
    tile: [0, 0],
}];

fn spec() -> FormatSpec {
    FormatSpec::new(
        b"PRBCHNK0",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::Crc32,
        varlit::RecoveryPolicy::Strict,
        varlit::ManifestPolicy::None,
        BLOCKS,
    )
    .with_block_identities(&[Sample::IDENTITY])
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
    .with_matrix_spec(DIMS, COMMITS, MATRIX_BLOCKS)
    .with_growing_matrix_dimension("scan", ROWS_PER_CHUNK)
}

fn dims() -> MatrixDimensions {
    MatrixDimensions::from_pairs([("scan", ROWS_PER_CHUNK), ("ch", CHANNELS)])
}
fn key(row: u64, ch: u64) -> MatrixKey {
    MatrixKey::new(row, ch)
}

#[test]
fn following_an_overwritten_chunk_adopts_only_its_confirmed_version() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("overwrite.vrv");
    let cell = key(ROWS_PER_CHUNK, 0);
    let mut writer = spec().create_writer_with_dims(&path, dims())?;
    writer.write_matrix_cell(cell, &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(cell)?;
    writer.sync()?;
    let mut old = spec().open_reader(&path)?;
    assert_eq!(old.read_cell(cell)?.value, 1);
    writer.write_matrix_cell(cell, &Sample { value: 2 })?;
    writer.commit_matrix_cell::<Sample>(cell)?;
    writer.flush()?;
    old.follow()?;
    assert_eq!(old.read_cell(cell)?.value, 1);
    assert_eq!(spec().open_reader(&path)?.read_cell(cell)?.value, 1);
    writer.sync()?;
    assert_eq!(old.read_cell(cell)?.value, 1);
    old.follow()?;
    assert_eq!(old.read_cell(cell)?.value, 2);
    Ok(())
}
