//! What closing the gap would look like: the production body left in plain
//! Rust syntax, with the spec attached by attribute on the function itself.
//! Written with `cfg_attr(verus_keep_ghost, ...)` (a cfg Verus sets), so plain
//! rustc ignores the annotations and no dependency is added to normal builds.
#![allow(dead_code)]
#[cfg(verus_keep_ghost)]
use vstd::prelude::*;

#[cfg_attr(verus_keep_ghost, verus_verify)]
#[cfg_attr(verus_keep_ghost, verus_spec(r =>
    requires
        row * cols + col <= u64::MAX,
    ensures
        r == row * cols + col,
))]
pub(crate) const fn slot_index(row: u64, col: u64, cols: u64) -> u64 {
    row * cols + col
}

fn main() {}
