//! Varlit keyed storage: one record per chunk keyed by its linear chunk id,
//! raw little-endian values, in a format that declares
//! `keyed_storage: in_place`. Every arriving step is published with `sync()`;
//! a time-deep chunk is completed by reading it by key and putting it back,
//! which replaces it in place. The library finds chunks by key: no
//! application lookup, and the file keeps one version of each chunk.
use super::*;
use varlit::{VarlitEmpty, VarlitReader, varlit_format};

varlit_format! {
    pub format GridHeap {
        magic: b"GRIDHEP1";
        version: 1;
        limits {
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
        }
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        keyed_storage: in_place;
        blocks {
            variable HChunk(id = 1, key = [id]) {
                id: u64,
                data: Vec<u8>,
            }
        }
    }
}

pub struct Wr {
    w: GridHeapWriter,
    scratch: Vec<u8>,
    cur: HChunk,
    ck: Ck,
}
pub fn create(p: &Path, ck: Ck) -> Result<Box<dyn GridWrite>> {
    Ok(Box::new(Wr {
        w: GridHeap::create_writer(p)?,
        scratch: Vec::new(),
        cur: HChunk {
            id: 0,
            data: vec![0u8; CB],
        },
        ck,
    }))
}
impl GridWrite for Wr {
    fn step(&mut self, t: usize, step: &[f32]) -> Result<()> {
        let ck = self.ck;
        let fresh = t.is_multiple_of(ck.c[0]);
        for id in ck.ids_of_step(t) {
            if fresh {
                self.cur.data.clear();
                self.cur.data.resize(CB, 0);
            } else if !self
                .w
                .get_h_chunk(&(id as u64), &mut self.scratch, &mut self.cur)?
            {
                return Err("missing chunk".into());
            }
            self.cur.id = id as u64;
            ck.put_step(step, t, id, &mut self.cur.data);
            self.w.put_h_chunk(&self.cur)?;
        }
        self.w.sync()?;
        Ok(())
    }
    fn finish(self: Box<Self>) -> Result<()> {
        if std::env::var_os("FB_KEYED_STATS").is_some() {
            eprintln!(
                "keyed stats {:?}",
                self.w.inner().file().keyed_storage_stats()?
            );
        }
        Ok(())
    }
}

pub struct Rd {
    r: VarlitReader,
    scratch: Vec<u8>,
    ck: Ck,
    cur: HChunk,
}
pub fn open(p: &Path, ck: Ck) -> Result<Box<dyn GridRead>> {
    Ok(Box::new(Rd {
        r: vl_spec(GridHeap::spec()).open_reader(p)?,
        scratch: Vec::new(),
        ck,
        cur: HChunk::empty(),
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
                if !self
                    .r
                    .get_keyed(&(id as u64), &mut self.scratch, &mut self.cur)?
                {
                    return Err("missing chunk".into());
                }
                ck.copy_out(&start, &count, id, &self.cur.data, out);
            }
        }
        Ok(())
    }
}
