use varlit::{MatrixDimensions, MatrixKey, varlit_format};
varlit_format! {
    pub format Storage {
        magic: b"MATCAP01"; version: 1; endian: little;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks { matrix Cell(id = 1, dims = [row, col], category = data) { value: u64, } }
    }
}

#[test]
fn observations_survive_reopen_without_changing_reads() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("matrix");
    let spec = Storage::spec();
    let mut writer = spec.create_writer_with_dims(
        &path,
        MatrixDimensions::from_pairs([("row", 4096), ("col", 4)]),
    )?;
    let caps = writer
        .matrix_storage_diagnostics()
        .expect("matrix observations");
    assert_eq!(
        caps.hole_punch_api_available,
        cfg!(any(windows, target_os = "linux", target_os = "macos"))
    );
    let at = MatrixKey::new(7, 2);
    writer.write_matrix_cell(at, &Cell { value: 91 })?;
    writer.commit_matrix_cell::<Cell>(at)?;
    writer.sync()?;
    let reader = spec.open_reader(&path)?;
    let reopened = reader.matrix_storage_diagnostics().unwrap();
    assert_eq!(
        reopened.hole_punch_api_available,
        caps.hole_punch_api_available
    );
    for _ in 0..32 {
        assert_eq!(reader.matrix_storage_diagnostics(), Some(reopened));
        assert_eq!(reader.read_matrix_cell::<Cell>(at)?.value, 91);
    }
    let report = varlit::diagnose_file(spec, &path);
    assert!(
        !report
            .items
            .iter()
            .any(|d| d.code == "matrix.storage.sparse_mark_failed"),
        "a create-time request result cannot be invented by a later open"
    );
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn missing_allocation_map_warns_without_rejecting_the_file() -> varlit::Result<()> {
    use varlit::MatrixRecoveryReport;
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            MatrixRecoveryReport::force_matrix_allocation_map_unavailable(false);
        }
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("unmapped");
    let spec = Storage::spec();
    let at = MatrixKey::new(7, 2);
    let mut writer = spec.create_writer_with_dims(
        &path,
        MatrixDimensions::from_pairs([("row", 4096), ("col", 4)]),
    )?;
    writer.write_matrix_cell(at, &Cell { value: 91 })?;
    writer.commit_matrix_cell::<Cell>(at)?;
    writer.sync()?;
    MatrixRecoveryReport::force_matrix_allocation_map_unavailable(true);
    let _restore = Restore;
    let reader = spec.open_reader(&path)?;
    assert!(
        !reader
            .matrix_storage_diagnostics()
            .unwrap()
            .allocation_map_available
    );
    assert_eq!(reader.read_matrix_cell::<Cell>(at)?.value, 91);
    let report = varlit::diagnose_file(spec, &path);
    let warning = report
        .items
        .iter()
        .find(|d| d.code == "matrix.storage.allocation_map_unavailable")
        .unwrap();
    assert_eq!(warning.domain, varlit::DiagnosticDomain::Environment);
    assert_eq!(warning.severity, varlit::DiagnosticSeverity::Warning);
    Ok(())
}
