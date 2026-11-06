use varlit::{AppendOnlyReaderToken, GuardedMmapWindow, RecordIndexEntry};
fn protected(token: &AppendOnlyReaderToken, entry: &RecordIndexEntry) -> varlit::Result<GuardedMmapWindow> {
    // The reader may drop: the returned owned mapping retains protection.
    token.open_reader()?.mmap_payload_window(entry)
}
fn main() { let _ = protected; }
