// Compile-fail enforcement.
//
// `RecordOverwrite` is the permission to rewrite the bytes of an
// already-indexed record, and the only route to `RecordFile`'s in-place write.
// Its only constructor, `RecordOverwrite::prepare`, consumes a version-checked
// `ReplacementTarget` and drops the target block's resident keyed-tail map
// before the permission exists. Fabricating one would write record bytes with
// a stale keyed-tail cache still resident and without the
// version refusal.

use varlit_core::enforcement_probe::RecordOverwrite;

fn main() {
    let _forged = RecordOverwrite {
        record_offset: 0,
        payload_offset: 0,
    };
}
