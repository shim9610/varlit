// SHAPE B, mechanical enforcement (round 12).
//
// `MutationPermit` is the witness that a writer's poison flag was observed
// clear. Its only constructor is `PoisonFlag::issue`, which performs the check
// and is private to `crate::writer_permit`. If the witness could be fabricated,
// every guarded mutation in `stream.rs`, `indexed.rs`, `file.rs` and `layout.rs`
// would be back to being protected by a convention — which is the exact
// configuration F-05 was found in. This fixture fails to compile, and must keep
// failing.


use varlit_core::VarlitStreamWriter;
use varlit_core::enforcement_probe::MutationPermit;

fn cannot_forge(writer: &mut VarlitStreamWriter) {
    let _forged = MutationPermit { writer, context: "forged" };
}
fn main() {}
