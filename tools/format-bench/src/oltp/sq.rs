//! SQLite: one table with an INTEGER PRIMARY KEY and a secondary index on
//! branch, built after the bulk load. Bulk load in one rollback-journal
//! transaction (new pages are not journaled), then WAL with
//! `synchronous=FULL` for the transactions. Aggregates are plain SQL.
//! Candidates: page size and page cache size.
//! app-built: nothing
use super::*;
use ::rusqlite::{Connection, OpenFlags, params};

pub const CANDIDATES: &[&str] = &["p4096", "p4096-c1g", "p16384-c1g"];

fn setup(c: &Connection, cand: &str) -> Result<()> {
    if cand.ends_with("-c1g") {
        c.execute_batch("PRAGMA cache_size=-1048576;")?;
    }
    Ok(())
}

pub struct W(Connection);
pub fn create(p: &Path, cand: &str) -> Result<Box<dyn Oltp>> {
    let c = Connection::open(p)?;
    let page = if cand.starts_with("p16384") {
        16384
    } else {
        4096
    };
    c.execute_batch(&format!(
        "PRAGMA page_size={page}; PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
         CREATE TABLE accounts(id INTEGER PRIMARY KEY, branch INTEGER NOT NULL,
           balance INTEGER NOT NULL, name BLOB NOT NULL, created INTEGER NOT NULL);"
    ))?;
    setup(&c, cand)?;
    Ok(Box::new(W(c)))
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        let tx = self.0.transaction()?;
        {
            let mut st = tx.prepare("INSERT INTO accounts VALUES (?1, ?2, ?3, ?4, ?5)")?;
            for id in 0..accounts() {
                let r = initial_row(id);
                st.execute(params![
                    id as i64,
                    r.branch,
                    r.balance,
                    &r.name[..],
                    r.created as i64
                ])?;
            }
        }
        tx.execute_batch("CREATE INDEX accounts_branch ON accounts(branch);")?;
        tx.commit()?;
        self.0.execute_batch("PRAGMA journal_mode=WAL;")?;
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        let tx = self.0.transaction()?;
        {
            let mut st =
                tx.prepare_cached("UPDATE accounts SET balance = balance + ?2 WHERE id = ?1")?;
            for &(id, d) in deltas {
                if st.execute(params![id as i64, d])? != 1 {
                    return Err("missing account".into());
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.0
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }
}

pub struct R(Connection);
pub fn open(p: &Path, cand: &str) -> Result<Box<dyn OltpRead>> {
    let c = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    setup(&c, cand)?;
    Ok(Box::new(R(c)))
}
impl OltpRead for R {
    fn point(&mut self, id: u64) -> Result<Row> {
        let mut st = self
            .0
            .prepare_cached("SELECT branch, balance, name, created FROM accounts WHERE id = ?1")?;
        Ok(st.query_row([id as i64], |r| {
            Ok(Row {
                id,
                branch: r.get(0)?,
                balance: r.get(1)?,
                name: r.get_ref(2)?.as_blob()?.try_into().unwrap(),
                created: r.get::<_, i64>(3)? as u64,
            })
        })?)
    }
    fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
        let mut st = self
            .0
            .prepare_cached("SELECT total(balance), count(*) FROM accounts WHERE branch = ?1")?;
        Ok(st.query_row([b], |r| {
            Ok((r.get::<_, f64>(0)? as i64, r.get::<_, i64>(1)? as u64))
        })?)
    }
}
