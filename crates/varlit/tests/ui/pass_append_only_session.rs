use varlit::{AppendOnlySession, AppendOnlyWriterState, VarlitFile};
unsafe fn lifecycle(file: VarlitFile) -> varlit::Result<()> {
    let session = unsafe { AppendOnlySession::new(file)? };
    let clone = session.clone();
    let token = session.reader_token();
    let reader = token.open_reader()?;
    let empty = reader.mmap_payloads()?;
    session.open_writer()?.close()?;
    clone.open_writer()?.close()?;
    assert_eq!(reader.session_status()?.writer, AppendOnlyWriterState::Closed);
    drop(reader);
    drop(empty);
    clone.recover()?;
    session.close()?;
    Ok(())
}
fn main() { let _ = lifecycle; }
