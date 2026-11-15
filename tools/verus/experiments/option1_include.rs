//! Option 1 experiment: `#[path]`-include the production geometry.rs.
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

fn main() {}
