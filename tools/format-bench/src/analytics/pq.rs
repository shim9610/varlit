//! Parquet: one file written by `ArrowWriter` (it buffers rows into row
//! groups), dictionary-encoded category/region, delta-encoded ts,
//! byte-stream-split floats, chunk statistics for ts pruning.
//! Candidates: rows per row group, zstd or no compression.
//! app-built: nothing
use super::*;
use arrow_array::types::Int8Type;
use arrow_array::{
    Array, ArrayRef, DictionaryArray, Float32Array, Float64Array, Int8Array, Int32Array,
    Int64Array, RecordBatch, StringArray, UInt8Array,
};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder,
};
use parquet::arrow::{ArrowWriter, ProjectionMask};
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::statistics::Statistics;
use parquet::schema::types::ColumnPath;
use std::sync::Arc;

pub const CANDIDATES: &[&str] = &[
    "rg131072-zstd",
    "rg131072-none",
    "rg1048576-zstd",
    "rg1048576-none",
];

fn schema() -> Arc<Schema> {
    let dt = DataType::Dictionary(Box::new(DataType::Int8), Box::new(DataType::Utf8));
    Arc::new(Schema::new(vec![
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
    ]))
}
fn dict(codes: &[u8], names: Vec<String>) -> ArrayRef {
    let keys = Int8Array::from_iter_values(codes.iter().map(|&c| c as i8));
    Arc::new(
        DictionaryArray::<Int8Type>::try_new(keys, Arc::new(StringArray::from(names))).unwrap(),
    )
}

pub struct W {
    w: Option<ArrowWriter<std::fs::File>>,
}
pub fn create(p: &Path, cand: &str) -> Result<Box<dyn Writer>> {
    let rg: usize = cand
        .trim_start_matches("rg")
        .split('-')
        .next()
        .unwrap()
        .parse()?;
    let comp = if cand.ends_with("-none") {
        Compression::UNCOMPRESSED
    } else {
        Compression::ZSTD(ZstdLevel::try_new(1)?)
    };
    let cp = |n: &str| ColumnPath::from(n);
    let mut b = WriterProperties::builder()
        .set_compression(comp)
        .set_max_row_group_row_count(Some(rg))
        .set_statistics_enabled(EnabledStatistics::Chunk);
    b = b
        .set_column_dictionary_enabled(cp("ts"), false)
        .set_column_encoding(cp("ts"), Encoding::DELTA_BINARY_PACKED);
    b = b.set_column_dictionary_enabled(cp("user"), false);
    for f in ["price", "discount", "m0", "m1", "m2", "m3"] {
        b = b
            .set_column_dictionary_enabled(cp(f), false)
            .set_column_encoding(cp(f), Encoding::BYTE_STREAM_SPLIT);
    }
    let w = ArrowWriter::try_new(std::fs::File::create(p)?, schema(), Some(b.build()))?;
    Ok(Box::new(W { w: Some(w) }))
}
impl Writer for W {
    fn append(&mut self, t: &Batch) -> Result<()> {
        let cols: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(t.ts.clone())),
            Arc::new(Int64Array::from(t.user.clone())),
            dict(&t.cat, (0..NCAT).map(cat_name).collect()),
            dict(&t.region, (0..NREG).map(region_name).collect()),
            Arc::new(Int32Array::from(t.qty.clone())),
            Arc::new(Float64Array::from(t.price.clone())),
            Arc::new(Float32Array::from(t.discount.clone())),
            Arc::new(UInt8Array::from(t.flags.clone())),
            Arc::new(Float64Array::from(t.m[0].clone())),
            Arc::new(Float64Array::from(t.m[1].clone())),
            Arc::new(Float64Array::from(t.m[2].clone())),
            Arc::new(Float64Array::from(t.m[3].clone())),
        ];
        self.w
            .as_mut()
            .unwrap()
            .write(&RecordBatch::try_new(schema(), cols)?)?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.w.take().unwrap().into_inner()?.sync_all()?;
        Ok(())
    }
}

pub struct R(PathBuf, ArrowReaderMetadata);
pub fn open(p: &Path, _cand: &str) -> Result<Box<dyn Analytics>> {
    let f = std::fs::File::open(p)?;
    let m = ArrowReaderMetadata::load(&f, Default::default())?;
    Ok(Box::new(R(p.to_path_buf(), m)))
}
impl R {
    fn reader(&self, cols: &[usize], rgs: Option<Vec<usize>>) -> Result<ParquetRecordBatchReader> {
        let f = std::fs::File::open(&self.0)?;
        let mask = ProjectionMask::roots(self.1.parquet_schema(), cols.iter().copied());
        let mut b = ParquetRecordBatchReaderBuilder::new_with_metadata(f, self.1.clone())
            .with_projection(mask)
            .with_batch_size(BATCH);
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
            let c = b
                .column(0)
                .as_any()
                .downcast_ref::<DictionaryArray<Int8Type>>()
                .unwrap();
            let vals = c.values().as_any().downcast_ref::<StringArray>().unwrap();
            let map: Vec<usize> = (0..vals.len())
                .map(|i| vals.value(i)[9..].parse().unwrap())
                .collect();
            let keys = c.keys().values();
            let q = b
                .column(1)
                .as_any()
                .downcast_ref::<Int32Array>()
                .unwrap()
                .values();
            let p = b
                .column(2)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .values();
            for i in 0..b.num_rows() {
                if q[i] > 50 {
                    out[map[keys[i] as usize]] += p[i] * q[i] as f64;
                }
            }
        }
        Ok(out)
    }
    fn q2(&mut self, a: i64, b: i64) -> Result<(u64, f64)> {
        let md = self.1.metadata();
        let rgs: Vec<usize> = (0..md.num_row_groups())
            .filter(|&i| match md.row_group(i).column(0).statistics() {
                Some(Statistics::Int64(s)) => {
                    !(*s.max_opt().unwrap() < a || *s.min_opt().unwrap() > b)
                }
                _ => true,
            })
            .collect();
        let (mut n, mut s) = (0, 0.0);
        for bt in self.reader(&[0, 5], Some(rgs))? {
            let bt = bt?;
            let ts = bt
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .values();
            let p = bt
                .column(1)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .values();
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
