//! Varlit native, raw u16 bytes: identical to `vl_native.rs` except the 500 x u16 field is stored as
//! little-endian bytes (Vec<u8>) (a bulk encode path).
//! app-built: nothing
use super::vl_native::Fwd;
use super::*;
use varlit::VarlitEmpty;
use varlit::{VarlitBlock, VarlitFile, varlit_format};
use vl::{BBlock as VB, CBlock as VC};
type Outs = (ARaw, BRaw, CRaw);

varlit_format! {
    pub format MixedRaw {
        magic: b"MIXEDRAW";
        version: 1;
        limits {
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
        }
        endian: little;
        integrity: crc32_with_header;
        index: header_tails;
        commit: transaction_marker(explicit);
        blocks {
            variable ARaw(id = 1) {
                w: u64, t0: u64,
                t: Vec<u64>, a: Vec<i8>, f: Vec<bool>, x: Vec<u8>,
            }
            variable BRaw(id = 2) { w: u64, t0: u64, t: Vec<u64>, x: Vec<f32>, status: Vec<u16>, }
            variable CRaw(id = 3) { w: u64, t0: u64, t: Vec<u64>, code: Vec<u32>, msg: Vec<String>, }
        }
    }
}
fn to_raw(b: &BA) -> ARaw {
    let mut x = Vec::with_capacity(b.recs.len() * 1000);
    for r in &b.recs {
        for v in r.x {
            x.extend_from_slice(&v.to_le_bytes());
        }
    }
    ARaw {
        w: b.w,
        t0: b.t0,
        t: b.recs.iter().map(|r| r.t).collect(),
        a: b.recs.iter().flat_map(|r| r.a).collect(),
        f: b.recs.iter().flat_map(|r| r.f).collect(),
        x,
    }
}
pub struct W(VarlitFile);
pub fn create(p: &Path) -> Result<Box<dyn AcqWriter>> {
    let mut f = VarlitFile::create(MixedRaw::spec(), p)?;
    f.commit_durable()?;
    Ok(Box::new(W(f)))
}
impl AcqWriter for W {
    fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
        let (a, b, c) = (&a, &b, &c);
        self.0.push_info(&to_raw(a))?;
        let (b, c) = (vl::to_b(b), vl::to_c(c));
        self.0.push_info(&BRaw {
            w: b.w,
            t0: b.t0,
            t: b.t,
            x: b.x,
            status: b.status,
        })?;
        self.0.push_info(&CRaw {
            w: c.w,
            t0: c.t0,
            t: c.t,
            code: c.code,
            msg: c.msg,
        })?;
        Ok(())
    }
    fn publish(&mut self, durable: bool) -> Result<()> {
        if durable {
            self.0.commit_durable()?;
        } else {
            self.0.commit()?;
        }
        Ok(())
    }
}
pub struct R {
    f: VarlitFile,
    want: [bool; 3],
    fwd: Fwd,
    scratch: Vec<u8>,
    outs: Outs,
}
pub fn open(p: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
    // Lazy open from the header tail region: nothing is scanned and no record
    // directory is kept.
    Ok(Box::new(R {
        f: VarlitFile::open_readonly_lazy(vl_spec(MixedRaw::spec()), p)?,
        want,
        fwd: Fwd::new(),
        scratch: Vec::new(),
        outs: Outs::empty(),
    }))
}
impl AcqReader for R {
    fn poll(&mut self, s: &mut Sink) -> Result<bool> {
        let (want, scratch, outs) = (self.want, &mut self.scratch, &mut self.outs);
        self.fwd.poll(&mut self.f, |f, e| {
            if e.block_id == ARaw::ID && want[0] {
                f.decode_block_into(e, scratch, &mut outs.0)?;
                let v = &outs.0;
                s.a(
                    v.w,
                    v.t0,
                    (0..v.t.len()).map(|k| RecA {
                        t: v.t[k],
                        a: v.a[k * 10..k * 10 + 10].try_into().unwrap(),
                        f: v.f[k * 3..k * 3 + 3].try_into().unwrap(),
                        x: std::array::from_fn(|j| {
                            u16::from_le_bytes(
                                v.x[k * 1000 + 2 * j..k * 1000 + 2 * j + 2]
                                    .try_into()
                                    .unwrap(),
                            )
                        }),
                    }),
                );
            } else if e.block_id == VB::ID && want[1] {
                f.decode_block_into(e, scratch, &mut outs.1)?;
                let v = &outs.1;
                vl::sink_b_parts(s, v.w, v.t0, &v.t, &v.x, &v.status);
            } else if e.block_id == VC::ID && want[2] {
                f.decode_block_into(e, scratch, &mut outs.2)?;
                let v = &outs.2;
                vl::sink_c_parts(s, v.w, v.t0, &v.t, &v.code, &v.msg);
            }
            Ok(())
        })
    }
}
