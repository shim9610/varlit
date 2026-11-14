//! Tiled matrix storage through the public API: what the file looks like
//! after overwrites, damage, crashes and readers that fall behind.
#![cfg(feature = "integrity")]

use std::io::{Read, Seek, SeekFrom, Write};

#[cfg(feature = "scalable-fault-injection")]
use varlit::VarlitFile;
use varlit::{
    Error, FormatSpec, MatrixCellStatus, MatrixDimensions, MatrixKey, VarlitEmpty, varlit_format,
};

varlit_format! {
    pub format Tiled {
        magic: b"TILEDT01";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks {
            matrix Cell(id = 1, dims = [row, col], category = data, tile = [4, 8]) {
                value: u64,
            }
        }
    }
}

fn spec() -> FormatSpec {
    Tiled::spec()
}

fn dims(rows: u64, cols: u64) -> MatrixDimensions {
    MatrixDimensions::from_pairs([("row", rows), ("col", cols)])
}

fn value(reader: &impl CellRead, key: MatrixKey) -> varlit::Result<u64> {
    let mut out = Cell::empty();
    reader.read(key, &mut out)?;
    Ok(out.value)
}

trait CellRead {
    fn read(&self, key: MatrixKey, out: &mut Cell) -> varlit::Result<()>;
}
impl CellRead for varlit::VarlitReader {
    fn read(&self, key: MatrixKey, out: &mut Cell) -> varlit::Result<()> {
        self.read_matrix_cell::<Cell>(key, out)
    }
}
impl CellRead for varlit::VarlitWriter {
    fn read(&self, key: MatrixKey, out: &mut Cell) -> varlit::Result<()> {
        self.read_matrix_cell::<Cell>(key, out)
    }
}

fn put(writer: &mut varlit::VarlitWriter, key: MatrixKey, value: u64) -> varlit::Result<()> {
    writer.write_matrix_cell(key, &Cell { value })?;
    writer.commit_matrix_cell::<Cell>(key)
}

/// Overwriting cells in place never grows the file: the write-ahead ring is
/// a fixed size and every cell keeps its place. Only a tile written for the
/// first time adds bytes.
#[test]
fn overwrites_keep_the_file_size_and_new_tiles_add_one_tile() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("size");
    let mut writer = spec().create_writer_with_dims(&path, dims(64, 64))?;
    for row in 0..8 {
        for col in 0..16 {
            put(&mut writer, MatrixKey::new(row, col), 0)?;
        }
    }
    writer.sync()?;
    let len = std::fs::metadata(&path)?.len();
    for round in 1..300u64 {
        put(&mut writer, MatrixKey::new(round % 8, round % 16), round)?;
        put(&mut writer, MatrixKey::new(0, 0), round)?;
        writer.sync()?;
    }
    assert_eq!(
        std::fs::metadata(&path)?.len(),
        len,
        "overwrites grew the file"
    );
    // A cell in a tile nothing has touched allocates that tile, and only it.
    put(&mut writer, MatrixKey::new(40, 40), 1)?;
    writer.sync()?;
    let grown = std::fs::metadata(&path)?.len() - len;
    // 32 cells of 8 bytes plus checksums, the tile header and bitmap, its
    // record framing, and a directory page.
    assert!(grown < 8 * 1024, "one new tile grew the file by {grown}");
    let reader = spec().open_reader(&path)?;
    assert_eq!(value(&reader, MatrixKey::new(0, 0))?, 299);
    assert_eq!(value(&reader, MatrixKey::new(40, 40))?, 1);
    Ok(())
}

/// A growing dimension grows past its declared extent; columns are bounded
/// by theirs. Tiles exist only where something was written.
#[test]
fn a_growing_matrix_writes_far_past_its_extent() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("grow");
    let growing = spec().with_growing_matrix_dimension("row", 4);
    let mut writer = growing.create_writer_with_dims(&path, dims(4, 1000))?;
    for (row, col) in [(0u64, 0u64), (1_000_000, 999), (5, 500)] {
        put(&mut writer, MatrixKey::new(row, col), row + col)?;
    }
    assert!(matches!(
        writer.write_matrix_cell(MatrixKey::new(0, 1000), &Cell { value: 1 }),
        Err(Error::MatrixKeyOutOfBounds { .. })
    ));
    writer.sync()?;
    let reader = growing.open_reader(&path)?;
    assert_eq!(value(&reader, MatrixKey::new(1_000_000, 999))?, 1_000_999);
    assert_eq!(value(&reader, MatrixKey::new(5, 500))?, 505);
    assert_eq!(
        reader.matrix_cell_status::<Cell>(MatrixKey::new(999_999, 999))?,
        MatrixCellStatus::NotCommitted
    );
    // Three tiles were written; the file holds three tiles, not a million rows.
    assert!(std::fs::metadata(&path)?.len() < 1 << 20);
    Ok(())
}

/// A reader keeps its snapshot while it is in the ring; once the ring has
/// moved past it, a read says so instead of mixing generations, and
/// `follow` brings the reader to the newest one.
#[test]
fn a_reader_that_falls_behind_the_ring_is_told_to_follow() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("expire");
    let small = spec().with_matrix_ring_bytes(80 * 1024);
    let mut writer = small.create_writer_with_dims(&path, dims(8, 8))?;
    put(&mut writer, MatrixKey::new(0, 0), 1)?;
    writer.sync()?;
    let mut old = small.open_reader(&path)?;
    let mut expired = false;
    for round in 2..4000u64 {
        put(&mut writer, MatrixKey::new(0, 0), round)?;
        writer.sync()?;
        match value(&old, MatrixKey::new(0, 0)) {
            Ok(seen) => {
                assert!(!expired, "an expired snapshot answered again");
                assert_eq!(seen, 1, "the old snapshot moved");
            }
            Err(Error::MatrixSnapshotExpired { .. }) => {
                expired = true;
                break;
            }
            Err(other) => return Err(other),
        }
    }
    assert!(expired, "the ring never moved past the old snapshot");
    old.follow()?;
    assert!(value(&old, MatrixKey::new(0, 0))? > 1);
    Ok(())
}

/// Changes never published are gone after a reopen, as after a crash; a
/// torn ring frame at the end is ignored, and the publication before it
/// stands.
#[test]
fn unpublished_and_torn_publications_never_reach_a_reopen() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("crash");
    {
        let mut writer = spec().create_writer_with_dims(&path, dims(8, 8))?;
        put(&mut writer, MatrixKey::new(1, 1), 11)?;
        writer.sync()?;
        put(&mut writer, MatrixKey::new(1, 1), 22)?;
        put(&mut writer, MatrixKey::new(2, 2), 22)?;
        // Dropped without a sync.
    }
    {
        let mut writer = spec().open_writer(&path)?;
        assert_eq!(value(&writer, MatrixKey::new(1, 1))?, 11);
        assert_eq!(
            writer.matrix_cell_status::<Cell>(MatrixKey::new(2, 2))?,
            MatrixCellStatus::NotCommitted
        );
        put(&mut writer, MatrixKey::new(1, 1), 33)?;
        writer.sync()?;
    }
    // Damage the newest ring frame: the 33 publication's frame holds the
    // byte pattern of its slot, the cell's encoded value.
    let mut bytes = Vec::new();
    std::fs::File::open(&path)?.read_to_end(&mut bytes)?;
    let needle = 33u64.to_le_bytes();
    let at = bytes
        .windows(8)
        .rposition(|window| window == needle)
        .expect("the frame holds the value");
    let mut file = std::fs::OpenOptions::new().write(true).open(&path)?;
    file.seek(SeekFrom::Start(at as u64))?;
    file.write_all(&[!needle[0]])?;
    file.sync_all()?;
    drop(file);
    let reader = spec().open_reader(&path)?;
    let seen = value(&reader, MatrixKey::new(1, 1))?;
    assert!(seen == 11 || seen == 33, "a damaged frame produced {seen}");
    assert_eq!(seen, 11, "the damaged frame was believed");
    Ok(())
}

/// A flipped bit in a cell's bytes is a checksum mismatch on read; one in a
/// tile's commit bitmap makes the category's reads fail and is a finding
/// of `verify_matrix_metadata`, and clearing the category recovers it.
#[test]
fn damage_in_place_is_reported_and_a_category_clear_recovers() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("damage");
    let marker = 0x5eed_5eed_5eed_5eedu64;
    {
        let mut writer = spec().create_writer_with_dims(&path, dims(8, 8))?;
        put(&mut writer, MatrixKey::new(0, 0), marker)?;
        put(&mut writer, MatrixKey::new(0, 1), 7)?;
        writer.sync()?;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)?.read_to_end(&mut bytes)?;
    let at = bytes
        .windows(8)
        .position(|window| window == marker.to_le_bytes())
        .expect("the cell is in place: a fresh tile is written directly");
    let flip = |offset: usize| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;
        let mut byte = [0u8];
        file.seek(SeekFrom::Start(offset as u64))?;
        file.read_exact(&mut byte)?;
        file.seek(SeekFrom::Start(offset as u64))?;
        file.write_all(&[byte[0] ^ 1])?;
        file.sync_all()
    };
    flip(at)?;
    let reader = spec().open_reader(&path)?;
    assert!(matches!(
        value(&reader, MatrixKey::new(0, 0)),
        Err(Error::MatrixChecksumMismatch { .. })
    ));
    assert_eq!(value(&reader, MatrixKey::new(0, 1))?, 7);
    drop(reader);
    flip(at)?;

    // The tile's bitmap sits between its 40-byte header and its slots; the
    // first slot starts 8-aligned after the bitmap and its checksum.
    let bitmap = at - 8; // 32 cells: 4 bitmap bytes and 4 checksum bytes
    flip(bitmap)?;
    let reader = spec().open_reader(&path)?;
    assert!(value(&reader, MatrixKey::new(0, 1)).is_err());
    let report = reader.verify_matrix_metadata()?;
    assert!(
        !report.findings.is_empty(),
        "the damaged bitmap went unreported"
    );
    drop(reader);
    // Clearing the category resets the damaged bitmap; rebuilding from the
    // per-cell checksums then commits every cell whose bytes are intact.
    let mut writer = spec().open_writer(&path)?;
    writer.clear_matrix_category("data")?;
    assert_eq!(writer.rebuild_matrix_commit_from_crc::<Cell>()?, 2);
    writer.sync()?;
    drop(writer);
    let reader = spec().open_reader(&path)?;
    assert_eq!(value(&reader, MatrixKey::new(0, 0))?, marker);
    assert_eq!(value(&reader, MatrixKey::new(0, 1))?, 7);
    Ok(())
}

/// Compression cannot coexist with cells rewritten in place.
#[test]
fn chunk_compression_is_refused() {
    let compressed = spec()
        .with_growing_matrix_dimension("row", 4)
        .with_chunk_compression(varlit::VariableCompression::zstd(
            varlit::CompressionHeaderMode::RecordExplicit,
        ));
    assert!(matches!(
        compressed.validate(),
        Err(Error::InvalidFormatSpec(_))
    ));
}

/// A cell read is charged against `MaterializedBytes` before anything is
/// allocated for it.
#[test]
fn a_cell_read_is_refused_by_the_materialization_limit() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("limit");
    {
        let mut writer = spec().create_writer_with_dims(&path, dims(8, 8))?;
        put(&mut writer, MatrixKey::new(0, 0), 9)?;
        writer.sync()?;
    }
    let tight = spec().read_limits.with_max_materialized_bytes(4);
    let reader = spec().open_reader_with_limits(&path, tight)?;
    assert!(matches!(
        value(&reader, MatrixKey::new(0, 0)),
        Err(Error::LimitExceeded { .. })
    ));
    Ok(())
}

/// A large matrix file opens from an anchor: its tiles are records like any
/// other, and the open frames about one anchor spacing of them.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn a_large_matrix_file_opens_from_an_anchor() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("anchored");
    let anchored = spec().with_anchors(varlit::AnchorPolicy::every(16, true));
    {
        let mut writer = anchored.create_writer_with_dims(&path, dims(4096, 64))?;
        for row in (0..4096).step_by(4) {
            for col in (0..64).step_by(8) {
                put(&mut writer, MatrixKey::new(row, col), row * 64 + col)?;
            }
            if row % 256 == 0 {
                writer.sync()?;
            }
        }
        writer.sync()?;
    }
    let before = VarlitFile::records_framed();
    let reader = anchored.open_reader(&path)?;
    let framed = VarlitFile::records_framed() - before;
    assert!(framed < 400, "the open framed {framed} records");
    assert_eq!(value(&reader, MatrixKey::new(4092, 56))?, 4092 * 64 + 56);
    assert_eq!(value(&reader, MatrixKey::new(0, 0))?, 0);
    Ok(())
}

fn filled_grid(path: &std::path::Path, rows: u64, cols: u64) -> varlit::Result<()> {
    let mut writer = spec().create_writer_with_dims(path, dims(rows, cols))?;
    for row in 0..rows {
        for col in 0..cols {
            put(&mut writer, MatrixKey::new(row, col), row * 1000 + col)?;
        }
    }
    writer.sync()
}

/// A region is read row-major into the caller's buffer, page by page: each
/// page fills what the buffer holds and names where to continue, across
/// tile boundaries, until the region is done.
#[test]
fn a_region_reads_in_pages_that_continue_where_they_stopped() -> varlit::Result<()> {
    use varlit::MatrixRegion;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("region");
    filled_grid(&path, 20, 30)?;
    let reader = spec().open_reader(&path)?;
    // 5 rows x 13 columns crossing tile rows (4) and tile columns (8).
    let region = MatrixRegion::new(3..8, 5..18);
    let mut expected = Vec::new();
    for row in 3..8 {
        for col in 5..18 {
            expected.push(row * 1000 + col);
        }
    }
    for page_len in [1usize, 7, 13, 64, 100] {
        let mut got = Vec::new();
        let mut out = vec![Cell::empty(); page_len];
        let mut page = reader.read_matrix_region_into::<Cell>(region, &mut out)?;
        loop {
            got.extend(out[..page.filled].iter().map(|cell| cell.value));
            let Some(next) = page.next else { break };
            assert_eq!(
                page.filled, page_len,
                "a page ended before its buffer was full"
            );
            page = reader.read_matrix_region_from::<Cell>(next, &mut out)?;
        }
        assert_eq!(got, expected, "page of {page_len}");
    }
    // The encoded form matches.
    let mut bytes = vec![0u8; 8 * 30];
    let page = reader.read_matrix_region_bytes_into::<Cell>(region, &mut bytes)?;
    assert_eq!(page.filled, 30);
    let first: Vec<u64> = bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|b| u64::from_le_bytes(*b))
        .collect();
    assert_eq!(first, expected[..30]);
    let cursor = page.next.expect("the region goes on");
    assert_eq!(cursor.next_key(), MatrixKey::new(5, 9));
    Ok(())
}

/// A row and a column are regions one cell wide.
#[test]
fn rows_and_columns_are_one_wide_regions() -> varlit::Result<()> {
    use varlit::MatrixRegion;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("rowcol");
    filled_grid(&path, 12, 20)?;
    let reader = spec().open_reader(&path)?;
    let mut row = vec![Cell::empty(); 20];
    let page = reader.read_matrix_region_into::<Cell>(MatrixRegion::row(9, 0..20), &mut row)?;
    assert_eq!((page.filled, page.next), (20, None));
    assert!(
        row.iter()
            .enumerate()
            .all(|(c, cell)| cell.value == 9000 + c as u64)
    );
    let mut column = vec![Cell::empty(); 12];
    let page =
        reader.read_matrix_region_into::<Cell>(MatrixRegion::column(17, 0..12), &mut column)?;
    assert_eq!((page.filled, page.next), (12, None));
    assert!(
        column
            .iter()
            .enumerate()
            .all(|(r, cell)| cell.value == r as u64 * 1000 + 17)
    );
    Ok(())
}

/// Out-of-bounds regions and uncommitted cells are refused, as by the cell
/// reads.
#[test]
fn a_region_read_refuses_what_a_cell_read_refuses() -> varlit::Result<()> {
    use varlit::MatrixRegion;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("refuse");
    {
        let mut writer = spec().create_writer_with_dims(&path, dims(8, 8))?;
        put(&mut writer, MatrixKey::new(0, 0), 1)?;
        put(&mut writer, MatrixKey::new(0, 2), 3)?;
        writer.sync()?;
    }
    let reader = spec().open_reader(&path)?;
    let mut out = vec![Cell::empty(); 8];
    assert!(matches!(
        reader.read_matrix_region_into::<Cell>(MatrixRegion::row(0, 0..3), &mut out),
        Err(Error::MatrixNotCommitted)
    ));
    assert!(matches!(
        reader.read_matrix_region_into::<Cell>(MatrixRegion::row(0, 0..9), &mut out),
        Err(Error::MatrixKeyOutOfBounds { .. })
    ));
    assert!(matches!(
        reader.read_matrix_region_into::<Cell>(MatrixRegion::new(2..2, 0..1), &mut out),
        Err(Error::MatrixKeyOutOfBounds { .. })
    ));
    let mut bytes = vec![0u8; 7];
    assert!(
        reader
            .read_matrix_region_bytes_into::<Cell>(MatrixRegion::row(0, 0..1), &mut bytes)
            .is_err()
    );
    Ok(())
}

/// A page is one snapshot: while another thread publishes whole-grid
/// generations, every page a reader takes holds a single generation.
#[test]
fn a_page_never_mixes_generations() -> varlit::Result<()> {
    use varlit::MatrixRegion;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("pages");
    let mut writer = spec().create_writer_with_dims(&path, dims(16, 16))?;
    for row in 0..16 {
        for col in 0..16 {
            put(&mut writer, MatrixKey::new(row, col), 0)?;
        }
    }
    writer.sync()?;
    let reader_path = path.clone();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = done.clone();
    let checker = std::thread::spawn(move || -> varlit::Result<u64> {
        let mut reader = spec().open_reader(&reader_path)?;
        let mut out = vec![Cell::empty(); 256];
        let mut pages = 0;
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            reader.follow()?;
            let page = reader
                .read_matrix_region_into::<Cell>(MatrixRegion::new(0..16, 0..16), &mut out)?;
            assert_eq!(page.filled, 256);
            let first = out[0].value;
            assert!(
                out.iter().all(|cell| cell.value == first),
                "a page mixed generations"
            );
            pages += 1;
        }
        Ok(pages)
    });
    for generation in 1..150u64 {
        for row in 0..16 {
            for col in 0..16 {
                put(&mut writer, MatrixKey::new(row, col), generation)?;
            }
        }
        writer.sync()?;
    }
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let pages = checker.join().expect("the checker panicked")?;
    assert!(pages > 0);
    Ok(())
}

/// A batch of keys is read in the caller's order, whatever order the keys
/// come in, across tiles, with repeats, and past one decode batch.
#[test]
fn a_key_batch_reads_in_key_order_with_repeats() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("keys");
    filled_grid(&path, 40, 60)?;
    // Scattered keys: a stride walk over the grid, then repeats.
    let mut keys: Vec<MatrixKey> = (0..1500u64)
        .map(|i| {
            let linear = (i * 1031) % 2400;
            MatrixKey::new(linear / 60, linear % 60)
        })
        .collect();
    keys.extend_from_slice(&[
        MatrixKey::new(7, 7),
        MatrixKey::new(39, 59),
        MatrixKey::new(7, 7),
    ]);
    let expected: Vec<u64> = keys.iter().map(|k| k.scan * 1000 + k.ch).collect();
    let reader = spec().open_reader(&path)?;
    let mut out = vec![Cell::empty(); keys.len()];
    reader.read_matrix_cells_into::<Cell>(&keys, &mut out)?;
    assert_eq!(out.iter().map(|c| c.value).collect::<Vec<_>>(), expected);
    let mut bytes = vec![0u8; keys.len() * 8];
    reader.read_matrix_cells_bytes_into::<Cell>(&keys, &mut bytes)?;
    let decoded: Vec<u64> = bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|b| u64::from_le_bytes(*b))
        .collect();
    assert_eq!(decoded, expected);
    // Seven channels of the latest row.
    let picks: Vec<MatrixKey> = [53u64, 2, 31, 9, 44, 17, 58]
        .iter()
        .map(|ch| MatrixKey::new(39, *ch))
        .collect();
    let mut seven = vec![Cell::empty(); 7];
    reader.read_matrix_cells_into::<Cell>(&picks, &mut seven)?;
    assert_eq!(
        seven.iter().map(|c| c.value).collect::<Vec<_>>(),
        vec![39053, 39002, 39031, 39009, 39044, 39017, 39058]
    );
    // An empty batch reads nothing.
    reader.read_matrix_cells_into::<Cell>(&[], &mut [])?;
    // A writer reads its own unpublished values the same way.
    let mut writer = spec().open_writer(&path)?;
    writer.write_matrix_cell(MatrixKey::new(39, 2), &Cell { value: 1 })?;
    writer.commit_matrix_cell::<Cell>(MatrixKey::new(39, 2))?;
    writer.read_matrix_cells_into::<Cell>(&picks, &mut seven)?;
    assert_eq!(seven[1].value, 1);
    assert_eq!(seven[0].value, 39053);
    Ok(())
}

/// A key batch refuses what a cell read refuses, and an output of the
/// wrong length.
#[test]
fn a_key_batch_refuses_what_a_cell_read_refuses() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("keyrefuse");
    {
        let mut writer = spec().create_writer_with_dims(&path, dims(8, 8))?;
        put(&mut writer, MatrixKey::new(0, 0), 1)?;
        put(&mut writer, MatrixKey::new(5, 6), 2)?;
        writer.sync()?;
    }
    let reader = spec().open_reader(&path)?;
    let mut out = vec![Cell::empty(); 2];
    reader
        .read_matrix_cells_into::<Cell>(&[MatrixKey::new(5, 6), MatrixKey::new(0, 0)], &mut out)?;
    assert_eq!((out[0].value, out[1].value), (2, 1));
    // Same tile, uncommitted neighbour.
    assert!(matches!(
        reader.read_matrix_cells_into::<Cell>(
            &[MatrixKey::new(0, 0), MatrixKey::new(0, 1)],
            &mut out
        ),
        Err(Error::MatrixNotCommitted)
    ));
    // A tile never written.
    assert!(matches!(
        reader.read_matrix_cells_into::<Cell>(
            &[MatrixKey::new(0, 0), MatrixKey::new(0, 7)],
            &mut out
        ),
        Err(Error::MatrixNotCommitted)
    ));
    assert!(matches!(
        reader.read_matrix_cells_into::<Cell>(
            &[MatrixKey::new(0, 0), MatrixKey::new(8, 0)],
            &mut out
        ),
        Err(Error::MatrixKeyOutOfBounds { .. })
    ));
    assert!(matches!(
        reader.read_matrix_cells_into::<Cell>(&[MatrixKey::new(0, 0)], &mut out),
        Err(Error::MatrixSizeMismatch { .. })
    ));
    let mut bytes = vec![0u8; 15];
    assert!(
        reader
            .read_matrix_cells_bytes_into::<Cell>(
                &[MatrixKey::new(0, 0), MatrixKey::new(5, 6)],
                &mut bytes
            )
            .is_err()
    );
    let mut bytes = vec![0u8; 8];
    assert!(matches!(
        reader.read_matrix_cells_bytes_into::<Cell>(
            &[MatrixKey::new(0, 0), MatrixKey::new(5, 6)],
            &mut bytes
        ),
        Err(Error::MatrixSizeMismatch { .. })
    ));
    Ok(())
}

/// A key batch is one snapshot: while another thread publishes whole-grid
/// generations, every batch a reader takes holds a single generation.
#[test]
fn a_key_batch_never_mixes_generations() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("keygen");
    let mut writer = spec().create_writer_with_dims(&path, dims(16, 16))?;
    for row in 0..16 {
        for col in 0..16 {
            put(&mut writer, MatrixKey::new(row, col), 0)?;
        }
    }
    writer.sync()?;
    let keys: Vec<MatrixKey> = (0..64u64)
        .map(|i| MatrixKey::new((i * 7) % 16, (i * 11) % 16))
        .collect();
    let reader_path = path.clone();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = done.clone();
    let checker = std::thread::spawn(move || -> varlit::Result<u64> {
        let mut reader = spec().open_reader(&reader_path)?;
        let mut out = vec![Cell::empty(); keys.len()];
        let mut batches = 0;
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            reader.follow()?;
            reader.read_matrix_cells_into::<Cell>(&keys, &mut out)?;
            let first = out[0].value;
            assert!(
                out.iter().all(|cell| cell.value == first),
                "a batch mixed generations"
            );
            batches += 1;
        }
        Ok(batches)
    });
    for generation in 1..150u64 {
        for row in 0..16 {
            for col in 0..16 {
                put(&mut writer, MatrixKey::new(row, col), generation)?;
            }
        }
        writer.sync()?;
    }
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let batches = checker.join().expect("the checker panicked")?;
    assert!(batches > 0);
    Ok(())
}

varlit_format! {
    pub format TiledWithTails {
        magic: b"TILEDHT1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        index: header_tails;
        commit: transaction_marker(on_flush);
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks {
            matrix TCell(id = 1, dims = [row, col], category = data, tile = [4, 8]) { value: u64, }
            fixed Note(id = 2) { seq: u64 }
        }
    }
}

varlit_format! {
    pub format TiledWithSlots {
        magic: b"TILEDHS1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        header_slots {
            capacity: 256;
            integrity: rolling;
            blocks: [Mark];
        }
        blocks {
            matrix SCell(id = 1, dims = [row, col], category = data, tile = [4, 8]) { value: u64, }
            variable Mark(id = 2) { at: u64, }
        }
    }
}

/// A header tail region and a header slot region both grow the file header;
/// the matrix's records start after it, wherever that is, so both combine
/// with a matrix: cells, records and the region survive a reopen.
#[test]
fn header_tails_and_header_slots_combine_with_a_matrix() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("tails");
    {
        let mut writer = TiledWithTails::spec().create_writer_with_dims(&path, dims(10, 10))?;
        for i in 0..30u64 {
            let key = MatrixKey::new(i % 10, (i * 3) % 10);
            writer.write_matrix_cell(key, &TCell { value: i })?;
            writer.commit_matrix_cell::<TCell>(key)?;
            writer.push(&Note { seq: i })?;
            // `transaction_marker(on_flush)`: the flush commits the records,
            // the sync makes them and the matrix durable.
            writer.flush()?;
            writer.sync()?;
        }
    }
    let reader = TiledWithTails::spec().open_reader(&path)?;
    let mut cell = TCell::empty();
    reader.read_matrix_cell::<TCell>(MatrixKey::new(9, 7), &mut cell)?;
    assert_eq!(cell.value, 29);
    let mut notes = Vec::new();
    reader.decode_blocks_into::<Note>(&mut notes)?;
    assert_eq!(notes.len(), 30);
    assert_eq!(notes[29].seq, 29);

    let path = dir.path().join("slots");
    {
        let mut writer = TiledWithSlots::spec().create_writer_with_dims(&path, dims(10, 10))?;
        for i in 0..30u64 {
            let key = MatrixKey::new(i % 10, (i * 7) % 10);
            writer.write_matrix_cell(key, &SCell { value: i })?;
            writer.commit_matrix_cell::<SCell>(key)?;
            writer.write_header_block(&Mark { at: i })?;
            writer.sync()?;
        }
    }
    let reader = TiledWithSlots::spec().open_reader(&path)?;
    let mut cell = SCell::empty();
    reader.read_matrix_cell::<SCell>(MatrixKey::new(9, 3), &mut cell)?;
    assert_eq!(cell.value, 29);
    let mut mark = Mark::empty();
    assert!(reader.read_header_block(&mut mark)?);
    assert_eq!(mark.at, 29);
    Ok(())
}
