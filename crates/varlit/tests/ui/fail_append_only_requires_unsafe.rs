fn admit(file: varlit::VarlitFile) -> varlit::Result<varlit::AppendOnlyWriter> {
    varlit::AppendOnlyWriter::new(file)
}
fn main() { let _ = admit; }
