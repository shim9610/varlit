use std::panic::{AssertUnwindSafe, catch_unwind};
use varlit::{Decoder, Endian, Error, Result, VarlitDecode, WireType};

struct Replace;
impl VarlitDecode for Replace {
    const WIRE_TYPE: WireType = WireType::Nested;
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        *decoder = Decoder::new_limited(&[], Endian::Little, u64::MAX);
        Ok(Self)
    }
}
struct ResetSameState;
impl VarlitDecode for ResetSameState {
    const WIRE_TYPE: WireType = WireType::Nested;
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        let bytes = decoder.read_exact(1)?;
        decoder.charge_materialization(3, "test")?;
        *decoder = Decoder::new_limited(bytes, Endian::Little, 8);
        Ok(Self)
    }
}
struct Fail;
impl VarlitDecode for Fail {
    const WIRE_TYPE: WireType = WireType::Nested;
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        decoder.read_u8()?;
        decoder.charge_materialization(3, "test")?;
        Err(Error::InvalidUtf8)
    }
}
struct Panic;
impl VarlitDecode for Panic {
    const WIRE_TYPE: WireType = WireType::Nested;
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        decoder.read_u8()?;
        decoder.charge_materialization(3, "test")?;
        panic!("callback panic")
    }
}
#[test]
fn replacement_is_rejected_without_inventing_budget() {
    let mut budget = 8;
    assert!(
        Decoder::decode_from_slice_accounted::<Replace>(&[1], Endian::Little, &mut budget).is_err()
    );
    assert_eq!(budget, 0);
    let mut parent = Decoder::new_limited(&[2, 3], Endian::Big, 8);
    assert!(parent.decode_nested::<Replace>(&[1]).is_err());
    assert_eq!(parent.materialization_remaining(), 0);
    assert_eq!(parent.read_u8().unwrap(), 2);
}
#[test]
fn replacing_with_identical_public_settings_cannot_refund_charges() {
    let mut decoder = Decoder::new_limited(&[1], Endian::Little, 8);
    assert!(decoder.decode_value::<ResetSameState>().is_err());
    assert_eq!(decoder.materialization_remaining(), 0);
    assert_eq!(decoder.remaining(), 0);
}
#[test]
fn returning_error_retains_exact_charge_and_cursor() {
    let mut decoder = Decoder::new_limited(&[1, 2], Endian::Little, 8);
    assert!(matches!(
        decoder.decode_value::<Fail>(),
        Err(Error::InvalidUtf8)
    ));
    assert_eq!(decoder.materialization_remaining(), 5);
    assert_eq!(decoder.read_u8().unwrap(), 2);
}
#[test]
fn unwind_leaves_direct_nested_and_accounted_budgets_exhausted() {
    let mut decoder = Decoder::new_limited(&[1, 2], Endian::Little, 8);
    assert!(catch_unwind(AssertUnwindSafe(|| decoder.decode_value::<Panic>())).is_err());
    assert_eq!(decoder.materialization_remaining(), 0);
    assert_eq!(decoder.remaining(), 0);
    let mut parent = Decoder::new_limited(&[7], Endian::Little, 8);
    assert!(catch_unwind(AssertUnwindSafe(|| parent.decode_nested::<Panic>(&[1]))).is_err());
    assert_eq!(parent.materialization_remaining(), 0);
    assert_eq!(parent.read_u8().unwrap(), 7);
    let mut budget = 8;
    assert!(
        catch_unwind(AssertUnwindSafe(|| Decoder::decode_from_slice_accounted::<
            Panic,
        >(
            &[1], Endian::Little, &mut budget
        )))
        .is_err()
    );
    assert_eq!(budget, 0);
}
#[test]
fn library_containers_reject_replacement_before_the_next_element() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    struct Element;
    impl VarlitDecode for Element {
        const WIRE_TYPE: WireType = WireType::Nested;
        fn decode_varlit(d: &mut Decoder<'_>) -> Result<Self> {
            CALLS.fetch_add(1, Ordering::Relaxed);
            *d = Decoder::new_limited(&[], Endian::Little, u64::MAX);
            Ok(Self)
        }
    }
    let mut budget = 32;
    assert!(
        Decoder::decode_from_slice_accounted::<[Element; 2]>(&[1, 2], Endian::Little, &mut budget)
            .is_err()
    );
    assert_eq!(budget, 0);
    assert_eq!(CALLS.load(Ordering::Relaxed), 1);
}
struct ArrayOverride;
impl VarlitDecode for ArrayOverride {
    const WIRE_TYPE: WireType = WireType::Nested;
    fn decode_varlit(_: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self)
    }
    fn decode_array<const N: usize>(decoder: &mut Decoder<'_>) -> Result<[Self; N]> {
        *decoder = Decoder::new_limited(&[], Endian::Little, u64::MAX);
        Ok(std::array::from_fn(|_| Self))
    }
}
#[test]
fn array_override_is_a_checked_callback_too() {
    let mut decoder = Decoder::new_limited(&[], Endian::Little, 8);
    assert!(decoder.decode_value::<[ArrayOverride; 2]>().is_err());
    assert_eq!(decoder.materialization_remaining(), 0);
}

struct CatchInnerPanic;
impl VarlitDecode for CatchInnerPanic {
    const WIRE_TYPE: WireType = WireType::Nested;
    fn decode_varlit(d: &mut Decoder<'_>) -> Result<Self> {
        assert!(catch_unwind(AssertUnwindSafe(|| d.decode_value::<Panic>())).is_err());
        assert_eq!(d.materialization_remaining(), 0);
        assert_eq!(d.remaining(), 0);
        assert!(d.read_u8().is_err());
        assert!(d.charge_materialization(1, "retry").is_err());
        Ok(Self)
    }
}
#[test]
fn catching_an_inner_callback_panic_cannot_recover_spent_authority() {
    let mut d = Decoder::new_limited(&[1, 2], Endian::Little, 8);
    assert!(d.decode_value::<CatchInnerPanic>().is_ok());
    assert_eq!(d.materialization_remaining(), 0);
    assert_eq!(d.remaining(), 0);
}
