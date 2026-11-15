//! HDF5: one chunked 1-D dataset per column (category/region as codes plus a
//! names dataset), written slice by slice as rows arrive; a ts zone dataset
//! (first ts of every chunk) for Q2 pruning.
//! Candidates: chunk length, blosc-zstd or no filter.
//! app-built: zone map dataset, category dictionary dataset
use super::*;
use ::hdf5::File;
use ndarray::Array1;

pub const CANDIDATES: &[&str] = &[
    "c131072-zstd",
    "c131072-raw",
    "c1048576-zstd",
    "c1048576-raw",
];

pub struct W {
    f: File,
    p: PathBuf,
    ds: Vec<::hdf5::Dataset>,
    at: usize,
    chunk: usize,
    zone: Vec<i64>,
}
const NAMES: [&str; 12] = [
    "ts", "user", "category", "region", "qty", "price", "discount", "flags", "m0", "m1", "m2", "m3",
];
pub fn create(p: &Path, cand: &str) -> Result<Box<dyn Writer>> {
    let chunk: usize = cand
        .trim_start_matches('c')
        .split('-')
        .next()
        .unwrap()
        .parse()?;
    let zstd = cand.ends_with("-zstd");
    let f = File::with_options()
        .with_fapl(|a| a.libver_latest())
        .create(p)?;
    let n = rows();
    let mut ds = Vec::new();
    macro_rules! mk {
        ($t:ty, $name:expr) => {{
            let b = f.new_dataset::<$t>().chunk(chunk.min(n)).shape(n);
            let b = if zstd { b.blosc_zstd(1, true) } else { b };
            ds.push(b.create($name)?);
        }};
    }
    mk!(i64, NAMES[0]);
    mk!(i64, NAMES[1]);
    mk!(u8, NAMES[2]);
    mk!(u8, NAMES[3]);
    mk!(i32, NAMES[4]);
    mk!(f64, NAMES[5]);
    mk!(f32, NAMES[6]);
    mk!(u8, NAMES[7]);
    for k in 0..4 {
        mk!(f64, NAMES[8 + k]);
    }
    Ok(Box::new(W {
        f,
        p: p.to_path_buf(),
        ds,
        at: 0,
        chunk,
        zone: Vec::new(),
    }))
}
impl Writer for W {
    fn append(&mut self, t: &Batch) -> Result<()> {
        let r = self.at..self.at + t.len();
        for (i, &ts) in t.ts.iter().enumerate() {
            if (self.at + i) % self.chunk == 0 {
                self.zone.push(ts);
            }
        }
        self.ds[0].write_slice(&Array1::from(t.ts.clone()), r.clone())?;
        self.ds[1].write_slice(&Array1::from(t.user.clone()), r.clone())?;
        self.ds[2].write_slice(&Array1::from(t.cat.clone()), r.clone())?;
        self.ds[3].write_slice(&Array1::from(t.region.clone()), r.clone())?;
        self.ds[4].write_slice(&Array1::from(t.qty.clone()), r.clone())?;
        self.ds[5].write_slice(&Array1::from(t.price.clone()), r.clone())?;
        self.ds[6].write_slice(&Array1::from(t.discount.clone()), r.clone())?;
        self.ds[7].write_slice(&Array1::from(t.flags.clone()), r.clone())?;
        for k in 0..4 {
            self.ds[8 + k].write_slice(&Array1::from(t.m[k].clone()), r.clone())?;
        }
        self.at = r.end;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.f
            .new_dataset_builder()
            .with_data(&Array1::from(self.zone.clone()))
            .create("ts_zone")?;
        self.f
            .new_dataset_builder()
            .with_data(&Array1::from(vec![self.chunk as u64]))
            .create("chunk_rows")?;
        let names: Vec<::hdf5::types::VarLenUnicode> =
            (0..NCAT).map(|i| cat_name(i).parse().unwrap()).collect();
        self.f
            .new_dataset_builder()
            .with_data(&Array1::from(names))
            .create("category_names")?;
        self.f.flush()?;
        fsync_file(&self.p)?;
        Ok(())
    }
}

pub struct R {
    f: File,
    zone: Vec<i64>,
    chunk: usize,
}
pub fn open(p: &Path, _cand: &str) -> Result<Box<dyn Analytics>> {
    let f = File::open(p)?;
    let zone = f.dataset("ts_zone")?.read_raw::<i64>()?;
    let chunk = f.dataset("chunk_rows")?.read_raw::<u64>()?[0] as usize;
    Ok(Box::new(R { f, zone, chunk }))
}
impl Analytics for R {
    fn q1(&mut self) -> Result<Q1> {
        let names = self
            .f
            .dataset("category_names")?
            .read_raw::<::hdf5::types::VarLenUnicode>()?;
        let map: Vec<usize> = names
            .iter()
            .map(|n| n.as_str()[9..].parse().unwrap())
            .collect();
        let (cd, qd, pd) = (
            self.f.dataset("category")?,
            self.f.dataset("qty")?,
            self.f.dataset("price")?,
        );
        let mut out = [0.0; NCAT];
        let n = rows();
        for a in (0..n).step_by(self.chunk) {
            let r = a..(a + self.chunk).min(n);
            let c = cd.read_slice_1d::<u8, _>(r.clone())?;
            let q = qd.read_slice_1d::<i32, _>(r.clone())?;
            let p = pd.read_slice_1d::<f64, _>(r)?;
            for i in 0..c.len() {
                if q[i] > 50 {
                    out[map[c[i] as usize]] += p[i] * q[i] as f64;
                }
            }
        }
        Ok(out)
    }
    fn q2(&mut self, a: i64, b: i64) -> Result<(u64, f64)> {
        let (td, pd) = (self.f.dataset("ts")?, self.f.dataset("price")?);
        let (mut n, mut s) = (0, 0.0);
        let total = rows();
        for k in 0..self.zone.len() {
            let hi = self.zone.get(k + 1).map(|z| z - 1).unwrap_or(i64::MAX);
            if hi < a || self.zone[k] > b {
                continue;
            }
            let r = k * self.chunk..((k + 1) * self.chunk).min(total);
            let ts = td.read_slice_1d::<i64, _>(r.clone())?;
            let p = pd.read_slice_1d::<f64, _>(r)?;
            for i in 0..ts.len() {
                if ts[i] >= a && ts[i] <= b {
                    n += 1;
                    s += p[i];
                }
            }
        }
        Ok((n, s))
    }
}
