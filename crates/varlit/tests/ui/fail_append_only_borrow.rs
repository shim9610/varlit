fn escape(window: varlit::GuardedMmapWindow) -> &'static [u8] {
    window.as_bytes()
}
fn main() { let _ = escape; }
