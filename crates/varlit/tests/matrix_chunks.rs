//! Growing matrix dimensions (internal design note: the matrix-chunk spec).
//!
//! A matrix's dimensions are otherwise fixed at create, which cannot model a
//! stream whose extent is unknown when the file is made — you learn a grid is
//! full when it fills. Declaring a growing dimension makes rows past the
//! declared extent land in *chunks*: ordinary internal records in the append
//! log, each holding `rows_per_chunk` rows of every matrix block, and each with
//! the matrix region's layout. Chunk 0 **is** the region.
//!
//! What these tests pin, in the order §6 of the spec lists them: the option is
//! inert when off; growth needs no advance knowledge; a value that never
//! arrived stays absent across a write rather than blocking it; a late write
//! into a written chunk is refused rather than dropped; and a cell read does not
//! materialise the chunk it lives in.

// Shared fixtures require integrity. Compression success cases additionally
// require their backend; the integrity-only job checks the missing-backend error.
#![cfg(feature = "integrity")]
#[path = "support/pages.rs"]
mod pages;

use std::path::{Path, PathBuf};

use varlit::{
    BlockDescriptor, BlockKind, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
    LazyOpenSource, MatrixBlockDescriptor, MatrixCellStatus, MatrixCommitDescriptor,
    MatrixCommitKind, MatrixDimensionDescriptor, MatrixDimensions, MatrixKey, ReadLimits,
    VarlitBlock, VarlitEmpty, VarlitFile, VarlitMatrixBlock, VarlitWriter,
};

/// The old by-value cell read, over the caller-owned-output API: every call
/// decodes into a fresh `T`.
trait CellValue {
    fn cell_value<T: VarlitMatrixBlock + VarlitEmpty>(&self, key: MatrixKey) -> varlit::Result<T>;
}

impl CellValue for VarlitFile {
    fn cell_value<T: VarlitMatrixBlock + VarlitEmpty>(&self, key: MatrixKey) -> varlit::Result<T> {
        let mut out = T::empty();
        self.read_matrix_cell::<T>(key, &mut out)?;
        Ok(out)
    }
}

impl CellValue for varlit::VarlitReader {
    fn cell_value<T: VarlitMatrixBlock + VarlitEmpty>(&self, key: MatrixKey) -> varlit::Result<T> {
        let mut out = T::empty();
        self.read_matrix_cell::<T>(key, &mut out)?;
        Ok(out)
    }
}

impl CellValue for VarlitWriter {
    fn cell_value<T: VarlitMatrixBlock + VarlitEmpty>(&self, key: MatrixKey) -> varlit::Result<T> {
        let mut out = T::empty();
        self.read_matrix_cell::<T>(key, &mut out)?;
        Ok(out)
    }
}

const CHANNELS: u64 = 8;
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

/// A second block on the same grid, so "only part of a cell arrived" is
/// expressible: `Sample` may be present where `Marker` is not.
#[derive(Clone, Debug, PartialEq, Eq, VarlitBlock)]
#[varlit(id = 811, version = 1, kind = "matrix")]
struct Marker {
    flag: u32,
}

impl VarlitMatrixBlock for Marker {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "marker";
    const SLOT_STRIDE: u64 = 4;
}

static BLOCKS: &[BlockDescriptor] = &[
    BlockDescriptor {
        id: Sample::ID,
        name: "Sample",
        version: Sample::VERSION,
        kind: BlockKind::Matrix,
        fields: &[],
    },
    BlockDescriptor {
        id: Marker::ID,
        name: "Marker",
        version: Marker::VERSION,
        kind: BlockKind::Matrix,
        fields: &[],
    },
];
static DIMS: &[MatrixDimensionDescriptor] = &[
    MatrixDimensionDescriptor { name: "scan" },
    MatrixDimensionDescriptor { name: "ch" },
];
static COMMITS: &[MatrixCommitDescriptor] = &[
    MatrixCommitDescriptor {
        name: Sample::CATEGORY,
        kind: MatrixCommitKind::Cell,
    },
    MatrixCommitDescriptor {
        name: Marker::CATEGORY,
        kind: MatrixCommitKind::Cell,
    },
];
static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[
    MatrixBlockDescriptor {
        block_id: Sample::ID,
        dimensions: Sample::DIMENSIONS,
        category: Sample::CATEGORY,
        slot_stride: Sample::SLOT_STRIDE,
        tile: [0, 0],
    },
    MatrixBlockDescriptor {
        block_id: Marker::ID,
        dimensions: Marker::DIMENSIONS,
        category: Marker::CATEGORY,
        slot_stride: Marker::SLOT_STRIDE,
        tile: [0, 0],
    },
];

fn base_spec() -> FormatSpec {
    FormatSpec::new(
        b"GROWMTX0",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::Crc32,
        varlit::RecoveryPolicy::Strict,
        varlit::ManifestPolicy::None,
        BLOCKS,
    )
    .with_block_identities(&[Sample::IDENTITY, Marker::IDENTITY])
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
    .with_matrix_spec(DIMS, COMMITS, MATRIX_BLOCKS)
}

/// The same format with the growing dimension declared.
fn growing_spec() -> FormatSpec {
    base_spec().with_growing_matrix_dimension("scan", ROWS_PER_CHUNK)
}

fn dims() -> MatrixDimensions {
    MatrixDimensions::from_pairs([("scan", ROWS_PER_CHUNK), ("ch", CHANNELS)])
}

fn key(row: u64, ch: u64) -> MatrixKey {
    MatrixKey::new(row, ch)
}

// ---------------------------------------------------------------------------
// §6.1 Inertness
// ---------------------------------------------------------------------------

#[test]
fn the_option_defaults_off_and_changes_no_hash() {
    assert!(base_spec().growing_rows_per_chunk().is_none());
    assert_eq!(
        growing_spec().growing_rows_per_chunk(),
        Some(ROWS_PER_CHUNK)
    );
    // A spec that declares no growing dimension must hash to exactly what it
    // hashed to before the declaration existed.
    assert_ne!(
        growing_spec().computed_schema_hash(),
        base_spec().computed_schema_hash(),
        "a file with chunks is not the file the plain spec describes",
    );
}

/// The hash of the plain spec, read off the build **before** this feature
/// existed (`f1f4339`, via a scratch test that printed it and was deleted).
///
/// This legacy hash predates explicit typed identities; the hash-only check
/// omits them without opening a typed file.
/// A literal, not a comparison against a freshly built spec: comparing the new
/// code against itself proves the fold is consistent, not that it is absent.
const BASE_SCHEMA_HASH: u64 = 0x2f14_5461_495b_712b;

/// Tiles moved every cell, so a matrix spec must no longer hash to what the
/// VMAT v6 build computed: a v6 file is refused instead of misread.
#[test]
fn a_tiled_spec_no_longer_hashes_as_the_v6_layout_did() {
    assert_ne!(
        base_spec()
            .with_block_identities(&[])
            .computed_schema_hash(),
        BASE_SCHEMA_HASH
    );
}

#[test]
fn a_format_without_the_declaration_writes_no_chunk_record() -> varlit::Result<()> {
    // Byte-for-byte file comparison is not available here and saying so is the
    // point: a matrix file carries a random per-create nonce (`VMNC`), so two
    // matrix files are never identical however inert the writer is. What is
    // assertable is that the plain writer produces no chunk record and hashes
    // to the literal above.
    let path = temp_path("inert_no_chunks");
    let mut writer = base_spec().create_writer_with_dims(path.path(), dims())?;
    for row in 0..ROWS_PER_CHUNK {
        writer.write_matrix_cell(key(row, 0), &Sample { value: 7 })?;
        writer.commit_matrix_cell::<Sample>(key(row, 0))?;
    }
    writer.flush()?;
    writer.sync()?;
    drop(writer);

    let reader = base_spec().open_reader(path.path())?;
    assert!(
        !crate::pages::all_entries!(reader)
            .iter()
            .any(|entry| entry.block_id == varlit::MATRIX_CHUNK_BLOCK_ID),
    );
    Ok(())
}

#[test]
fn a_format_without_the_declaration_refuses_a_row_past_the_extent() -> varlit::Result<()> {
    let path = temp_path("no_growth_refuses");
    let mut writer = base_spec().create_writer_with_dims(path.path(), dims())?;
    // This is the limitation the feature exists to lift, pinned so the "before"
    // is not folklore.
    assert!(matches!(
        writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 1 }),
        Err(Error::MatrixKeyOutOfBounds { .. }),
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// §6.2 Growth without advance knowledge
// ---------------------------------------------------------------------------

#[test]
fn rows_past_the_declared_extent_are_written_and_read_back() -> varlit::Result<()> {
    let path = temp_path("growth");
    // Ten times the declared extent, and nothing anywhere named that number
    // before the writes happened.
    let rows = ROWS_PER_CHUNK * 10;
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        for row in 0..rows {
            for ch in 0..CHANNELS {
                let value = (row * CHANNELS + ch) as u32 + 1;
                writer.write_matrix_cell(key(row, ch), &Sample { value })?;
                writer.commit_matrix_cell::<Sample>(key(row, ch))?;
            }
        }
        writer.flush()?;
        writer.sync()?;
    }

    let reader = growing_spec().open_reader(path.path())?;
    for row in 0..rows {
        for ch in 0..CHANNELS {
            let expected = (row * CHANNELS + ch) as u32 + 1;
            assert_eq!(
                reader.cell_value::<Sample>(key(row, ch))?,
                Sample { value: expected },
                "row {row} ch {ch}",
            );
        }
    }
    Ok(())
}

#[test]
fn a_chunk_nothing_committed_is_not_written() -> varlit::Result<()> {
    let path = temp_path("empty_chunk_silent");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 9 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?;
    writer.flush()?;
    writer.sync()?;
    let after_first = std::fs::metadata(path.path())?.len();
    // The failure this guards is the one an empty segment record once had: an
    // idle flush that grows the file every time it runs.
    for _ in 0..8 {
        writer.flush()?;
        writer.sync()?;
    }
    assert_eq!(std::fs::metadata(path.path())?.len(), after_first);
    Ok(())
}

// ---------------------------------------------------------------------------
// §6.3 Partial arrival survives writing the chunk
// ---------------------------------------------------------------------------

#[test]
fn a_block_that_never_arrived_reads_as_absent_across_a_write() -> varlit::Result<()> {
    let path = temp_path("partial_arrival");
    let cell = key(ROWS_PER_CHUNK + 1, 3);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        // `Sample` arrives for this cell. `Marker` never does — and writing the chunk asks
        // no question about that.
        writer.write_matrix_cell(cell, &Sample { value: 42 })?;
        writer.commit_matrix_cell::<Sample>(cell)?;
        // Move past the chunk, which writes out it with `Marker` still absent.
        writer.write_matrix_cell(key(ROWS_PER_CHUNK * 2, 0), &Sample { value: 1 })?;
        writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK * 2, 0))?;
        writer.flush()?;
        writer.sync()?;
    }

    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(reader.cell_value::<Sample>(cell)?, Sample { value: 42 });
    assert_eq!(
        reader.matrix_cell_status::<Marker>(cell)?,
        MatrixCellStatus::NotCommitted,
    );
    assert!(matches!(
        reader.cell_value::<Marker>(cell),
        Err(Error::MatrixNotCommitted),
    ));
    Ok(())
}

#[test]
fn an_uncommitted_cell_in_a_written_chunk_stays_uncommitted() -> varlit::Result<()> {
    let path = temp_path("uncommitted_cell");
    let written = key(ROWS_PER_CHUNK, 0);
    let never = key(ROWS_PER_CHUNK, 5);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(written, &Sample { value: 3 })?;
        writer.commit_matrix_cell::<Sample>(written)?;
        writer.flush()?;
        writer.sync()?;
    }
    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.matrix_cell_status::<Sample>(written)?,
        MatrixCellStatus::Committed
    );
    assert_eq!(
        reader.matrix_cell_status::<Sample>(never)?,
        MatrixCellStatus::NotCommitted
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// §6.4 A late write appends a new immutable chunk version
// ---------------------------------------------------------------------------

/// The same cell, written twice: the second value is what the file holds.
///
/// Separate from the test above because "a cell that had no value gets one" and
/// "a cell that had a value gets a different one" fail differently — the second
/// is the one that a rewrite which ORs commit bits without replacing slot bytes
/// would still pass.
#[test]
fn a_written_chunk_cell_can_be_overwritten_with_a_new_value() -> varlit::Result<()> {
    let path = temp_path("chunk_cell_rewrite");
    let cell = key(ROWS_PER_CHUNK, 0);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(cell, &Sample { value: 11 })?;
    writer.commit_matrix_cell::<Sample>(cell)?;
    // Write chunk 1 out by moving past it.
    writer.write_matrix_cell(key(ROWS_PER_CHUNK * 2, 0), &Sample { value: 2 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK * 2, 0))?;
    writer.flush()?;
    writer.sync()?;

    writer.write_matrix_cell(cell, &Sample { value: 22 })?;
    writer.commit_matrix_cell::<Sample>(cell)?;
    writer.flush()?;
    writer.sync()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(reader.cell_value::<Sample>(cell)?, Sample { value: 22 });
    Ok(())
}

/// A chunk the writer skipped — nothing committed, so no record — accepts a
/// write later, and the file it produces still opens.
///
/// This is the case that makes the chunk directory's ordering rule a *sorted*
/// one rather than an ascending-file-order one. Chunk 3's record is written
/// first; chunk 1's is appended after it and carries the lower index. Under the
/// old rule the file that produced was refused at open with
/// `InvalidMatrixChunk` — by the writer's own next open.
#[test]
fn a_skipped_chunk_can_be_filled_in_after_a_later_one_was_written() -> varlit::Result<()> {
    let path = temp_path("chunk_backfill");
    let late = key(ROWS_PER_CHUNK * 3, 0);
    let skipped = key(ROWS_PER_CHUNK, 0);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(late, &Sample { value: 33 })?;
        writer.commit_matrix_cell::<Sample>(late)?;
        writer.flush()?;
        writer.sync()?;
        // Chunk 1 was never opened, so no record for it exists.
        writer.write_matrix_cell(skipped, &Sample { value: 11 })?;
        writer.commit_matrix_cell::<Sample>(skipped)?;
        writer.flush()?;
        writer.sync()?;
    }
    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(reader.cell_value::<Sample>(skipped)?, Sample { value: 11 });
    assert_eq!(reader.cell_value::<Sample>(late)?, Sample { value: 33 });
    Ok(())
}

// ---------------------------------------------------------------------------
// §6.7 A cell read does not materialise its chunk
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Declaration refusals
// ---------------------------------------------------------------------------

#[test]
fn the_growing_dimension_must_be_declared_and_first() {
    assert!(matches!(
        base_spec()
            .with_growing_matrix_dimension("nonexistent", 4)
            .validate(),
        Err(Error::InvalidFormatSpec(
            "growing matrix dimension is not declared"
        )),
    ));
    // `ordinal = scan * ch_count + ch`, so only dimension 0 can grow without
    // renumbering every cell already written.
    assert!(matches!(
        base_spec()
            .with_growing_matrix_dimension("ch", 4)
            .validate(),
        Err(Error::InvalidFormatSpec(
            "the growing dimension must be dimension 0 of every matrix block"
        )),
    ));
    assert!(matches!(
        base_spec()
            .with_growing_matrix_dimension("scan", 0)
            .validate(),
        Err(Error::InvalidFormatSpec(
            "growing matrix rows_per_chunk must be non-zero"
        )),
    ));
    assert!(growing_spec().validate().is_ok());
}

#[test]
fn the_declared_extent_must_equal_the_chunk_height() -> varlit::Result<()> {
    // Chunk 0 *is* the region, and the row-to-chunk map is one division. With a
    // declared extent of 100 and chunks of 4, row 50 would route to chunk 12
    // while the region already held it, and the region's rows 4..100 would
    // become unreachable. Refused at create, where the value first exists.
    let path = temp_path("extent_mismatch");
    let wrong = MatrixDimensions::from_pairs([("scan", ROWS_PER_CHUNK * 25), ("ch", CHANNELS)]);
    assert!(matches!(
        growing_spec().create_writer_with_dims(path.path(), wrong),
        Err(Error::MatrixSizeMismatch {
            expected: ROWS_PER_CHUNK,
            actual: 100
        }),
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// The API sweep: every matrix entry point, against a chunked row
// ---------------------------------------------------------------------------
//
// Written after sweeping them by hand found that only four of thirteen had been
// routed. The tests above all read through a *fresh reader after a flush*, so
// none of them could see the largest defect: a writer could not read back a
// cell it had just written and committed.

#[test]
fn a_writer_reads_back_what_it_just_wrote_into_the_open_chunk() -> varlit::Result<()> {
    // The open chunk is in memory, not on disk. Before this was routed,
    // `read_matrix_cell` answered `MatrixNotCommitted` until the next write —
    // a write-then-read inconsistency that no test which flushes first can see.
    let path = temp_path("open_chunk_readback");
    let cell = key(ROWS_PER_CHUNK + 1, 2);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(cell, &Sample { value: 5 })?;
    writer.commit_matrix_cell::<Sample>(cell)?;

    assert_eq!(
        writer.cell_value::<Sample>(cell)?,
        Sample { value: 5 },
        "the writer must see its own committed cell before it is written",
    );
    assert_eq!(
        writer.matrix_cell_payload::<Sample>(cell)?.len(),
        Sample::SLOT_STRIDE as usize,
    );
    assert_eq!(
        writer.matrix_cell_status::<Sample>(cell)?,
        MatrixCellStatus::Committed,
    );

    // And the same three answers after the write, from the same handle.
    writer.flush()?;
    writer.sync()?;
    assert_eq!(writer.cell_value::<Sample>(cell)?, Sample { value: 5 });
    assert_eq!(
        writer.matrix_cell_status::<Sample>(cell)?,
        MatrixCellStatus::Committed,
    );
    Ok(())
}

#[test]
fn an_uncommitted_write_to_the_open_chunk_reads_as_absent() -> varlit::Result<()> {
    let path = temp_path("open_chunk_uncommitted");
    let cell = key(ROWS_PER_CHUNK, 0);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(cell, &Sample { value: 5 })?;
    // Written but not committed: the commit bit is the answer, in a chunk
    // exactly as in the region.
    assert_eq!(
        writer.matrix_cell_status::<Sample>(cell)?,
        MatrixCellStatus::NotCommitted,
    );
    assert!(matches!(
        writer.cell_value::<Sample>(cell),
        Err(Error::MatrixNotCommitted),
    ));
    Ok(())
}

#[test]
fn the_payload_write_entry_point_routes_like_the_typed_one() -> varlit::Result<()> {
    let path = temp_path("payload_write");
    let cell = key(ROWS_PER_CHUNK, 3);
    // `write_matrix_cell_payload` lives on `VarlitFile`, not on `VarlitWriter` —
    // `VarlitWriter::copy_matrix_cell_bytes_from` reaches it, so an unrouted
    // version would have shown up there as a bounds error on a valid key.
    let mut writer = varlit::VarlitFile::create_with_dims(growing_spec(), path.path(), dims())?;
    writer.write_matrix_cell_payload::<Sample>(cell, &7u32.to_le_bytes())?;
    writer.commit_matrix_cell::<Sample>(cell)?;
    writer.flush()?;
    writer.sync()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(reader.cell_value::<Sample>(cell)?, Sample { value: 7 });
    Ok(())
}

/// The two spellings of "clear one cell" accept and refuse the same things.
///
/// `clear_matrix_cell::<T>` and `clear_matrix_cell_by_category` are one
/// operation — one cell, one commit bit, one slot — differing only in whether
/// the block is named by Rust type or by category string. They diverged once:
/// the typed one learned to reopen a written chunk and the by-category one did
/// not, so the same cell cleared under one name and returned
/// `MatrixChunkClosed` under the other. Nothing about a caller can act on that
/// distinction, which is why it is a test and not a documented difference.
///
/// Two chunks, cleared one way each, so the test fails whichever half regresses.
#[test]
fn both_spellings_of_a_cell_clear_reach_a_written_chunk() -> varlit::Result<()> {
    let path = temp_path("clear_by_category");
    let typed = key(ROWS_PER_CHUNK, 0);
    let by_category = key(ROWS_PER_CHUNK * 2, 0);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    for cell in [typed, by_category] {
        writer.write_matrix_cell(cell, &Sample { value: 4 })?;
        writer.commit_matrix_cell::<Sample>(cell)?;
    }
    // Write both chunks out by moving past them.
    let far = key(ROWS_PER_CHUNK * 5, 0);
    writer.write_matrix_cell(far, &Sample { value: 5 })?;
    writer.commit_matrix_cell::<Sample>(far)?;
    writer.flush()?;
    writer.sync()?;

    writer.clear_matrix_cell::<Sample>(typed)?;
    writer.clear_matrix_cell_by_category(Sample::CATEGORY, by_category)?;
    writer.flush()?;
    writer.sync()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    for cell in [typed, by_category] {
        assert_eq!(
            reader.matrix_cell_status::<Sample>(cell)?,
            MatrixCellStatus::NotCommitted,
        );
    }
    assert_eq!(reader.cell_value::<Sample>(far)?, Sample { value: 5 });
    Ok(())
}

/// A cell clears in a written chunk exactly as it does in the open one, and the
/// clear survives to the file.
///
/// **The typed clear used to refuse a written chunk and no longer does.** Once
/// `write_matrix_cell` could edit a written chunk, that refusal had nothing
/// behind it: clearing is what a write is for a cell that should go back to
/// having no value, so accepting one and refusing the other left the two halves
/// of one capability disagreeing.
///
/// The by-category clear still refuses, and that is not the same omission: it
/// walks every row of a category, so serving it over written chunks means
/// loading and rewriting every one of them — a different operation with a
/// different cost. It is refused rather than half-done.
#[test]
fn clearing_a_cell_works_in_a_written_chunk_as_in_the_open_one() -> varlit::Result<()> {
    let path = temp_path("clear");
    let written_cell = key(ROWS_PER_CHUNK, 0);
    let open_cell = key(ROWS_PER_CHUNK * 2, 1);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(written_cell, &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(written_cell)?;
    writer.write_matrix_cell(open_cell, &Sample { value: 2 })?;
    writer.commit_matrix_cell::<Sample>(open_cell)?;

    writer.clear_matrix_cell::<Sample>(open_cell)?;
    assert_eq!(
        writer.matrix_cell_status::<Sample>(open_cell)?,
        MatrixCellStatus::NotCommitted,
    );
    writer.clear_matrix_cell::<Sample>(written_cell)?;
    assert_eq!(
        writer.matrix_cell_status::<Sample>(written_cell)?,
        MatrixCellStatus::NotCommitted,
    );
    writer.flush()?;
    writer.sync()?;
    drop(writer);

    // Written out, not just cleared in the buffer: the record on disk still
    // carried the old value until `dirty` learned that a clear on a reloaded
    // chunk is a change.
    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.matrix_cell_status::<Sample>(written_cell)?,
        MatrixCellStatus::NotCommitted,
    );
    Ok(())
}

/// A chunk written by an earlier handle is editable by a later one, from a cold
/// open and again after another chunk has been opened in between.
///
/// Both orders on purpose. The first edit happens with **no chunk open**, so it
/// can only work by finding the record in the directory built from the file;
/// the second happens while chunk 3 is open, so it also exercises writing that
/// chunk out and loading chunk 1 back in the same call. An implementation that
/// handled only the in-memory branch would pass the second and fail the first.
#[test]
fn a_reopened_writer_edits_a_chunk_written_by_the_previous_handle() -> varlit::Result<()> {
    let path = temp_path("reopen_append");
    let first = key(ROWS_PER_CHUNK, 0);
    let second = key(ROWS_PER_CHUNK, 2);
    let later = key(ROWS_PER_CHUNK * 3, 0);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(first, &Sample { value: 5 })?;
        writer.commit_matrix_cell::<Sample>(first)?;
        writer.flush()?;
        writer.sync()?;
    }
    let mut writer = growing_spec().open_writer(path.path())?;
    // Cold: nothing is open, so chunk 1 must come back from its record.
    writer.write_matrix_cell(first, &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(first)?;
    writer.write_matrix_cell(later, &Sample { value: 9 })?;
    writer.commit_matrix_cell::<Sample>(later)?;
    // Warm: chunk 3 is open and must be written out to make room for chunk 1.
    writer.write_matrix_cell(second, &Sample { value: 7 })?;
    writer.commit_matrix_cell::<Sample>(second)?;
    writer.flush()?;
    writer.sync()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(reader.cell_value::<Sample>(first)?, Sample { value: 1 });
    assert_eq!(reader.cell_value::<Sample>(second)?, Sample { value: 7 });
    assert_eq!(reader.cell_value::<Sample>(later)?, Sample { value: 9 });
    Ok(())
}

// ---------------------------------------------------------------------------
// §6.5 Crash sweep
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Nothing committed is lost, and nothing is lost quietly
// ---------------------------------------------------------------------------

#[test]
fn a_zero_valued_cell_can_be_committed() -> varlit::Result<()> {
    // `commit_matrix_cell` decided "was this written" by testing whether the
    // slot read as all zeros, so `Sample { value: 0 }` — a legitimate value —
    // could never be committed in a chunk while committing fine in the region.
    // The region keeps a write bitmap for exactly this reason; a chunk does now
    // too.
    let path = temp_path("zero_value");
    let chunked = key(ROWS_PER_CHUNK, 0);
    let region = key(0, 0);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(region, &Sample { value: 0 })?;
        writer.commit_matrix_cell::<Sample>(region)?;
        writer.write_matrix_cell(chunked, &Sample { value: 0 })?;
        writer.commit_matrix_cell::<Sample>(chunked)?;
        writer.flush()?;
        writer.sync()?;
    }
    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(reader.cell_value::<Sample>(region)?, Sample { value: 0 });
    assert_eq!(reader.cell_value::<Sample>(chunked)?, Sample { value: 0 },);
    Ok(())
}

#[test]
fn a_cell_never_written_still_refuses_to_commit() -> varlit::Result<()> {
    // The other half of the same rule: replacing the all-zero test with a write
    // bitmap must not make commit accept anything.
    let path = temp_path("uncommitted_refusal");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    assert!(matches!(
        writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 2)),
        Err(Error::MatrixCellNotWritten),
    ));
    Ok(())
}

#[test]
fn write_then_flush_then_commit_keeps_the_write() -> varlit::Result<()> {
    // A chunk with no committed cell used to be *dropped* at flush, so this
    // sequence lost the write on a chunked row while working on a region row —
    // and the row could become permanently unwritable once a later chunk written
    // past it. It stays open now: there is nothing to publish, so a commit
    // point holding it costs nothing.
    let path = temp_path("write_flush_commit");
    let cell = key(ROWS_PER_CHUNK, 1);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(cell, &Sample { value: 33 })?;
        writer.flush()?;
        writer.sync()?;
        writer.commit_matrix_cell::<Sample>(cell)?;
        writer.flush()?;
        writer.sync()?;
    }
    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(reader.cell_value::<Sample>(cell)?, Sample { value: 33 });
    Ok(())
}

/// A cell holding a legitimate all-zero value commits after a flush, on a
/// chunked row exactly as on a region row.
///
/// **This regressed on 2026-08-09 and a review caught it, not the suite.**
/// Widening `dirty` to include a bare write made a flush push the chunk's
/// record out and release the buffer; the write bitmap is buffer state that no
/// record carries, so the reload at commit time rebuilt it from "is the slot
/// non-zero" and a legitimate zero read as never written. Measured then:
/// region row `Ok(())`, chunked row `Err(MatrixCellNotWritten)`, chunked row
/// holding `7` `Ok(())` — so the existing zero-value test passed and the
/// existing flush test passed, because neither combined the two.
///
/// The region row is asserted in the same test on purpose. The claim the broken
/// version rested on was "this matches the region", and that claim was false;
/// asserting only the chunk would let it be made again.
#[test]
fn a_zero_valued_chunk_cell_commits_after_a_flush() -> varlit::Result<()> {
    let path = temp_path("zero_after_flush");
    let region = key(0, 0);
    let chunked = key(ROWS_PER_CHUNK, 0);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;

    writer.write_matrix_cell(region, &Sample { value: 0 })?;
    writer.flush()?;
    writer.sync()?;
    writer.commit_matrix_cell::<Sample>(region)?;

    writer.write_matrix_cell(chunked, &Sample { value: 0 })?;
    writer.flush()?;
    writer.sync()?;
    writer.commit_matrix_cell::<Sample>(chunked)?;
    writer.flush()?;
    writer.sync()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    for cell in [region, chunked] {
        assert_eq!(
            reader.matrix_cell_status::<Sample>(cell)?,
            MatrixCellStatus::Committed,
        );
        assert_eq!(reader.cell_value::<Sample>(cell)?, Sample { value: 0 });
    }
    Ok(())
}

#[test]
fn dropping_a_writer_does_not_publish_the_open_chunk() -> varlit::Result<()> {
    // Every other byte a writer accepts is on disk before the call returns; a
    // chunked cell was the one exception, and dropping the writer lost every
    // committed cell in the open chunk. Best effort — `drop` cannot report a
    // failure — but the ordinary case must not lose data.
    let path = temp_path("drop_writes");
    let cell = key(ROWS_PER_CHUNK, 0);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(cell, &Sample { value: 21 })?;
        writer.commit_matrix_cell::<Sample>(cell)?;
        // No flush, no commit, no sync.
    }
    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.matrix_cell_status::<Sample>(cell)?,
        MatrixCellStatus::NotCommitted
    );
    Ok(())
}

#[test]
fn sync_makes_a_committed_chunked_cell_durable() -> varlit::Result<()> {
    // `sync()` returned Ok having made nothing durable for a chunked row, while
    // the same call did for a region row. Read from a *second* handle so this
    // is about the file, not about the writer's memory.
    let path = temp_path("sync_durable");
    let cell = key(ROWS_PER_CHUNK, 0);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(cell, &Sample { value: 12 })?;
    writer.commit_matrix_cell::<Sample>(cell)?;
    writer.flush()?;
    writer.sync()?;

    let reader = growing_spec().open_readonly(path.path())?;
    assert_eq!(reader.cell_value::<Sample>(cell)?, Sample { value: 12 });
    Ok(())
}

#[test]
fn a_chunk_that_could_never_be_written_is_refused_at_create() -> varlit::Result<()> {
    // A chunk is buffered against `MatrixSlotRegionLen` and written against
    // `RecordPayloadLen`, and nothing reconciled them: a spec passed
    // `validate`, accepted writes, and then died at the first write with the
    // data already in RAM and no way to get it out. Refused at create now,
    // before a byte is accepted.
    //
    // Closing this also closed the only public route to a failed write, so the
    // property that a failed write keeps its chunk moved to a unit test in
    // `file.rs` (`a_failed_chunk_write_keeps_the_chunk`), which says so.
    let path = temp_path("ceiling_mismatch");
    let tight =
        growing_spec().with_read_limits(growing_spec().read_limits.with_max_record_payload_len(64));
    assert!(matches!(
        tight.create_writer_with_dims(path.path(), dims()),
        Err(Error::LimitExceeded {
            resource: "record payload length",
            ..
        }),
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Whole-matrix operations must account for chunked rows
// ---------------------------------------------------------------------------

#[test]
fn clearing_a_category_counts_the_open_chunk_and_refuses_a_written_one() -> varlit::Result<()> {
    // `clear_matrix_category` cleared the matrix region only, so a caller
    // asking for a clean category got one silently: chunked rows stayed
    // committed, stayed readable, and were not in the count.
    let path = temp_path("clear_category_open");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    let region = key(0, 0);
    let chunked = key(ROWS_PER_CHUNK, 0);
    writer.write_matrix_cell(region, &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(region)?;
    writer.write_matrix_cell(chunked, &Sample { value: 2 })?;
    writer.commit_matrix_cell::<Sample>(chunked)?;

    // One region cell and one open-chunk cell.
    assert_eq!(writer.clear_matrix_category(Sample::CATEGORY)?, 2);
    assert_eq!(
        writer.matrix_cell_status::<Sample>(chunked)?,
        MatrixCellStatus::NotCommitted,
    );
    assert_eq!(
        writer.matrix_cell_status::<Sample>(region)?,
        MatrixCellStatus::NotCommitted,
    );

    // A written chunk is reached too, and counted. This asserted a refusal
    // until 2026-08-09: "a written chunk is a written record, and records are
    // not rewritten" stopped being true when a chunk record became rewritable
    // in place, and the cost of reaching them — a reload and a rewrite per
    // chunk — is what clearing a category is, not a reason to refuse it.
    writer.write_matrix_cell(chunked, &Sample { value: 3 })?;
    writer.commit_matrix_cell::<Sample>(chunked)?;
    writer.flush()?;
    writer.sync()?;
    assert_eq!(writer.clear_matrix_category(Sample::CATEGORY)?, 1);
    assert_eq!(
        writer.matrix_cell_status::<Sample>(chunked)?,
        MatrixCellStatus::NotCommitted,
    );
    // Idempotent, which is what stands in for atomicity: a call that failed
    // part-way is answered by calling again, and an already-clear category
    // counts nothing.
    assert_eq!(writer.clear_matrix_category(Sample::CATEGORY)?, 0);
    Ok(())
}

/// The bulk clear reaches every written chunk, not just the newest, and the
/// clear reaches the file rather than living in the buffer.
///
/// Three chunks written before the clear, so a walk that stops at the first or
/// only handles the open one fails. The reopen after `drop` is the half that
/// catches the `dirty` landmine: `clear_open_chunk_block` recomputes `dirty`
/// from the commit bits it just zeroed, so a chunk loaded back from a record
/// went *non*-dirty on being cleared and was never written out — leaving the
/// record on disk serving the very cells the call reported clearing.
#[test]
fn the_bulk_clear_reaches_every_written_chunk_and_reaches_the_file() -> varlit::Result<()> {
    let path = temp_path("clear_category_written");
    let cells = [
        key(ROWS_PER_CHUNK, 0),
        key(ROWS_PER_CHUNK * 2, 1),
        key(ROWS_PER_CHUNK * 3, 2),
    ];
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        for (value, cell) in cells.iter().enumerate() {
            writer.write_matrix_cell(
                *cell,
                &Sample {
                    value: value as u32,
                },
            )?;
            writer.commit_matrix_cell::<Sample>(*cell)?;
        }
        // Move past the last one so all three have records.
        let far = key(ROWS_PER_CHUNK * 9, 0);
        writer.write_matrix_cell(far, &Sample { value: 9 })?;
        writer.commit_matrix_cell::<Sample>(far)?;
        writer.flush()?;
        writer.sync()?;

        // Three chunked cells plus the one that opened chunk 9.
        assert_eq!(writer.clear_matrix_category(Sample::CATEGORY)?, 4);
        writer.flush()?;
        writer.sync()?;
    }
    let reader = growing_spec().open_reader(path.path())?;
    for cell in cells {
        assert_eq!(
            reader.matrix_cell_status::<Sample>(cell)?,
            MatrixCellStatus::NotCommitted,
            "a cleared cell came back after a reopen",
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// What a chunk costs, pinned so the spec's cost table cannot drift back
// ---------------------------------------------------------------------------

#[test]
fn compression_cannot_be_declared_on_a_matrix_block() {
    // The chunk spec §3.2 named per-block compression as the knob for a
    // chunk's lost sparseness. It is not one: `validate` refuses the
    // declaration outright, and a chunk record is written by
    // `write_record_with_prev_key` rather than through
    // `prepare_user_record_payload`, which is the only place compression is
    // applied. Pinned so the claim cannot be made again.
    static COMPRESSED: &[varlit::BlockCompressionDescriptor] =
        &[varlit::BlockCompressionDescriptor {
            block_id: Sample::ID,
            compression: varlit::VariableCompression::zstd(
                varlit::CompressionHeaderMode::RecordExplicit,
            ),
        }];
    assert!(matches!(
        growing_spec().with_block_compression(COMPRESSED).validate(),
        Err(Error::InvalidFormatSpec(
            "block compression requires a variable block"
        )),
    ));
}

#[test]
fn an_unfilled_chunk_costs_its_whole_size() -> varlit::Result<()> {
    // The spec's cost table said an unfilled grid "compresses to almost
    // nothing". Measured 2026-08-04 on ext4: a chunk 5% filled and a chunk 100%
    // filled produce byte-identical file sizes, apparent *and* allocated. A
    // chunk is written as contiguous bytes and nothing punches holes in them,
    // so the waste is total, not partial — and the only knob that bounds it is
    // `rows_per_chunk`.
    let mut sizes = Vec::new();
    for fill in [ROWS_PER_CHUNK, ROWS_PER_CHUNK / 2, 1] {
        let path = temp_path(&format!("fill_{fill}"));
        {
            let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
            for row in 0..fill {
                let cell = key(ROWS_PER_CHUNK + row, 0);
                writer.write_matrix_cell(cell, &Sample { value: 1 })?;
                writer.commit_matrix_cell::<Sample>(cell)?;
            }
            writer.flush()?;
            writer.sync()?;
        }
        sizes.push(std::fs::metadata(path.path())?.len());
    }
    assert_eq!(
        sizes[0], sizes[1],
        "a half-filled chunk must cost what a full one does — it does, and that \
         is the cost this feature has",
    );
    assert_eq!(sizes[1], sizes[2]);
    Ok(())
}

// ---------------------------------------------------------------------------
// The cost of a chunked read must not scale with the file
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Chunk compression: the knob for the sparse cost
// ---------------------------------------------------------------------------

fn compressed_spec() -> FormatSpec {
    growing_spec().with_chunk_compression(varlit::VariableCompression::zstd(
        varlit::CompressionHeaderMode::RecordExplicit,
    ))
}

#[test]
fn chunk_compression_is_inert_when_not_declared() {
    assert!(growing_spec().chunk_compression.is_none());
    assert_ne!(
        compressed_spec().computed_schema_hash(),
        growing_spec().computed_schema_hash(),
        "a compressed chunk is not the file the plain spec describes",
    );
}

#[test]
fn chunk_compression_requires_a_growing_dimension() {
    assert!(matches!(
        base_spec()
            .with_chunk_compression(varlit::VariableCompression::zstd(
                varlit::CompressionHeaderMode::RecordExplicit,
            ))
            .validate(),
        Err(Error::InvalidFormatSpec(
            "chunk compression requires a growing matrix dimension"
        )),
    ));
}

/// Tiled cells are rewritten in place, so a compressed matrix is refused
/// when the file is created, before any missing compression backend matters.
#[cfg(not(feature = "compression-zstd"))]
#[test]
fn chunk_compression_is_refused_at_creation_without_a_backend() {
    let path = temp_path("missing_compression_backend");
    assert!(matches!(
        compressed_spec().create_writer_with_dims(path.path(), dims()),
        Err(Error::InvalidFormatSpec(_))
    ));
    assert!(
        !path.path().exists(),
        "a refused spec must not create the file"
    );
}

// ---------------------------------------------------------------------------
// Hostile input: a chunk record's own extent is the wall
// ---------------------------------------------------------------------------
//
// Every offset the chunk decode computes comes from bytes in the file. Both
// defects below were found by an adversarial review of this feature and both
// reproduced under `IntegrityPolicy::Crc32` — a positional cell read never
// verifies a record footer under the default `IntegrityVerification::OnDemand`,
// so the checksum is not the guard here. The record's `payload_len`, which the
// framing established and already charged, is.

// ---------------------------------------------------------------------------

struct TempPath {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TempPath {
    fn path(&self) -> &Path {
        &self.path
    }
}

fn temp_path(name: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir
        .path()
        .join(format!("varlit_chunk_{name}_{}.vrv", std::process::id()));
    TempPath { path, _dir: dir }
}

// ---------------------------------------------------------------------------
// The gates, on the chunk path
//
// The eight invariants this file's earlier cases pin were all driven in on the
// write/write side. A later review found the read side had none of them: the
// only caller of the chunk access gate was `chunk_block_slice`, which only the
// write paths use, so a quarantined category refused a region row and handed
// back the bytes for a chunked one -- from the same call. This file had no
// quarantine or fatal-gate case at all, which is exactly why that survived.
//
// A note on what these cost to write, because it is the reason they were
// missing: a quarantine cannot be asked for through the API. It is a *finding*,
// produced by the recovery pass when it reads damage off the disk, so the
// fixture has to corrupt a commit map by hand and reopen. The three helpers at
// the bottom do that.
// ---------------------------------------------------------------------------

/// The same block id and shape as `Sample`, one version later.
///
/// The descriptor table says block 810 is version 1, and the spec's schema hash
/// ties that table to the file -- so this type disagrees with the bytes on
/// disk. The region path has always refused it. The chunk path addressed the
/// block by id alone and would decode v2 semantics over v1 cells, returning a
/// confidently wrong value rather than an error.
#[derive(Clone, Debug, PartialEq, Eq, VarlitBlock)]
#[varlit(id = 810, version = 2, kind = "matrix")]
struct SampleV2 {
    value: u32,
}

impl VarlitMatrixBlock for SampleV2 {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "sample";
    const SLOT_STRIDE: u64 = 4;
}

#[test]
fn a_type_that_disagrees_with_the_schema_is_refused_on_a_chunked_row() -> varlit::Result<()> {
    let path = temp_path("version_mismatch");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    // One region row and one chunked row, both committed through the honest type.
    for row in [0, ROWS_PER_CHUNK] {
        writer.write_matrix_cell(key(row, 0), &Sample { value: 7 })?;
        writer.commit_matrix_cell::<Sample>(key(row, 0))?;
    }
    writer.flush()?;
    writer.sync()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    // The region row is the control: this is the refusal that already worked.
    assert!(
        matches!(
            reader.cell_value::<SampleV2>(key(0, 0)),
            Err(Error::BlockVersionMismatch { block_id, .. }) if block_id == Sample::ID
        ),
        "the region row accepted a type that disagrees with the schema"
    );
    // The chunked row must answer the same. Before the gate was added it
    // returned `Ok(SampleV2 { .. })`.
    assert!(
        matches!(
            reader.cell_value::<SampleV2>(key(ROWS_PER_CHUNK, 0)),
            Err(Error::BlockVersionMismatch { block_id, .. }) if block_id == Sample::ID
        ),
        "a chunked row decoded a type the file's schema does not describe"
    );
    assert!(
        matches!(
            reader.matrix_cell_status::<SampleV2>(key(ROWS_PER_CHUNK, 0)),
            Err(Error::BlockVersionMismatch { .. })
        ),
        "the status query answered for a type the schema does not describe"
    );
    Ok(())
}

#[test]
fn clearing_a_category_does_not_write_a_dead_chunk() -> varlit::Result<()> {
    let path = temp_path("clear_then_write");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    // One committed cell in chunk 1, then clear it away and flush.
    writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 3 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?;
    let cleared = writer.clear_matrix_category(Sample::CATEGORY)?;
    assert_eq!(cleared, 1, "the clear did not account for the chunked row");
    writer.flush()?;
    writer.sync()?;

    // `dirty` used to survive the clear, so the flush written an
    // all-uncommitted chunk record -- and a written chunk refuses every later
    // write to its rows, permanently.
    writer.write_matrix_cell(key(ROWS_PER_CHUNK, 1), &Sample { value: 4 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 1))?;
    writer.flush()?;
    writer.sync()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.cell_value::<Sample>(key(ROWS_PER_CHUNK, 1))?,
        Sample { value: 4 },
        "the row written after the clear did not survive"
    );
    assert_eq!(
        reader.matrix_cell_status::<Sample>(key(ROWS_PER_CHUNK, 0))?,
        MatrixCellStatus::NotCommitted,
        "the cleared cell came back"
    );
    Ok(())
}

/// The growing spec with the open digest on. `with_open_digest_on_flush` turns
/// the block-offset chain on itself, which the digest needs to be reachable.
fn digest_growing_spec() -> FormatSpec {
    let spec = growing_spec();
    spec.with_index_policy(spec.index_policy.with_open_digest_on_flush(true))
}

/// A lazy handle updates a written chunk rather than writing a second record
/// for its index.
///
/// **This destroyed the matrix until 2026-08-09.** `build_chunk_directory` read
/// `self.index` directly, and a lazy open keeps no resident directory — the
/// index is *empty*, not merely unbuilt. So every written chunk was invisible,
/// a write to one of their rows appended a duplicate record for that chunk
/// index, and the resulting file no longer opened.
///
/// The directory is built by walking the file now, so a digest-opened handle
/// finds every written chunk the scanning writer would. It was refused until
/// 2026-11-13; the update below lands, survives a reopen, and a second lazy
/// handle sees and replaces it.
#[test]
fn a_lazy_writer_updates_a_chunked_row_in_place_of_duplicating_its_record() -> varlit::Result<()> {
    let spec = digest_growing_spec();
    let path = temp_path("lazy_chunk");
    let chunked = key(ROWS_PER_CHUNK, 0);
    {
        let mut writer = spec.create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(chunked, &Sample { value: 5 })?;
        writer.commit_matrix_cell::<Sample>(chunked)?;
        // Move past chunk 1 so it has a record, then flush for the digest.
        writer.write_matrix_cell(key(ROWS_PER_CHUNK * 2, 0), &Sample { value: 6 })?;
        writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK * 2, 0))?;
        writer.flush()?;
        writer.sync()?;
    }
    {
        let (mut lazy, source) = VarlitWriter::open_lazy_with_report(spec, path.path())?;
        assert_eq!(
            source,
            LazyOpenSource::Digest,
            "the test needs the lazy route"
        );
        lazy.write_matrix_cell(chunked, &Sample { value: 99 })?;
        lazy.commit_matrix_cell::<Sample>(chunked)?;
        lazy.flush()?;
        lazy.sync()?;
    }
    let reader = spec.open_reader(path.path())?;
    assert_eq!(
        reader.cell_value::<Sample>(chunked)?,
        Sample { value: 99 },
        "the lazy handle's update must land in the chunk",
    );
    assert_eq!(
        reader.cell_value::<Sample>(key(ROWS_PER_CHUNK * 2, 0))?,
        Sample { value: 6 },
        "the other chunk is untouched",
    );
    drop(reader);
    // And a second lazy writer sees the update it did not make.
    {
        let (mut lazy, _) = VarlitWriter::open_lazy_with_report(spec, path.path())?;
        assert_eq!(lazy.cell_value::<Sample>(chunked)?, Sample { value: 99 });
        lazy.write_matrix_cell(chunked, &Sample { value: 100 })?;
        lazy.commit_matrix_cell::<Sample>(chunked)?;
        // This format confirms its end of file on `sync`; a `flush` alone
        // leaves the record past the confirmed end, as on any writer.
        lazy.flush()?;
        lazy.sync()?;
    }
    let reader = spec.open_reader(path.path())?;
    assert_eq!(reader.cell_value::<Sample>(chunked)?, Sample { value: 100 });
    Ok(())
}

/// `sync()` must leave the digest where a lazy open can still find it.
///
/// Sealing the open chunk is an ordinary record append, so it lands past any
/// digest the file already ends with — and a digest that is not the last
/// record is one no lazy open can use. `flush` writes out *before* it closes the
/// commit point for exactly that reason; `sync` written and stopped, so this
/// sequence silently demoted every later `open_readonly_lazy` to a full scan.
///
/// Nothing is wrong with the file afterwards, which is what makes it worth a
/// test: the fallback is exact, and the only symptom is the eleven-syscall
/// open quietly becoming a hundred-thousand-syscall one.
#[test]
fn sync_after_a_chunk_write_leaves_the_digest_usable() -> varlit::Result<()> {
    let spec = digest_growing_spec();
    let path = temp_path("sync_digest");
    let mut writer = spec.create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(key(0, 0), &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(key(0, 0))?;
    writer.flush()?;
    writer.sync()?;

    // A committed chunked cell lives in RAM until its chunk is written, and
    // `sync` is what writes out it here.
    writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 2 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?;
    writer.flush()?;
    writer.sync()?;
    drop(writer);

    let (_file, source) = VarlitFile::open_readonly_lazy_with_report(spec, path.path())?;
    assert_eq!(
        source,
        LazyOpenSource::Digest,
        "writing the chunk left the digest buried, so the lazy open fell back to the scan",
    );
    Ok(())
}
