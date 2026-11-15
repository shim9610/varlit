//! The codec's unsafe code, all of it: sizing a vector from zeroed allocator
//! memory, and viewing vectors of primitive numbers and booleans as bytes.
//!
//! Every function here is safe to call with any arguments. The rest of the
//! codec is compiled under `deny(unsafe_code)` and reaches raw memory only
//! through these.
#![allow(unsafe_code)]

use super::{allocation_request, check_bool_bytes};
use crate::{Error, Result};

/// A primitive number: its all-zero bit pattern is a value, every other bit
/// pattern is too, and it has no padding. Freshly zeroed memory from the
/// allocator is therefore initialized storage for it, and its storage may be
/// written as bytes.
///
/// # Safety
///
/// Implemented only for primitive integers and floats.
pub(super) unsafe trait ZeroInit: Copy + 'static {}

// SAFETY (each line): a primitive number; see the trait.
unsafe impl ZeroInit for u8 {}
unsafe impl ZeroInit for i8 {}
unsafe impl ZeroInit for u16 {}
unsafe impl ZeroInit for i16 {}
unsafe impl ZeroInit for u32 {}
unsafe impl ZeroInit for i32 {}
unsafe impl ZeroInit for u64 {}
unsafe impl ZeroInit for i64 {}
unsafe impl ZeroInit for u128 {}
unsafe impl ZeroInit for i128 {}
unsafe impl ZeroInit for f32 {}
unsafe impl ZeroInit for f64 {}

/// Sizes `out` to `len` for a fill that overwrites every element.
///
/// A vector with room is reused as it is: nothing is reallocated or cleared.
/// Otherwise its storage is replaced by zeroed memory straight from the
/// allocator, which for a large request is fresh pages rather than a pass
/// writing zeros.
pub(super) fn size_for_fill<T: ZeroInit>(
    out: &mut Vec<T>,
    len: usize,
    resource: &'static str,
) -> Result<()> {
    if out.capacity() >= len {
        if out.len() >= len {
            out.truncate(len);
        } else {
            // SAFETY: zero is a `T`, so the spare capacity up to `len` can be
            // zeroed in place and counted as initialized.
            unsafe {
                let spare = out.as_mut_ptr().add(out.len());
                std::ptr::write_bytes(spare, 0, len - out.len());
                out.set_len(len);
            }
        }
        return Ok(());
    }
    let refused = || Error::AllocationFailed {
        resource,
        requested: allocation_request::<T>(len),
    };
    // The old contents are about to be overwritten, so its block is freed
    // before the new one is taken: the two are never live together, and
    // growing a value peaks at the new size instead of old + new. A request
    // that is refused, by the allocator or because no layout has that size,
    // leaves `out` empty, as a failed fill does.
    *out = Vec::new();
    let layout = std::alloc::Layout::array::<T>(len).map_err(|_| refused())?;
    // SAFETY: `len > capacity >= 0` and `T` is not zero-sized, so the layout
    // has a nonzero size. The block has `T`'s array layout for `len`
    // elements, every one a zero `T`, and comes from the global allocator
    // that `Vec` frees it with.
    *out = unsafe {
        let block = std::alloc::alloc_zeroed(layout).cast::<T>();
        if block.is_null() {
            return Err(refused());
        }
        Vec::from_raw_parts(block, len, len)
    };
    Ok(())
}

/// The storage of `values`, as bytes that may be overwritten freely.
pub(super) fn bytes_of_mut<T: ZeroInit>(values: &mut [T]) -> &mut [u8] {
    // SAFETY: `values` is `size_of_val(values)` initialized, writable bytes;
    // `T` is a primitive number without padding for which every bit pattern
    // is a value, so any bytes written leave valid `T`s; `u8` has alignment
    // 1; and the borrow of `values` lasts as long as the returned slice.
    unsafe { std::slice::from_raw_parts_mut(values.as_mut_ptr().cast::<u8>(), size_of_val(values)) }
}

/// The allocation of a boolean vector, as the bytes it holds.
pub(super) fn bools_into_bytes(values: Vec<bool>) -> Vec<u8> {
    let mut values = std::mem::ManuallyDrop::new(values);
    // SAFETY: `bool` and `u8` have the same size and alignment, a `bool` is
    // a valid `u8`, and `values` gives up its allocation here without
    // dropping it.
    unsafe {
        Vec::from_raw_parts(
            values.as_mut_ptr().cast::<u8>(),
            values.len(),
            values.capacity(),
        )
    }
}

/// The allocation of a byte vector as booleans, once every byte is 0 or 1.
/// Bytes that are not are refused and the allocation is freed.
pub(super) fn bytes_into_bools(bytes: Vec<u8>) -> Result<Vec<bool>> {
    check_bool_bytes(&bytes)?;
    let mut bytes = std::mem::ManuallyDrop::new(bytes);
    // SAFETY: same size and alignment as above, the allocation changes hands
    // without being dropped, and every byte was just checked to be 0 or 1,
    // the two values a `bool` has.
    Ok(unsafe {
        Vec::from_raw_parts(
            bytes.as_mut_ptr().cast::<bool>(),
            bytes.len(),
            bytes.capacity(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizing_reuses_room_zeroes_new_elements_and_reallocates_exactly() {
        // Room and length: truncated, storage kept.
        let mut values: Vec<u32> = vec![7; 8];
        let before = values.as_ptr();
        size_for_fill(&mut values, 3, "test").unwrap();
        assert_eq!(
            (values.as_slice(), values.as_ptr()),
            (&[7, 7, 7][..], before)
        );
        // Room but not length: the new elements are zero, the old ones kept.
        size_for_fill(&mut values, 6, "test").unwrap();
        assert_eq!(
            (values.as_slice(), values.as_ptr()),
            (&[7, 7, 7, 0, 0, 0][..], before)
        );
        // No room: fresh zeroed storage of exactly the length.
        size_for_fill(&mut values, 100, "test").unwrap();
        assert_eq!(values.len(), 100);
        assert!(values.capacity() >= 100);
        assert!(values.iter().all(|value| *value == 0));
        // From nothing, to nothing, and every primitive width.
        let mut empty: Vec<u128> = Vec::new();
        size_for_fill(&mut empty, 0, "test").unwrap();
        assert!(empty.is_empty());
        size_for_fill(&mut empty, 5, "test").unwrap();
        assert_eq!(empty, [0u128; 5]);
        let mut floats: Vec<f64> = Vec::new();
        size_for_fill(&mut floats, 4, "test").unwrap();
        assert_eq!(floats, [0.0; 4]);
        let mut bytes: Vec<u8> = Vec::with_capacity(2);
        size_for_fill(&mut bytes, 9, "test").unwrap();
        assert_eq!(bytes, [0u8; 9]);
    }

    #[test]
    fn a_request_no_allocation_can_hold_is_refused_and_leaves_nothing() {
        let mut values: Vec<u64> = vec![1, 2, 3];
        let error = size_for_fill(&mut values, usize::MAX / 4, "test").unwrap_err();
        assert!(matches!(
            error,
            Error::AllocationFailed {
                resource: "test",
                ..
            }
        ));
        std::mem::forget(error);
        assert!(values.is_empty());
    }

    #[test]
    fn number_storage_is_written_as_bytes_at_every_width() {
        let mut values = [0u16; 3];
        bytes_of_mut(&mut values).copy_from_slice(&[1, 0, 2, 0, 0xFF, 0xFF]);
        assert_eq!(
            values,
            [
                u16::from_ne_bytes([1, 0]),
                u16::from_ne_bytes([2, 0]),
                u16::MAX
            ]
        );
        let mut wide = [0u128; 2];
        assert_eq!(bytes_of_mut(&mut wide).len(), 32);
        bytes_of_mut(&mut wide).fill(0xAB);
        assert_eq!(wide, [u128::from_ne_bytes([0xAB; 16]); 2]);
        let mut floats = [0f32; 1];
        bytes_of_mut(&mut floats).copy_from_slice(&1.5f32.to_ne_bytes());
        assert_eq!(floats, [1.5]);
        let mut none: [i64; 0] = [];
        assert!(bytes_of_mut(&mut none).is_empty());
    }

    #[test]
    fn boolean_vectors_change_hands_with_their_allocation() {
        let mut values = Vec::with_capacity(16);
        values.extend([true, false, true]);
        let at = values.as_ptr() as usize;
        let bytes = bools_into_bytes(values);
        assert_eq!((bytes.as_slice(), bytes.capacity()), (&[1, 0, 1][..], 16));
        assert_eq!(bytes.as_ptr() as usize, at);
        let back = bytes_into_bools(bytes).unwrap();
        assert_eq!(
            (back.as_slice(), back.capacity()),
            (&[true, false, true][..], 16)
        );
        assert_eq!(back.as_ptr() as usize, at);
        assert!(bytes_into_bools(Vec::new()).unwrap().is_empty());
        assert!(bools_into_bytes(Vec::new()).is_empty());
    }

    #[test]
    fn a_byte_that_is_not_a_boolean_never_becomes_one() {
        for bad in [2u8, 0x80, 0xFF] {
            let error = bytes_into_bools(vec![0, 1, bad, 1]).unwrap_err();
            assert!(matches!(error, Error::InvalidCanonicalEncoding(_)));
            std::mem::forget(error);
        }
    }
}
