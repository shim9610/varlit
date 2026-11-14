//! HDF5 (compound + SWMR)
//! Each record type is an HDF5 compound type with fixed-size array members, in
//! its own extendible chunked dataset, plus a per-window t0 dataset.
//! app-built: per-window t0 dataset as completion marker; fixed-length message strings (SWMR)
use super::*;
use hdf5::{File, OpenMode, types::FixedAscii};
use ndarray::{Array1, Array2, s};

// SWMR does not support variable-length types (global heap), so the
// message is a fixed-length string, as HDF5 SWMR applications must do.
#[derive(Clone, hdf5::H5Type)]
#[repr(C)]
struct H5C {
    t: u64,
    code: u32,
    msg: FixedAscii<64>,
}

pub struct W {
    f: File,
    a: hdf5::Dataset,
    b: hdf5::Dataset,
    c: hdf5::Dataset,
    t0: hdf5::Dataset,
    p: PathBuf,
    n: usize,
}
pub fn create(p: &Path) -> Result<Box<dyn AcqWriter>> {
    let f = File::with_options()
        .with_fapl(|a| a.libver_latest())
        .create(p)?;
    let a = f.new_dataset::<RecA>().chunk(NA).shape(0..).create("A")?;
    let b = f
        .new_dataset::<RecB>()
        .chunk(NB * 10)
        .shape(0..)
        .create("B")?;
    let c = f
        .new_dataset::<H5C>()
        .chunk(NC * 100)
        .shape(0..)
        .create("C")?;
    let t0 = f
        .new_dataset::<u64>()
        .chunk((256, 3))
        .shape((0.., 3))
        .create("t0")?;
    f.start_swmr()?;
    Ok(Box::new(W {
        f,
        a,
        b,
        c,
        t0,
        p: p.to_path_buf(),
        n: 0,
    }))
}
impl AcqWriter for W {
    fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
        let (a, b, c) = (&a, &b, &c);
        let w = self.n;
        self.a.resize((w + 1) * NA)?;
        self.a
            .write_slice(&Array1::from(a.recs.clone()), s![w * NA..(w + 1) * NA])?;
        self.b.resize((w + 1) * NB)?;
        self.b
            .write_slice(&Array1::from(b.recs.clone()), s![w * NB..(w + 1) * NB])?;
        let cr: Vec<H5C> = c
            .recs
            .iter()
            .map(|r| H5C {
                t: r.t,
                code: r.code,
                msg: FixedAscii::from_ascii(r.msg.as_bytes()).unwrap(),
            })
            .collect();
        self.c.resize((w + 1) * NC)?;
        self.c
            .write_slice(&Array1::from(cr), s![w * NC..(w + 1) * NC])?;
        // t0 last: a window is complete when its t0 row is visible.
        self.t0.resize((w + 1, 3))?;
        self.t0.write_slice(
            &Array2::from_shape_vec((1, 3), vec![a.t0, b.t0, c.t0])?,
            s![w..w + 1, ..],
        )?;
        self.n += 1;
        Ok(())
    }
    fn publish(&mut self, durable: bool) -> Result<()> {
        // SWMR readers see what H5Fflush wrote; fsync adds durability.
        self.f.flush()?;
        if durable {
            fsync_file(&self.p)?;
        }
        Ok(())
    }
}
pub struct R {
    _f: File,
    a: hdf5::Dataset,
    b: hdf5::Dataset,
    c: hdf5::Dataset,
    t0: hdf5::Dataset,
    want: [bool; 3],
}
pub fn open(p: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
    let f = File::open_as(p, OpenMode::ReadSWMR)?;
    Ok(Box::new(R {
        a: f.dataset("A")?,
        b: f.dataset("B")?,
        c: f.dataset("C")?,
        t0: f.dataset("t0")?,
        _f: f,
        want,
    }))
}
impl AcqReader for R {
    fn poll(&mut self, s: &mut Sink) -> Result<bool> {
        // SWMR refreshes each dataset separately; each type takes the
        // windows visible in both its own dataset and t0.
        self.t0.refresh()?;
        let tw = self.t0.shape()[0] as u64;
        let mut got = false;
        for i in 0..3 {
            if !self.want[i] {
                continue;
            }
            let (ds, per) = match i {
                0 => (&self.a, NA),
                1 => (&self.b, NB),
                _ => (&self.c, NC),
            };
            ds.refresh()?;
            let vis = (ds.shape()[0] / per) as u64;
            let upto = vis.min(tw);
            let from = s.seen[i];
            if upto <= from {
                continue;
            }
            let t0: Array2<u64> = self.t0.read_slice(s![from as usize..upto as usize, ..])?;
            let (a, b) = (from as usize * per, upto as usize * per);
            match i {
                0 => {
                    let v: Array1<RecA> = ds.read_slice_1d(a..b)?;
                    for (k, w) in (from..upto).enumerate() {
                        s.a(
                            w,
                            t0[[k, 0]],
                            v.slice(s![k * per..(k + 1) * per])
                                .iter()
                                .copied()
                                .collect::<Vec<_>>()
                                .into_iter(),
                        );
                    }
                }
                1 => {
                    let v: Array1<RecB> = ds.read_slice_1d(a..b)?;
                    for (k, w) in (from..upto).enumerate() {
                        s.b(
                            w,
                            t0[[k, 1]],
                            v.slice(s![k * per..(k + 1) * per])
                                .iter()
                                .copied()
                                .collect::<Vec<_>>()
                                .into_iter(),
                        );
                    }
                }
                _ => {
                    let v: Array1<H5C> = ds.read_slice_1d(a..b)?;
                    for (k, w) in (from..upto).enumerate() {
                        let recs: Vec<RecC> = v
                            .slice(s![k * per..(k + 1) * per])
                            .iter()
                            .map(|r| RecC {
                                t: r.t,
                                code: r.code,
                                msg: r.msg.as_str().to_string(),
                            })
                            .collect();
                        s.c(w, t0[[k, 2]], recs.into_iter());
                    }
                }
            }
            got = true;
        }
        Ok(got)
    }
}
