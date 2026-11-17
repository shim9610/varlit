//! Varlit keyed storage, one keyed record per account (key = account id),
//! declared `fixed`: the record is the packed fields, with no per-field
//! headers,
//! in a format that declares `keyed_storage: in_place`: the same shape as
//! SQLite's table. A transaction reads each touched account by key, puts it
//! back with the new balance and `sync()`s; the library's B-tree changes the
//! one leaf page that holds it. Branch aggregates use an application-built
//! branch -> ids record, as SQLite uses its secondary index.
//! app-built: branch -> ids records (csr.rs)
use super::*;
use varlit::{VarlitEmpty, VarlitReader, varlit_format};

varlit_format! {
    pub format BankRows {
        magic: b"BANKROW1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        keyed_storage: in_place;
        blocks {
            fixed Account(id = 1, key = [id]) {
                id: u64,
                branch: u32,
                balance: i64,
                name: [u8; 24],
                created: u64,
            }
            variable RowBranch(id = 2, key = [branch]) {
                branch: u32,
                ids: Vec<u64>,
            }
        }
    }
}

fn row(a: &Account) -> Row {
    Row {
        id: a.id,
        branch: a.branch,
        balance: a.balance,
        name: a.name,
        created: a.created,
    }
}

thread_local! {
    static SPLIT: std::cell::RefCell<(f64, f64, f64, u64)> = const { std::cell::RefCell::new((0.0, 0.0, 0.0, 0)) };
}

pub struct W {
    w: BankRowsWriter,
    scratch: Vec<u8>,
    acc: Account,
}
pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
    Ok(Box::new(W {
        w: BankRows::create_writer(p)?,
        scratch: Vec::new(),
        acc: Account::empty(),
    }))
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        let t0 = std::time::Instant::now();
        for id in 0..accounts() {
            let r = initial_row(id);
            self.w.put_account(&Account {
                id,
                branch: r.branch,
                balance: r.balance,
                name: r.name,
                created: r.created,
            })?;
        }
        if std::env::var_os("FB_KEYED_STATS").is_some() {
            eprintln!(
                "after accounts {:?}",
                self.w.inner().file().keyed_storage_stats()?
            );
        }
        let t1 = std::time::Instant::now();
        let w = &mut self.w;
        let mut br = RowBranch {
            branch: 0,
            ids: Vec::new(),
        };
        csr::by_branch(&std::env::temp_dir().join("csr"), |b, ids| {
            br.branch = b;
            br.ids.clear();
            br.ids.extend_from_slice(ids);
            w.put_row_branch(&br)?;
            Ok(())
        })?;
        let t2 = std::time::Instant::now();
        self.w.sync()?;
        if std::env::var_os("FB_LOAD_SPLIT").is_some() {
            eprintln!(
                "load split: accounts {:.3}s branches {:.3}s sync {:.3}s",
                (t1 - t0).as_secs_f64(),
                (t2 - t1).as_secs_f64(),
                t2.elapsed().as_secs_f64()
            );
        }
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        let (mut get, mut put) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        for &(id, d) in deltas {
            let t = std::time::Instant::now();
            if !self.w.get_account(&id, &mut self.scratch, &mut self.acc)? {
                return Err("missing account".into());
            }
            get += t.elapsed();
            self.acc.balance += d;
            let t = std::time::Instant::now();
            self.w.put_account(&self.acc)?;
            put += t.elapsed();
        }
        let t = std::time::Instant::now();
        self.w.sync()?;
        let sync = t.elapsed();
        if std::env::var_os("FB_TXN_SPLIT").is_some() {
            SPLIT.with(|c| {
                let mut c = c.borrow_mut();
                c.0 += get.as_secs_f64();
                c.1 += put.as_secs_f64();
                c.2 += sync.as_secs_f64();
                c.3 += 1;
                if c.3 % 1000 == 0 {
                    eprintln!(
                        "txn split over {} txns: get {:.3}s put {:.3}s sync {:.3}s",
                        c.3, c.0, c.1, c.2
                    );
                }
            });
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        if std::env::var_os("FB_KEYED_STATS").is_some() {
            eprintln!(
                "keyed stats {:?}",
                self.w.inner().file().keyed_storage_stats()?
            );
        }
        Ok(())
    }
}

pub struct R {
    r: VarlitReader,
    scratch: Vec<u8>,
    acc: Account,
    br: RowBranch,
}
pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
    let r = vl_spec(BankRows::spec()).open_reader(p)?;
    Ok(Box::new(R {
        r,
        scratch: Vec::new(),
        acc: Account::empty(),
        br: RowBranch::empty(),
    }))
}
impl OltpRead for R {
    fn point(&mut self, id: u64) -> Result<Row> {
        if !self.r.get_keyed(&id, &mut self.scratch, &mut self.acc)? {
            return Err("missing account".into());
        }
        Ok(row(&self.acc))
    }
    fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
        if !self.r.get_keyed(&b, &mut self.scratch, &mut self.br)? {
            return Err("missing branch".into());
        }
        let mut sum = 0;
        for i in 0..self.br.ids.len() {
            let id = self.br.ids[i];
            if !self.r.get_keyed(&id, &mut self.scratch, &mut self.acc)? {
                return Err("missing account".into());
            }
            sum += self.acc.balance;
        }
        Ok((sum, self.br.ids.len() as u64))
    }
}
