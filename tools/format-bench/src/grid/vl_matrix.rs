//! Varlit matrix: the chunk grid maps onto the 2-D matrix keyspace: scan =
//! (time-chunk, ensemble, variable) chunk, ch = (level, lat, lon) chunk, so
//! chunks adjacent along the last axes are adjacent cells and a run of chunk
//! ids is one run read. The scan axis grows by one time-chunk row group at a
//! time; every arriving step is published with `sync()`. A time-deep chunk is
//! completed by reading the cell back and rewriting it.
//! app-built: chunk <-> matrix key mapping
use super::*;
use varlit::{MatrixKey, VarlitFile, VarlitReader, varlit_format};

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
fn cc(ck: &Ck) -> usize {
    ck.cg[3..].iter().product()
}
fn rows(ck: &Ck) -> usize {
    ck.cg[1] * ck.cg[2]
}
fn spec(ck: &Ck) -> varlit::FormatSpec {
    GridFmt::spec().with_growing_matrix_dimension("cs", rows(ck) as u64)
}
fn key(ck: &Ck, id: usize) -> MatrixKey {
    MatrixKey::new((id / cc(ck)) as u64, (id % cc(ck)) as u64)
}

pub struct Wr {
    w: VarlitFile,
    ck: Ck,
    buf: Vec<u8>,
}
pub fn create(p: &Path, ck: Ck) -> Result<Box<dyn GridWrite>> {
    let w = spec(&ck).create_with_dims(
        p,
        varlit::MatrixDimensions::from_pairs([("cs", rows(&ck) as u64), ("cc", cc(&ck) as u64)]),
    )?;
    Ok(Box::new(Wr {
        w,
        ck,
        buf: vec![0; CB],
    }))
}
impl GridWrite for Wr {
    fn step(&mut self, t: usize, step: &[f32]) -> Result<()> {
        let ck = self.ck;
        for id in ck.ids_of_step(t) {
            if t % ck.c[0] == 0 {
                self.buf.fill(0);
            } else {
                self.w
                    .read_matrix_run_bytes_into::<Chunk>(key(&ck, id), &mut self.buf)?;
            }
            ck.put_step(step, t, id, &mut self.buf);
            self.w
                .write_matrix_cell_payload::<Chunk>(key(&ck, id), &self.buf)?;
            self.w.commit_matrix_cell::<Chunk>(key(&ck, id))?;
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
    ck: Ck,
    buf: Vec<u8>,
}
pub fn open(p: &Path, ck: Ck) -> Result<Box<dyn GridRead>> {
    Ok(Box::new(Rd {
        r: vl_spec(spec(&ck)).open_reader(p)?,
        ck,
        buf: Vec::new(),
    }))
}
impl GridRead for Rd {
    fn slab(&mut self, start: Ix, count: Ix, out: &mut Vec<f32>) -> Result<()> {
        prepare(&count, out);
        let ck = self.ck;
        let mut runs = Vec::new();
        ck.runs(&start, &count, |id0, n| runs.push((id0, n)));
        for (id0, n) in runs {
            self.buf.resize(n * CB, 0);
            self.r
                .read_matrix_run_bytes_into::<Chunk>(key(&ck, id0), &mut self.buf)?;
            for (j, ch) in self.buf.chunks_exact(CB).enumerate() {
                ck.copy_out(&start, &count, id0 + j, ch, out);
            }
        }
        Ok(())
    }
}
