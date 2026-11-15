//! Varlit append: the grouped base is written once at load and never
//! rewritten; each transaction appends the new balances of its accounts to a
//! keyed delta log (`key = [id]`, disk key index) and `sync()`s. A read looks
//! the account up in the delta log first and falls back to the base. Nothing
//! per account is held in memory on either side.
//! app-built: base + delta lookup order; base as in vl_grouped.rs
use super::*;
use varlit::{BatchOptions, DiskIndexOptions, varlit_format};

varlit_format! {
    pub format BankDelta {
        magic: b"BANKDLT1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        index: keyed_offset_chain;
        blocks {
            fixed Bal(id = 1, key = [id], key_index = disk) { id: u64, balance: i64, }
        }
    }
}

fn opts() -> DiskIndexOptions {
    vl_dio(DiskIndexOptions {
        cache_bytes: 64 << 20,
        ..DiskIndexOptions::default()
    })
}
fn base_path(p: &Path) -> PathBuf {
    p.join("base.varlit")
}
fn delta_path(p: &Path) -> PathBuf {
    p.join("delta.varlit")
}

/// Current balance: newest delta, else the base.
fn current(
    delta: &BankDeltaIndexedReader,
    base: &mut vl_grouped::R,
    id: u64,
    b: &mut Bal,
) -> Result<i64> {
    if delta.get_bal(&id, b)? {
        return Ok(b.balance);
    }
    Ok(base.point(id)?.balance)
}

pub struct W {
    base: Box<dyn Oltp>,
    log: Option<BankDeltaIndexedWriter>,
    delta: Option<BankDeltaIndexedReader>,
    read: Option<vl_grouped::R>,
    p: PathBuf,
}
pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
    std::fs::create_dir_all(p)?;
    Ok(Box::new(W {
        base: vl_grouped::create(&base_path(p))?,
        log: None,
        delta: None,
        read: None,
        p: p.to_path_buf(),
    }))
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        self.base.load()?;
        let mut log = BankDelta::create_indexed_writer(delta_path(&self.p), opts())?;
        log.sync()?;
        self.log = Some(log);
        self.delta = Some(BankDelta::open_indexed_reader(delta_path(&self.p), opts())?);
        self.read = Some(vl_grouped::open_reader(&base_path(&self.p))?);
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        let (delta, base) = (self.delta.as_ref().unwrap(), self.read.as_mut().unwrap());
        let mut b = Bal { id: 0, balance: 0 };
        let mut rows = Vec::with_capacity(deltas.len());
        for &(id, d) in deltas {
            rows.push(Bal {
                id,
                balance: current(delta, base, id, &mut b)? + d,
            });
        }
        let log = self.log.as_mut().unwrap();
        log.push_bals(rows, BatchOptions::default())
            .map_err(|f| f.source)?;
        log.sync()?;
        self.delta.as_mut().unwrap().follow()?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.log.as_mut().unwrap().compact_index()?;
        Ok(())
    }
}

pub struct R {
    base: vl_grouped::R,
    delta: BankDeltaIndexedReader,
    b: Bal,
}
pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
    Ok(Box::new(R {
        base: vl_grouped::open_reader(&base_path(p))?,
        delta: BankDelta::open_indexed_reader(delta_path(p), opts())?,
        b: Bal { id: 0, balance: 0 },
    }))
}
impl OltpRead for R {
    fn point(&mut self, id: u64) -> Result<Row> {
        let mut row = self.base.point(id)?;
        if self.delta.get_bal(&id, &mut self.b)? {
            row.balance = self.b.balance;
        }
        Ok(row)
    }
    fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
        let ids = self.base.branch_ids(b)?;
        let mut sum = 0;
        for &id in &ids {
            sum += current(&self.delta, &mut self.base, id, &mut self.b)?;
        }
        Ok((sum, ids.len() as u64))
    }
}
