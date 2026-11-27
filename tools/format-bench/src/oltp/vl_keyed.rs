//! Varlit keyed storage, one record per account keyed by its key, declared
//! `fixed`: the record is the packed fields (the key fields stored once, in
//! the key). The format declares `keyed_storage: in_place`: a B-tree of
//! 4 KiB pages, one tree per block type. A transaction reads each touched
//! account by key, puts it back with the new balance and `sync()`s.
//! Branch aggregates read an application-built branch -> keys record per
//! branch, as SQLite uses its secondary index.
//! app-built: branch -> keys records (csr.rs)
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

pub struct W {
    w: BankRowsWriter,
    scratch: Vec<u8>,
    acc: Account,
}
pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
    Ok(Box::new(W {
        w: BankRowsWriter::from_inner(vl_write(BankRows::spec()).create_writer(p)?)?,
        scratch: Vec::new(),
        acc: Account::empty(),
    }))
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        for id in (0..accounts()).map(key_of) {
            let r = initial_row(id);
            self.w.put_account(&Account {
                id,
                branch: r.branch,
                balance: r.balance,
                name: r.name,
                created: r.created,
            })?;
        }
        let w = &mut self.w;
        let mut br = RowBranch {
            branch: 0,
            ids: Vec::new(),
        };
        csr::by_branch(&std::env::temp_dir().join("csr"), |b, positions| {
            br.branch = b;
            br.ids.clear();
            br.ids.extend(positions.iter().map(|&i| key_of(i)));
            w.put_row_branch(&br)?;
            Ok(())
        })?;
        self.w.sync()?;
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        for &(id, d) in deltas {
            if !self.w.get_account(&id, &mut self.scratch, &mut self.acc)? {
                return Err("missing account".into());
            }
            self.acc.balance += d;
            self.w.put_account(&self.acc)?;
        }
        self.w.sync()?;
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
