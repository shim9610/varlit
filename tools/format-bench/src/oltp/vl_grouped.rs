//! Varlit grouped: 256 accounts packed per keyed record (application layout,
//! 44 bytes per account slot), plus branch -> ids records, in a native append
//! file. A transaction rewrites each touched group and `sync()`s.
//!
//! Keyed lookup is application-built: the native file has no on-disk key
//! index. The writer keeps the newest record offset of every group (one `u64`
//! per group, from each append's `AppendInfo`) and reads a group back by
//! offset. A reader lists the record entries of each block in file order
//! (the resident directory, nothing decoded): the base groups and the branch
//! records are written in key order, so entry `g` is group `g` and entry `b`
//! is branch `b`; only the group versions rewritten by transactions, which
//! follow the base, are decoded at open to learn their key.
//! app-built: branch -> ids records (csr.rs); packing in pack.rs; key -> record
//! lookup (write order + rewrite overlay)
use super::pack::*;
use super::*;
use varlit::{RecordIndexEntry, VarlitEmpty, VarlitReader, varlit_format};

varlit_format! {
    pub format BankGrouped {
        magic: b"BANKGRP1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        index: keyed_offset_chain;
        blocks {
            variable AcctGroup(id = 1, key = [g]) {
                g: u32,
                data: Vec<u8>,
            }
            variable Branch(id = 2, key = [branch]) {
                branch: u32,
                ids: Vec<u64>,
            }
        }
    }
}

pub struct W {
    w: BankGroupedWriter,
    /// Newest record offset of each group, indexed by group number.
    at: Vec<u64>,
    scratch: Vec<u8>,
}
pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
    Ok(Box::new(W {
        w: BankGrouped::create_writer(p)?,
        at: Vec::new(),
        scratch: Vec::new(),
    }))
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        let mut blk = AcctGroup {
            g: 0,
            data: vec![0u8; G * AB],
        };
        self.at = Vec::with_capacity(ngroups());
        for g in 0..ngroups() {
            blk.g = g as u32;
            initial_group(g, &mut blk.data);
            self.at.push(self.w.push_acct_group(&blk)?.record_offset);
        }
        let w = &mut self.w;
        let mut br = Branch {
            branch: 0,
            ids: Vec::new(),
        };
        csr::by_branch(&std::env::temp_dir().join("csr"), |b, ids| {
            br.branch = b;
            br.ids.clear();
            br.ids.extend_from_slice(ids);
            w.push_branch(&br)?;
            Ok(())
        })?;
        self.w.sync()?;
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        let mut blk = AcctGroup::empty();
        for (g, ups) in by_group(deltas) {
            self.w
                .inner()
                .file()
                .read_block_at(self.at[g], &mut self.scratch, &mut blk)?;
            if blk.g as usize != g {
                return Err("group offset names another group".into());
            }
            for (slot, d) in ups {
                add_bal(&mut blk.data, slot, d);
            }
            self.at[g] = self.w.push_acct_group(&blk)?.record_offset;
        }
        self.w.sync()?;
        Ok(())
    }
}

pub struct R {
    r: VarlitReader,
    /// Newest entry of each group, indexed by group number.
    groups: Vec<RecordIndexEntry>,
    /// The entry of each branch, indexed by branch number.
    branches: Vec<RecordIndexEntry>,
    scratch: Vec<u8>,
    blk: AcctGroup,
    br: Branch,
}
pub fn open_reader(p: &Path) -> Result<R> {
    let r = vl_spec(BankGrouped::spec()).open_reader(p)?;
    let f = r.file();
    let mut groups = Vec::new();
    f.block_entries_into::<AcctGroup>(&mut groups)?;
    let n = ngroups();
    if groups.len() < n {
        return Err("missing base groups".into());
    }
    // Versions rewritten by transactions follow the base in file order.
    let (mut scratch, mut blk) = (Vec::new(), AcctGroup::empty());
    for i in n..groups.len() {
        let e = groups[i].clone();
        f.decode_block_into(&e, &mut scratch, &mut blk)?;
        groups[blk.g as usize] = e;
    }
    groups.truncate(n);
    let mut brs = Vec::new();
    f.block_entries_into::<Branch>(&mut brs)?;
    if brs.len() != branches() as usize {
        return Err("missing branches".into());
    }
    Ok(R {
        r,
        groups,
        branches: brs,
        scratch,
        blk,
        br: Branch::empty(),
    })
}
pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
    Ok(Box::new(open_reader(p)?))
}
impl R {
    fn group(&mut self, g: usize) -> Result<()> {
        let e = self.groups.get(g).ok_or("missing group")?;
        self.r
            .file()
            .decode_block_into(e, &mut self.scratch, &mut self.blk)?;
        if self.blk.g as usize != g {
            return Err("group entry names another group".into());
        }
        Ok(())
    }
    fn load_branch(&mut self, b: u32) -> Result<()> {
        let e = self.branches.get(b as usize).ok_or("missing branch")?;
        self.r
            .file()
            .decode_block_into(e, &mut self.scratch, &mut self.br)?;
        if self.br.branch != b {
            return Err("branch entry names another branch".into());
        }
        Ok(())
    }
    pub fn branch_ids(&mut self, b: u32) -> Result<Vec<u64>> {
        self.load_branch(b)?;
        Ok(self.br.ids.clone())
    }
}
impl OltpRead for R {
    fn point(&mut self, id: u64) -> Result<Row> {
        self.group(id as usize / G)?;
        let s = id as usize % G;
        Ok(dec(id, &self.blk.data[s * AB..(s + 1) * AB]))
    }
    fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
        let ids = self.branch_ids(b)?;
        let mut sum = 0;
        for (g, slots) in by_group(&ids.iter().map(|&id| (id, 0)).collect::<Vec<_>>()) {
            self.group(g)?;
            for (slot, _) in slots {
                sum += bal_at(&self.blk.data, slot);
            }
        }
        Ok((sum, ids.len() as u64))
    }
}
