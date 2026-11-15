//! Varlit indexed: one keyed record per cell version (`key = [id]`, disk key
//! index sidecar). An update appends the new versions of its cells; `sync()`
//! makes them durable and publishes the new index root atomically, so readers
//! that `follow()` see all or none of an update. Lookups go through the disk
//! index; nothing per cell is held in memory. The indexed writer has no
//! publication without a storage barrier, so the loss bound cannot be relaxed.
//! app-built: nothing
use super::*;
use varlit::{BatchOptions, DiskIndexOptions, varlit_format};

varlit_format! {
    pub format CellKv {
        magic: b"CELLKV01";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        index: keyed_offset_chain;
        blocks {
            fixed Cell(id = 1, key = [id], key_index = disk) { id: u32, v: [u8; 1024], }
        }
    }
}

fn opts() -> DiskIndexOptions {
    vl_dio(DiskIndexOptions::default())
}

pub struct W {
    w: CellKvIndexedWriter,
    pending: Vec<Cell>,
}
pub fn create(p: &Path) -> Result<Box<dyn CellWriter>> {
    Ok(Box::new(W {
        w: CellKv::create_indexed_writer(p, opts())?,
        pending: Vec::with_capacity(BATCH + 1),
    }))
}
fn cell(id: u32, data: &[u8]) -> Cell {
    Cell {
        id,
        v: data.try_into().unwrap(),
    }
}
impl CellWriter for W {
    fn load(&mut self, first: u32, data: &[u8]) -> Result<()> {
        let it = data
            .chunks_exact(CELL)
            .enumerate()
            .map(|(i, c)| cell(first + i as u32, c));
        self.w
            .push_cells(it, BatchOptions::default())
            .map_err(|f| f.source)?;
        Ok(())
    }
    fn loaded(&mut self) -> Result<()> {
        self.w.sync()?;
        self.w.compact_index()?;
        Ok(())
    }
    fn write(&mut self, id: u32, data: &[u8]) -> Result<()> {
        self.pending.push(cell(id, data));
        Ok(())
    }
    fn publish(&mut self, _durable: bool) -> Result<()> {
        self.w
            .push_cells(self.pending.drain(..), BatchOptions::default())
            .map_err(|f| f.source)?;
        self.w.sync()?;
        Ok(())
    }
}

pub struct R {
    r: CellKvIndexedReader,
    out: Cell,
}
pub fn open(p: &Path) -> Result<Box<dyn CellReader>> {
    Ok(Box::new(R {
        r: CellKv::open_indexed_reader(p, opts())?,
        out: Cell {
            id: 0,
            v: [0; CELL],
        },
    }))
}
impl CellReader for R {
    fn refresh(&mut self) -> Result<()> {
        self.r.follow()?;
        Ok(())
    }
    fn read(&mut self, id: u32, out: &mut [u8]) -> Result<()> {
        if !self.r.get_cell(&id, &mut self.out)? {
            return Err("cell missing".into());
        }
        out.copy_from_slice(&self.out.v);
        Ok(())
    }
}
