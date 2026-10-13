use stratava::{BlockKind, Decoder, Encoder, Endian, Result, StratavaBlock, StratavaDecode, StratavaEncode, WireType};

struct ManualNoFingerprint(u64);

impl StratavaEncode for ManualNoFingerprint {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_stratava(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_stratava(encoder)
    }
}

impl StratavaDecode for ManualNoFingerprint {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_stratava(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_stratava(decoder)?))
    }
}

// A manual impl may no longer omit the schema fingerprint: without it a safe
// manual block could impersonate a registered generated type by matching only
// id/version/kind.
impl StratavaBlock for ManualNoFingerprint {
    const ID: u32 = 1;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
}

fn main() {}
