//! API-04: `PackedBitmap` is a documented public codec, so an ordinary derived
//! variable field must compile.
//!
//! `#[derive(VarlitBlock)]` rejects every field codec whose encode or decode
//! identity resolves to zero. `PackedBitmap` inherited that zero default when
//! the identity contract was introduced, which silently made this documented
//! public surface uncompilable; it now declares a stable non-zero identity.

use varlit::{PackedBitmap, VarlitBlock};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 140, kind = "variable", key = "id")]
struct BitmapRecord {
    #[varlit(field_id = 1)]
    id: u64,
    #[varlit(field_id = 2)]
    mask: PackedBitmap,
}

fn main() {
    let mut mask = PackedBitmap::new(24).expect("packed bitmap");
    mask.set(3, true).expect("set bit");
    let record = BitmapRecord { id: 7, mask };
    let bytes = varlit::encode_to_vec(&record, varlit::Endian::Little).expect("encode");
    let decoded =
        varlit::decode_from_slice::<BitmapRecord>(&bytes, varlit::Endian::Little).expect("decode");
    assert_eq!(decoded, record);
    assert!(decoded.mask.get(3).expect("get bit"));
}
