//! Parquet
//! One schema per file: per publication one file per type (FixedSizeList
//! columns keep exact element types), then an empty marker renamed into place
//! last so a publication becomes visible for all types at once.
//! app-built: file per type per publication + renamed marker for atomic visibility; reader directory scan
use super::*;
use arrow_array::builder::{
    BooleanBuilder, FixedSizeListBuilder, Float32Builder, Int8Builder, UInt16Builder,
};
use arrow_array::{
    Array, ArrayRef, FixedSizeListArray, RecordBatch, StringArray, UInt16Array, UInt32Array,
    UInt64Array,
};
use parquet::arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder};
use parquet::file::properties::WriterProperties;
use std::sync::Arc;

pub struct W {
    dir: PathBuf,
    pend: [Vec<RecordBatch>; 3],
    n: u64,
    /// Files and markers published since the last durable publication.
    unsynced: Vec<PathBuf>,
}
pub fn create(dir: &Path) -> Result<Box<dyn AcqWriter>> {
    std::fs::create_dir_all(dir)?;
    Ok(Box::new(W {
        dir: dir.to_path_buf(),
        pend: Default::default(),
        n: 0,
        unsynced: Vec::new(),
    }))
}
impl W {
    fn sync_unsynced(&mut self) -> Result<()> {
        for p in self.unsynced.drain(..) {
            fsync_file(&p)?;
        }
        fsync_dir(&self.dir)
    }
}
fn fsl<B: arrow_array::builder::ArrayBuilder>(b: B, n: i32) -> FixedSizeListBuilder<B> {
    FixedSizeListBuilder::new(b, n)
}
fn batch_a(b: &BA) -> Result<RecordBatch> {
    let mut a = fsl(Int8Builder::new(), 10);
    let mut f = fsl(BooleanBuilder::new(), 3);
    let mut x = fsl(UInt16Builder::new(), 500);
    for r in &b.recs {
        a.values().append_slice(&r.a);
        a.append(true);
        for v in r.f {
            f.values().append_value(v);
        }
        f.append(true);
        x.values().append_slice(&r.x);
        x.append(true);
    }
    let n = b.recs.len();
    Ok(RecordBatch::try_from_iter([
        ("w", Arc::new(UInt64Array::from(vec![b.w; n])) as ArrayRef),
        ("t0", Arc::new(UInt64Array::from(vec![b.t0; n]))),
        (
            "t",
            Arc::new(UInt64Array::from_iter_values(b.recs.iter().map(|r| r.t))),
        ),
        ("a", Arc::new(a.finish())),
        ("f", Arc::new(f.finish())),
        ("x", Arc::new(x.finish())),
    ])?)
}
fn batch_b(b: &BB) -> Result<RecordBatch> {
    let mut x = fsl(Float32Builder::new(), 32);
    for r in &b.recs {
        x.values().append_slice(&r.x);
        x.append(true);
    }
    let n = b.recs.len();
    Ok(RecordBatch::try_from_iter([
        ("w", Arc::new(UInt64Array::from(vec![b.w; n])) as ArrayRef),
        ("t0", Arc::new(UInt64Array::from(vec![b.t0; n]))),
        (
            "t",
            Arc::new(UInt64Array::from_iter_values(b.recs.iter().map(|r| r.t))),
        ),
        ("x", Arc::new(x.finish())),
        (
            "status",
            Arc::new(UInt16Array::from_iter_values(
                b.recs.iter().map(|r| r.status),
            )),
        ),
    ])?)
}
fn batch_c(b: &BC) -> Result<RecordBatch> {
    let n = b.recs.len();
    Ok(RecordBatch::try_from_iter([
        ("w", Arc::new(UInt64Array::from(vec![b.w; n])) as ArrayRef),
        ("t0", Arc::new(UInt64Array::from(vec![b.t0; n]))),
        (
            "t",
            Arc::new(UInt64Array::from_iter_values(b.recs.iter().map(|r| r.t))),
        ),
        (
            "code",
            Arc::new(UInt32Array::from_iter_values(b.recs.iter().map(|r| r.code))),
        ),
        (
            "msg",
            Arc::new(StringArray::from_iter_values(
                b.recs.iter().map(|r| r.msg.as_str()),
            )),
        ),
    ])?)
}
impl AcqWriter for W {
    fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
        let (a, b, c) = (&a, &b, &c);
        self.pend[0].push(batch_a(a)?);
        self.pend[1].push(batch_b(b)?);
        self.pend[2].push(batch_c(c)?);
        Ok(())
    }
    fn publish(&mut self, durable: bool) -> Result<()> {
        if self.pend[0].is_empty() {
            if durable {
                self.sync_unsynced()?;
            }
            return Ok(());
        }
        let props = WriterProperties::builder()
            .set_compression(parquet::basic::Compression::UNCOMPRESSED)
            .set_dictionary_enabled(false)
            .build();
        for (i, t) in ["a", "b", "c"].into_iter().enumerate() {
            let batches = std::mem::take(&mut self.pend[i]);
            let p = self.dir.join(format!("{t}_{:06}.parquet", self.n));
            let mut w = ArrowWriter::try_new(
                std::fs::File::create(&p)?,
                batches[0].schema(),
                Some(props.clone()),
            )?;
            for b in &batches {
                w.write(b)?;
            }
            w.into_inner()?;
            self.unsynced.push(p);
        }
        // A file is immutable once its footer is written; the marker
        // makes this publication's files visible together. Durability:
        // sync the data files before the marker that names them.
        if durable {
            self.sync_unsynced()?;
        }
        let tmp = self.dir.join("tmp.done");
        let marker = std::fs::File::create(&tmp)?;
        let done = self.dir.join(format!("{:06}.done", self.n));
        if durable {
            marker.sync_all()?;
        }
        std::fs::rename(&tmp, &done)?;
        if durable {
            fsync_dir(&self.dir)?;
        } else {
            self.unsynced.push(done);
        }
        self.n += 1;
        Ok(())
    }
}
pub struct R {
    dir: PathBuf,
    want: [bool; 3],
    n: u64,
}
pub fn open(dir: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
    Ok(Box::new(R {
        dir: dir.to_path_buf(),
        want,
        n: 0,
    }))
}
fn col<'a, T: 'static>(b: &'a RecordBatch, n: &str) -> &'a T {
    b.column_by_name(n)
        .unwrap()
        .as_any()
        .downcast_ref::<T>()
        .unwrap()
}
fn list_vals<'a, T: 'static>(b: &'a RecordBatch, n: &str) -> (usize, &'a T) {
    let l = col::<FixedSizeListArray>(b, n);
    (
        l.value_length() as usize,
        l.values().as_any().downcast_ref::<T>().unwrap(),
    )
}
impl AcqReader for R {
    fn poll(&mut self, s: &mut Sink) -> Result<bool> {
        let mut got = false;
        while self.dir.join(format!("{:06}.done", self.n)).exists() {
            for (i, t) in ["a", "b", "c"].into_iter().enumerate() {
                if !self.want[i] {
                    continue;
                }
                let p = self.dir.join(format!("{t}_{:06}.parquet", self.n));
                let mut rows: Vec<(u64, u64, usize, RecordBatch)> = Vec::new();
                for b in
                    ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(p)?)?.build()?
                {
                    let b = b?;
                    for k in 0..b.num_rows() {
                        rows.push((
                            col::<UInt64Array>(&b, "w").value(k),
                            col::<UInt64Array>(&b, "t0").value(k),
                            k,
                            b.clone(),
                        ));
                    }
                }
                // Group consecutive rows by window.
                let mut k = 0;
                while k < rows.len() {
                    let w = rows[k].0;
                    let t0 = rows[k].1;
                    let end = (k..rows.len())
                        .find(|&j| rows[j].0 != w)
                        .unwrap_or(rows.len());
                    let it = rows[k..end].iter().map(|(_, _, r, b)| (*r, b));
                    match i {
                        0 => s.a(
                            w,
                            t0,
                            it.map(|(r, b)| {
                                let (_, a) = list_vals::<arrow_array::Int8Array>(b, "a");
                                let (_, f) = list_vals::<arrow_array::BooleanArray>(b, "f");
                                let (_, x) = list_vals::<UInt16Array>(b, "x");
                                RecA {
                                    t: col::<UInt64Array>(b, "t").value(r),
                                    a: std::array::from_fn(|j| a.value(r * 10 + j)),
                                    f: std::array::from_fn(|j| f.value(r * 3 + j)),
                                    x: std::array::from_fn(|j| x.value(r * 500 + j)),
                                }
                            })
                            .collect::<Vec<_>>()
                            .into_iter(),
                        ),
                        1 => s.b(
                            w,
                            t0,
                            it.map(|(r, b)| {
                                let (_, x) = list_vals::<arrow_array::Float32Array>(b, "x");
                                RecB {
                                    t: col::<UInt64Array>(b, "t").value(r),
                                    x: std::array::from_fn(|j| x.value(r * 32 + j)),
                                    status: col::<UInt16Array>(b, "status").value(r),
                                }
                            })
                            .collect::<Vec<_>>()
                            .into_iter(),
                        ),
                        _ => s.c(
                            w,
                            t0,
                            it.map(|(r, b)| RecC {
                                t: col::<UInt64Array>(b, "t").value(r),
                                code: col::<UInt32Array>(b, "code").value(r),
                                msg: col::<StringArray>(b, "msg").value(r).to_string(),
                            })
                            .collect::<Vec<_>>()
                            .into_iter(),
                        ),
                    }
                    k = end;
                }
            }
            self.n += 1;
            got = true;
        }
        Ok(got)
    }
}
