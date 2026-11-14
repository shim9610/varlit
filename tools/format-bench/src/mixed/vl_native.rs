//! Varlit native append log: one record per type per burst, explicit
//! transaction markers (`commit` / `commit_durable` with one barrier), header
//! tail region so readers open lazily. Readers walk forward from where they
//! stopped (`record_map_from`), in bounded batches.
//! app-built: nothing
use super::*;
use varlit::VarlitEmpty;
use varlit::{RecordIndexEntry, VarlitBlock, VarlitFile, varlit_format};
use vl::{ABlock as VA, BBlock as VB, CBlock as VC};
/// Caller-owned decode targets, reused for every record.
type Outs = (VA, VB, VC);

// Same block declarations, native append log with explicit commits.
varlit_format! {
    pub struct MixedNative {
        magic: b"MIXEDNAT";
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
        blocks: [VA, VB, VC];
    }
}
// Identical, but `commit_durable` uses one storage barrier.
varlit_format! {
    pub struct MixedNativeSb {
        magic: b"MIXEDNSB";
        version: 1;
        limits {
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
        }
        endian: little;
        integrity: crc32_with_header;
        index: header_tails;
        commit: transaction_marker(explicit, single_barrier);
        blocks: [VA, VB, VC];
    }
}
/// The single-barrier commit is this layout's best durable choice (it
/// already carries `crc32_with_header`); `FB_TWO_BARRIER=1` selects the
/// default two-barrier commit for comparison.
pub fn native_spec() -> varlit::FormatSpec {
    if std::env::var_os("FB_TWO_BARRIER").is_some() {
        MixedNative::spec()
    } else {
        MixedNativeSb::spec()
    }
}

pub struct W(VarlitFile);
pub fn create(p: &Path) -> Result<Box<dyn AcqWriter>> {
    let mut f = VarlitFile::create(native_spec(), p)?;
    f.commit_durable()?;
    Ok(Box::new(W(f)))
}
impl AcqWriter for W {
    fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
        let (a, b, c) = (&a, &b, &c);
        let t = mono_ns();
        let (xa, xb, xc) = (vl::to_a(a), vl::to_b(b), vl::to_c(c));
        let t2 = mono_ns();
        self.0.push_info(&xa)?;
        self.0.push_info(&xb)?;
        self.0.push_info(&xc)?;
        let t3 = mono_ns();
        if std::env::var_os("FB_PROFILE").is_some() {
            eprintln!("PROF convert_ns={} push_ns={}", t2 - t, t3 - t2);
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
        f: VarlitFile::open_readonly_lazy(vl_spec(native_spec()), p)?,
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
            if e.block_id == VA::ID && want[0] {
                f.decode_block_into(e, scratch, &mut outs.0)?;
                vl::sink_a(s, &outs.0);
            } else if e.block_id == VB::ID && want[1] {
                f.decode_block_into(e, scratch, &mut outs.1)?;
                vl::sink_b(s, &outs.1);
            } else if e.block_id == VC::ID && want[2] {
                f.decode_block_into(e, scratch, &mut outs.2)?;
                vl::sink_c(s, &outs.2);
            }
            Ok(())
        })
    }
}

/// Forward reader shared by the native candidates: after each `follow()` it
/// walks only the records appended since its last walk, in file order, in
/// batches of `BATCH` index entries (`record_map_from` + `release`), so its
/// memory is one batch however far behind it starts.
pub struct Fwd {
    at: Option<u64>,
    buf: Vec<RecordIndexEntry>,
}
const BATCH: usize = 256;
impl Fwd {
    pub fn new() -> Self {
        Self {
            at: None,
            buf: Vec::with_capacity(BATCH),
        }
    }
    pub fn poll(
        &mut self,
        f: &mut VarlitFile,
        mut each: impl FnMut(&VarlitFile, &RecordIndexEntry) -> Result<()>,
    ) -> Result<bool> {
        // One follow() fixes one committed snapshot for every block type.
        f.follow()?;
        let at = match self.at {
            Some(a) => a,
            None => f.record_map(&mut self.buf)?.resume_offset(),
        };
        let mut map = f.record_map_from(&mut self.buf, at)?;
        let mut got = false;
        loop {
            map.release();
            if map.fill_to(BATCH)? == 0 {
                break;
            }
            for e in map.entries() {
                each(f, e)?;
                got = true;
            }
        }
        self.at = Some(map.resume_offset());
        Ok(got)
    }
}
