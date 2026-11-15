//! Parquet: files are immutable and readable only once their footer is
//! written, so each arriving step becomes its own file (temp + rename +
//! directory fsync): one row per chunk, one data page per chunk and a page
//! index, so a row selection fetches exactly the needed chunk pages. A chunk
//! deeper than one step would need rewriting a closed file: unsupported.
//! app-built: file per time step, step -> file mapping
use super::*;
use arrow_array::{Array, ArrayRef, FixedSizeBinaryArray, RecordBatch, UInt32Array};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection,
    RowSelectionPolicy, RowSelector,
};
use parquet::arrow::{ArrowWriter, ProjectionMask};
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use std::sync::Arc;

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("chunk", DataType::UInt32, false),
        Field::new("data", DataType::FixedSizeBinary(CB as i32), false),
    ]))
}

pub struct Wr {
    dir: PathBuf,
    ck: Ck,
    buf: Vec<u8>,
}
pub fn create(p: &Path, ck: Ck) -> Result<Box<dyn GridWrite>> {
    if ck.c[0] > 1 {
        return Err("parquet: a closed file cannot be rewritten to complete a time-deep chunk (unsupported)".into());
    }
    std::fs::create_dir_all(p)?;
    Ok(Box::new(Wr {
        dir: p.to_path_buf(),
        ck,
        buf: Vec::new(),
    }))
}
impl GridWrite for Wr {
    fn step(&mut self, t: usize, step: &[f32]) -> Result<()> {
        let ck = self.ck;
        let ids = ck.ids_of_step(t);
        self.buf.clear();
        self.buf.resize(ids.len() * CB, 0);
        for (j, id) in ids.clone().enumerate() {
            ck.put_step(step, t, id, &mut self.buf[j * CB..(j + 1) * CB]);
        }
        let props = WriterProperties::builder()
            .set_compression(parquet::basic::Compression::UNCOMPRESSED)
            .set_dictionary_enabled(false)
            .set_max_row_group_row_count(Some(ids.len()))
            .set_data_page_row_count_limit(1)
            .set_write_batch_size(1)
            .set_data_page_size_limit(1)
            .set_statistics_enabled(EnabledStatistics::Page)
            .build();
        let tmp = self.dir.join("tmp.parquet");
        let mut w = ArrowWriter::try_new(std::fs::File::create(&tmp)?, schema(), Some(props))?;
        let idc = UInt32Array::from_iter_values(ids.map(|i| i as u32));
        let data =
            FixedSizeBinaryArray::try_new(CB as i32, std::mem::take(&mut self.buf).into(), None)?;
        let cols: Vec<ArrayRef> = vec![Arc::new(idc), Arc::new(data)];
        w.write(&RecordBatch::try_new(schema(), cols)?)?;
        w.into_inner()?.sync_all()?;
        std::fs::rename(&tmp, self.dir.join(format!("step_{t:05}.parquet")))?;
        fsync_dir(&self.dir)?;
        Ok(())
    }
    fn finish(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

pub struct Rd {
    files: Vec<(std::fs::File, ArrowReaderMetadata)>,
    ck: Ck,
}
pub fn open(p: &Path, ck: Ck) -> Result<Box<dyn GridRead>> {
    let mut files = Vec::new();
    for t in 0..shape()[0] {
        let f = std::fs::File::open(p.join(format!("step_{t:05}.parquet")))?;
        let meta = ArrowReaderMetadata::load(
            &f,
            ArrowReaderOptions::new()
                .with_page_index_policy(parquet::file::metadata::PageIndexPolicy::Required),
        )?;
        files.push((f, meta));
    }
    Ok(Box::new(Rd { files, ck }))
}
impl GridRead for Rd {
    fn slab(&mut self, start: Ix, count: Ix, out: &mut Vec<f32>) -> Result<()> {
        prepare(&count, out);
        let ck = self.ck;
        let per = ck.per_tchunk();
        let mut runs = Vec::new();
        ck.runs(&start, &count, |id0, n| runs.push((id0, n)));
        // Group runs by step file (runs never cross a step).
        let mut i = 0;
        while i < runs.len() {
            let t = runs[i].0 / per;
            let (mut sel, mut pos, mut ids) = (Vec::new(), 0, Vec::new());
            while i < runs.len() && runs[i].0 / per == t {
                let (id0, n) = runs[i];
                let lo = id0 % per;
                sel.push(RowSelector::skip(lo - pos));
                sel.push(RowSelector::select(n));
                pos = lo + n;
                ids.extend(id0..id0 + n);
                i += 1;
            }
            sel.push(RowSelector::skip(per - pos));
            let (f, meta) = &self.files[t];
            let mask = ProjectionMask::roots(meta.parquet_schema(), [1]);
            let r =
                ParquetRecordBatchReaderBuilder::new_with_metadata(f.try_clone()?, meta.clone())
                    .with_projection(mask)
                    .with_row_selection(RowSelection::from(sel))
                    .with_row_selection_policy(RowSelectionPolicy::Selectors)
                    .with_batch_size(per)
                    .build()?;
            let mut k = 0;
            for b in r {
                let b = b?;
                let d = b
                    .column(0)
                    .as_any()
                    .downcast_ref::<FixedSizeBinaryArray>()
                    .unwrap();
                for j in 0..d.len() {
                    ck.copy_out(&start, &count, ids[k], d.value(j), out);
                    k += 1;
                }
            }
            if k != ids.len() {
                return Err("missing chunk".into());
            }
        }
        Ok(())
    }
}
