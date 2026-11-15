//! Option 1 experiment: attach a spec to the included production function
//! from outside with `assume_specification`. The ensures clause below is
//! deliberately FALSE (slot_index does not always return 0). Verus accepts
//! it, which shows the production body is trusted here, not verified.
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
pub struct MatrixKey {
    pub scan: u64,
    pub ch: u64,
}

#[path = "option1_matrix_tiles.rs"]
mod matrix_tiles;
use matrix_tiles::geometry::BlockGeom;

verus! {

#[verifier::external_type_specification]
pub struct ExBlockGeom(BlockGeom);

pub assume_specification[ BlockGeom::slot_index ](row: u64, col: u64, cols: u64) -> (r: u64)
    ensures
        r == 0,
;

fn caller() {
    let r = BlockGeom::slot_index(3, 4, 5);
    assert(r == 0);  // "proved", but the real function returns 19
}

} // verus!

fn main() {}
