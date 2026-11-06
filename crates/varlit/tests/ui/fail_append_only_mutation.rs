fn escape(writer: &mut varlit::AppendOnlyWriter) {
    writer.file_mut();
    writer.defragment();
}
fn consume(writer: varlit::AppendOnlyWriter) { writer.into_inner(); }
fn main() { let _ = (escape, consume); }
