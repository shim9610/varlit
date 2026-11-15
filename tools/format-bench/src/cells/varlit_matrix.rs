//! Varlit matrix: one matrix cell per grid cell, (cells / 1024) x 1024 keyspace.
//! Updates write cells in place (copy-on-write generation pages); `sync()`
//! publishes a generation atomically. The matrix has no publication without a
//! storage barrier, so the loss bound cannot be relaxed.
//! app-built: nothing
use super::*;
use varlit::{MatrixKey, VarlitFile, VarlitReader, varlit_format};

varlit_format! {
    pub format CellGrid {
        magic: b"CELLGRID";
        version: 1;
        limits {
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            matrix_dimension: 16_777_216;
            matrix_cells: 16_777_216;
            matrix_bitmap: 268_435_456;
            matrix_crc: 1_073_741_824;
            matrix_metadata: 1_073_741_824;
            matrix_slot_region: 34_359_738_368;
            sidecar: 1_073_741_824;
        }
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        dims { r: u32, c: u32, }
        commit: cell_bitmap { keyspace = [r, c]; categories = [data]; };
        blocks {
            matrix Cell(id = 1, dims = [r, c], category = data) { v: [u8; 1024], }
        }
    }
}
const COLS: usize = 1024;
fn key(id: u32) -> MatrixKey {
    MatrixKey::new((id as usize / COLS) as u64, (id as usize % COLS) as u64)
}

pub struct W(VarlitFile);
pub fn create(p: &Path) -> Result<Box<dyn CellWriter>> {
    let dims =
        varlit::MatrixDimensions::from_pairs([("r", (cells() / COLS) as u64), ("c", COLS as u64)]);
    Ok(Box::new(W(CellGrid::spec().create_with_dims(p, dims)?)))
}
impl CellWriter for W {
    fn write(&mut self, id: u32, cell: &[u8]) -> Result<()> {
        self.0.write_matrix_cell_payload::<Cell>(key(id), cell)?;
        self.0.commit_matrix_cell::<Cell>(key(id))?;
        Ok(())
    }
    fn publish(&mut self, _durable: bool) -> Result<()> {
        self.0.sync()?;
        Ok(())
    }
}

pub struct R(VarlitReader);
pub fn open(p: &Path) -> Result<Box<dyn CellReader>> {
    Ok(Box::new(R(vl_spec(CellGrid::spec()).open_reader(p)?)))
}
impl CellReader for R {
    fn refresh(&mut self) -> Result<()> {
        self.0.follow()?;
        Ok(())
    }
    fn read(&mut self, id: u32, out: &mut [u8]) -> Result<()> {
        self.0.read_matrix_run_bytes_into::<Cell>(key(id), out)?;
        Ok(())
    }
}
