use varlit::{Decoder, Encoder, Result, VarlitBlock, VarlitDecode, VarlitEncode, WireType};

// Wrapping an identity-less codec in a built-in container must not launder it
// into an identity: the container fold propagates the missing SCHEMA_ID
// outward, so the derive still rejects the field (API-04).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Packed(u64);

impl VarlitEncode for Packed {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_varlit(encoder)
    }
}

impl VarlitDecode for Packed {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_varlit(decoder)?))
    }
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "fixed")]
struct Outer {
    packed: [Packed; 2],
}

fn main() {}
