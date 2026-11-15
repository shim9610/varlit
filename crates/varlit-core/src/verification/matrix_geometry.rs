//! Tile arithmetic for a matrix block whose shape passed spec validation:
//! every cell has exactly one slot, the slot lies inside its tile record,
//! and tile coordinates interleave into distinct directory keys.
use super::{BlockGeom, TILE_HEADER_LEN, TileCoord};
use crate::format::{MATRIX_MAX_TILE_BYTES, MATRIX_MAX_TILE_CELLS};

/// A tile with sides up to 63: bounded so the product of two unknowns
/// stays within what the solver decides quickly. Nothing in the property
/// depends on the scale; the largest admitted shapes are exercised by the
/// native tests.
fn bounded_tile() -> [u64; 2] {
    let rows: u8 = kani::any();
    let cols: u8 = kani::any();
    kani::assume(rows != 0 && cols != 0 && rows < 64 && cols < 64);
    [u64::from(rows), u64::from(cols)]
}

/// A cell inside a tile gets a slot index below the tile's cell count, and
/// its commit bit lies inside the bitmap.
#[kani::proof]
fn a_cell_inside_a_tile_gets_one_of_its_slots() {
    let tile = bounded_tile();
    let row: u8 = kani::any();
    let col: u8 = kani::any();
    kani::assume(u64::from(row) < tile[0] && u64::from(col) < tile[1]);
    let geom = BlockGeom {
        block_id: 0,
        stride: 1,
        tile,
    };
    let slot = BlockGeom::slot_index(u64::from(row), u64::from(col), tile[1]);
    assert!(slot < geom.cells());
    assert!(slot / 8 < geom.bitmap_len());
}

/// Every slot of a block lies after the header and the bitmap unit and ends
/// inside the tile record, for every admitted cell count up to 2^20 at four
/// strides: the smallest, a word, an odd one and one that fills 64 MiB with
/// few cells. The stride is a constant in each case, so the proof is linear
/// in the unknowns. The cell count stands for any tile shape with that many
/// cells.
#[kani::proof]
fn every_slot_lies_inside_its_tile_record() {
    let stride: u64 = match kani::any::<u8>() % 4 {
        0 => 1,
        1 => 8,
        2 => 61,
        _ => 65_532,
    };
    let cells: u32 = kani::any();
    let slot: u32 = kani::any();
    kani::assume(cells != 0 && u64::from(cells) <= MATRIX_MAX_TILE_CELLS && slot < cells);
    kani::assume(u64::from(cells) * (stride + 4) <= MATRIX_MAX_TILE_BYTES);
    let geom = BlockGeom {
        block_id: kani::any(),
        stride,
        tile: [1, u64::from(cells)],
    };
    let tile_bytes = geom.tile_bytes();
    assert!(
        tile_bytes <= TILE_HEADER_LEN + MATRIX_MAX_TILE_CELLS / 8 + 4 + 8 + MATRIX_MAX_TILE_BYTES
    );
    assert!(geom.bitmap_at() == TILE_HEADER_LEN);
    assert!(geom.bitmap_at() + geom.bitmap_unit_len() <= geom.slots_at());
    assert_eq!(geom.slots_at() % 8, 0);
    let at = geom.slot_at(u64::from(slot));
    assert!(at >= geom.slots_at());
    assert!(at + geom.slot_len() <= tile_bytes);
}

/// A directory key holds the row's bits at even positions and the column's
/// at odd ones, so distinct tile coordinates get distinct keys; coordinates
/// that do not fit 32 bits are refused.
#[kani::proof]
fn morton_keys_interleave_row_and_column_bits() {
    let coord = TileCoord {
        row: kani::any(),
        col: kani::any(),
    };
    let fits = coord.row <= u64::from(u32::MAX) && coord.col <= u64::from(u32::MAX);
    match coord.morton() {
        Ok(key) => {
            assert!(fits);
            let bit: u32 = kani::any();
            kani::assume(bit < 32);
            assert_eq!((key >> (2 * bit)) & 1, (coord.row >> bit) & 1);
            assert_eq!((key >> (2 * bit + 1)) & 1, (coord.col >> bit) & 1);
        }
        Err(error) => {
            assert!(!fits);
            // `InvalidMatrixLayout` owns nothing; other variants hold boxed
            // causes whose destruction is not what this harness is about.
            std::mem::forget(error);
        }
    }
}
