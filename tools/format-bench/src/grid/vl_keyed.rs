//! Varlit keyed records: one record per chunk keyed by its linear chunk id
//! (disk key index), raw little-endian values. Every arriving step is
//! published with `sync()`; a time-deep chunk is completed by reading the
//! newest version through a following reader and appending a new version.
//! Readers look up each chunk by key.
//! app-built: chunk id keying
use super::*;
use varlit::{BatchOptions, DiskIndexOptions, VarlitEmpty, varlit_format};

varlit_format! {
    pub format GridKeyed {
        magic: b"GRIDKEY1";
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
        blocks {
            variable GChunk(id = 1, key = [id], key_index = disk) {
                id: u64,
                data: Vec<u8>,
            }
        }
    }
}
fn opts() -> DiskIndexOptions {
    vl_dio(DiskIndexOptions {
        cache_bytes: 64 << 20,
        ..DiskIndexOptions::default()
    })
}

pub struct Wr {
    w: GridKeyedIndexedWriter,
    r: Option<GridKeyedIndexedReader>,
    p: PathBuf,
    ck: Ck,
}
pub fn create(p: &Path, ck: Ck) -> Result<Box<dyn GridWrite>> {
    Ok(Box::new(Wr {
        w: GridKeyed::create_indexed_writer(p, opts())?,
        r: None,
        p: p.to_path_buf(),
        ck,
    }))
}
impl GridWrite for Wr {
    fn step(&mut self, t: usize, step: &[f32]) -> Result<()> {
        let ck = self.ck;
        let fresh = t % ck.c[0] == 0;
        let mut out = Vec::with_capacity(ck.per_tchunk());
        let mut cur = GChunk::empty();
        for id in ck.ids_of_step(t) {
            let mut data = vec![0u8; CB];
            if !fresh {
                let r = self.r.as_ref().ok_or("no reader")?;
                if !r.get_g_chunk(&(id as u64), &mut cur)? {
                    return Err("missing chunk".into());
                }
                data.copy_from_slice(&cur.data);
            }
            ck.put_step(step, t, id, &mut data);
            out.push(GChunk {
                id: id as u64,
                data,
            });
        }
        self.w
            .push_g_chunks(out, BatchOptions::default())
            .map_err(|f| f.source)?;
        self.w.sync()?;
        if ck.c[0] > 1 {
            match self.r.as_mut() {
                Some(r) => {
                    r.follow()?;
                }
                None => self.r = Some(GridKeyed::open_indexed_reader(&self.p, opts())?),
            }
        }
        Ok(())
    }
    fn finish(mut self: Box<Self>) -> Result<()> {
        self.w.compact_index()?;
        Ok(())
    }
}

pub struct Rd {
    r: GridKeyedIndexedReader,
    ck: Ck,
    cur: GChunk,
}
pub fn open(p: &Path, ck: Ck) -> Result<Box<dyn GridRead>> {
    Ok(Box::new(Rd {
        r: GridKeyed::open_indexed_reader(p, opts())?,
        ck,
        cur: GChunk::empty(),
    }))
}
impl GridRead for Rd {
    fn slab(&mut self, start: Ix, count: Ix, out: &mut Vec<f32>) -> Result<()> {
        prepare(&count, out);
        let ck = self.ck;
        let mut runs = Vec::new();
        ck.runs(&start, &count, |id0, n| runs.push((id0, n)));
        for (id0, n) in runs {
            for id in id0..id0 + n {
                if !self.r.get_g_chunk(&(id as u64), &mut self.cur)? {
                    return Err("missing chunk".into());
                }
                ck.copy_out(&start, &count, id, &self.cur.data, out);
            }
        }
        Ok(())
    }
}
