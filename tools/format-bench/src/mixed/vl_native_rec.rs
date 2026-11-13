//! Varlit native, record sequences (same log and reader as `vl_native.rs`):
//! A and B records go into their window blocks as `Vec<RecA>` / `Vec<RecB>`:
//! the producer's records are moved in, no per-field split, no copy.
//! app-built: nothing
use super::vl_native::Fwd;
use super::*;
use varlit::VarlitEmpty;
use varlit::{VarlitBlock, VarlitFile, varlit_format};
use vl::CBlock as VC;
type Outs = (AWin, BWin, VC);

#[derive(Clone, Debug, PartialEq, varlit::VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable")]
pub struct AWin {
    #[varlit(field_id = 1)]
    pub w: u64,
    #[varlit(field_id = 2)]
    pub t0: u64,
    #[varlit(field_id = 3)]
    pub recs: Vec<RecA>,
}
#[derive(Clone, Debug, PartialEq, varlit::VarlitBlock)]
#[varlit(id = 2, version = 1, kind = "variable")]
pub struct BWin {
    #[varlit(field_id = 1)]
    pub w: u64,
    #[varlit(field_id = 2)]
    pub t0: u64,
    #[varlit(field_id = 3)]
    pub recs: Vec<RecB>,
}
varlit_format! {
    pub struct MixedRec {
        magic: b"MIXEDREC";
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
        blocks: [AWin, BWin, VC];
    }
}

pub struct W(VarlitFile);
pub fn create(p: &Path) -> Result<Box<dyn AcqWriter>> {
    let mut f = VarlitFile::create(MixedRec::spec(), p)?;
    f.commit_durable()?;
    Ok(Box::new(W(f)))
}
impl AcqWriter for W {
    fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
        let t = mono_ns();
        let c = vl::to_c(&c);
        let (a, b) = (
            AWin {
                w: a.w,
                t0: a.t0,
                recs: a.recs,
            },
            BWin {
                w: b.w,
                t0: b.t0,
                recs: b.recs,
            },
        );
        let t2 = mono_ns();
        self.0.push_info(&a)?;
        self.0.push_info(&b)?;
        self.0.push_info(&c)?;
        if std::env::var_os("FB_PROFILE").is_some() {
            eprintln!("PROF convert_ns={} push_ns={}", t2 - t, mono_ns() - t2);
        }
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
        f: VarlitFile::open_readonly_lazy(vl_spec(MixedRec::spec()), p)?,
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
            if e.block_id == AWin::ID && want[0] {
                f.decode_block_into(e, scratch, &mut outs.0)?;
                s.a(outs.0.w, outs.0.t0, outs.0.recs.iter().copied());
            } else if e.block_id == BWin::ID && want[1] {
                f.decode_block_into(e, scratch, &mut outs.1)?;
                s.b(outs.1.w, outs.1.t0, outs.1.recs.iter().copied());
            } else if e.block_id == VC::ID && want[2] {
                f.decode_block_into(e, scratch, &mut outs.2)?;
                vl::sink_c(s, &outs.2);
            }
            Ok(())
        })
    }
}
