//! Varlit: column chunks as keyed records (`key = [col, rg]`) in a native
//! append file, raw little-endian values, plus a per-chunk ts zone map record
//! and the category dictionary. The writer buffers arriving rows up to its
//! chunk size. Candidates: rows per chunk, zstd or no compression.
//!
//! The native file has no on-disk key index. Each chunk is written once, row
//! group by row group and column by column, so the reader lists the chunk
//! record entries in file order (the resident directory, nothing decoded) and
//! finds chunk `(col, rg)` at entry `rg * 12 + col`; every decoded chunk is
//! checked against the key it was looked up by. Zones and the dictionary are
//! small and are read once at open.
//! app-built: columnar chunking, value encoding, zone map, row buffering,
//! (col, rg) -> record lookup (write order)
use super::*;

pub const CANDIDATES: &[&str] = &[
    "c131072-zstd",
    "c131072-raw",
    "c1048576-zstd",
    "c1048576-raw",
];

macro_rules! sales_format {
    ($m:ident, $name:ident, $magic:literal, $($comp:tt)*) => {
        #[allow(dead_code)]
        pub mod $m {
            use varlit::varlit_format;
            varlit_format! {
                pub format $name {
                    magic: $magic;
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
                    $($comp)*
                    blocks {
                        variable Chunk(id = 1, key = [col, rg]) {
                            col: u8,
                            rg: u16,
                            data: Vec<u8>,
                        }
                        fixed Zone(id = 2, key = [rg], key_domain = [rg = 0..4096]) {
                            rg: u16,
                            ts_min: i64,
                            ts_max: i64,
                        }
                        variable Dict(id = 3, key = [col], key_domain = [col = 0..12]) {
                            col: u8,
                            names: Vec<String>,
                        }
                    }
                }
            }
        }
    };
}
sales_format!(zstd, SalesZ, b"SALESCLZ", compression: variable_blocks(zstd, level = fast, header = record_explicit, min_len = 1024, only_if_smaller = true, max_len = 67108864););
sales_format!(raw, SalesR, b"SALESCLR",);

fn le<T: Copy, const W: usize>(v: &[T], to: fn(T) -> [u8; W]) -> Vec<u8> {
    let mut o = Vec::with_capacity(v.len() * W);
    for &x in v {
        o.extend_from_slice(&to(x));
    }
    o
}
fn columns(t: &Batch) -> [Vec<u8>; 12] {
    [
        le(&t.ts, i64::to_le_bytes),
        le(&t.user, i64::to_le_bytes),
        t.cat.clone(),
        t.region.clone(),
        le(&t.qty, i32::to_le_bytes),
        le(&t.price, f64::to_le_bytes),
        le(&t.discount, f32::to_le_bytes),
        t.flags.clone(),
        le(&t.m[0], f64::to_le_bytes),
        le(&t.m[1], f64::to_le_bytes),
        le(&t.m[2], f64::to_le_bytes),
        le(&t.m[3], f64::to_le_bytes),
    ]
}

// The two candidate formats share the adapter through this macro.
macro_rules! adapter {
    ($m:ident, $fm:ident, $F:ident, $W:ident) => {
        pub mod $m {
            use super::super::*;
            use super::columns;
            use super::$fm as f;
            use f::{Chunk, Dict, DictKey, Zone, ZoneKey, $F};
            use varlit::{RecordIndexEntry, VarlitReader};
            pub struct W {
                w: f::$W,
                buf: Batch,
                chunk: usize,
                rg: usize,
            }
            pub fn create(p: &Path, chunk: usize) -> Result<Box<dyn Writer>> {
                Ok(Box::new(W {
                    w: f::$W::from_inner(vl_write($F::spec()).create_writer(p)?)?,
                    buf: Batch::default(),
                    chunk,
                    rg: 0,
                }))
            }
            impl W {
                fn flush(&mut self) -> Result<()> {
                    if self.buf.len() == 0 {
                        return Ok(());
                    }
                    let rg = self.rg as u16;
                    let cols = columns(&self.buf);
                    for (c, data) in cols.into_iter().enumerate() {
                        self.w.push_chunk(&Chunk {
                            col: c as u8,
                            rg,
                            data,
                        })?;
                    }
                    self.w.push_zone(&Zone {
                        key: ZoneKey::from_values(rg)?,
                        ts_min: self.buf.ts[0],
                        ts_max: *self.buf.ts.last().unwrap(),
                    })?;
                    self.rg += 1;
                    self.buf.clear();
                    Ok(())
                }
            }
            impl Writer for W {
                fn append(&mut self, b: &Batch) -> Result<()> {
                    let mut a = 0;
                    while a < b.len() {
                        let take = (self.chunk - self.buf.len()).min(b.len() - a);
                        self.buf.extend(b, a, a + take);
                        a += take;
                        if self.buf.len() == self.chunk {
                            self.flush()?;
                        }
                    }
                    Ok(())
                }
                fn finish(&mut self) -> Result<()> {
                    self.flush()?;
                    self.w.push_dict(&Dict {
                        key: DictKey::from_values(2)?,
                        names: (0..NCAT).map(cat_name).collect(),
                    })?;
                    self.w.push_dict(&Dict {
                        key: DictKey::from_values(3)?,
                        names: (0..NREG).map(region_name).collect(),
                    })?;
                    self.w.sync()?;
                    Ok(())
                }
            }
            pub struct R {
                r: VarlitReader,
                /// Chunk record entries in file order: `(col, rg)` is entry
                /// `rg * 12 + col`.
                chunks: Vec<RecordIndexEntry>,
                zones: Vec<(i64, i64)>,
                scratch: Vec<u8>,
            }
            pub fn open(p: &Path) -> Result<Box<dyn Analytics>> {
                let r = vl_spec($F::spec()).open_reader(p)?;
                let mut decoded = Vec::new();
                r.file().decode_blocks_into::<Zone>(&mut decoded)?;
                let mut zones = vec![None; decoded.len()];
                for z in decoded {
                    let rg = z.key.values() as usize;
                    *zones.get_mut(rg).ok_or("zone out of range")? = Some((z.ts_min, z.ts_max));
                }
                let zones = zones
                    .into_iter()
                    .map(|z| z.ok_or("missing zone"))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let mut chunks = Vec::new();
                r.file().block_entries_into::<Chunk>(&mut chunks)?;
                if chunks.len() != zones.len() * 12 {
                    return Err("chunk count does not match the zones".into());
                }
                Ok(Box::new(R {
                    r,
                    chunks,
                    zones,
                    scratch: Vec::new(),
                }))
            }
            impl R {
                fn col(&mut self, c: u8, rg: usize, out: &mut Chunk) -> Result<()> {
                    let e = self
                        .chunks
                        .get(rg * 12 + c as usize)
                        .ok_or("missing chunk")?;
                    self.r.file().decode_block_into(e, &mut self.scratch, out)?;
                    if (out.col, out.rg as usize) != (c, rg) {
                        return Err("chunk entry names another chunk".into());
                    }
                    Ok(())
                }
                fn dict(&self, col: u8) -> Result<Dict> {
                    let dicts = self.r.file().keyed_blocks::<Dict>()?;
                    got(|o| dicts.get(&DictKey::from_values(col)?, o), "dict")
                }
            }
            impl Analytics for R {
                fn q1(&mut self) -> Result<Q1> {
                    let names = self.dict(2)?.names;
                    let map: Vec<usize> = names.iter().map(|n| n[9..].parse().unwrap()).collect();
                    let mut out = [0.0; NCAT];
                    let (mut c, mut q, mut p) = (
                        <Chunk as varlit::VarlitEmpty>::empty(),
                        <Chunk as varlit::VarlitEmpty>::empty(),
                        <Chunk as varlit::VarlitEmpty>::empty(),
                    );
                    for rg in 0..self.zones.len() {
                        self.col(2, rg, &mut c)?;
                        self.col(4, rg, &mut q)?;
                        self.col(5, rg, &mut p)?;
                        for i in 0..c.data.len() {
                            let qty =
                                i32::from_le_bytes(q.data[i * 4..i * 4 + 4].try_into().unwrap());
                            if qty > 50 {
                                let price = f64::from_le_bytes(
                                    p.data[i * 8..i * 8 + 8].try_into().unwrap(),
                                );
                                out[map[c.data[i] as usize]] += price * qty as f64;
                            }
                        }
                    }
                    Ok(out)
                }
                fn q2(&mut self, a: i64, b: i64) -> Result<(u64, f64)> {
                    let (mut n, mut s) = (0, 0.0);
                    let (mut ts, mut p) = (
                        <Chunk as varlit::VarlitEmpty>::empty(),
                        <Chunk as varlit::VarlitEmpty>::empty(),
                    );
                    for rg in 0..self.zones.len() {
                        let (lo, hi) = self.zones[rg];
                        if hi < a || lo > b {
                            continue;
                        }
                        self.col(0, rg, &mut ts)?;
                        self.col(5, rg, &mut p)?;
                        for i in 0..ts.data.len() / 8 {
                            let t =
                                i64::from_le_bytes(ts.data[i * 8..i * 8 + 8].try_into().unwrap());
                            if t >= a && t <= b {
                                n += 1;
                                s += f64::from_le_bytes(
                                    p.data[i * 8..i * 8 + 8].try_into().unwrap(),
                                );
                            }
                        }
                    }
                    Ok((n, s))
                }
            }
        }
    };
}
adapter!(za, zstd, SalesZ, SalesZWriter);
adapter!(ra, raw, SalesR, SalesRWriter);

fn chunk_of(cand: &str) -> usize {
    cand.trim_start_matches('c')
        .split('-')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}
pub fn create(p: &Path, cand: &str) -> Result<Box<dyn Writer>> {
    if cand.ends_with("-raw") {
        ra::create(p, chunk_of(cand))
    } else {
        za::create(p, chunk_of(cand))
    }
}
pub fn open(p: &Path, cand: &str) -> Result<Box<dyn Analytics>> {
    if cand.ends_with("-raw") {
        ra::open(p)
    } else {
        za::open(p)
    }
}
