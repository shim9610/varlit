use varlit::{AppendOnlyReader, AppendOnlyReaderToken};
fn token_escalation(token: &AppendOnlyReaderToken) { let _ = token.session(); }
fn reader_escalation(reader: &AppendOnlyReader) { let _ = reader.session(); }
fn main() {}
