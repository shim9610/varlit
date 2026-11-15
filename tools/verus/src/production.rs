//! Verus driver: includes the production `matrix_tiles/geometry.rs` itself,
//! so the bodies Verus verifies are the bodies the library compiles.
//!
//! The stand-ins below exist only so the included file's imports resolve;
//! nothing Verus verifies depends on what they contain. The two
//! `assume_specification` items are trusted: Verus has no specification for
//! these standard-library functions, so their documented behaviour is
//! restated here and not checked against the standard library.
#![allow(dead_code, unused_imports)]
use vstd::prelude::*;

#[derive(Debug)]
pub enum Error {
    InvalidMatrixLayout,
    AllocationFailed { resource: &'static str, requested: u64 },
    MatrixKeyOutOfBounds { scan: u64, ch: u64 },
}
pub type Result<T> = core::result::Result<T, Error>;

#[derive(Clone, Copy)]
#[cfg_attr(verus_keep_ghost, verus_verify)]
pub struct MatrixKey {
    pub scan: u64,
    pub ch: u64,
}

#[path = "production_matrix_tiles.rs"]
pub(crate) mod matrix_tiles;

use matrix_tiles::geometry::{spec_div_ceil, spec_next_multiple_of};

verus! {

pub assume_specification[ u64::div_ceil ](a: u64, b: u64) -> (r: u64)
    requires
        b != 0,
    ensures
        r == spec_div_ceil(a as int, b as int),
;

pub assume_specification[ u64::next_multiple_of ](a: u64, b: u64) -> (r: u64)
    requires
        b != 0,
        spec_next_multiple_of(a as int, b as int) <= u64::MAX,
    ensures
        r == spec_next_multiple_of(a as int, b as int),
;

} // verus!

fn main() {}
