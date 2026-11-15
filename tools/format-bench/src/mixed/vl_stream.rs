//! Varlit (stream API)
//! app-built: nothing beyond the block declarations (window t0 stored in each block)
use super::*;
use varlit::{BatchOptions, StreamOptions, StreamingBlocks, VarlitEmpty, varlit_format};

// One block type per record type; fields keep their exact element types
// (struct-of-arrays inside the block, N records per block).
varlit_format! {
    pub format Mixed {
        magic: b"MIXEDACQ";
        version: 1;
        limits {
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
        }
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        blocks {
            variable ABlock(id = 1) {
                w: u64, t0: u64,
                t: Vec<u64>, a: Vec<i8>, f: Vec<bool>, x: Vec<u16>,
            }
            variable BBlock(id = 2) {
                w: u64, t0: u64,
                t: Vec<u64>, x: Vec<f32>, status: Vec<u16>,
            }
            variable CBlock(id = 3) {
                w: u64, t0: u64,
                t: Vec<u64>, code: Vec<u32>, msg: Vec<String>,
            }
        }
    }
}
pub fn to_a(b: &BA) -> ABlock {
    ABlock {
        w: b.w,
        t0: b.t0,
        t: b.recs.iter().map(|r| r.t).collect(),
        a: b.recs.iter().flat_map(|r| r.a).collect(),
        f: b.recs.iter().flat_map(|r| r.f).collect(),
        x: b.recs.iter().flat_map(|r| r.x).collect(),
    }
}
pub fn to_b(b: &BB) -> BBlock {
    BBlock {
        w: b.w,
        t0: b.t0,
        t: b.recs.iter().map(|r| r.t).collect(),
        x: b.recs.iter().flat_map(|r| r.x).collect(),
        status: b.recs.iter().map(|r| r.status).collect(),
    }
}
pub fn to_c(b: &BC) -> CBlock {
    CBlock {
        w: b.w,
        t0: b.t0,
        t: b.recs.iter().map(|r| r.t).collect(),
        code: b.recs.iter().map(|r| r.code).collect(),
        msg: b.recs.iter().map(|r| r.msg.clone()).collect(),
    }
}
pub fn sink_a(s: &mut Sink, v: &ABlock) {
    s.a(
        v.w,
        v.t0,
        (0..v.t.len()).map(|k| RecA {
            t: v.t[k],
            a: v.a[k * 10..k * 10 + 10].try_into().unwrap(),
            f: v.f[k * 3..k * 3 + 3].try_into().unwrap(),
            x: v.x[k * 500..k * 500 + 500].try_into().unwrap(),
        }),
    );
}
pub fn sink_b(s: &mut Sink, v: &BBlock) {
    sink_b_parts(s, v.w, v.t0, &v.t, &v.x, &v.status);
}
pub fn sink_b_parts(s: &mut Sink, w: u64, t0: u64, t: &[u64], x: &[f32], status: &[u16]) {
    s.b(
        w,
        t0,
        (0..t.len()).map(|k| RecB {
            t: t[k],
            x: x[k * 32..k * 32 + 32].try_into().unwrap(),
            status: status[k],
        }),
    );
}
pub fn sink_c(s: &mut Sink, v: &CBlock) {
    sink_c_parts(s, v.w, v.t0, &v.t, &v.code, &v.msg);
}
pub fn sink_c_parts(s: &mut Sink, w: u64, t0: u64, t: &[u64], code: &[u32], msg: &[String]) {
    s.c(
        w,
        t0,
        t.iter().zip(code).zip(msg).map(|((&t, &code), msg)| RecC {
            t,
            code,
            msg: msg.clone(),
        }),
    );
}

pub struct W(MixedStreamWriter);
pub fn create(p: &Path) -> Result<Box<dyn AcqWriter>> {
    let mut w = Mixed::create_stream_writer(p, StreamOptions::default())?;
    w.sync()?;
    Ok(Box::new(W(w)))
}
impl AcqWriter for W {
    fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
        let (a, b, c) = (&a, &b, &c);
        self.0.push_a_block(&to_a(a))?;
        self.0.push_b_block(&to_b(b))?;
        self.0.push_c_block(&to_c(c))?;
        let _ = BatchOptions::default();
        Ok(())
    }
    fn publish(&mut self, _durable: bool) -> Result<()> {
        // Stream followers advance on the sidecar generation `sync`
        // publishes; `flush` alone is not visible to them, so meeting the
        // freshness bound needs a sync every publication.
        self.0.sync()?;
        Ok(())
    }
}

pub struct R {
    r: MixedStreamReader,
    a: Option<StreamingBlocks<ABlock>>,
    b: Option<StreamingBlocks<BBlock>>,
    c: Option<StreamingBlocks<CBlock>>,
    // Caller-owned decode targets, reused for every record.
    oa: ABlock,
    ob: BBlock,
    oc: CBlock,
}
pub fn open(p: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
    let r = Mixed::open_stream_reader(p, vl_stream(StreamOptions::default()))?;
    let a = if want[0] { Some(r.a_blocks()?) } else { None };
    let b = if want[1] { Some(r.b_blocks()?) } else { None };
    let c = if want[2] { Some(r.c_blocks()?) } else { None };
    Ok(Box::new(R {
        r,
        a,
        b,
        c,
        oa: ABlock::empty(),
        ob: BBlock::empty(),
        oc: CBlock::empty(),
    }))
}
impl AcqReader for R {
    fn poll(&mut self, s: &mut Sink) -> Result<bool> {
        // follow_blocks follows the reader each call: repeat the pass until
        // no later cursor advanced, so every cursor shares one generation.
        loop {
            let mut later = 0;
            let mut first = true;
            if let Some(c) = &mut self.a {
                self.r.follow_blocks(c)?;
                first = false;
            }
            if let Some(c) = &mut self.b {
                let n = self.r.follow_blocks(c)?;
                if !first {
                    later += n;
                }
                first = false;
            }
            if let Some(c) = &mut self.c {
                let n = self.r.follow_blocks(c)?;
                if !first {
                    later += n;
                }
            }
            if later == 0 {
                break;
            }
        }
        let mut got = false;
        if let Some(c) = &mut self.a {
            while c.next_into(&mut self.oa)? {
                sink_a(s, &self.oa);
                got = true;
            }
        }
        if let Some(c) = &mut self.b {
            while c.next_into(&mut self.ob)? {
                sink_b(s, &self.ob);
                got = true;
            }
        }
        if let Some(c) = &mut self.c {
            while c.next_into(&mut self.oc)? {
                sink_c(s, &self.oc);
                got = true;
            }
        }
        Ok(got)
    }
}
