//! HDF5: one native 6-D chunked dataset with an unlimited time axis, extended
//! by one step per write; every step is flushed and fsynced. A time-deep chunk
//! is completed by HDF5 itself (chunk cache read-modify-write). Every
//! selection is one hyperslab read.
//! app-built: nothing
use super::*;
use ::hdf5::File;
use ndarray::{ArrayView6, Ix6, s};

pub struct Wr {
    f: File,
    d: ::hdf5::Dataset,
    p: PathBuf,
}
pub fn create(p: &Path, ck: Ck) -> Result<Box<dyn GridWrite>> {
    let f = File::with_options()
        .with_fapl(|a| a.libver_latest())
        .create(p)?;
    let sh = shape();
    let mut ext = vec![::hdf5::Extent::resizable(0)];
    ext.extend(sh[1..].iter().map(|&n| ::hdf5::Extent::fixed(n)));
    let d = f
        .new_dataset::<f32>()
        .chunk(ck.c.to_vec())
        .shape(::hdf5::SimpleExtents::from_vec(ext))
        .create("grid")?;
    Ok(Box::new(Wr {
        f,
        d,
        p: p.to_path_buf(),
    }))
}
impl GridWrite for Wr {
    fn step(&mut self, t: usize, step: &[f32]) -> Result<()> {
        let sh = shape();
        let a = ArrayView6::from_shape((1, sh[1], sh[2], sh[3], sh[4], sh[5]), step)?;
        let mut dims = sh.to_vec();
        dims[0] = t + 1;
        self.d.resize(dims)?;
        self.d.write_slice(&a, s![t..t + 1, .., .., .., .., ..])?;
        self.f.flush()?;
        fsync_file(&self.p)?;
        Ok(())
    }
    fn finish(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}
pub struct Rd {
    _f: File,
    d: ::hdf5::Dataset,
}
pub fn open(p: &Path, _ck: Ck) -> Result<Box<dyn GridRead>> {
    let f = File::open(p)?;
    let d = f.dataset("grid")?;
    Ok(Box::new(Rd { _f: f, d }))
}
impl GridRead for Rd {
    fn slab(&mut self, a: Ix, n: Ix, out: &mut Vec<f32>) -> Result<()> {
        let sel = s![
            a[0]..a[0] + n[0],
            a[1]..a[1] + n[1],
            a[2]..a[2] + n[2],
            a[3]..a[3] + n[3],
            a[4]..a[4] + n[4],
            a[5]..a[5] + n[5]
        ];
        *out = self
            .d
            .read_slice::<f32, _, Ix6>(sel)?
            .into_raw_vec_and_offset()
            .0;
        Ok(())
    }
}
