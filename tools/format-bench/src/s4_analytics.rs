//! Scenario 4 (Parquet-favored): wide-table analytics.
//!
//! ROWS sales rows x 12 columns, written once. A fresh reader runs
//!  Q1: SELECT category, SUM(price*qty) WHERE qty > 50 GROUP BY category
//!      (3 of 12 columns, full scan)
//!  Q2: 20 x SELECT COUNT(*), SUM(price) WHERE ts BETWEEN a AND b
//!      (narrow ranges on the sorted ts column; pruning matters)
//! plus total bytes on disk. Results are checked against an oracle.
use crate::common::*;
use std::path::{Path, PathBuf};

pub const ROWS: usize = 4 * 1024 * 1024;
pub const RG: usize = 128 * 1024; // rows per row group / column chunk
pub const NCAT: usize = 32;
pub const NREG: usize = 8;
pub const Q2: usize = 20;
pub const Q2_SPAN: i64 = 400_000; // ~1% of the ts domain

pub struct Table {
    pub ts: Vec<i64>,
    pub user: Vec<i64>,
    pub cat: Vec<u8>, // dictionary code into cat_names
    pub region: Vec<u8>,
    pub qty: Vec<i32>,
    pub price: Vec<f64>,
    pub discount: Vec<f32>,
    pub flags: Vec<u8>,
    pub m: [Vec<f64>; 4],
}
pub fn cat_name(i: usize) -> String {
    format!("category_{i:02}")
}
pub fn region_name(i: usize) -> String {
    format!("region_{i}")
}

fn generate() -> Table {
    let mut s = 0x7AB1Eu64;
    let mut r = || {
        s = splitmix(s);
        s
    };
    let mut t = Table {
        ts: Vec::with_capacity(ROWS),
        user: Vec::with_capacity(ROWS),
        cat: Vec::with_capacity(ROWS),
        region: Vec::with_capacity(ROWS),
        qty: Vec::with_capacity(ROWS),
        price: Vec::with_capacity(ROWS),
        discount: Vec::with_capacity(ROWS),
        flags: Vec::with_capacity(ROWS),
        m: Default::default(),
    };
    for i in 0..ROWS {
        t.ts.push(1_700_000_000_000 + i as i64 * 10 + (r() % 10) as i64);
        t.user.push((r() % 100_000) as i64);
        // Skewed categories, like real data.
        let c = r() % 1024;
        t.cat.push(((c * c) / (1024 * 1024 / NCAT as u64)) as u8);
        t.region.push((r() % NREG as u64) as u8);
        t.qty.push((r() % 100) as i32 + 1);
        t.price.push((r() % 100_000) as f64 / 100.0 + 1.0);
        t.discount.push((r() % 31) as f32 / 100.0);
        t.flags.push((r() & 0x7) as u8);
        for k in 0..4 {
            t.m[k].push((r() >> 11) as f64 / (1u64 << 53) as f64);
        }
    }
    t
}

pub type Q1 = [f64; NCAT];
pub trait Analytics {
    fn q1(&mut self) -> Result<Q1>;
    fn q2(&mut self, a: i64, b: i64) -> Result<(u64, f64)>;
}

fn le<T: Copy, const W: usize>(v: &[T], to: fn(T) -> [u8; W]) -> Vec<u8> {
    let mut o = Vec::with_capacity(v.len() * W);
    for &x in v {
        o.extend_from_slice(&to(x));
    }
    o
}

// ---------------------------------------------------------------- Varlit column chunks
mod vl {
    use super::*;
    use varlit::{BatchOptions, DiskIndexOptions, varlit_format};
    const NRG: usize = ROWS / RG; // 32

    varlit_format! {
        pub format Sales {
            magic: b"SALESCOL";
            version: 1;
            limits {
                record_payload: 67_108_864;
                logical_payload: 268_435_456;
                materialized_bytes: 1_073_741_824;
            }
            endian: little;
            schema_hash: computed;
            integrity: crc32_with_header;
            index: keyed_offset_chain;
            compression: variable_blocks(
                zstd,
                level = fast,
                header = record_explicit,
                min_len = 1024,
                only_if_smaller = true,
                max_len = 67108864,
            );
            blocks {
                // One column chunk (RG rows, raw little-endian values).
                variable Chunk(id = 1, key = [col, rg], key_index = disk,
                    key_domain = [col = 0..12, rg = 0..32]) {
                    col: u8,
                    rg: u16,
                    data: Vec<u8>,
                }
                // Per-row-group zone map for pruning.
                fixed Zone(id = 2, key = [rg], key_index = disk, key_domain = [rg = 0..32]) {
                    rg: u16,
                    ts_min: i64,
                    ts_max: i64,
                }
                variable Dict(id = 3, key = [col], key_index = disk, key_domain = [col = 0..12]) {
                    col: u8,
                    names: Vec<String>,
                }
            }
        }
    }

    pub fn write(p: &Path, t: &Table) -> Result<()> {
        let mut w = Sales::create_indexed_writer(p, DiskIndexOptions::default())?;
        for rg in 0..NRG {
            let r = rg * RG..(rg + 1) * RG;
            let cols: [Vec<u8>; 12] = [
                le(&t.ts[r.clone()], i64::to_le_bytes),
                le(&t.user[r.clone()], i64::to_le_bytes),
                t.cat[r.clone()].to_vec(),
                t.region[r.clone()].to_vec(),
                le(&t.qty[r.clone()], i32::to_le_bytes),
                le(&t.price[r.clone()], f64::to_le_bytes),
                le(&t.discount[r.clone()], f32::to_le_bytes),
                t.flags[r.clone()].to_vec(),
                le(&t.m[0][r.clone()], f64::to_le_bytes),
                le(&t.m[1][r.clone()], f64::to_le_bytes),
                le(&t.m[2][r.clone()], f64::to_le_bytes),
                le(&t.m[3][r.clone()], f64::to_le_bytes),
            ];
            w.push_chunks(
                cols.into_iter().enumerate().map(|(c, data)| Chunk {
                    key: ChunkKey::from_values((c as u8, rg as u16)).unwrap(),
                    data,
                }),
                BatchOptions::default(),
            )
            .map_err(|f| f.source)?;
            w.push_zone(&Zone {
                key: ZoneKey::from_values(rg as u16)?,
                ts_min: t.ts[r.start],
                ts_max: t.ts[r.end - 1],
            })?;
        }
        w.push_dict(&Dict { key: DictKey::from_values(2)?, names: (0..NCAT).map(cat_name).collect() })?;
        w.push_dict(&Dict { key: DictKey::from_values(3)?, names: (0..NREG).map(region_name).collect() })?;
        w.sync()?;
        Ok(())
    }
    pub struct R(SalesIndexedReader);
    pub fn open(p: &Path) -> Result<Box<dyn Analytics>> {
        Ok(Box::new(R(Sales::open_indexed_reader(p, vl_dio(DiskIndexOptions::default()))?)))
    }
    impl R {
        fn col(&self, c: u8, rg: usize) -> Result<Vec<u8>> {
            Ok(got(|o| self.0.get_chunk(&ChunkKey::from_values((c, rg as u16))?, o), "missing chunk")?.data)
        }
    }
    impl Analytics for R {
        fn q1(&mut self) -> Result<Q1> {
            let names = got(|o| self.0.get_dict(&DictKey::from_values(2)?, o), "dict")?.names;
            assert_eq!(names.len(), NCAT);
            let mut out = [0.0; NCAT];
            for rg in 0..NRG {
                let (c, q, p) = (self.col(2, rg)?, self.col(4, rg)?, self.col(5, rg)?);
                for i in 0..RG {
                    let qty = i32::from_le_bytes(q[i * 4..i * 4 + 4].try_into().unwrap());
                    if qty > 50 {
                        let price = f64::from_le_bytes(p[i * 8..i * 8 + 8].try_into().unwrap());
                        out[c[i] as usize] += price * qty as f64;
                    }
                }
            }
            Ok(out)
        }
        fn q2(&mut self, a: i64, b: i64) -> Result<(u64, f64)> {
            let (mut n, mut s) = (0, 0.0);
            for rg in 0..NRG {
                let z = got(|o| self.0.get_zone(&ZoneKey::from_values(rg as u16)?, o), "zone")?;
                if z.ts_max < a || z.ts_min > b {
                    continue;
                }
                let (ts, p) = (self.col(0, rg)?, self.col(5, rg)?);
                for i in 0..RG {
                    let t = i64::from_le_bytes(ts[i * 8..i * 8 + 8].try_into().unwrap());
                    if t >= a && t <= b {
                        n += 1;
                        s += f64::from_le_bytes(p[i * 8..i * 8 + 8].try_into().unwrap());
                    }
                }
            }
            Ok((n, s))
        }
    }
}

// ---------------------------------------------------------------- Parquet
mod pq {
    use super::*;
    use arrow_array::types::Int8Type;
    use arrow_array::{
        Array, ArrayRef, DictionaryArray, Float32Array, Float64Array, Int32Array, Int64Array, Int8Array, RecordBatch,
        StringArray, UInt8Array,
    };
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::arrow::{ArrowWriter, ProjectionMask};
    use parquet::basic::{Compression, Encoding, ZstdLevel};
    use parquet::schema::types::ColumnPath;
    use parquet::file::properties::{EnabledStatistics, WriterProperties};
    use parquet::file::statistics::Statistics;
    use std::sync::Arc;

    fn dict(codes: &[u8], names: Vec<String>) -> ArrayRef {
        let keys = Int8Array::from_iter_values(codes.iter().map(|&c| c as i8));
        Arc::new(DictionaryArray::<Int8Type>::try_new(keys, Arc::new(StringArray::from(names))).unwrap())
    }
    pub fn write(p: &Path, t: &Table) -> Result<()> {
        let dt = DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Utf8));
        let schema = Arc::new(Schema::new(vec![
            Field::new("ts", DataType::Int64, false),
            Field::new("user", DataType::Int64, false),
            Field::new("category", dt.clone(), false),
            Field::new("region", dt, false),
            Field::new("qty", DataType::Int32, false),
            Field::new("price", DataType::Float64, false),
            Field::new("discount", DataType::Float32, false),
            Field::new("flags", DataType::UInt8, false),
            Field::new("m0", DataType::Float64, false),
            Field::new("m1", DataType::Float64, false),
            Field::new("m2", DataType::Float64, false),
            Field::new("m3", DataType::Float64, false),
        ]));
        // Per-column encodings: delta for the sorted timestamp, byte-stream-split
        // for floats, dictionary only for low-cardinality columns.
        let mut b = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::try_new(1)?))
            .set_max_row_group_row_count(Some(RG))
            .set_statistics_enabled(EnabledStatistics::Chunk);
        let cp = |n: &str| ColumnPath::from(n);
        b = b.set_column_dictionary_enabled(cp("ts"), false).set_column_encoding(cp("ts"), Encoding::DELTA_BINARY_PACKED);
        b = b.set_column_dictionary_enabled(cp("user"), false);
        for f in ["price", "discount", "m0", "m1", "m2", "m3"] {
            b = b.set_column_dictionary_enabled(cp(f), false).set_column_encoding(cp(f), Encoding::BYTE_STREAM_SPLIT);
        }
        let props = b.build();
        let mut w = ArrowWriter::try_new(std::fs::File::create(p)?, schema.clone(), Some(props))?;
        for rg in 0..ROWS / RG {
            let r = rg * RG..(rg + 1) * RG;
            let cols: Vec<ArrayRef> = vec![
                Arc::new(Int64Array::from(t.ts[r.clone()].to_vec())),
                Arc::new(Int64Array::from(t.user[r.clone()].to_vec())),
                dict(&t.cat[r.clone()], (0..NCAT).map(cat_name).collect()),
                dict(&t.region[r.clone()], (0..NREG).map(region_name).collect()),
                Arc::new(Int32Array::from(t.qty[r.clone()].to_vec())),
                Arc::new(Float64Array::from(t.price[r.clone()].to_vec())),
                Arc::new(Float32Array::from(t.discount[r.clone()].to_vec())),
                Arc::new(UInt8Array::from(t.flags[r.clone()].to_vec())),
                Arc::new(Float64Array::from(t.m[0][r.clone()].to_vec())),
                Arc::new(Float64Array::from(t.m[1][r.clone()].to_vec())),
                Arc::new(Float64Array::from(t.m[2][r.clone()].to_vec())),
                Arc::new(Float64Array::from(t.m[3][r.clone()].to_vec())),
            ];
            w.write(&RecordBatch::try_new(schema.clone(), cols)?)?;
        }
        w.into_inner()?.sync_all()?;
        Ok(())
    }
    pub struct R(PathBuf, parquet::arrow::arrow_reader::ArrowReaderMetadata);
    pub fn open(p: &Path) -> Result<Box<dyn Analytics>> {
        let f = std::fs::File::open(p)?;
        let m = parquet::arrow::arrow_reader::ArrowReaderMetadata::load(&f, Default::default())?;
        Ok(Box::new(R(p.to_path_buf(), m)))
    }
    impl R {
        fn reader(&self, cols: &[usize], rgs: Option<Vec<usize>>) -> Result<parquet::arrow::arrow_reader::ParquetRecordBatchReader> {
            let f = std::fs::File::open(&self.0)?;
            let mask = ProjectionMask::roots(self.1.parquet_schema(), cols.iter().copied());
            let mut b = ParquetRecordBatchReaderBuilder::new_with_metadata(f, self.1.clone())
                .with_projection(mask)
                .with_batch_size(RG);
            if let Some(r) = rgs {
                b = b.with_row_groups(r);
            }
            Ok(b.build()?)
        }
    }
    impl Analytics for R {
        fn q1(&mut self) -> Result<Q1> {
            let mut out = [0.0; NCAT];
            for b in self.reader(&[2, 4, 5], None)? {
                let b = b?;
                let c = b.column(0).as_any().downcast_ref::<DictionaryArray<Int8Type>>().unwrap();
                // Map this batch's dictionary to global category codes.
                let vals = c.values().as_any().downcast_ref::<StringArray>().unwrap();
                let map: Vec<usize> = (0..vals.len()).map(|i| vals.value(i)[9..].parse().unwrap()).collect();
                let keys = c.keys().values();
                let q = b.column(1).as_any().downcast_ref::<Int32Array>().unwrap().values();
                let p = b.column(2).as_any().downcast_ref::<Float64Array>().unwrap().values();
                for i in 0..b.num_rows() {
                    if q[i] > 50 {
                        out[map[keys[i] as usize]] += p[i] * q[i] as f64;
                    }
                }
            }
            Ok(out)
        }
        fn q2(&mut self, a: i64, b: i64) -> Result<(u64, f64)> {
            // Row-group pruning from the footer's min/max statistics.
            let md = self.1.metadata();
            let rgs: Vec<usize> = (0..md.num_row_groups())
                .filter(|&i| match md.row_group(i).column(0).statistics() {
                    Some(Statistics::Int64(s)) => !(*s.max_opt().unwrap() < a || *s.min_opt().unwrap() > b),
                    _ => true,
                })
                .collect();
            let (mut n, mut s) = (0, 0.0);
            for bt in self.reader(&[0, 5], Some(rgs))? {
                let bt = bt?;
                let ts = bt.column(0).as_any().downcast_ref::<Int64Array>().unwrap().values();
                let p = bt.column(1).as_any().downcast_ref::<Float64Array>().unwrap().values();
                for i in 0..bt.num_rows() {
                    if ts[i] >= a && ts[i] <= b {
                        n += 1;
                        s += p[i];
                    }
                }
            }
            Ok((n, s))
        }
    }
}

// ---------------------------------------------------------------- SQLite
mod sq {
    use super::*;
    use rusqlite::{Connection, OpenFlags, params};
    pub fn write(p: &Path, t: &Table) -> Result<()> {
        let mut c = Connection::open(p)?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA cache_size=-262144;
             CREATE TABLE category(code INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE region(code INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE sales(ts INTEGER PRIMARY KEY, user INTEGER, category INTEGER, region INTEGER,
               qty INTEGER, price REAL, discount REAL, flags INTEGER, m0 REAL, m1 REAL, m2 REAL, m3 REAL);",
        )?;
        let tx = c.transaction()?;
        {
            let mut st = tx.prepare("INSERT INTO category VALUES (?1, ?2)")?;
            for i in 0..NCAT {
                st.execute(params![i as i64, cat_name(i)])?;
            }
            let mut st = tx.prepare("INSERT INTO region VALUES (?1, ?2)")?;
            for i in 0..NREG {
                st.execute(params![i as i64, region_name(i)])?;
            }
            let mut st = tx.prepare("INSERT INTO sales VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)")?;
            for i in 0..ROWS {
                st.execute(params![
                    t.ts[i], t.user[i], t.cat[i], t.region[i], t.qty[i], t.price[i], t.discount[i] as f64,
                    t.flags[i], t.m[0][i], t.m[1][i], t.m[2][i], t.m[3][i]
                ])?;
            }
        }
        // Covering index so Q1 never touches the wide table rows.
        tx.execute_batch("CREATE INDEX sales_q1 ON sales(qty, category, price);")?;
        tx.commit()?;
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }
    pub struct R(Connection);
    pub fn open(p: &Path) -> Result<Box<dyn Analytics>> {
        let c = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        c.execute_batch("PRAGMA cache_size=-262144; PRAGMA mmap_size=4294967296;")?;
        Ok(Box::new(R(c)))
    }
    impl Analytics for R {
        fn q1(&mut self) -> Result<Q1> {
            let mut out = [0.0; NCAT];
            let mut st = self.0.prepare_cached(
                "SELECT c.name, s.total FROM (SELECT category, total(price*qty) AS total FROM sales
                 WHERE qty > 50 GROUP BY category) s JOIN category c ON c.code = s.category",
            )?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                let name: String = r.get(0)?;
                out[name[9..].parse::<usize>()?] = r.get(1)?;
            }
            Ok(out)
        }
        fn q2(&mut self, a: i64, b: i64) -> Result<(u64, f64)> {
            let mut st = self.0.prepare_cached("SELECT count(*), total(price) FROM sales WHERE ts BETWEEN ?1 AND ?2")?;
            Ok(st.query_row([a, b], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)))?)
        }
    }
}

// ---------------------------------------------------------------- HDF5 column datasets
mod h5 {
    use super::*;
    use hdf5::File;
    use ndarray::Array1;
    pub fn write(p: &Path, t: &Table) -> Result<()> {
        let f = File::with_options().with_fapl(|a| a.libver_latest()).create(p)?;
        macro_rules! col {
            ($name:expr, $v:expr) => {
                f.new_dataset_builder()
                    .chunk(RG)
                    .blosc_zstd(1, true)
                    .with_data(&Array1::from($v.clone()))
                    .create($name)?;
            };
        }
        col!("ts", t.ts);
        col!("user", t.user);
        col!("category", t.cat);
        col!("region", t.region);
        col!("qty", t.qty);
        col!("price", t.price);
        col!("discount", t.discount);
        col!("flags", t.flags);
        for k in 0..4 {
            col!(format!("m{k}").as_str(), t.m[k]);
        }
        // Zone map: first ts of every chunk (ts is sorted).
        let zone: Vec<i64> = t.ts.iter().step_by(RG).copied().collect();
        f.new_dataset_builder().with_data(&Array1::from(zone)).create("ts_zone")?;
        let names: Vec<hdf5::types::VarLenUnicode> = (0..NCAT).map(|i| cat_name(i).parse().unwrap()).collect();
        f.new_dataset_builder().with_data(&Array1::from(names)).create("category_names")?;
        f.flush()?;
        drop(f);
        fsync_file(p)?;
        Ok(())
    }
    pub struct R {
        f: File,
        zone: Vec<i64>,
    }
    pub fn open(p: &Path) -> Result<Box<dyn Analytics>> {
        let f = File::open(p)?;
        let zone = f.dataset("ts_zone")?.read_raw::<i64>()?;
        Ok(Box::new(R { f, zone }))
    }
    impl Analytics for R {
        fn q1(&mut self) -> Result<Q1> {
            let names = self.f.dataset("category_names")?.read_raw::<hdf5::types::VarLenUnicode>()?;
            let map: Vec<usize> = names.iter().map(|n| n.as_str()[9..].parse().unwrap()).collect();
            let (cd, qd, pd) = (self.f.dataset("category")?, self.f.dataset("qty")?, self.f.dataset("price")?);
            let mut out = [0.0; NCAT];
            // Chunk-sized slices keep memory bounded like the other readers.
            for rg in 0..ROWS / RG {
                let r = rg * RG..(rg + 1) * RG;
                let c = cd.read_slice_1d::<u8, _>(r.clone())?;
                let q = qd.read_slice_1d::<i32, _>(r.clone())?;
                let p = pd.read_slice_1d::<f64, _>(r)?;
                for i in 0..RG {
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
            for rg in 0..self.zone.len() {
                let hi = self.zone.get(rg + 1).map(|z| z - 1).unwrap_or(i64::MAX);
                if hi < a || self.zone[rg] > b {
                    continue;
                }
                let r = rg * RG..(rg + 1) * RG;
                let ts = td.read_slice_1d::<i64, _>(r.clone())?;
                let p = pd.read_slice_1d::<f64, _>(r)?;
                for i in 0..RG {
                    if ts[i] >= a && ts[i] <= b {
                        n += 1;
                        s += p[i];
                    }
                }
            }
            Ok((n, s))
        }
    }
}

// ---------------------------------------------------------------- driver
pub fn run(fmt: &str, dir: &Path) -> Result<Metrics> {
    let p = dir.join(format!("sales.{fmt}"));
    let t = generate(); // untimed
    // Oracle (row order, same accumulation order as the readers).
    let mut q1 = [0.0; NCAT];
    for i in 0..ROWS {
        if t.qty[i] > 50 {
            q1[t.cat[i] as usize] += t.price[i] * t.qty[i] as f64;
        }
    }
    let mut s = 0xC0FFEEu64;
    let ranges: Vec<(i64, i64)> = (0..Q2)
        .map(|_| {
            s = splitmix(s);
            let a = t.ts[0] + (s % (ROWS as u64 * 10 - Q2_SPAN as u64)) as i64;
            (a, a + Q2_SPAN)
        })
        .collect();
    let q2: Vec<(u64, f64)> = ranges
        .iter()
        .map(|&(a, b)| {
            let lo = t.ts.partition_point(|&x| x < a);
            let hi = t.ts.partition_point(|&x| x <= b);
            ((hi - lo) as u64, t.price[lo..hi].iter().sum())
        })
        .collect();

    let mut m = Metrics::default();
    let w = Timer::start();
    match fmt {
        "varlit" => vl::write(&p, &t)?,
        "parquet" => pq::write(&p, &t)?,
        "sqlite" => sq::write(&p, &t)?,
        "hdf5" => h5::write(&p, &t)?,
        _ => return Err("format".into()),
    }
    m.set("write_s", w.secs());
    m.set("bytes", dir_bytes(dir));
    drop(t);
    let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 * a.abs().max(1.0);
    for pass in ["cold", "warm"] {
        if pass == "cold" {
            evict(dir);
        }
        let tm = Timer::start();
        let mut r = match fmt {
            "varlit" => vl::open(&p)?,
            "parquet" => pq::open(&p)?,
            "sqlite" => sq::open(&p)?,
            _ => h5::open(&p)?,
        };
        m.set(&format!("{pass}_open_ms"), tm.secs() * 1e3);
        let tm = Timer::start();
        let got = r.q1()?;
        m.set(&format!("{pass}_q1_s"), tm.secs());
        for k in 0..NCAT {
            assert!(close(got[k], q1[k]), "q1 mismatch cat {k}: {} vs {}", got[k], q1[k]);
        }
        let tm = Timer::start();
        let mut res = Vec::new();
        for &(a, b) in &ranges {
            res.push(r.q2(a, b)?);
        }
        m.set(&format!("{pass}_q2_s"), tm.secs());
        for (g, e) in res.iter().zip(&q2) {
            assert!(g.0 == e.0 && close(g.1, e.1), "q2 mismatch {g:?} vs {e:?}");
        }
    }
    m.set("verified", true);
    Ok(m)
}
