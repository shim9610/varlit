//! Varlit keyed records: one record per chunk keyed by its linear chunk id,
//! raw little-endian values, in a native append file. Every arriving step is
//! published with `sync()`; a time-deep chunk is completed by reading its
//! newest version back from the file and appending a new version.
//!
//! The native file has no on-disk key index, so the chunk -> record lookup is
//! application-built. The writer keeps the newest record offset of every
//! chunk (one `u64` per chunk, from each append's `AppendInfo`). A reader
//! lists the chunk record entries in file order (the resident directory,
//! nothing decoded): step `t` appended the chunks of its time-chunk row in id
//! order, so entry `k` is chunk `(k / per / c0) * per + k % per`, and the last
//! entry of a chunk is its newest version.
//! app-built: chunk id keying; chunk id -> record lookup (write order)
use super::*;
use varlit::{RecordIndexEntry, VarlitEmpty, VarlitReader, varlit_format};

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
            variable GChunk(id = 1, key = [id]) {
                id: u64,
                data: Vec<u8>,
            }
        }
    }
}

fn nchunks(ck: &Ck) -> usize {
    ck.cg[0] * ck.per_tchunk()
}

pub struct Wr {
    w: GridKeyedWriter,
    /// Newest record offset of each chunk, indexed by chunk id.
    at: Vec<u64>,
    scratch: Vec<u8>,
    ck: Ck,
}
pub fn create(p: &Path, ck: Ck) -> Result<Box<dyn GridWrite>> {
    Ok(Box::new(Wr {
        w: GridKeyed::create_writer(p)?,
        at: vec![u64::MAX; nchunks(&ck)],
        scratch: Vec::new(),
        ck,
    }))
}
impl GridWrite for Wr {
    fn step(&mut self, t: usize, step: &[f32]) -> Result<()> {
        let ck = self.ck;
        let fresh = t.is_multiple_of(ck.c[0]);
        let mut cur = GChunk {
            id: 0,
            data: vec![0u8; CB],
        };
        for id in ck.ids_of_step(t) {
            if fresh {
                cur.data.fill(0);
            } else {
                let off = self.at[id];
                if off == u64::MAX {
                    return Err("missing chunk".into());
                }
                self.w
                    .inner()
                    .file()
                    .read_block_at(off, &mut self.scratch, &mut cur)?;
                if cur.id != id as u64 {
                    return Err("chunk offset names another chunk".into());
                }
            }
            cur.id = id as u64;
            ck.put_step(step, t, id, &mut cur.data);
            self.at[id] = self.w.push_g_chunk(&cur)?.record_offset;
        }
        self.w.sync()?;
        Ok(())
    }
    fn finish(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

pub struct Rd {
    r: VarlitReader,
    /// Newest entry of each chunk, indexed by chunk id.
    at: Vec<RecordIndexEntry>,
    scratch: Vec<u8>,
    ck: Ck,
    cur: GChunk,
}
pub fn open(p: &Path, ck: Ck) -> Result<Box<dyn GridRead>> {
    let r = vl_spec(GridKeyed::spec()).open_reader(p)?;
    let mut all = Vec::new();
    r.file().block_entries_into::<GChunk>(&mut all)?;
    let per = ck.per_tchunk();
    if !all.len().is_multiple_of(per) {
        return Err("chunk records do not form whole steps".into());
    }
    let mut at: Vec<Option<RecordIndexEntry>> = vec![None; nchunks(&ck)];
    for (k, e) in all.into_iter().enumerate() {
        let id = (k / per / ck.c[0]) * per + k % per;
        *at.get_mut(id).ok_or("more steps than the shape holds")? = Some(e);
    }
    let at = at
        .into_iter()
        .map(|e| e.ok_or("missing chunk"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(Box::new(Rd {
        r,
        at,
        scratch: Vec::new(),
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
                self.r
                    .file()
                    .decode_block_into(&self.at[id], &mut self.scratch, &mut self.cur)?;
                if self.cur.id != id as u64 {
                    return Err("chunk entry names another chunk".into());
                }
                ck.copy_out(&start, &count, id, &self.cur.data, out);
            }
        }
        Ok(())
    }
}
