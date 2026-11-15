//! SQLite: one table with ts as INTEGER PRIMARY KEY (rows clustered by time,
//! Q2 is a rowid range), category/region as small lookup tables. Bulk load in
//! one rollback-journal transaction; optional covering index for Q1.
//! Candidates: page size, covering index (qty, category, price).
//! app-built: nothing
use super::*;
use ::rusqlite::{Connection, OpenFlags, params};

pub const CANDIDATES: &[&str] = &["p4096", "p4096-q1idx", "p65536-q1idx"];

pub struct W {
    c: Connection,
    idx: bool,
    started: bool,
}
pub fn create(p: &Path, cand: &str) -> Result<Box<dyn Writer>> {
    let c = Connection::open(p)?;
    let page = if cand.starts_with("p65536") {
        65536
    } else {
        4096
    };
    c.execute_batch(&format!(
        "PRAGMA page_size={page}; PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-262144;
         CREATE TABLE category(code INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE region(code INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE sales(ts INTEGER PRIMARY KEY, user INTEGER, category INTEGER, region INTEGER,
           qty INTEGER, price REAL, discount REAL, flags INTEGER, m0 REAL, m1 REAL, m2 REAL, m3 REAL);"
    ))?;
    Ok(Box::new(W {
        c,
        idx: cand.ends_with("-q1idx"),
        started: false,
    }))
}
impl Writer for W {
    fn append(&mut self, t: &Batch) -> Result<()> {
        if !self.started {
            self.c.execute_batch("BEGIN")?;
            for i in 0..NCAT {
                self.c.execute(
                    "INSERT INTO category VALUES (?1, ?2)",
                    params![i as i64, cat_name(i)],
                )?;
            }
            for i in 0..NREG {
                self.c.execute(
                    "INSERT INTO region VALUES (?1, ?2)",
                    params![i as i64, region_name(i)],
                )?;
            }
            self.started = true;
        }
        let mut st = self
            .c
            .prepare_cached("INSERT INTO sales VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)")?;
        for i in 0..t.len() {
            st.execute(params![
                t.ts[i],
                t.user[i],
                t.cat[i],
                t.region[i],
                t.qty[i],
                t.price[i],
                t.discount[i] as f64,
                t.flags[i],
                t.m[0][i],
                t.m[1][i],
                t.m[2][i],
                t.m[3][i]
            ])?;
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        if self.idx {
            self.c
                .execute_batch("CREATE INDEX sales_q1 ON sales(qty, category, price);")?;
        }
        self.c.execute_batch("COMMIT")?;
        Ok(())
    }
}

pub struct R(Connection);
pub fn open(p: &Path, _cand: &str) -> Result<Box<dyn Analytics>> {
    let c = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    c.execute_batch("PRAGMA cache_size=-262144;")?;
    Ok(Box::new(R(c)))
}
impl Analytics for R {
    fn q1(&mut self) -> Result<Q1> {
        let mut out = [0.0; NCAT];
        let mut st = self.0.prepare_cached(
            "SELECT c.name, s.total FROM (SELECT category, total(price*qty) AS total FROM sales
             WHERE qty > 50 GROUP BY category) s JOIN category c ON c.code = s.category",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let name: String = r.get(0)?;
            out[name[9..].parse::<usize>()?] = r.get(1)?;
        }
        Ok(out)
    }
    fn q2(&mut self, a: i64, b: i64) -> Result<(u64, f64)> {
        let mut st = self.0.prepare_cached(
            "SELECT count(*), total(price) FROM sales WHERE ts BETWEEN ?1 AND ?2",
        )?;
        Ok(st.query_row([a, b], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)))?)
    }
}
