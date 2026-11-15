//! Verus proofs about the tile slot arithmetic of
//! `crates/varlit-core/src/matrix_tiles/geometry.rs`.
//!
//! This file TRANSCRIBES the production arithmetic; it does not include the
//! production file. `run.sh` checks that each transcribed body still appears
//! verbatim in geometry.rs, but that is a text check, not a proof link. See
//! README.md for what this does and does not establish.
//!
//! Run with `./run.sh`, not cargo: this is a single file given to `verus`.
#![allow(dead_code)]
use vstd::prelude::*;

verus! {

// ---------------------------------------------------------------------------
// Trusted assumptions about two std functions Verus/vstd has no spec for.
// They restate the documented behaviour of `u64::div_ceil` and
// `u64::next_multiple_of`; Verus does not check them against std.
// ---------------------------------------------------------------------------
pub open spec fn spec_div_ceil(a: int, b: int) -> int {
    if a % b == 0 {
        a / b
    } else {
        a / b + 1
    }
}

pub open spec fn spec_next_multiple_of(a: int, b: int) -> int {
    if a % b == 0 {
        a
    } else {
        a + (b - a % b)
    }
}

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

// ---------------------------------------------------------------------------
// Constants: format.rs MATRIX_MAX_TILE_CELLS = 1 << 20,
// MATRIX_MAX_TILE_BYTES = 64 << 20; geometry.rs TILE_HEADER_LEN = 40.
// ---------------------------------------------------------------------------
pub const TILE_HEADER_LEN: u64 = 40;

pub const MATRIX_MAX_TILE_CELLS: u64 = 0x10_0000;

pub const MATRIX_MAX_TILE_BYTES: u64 = 0x400_0000;

pub struct MatrixKey {
    pub scan: u64,
    pub ch: u64,
}

pub struct TileCoord {
    pub row: u64,
    pub col: u64,
}

pub struct BlockGeom {
    pub block_id: u32,
    pub stride: u64,
    pub tile: [u64; 2],
}

impl BlockGeom {
    // ---- mathematical model (unbounded integers) --------------------------
    pub open spec fn rows(&self) -> int {
        self.tile[0] as int
    }

    pub open spec fn cols(&self) -> int {
        self.tile[1] as int
    }

    pub open spec fn spec_cells(&self) -> int {
        self.rows() * self.cols()
    }

    pub open spec fn spec_slot_len(&self) -> int {
        self.stride + 4
    }

    pub open spec fn spec_slots_at(&self) -> int {
        spec_next_multiple_of(TILE_HEADER_LEN + spec_div_ceil(self.spec_cells(), 8) + 4, 8)
    }

    pub open spec fn spec_slot_at(&self, slot: int) -> int {
        self.spec_slots_at() + slot * self.spec_slot_len()
    }

    pub open spec fn spec_tile_bytes(&self) -> int {
        self.spec_slots_at() + self.spec_cells() * self.spec_slot_len()
    }

    /// What spec validation admits: the whole range, no smaller bound.
    pub open spec fn admitted(&self) -> bool {
        &&& self.stride >= 1
        &&& self.rows() >= 1
        &&& self.cols() >= 1
        &&& self.rows() * self.cols() <= MATRIX_MAX_TILE_CELLS
        &&& self.rows() * self.cols() * (self.stride + 4) <= MATRIX_MAX_TILE_BYTES
    }

    /// Everything the exec functions below need, derived once from `admitted`.
    pub proof fn lemma_bounds(&self)
        requires
            self.admitted(),
        ensures
            1 <= self.spec_cells() <= MATRIX_MAX_TILE_CELLS,
            self.spec_cells() * self.spec_slot_len() <= MATRIX_MAX_TILE_BYTES,
            self.spec_slot_len() <= MATRIX_MAX_TILE_BYTES,
            48 <= self.spec_slots_at() <= 40 + 131072 + 4 + 8,
            self.spec_tile_bytes() <= 40 + 131072 + 4 + 8 + MATRIX_MAX_TILE_BYTES,
    {
        let cells = self.spec_cells();
        let len = self.spec_slot_len();
        assert(cells >= 1) by (nonlinear_arith)
            requires
                cells == self.rows() * self.cols(),
                self.rows() >= 1,
                self.cols() >= 1,
        ;
        assert(len <= cells * len) by (nonlinear_arith)
            requires
                cells >= 1,
                len >= 0,
        ;
    }

    // ---- transcribed production bodies ------------------------------------
    // geometry.rs: self.tile[0] * self.tile[1]
    pub const fn cells(&self) -> (r: u64)
        requires
            self.admitted(),
        ensures
            r == self.spec_cells(),
    {
        self.tile[0] * self.tile[1]
    }

    // geometry.rs: self.stride + 4
    pub const fn slot_len(&self) -> (r: u64)
        requires
            self.admitted(),
        ensures
            r == self.spec_slot_len(),
    {
        proof {
            self.lemma_bounds();
        }
        self.stride + 4
    }

    // geometry.rs: self.cells().div_ceil(8)
    pub fn bitmap_len(&self) -> (r: u64)
        requires
            self.admitted(),
        ensures
            r == spec_div_ceil(self.spec_cells(), 8),
    {
        self.cells().div_ceil(8)
    }

    // geometry.rs: self.bitmap_len() + 4
    pub fn bitmap_unit_len(&self) -> (r: u64)
        requires
            self.admitted(),
        ensures
            r == spec_div_ceil(self.spec_cells(), 8) + 4,
    {
        proof {
            self.lemma_bounds();
        }
        self.bitmap_len() + 4
    }

    // geometry.rs: (TILE_HEADER_LEN + self.bitmap_unit_len()).next_multiple_of(8)
    pub fn slots_at(&self) -> (r: u64)
        requires
            self.admitted(),
        ensures
            r == self.spec_slots_at(),
    {
        proof {
            self.lemma_bounds();
        }
        (TILE_HEADER_LEN + self.bitmap_unit_len()).next_multiple_of(8)
    }

    // geometry.rs: self.slots_at() + self.cells() * self.slot_len()
    pub fn tile_bytes(&self) -> (r: u64)
        requires
            self.admitted(),
        ensures
            r == self.spec_tile_bytes(),
    {
        proof {
            self.lemma_bounds();
        }
        self.slots_at() + self.cells() * self.slot_len()
    }

    // geometry.rs: self.slots_at() + slot * self.slot_len()
    //
    // Property (c): for slot < cells the slot's bytes end inside the tile, and
    // nothing here overflows u64 (Verus checks every `+` and `*`).
    pub fn slot_at(&self, slot: u64) -> (r: u64)
        requires
            self.admitted(),
            slot < self.spec_cells(),
        ensures
            r == self.spec_slot_at(slot as int),
            r + self.spec_slot_len() <= self.spec_tile_bytes(),
            self.spec_tile_bytes() <= u64::MAX,
    {
        proof {
            self.lemma_bounds();
            self.lemma_slot_within_tile(slot as int);
        }
        self.slots_at() + slot * self.slot_len()
    }

    // geometry.rs: row * cols + col
    //
    // Property (a). `rows` is a ghost argument: the production function takes
    // only (row, col, cols), so the bound on the result needs the tile height
    // from the caller's context.
    pub const fn slot_index(row: u64, col: u64, cols: u64, Ghost(rows): Ghost<int>) -> (r: u64)
        requires
            row < rows,
            col < cols,
            rows * cols <= u64::MAX,
        ensures
            r == row * cols + col,
            r < rows * cols,
    {
        proof {
            lemma_slot_index_in_range(row as int, col as int, rows, cols as int);
        }
        row * cols + col
    }

    // geometry.rs `locate`: the production caller of slot_index.
    pub fn locate(&self, key: MatrixKey) -> (r: (TileCoord, u64))
        requires
            self.admitted(),
        ensures
            r.1 < self.spec_cells(),
            r.1 == (key.scan % self.tile[0]) * self.tile[1] + key.ch % self.tile[1],
    {
        let [rows, cols] = self.tile;
        (
            TileCoord { row: key.scan / rows, col: key.ch / cols },
            Self::slot_index(key.scan % rows, key.ch % cols, cols, Ghost(rows as int)),
        )
    }

    /// (c) in the model: slot < cells ==> slot_at(slot) + slot_len <= tile_bytes.
    pub proof fn lemma_slot_within_tile(&self, slot: int)
        requires
            self.admitted(),
            0 <= slot < self.spec_cells(),
        ensures
            self.spec_slot_at(slot) + self.spec_slot_len() <= self.spec_tile_bytes(),
    {
        let cells = self.spec_cells();
        let len = self.spec_slot_len();
        assert(slot * len + len <= cells * len) by (nonlinear_arith)
            requires
                0 <= slot < cells,
                len >= 0,
        ;
    }
}

/// (a) row < rows && col < cols ==> row * cols + col < rows * cols.
pub proof fn lemma_slot_index_in_range(row: int, col: int, rows: int, cols: int)
    requires
        0 <= row < rows,
        0 <= col < cols,
    ensures
        0 <= row * cols + col < rows * cols,
{
    assert(row * cols + col < rows * cols && row * cols >= 0) by (nonlinear_arith)
        requires
            0 <= row < rows,
            0 <= col < cols,
    ;
}

/// (b) slot_index is injective on [0, rows) x [0, cols).
pub proof fn lemma_slot_index_injective(
    row_a: int,
    col_a: int,
    row_b: int,
    col_b: int,
    rows: int,
    cols: int,
)
    requires
        0 <= row_a < rows,
        0 <= row_b < rows,
        0 <= col_a < cols,
        0 <= col_b < cols,
        row_a * cols + col_a == row_b * cols + col_b,
    ensures
        row_a == row_b,
        col_a == col_b,
{
    if row_a < row_b {
        assert(row_a * cols + col_a < row_b * cols + col_b) by (nonlinear_arith)
            requires
                0 <= row_a < row_b,
                0 <= col_a < cols,
                0 <= col_b,
        ;
    } else if row_b < row_a {
        assert(row_b * cols + col_b < row_a * cols + col_a) by (nonlinear_arith)
            requires
                0 <= row_b < row_a,
                0 <= col_b < cols,
                0 <= col_a,
        ;
    }
}

/// (b) stated on the exec function: two in-range cells with the same slot
/// index are the same cell. Full admitted range: any rows * cols <= u64::MAX.
pub fn slot_index_injective_exec(row_a: u64, col_a: u64, row_b: u64, col_b: u64, rows: u64, cols: u64)
    requires
        row_a < rows,
        row_b < rows,
        col_a < cols,
        col_b < cols,
        rows * cols <= u64::MAX,
{
    let a = BlockGeom::slot_index(row_a, col_a, cols, Ghost(rows as int));
    let b = BlockGeom::slot_index(row_b, col_b, cols, Ghost(rows as int));
    proof {
        if a == b {
            lemma_slot_index_injective(
                row_a as int,
                col_a as int,
                row_b as int,
                col_b as int,
                rows as int,
                cols as int,
            );
        }
    }
    assert(a == b ==> row_a == row_b && col_a == col_b);
}

/// The admitted range is not vacuous: its extreme shapes satisfy `admitted`
/// (the same shapes as matrix_tiles/tests.rs).
pub proof fn lemma_admitted_is_inhabited() {
    let wide = BlockGeom { block_id: 0, stride: 60, tile: [1, MATRIX_MAX_TILE_CELLS] };
    let tall = BlockGeom { block_id: 0, stride: 60, tile: [MATRIX_MAX_TILE_CELLS, 1] };
    // stride = MATRIX_MAX_TILE_BYTES - 4
    let fat = BlockGeom { block_id: 0, stride: 0x3FF_FFFC, tile: [1, 1] };
    assert(wide.admitted());
    assert(tall.admitted());
    assert(fat.admitted());
}

} // verus!

fn main() {}
