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
