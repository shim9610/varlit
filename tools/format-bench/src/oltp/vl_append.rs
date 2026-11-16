//! Varlit append: the grouped base is written once at load and never
//! rewritten; each transaction appends the new balances of its accounts to a
//! keyed native delta log (`key = [id]`) and `sync()`s. A read looks the
//! account up in the delta log first and falls back to the base.
//!
//! The native log has no on-disk key index. The writer keeps the newest
//! record offset of each account it has logged (from each append's
//! `AppendInfo`); a reader builds the same map at open with
//! `key_tail_offsets`, which walks the delta records once. Both maps hold one
//! entry per account touched by a transaction, never one per account. Values
//! are always read back from the file by offset.
//! app-built: base + delta lookup order; base as in vl_grouped.rs
use super::*;
use varlit::{VarlitReader, VarlitWriter, varlit_format};

varlit_format! {
    pub format BankDelta {
        magic: b"BANKDLT1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        index: keyed_offset_chain;
        blocks {
            fixed Bal(id = 1, key = [id]) { id: u64, balance: i64, }
        }
    }
}

fn base_path(p: &Path) -> PathBuf {
    p.join("base.varlit")
}
fn delta_path(p: &Path) -> PathBuf {
    p.join("delta.varlit")
}

/// Current balance: newest delta (found through `at`), else the base.
fn current(
    file: &varlit::VarlitFile,
    at: &HashMap<u64, u64>,
    scratch: &mut Vec<u8>,
    base: &mut vl_grouped::R,
    id: u64,
    b: &mut Bal,
) -> Result<i64> {
    if let Some(&off) = at.get(&id) {
        file.read_block_at(off, scratch, b)?;
        if b.id != id {
            return Err("delta offset names another account".into());
        }
        return Ok(b.balance);
    }
    Ok(base.point(id)?.balance)
}

pub struct W {
    base: Box<dyn Oltp>,
    log: Option<VarlitWriter>,
    /// Newest delta record offset of each logged account.
    at: HashMap<u64, u64>,
    read: Option<vl_grouped::R>,
    scratch: Vec<u8>,
    p: PathBuf,
}
pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
    std::fs::create_dir_all(p)?;
    Ok(Box::new(W {
        base: vl_grouped::create(&base_path(p))?,
        log: None,
        at: HashMap::new(),
        read: None,
        scratch: Vec::new(),
        p: p.to_path_buf(),
    }))
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        self.base.load()?;
        let mut log = BankDelta::create_writer(delta_path(&self.p))?.into_inner();
        log.sync()?;
        self.log = Some(log);
        self.read = Some(vl_grouped::open_reader(&base_path(&self.p))?);
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        let log = self.log.as_mut().unwrap();
        let base = self.read.as_mut().unwrap();
        let mut b = Bal { id: 0, balance: 0 };
        let mut rows = Vec::with_capacity(deltas.len());
        for &(id, d) in deltas {
            let cur = current(log.file(), &self.at, &mut self.scratch, base, id, &mut b)?;
            rows.push(Bal {
                id,
                balance: cur + d,
            });
        }
        for row in &rows {
            let info = log.push_keyed_info(row)?;
            self.at.insert(row.id, info.record_offset);
        }
        log.sync()?;
        Ok(())
    }
}

pub struct R {
    base: vl_grouped::R,
    delta: VarlitReader,
    at: HashMap<u64, u64>,
    scratch: Vec<u8>,
    b: Bal,
}
pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
    let delta = vl_spec(BankDelta::spec()).open_reader(delta_path(p))?;
    let at = delta.file().key_tail_offsets::<Bal>()?;
    Ok(Box::new(R {
        base: vl_grouped::open_reader(&base_path(p))?,
        delta,
        at,
        scratch: Vec::new(),
        b: Bal { id: 0, balance: 0 },
    }))
}
impl OltpRead for R {
    fn point(&mut self, id: u64) -> Result<Row> {
        let mut row = self.base.point(id)?;
        if let Some(&off) = self.at.get(&id) {
            self.delta
                .file()
                .read_block_at(off, &mut self.scratch, &mut self.b)?;
            row.balance = self.b.balance;
        }
        Ok(row)
    }
    fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
        let ids = self.base.branch_ids(b)?;
        let mut sum = 0;
        for &id in &ids {
            sum += current(
                self.delta.file(),
                &self.at,
                &mut self.scratch,
                &mut self.base,
                id,
                &mut self.b,
            )?;
        }
        Ok((sum, ids.len() as u64))
    }
}
