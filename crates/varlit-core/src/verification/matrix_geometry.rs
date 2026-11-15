//! Tile arithmetic for a matrix block whose shape passed spec validation:
//! every cell has exactly one slot, the slot lies inside its tile record,
//! and tile coordinates interleave into distinct directory keys.
use super::{BlockGeom, TILE_HEADER_LEN, TileCoord};

/// A tile with sides up to 255: bounded so the products stay within what the
/// solver decides. Nothing in the properties depends on the scale; the
/// largest admitted shapes are exercised by the native tests.
fn bounded_tile() -> [u64; 2] {
    let rows: u8 = kani::any();
    let cols: u8 = kani::any();
    kani::assume(rows != 0 && cols != 0);
    [u64::from(rows), u64::from(cols)]
}

/// A row and a column inside a tile, as `locate` reduces a key to them.
fn cell_in(tile: [u64; 2]) -> (u64, u64) {
    let row: u8 = kani::any();
    let col: u8 = kani::any();
    kani::assume(u64::from(row) < tile[0] && u64::from(col) < tile[1]);
    (u64::from(row), u64::from(col))
}

/// A cell inside a tile gets a slot index below the tile's cell count, and
/// its commit bit lies inside the bitmap.
#[kani::proof]
fn a_cell_inside_a_tile_gets_one_of_its_slots() {
    let tile = bounded_tile();
    let (row, col) = cell_in(tile);
    let geom = BlockGeom {
        block_id: 0,
        stride: 1,
        tile,
    };
    let slot = BlockGeom::slot_index(row, col, tile[1]);
    assert!(slot < geom.cells());
    assert!(slot / 8 < geom.bitmap_len());
}

/// Every slot of a block lies after the header and the bitmap unit and ends
/// inside the tile record, for up to 65535 cells of up to 255 bytes. The
/// cell count stands for any tile shape with that many cells.
#[kani::proof]
fn every_slot_lies_inside_its_tile_record() {
    let cells: u16 = kani::any();
    let stride: u8 = kani::any();
    let slot: u16 = kani::any();
    kani::assume(cells != 0 && stride != 0 && slot < cells);
    let geom = BlockGeom {
        block_id: kani::any(),
        stride: u64::from(stride),
        tile: [1, u64::from(cells)],
    };
    let tile_bytes = geom.tile_bytes();
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
