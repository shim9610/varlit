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
