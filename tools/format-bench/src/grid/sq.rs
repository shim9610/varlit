//! SQLite: one row per chunk keyed by its linear chunk id (rowid), so each run
//! of chunk ids is one rowid range scan. One durable transaction per step
//! (WAL, `synchronous=FULL`); a time-deep chunk is read back and replaced.
//! app-built: chunk id keying
use super::*;
use ::rusqlite::{Connection, OpenFlags, params};

pub struct Wr {
    c: Connection,
    ck: Ck,
    buf: Vec<u8>,
}
pub fn create(p: &Path, ck: Ck) -> Result<Box<dyn GridWrite>> {
    let c = Connection::open(p)?;
    c.execute_batch(
        "PRAGMA page_size=32768; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
         CREATE TABLE chunks(id INTEGER PRIMARY KEY, data BLOB NOT NULL);",
    )?;
    Ok(Box::new(Wr {
        c,
        ck,
        buf: vec![0; CB],
    }))
}
impl GridWrite for Wr {
    fn step(&mut self, t: usize, step: &[f32]) -> Result<()> {
        let ck = self.ck;
        let tx = self.c.transaction()?;
        {
            let mut get = tx.prepare_cached("SELECT data FROM chunks WHERE id = ?1")?;
            let mut put = tx.prepare_cached("INSERT OR REPLACE INTO chunks VALUES (?1, ?2)")?;
            for id in ck.ids_of_step(t) {
                if t % ck.c[0] == 0 {
                    self.buf.fill(0);
                } else {
                    let v: Vec<u8> = get.query_row([id as i64], |r| r.get(0))?;
                    self.buf.copy_from_slice(&v);
                }
                ck.put_step(step, t, id, &mut self.buf);
                put.execute(params![id as i64, &self.buf[..]])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    fn finish(self: Box<Self>) -> Result<()> {
        self.c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }
}
pub struct Rd {
    c: Connection,
    ck: Ck,
}
pub fn open(p: &Path, ck: Ck) -> Result<Box<dyn GridRead>> {
    let c = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    c.execute_batch("PRAGMA cache_size=-65536;")?;
    Ok(Box::new(Rd { c, ck }))
}
impl GridRead for Rd {
    fn slab(&mut self, start: Ix, count: Ix, out: &mut Vec<f32>) -> Result<()> {
        prepare(&count, out);
        let ck = self.ck;
        let mut st = self
            .c
            .prepare_cached("SELECT id, data FROM chunks WHERE id BETWEEN ?1 AND ?2")?;
        let mut runs = Vec::new();
        ck.runs(&start, &count, |id0, n| runs.push((id0, n)));
        for (id0, n) in runs {
            let mut rows = st.query(params![id0 as i64, (id0 + n - 1) as i64])?;
            let mut got = 0;
            while let Some(r) = rows.next()? {
                ck.copy_out(
                    &start,
                    &count,
                    r.get::<_, i64>(0)? as usize,
                    r.get_ref(1)?.as_blob()?,
                    out,
                );
                got += 1;
            }
            if got != n {
                return Err("missing chunk".into());
            }
        }
        Ok(())
    }
}
