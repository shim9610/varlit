//! Varlit keyed storage: the grouped layout (256 accounts per keyed record,
//! branch -> ids records) in a format that declares
//! `keyed_storage: in_place`. A transaction reads each touched group by key,
//! puts it back in place and `sync()`s. The file keeps one version of each
//! group, and the library finds records by key: no application lookup.
//! app-built: branch -> ids records (csr.rs); packing in pack.rs
use super::pack::*;
use super::*;
use varlit::{VarlitEmpty, VarlitReader, varlit_format};

varlit_format! {
    pub format BankHeap {
        magic: b"BANKHEP1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        keyed_storage: in_place;
        blocks {
            variable HeapGroup(id = 1, key = [g]) {
                g: u32,
                data: Vec<u8>,
            }
            variable HeapBranch(id = 2, key = [branch]) {
                branch: u32,
                ids: Vec<u64>,
            }
        }
    }
}

pub struct W {
    w: BankHeapWriter,
    scratch: Vec<u8>,
    blk: HeapGroup,
}
pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
    Ok(Box::new(W {
        w: BankHeap::create_writer(p)?,
        scratch: Vec::new(),
        blk: HeapGroup::empty(),
    }))
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        let mut blk = HeapGroup {
            g: 0,
            data: vec![0u8; G * AB],
        };
        for g in 0..ngroups() {
            blk.g = g as u32;
            initial_group(g, &mut blk.data);
            self.w.put_heap_group(&blk)?;
        }
        let w = &mut self.w;
        let mut br = HeapBranch {
            branch: 0,
            ids: Vec::new(),
        };
        csr::by_branch(&std::env::temp_dir().join("csr"), |b, ids| {
            br.branch = b;
            br.ids.clear();
            br.ids.extend_from_slice(ids);
            w.put_heap_branch(&br)?;
            Ok(())
        })?;
        self.w.sync()?;
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        for (g, ups) in by_group(deltas) {
            if !self
                .w
                .get_heap_group(&(g as u32), &mut self.scratch, &mut self.blk)?
            {
                return Err("missing group".into());
            }
            for (slot, d) in ups {
                add_bal(&mut self.blk.data, slot, d);
            }
            self.w.put_heap_group(&self.blk)?;
        }
        self.w.sync()?;
        Ok(())
    }
}

pub struct R {
    r: VarlitReader,
    scratch: Vec<u8>,
    blk: HeapGroup,
    br: HeapBranch,
}
pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
    let r = vl_spec(BankHeap::spec()).open_reader(p)?;
    Ok(Box::new(R {
        r,
        scratch: Vec::new(),
        blk: HeapGroup::empty(),
        br: HeapBranch::empty(),
    }))
}
impl R {
    fn group(&mut self, g: usize) -> Result<()> {
        if !self
            .r
            .get_keyed(&(g as u32), &mut self.scratch, &mut self.blk)?
        {
            return Err("missing group".into());
        }
        Ok(())
    }
}
impl OltpRead for R {
    fn point(&mut self, id: u64) -> Result<Row> {
        self.group(id as usize / G)?;
        let s = id as usize % G;
        Ok(dec(id, &self.blk.data[s * AB..(s + 1) * AB]))
    }
    fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
        if !self.r.get_keyed(&b, &mut self.scratch, &mut self.br)? {
            return Err("missing branch".into());
        }
        let ids = std::mem::take(&mut self.br.ids);
        let mut sum = 0;
        for (g, slots) in by_group(&ids.iter().map(|&id| (id, 0)).collect::<Vec<_>>()) {
            self.group(g)?;
            for (slot, _) in slots {
                sum += bal_at(&self.blk.data, slot);
            }
        }
        let n = ids.len() as u64;
        self.br.ids = ids;
        Ok((sum, n))
    }
}
