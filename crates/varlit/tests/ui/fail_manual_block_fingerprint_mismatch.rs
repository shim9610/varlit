use varlit::{BlockKind, Decoder, Encoder, Endian, Result, VarlitBlock, VarlitDecode, VarlitEncode, WireType};

struct ManualNoFingerprint(u64);

impl VarlitEncode for ManualNoFingerprint {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_varlit(encoder)
    }
}

impl VarlitDecode for ManualNoFingerprint {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_varlit(decoder)?))
    }
}

// A manual impl may no longer omit the schema fingerprint: without it a safe
// manual block could impersonate a registered generated type by matching only
// id/version/kind.
impl VarlitBlock for ManualNoFingerprint {
    const ID: u32 = 1;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
}

fn main() {}
