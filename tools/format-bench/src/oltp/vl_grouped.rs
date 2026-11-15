//! Varlit grouped: 256 accounts packed per keyed record (application layout,
//! 44 bytes per account slot), plus branch -> ids records. A transaction
//! rewrites each touched group and `sync()`s.
//! app-built: branch -> ids records (csr.rs); packing in pack.rs
use super::pack::*;
use super::*;
use varlit::{BatchOptions, DiskIndexOptions, VarlitEmpty, varlit_format};

varlit_format! {
    pub format BankGrouped {
        magic: b"BANKGRP1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        index: keyed_offset_chain;
        blocks {
            variable AcctGroup(id = 1, key = [g], key_index = disk) {
                g: u32,
                data: Vec<u8>,
            }
            variable Branch(id = 2, key = [branch], key_index = disk) {
                branch: u32,
                ids: Vec<u64>,
            }
        }
    }
}

fn opts() -> DiskIndexOptions {
    vl_dio(DiskIndexOptions {
        cache_bytes: 256 << 20,
        ..DiskIndexOptions::default()
    })
}

pub struct W {
    w: BankGroupedIndexedWriter,
    r: Option<BankGroupedIndexedReader>,
    p: PathBuf,
}
pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
    Ok(Box::new(W {
        w: BankGrouped::create_indexed_writer(p, opts())?,
        r: None,
        p: p.to_path_buf(),
    }))
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        let groups = (0..ngroups()).map(|g| {
            let mut data = vec![0u8; G * AB];
            initial_group(g, &mut data);
            AcctGroup { g: g as u32, data }
        });
        self.w
            .push_acct_groups(groups, BatchOptions::default())
            .map_err(|f| f.source)?;
        let mut batch = Vec::new();
        let w = &mut self.w;
        csr::by_branch(&std::env::temp_dir().join("csr"), |b, ids| {
            batch.push(Branch {
                branch: b,
                ids: ids.to_vec(),
            });
            if batch.len() == 10_000 {
                w.push_branchs(std::mem::take(&mut batch), BatchOptions::default())
                    .map_err(|f| f.source)?;
            }
            Ok(())
        })?;
        self.w
            .push_branchs(batch, BatchOptions::default())
            .map_err(|f| f.source)?;
        self.w.sync()?;
        self.w.compact_index()?;
        self.r = Some(BankGrouped::open_indexed_reader(&self.p, opts())?);
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        let r = self.r.as_mut().unwrap();
        let mut out = Vec::new();
        for (g, ups) in by_group(deltas) {
            let mut blk = got(|o| r.get_acct_group(&(g as u32), o), "missing group")?;
            for (slot, d) in ups {
                add_bal(&mut blk.data, slot, d);
            }
            out.push(blk);
        }
        self.w
            .push_acct_groups(out, BatchOptions::default())
            .map_err(|f| f.source)?;
        self.w.sync()?;
        r.follow()?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.w.compact_index()?;
        Ok(())
    }
}

pub struct R(pub BankGroupedIndexedReader);
pub fn open_reader(p: &Path) -> Result<R> {
    Ok(R(BankGrouped::open_indexed_reader(p, opts())?))
}
pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
    Ok(Box::new(open_reader(p)?))
}
impl R {
    pub fn branch_ids(&self, b: u32) -> Result<Vec<u64>> {
        Ok(got(|o| self.0.get_branch(&b, o), "missing branch")?.ids)
    }
}
impl OltpRead for R {
    fn point(&mut self, id: u64) -> Result<Row> {
        let blk = got(
            |o| self.0.get_acct_group(&((id as usize / G) as u32), o),
            "missing",
        )?;
        let s = id as usize % G;
        Ok(dec(id, &blk.data[s * AB..(s + 1) * AB]))
    }
    fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
        let br = got(|o| self.0.get_branch(&b, o), "missing branch")?;
        let mut sum = 0;
        let mut blk = AcctGroup::empty();
        for (g, slots) in by_group(&br.ids.iter().map(|&id| (id, 0)).collect::<Vec<_>>()) {
            if !self.0.get_acct_group(&(g as u32), &mut blk)? {
                return Err("missing".into());
            }
            for (slot, _) in slots {
                sum += bal_at(&blk.data, slot);
            }
        }
        Ok((sum, br.ids.len() as u64))
    }
}
