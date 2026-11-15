//! Conversions from file-sized numbers to in-memory sizes.
//!
//! Offsets and lengths in a file are `u64`; a buffer length or an index is
//! `usize`, which is 32 bits on some targets. `value as usize` silently drops
//! the high half there, so the crate does not write it: a runtime value goes
//! through [`index`] and is refused when it does not fit, and a format
//! constant goes through [`small`].

use crate::{Error, Result};

/// `value` as a length or index in memory, or `LengthOverflow` on a target
/// whose address space cannot hold it. On a 64-bit target this always
/// succeeds and compiles to nothing.
#[inline(always)]
pub(crate) fn index(value: u64) -> Result<usize> {
    usize::try_from(value).map_err(|_| Error::LengthOverflow { value })
}

/// A format constant as a length in memory. Every such constant is far below
/// 2^32; one that were not would stop here instead of being cut short.
#[inline(always)]
pub(crate) const fn small(value: u64) -> usize {
    assert!(
        value <= u32::MAX as u64,
        "a format constant must fit 32 bits"
    );
    #[allow(
        clippy::cast_possible_truncation,
        reason = "checked against u32::MAX on the line above"
    )]
    {
        value as usize
    }
}

/// `value` as a length in memory when the caller already holds `bound`, an
/// in-memory size the value does not exceed: the result is `value` exactly.
/// A value above the bound, which the caller has ruled out, would give the
/// bound instead of a number cut short.
#[inline(always)]
pub(crate) fn bounded(value: u64, bound: usize) -> usize {
    match usize::try_from(value) {
        Ok(value) if value <= bound => value,
        _ => bound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_that_fits_is_itself_and_one_that_does_not_is_refused() {
        assert_eq!(index(0).unwrap(), 0);
        assert_eq!(index(4096).unwrap(), 4096);
        assert_eq!(index(u64::from(u32::MAX)).unwrap(), u32::MAX as usize);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(index(u64::MAX).unwrap() as u64, u64::MAX);
        #[cfg(target_pointer_width = "32")]
        for value in [1u64 << 32, u64::MAX] {
            let error = index(value).unwrap_err();
            assert!(matches!(error, Error::LengthOverflow { value: refused } if refused == value));
            std::mem::forget(error);
        }
    }

    #[test]
    fn a_value_under_a_held_bound_is_itself() {
        assert_eq!(bounded(0, 10), 0);
        assert_eq!(bounded(10, 10), 10);
        assert_eq!(bounded(11, 10), 10);
        assert_eq!(bounded(u64::MAX, 7), 7);
        assert_eq!(bounded(5, usize::MAX), 5);
    }

    #[test]
    fn constants_convert_at_compile_time() {
        const LEN: usize = small(4096);
        assert_eq!(LEN, 4096);
        assert_eq!(small(u64::from(u32::MAX)), u32::MAX as usize);
    }

    #[test]
    #[should_panic(expected = "must fit 32 bits")]
    fn a_constant_past_32_bits_stops() {
        let _ = small(std::hint::black_box(1 << 32));
    }
}
