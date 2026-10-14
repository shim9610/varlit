use varlit::{
    BlockKind, Decoder, Encoder, Endian, Result, VarlitBlock, VarlitDecode, VarlitEncode,
    VarlitKeyedBlock, WireType,
};

struct ManualKeyed(u64);

impl VarlitEncode for ManualKeyed {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_varlit(encoder)
    }
}

impl VarlitDecode for ManualKeyed {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_varlit(decoder)?))
    }
}

impl VarlitBlock for ManualKeyed {
    const ID: u32 = 1;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x4d61_6e75_616c_4b31;
}

impl VarlitKeyedBlock for ManualKeyed {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.0
    }
}

fn main() {}
