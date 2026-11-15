//! Option 1 experiment: a plain Rust function with the production body and
//! no Verus annotations, checked with the deprecated
//! `--no-external-by-default`. Verus does verify the body, and rejects it:
//! without a `requires` the arithmetic can overflow, and a `requires` can
//! only be written on the function itself.
#![allow(dead_code)]
use vstd::prelude::*;

pub(crate) const fn slot_index(row: u64, col: u64, cols: u64) -> u64 {
    row * cols + col
}

fn main() {}
