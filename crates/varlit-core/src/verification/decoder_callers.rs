//! Actual length decode -> budget charge -> allocation -> complete decode.
//! At most four payload bytes; arbitrary u64 encoded length and budget. The
//! allocator obeys Rust's contract; its one reservation may be refused.
use super::*;
use std::alloc::{GlobalAlloc, Layout, System};

static mut AUTHORIZED: bool = false;
static mut DENIED: bool = false;
static mut ALLOCATIONS: usize = 0;

unsafe fn guarded_allocate(layout: Layout) -> *mut u8 {
    unsafe {
        assert!(
            AUTHORIZED,
            "byte decoder allocated before input/budget admission"
        );
        assert!(
            layout.size() <= 4,
            "byte decoder reserved from untrusted length"
        );
        ALLOCATIONS += 1;
        if DENIED {
            std::ptr::null_mut()
        } else {
            System.alloc(layout)
        }
    }
}

#[kani::proof]
#[kani::unwind(14)]
#[kani::stub(std::alloc::alloc, guarded_allocate)]
#[kani::stub(std::vec::Vec::<u8>::new, crate::fallible::proofs::model_empty_vec)]
fn complete_byte_decode_admits_before_allocating_and_never_refunds() {
    let encoded: u64 = kani::any();
    let limit: u64 = kani::any();
    let available: usize = kani::any();
    let denied: bool = kani::any();
    kani::assume(available <= 4);
    let mut input = [0u8; 12];
    input[..8].copy_from_slice(&encoded.to_le_bytes());
    let payload: [u8; 4] = kani::any();
    input[8..].copy_from_slice(&payload);
    let admitted = encoded <= available as u64 && encoded <= limit;
    unsafe {
        AUTHORIZED = admitted;
        DENIED = denied;
        ALLOCATIONS = 0;
    }
    let mut remaining = limit;
    let result = Decoder::decode_from_slice_accounted::<Vec<u8>>(
        &input[..8 + available],
        Endian::Little,
        &mut remaining,
    );
    assert_eq!(remaining, if admitted { limit - encoded } else { limit });
    assert_eq!(
        unsafe { ALLOCATIONS },
        usize::from(admitted && encoded != 0)
    );
    assert_eq!(
        result.is_ok(),
        encoded == available as u64 && admitted && (!denied || encoded == 0)
    );
    match result {
        Ok(value) => {
            assert_eq!(value.as_slice(), &payload[..available]);
            drop(value);
        }
        Err(error) => std::mem::forget(error),
    }
}

/// A terminating, allocation-free custom codec which uses only Decoder's
/// charging/reading API. Two full-width charges surround a possibly short read.
pub(crate) struct TwoCharges;
impl VarlitDecode for TwoCharges {
    const WIRE_TYPE: WireType = WireType::Nested;
    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        let first = decoder.read_u64()?;
        decoder.charge_materialization(first, "first callback stage")?;
        let second = decoder.read_u64()?;
        decoder.charge_materialization(second, "second callback stage")?;
        if decoder.read_u8()? != 0 {
            return Err(Error::InvalidUtf8);
        }
        Ok(Self)
    }
}

pub(crate) fn two_charge_oracle(before: u64, first: u64, second: u64, len: u8) -> u64 {
    if len < 8 || first > before {
        before
    } else if len < 16 || second > before - first {
        before - first
    } else {
        before - first - second
    }
}

#[kani::proof]
#[kani::unwind(10)]
fn nested_two_stage_contract_preserves_parent_and_committed_charges() {
    let limit: u64 = kani::any();
    let remaining: u64 = kani::any();
    kani::assume(remaining <= limit);
    let first: u64 = kani::any();
    let second: u64 = kani::any();
    let len: u8 = kani::any();
    let start: u8 = kani::any();
    let flag: u8 = kani::any();
    let big: bool = kani::any();
    kani::assume(len <= 18 && start <= 4);
    let mut input = [0u8; 18];
    input[..8].copy_from_slice(&if big {
        first.to_be_bytes()
    } else {
        first.to_le_bytes()
    });
    input[8..16].copy_from_slice(&if big {
        second.to_be_bytes()
    } else {
        second.to_le_bytes()
    });
    input[16] = flag;
    let backing = [1u8, 2, 3, 4];
    let mut parent = Decoder::new_limited(
        &backing,
        if big { Endian::Big } else { Endian::Little },
        limit,
    );
    parent.position = usize::from(start);
    parent.materialization_remaining = remaining;
    let result = parent.decode_nested::<TwoCharges>(&input[..usize::from(len)]);
    assert_eq!(
        parent.position,
        usize::from(start),
        "nested decoder moved parent cursor"
    );
    assert_eq!(parent.materialization_limit, limit);
    assert_eq!(parent.input, &backing);
    assert_eq!(
        parent.input.as_ptr(),
        backing.as_ptr(),
        "nested decoder replaced parent backing"
    );
    assert_eq!(
        parent.materialization_remaining,
        two_charge_oracle(remaining, first, second, len),
        "nested decoder refunded completed charges"
    );
    let success = len == 17 && first <= remaining && second <= remaining - first && flag == 0;
    assert_eq!(result.is_ok(), success);
    std::mem::forget(result);
}

/// Over-approximate a returning callback's scalar effects, including effects
/// unavailable through the safe public API. No particular callback path is
/// enumerated: every numeric post-state and linkage outcome is symbolic.
static OTHER_SCOPE: u8 = 0;
static OTHER_INPUT: [u8; 4] = [1, 2, 3, 4];
static mut EXPECTED_ACCEPTED: bool = false;
static mut EXPECTED_OK: bool = false;
static mut EXPECTED_POSITION: usize = 0;
static mut EXPECTED_REMAINING: u64 = 0;
struct ScalarEffects;
impl VarlitDecode for ScalarEffects {
    const WIRE_TYPE: WireType = WireType::Nested;
    fn decode_varlit(d: &mut Decoder<'_>) -> Result<Self> {
        let scope = d.callback_scope.unwrap();
        let input = d.input;
        let endian = d.endian;
        let limit = d.materialization_limit;
        let before = d.materialization_remaining;
        let start = d.position;
        d.position = kani::any();
        d.materialization_remaining = kani::any();
        d.materialization_limit = kani::any();
        if kani::any() {
            d.callback_scope = None;
        } else if kani::any() {
            d.callback_scope = Some(&OTHER_SCOPE);
        }
        if kani::any() {
            d.input = &[];
        } else if kani::any() {
            d.input = &OTHER_INPUT;
        }
        if kani::any() {
            d.endian = Endian::Big;
        }
        let ok: bool = kani::any();
        let accepted = d.callback_scope.is_some_and(|p| std::ptr::eq(p, scope))
            && std::ptr::eq(d.input, input)
            && d.endian == endian
            && d.materialization_limit == limit
            && d.position as u128 >= start as u128
            && d.position as u128 <= input.len() as u128
            && d.materialization_remaining as u128 <= before as u128;
        unsafe {
            EXPECTED_ACCEPTED = accepted;
            EXPECTED_OK = accepted && ok;
            EXPECTED_POSITION = d.position;
            EXPECTED_REMAINING = d.materialization_remaining;
        }
        if ok {
            Ok(Self)
        } else {
            Err(Error::InvalidUtf8)
        }
    }
}
#[kani::proof]
#[kani::unwind(6)]
fn arbitrary_returning_callback_preserves_the_owned_numeric_invariant() {
    let limit: u64 = kani::any();
    let remaining: u64 = kani::any();
    let start: usize = kani::any();
    kani::assume(remaining <= limit && start <= 4);
    let input = [1u8, 2, 3, 4];
    let mut d = Decoder::new_limited(&input, Endian::Little, limit);
    d.position = start;
    d.materialization_remaining = remaining;
    let result = d.decode_value::<ScalarEffects>();
    assert_eq!(result.is_ok(), unsafe { EXPECTED_OK });
    assert_eq!(
        d.position,
        if unsafe { EXPECTED_ACCEPTED } {
            unsafe { EXPECTED_POSITION }
        } else {
            input.len()
        }
    );
    assert_eq!(
        d.materialization_remaining,
        if unsafe { EXPECTED_ACCEPTED } {
            unsafe { EXPECTED_REMAINING }
        } else {
            0
        }
    );
    assert_eq!(d.materialization_limit, limit);
    assert!(
        d.materialization_remaining <= remaining,
        "callback expanded the owned budget"
    );
    assert!(
        d.position >= start && d.position <= input.len(),
        "callback escaped the owned input"
    );
    assert_eq!(d.input.as_ptr(), input.as_ptr());
    assert_eq!(d.input.len(), input.len());
    assert_eq!(d.endian, Endian::Little);
    std::mem::forget(result);
}
struct ReplaceFrame;
impl VarlitDecode for ReplaceFrame {
    const WIRE_TYPE: WireType = WireType::Nested;
    fn decode_varlit(d: &mut Decoder<'_>) -> Result<Self> {
        *d = Decoder::new_limited(&[], Endian::Little, kani::any());
        Ok(Self)
    }
}
#[kani::proof]
fn safe_whole_value_replacement_cannot_escape_nested_or_accounted_decode() {
    let budget: u64 = kani::any();
    let mut remaining = budget;
    let result =
        Decoder::decode_from_slice_accounted::<ReplaceFrame>(&[], Endian::Little, &mut remaining);
    assert!(result.is_err(), "safe replacement was accepted");
    assert_eq!(remaining, 0);
    std::mem::forget(result);
    let mut parent = Decoder::new_limited(&[], Endian::Little, budget);
    let result = parent.decode_nested::<ReplaceFrame>(&[]);
    assert!(result.is_err());
    assert_eq!(parent.materialization_remaining(), 0);
    std::mem::forget(result);
}

/// An arbitrary legal primitive transition is the inductive step for callbacks
/// confined to the public API. Whole replacement is a separate operation and
/// must be rejected even when every public setting is identical.
struct SafeTransition;
impl VarlitDecode for SafeTransition {
    const WIRE_TYPE: WireType = WireType::Nested;
    fn decode_varlit(d: &mut Decoder<'_>) -> Result<Self> {
        match kani::any::<u8>() % 3 {
            0 => {
                d.read_exact(kani::any())?;
            }
            1 => {
                d.charge_materialization(kani::any(), "inductive callback")?;
            }
            _ => {
                *d = Decoder::new_limited(&[], Endian::Little, kani::any());
            }
        }
        Ok(Self)
    }
}
#[kani::proof]
#[kani::unwind(6)]
fn active_callback_step_preserves_invariant_or_rejects_replacement() {
    let limit: u64 = kani::any();
    let remaining: u64 = kani::any();
    let start: usize = kani::any();
    kani::assume(remaining <= limit && start <= 4);
    let input = [1u8, 2, 3, 4];
    let marker = 0u8;
    let mut d = Decoder::new_limited(&input, Endian::Little, limit);
    d.callback_scope = Some(&marker);
    d.position = start;
    d.materialization_remaining = remaining;
    let result = d.decode_value::<SafeTransition>();
    assert_eq!(d.materialization_limit, limit);
    assert!(d.materialization_remaining <= remaining);
    assert!(d.position >= start && d.position <= input.len());
    assert_eq!(d.input.as_ptr(), input.as_ptr());
    assert_eq!(d.input.len(), input.len());
    assert_eq!(d.endian, Endian::Little);
    assert!(d.callback_scope.is_some_and(|p| std::ptr::eq(p, &marker)));
    std::mem::forget(result);
}
