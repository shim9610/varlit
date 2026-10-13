use stratava::{MatrixCellStatus, MatrixMetadataVerification, ReadLimits, stratava_format};

stratava_format! {
    pub format Multiple {
        magic: b"MDIRECT1"; version: 1; endian: little;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [left, right]; };
        blocks {
            matrix Left(id = 1, dims = [row, col], category = left) { value: u64, }
            matrix Right(id = 2, dims = [row, col], category = right) { value: u64, }
        }
    }
}

#[test]
fn independent_matrix_directories_survive_zero_cache_follow_and_compaction() -> stratava::Result<()>
{
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("multiple.stratava");
    let spec = Multiple::spec().with_resource_limits(
        ReadLimits::STANDARD
            .with_matrix_directory_cache_bytes(0)
            .with_matrix_metadata_verification(MatrixMetadataVerification::OnDemand),
    );
    let mut writer = spec.create_writer_with_dims(
        &path,
        stratava::MatrixDimensions::from_pairs([("row", 128 * 32768), ("col", 1)]),
    )?;
    let key = |page| stratava::MatrixKey::new(page * 32768, 0);
    for page in 0..128 {
        writer.write_matrix_cell(key(page), &Left { value: page })?;
        writer.commit_matrix_cell::<Left>(key(page))?;
        writer.write_matrix_cell(key(127 - page), &Right { value: page + 1000 })?;
        writer.commit_matrix_cell::<Right>(key(127 - page))?;
    }
    writer.sync()?;
    let held = spec.open_reader(&path)?;
    let mut reader = spec.open_reader(&path)?;
    for page in (0..128).step_by(2) {
        writer.clear_matrix_cell::<Left>(key(page))?;
    }
    writer.sync()?;
    writer.compact_matrix()?;
    reader.follow()?;
    for page in 0..128 {
        assert_eq!(held.read_matrix_cell::<Left>(key(page))?.value, page);
        if page % 2 == 0 {
            assert_eq!(
                reader.matrix_cell_status::<Left>(key(page))?,
                MatrixCellStatus::NotCommitted
            );
        } else {
            assert_eq!(reader.read_matrix_cell::<Left>(key(page))?.value, page);
        }
        assert_eq!(
            reader.read_matrix_cell::<Right>(key(page))?.value,
            1127 - page
        );
    }
    Ok(())
}
