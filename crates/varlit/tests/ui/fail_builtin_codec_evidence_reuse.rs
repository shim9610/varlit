use varlit::{Decoder, Result, VarlitDecode, WireType, __core::BuiltinCodec};

struct Custom;
impl VarlitDecode for Custom {
    const WIRE_TYPE: WireType = WireType::Unit;
    const __VARLIT_BUILTIN: Option<BuiltinCodec<Self>> = <u64 as VarlitDecode>::__VARLIT_BUILTIN;
    fn decode_varlit(_: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self)
    }
}
fn main() {}
