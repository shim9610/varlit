//! Controlled mapped access after one explicit external-ownership admission.
use varlit::{
    AppendOnlySession, AppendOnlyWriterState, CommitPolicy, Endian, Result, TransactionMarkerMode,
    decode_from_slice, varlit_format,
};

varlit_format! {
    format Log {
        magic: b"LEASEDEMO"; version: 1; index: block_offset_chain;
        blocks { variable Samples(id = 1) { data: Vec<u8>, } }
    }
}

fn main() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("samples");
    let spec = Log::spec().with_commit_policy(CommitPolicy::TransactionMarker(
        TransactionMarkerMode::Explicit,
    ));
    let file = spec.create(&path)?;
    // SAFETY: this example owns the private backing object. All modifications
    // use this append-only facade; no other handles/processes rewrite or
    // truncate it while any token, reader, mapping or borrowed slice exists.
    let session = unsafe { AppendOnlySession::new(file)? };
    let token = session.reader_token();
    let mut reader = token.open_reader()?;
    assert_eq!(
        reader.session_status()?.writer,
        AppendOnlyWriterState::NotStarted
    );
    let mut writer = session.open_writer()?;
    writer.push_info(&Samples {
        data: vec![1; 64 << 10],
    })?;
    writer.commit_durable()?;
    reader.follow()?;
    let mut entries = Vec::new();
    reader.block_entries_into::<Samples>(&mut entries)?;
    let mapped = reader.mmap_payload_window(&entries[0])?;
    let captured = mapped.as_bytes();

    writer.close()?;
    // Cached native state reconnects while reader and captured bytes remain.
    let mut writer = session.open_writer()?;
    writer.push_info(&Samples {
        data: vec![2; 64 << 10],
    })?;
    writer.flush()?;
    assert_eq!(reader.follow()?, 0);
    writer.commit_durable()?;
    assert!(reader.follow()? > 0);
    reader.block_entries_into::<Samples>(&mut entries)?;
    assert_eq!(entries.len(), 2);
    assert_eq!(
        decode_from_slice::<Samples>(captured, Endian::Little)?.data,
        vec![1; 64 << 10]
    );

    writer.close()?;
    session.close()?;
    assert_eq!(
        token.session_status()?.writer,
        AppendOnlyWriterState::Finished
    );
    drop(session);
    drop(token);
    drop(reader);
    // An owned window remains protected after its originating handles drop.
    assert!(spec.open(&path).is_err());
    assert_eq!(
        decode_from_slice::<Samples>(mapped.as_bytes(), Endian::Little)?
            .data
            .len(),
        64 << 10
    );
    drop(mapped);
    let _editable = spec.open(&path)?;
    Ok(())
}
