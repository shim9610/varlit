use stratava::{
    Error, IntegrityPolicy, MatrixDimensions, MatrixKey, MatrixMetadataResidency,
    MatrixMetadataVerification, ReadLimits, stratava_format,
};

stratava_format! {
    pub format Bulk {
        magic: b"MATRUN01"; version: 1; endian: little;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks { matrix Cell(id = 1, dims = [row, col], category = data) { value: u64, } }
    }
}
stratava_format! {
    pub format Columns {
        magic: b"MATCOL01"; version: 1; endian: little;
        dims { col: u32, row: u32, }
        commit: cell_bitmap { keyspace = [col, row]; categories = [data]; };
        blocks { matrix Column(id = 1, dims = [col, row], category = data) { value: u64, } }
    }
}
fn policies() -> Vec<IntegrityPolicy> {
    vec![
        IntegrityPolicy::None,
        #[cfg(feature = "integrity")]
        IntegrityPolicy::Crc32,
    ]
}
fn key(i: u64, width: u64) -> MatrixKey {
    MatrixKey::new(i / width, i % width)
}
fn spec(crc: IntegrityPolicy, growing: bool) -> stratava::FormatSpec {
    let spec = Bulk::spec()
        .with_integrity_policy(crc)
        .with_resource_limits(
            ReadLimits::STANDARD
                .with_matrix_directory_cache_bytes(0)
                .with_matrix_metadata_residency(MatrixMetadataResidency::Lazy { cache_bytes: 0 })
                .with_matrix_metadata_verification(MatrixMetadataVerification::OnDemand),
        );
    if growing {
        spec.with_growing_matrix_dimension("row", 7)
    } else {
        spec
    }
}

#[test]
fn runs_cross_rows_chunks_and_bitmap_pages_with_no_cache() -> stratava::Result<()> {
    for crc in policies() {
        for growing in [false, true] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("run");
            let spec = spec(crc, growing);
            let width = 13;
            let count = width * 2600;
            let mut w = spec.create_writer_with_dims(
                &path,
                MatrixDimensions::from_pairs([
                    ("row", if growing { 7 } else { 2600 }),
                    ("col", width),
                ]),
            )?;
            for i in 0..count {
                w.write_matrix_cell(key(i, width), &Cell { value: i })?;
                w.commit_matrix_cell::<Cell>(key(i, width))?;
            }
            // Writer reads include its current open chunk, without publishing it.
            let mut end = vec![Cell { value: 0 }; 20];
            w.read_matrix_run_into(key(count - 20, width), &mut end)?;
            assert_eq!(
                end.iter().map(|x| x.value).collect::<Vec<_>>(),
                (count - 20..count).collect::<Vec<_>>()
            );
            w.sync()?;
            let r = spec.open_reader(&path)?;
            let mut out = vec![Cell { value: 0 }; count as usize - 11];
            r.read_matrix_run_into(key(9, width), &mut out)?;
            for (i, v) in out.iter().enumerate() {
                assert_eq!(v.value, i as u64 + 9);
            }
            let mut bytes = vec![0; out.len() * 8];
            r.read_matrix_run_bytes_into::<Cell>(key(9, width), &mut bytes)?;
            for (i, b) in bytes.chunks_exact(8).enumerate() {
                assert_eq!(u64::from_le_bytes(b.try_into().unwrap()), i as u64 + 9);
            }
            let mut row = vec![Cell { value: 0 }; width as usize];
            r.read_matrix_row_into(8, &mut row)?;
            assert_eq!(row[12].value, 8 * width + 12);
            assert!(r.read_matrix_row_into(8, &mut row[..12]).is_err());
            assert!(
                r.read_matrix_run_bytes_into::<Cell>(key(0, width), &mut [0; 7])
                    .is_err()
            );
            assert!(
                r.read_matrix_run_into(MatrixKey::new(u64::MAX, 0), &mut row)
                    .is_err()
            );
            assert!(
                r.read_matrix_run_into(MatrixKey::new(0, width), &mut row)
                    .is_err()
            );
            if !growing {
                assert!(
                    r.read_matrix_run_into(key(count - 1, width), &mut row)
                        .is_err()
                );
            }
        }
    }
    Ok(())
}

#[test]
fn bulk_reads_keep_old_generation_and_reject_a_missing_middle_cell() -> stratava::Result<()> {
    for growing in [false, true] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("snapshot");
        let spec = spec(IntegrityPolicy::None, growing);
        let mut w = spec.create_writer_with_dims(
            &path,
            MatrixDimensions::from_pairs([("row", if growing { 7 } else { 20 }), ("col", 3)]),
        )?;
        for i in 0..60 {
            w.write_matrix_cell(key(i, 3), &Cell { value: i })?;
            w.commit_matrix_cell::<Cell>(key(i, 3))?;
        }
        w.sync()?;
        let held = spec.open_reader(&path)?;
        let mut follow = spec.open_reader(&path)?;
        for i in 0..60 {
            w.write_matrix_cell(key(i, 3), &Cell { value: 1000 + i })?;
            w.commit_matrix_cell::<Cell>(key(i, 3))?;
        }
        let dirty = spec.open_reader(&path)?;
        let mut out = vec![Cell { value: 0 }; 60];
        dirty.read_matrix_run_into(key(0, 3), &mut out)?;
        assert_eq!(out[59].value, 59);
        w.sync()?;
        follow.follow()?;
        follow.read_matrix_run_into(key(0, 3), &mut out)?;
        assert_eq!(out[59].value, 1059);
        held.read_matrix_run_into(key(0, 3), &mut out)?;
        assert_eq!(out[59].value, 59);
        w.clear_matrix_cell::<Cell>(key(30, 3))?;
        w.sync()?;
        follow.follow()?;
        assert!(matches!(
            follow.read_matrix_run_into(key(0, 3), &mut out),
            Err(Error::MatrixNotCommitted)
        ));
        held.read_matrix_run_into(key(0, 3), &mut out)?;
        assert_eq!(out[30].value, 30);
        if !growing {
            w.compact_matrix()?;
            follow.follow()?;
            assert!(matches!(
                follow.read_matrix_run_into(key(0, 3), &mut out),
                Err(Error::MatrixNotCommitted)
            ));
        }
    }
    Ok(())
}

#[test]
fn declaration_order_selects_contiguous_columns_and_generated_run_methods() -> stratava::Result<()>
{
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("columns");
    let mut w = Columns::create_writer_with_dims(&path, ColumnsDims { col: 3, row: 5 })?;
    for col in 0..3 {
        for row in 0..5 {
            let k = ColumnKey { col, row };
            w.write_column(
                k,
                &Column {
                    value: 100 * col + row,
                },
            )?;
            w.commit_column(k)?;
        }
    }
    w.sync()?;
    let r = Columns::open_reader(&path)?;
    let mut out = vec![Column { value: 0 }; 5];
    r.column_row_into(1, &mut out)?;
    assert_eq!(
        out.iter().map(|x| x.value).collect::<Vec<_>>(),
        [100, 101, 102, 103, 104]
    );
    r.column_run_into(ColumnKey { col: 1, row: 3 }, &mut out)?;
    assert_eq!(
        out.iter().map(|x| x.value).collect::<Vec<_>>(),
        [103, 104, 200, 201, 202]
    );
    let mut bytes = [0; 40];
    r.column_run_bytes_into(ColumnKey { col: 0, row: 0 }, &mut bytes)?;
    assert_eq!(u64::from_le_bytes(bytes[32..40].try_into().unwrap()), 4);
    fn via_trait(r: &impl ColumnsRead, out: &mut [Column]) -> stratava::Result<()> {
        r.column_run_into(ColumnKey { col: 0, row: 0 }, out)
    }
    via_trait(&r, &mut out)?;
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn chunk_run_reads_metadata_per_batch_instead_of_per_cell() -> stratava::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("cost");
    let spec = spec(IntegrityPolicy::None, true);
    let mut w = spec.create_writer_with_dims(
        &path,
        MatrixDimensions::from_pairs([("row", 7), ("col", 1024)]),
    )?;
    for i in 7 * 1024..8 * 1024 {
        w.write_matrix_cell(key(i, 1024), &Cell { value: i })?;
        w.commit_matrix_cell::<Cell>(key(i, 1024))?;
    }
    w.sync()?;
    let r = spec.open_reader(&path)?;
    r.read_matrix_cell::<Cell>(key(7 * 1024, 1024))?; // directory setup is charged separately
    let before = stratava::StratavaFile::chunk_bytes_read();
    let mut out = vec![Cell { value: 0 }; 1024];
    r.read_matrix_run_into(key(7 * 1024, 1024), &mut out)?;
    let bytes = stratava::StratavaFile::chunk_bytes_read() - before;
    assert!(
        bytes <= 8192 + 256,
        "payload plus one descriptor/bitmap batch: {bytes}"
    );
    assert_eq!(out[1023].value, 8191);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn corrupted_chunk_cell_is_not_returned_by_bulk_read() -> stratava::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("corrupt");
    let spec = spec(IntegrityPolicy::Crc32, true);
    let marker = 0x7142_8394_a5b6_c7d8u64;
    let mut w = spec.create_writer_with_dims(
        &path,
        MatrixDimensions::from_pairs([("row", 7), ("col", 3)]),
    )?;
    for i in 21..24 {
        w.write_matrix_cell(key(i, 3), &Cell { value: marker + i })?;
        w.commit_matrix_cell::<Cell>(key(i, 3))?;
    }
    w.sync()?;
    drop(w);
    let bytes = std::fs::read(&path)?;
    let needle = (marker + 22).to_le_bytes();
    let at = bytes.windows(8).position(|x| x == needle).unwrap();
    let mut f = std::fs::OpenOptions::new().write(true).open(&path)?;
    f.seek(SeekFrom::Start(at as u64))?;
    f.write_all(&[0])?;
    drop(f);
    let r = spec.open_reader(&path)?;
    let mut out = vec![Cell { value: 0 }; 3];
    assert!(matches!(
        r.read_matrix_run_into(key(21, 3), &mut out),
        Err(Error::MatrixChecksumMismatch { .. })
    ));
    Ok(())
}

#[test]
fn typed_run_budget_is_checked_before_output_changes() -> stratava::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("budget");
    let base = spec(IntegrityPolicy::None, false);
    let mut w = base.create_writer_with_dims(
        &path,
        MatrixDimensions::from_pairs([("row", 1), ("col", 128)]),
    )?;
    for i in 0..128 {
        w.write_matrix_cell(key(i, 128), &Cell { value: i })?;
        w.commit_matrix_cell::<Cell>(key(i, 128))?;
    }
    w.sync()?;
    let limited = base.with_resource_limits(ReadLimits::STANDARD.with_max_materialized_bytes(1024));
    let r = limited.open_reader(&path)?;
    let mut out = vec![Cell { value: u64::MAX }; 128];
    assert!(r.read_matrix_run_into(key(0, 128), &mut out).is_err());
    assert!(out.iter().all(|x| x.value == u64::MAX));
    // Bytes need only the supplied output; typed runs additionally need decode scratch.
    let mut raw = vec![0; 1024];
    r.read_matrix_run_bytes_into::<Cell>(key(0, 128), &mut raw)?;
    let r = base
        .with_resource_limits(ReadLimits::STANDARD.with_max_materialized_bytes(2048))
        .open_reader(&path)?;
    r.read_matrix_run_into(key(0, 128), &mut out)?;
    assert_eq!(out[127].value, 127);
    Ok(())
}

#[cfg(all(feature = "integrity", feature = "compression-zstd"))]
#[test]
fn compressed_runs_cross_subblocks_and_match_encoded_output() -> stratava::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("compressed");
    let spec = spec(IntegrityPolicy::Crc32, true).with_chunk_compression(
        stratava::VariableCompression::zstd(stratava::CompressionHeaderMode::RecordExplicit),
    );
    let mut w = spec.create_writer_with_dims(
        &path,
        MatrixDimensions::from_pairs([("row", 7), ("col", 1024)]),
    )?;
    for i in 0..15 * 1024 {
        w.write_matrix_cell(key(i, 1024), &Cell { value: i })?;
        w.commit_matrix_cell::<Cell>(key(i, 1024))?;
    }
    w.sync()?;
    let r = spec.open_reader(&path)?;
    let first = 7 * 1024 - 3;
    let mut out = vec![Cell { value: 0 }; 8 * 1024];
    r.read_matrix_run_into(key(first, 1024), &mut out)?;
    for (i, v) in out.iter().enumerate() {
        assert_eq!(v.value, first + i as u64);
    }
    let mut raw = vec![0; out.len() * 8];
    r.read_matrix_run_bytes_into::<Cell>(key(first, 1024), &mut raw)?;
    for (i, b) in raw.chunks_exact(8).enumerate() {
        assert_eq!(u64::from_le_bytes(b.try_into().unwrap()), first + i as u64);
    }
    Ok(())
}
