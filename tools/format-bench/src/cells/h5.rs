//! HDF5 SWMR: a (cells, 1024) u8 dataset; cells rewritten in place, `H5Fflush`
//! publishes, fsync adds durability. SWMR has no multi-write atomicity, so a
//! reader can see part of an update (counted as torn).
//! Candidates: cells per chunk.
//! app-built: nothing (atomic updates are not available)
use super::*;
use ::hdf5::{File, OpenMode};
use ndarray::{ArrayView2, s};

pub const CANDIDATES: &[&str] = &["c1", "c16", "c64"];

pub struct W {
    f: File,
    d: ::hdf5::Dataset,
    p: PathBuf,
}
pub fn create(p: &Path, cand: &str) -> Result<Box<dyn CellWriter>> {
    let rows: usize = cand.trim_start_matches('c').parse().unwrap_or(16);
    let f = File::with_options()
        .with_fapl(|a| a.libver_latest())
        .create(p)?;
    let d = f
        .new_dataset::<u8>()
        .chunk((rows, CELL))
        .shape((cells(), CELL))
        .create("cells")?;
    Ok(Box::new(W {
        f,
        d,
        p: p.to_path_buf(),
    }))
}
impl CellWriter for W {
    fn load(&mut self, first: u32, data: &[u8]) -> Result<()> {
        let n = data.len() / CELL;
        let a = ArrayView2::from_shape((n, CELL), data)?;
        self.d
            .write_slice(&a, s![first as usize..first as usize + n, ..])?;
        Ok(())
    }
    fn loaded(&mut self) -> Result<()> {
        self.f.flush()?;
        fsync_file(&self.p)?;
        self.f.start_swmr()?;
        Ok(())
    }
    fn write(&mut self, id: u32, cell: &[u8]) -> Result<()> {
        let a = ArrayView2::from_shape((1, CELL), cell)?;
        self.d
            .write_slice(&a, s![id as usize..id as usize + 1, ..])?;
        Ok(())
    }
    fn publish(&mut self, durable: bool) -> Result<()> {
        self.f.flush()?;
        if durable {
            fsync_file(&self.p)?;
        }
        Ok(())
    }
}

pub struct R {
    _f: File,
    d: ::hdf5::Dataset,
}
pub fn open(p: &Path, _cand: &str) -> Result<Box<dyn CellReader>> {
    let f = File::open_as(p, OpenMode::ReadSWMR)?;
    let d = f.dataset("cells")?;
    Ok(Box::new(R { _f: f, d }))
}
impl CellReader for R {
    fn refresh(&mut self) -> Result<()> {
        self.d.refresh()?;
        Ok(())
    }
    fn read(&mut self, id: u32, out: &mut [u8]) -> Result<()> {
        let v = self.d.read_slice_1d::<u8, _>(s![id as usize, ..])?;
        out.copy_from_slice(v.as_slice().ok_or("layout")?);
        Ok(())
    }
}
