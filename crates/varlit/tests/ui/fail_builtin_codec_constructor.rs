use varlit::__core::BuiltinCodec;

fn main() {
    let _evidence = BuiltinCodec::<()> {
        invariant: std::marker::PhantomData,
        callback_free: true,
    };
}
