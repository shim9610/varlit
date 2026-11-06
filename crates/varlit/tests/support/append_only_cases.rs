use varlit::{
    AppendOnlyReader, AppendOnlyWriter, CommitPolicy, FormatSpec, ReadValidation, Result,
    TransactionMarkerMode, VarlitFile, varlit_format,
};

varlit_format! {
    pub format SessionFormat {
        magic: b"MMSESSION"; version: 1; index: block_offset_chain;
        blocks { fixed Value(id = 81) { value: u64, } }
    }
}

pub fn spec() -> FormatSpec {
    SessionFormat::spec()
        .with_integrity_policy(if cfg!(feature = "integrity") {
            varlit::IntegrityPolicy::Crc32
        } else {
            varlit::IntegrityPolicy::None
        })
        .with_commit_policy(CommitPolicy::TransactionMarker(
            TransactionMarkerMode::Explicit,
        ))
}

pub fn default_validation() -> ReadValidation {
    if cfg!(feature = "integrity") {
        ReadValidation::FULL
    } else {
        ReadValidation::STRUCTURE_ONLY
    }
}

pub fn writer(
    directory: &tempfile::TempDir,
    validation: ReadValidation,
) -> Result<AppendOnlyWriter> {
    let file = spec()
        .with_read_limits(varlit::ReadLimits::STANDARD.with_read_validation(validation))
        .create(directory.path().join("session"))?;
    // SAFETY: the test owns these private files. Only this facade modifies the
    // backing object; path-switch tests keep the original object unchanged.
    unsafe { AppendOnlyWriter::new(file) }
}

pub fn entries(reader: &AppendOnlyReader) -> Result<Vec<varlit::RecordIndexEntry>> {
    let mut out = Vec::new();
    reader.block_entries_into::<Value>(&mut out)?;
    Ok(out)
}

pub fn populated() -> Result<(tempfile::TempDir, AppendOnlyWriter, AppendOnlyReader)> {
    let directory = tempfile::tempdir()?;
    let mut writer = writer(&directory, default_validation())?;
    writer.push_info(&Value { value: 19 })?;
    writer.commit_durable()?;
    let reader = writer.reader_token().open_reader()?;
    Ok((directory, writer, reader))
}

pub fn assert_locked(path: &std::path::Path) {
    assert!(matches!(
        VarlitFile::open(spec(), path),
        Err(varlit::Error::WriterLockHeld(_))
    ));
    assert!(matches!(
        VarlitFile::create(spec(), path),
        Err(varlit::Error::WriterLockHeld(_))
    ));
}

pub fn limited_writer(records: u64) -> Result<(tempfile::TempDir, AppendOnlyWriter)> {
    let directory = tempfile::tempdir()?;
    let spec = spec()
        .with_commit_policy(CommitPolicy::TransactionMarker(
            TransactionMarkerMode::OnFlush,
        ))
        .with_read_limits(varlit::ReadLimits::STANDARD.with_max_records(records));
    let file = spec.create(directory.path().join("session"))?;
    // SAFETY: exclusively owned private test backing object.
    Ok((directory, unsafe { AppendOnlyWriter::new(file)? }))
}
