//! Varlit indexed: one keyed record per account (`key = [id]`, disk key index)
//! and one keyed record per branch holding its account ids (the application
//! maintains this secondary index). A transaction reads through a reader that
//! follows the writer, appends the new account versions and `sync()`s.
//! app-built: branch -> ids records (built with csr.rs)
use super::*;
use varlit::{BatchOptions, DiskIndexOptions, VarlitEmpty, varlit_format};

varlit_format! {
    pub format Bank {
        magic: b"BANKOLTP";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        index: keyed_offset_chain;
        blocks {
            fixed Account(id = 1, key = [id], key_index = disk) {
                id: u64,
                branch: u32,
                balance: i64,
                name: [u8; 24],
                created: u64,
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
    w: BankIndexedWriter,
    r: Option<BankIndexedReader>,
    p: PathBuf,
}
pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
    Ok(Box::new(W {
        w: Bank::create_indexed_writer(p, opts())?,
        r: None,
        p: p.to_path_buf(),
    }))
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        self.w
            .push_accounts(
                (0..accounts()).map(|id| {
                    let r = initial_row(id);
                    Account {
                        id,
                        branch: r.branch,
                        balance: r.balance,
                        name: r.name,
                        created: r.created,
                    }
                }),
                BatchOptions::default(),
            )
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
        self.r = Some(Bank::open_indexed_reader(&self.p, opts())?);
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        let r = self.r.as_mut().unwrap();
        let mut rows = Vec::with_capacity(deltas.len());
        for &(id, d) in deltas {
            let mut a = got(|o| r.get_account(&id, o), "missing account")?;
            a.balance += d;
            rows.push(a);
        }
        self.w
            .push_accounts(rows, BatchOptions::default())
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

pub struct R(BankIndexedReader);
pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
    Ok(Box::new(R(Bank::open_indexed_reader(p, opts())?)))
}
impl OltpRead for R {
    fn point(&mut self, id: u64) -> Result<Row> {
        let a = got(|o| self.0.get_account(&id, o), "missing")?;
        Ok(Row {
            id: a.id,
            branch: a.branch,
            balance: a.balance,
            name: a.name,
            created: a.created,
        })
    }
    fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
        let br = got(|o| self.0.get_branch(&b, o), "missing branch")?;
        let mut s = 0;
        let mut a = Account::empty();
        for id in &br.ids {
            if !self.0.get_account(id, &mut a)? {
                return Err("missing".into());
            }
            s += a.balance;
        }
        Ok((s, br.ids.len() as u64))
    }
}
