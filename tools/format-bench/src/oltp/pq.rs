//! Parquet: immutable files, so updates go to small delta files (LSM style):
//! the base table is written once (streamed in row groups), each transaction
//! writes one delta file with the new balances (temp + rename + fsync = atomic,
//! durable). The writer keeps its own newest-balance table for read-modify-
//! write; readers replay all delta files at open. The branch secondary index
//! is a second file of ids sorted by branch plus an offsets file.
//! Candidates: rows per row group.
//! app-built: delta files + replay (LSM), writer memtable, CSR branch files (csr.rs)
use super::*;
use arrow_array::{
    Array, ArrayRef, FixedSizeBinaryArray, Int64Array, RecordBatch, UInt32Array, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection,
    RowSelectionPolicy, RowSelector,
};
use parquet::arrow::{ArrowWriter, ProjectionMask};
use parquet::file::metadata::PageIndexPolicy;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use std::sync::Arc;

pub const CANDIDATES: &[&str] = &["rg65536", "rg1048576"];
const PAGE: usize = 1024;
const STEP: usize = 1 << 20;

fn props(rg: usize) -> WriterProperties {
    WriterProperties::builder()
        .set_compression(parquet::basic::Compression::UNCOMPRESSED)
        .set_dictionary_enabled(false)
        .set_max_row_group_row_count(Some(rg))
        .set_data_page_row_count_limit(PAGE)
        .set_write_batch_size(PAGE)
        .set_statistics_enabled(EnabledStatistics::Page)
        .build()
}
fn base_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt64, false),
        Field::new("branch", DataType::UInt32, false),
        Field::new("balance", DataType::Int64, false),
        Field::new("name", DataType::FixedSizeBinary(24), false),
        Field::new("created", DataType::UInt64, false),
    ]))
}
fn u64_schema(name: &str) -> Arc<Schema> {
    Arc::new(Schema::new(vec![Field::new(name, DataType::UInt64, false)]))
}
fn delta_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt64, false),
        Field::new("balance", DataType::Int64, false),
    ]))
}
/// Streams batches into `dir/name` via a temporary file, then renames it
/// into place and syncs the directory.
fn write_file(
    dir: &Path,
    name: &str,
    schema: Arc<Schema>,
    rg: usize,
    mut fill: impl FnMut(&mut ArrowWriter<std::fs::File>) -> Result<()>,
) -> Result<()> {
    let tmp = dir.join("tmp.parquet");
    let mut w = ArrowWriter::try_new(std::fs::File::create(&tmp)?, schema, Some(props(rg)))?;
    fill(&mut w)?;
    w.into_inner()?.sync_all()?;
    std::fs::rename(&tmp, dir.join(name))?;
    fsync_dir(dir)?;
    Ok(())
}

struct Base {
    f: std::fs::File,
    meta: ArrowReaderMetadata,
}
impl Base {
    fn open(p: &Path) -> Result<Self> {
        let f = std::fs::File::open(p)?;
        let meta = ArrowReaderMetadata::load(
            &f,
            ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Required),
        )?;
        Ok(Self { f, meta })
    }
    /// Read `cols` for the sorted global row positions `rows` in one pass.
    fn rows(
        &self,
        rows: &[usize],
        cols: &[usize],
        mut each: impl FnMut(&RecordBatch, usize),
    ) -> Result<()> {
        let total = self.meta.metadata().file_metadata().num_rows() as usize;
        let mut sel = Vec::new();
        let mut pos = 0;
        for &r in rows {
            if r > pos {
                sel.push(RowSelector::skip(r - pos));
            }
            if r >= pos {
                sel.push(RowSelector::select(1));
                pos = r + 1;
            }
        }
        if pos < total {
            sel.push(RowSelector::skip(total - pos));
        }
        let mask = ProjectionMask::roots(self.meta.parquet_schema(), cols.iter().copied());
        let r = ParquetRecordBatchReaderBuilder::new_with_metadata(
            self.f.try_clone()?,
            self.meta.clone(),
        )
        .with_projection(mask)
        .with_row_selection(RowSelection::from(sel))
        .with_row_selection_policy(RowSelectionPolicy::Selectors)
        .build()?;
        for b in r {
            let b = b?;
            for i in 0..b.num_rows() {
                each(&b, i);
            }
        }
        Ok(())
    }
}
fn u64c(b: &RecordBatch, c: usize) -> &UInt64Array {
    b.column(c).as_any().downcast_ref().unwrap()
}
fn i64c(b: &RecordBatch, c: usize) -> &Int64Array {
    b.column(c).as_any().downcast_ref().unwrap()
}

pub struct W {
    dir: PathBuf,
    rg: usize,
    base: Option<Base>,
    mem: HashMap<u64, i64>,
    n: usize,
}
pub fn create(dir: &Path, cand: &str) -> Result<Box<dyn Oltp>> {
    std::fs::create_dir_all(dir)?;
    let rg = cand.trim_start_matches("rg").parse().unwrap_or(65536);
    Ok(Box::new(W {
        dir: dir.to_path_buf(),
        rg,
        base: None,
        mem: HashMap::new(),
        n: 0,
    }))
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        let n = accounts() as usize;
        write_file(&self.dir, "base.parquet", base_schema(), self.rg, |w| {
            for a in (0..n).step_by(STEP) {
                let z = (a + STEP).min(n) as u64;
                let ids = a as u64..z;
                let mut names = Vec::with_capacity((z as usize - a) * 24);
                for id in ids.clone() {
                    names.extend_from_slice(&name_of(id));
                }
                let cols: Vec<ArrayRef> = vec![
                    Arc::new(UInt64Array::from_iter_values(ids.clone())),
                    Arc::new(UInt32Array::from_iter_values(ids.clone().map(branch_of))),
                    Arc::new(Int64Array::from_iter_values(std::iter::repeat_n(
                        START,
                        z as usize - a,
                    ))),
                    Arc::new(FixedSizeBinaryArray::try_new(24, names.into(), None)?),
                    Arc::new(UInt64Array::from_iter_values(ids.map(created_of))),
                ];
                w.write(&RecordBatch::try_new(base_schema(), cols)?)?;
            }
            Ok(())
        })?;
        let mut off = vec![0u64];
        write_file(
            &self.dir,
            "branch_ids.parquet",
            u64_schema("id"),
            self.rg,
            |w| {
                let mut buf: Vec<u64> = Vec::with_capacity(STEP);
                csr::by_branch(&std::env::temp_dir().join("csr"), |_, list| {
                    buf.extend_from_slice(list);
                    off.push(off.last().unwrap() + list.len() as u64);
                    if buf.len() >= STEP {
                        let a: ArrayRef = Arc::new(UInt64Array::from(std::mem::take(&mut buf)));
                        w.write(&RecordBatch::try_new(u64_schema("id"), vec![a])?)?;
                    }
                    Ok(())
                })?;
                let a: ArrayRef = Arc::new(UInt64Array::from(buf));
                w.write(&RecordBatch::try_new(u64_schema("id"), vec![a])?)?;
                Ok(())
            },
        )?;
        write_file(
            &self.dir,
            "branch_off.parquet",
            u64_schema("off"),
            self.rg,
            |w| {
                let a: ArrayRef = Arc::new(UInt64Array::from(std::mem::take(&mut off)));
                w.write(&RecordBatch::try_new(u64_schema("off"), vec![a])?)?;
                Ok(())
            },
        )?;
        self.base = Some(Base::open(&self.dir.join("base.parquet"))?);
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        let mut need: Vec<usize> = deltas
            .iter()
            .filter(|d| !self.mem.contains_key(&d.0))
            .map(|d| d.0 as usize)
            .collect();
        need.sort_unstable();
        let mut cur: HashMap<u64, i64> = HashMap::new();
        if !need.is_empty() {
            self.base.as_ref().unwrap().rows(&need, &[0, 2], |b, i| {
                cur.insert(u64c(b, 0).value(i), i64c(b, 1).value(i));
            })?;
        }
        let (mut ids, mut bal) = (Vec::new(), Vec::new());
        for &(id, d) in deltas {
            let v = self
                .mem
                .get(&id)
                .or(cur.get(&id))
                .copied()
                .ok_or("missing")?
                + d;
            ids.push(id);
            bal.push(v);
        }
        let batch = RecordBatch::try_new(
            delta_schema(),
            vec![
                Arc::new(UInt64Array::from(ids.clone())),
                Arc::new(Int64Array::from(bal.clone())),
            ],
        )?;
        write_file(
            &self.dir,
            &format!("delta_{:06}.parquet", self.n),
            delta_schema(),
            self.rg,
            |w| Ok(w.write(&batch)?),
        )?;
        self.n += 1;
        for (id, v) in ids.into_iter().zip(bal) {
            self.mem.insert(id, v);
        }
        Ok(())
    }
}

pub struct R {
    base: Base,
    ids: Base,
    off: Base,
    mem: HashMap<u64, i64>,
}
pub fn open(dir: &Path, _cand: &str) -> Result<Box<dyn OltpRead>> {
    let mut mem = HashMap::new();
    let mut names: Vec<_> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.file_name().into_string().unwrap())
        .filter(|n| n.starts_with("delta_"))
        .collect();
    names.sort();
    for n in names {
        for b in
            ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(dir.join(n))?)?.build()?
        {
            let b = b?;
            for i in 0..b.num_rows() {
                mem.insert(u64c(&b, 0).value(i), i64c(&b, 1).value(i));
            }
        }
    }
    Ok(Box::new(R {
        base: Base::open(&dir.join("base.parquet"))?,
        ids: Base::open(&dir.join("branch_ids.parquet"))?,
        off: Base::open(&dir.join("branch_off.parquet"))?,
        mem,
    }))
}
impl OltpRead for R {
    fn point(&mut self, id: u64) -> Result<Row> {
        let mut row = None;
        self.base.rows(&[id as usize], &[0, 1, 2, 3, 4], |b, i| {
            let name = b
                .column(3)
                .as_any()
                .downcast_ref::<FixedSizeBinaryArray>()
                .unwrap();
            row = Some(Row {
                id: u64c(b, 0).value(i),
                branch: b
                    .column(1)
                    .as_any()
                    .downcast_ref::<UInt32Array>()
                    .unwrap()
                    .value(i),
                balance: i64c(b, 2).value(i),
                name: name.value(i).try_into().unwrap(),
                created: u64c(b, 4).value(i),
            });
        })?;
        let mut row = row.ok_or("missing")?;
        if let Some(&v) = self.mem.get(&id) {
            row.balance = v;
        }
        Ok(row)
    }
    fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
        let mut o = Vec::new();
        self.off
            .rows(&[b as usize, b as usize + 1], &[0], |bt, i| {
                o.push(u64c(bt, 0).value(i) as usize)
            })?;
        let (a, z) = (o[0], o[1]);
        let mut ids = Vec::new();
        let rows: Vec<usize> = (a..z).collect();
        self.ids
            .rows(&rows, &[0], |bt, i| ids.push(u64c(bt, 0).value(i) as usize))?;
        ids.sort_unstable();
        let mut s = 0;
        let mem = &self.mem;
        self.base.rows(&ids, &[0, 2], |bt, i| {
            let id = u64c(bt, 0).value(i);
            s += mem.get(&id).copied().unwrap_or(i64c(bt, 1).value(i));
        })?;
        Ok((s, ids.len() as u64))
    }
}
