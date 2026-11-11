//! Scenario 2 (HDF5-favored): a 6-D chunked f32 array larger than memory,
//! read by hyperslabs in every direction.
//!
//! Model output in the shape climate and simulation codes write:
//! [time 64, ensemble 8, variable 4, level 16, lat 256, lon 512] (17.2 GB;
//! `FB_GRID=small` gives a 268 MB shape for smoke tests). The writer receives
//! one time step at a time. Runs are meant to execute under a memory cap
//! (run.py puts this scenario in a 4 GiB cgroup, page cache included), so the
//! data is about four times the memory available to it.
//!
//! All engines store the same chunk shape [1, 2, 1, 4, 32, 32] (32 KB), so
//! any fixed chunking makes some directions expensive. The reader therefore
//! measures every direction instead of a chosen few: lines along each of the
//! six axes, planes over each of the 15 axis pairs, random sub-cubes and
//! ensemble means. Each engine stores and finds chunks with its own best
//! mechanism. Every selection is compared element-wise to the oracle.
use crate::common::*;
use std::path::{Path, PathBuf};

pub const CHUNK: [usize; 6] = [1, 2, 1, 4, 32, 32];
pub const CE: usize = 2 * 4 * 32 * 32; // elements per chunk
pub const CB: usize = CE * 4;

struct Geo {
    shape: [usize; 6],
    cg: [usize; 6],
}
fn geo() -> &'static Geo {
    static G: std::sync::OnceLock<Geo> = std::sync::OnceLock::new();
    G.get_or_init(|| {
        let shape = match std::env::var("FB_GRID").as_deref() {
            Ok("small") => [16, 8, 4, 8, 128, 128],
            _ => [64, 8, 4, 16, 256, 512],
        };
        let mut cg = [0; 6];
        for d in 0..6 {
            assert_eq!(shape[d] % CHUNK[d], 0);
            cg[d] = shape[d] / CHUNK[d];
        }
        Geo { shape, cg }
    })
}
pub fn shape() -> [usize; 6] {
    geo().shape
}
/// Chunks per axis.
pub fn cg() -> [usize; 6] {
    geo().cg
}
/// Elements per time step.
pub fn step_len() -> usize {
    shape()[1..].iter().product()
}
/// Chunks per time step.
fn step_chunks() -> usize {
    cg()[1..].iter().product()
}

type Ix = [usize; 6];

fn strides(shape: &Ix) -> Ix {
    let mut s = [1; 6];
    for d in (0..5).rev() {
        s[d] = s[d + 1] * shape[d + 1];
    }
    s
}
fn lin(i: &Ix, shape: &Ix) -> usize {
    let s = strides(shape);
    (0..6).map(|d| i[d] * s[d]).sum()
}
pub fn value_at(l: usize) -> f32 {
    (splitmix(l as u64) >> 40) as f32 / 1024.0
}
pub fn chunk_id(c: &Ix) -> usize {
    lin(c, &cg())
}
fn chunk_coords(mut id: usize) -> Ix {
    let mut c = [0; 6];
    for d in (0..6).rev() {
        c[d] = id % cg()[d];
        id /= cg()[d];
    }
    c
}

/// Calls `f(i)` for every index in [lo, hi) over axes 0..5 with `i[5] = lo[5]`;
/// the caller handles the contiguous run along the last axis.
fn each_run(lo: &Ix, hi: &Ix, mut f: impl FnMut(&Ix)) {
    if (0..6).any(|d| lo[d] >= hi[d]) {
        return;
    }
    let mut i = *lo;
    loop {
        f(&i);
        let mut d = 4;
        loop {
            i[d] += 1;
            if i[d] < hi[d] {
                break;
            }
            i[d] = lo[d];
            if d == 0 {
                return;
            }
            d -= 1;
        }
    }
}

/// Runs of consecutive chunk ids touched by a selection: `f(first_id, n)`.
fn chunk_runs(start: &Ix, count: &Ix, mut f: impl FnMut(usize, usize)) {
    let mut lo = [0; 6];
    let mut hi = [0; 6];
    for d in 0..6 {
        lo[d] = start[d] / CHUNK[d];
        hi[d] = (start[d] + count[d] - 1) / CHUNK[d] + 1;
    }
    each_run(&lo, &hi, |c| f(chunk_id(c), hi[5] - lo[5]));
}

fn le_f32(bytes: &[u8], out: &mut [f32]) {
    for (o, b) in out.iter_mut().zip(bytes.chunks_exact(4)) {
        *o = f32::from_le_bytes(b.try_into().unwrap());
    }
}

/// Copy the part of chunk `id` (little-endian bytes) inside the selection
/// into `out` (row-major, shape `count`).
fn copy_chunk(start: &Ix, count: &Ix, id: usize, chunk: &[u8], out: &mut [f32]) {
    let c = chunk_coords(id);
    let mut lo = [0; 6];
    let mut hi = [0; 6];
    for d in 0..6 {
        lo[d] = start[d].max(c[d] * CHUNK[d]);
        hi[d] = (start[d] + count[d]).min((c[d] + 1) * CHUNK[d]);
    }
    let os = strides(count);
    let cs = strides(&CHUNK);
    let n = hi[5].saturating_sub(lo[5]);
    each_run(&lo, &hi, |i| {
        let (mut o, mut k) = (0, 0);
        for d in 0..6 {
            o += (i[d] - start[d]) * os[d];
            k += (i[d] - c[d] * CHUNK[d]) * cs[d];
        }
        le_f32(&chunk[k * 4..(k + n) * 4], &mut out[o..o + n]);
    });
}

/// Fill `out` with chunk `id` taken from one time step (shape shape()[1..]).
fn chunk_from_step(step: &[f32], id: usize, out: &mut [u8]) {
    let c = chunk_coords(id);
    let mut lo = [0; 6];
    let mut hi = [0; 6];
    for d in 1..6 {
        lo[d] = c[d] * CHUNK[d];
        hi[d] = lo[d] + CHUNK[d];
    }
    hi[0] = 1;
    let ss = strides(&shape());
    let mut k = 0;
    each_run(&lo, &hi, |i| {
        let s: usize = (1..6).map(|d| i[d] * ss[d]).sum();
        for v in &step[s..s + CHUNK[5]] {
            out[k..k + 4].copy_from_slice(&v.to_le_bytes());
            k += 4;
        }
    });
}

pub trait GridWrite {
    /// One time step, row-major over shape()[1..].
    fn step(&mut self, t: usize, step: &[f32]) -> Result<()>;
    fn finish(self: Box<Self>) -> Result<()>;
}
pub trait GridRead {
    /// Row-major hyperslab `start .. start + count`.
    fn slab(&mut self, start: Ix, count: Ix, out: &mut Vec<f32>) -> Result<()>;
}

fn prepare(count: &Ix, out: &mut Vec<f32>) {
    out.clear();
    out.resize(count.iter().product(), 0.0);
}

// ---------------------------------------------------------------- Varlit matrix
// The chunk grid maps onto the 2-D matrix keyspace: scan = (time, ensemble,
// variable) chunk, ch = (level, lat, lon) chunk. Chunks adjacent along lon are
// adjacent cells, so each run of chunk ids is one run read.
mod vl {
    use super::*;
    use varlit::{MatrixKey, VarlitFile, VarlitReader, varlit_format};
    pub fn cc() -> usize {
        cg()[3..].iter().product()
    }

    varlit_format! {
        pub format GridFmt {
            magic: b"GRID6F32";
            version: 1;
            limits {
                record_payload: 536_870_912;
                logical_payload: 536_870_912;
                materialized_bytes: 1_073_741_824;
                matrix_dimension: 16_000_000;
                matrix_cells: 16_000_000;
                matrix_bitmap: 64_000_000;
                matrix_crc: 128_000_000;
                matrix_metadata: 268_435_456;
                matrix_slot_region: 68_719_476_736;
                sidecar: 268_435_456;
            }
            endian: little;
            schema_hash: computed;
            integrity: crc32_with_header;
            dims { cs: u32, cc: u32, }
            commit: cell_bitmap { keyspace = [cs, cc]; categories = [data]; };
            blocks {
                matrix Chunk(id = 1, dims = [cs, cc], category = data) { v: [f32; 8192], }
            }
        }
    }

    /// The time axis grows: scan rows are appended one time step (all of its
    /// ensemble x variable chunk rows) at a time, as one growing chunk.
    pub fn spec() -> varlit::FormatSpec {
        if fixed() {
            return GridFmt::spec();
        }
        GridFmt::spec().with_growing_matrix_dimension("cs", declared_rows() as u64)
    }
    /// `FB_VL_FIXED=1`: declare the whole time extent up front (no growth),
    /// so every cell lives in the paged matrix region. For comparison only.
    fn fixed() -> bool {
        std::env::var_os("FB_VL_FIXED").is_some()
    }
    fn declared_rows() -> usize {
        if fixed() { cg()[..3].iter().product() } else { rows_per_chunk() }
    }
    /// Scan rows per growing chunk record (`FB_VL_ROWS`, default 1: a 16.8 MB
    /// record at the large shape, buffered in memory until it is written).
    pub fn rows_per_chunk() -> usize {
        std::env::var("FB_VL_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(1)
    }
    pub fn key(id: usize) -> MatrixKey {
        MatrixKey::new((id / cc()) as u64, (id % cc()) as u64)
    }

    pub struct Wr {
        w: VarlitFile,
        buf: Vec<u8>,
    }
    pub fn create(p: &Path) -> Result<Box<dyn GridWrite>> {
        // Declared extent: one time step; later steps grow the file.
        let w = spec().create_with_dims(
            p,
            varlit::MatrixDimensions::from_pairs([("cs", declared_rows() as u64), ("cc", cc() as u64)]),
        )?;
        Ok(Box::new(Wr { w, buf: vec![0; CB] }))
    }
    impl GridWrite for Wr {
        fn step(&mut self, t: usize, step: &[f32]) -> Result<()> {
            for id in t * step_chunks()..(t + 1) * step_chunks() {
                chunk_from_step(step, id, &mut self.buf);
                self.w.write_matrix_cell_payload::<Chunk>(key(id), &self.buf)?;
                self.w.commit_matrix_cell::<Chunk>(key(id))?;
            }
            Ok(())
        }
        fn finish(mut self: Box<Self>) -> Result<()> {
            self.w.sync()?;
            if std::env::var_os("VL_COMPACT").is_some() {
                self.w.compact_matrix()?;
                self.w.sync()?;
            }
            Ok(())
        }
    }

    pub struct Rd {
        r: VarlitReader,
        buf: Vec<u8>,
    }
    pub fn open(p: &Path) -> Result<Box<dyn GridRead>> {
        Ok(Box::new(Rd { r: vl_spec(spec()).open_reader(p)?, buf: Vec::new() }))
    }
    impl GridRead for Rd {
        fn slab(&mut self, start: Ix, count: Ix, out: &mut Vec<f32>) -> Result<()> {
            prepare(&count, out);
            let mut err = None;
            chunk_runs(&start, &count, |id0, n| {
                if err.is_some() {
                    return;
                }
                self.buf.resize(n * CB, 0);
                if let Err(e) = self.r.read_matrix_run_bytes_into::<Chunk>(key(id0), &mut self.buf) {
                    err = Some(e);
                    return;
                }
                for (j, ch) in self.buf.chunks_exact(CB).enumerate() {
                    copy_chunk(&start, &count, id0 + j, ch, out);
                }
            });
            match err {
                Some(e) => Err(e.into()),
                None => Ok(()),
            }
        }
    }
}

// ---------------------------------------------------------------- Varlit matrix via mmap
// Same file as `vl`; chunks are borrowed from a read-only mapping. The writer
// is closed before opening, satisfying the mmap immutability contract.
mod vm {
    use super::vl::{Chunk, key, spec};
    use super::*;
    use varlit::{MmapMatrix, VarlitReader};

    pub struct Rd {
        m: MmapMatrix,
        _r: VarlitReader,
    }
    pub fn open(p: &Path) -> Result<Box<dyn GridRead>> {
        let r = vl_spec(spec()).open_reader(p)?;
        // SAFETY: the writer has been dropped and nothing modifies the files
        // for the lifetime of this mapping.
        let m = unsafe { r.mmap_matrix()? };
        Ok(Box::new(Rd { m, _r: r }))
    }
    impl GridRead for Rd {
        fn slab(&mut self, start: Ix, count: Ix, out: &mut Vec<f32>) -> Result<()> {
            prepare(&count, out);
            let mut err = None;
            chunk_runs(&start, &count, |id0, n| {
                for id in id0..id0 + n {
                    match self.m.cell_payload_window::<Chunk>(key(id)) {
                        Ok(ch) => copy_chunk(&start, &count, id, ch, out),
                        Err(e) => err = Some(e),
                    }
                }
            });
            match err {
                Some(e) => Err(e.into()),
                None => Ok(()),
            }
        }
    }
}

// ---------------------------------------------------------------- Varlit per-cell mmap
// Same file as `vl`; each chunk is mapped on its own (`mmap_matrix_cell`), so
// nothing is mapped or copied beyond the chunks a selection touches.
mod vc {
    use super::vl::{Chunk, key, spec};
    use super::*;
    use varlit::VarlitReader;

    pub struct Rd(VarlitReader);
    pub fn open(p: &Path) -> Result<Box<dyn GridRead>> {
        Ok(Box::new(Rd(vl_spec(spec()).open_reader(p)?)))
    }
    impl GridRead for Rd {
        fn slab(&mut self, start: Ix, count: Ix, out: &mut Vec<f32>) -> Result<()> {
            prepare(&count, out);
            let mut err = None;
            chunk_runs(&start, &count, |id0, n| {
                for id in id0..id0 + n {
                    // SAFETY: the writer has been dropped and nothing modifies
                    // the file while the mapping lives.
                    match unsafe { self.0.mmap_matrix_cell::<Chunk>(key(id)) } {
                        Ok(w) => copy_chunk(&start, &count, id, w.as_bytes(), out),
                        Err(e) => err = Some(e),
                    }
                }
            });
            match err {
                Some(e) => Err(e.into()),
                None => Ok(()),
            }
        }
    }
}

// ---------------------------------------------------------------- HDF5 chunked
// Native 6-D chunked dataset; every selection is one hyperslab H5Dread.
mod h5 {
    use super::*;
    use hdf5::File;
    use ndarray::{ArrayView6, Ix6, s};

    pub struct Wr {
        f: File,
        d: hdf5::Dataset,
        p: PathBuf,
    }
    pub fn create(p: &Path) -> Result<Box<dyn GridWrite>> {
        let f = File::with_options().with_fapl(|a| a.libver_latest()).create(p)?;
        // Unlimited time axis, extended by one step per write.
        let sh = shape();
        let mut ext = vec![hdf5::Extent::resizable(0)];
        ext.extend(sh[1..].iter().map(|&n| hdf5::Extent::fixed(n)));
        let d = f
            .new_dataset::<f32>()
            .chunk(CHUNK.to_vec())
            .shape(hdf5::SimpleExtents::from_vec(ext))
            .create("grid")?;
        Ok(Box::new(Wr { f, d, p: p.to_path_buf() }))
    }
    impl GridWrite for Wr {
        fn step(&mut self, t: usize, step: &[f32]) -> Result<()> {
            let a = ArrayView6::from_shape((1, shape()[1], shape()[2], shape()[3], shape()[4], shape()[5]), step)?;
            let mut dims = shape().to_vec();
            dims[0] = t + 1;
            self.d.resize(dims)?;
            self.d.write_slice(&a, s![t..t + 1, .., .., .., .., ..])?;
            Ok(())
        }
        fn finish(self: Box<Self>) -> Result<()> {
            self.f.flush()?;
            let p = self.p.clone();
            drop(self);
            fsync_file(&p)?;
            Ok(())
        }
    }
    pub struct Rd {
        _f: File,
        d: hdf5::Dataset,
    }
    pub fn open(p: &Path) -> Result<Box<dyn GridRead>> {
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
            *out = self.d.read_slice::<f32, _, Ix6>(sel)?.into_raw_vec_and_offset().0;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- SQLite chunk table
// One row per chunk keyed by its linear chunk id (rowid), so each run of
// chunk ids is one rowid range scan. One transaction per time step;
// durable at the end through a WAL checkpoint.
mod sq {
    use super::*;
    use rusqlite::{Connection, OpenFlags, params};

    pub struct Wr {
        c: Connection,
        buf: Vec<u8>,
    }
    pub fn create(p: &Path) -> Result<Box<dyn GridWrite>> {
        let c = Connection::open(p)?;
        let ps = std::env::var("SQ_PAGE").unwrap_or("32768".into());
        c.execute_batch(&format!(
            "PRAGMA page_size={ps}; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE chunks(id INTEGER PRIMARY KEY, data BLOB NOT NULL);"
        ))?;
        Ok(Box::new(Wr { c, buf: vec![0; CB] }))
    }
    impl GridWrite for Wr {
        fn step(&mut self, t: usize, step: &[f32]) -> Result<()> {
            let tx = self.c.transaction()?;
            {
                let mut st = tx.prepare_cached("INSERT INTO chunks VALUES (?1, ?2)")?;
                for id in t * step_chunks()..(t + 1) * step_chunks() {
                    chunk_from_step(step, id, &mut self.buf);
                    st.execute(params![id as i64, &self.buf[..]])?;
                }
            }
            tx.commit()?;
            Ok(())
        }
        fn finish(self: Box<Self>) -> Result<()> {
            self.c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            Ok(())
        }
    }
    pub struct Rd {
        c: Connection,
    }
    pub fn open(p: &Path) -> Result<Box<dyn GridRead>> {
        let c = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        c.execute_batch("PRAGMA cache_size=-65536; PRAGMA mmap_size=1073741824;")?;
        Ok(Box::new(Rd { c }))
    }
    impl GridRead for Rd {
        fn slab(&mut self, start: Ix, count: Ix, out: &mut Vec<f32>) -> Result<()> {
            prepare(&count, out);
            let mut st = self.c.prepare_cached("SELECT id, data FROM chunks WHERE id BETWEEN ?1 AND ?2")?;
            let mut runs = Vec::new();
            chunk_runs(&start, &count, |id0, n| runs.push((id0, n)));
            for (id0, n) in runs {
                let mut rows = st.query(params![id0 as i64, (id0 + n - 1) as i64])?;
                let mut got = 0;
                while let Some(r) = rows.next()? {
                    let id = r.get::<_, i64>(0)? as usize;
                    copy_chunk(&start, &count, id, r.get_ref(1)?.as_blob()?, out);
                    got += 1;
                }
                if got != n {
                    return Err("missing chunk".into());
                }
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- Parquet chunk rows
// One row per chunk, one row group per time step, one data page per chunk and
// a page index, so a RowSelection fetches exactly the needed chunk pages.
mod pq {
    use super::*;
    use arrow_array::{Array, ArrayRef, FixedSizeBinaryArray, RecordBatch, UInt32Array};
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::arrow_reader::{
        ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection, RowSelectionPolicy,
        RowSelector,
    };
    use parquet::arrow::{ArrowWriter, ProjectionMask};
    use parquet::file::properties::{EnabledStatistics, WriterProperties};
    use std::sync::Arc;

    pub struct Wr {
        w: ArrowWriter<std::fs::File>,
        schema: Arc<Schema>,
        buf: Vec<u8>,
        chunk: Vec<u8>,
    }
    pub fn create(p: &Path) -> Result<Box<dyn GridWrite>> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("chunk", DataType::UInt32, false),
            Field::new("data", DataType::FixedSizeBinary(CB as i32), false),
        ]));
        let props = WriterProperties::builder()
            .set_compression(parquet::basic::Compression::UNCOMPRESSED)
            .set_dictionary_enabled(false)
            .set_max_row_group_row_count(Some(step_chunks()))
            .set_data_page_row_count_limit(1)
            .set_write_batch_size(1)
            .set_data_page_size_limit(1)
            .set_statistics_enabled(EnabledStatistics::Page)
            .build();
        let w = ArrowWriter::try_new(std::fs::File::create(p)?, schema.clone(), Some(props))?;
        Ok(Box::new(Wr { w, schema, buf: Vec::with_capacity(step_chunks() * CB), chunk: vec![0; CB] }))
    }
    impl GridWrite for Wr {
        fn step(&mut self, t: usize, step: &[f32]) -> Result<()> {
            self.buf.clear();
            let ids = t * step_chunks()..(t + 1) * step_chunks();
            for id in ids.clone() {
                chunk_from_step(step, id, &mut self.chunk);
                self.buf.extend_from_slice(&self.chunk);
            }
            let idc = UInt32Array::from_iter_values(ids.map(|i| i as u32));
            let data = FixedSizeBinaryArray::try_new(CB as i32, self.buf.clone().into(), None)?;
            let cols: Vec<ArrayRef> = vec![Arc::new(idc), Arc::new(data)];
            self.w.write(&RecordBatch::try_new(self.schema.clone(), cols)?)?;
            self.w.flush()?; // close the row group = one time step
            Ok(())
        }
        fn finish(self: Box<Self>) -> Result<()> {
            self.w.into_inner()?.sync_all()?;
            Ok(())
        }
    }
    pub struct Rd {
        f: std::fs::File,
        meta: ArrowReaderMetadata,
    }
    pub fn open(p: &Path) -> Result<Box<dyn GridRead>> {
        let f = std::fs::File::open(p)?;
        let meta = ArrowReaderMetadata::load(
            &f,
            ArrowReaderOptions::new().with_page_index_policy(parquet::file::metadata::PageIndexPolicy::Required),
        )?;
        Ok(Box::new(Rd { f, meta }))
    }
    impl GridRead for Rd {
        fn slab(&mut self, start: Ix, count: Ix, out: &mut Vec<f32>) -> Result<()> {
            prepare(&count, out);
            let mut runs = Vec::new();
            chunk_runs(&start, &count, |id0, n| runs.push((id0, n)));
            // Runs are ascending and never cross a time step (row group).
            let mut rgs: Vec<usize> = Vec::new();
            let mut sel = Vec::new();
            let mut pos = 0;
            let mut ids = Vec::new();
            for &(id0, n) in &runs {
                let (rg, lo) = (id0 / step_chunks(), id0 % step_chunks());
                if rgs.last() != Some(&rg) {
                    if !rgs.is_empty() {
                        sel.push(RowSelector::skip(step_chunks() - pos));
                    }
                    rgs.push(rg);
                    pos = 0;
                }
                sel.push(RowSelector::skip(lo - pos));
                sel.push(RowSelector::select(n));
                pos = lo + n;
                ids.extend(id0..id0 + n);
            }
            sel.push(RowSelector::skip(step_chunks() - pos));
            let mask = ProjectionMask::roots(self.meta.parquet_schema(), [1]);
            let r = ParquetRecordBatchReaderBuilder::new_with_metadata(self.f.try_clone()?, self.meta.clone())
                .with_row_groups(rgs)
                .with_projection(mask)
                .with_row_selection(RowSelection::from(sel))
                .with_row_selection_policy(RowSelectionPolicy::Selectors)
                .with_batch_size(step_chunks())
                .build()?;
            let mut i = 0;
            for b in r {
                let b = b?;
                let d = b.column(0).as_any().downcast_ref::<FixedSizeBinaryArray>().unwrap();
                for j in 0..d.len() {
                    copy_chunk(&start, &count, ids[i], d.value(j), out);
                    i += 1;
                }
            }
            if i != ids.len() {
                return Err("missing chunk".into());
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- driver
fn verify(start: &Ix, count: &Ix, v: &[f32], what: &str) {
    assert_eq!(v.len(), count.iter().product::<usize>(), "{what} size");
    let mut hi = [0; 6];
    for d in 0..6 {
        hi[d] = start[d] + count[d];
    }
    let mut k = 0;
    each_run(start, &hi, |i| {
        let base = lin(i, &shape());
        for x in 0..count[5] {
            assert_eq!(v[k], value_at(base + x), "{what} mismatch");
            k += 1;
        }
    });
}

pub fn run(fmt: &str, dir: &Path) -> Result<Metrics> {
    let p = dir.join(format!("grid.{fmt}"));
    let mut m = Metrics::default();
    let sh = shape();
    m.set("shape", format!("{sh:?}"));
    let mut step = vec![0f32; step_len()];
    let t = Timer::start();
    let mut gen_s = 0.0;
    let mut w = match fmt {
        "varlit" | "varlit-mmap" | "varlit-mmap-cell" => vl::create(&p)?,
        "hdf5" => h5::create(&p)?,
        "sqlite" => sq::create(&p)?,
        "parquet" => pq::create(&p)?,
        _ => return Err("format".into()),
    };
    for ts in 0..sh[0] {
        let g = Timer::start();
        let base = ts * step_len();
        for (j, v) in step.iter_mut().enumerate() {
            *v = value_at(base + j);
        }
        gen_s += g.secs();
        w.step(ts, &step)?;
    }
    w.finish()?;
    drop(step);
    m.set("write_s", t.secs() - gen_s);
    m.set("bytes", dir_bytes(dir));

    // Query plan (untimed): every direction.
    let mut rng = 0x5EEDu64;
    let mut next = |n: usize| {
        rng = splitmix(rng);
        (rng % n as u64) as usize
    };
    let point = |next: &mut dyn FnMut(usize) -> usize| {
        let mut s = [0; 6];
        for d in 0..6 {
            s[d] = next(sh[d]);
        }
        s
    };
    let mut sets: Vec<(String, Vec<(Ix, Ix)>)> = Vec::new();
    for d in 0..6 {
        let sels = (0..8)
            .map(|_| {
                let mut s = point(&mut next);
                let mut c = [1; 6];
                s[d] = 0;
                c[d] = sh[d];
                (s, c)
            })
            .collect();
        sets.push((format!("line{d}"), sels));
    }
    for a in 0..6 {
        for b in a + 1..6 {
            let sels = (0..4)
                .map(|_| {
                    let mut s = point(&mut next);
                    let mut c = [1; 6];
                    for d in [a, b] {
                        s[d] = 0;
                        c[d] = sh[d];
                    }
                    (s, c)
                })
                .collect();
            sets.push((format!("plane{a}{b}"), sels));
        }
    }
    let cube = [8, 4, 2, 4, 64, 64];
    let cubes = (0..16)
        .map(|_| {
            let mut s = [0; 6];
            for d in 0..6 {
                s[d] = next(sh[d] - cube[d] + 1);
            }
            (s, cube)
        })
        .collect();
    sets.push(("cubes".into(), cubes));
    let (ne, ny, nx) = (sh[1], sh[4], sh[5]);
    let means: Vec<(Ix, Ix)> = (0..4)
        .map(|_| ([next(sh[0]), 0, next(sh[2]), next(sh[3]), 0, 0], [1, ne, 1, 1, ny, nx]))
        .collect();

    // One pass from a cold cache; under the memory cap nothing stays warm.
    evict(dir);
    let t = Timer::start();
    let mut r = match fmt {
        "varlit" => vl::open(&p)?,
        "varlit-mmap" => vm::open(&p)?,
        "varlit-mmap-cell" => vc::open(&p)?,
        "hdf5" => h5::open(&p)?,
        "sqlite" => sq::open(&p)?,
        _ => pq::open(&p)?,
    };
    m.set("open_ms", t.secs() * 1e3);
    let mut total = 0.0;
    let mut out = Vec::new();
    for (name, sels) in &sets {
        let mut secs = 0.0;
        for (s, c) in sels {
            let t = Timer::start();
            r.slab(*s, *c, &mut out)?;
            secs += t.secs();
            verify(s, c, &out, name);
        }
        m.set(&format!("{name}_s"), secs);
        total += secs;
    }
    // Ensemble mean: read all members of a lat x lon plane, average.
    let mut secs = 0.0;
    for (s, c) in &means {
        let t = Timer::start();
        r.slab(*s, *c, &mut out)?;
        let plane = ny * nx;
        let mut mean = vec![0f32; plane];
        for e in 0..ne {
            for (a, v) in mean.iter_mut().zip(&out[e * plane..(e + 1) * plane]) {
                *a += v;
            }
        }
        mean.iter_mut().for_each(|a| *a /= ne as f32);
        secs += t.secs();
        std::hint::black_box(&mean);
        verify(s, c, &out, "ensmean");
    }
    m.set("ensmean_s", secs);
    total += secs;
    m.set("reads_s", total);
    m.set("peak_rss_mb", peak_rss_mb());
    m.set("verified", true);
    Ok(m)
}

fn peak_rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmHWM:")).map(|l| l.to_string()))
        .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()))
        .map_or(0.0, |kb| kb / 1024.0)
}
