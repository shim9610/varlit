//! HDF5: one 1-D dataset per column (name as (n, 24) u8), written in chunks;
//! the branch secondary index is CSR in two datasets (ids sorted by branch,
//! offsets). A transaction reads and writes balances with point selections,
//! then flush + fsync (HDF5 has no transactions: a crash can leave a partial
//! update).
//! Candidates: chunk length.
//! app-built: CSR branch index datasets (csr.rs); no atomic transactions
use super::*;
use ::hdf5::{File, Selection};
use ndarray::{Array1, Array2, s};

pub const CANDIDATES: &[&str] = &["c4096", "c65536"];
const STEP: usize = 1 << 20;

fn points(ids: impl Iterator<Item = u64>) -> Selection {
    let v: Vec<usize> = ids.map(|i| i as usize).collect();
    let n = v.len();
    Selection::Points(Array2::from_shape_vec((n, 1), v).unwrap())
}

pub struct W {
    f: File,
    ch: usize,
    p: PathBuf,
    bal: Option<::hdf5::Dataset>,
}
pub fn create(p: &Path, cand: &str) -> Result<Box<dyn Oltp>> {
    let ch = cand.trim_start_matches('c').parse().unwrap_or(4096);
    let f = File::with_options()
        .with_fapl(|a| a.libver_latest())
        .create(p)?;
    Ok(Box::new(W {
        f,
        ch,
        p: p.to_path_buf(),
        bal: None,
    }))
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        let n = accounts() as usize;
        let ch = self.ch;
        let bal = self
            .f
            .new_dataset::<i64>()
            .chunk(ch)
            .shape(n)
            .create("balance")?;
        let br = self
            .f
            .new_dataset::<u32>()
            .chunk(ch)
            .shape(n)
            .create("branch")?;
        let name = self
            .f
            .new_dataset::<u8>()
            .chunk((ch, 24))
            .shape((n, 24))
            .create("name")?;
        let cr = self
            .f
            .new_dataset::<u64>()
            .chunk(ch)
            .shape(n)
            .create("created")?;
        for a in (0..n).step_by(STEP) {
            let z = (a + STEP).min(n);
            let ids = a as u64..z as u64;
            bal.write_slice(&Array1::from_elem(z - a, START), a..z)?;
            br.write_slice(&Array1::from_iter(ids.clone().map(branch_of)), a..z)?;
            let mut nm = Array2::<u8>::zeros((z - a, 24));
            for (i, id) in ids.clone().enumerate() {
                nm.row_mut(i)
                    .as_slice_mut()
                    .unwrap()
                    .copy_from_slice(&name_of(id));
            }
            name.write_slice(&nm, s![a..z, ..])?;
            cr.write_slice(&Array1::from_iter(ids.map(created_of)), a..z)?;
        }
        let bids = self
            .f
            .new_dataset::<u64>()
            .chunk(ch)
            .shape(n)
            .create("br_ids")?;
        let mut off = Vec::with_capacity(branches() as usize + 1);
        off.push(0u64);
        let mut buf: Vec<u64> = Vec::with_capacity(STEP);
        let mut at = 0usize;
        csr::by_branch(&std::env::temp_dir().join("csr"), |_, list| {
            buf.extend_from_slice(list);
            off.push(off.last().unwrap() + list.len() as u64);
            if buf.len() >= STEP {
                let len = buf.len();
                bids.write_slice(&Array1::from(std::mem::take(&mut buf)), at..at + len)?;
                at += len;
            }
            Ok(())
        })?;
        if !buf.is_empty() {
            let len = buf.len();
            bids.write_slice(&Array1::from(buf), at..at + len)?;
        }
        let ol = off.len();
        self.f
            .new_dataset_builder()
            .chunk(ch.min(ol))
            .with_data(&Array1::from(off))
            .create("br_off")?;
        self.f.flush()?;
        fsync_file(&self.p)?;
        self.bal = Some(bal);
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        let bal = self.bal.as_ref().unwrap();
        let sel = points(deltas.iter().map(|d| d.0));
        let mut cur: Array1<i64> = bal.read_slice_1d(sel.clone())?;
        for (c, d) in cur.iter_mut().zip(deltas) {
            *c += d.1;
        }
        bal.write_slice(&cur, sel)?;
        self.f.flush()?;
        fsync_file(&self.p)?;
        Ok(())
    }
}

pub struct R {
    _f: File,
    bal: ::hdf5::Dataset,
    br: ::hdf5::Dataset,
    name: ::hdf5::Dataset,
    cr: ::hdf5::Dataset,
    ids: ::hdf5::Dataset,
    off: ::hdf5::Dataset,
}
pub fn open(p: &Path, _cand: &str) -> Result<Box<dyn OltpRead>> {
    let f = File::open(p)?;
    Ok(Box::new(R {
        bal: f.dataset("balance")?,
        br: f.dataset("branch")?,
        name: f.dataset("name")?,
        cr: f.dataset("created")?,
        ids: f.dataset("br_ids")?,
        off: f.dataset("br_off")?,
        _f: f,
    }))
}
impl OltpRead for R {
    fn point(&mut self, id: u64) -> Result<Row> {
        let i = id as usize;
        let nm: Array2<u8> = self.name.read_slice(s![i..i + 1, ..])?;
        Ok(Row {
            id,
            balance: self.bal.read_slice_1d::<i64, _>(i..i + 1)?[0],
            branch: self.br.read_slice_1d::<u32, _>(i..i + 1)?[0],
            name: nm.as_slice().unwrap().try_into().unwrap(),
            created: self.cr.read_slice_1d::<u64, _>(i..i + 1)?[0],
        })
    }
    fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
        let o: Array1<u64> = self.off.read_slice_1d(b as usize..b as usize + 2)?;
        let (a, z) = (o[0] as usize, o[1] as usize);
        if a == z {
            return Ok((0, 0));
        }
        let ids: Array1<u64> = self.ids.read_slice_1d(a..z)?;
        let bal: Array1<i64> = self.bal.read_slice_1d(points(ids.iter().copied()))?;
        Ok((bal.sum(), (z - a) as u64))
    }
}
