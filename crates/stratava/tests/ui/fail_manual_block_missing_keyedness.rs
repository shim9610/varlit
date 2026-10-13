use stratava::{
    BlockKind, Decoder, Encoder, Endian, Result, StratavaBlock, StratavaDecode, StratavaEncode,
    StratavaKeyedBlock, WireType,
};

struct ManualKeyed(u64);

impl StratavaEncode for ManualKeyed {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_stratava(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_stratava(encoder)
    }
}

impl StratavaDecode for ManualKeyed {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_stratava(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_stratava(decoder)?))
    }
}

impl StratavaBlock for ManualKeyed {
    const ID: u32 = 1;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x4d61_6e75_616c_4b31;
}

impl StratavaKeyedBlock for ManualKeyed {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.0
    }
}

fn main() {}
