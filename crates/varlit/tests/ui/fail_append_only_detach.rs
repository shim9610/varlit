fn detach(window: varlit::GuardedMmapWindow) -> varlit::MmapPayloadWindow {
    window.mapped
}
fn token(token: varlit::AppendOnlyReaderToken) { let _ = token.protection; }
fn main() { let _ = (detach, token); }
