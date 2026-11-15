//! A matrix inside an indexed (high-cardinality) primary, across anchors.
//!
//! Matrix storage records are appended into the same log as keyed records and
//! then rewritten in place. The sync-marker chain that recovery and readers
//! trust leaves their payloads out, so rewriting a cell long after its tile
//! was appended never makes an acknowledged link look damaged; the matrix's
//! ring keeps its own generations.
#![cfg(feature = "integrity")]

use varlit::{
    AnchorPolicy, DiskIndexOptions, DiskIndexPlan, DiskIndexedBlock, Error, FormatSpec,
    MatrixDimensions, MatrixKey, MatrixRegion, VarlitEmpty, VarlitIndexedReader,
    VarlitIndexedWriter, varlit_format,
};

varlit_format! {
    pub format IndexedGrid {
        magic: b"IDXGRID1";
        version: 1;
        // The indexed entry points read with explicit, finite limits only;
        // a matrix needs none of its own (its sizes follow from the block).
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks {
            matrix Cell(id = 1, dims = [row, col], category = data, tile = [4, 8]) { value: u64, }
            fixed Entry(id = 2, key = [id], key_index = disk) { id: u64, value: u64 }
        }
    }
}

const BLOCKS: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Entry>()];

fn spec() -> FormatSpec {
    IndexedGrid::spec().with_anchors(AnchorPolicy::every(13, true))
}

fn dims() -> MatrixDimensions {
    MatrixDimensions::from_pairs([("row", 16), ("col", 24)])
}

fn put(writer: &mut VarlitIndexedWriter, key: MatrixKey, value: u64) -> varlit::Result<()> {
    writer.write_matrix_cell(key, &Cell { value })?;
    writer.commit_matrix_cell::<Cell>(key)
}

fn cell(reader: &VarlitIndexedReader, key: MatrixKey) -> varlit::Result<u64> {
    let mut out = Cell::empty();
    reader.read_matrix_cell::<Cell>(key, &mut out)?;
    Ok(out.value)
}

fn entry(reader: &VarlitIndexedReader, id: u64) -> varlit::Result<Option<u64>> {
    let mut out = Entry::empty();
    Ok(reader.get::<Entry>(&id, &mut out)?.then_some(out.value))
}

/// Writes, overwrites and keyed records interleave over many syncs; a reader
/// following throughout sees each sync whole, across anchors, and a reopen
/// sees the last one.
#[test]
fn keyed_records_and_a_matrix_share_an_indexed_primary() -> varlit::Result<()> {
    let spec = spec();
    let plan = DiskIndexPlan::canonical(spec, BLOCKS)?;
    let options = DiskIndexOptions::default();
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("grid.varlit");
    let mut writer = VarlitIndexedWriter::create_with_dims(spec, &path, options, plan, dims())?;
    let mut reader = VarlitIndexedReader::open(spec, &path, options, plan)?;
    for round in 0..40u64 {
        // Every cell of two rows, rewritten every round: tiles appended in
        // the first rounds are rewritten in place for the rest.
        for row in [round % 16, (round + 7) % 16] {
            for col in 0..24 {
                put(
                    &mut writer,
                    MatrixKey::new(row, col),
                    round * 1000 + row * 24 + col,
                )?;
            }
        }
        for id in 0..20u64 {
            writer.push_info(&Entry {
                id: id + (round % 3) * 20,
                value: round,
            })?;
        }
        writer.sync()?;
        reader.follow()?;
        let row = round % 16;
        assert_eq!(
            cell(&reader, MatrixKey::new(row, 5))?,
            round * 1000 + row * 24 + 5
        );
        assert_eq!(entry(&reader, (round % 3) * 20 + 7)?, Some(round));
        let mut strip = vec![Cell::empty(); 24];
        reader.read_matrix_row_into::<Cell>(row, &mut strip)?;
        assert!(
            strip
                .iter()
                .enumerate()
                .all(|(col, c)| c.value == round * 1000 + row * 24 + col as u64)
        );
    }
    let physical = std::fs::metadata(&path)?.len();
    assert!(physical > 3 * 8192, "the fixture crossed too few anchors");
    let keys: Vec<MatrixKey> = (0..16).map(|row| MatrixKey::new(row, 11)).collect();
    let mut picked = vec![Cell::empty(); keys.len()];
    reader.read_matrix_cells_into::<Cell>(&keys, &mut picked)?;
    let mut page = vec![Cell::empty(); 8];
    let first =
        reader.read_matrix_region_into::<Cell>(MatrixRegion::new(0..2, 0..24), &mut page)?;
    assert_eq!(first.filled, 8);
    drop(reader);
    drop(writer);

    let reader = VarlitIndexedReader::open(spec, &path, options, plan)?;
    let mut again = vec![Cell::empty(); keys.len()];
    reader.read_matrix_cells_into::<Cell>(&keys, &mut again)?;
    assert_eq!(
        again.iter().map(|c| c.value).collect::<Vec<_>>(),
        picked.iter().map(|c| c.value).collect::<Vec<_>>()
    );
    assert_eq!(entry(&reader, 7)?, Some(39));
    Ok(())
}

/// A writer that closes with unsynced records leaves its sidecar behind the
/// primary, so the next open walks the acknowledged links — links holding
/// tiles rewritten in place since — and keeps every one of them. The unsynced
/// records and cells are cut; the matrix shows the last synced generation.
#[test]
fn recovery_keeps_links_whose_tiles_were_rewritten_since() -> varlit::Result<()> {
    let spec = spec();
    let plan = DiskIndexPlan::canonical(spec, BLOCKS)?;
    let options = DiskIndexOptions::default();
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("recover.varlit");
    {
        let mut writer = VarlitIndexedWriter::create_with_dims(spec, &path, options, plan, dims())?;
        for round in 0..30u64 {
            for col in 0..24 {
                put(&mut writer, MatrixKey::new(round % 4, col), round)?;
            }
            writer.push_info(&Entry {
                id: round,
                value: round,
            })?;
            writer.sync()?;
        }
        // Never synced: cut by the next open.
        for col in 0..24 {
            put(&mut writer, MatrixKey::new(0, col), 999)?;
        }
        writer.push_info(&Entry { id: 1000, value: 1 })?;
    }
    let writer = VarlitIndexedWriter::open(spec, &path, options, plan)?;
    drop(writer);
    let reader = VarlitIndexedReader::open(spec, &path, options, plan)?;
    // Round 28 last wrote row 0, round 29 row 1.
    assert_eq!(cell(&reader, MatrixKey::new(0, 3))?, 28);
    assert_eq!(cell(&reader, MatrixKey::new(1, 3))?, 29);
    assert_eq!(entry(&reader, 29)?, Some(29));
    assert_eq!(entry(&reader, 0)?, Some(0));
    assert_eq!(entry(&reader, 1000)?, None);
    Ok(())
}

/// The plain stream API has no matrix engine and keeps refusing one.
#[test]
fn a_plain_stream_still_refuses_a_matrix_format() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stream.varlit");
    let result =
        varlit::VarlitStreamWriter::create(spec(), &path, varlit::StreamOptions::default());
    assert!(matches!(result, Err(Error::StreamingUnsupported)));
}
