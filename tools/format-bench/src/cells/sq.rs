//! SQLite: one row per cell (`id INTEGER PRIMARY KEY, data BLOB`); an update
//! is one transaction. WAL + `synchronous=NORMAL` makes a commit visible
//! without a WAL sync; durable updates run at FULL; WAL checkpoints at durable
//! commits once ~4 MB accumulated. Readers hold one read transaction per poll.
//! Candidates: page size, optional memory map.
//! app-built: nothing
use super::*;
use ::rusqlite::{Connection, OpenFlags, params};

pub const CANDIDATES: &[&str] = &["p4096", "p16384", "p4096-mmap"];

fn pragmas(c: &Connection, cand: &str) -> Result<()> {
    if cand.ends_with("-mmap") {
        c.execute_batch("PRAGMA mmap_size=34359738368;")?;
    }
    Ok(())
}

pub struct W {
    c: Connection,
    full: bool,
    wal: usize,
}
pub fn create(p: &Path, cand: &str) -> Result<Box<dyn CellWriter>> {
    let c = Connection::open(p)?;
    let page = if cand.starts_with("p16384") {
        16384
    } else {
        4096
    };
    c.execute_batch(&format!(
        "PRAGMA page_size={page}; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
         PRAGMA wal_autocheckpoint=0;
         CREATE TABLE cells(id INTEGER PRIMARY KEY, data BLOB NOT NULL);"
    ))?;
    pragmas(&c, cand)?;
    Ok(Box::new(W {
        c,
        full: false,
        wal: 0,
    }))
}
impl CellWriter for W {
    fn load(&mut self, first: u32, data: &[u8]) -> Result<()> {
        if first == 0 {
            self.c.execute_batch("PRAGMA synchronous=FULL; BEGIN;")?;
        }
        let mut st = self.c.prepare_cached("INSERT INTO cells VALUES (?1, ?2)")?;
        for (i, cell) in data.chunks_exact(CELL).enumerate() {
            st.execute(params![(first as usize + i) as i64, cell])?;
        }
        Ok(())
    }
    fn loaded(&mut self) -> Result<()> {
        self.c.execute_batch("COMMIT")?;
        self.c
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        self.c.execute_batch("PRAGMA synchronous=NORMAL;")?;
        Ok(())
    }
    fn begin(&mut self, durable: bool) -> Result<()> {
        if durable != self.full {
            self.c.execute_batch(if durable {
                "PRAGMA synchronous=FULL"
            } else {
                "PRAGMA synchronous=NORMAL"
            })?;
            self.full = durable;
        }
        self.c.execute_batch("BEGIN")?;
        Ok(())
    }
    fn write(&mut self, id: u32, cell: &[u8]) -> Result<()> {
        self.c
            .prepare_cached("INSERT OR REPLACE INTO cells VALUES (?1, ?2)")?
            .execute(params![id as i64, cell])?;
        self.wal += cell.len();
        Ok(())
    }
    fn publish(&mut self, durable: bool) -> Result<()> {
        self.c.execute_batch("COMMIT")?;
        if durable && self.wal >= 4 << 20 {
            self.c
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
            self.wal = 0;
        }
        Ok(())
    }
}

pub struct R(Connection);
pub fn open(p: &Path, cand: &str) -> Result<Box<dyn CellReader>> {
    let c = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    pragmas(&c, cand)?;
    Ok(Box::new(R(c)))
}
impl CellReader for R {
    fn refresh(&mut self) -> Result<()> {
        self.0.execute_batch("BEGIN")?;
        Ok(())
    }
    fn read(&mut self, id: u32, out: &mut [u8]) -> Result<()> {
        let mut st = self
            .0
            .prepare_cached("SELECT data FROM cells WHERE id = ?1")?;
        let mut rows = st.query([id as i64])?;
        let r = rows.next()?.ok_or("cell missing")?;
        out.copy_from_slice(r.get_ref(0)?.as_blob()?);
        Ok(())
    }
    fn end(&mut self) -> Result<()> {
        self.0.execute_batch("COMMIT")?;
        Ok(())
    }
}
