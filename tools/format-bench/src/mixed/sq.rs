//! SQLite
//! One table per type, one row per block holding the packed records
//! (little-endian, exact field widths); one transaction per publication.
//! app-built: record packing into one BLOB per window and type (little-endian codec)
use super::*;
use rusqlite::{Connection, OpenFlags, params};

fn pack_a(b: &BA, out: &mut Vec<u8>) {
    out.clear();
    for r in &b.recs {
        out.extend_from_slice(&r.t.to_le_bytes());
        out.extend(r.a.iter().map(|&v| v as u8));
        out.extend(r.f.iter().map(|&v| v as u8));
        for v in r.x {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}
fn pack_b(b: &BB, out: &mut Vec<u8>) {
    out.clear();
    for r in &b.recs {
        out.extend_from_slice(&r.t.to_le_bytes());
        for v in r.x {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&r.status.to_le_bytes());
    }
}
fn pack_c(b: &BC, out: &mut Vec<u8>) {
    out.clear();
    for r in &b.recs {
        out.extend_from_slice(&r.t.to_le_bytes());
        out.extend_from_slice(&r.code.to_le_bytes());
        out.extend_from_slice(&(r.msg.len() as u32).to_le_bytes());
        out.extend_from_slice(r.msg.as_bytes());
    }
}
fn u64le(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}
fn unpack_a(d: &[u8]) -> impl ExactSizeIterator<Item = RecA> + '_ {
    d.chunks_exact(1021).map(|c| RecA {
        t: u64le(c),
        a: std::array::from_fn(|j| c[8 + j] as i8),
        f: std::array::from_fn(|j| c[18 + j] != 0),
        x: std::array::from_fn(|j| {
            u16::from_le_bytes(c[21 + 2 * j..23 + 2 * j].try_into().unwrap())
        }),
    })
}
fn unpack_b(d: &[u8]) -> impl ExactSizeIterator<Item = RecB> + '_ {
    d.chunks_exact(138).map(|c| RecB {
        t: u64le(c),
        x: std::array::from_fn(|j| {
            f32::from_le_bytes(c[8 + 4 * j..12 + 4 * j].try_into().unwrap())
        }),
        status: u16::from_le_bytes(c[136..138].try_into().unwrap()),
    })
}
fn unpack_c(mut d: &[u8]) -> Vec<RecC> {
    let mut v = Vec::new();
    while !d.is_empty() {
        let n = u32::from_le_bytes(d[12..16].try_into().unwrap()) as usize;
        v.push(RecC {
            t: u64le(d),
            code: u32::from_le_bytes(d[8..12].try_into().unwrap()),
            msg: String::from_utf8(d[16..16 + n].to_vec()).unwrap(),
        });
        d = &d[16 + n..];
    }
    v
}

pub struct W {
    c: Connection,
    buf: Vec<u8>,
    open: bool,
    full: bool,
    want_full: bool,
    wal_bytes: usize,
}
pub fn create(p: &Path) -> Result<Box<dyn AcqWriter>> {
    let c = Connection::open(p)?;
    c.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA wal_autocheckpoint=0;
         CREATE TABLE a(w INTEGER PRIMARY KEY, t0 INTEGER, data BLOB);
         CREATE TABLE b(w INTEGER PRIMARY KEY, t0 INTEGER, data BLOB);
         CREATE TABLE c(w INTEGER PRIMARY KEY, t0 INTEGER, data BLOB);",
    )?;
    Ok(Box::new(W {
        c,
        buf: Vec::new(),
        open: false,
        full: false,
        want_full: false,
        wal_bytes: 0,
    }))
}
impl AcqWriter for W {
    fn append(&mut self, a: BA, b: BB, c: BC) -> Result<()> {
        let (a, b, c) = (&a, &b, &c);
        if !self.open {
            // WAL + NORMAL: a commit is visible without syncing the WAL;
            // FULL syncs the WAL at commit. The level is per connection
            // and cannot change inside a transaction.
            if self.want_full != self.full {
                self.c.execute_batch(if self.want_full {
                    "PRAGMA synchronous=FULL"
                } else {
                    "PRAGMA synchronous=NORMAL"
                })?;
                self.full = self.want_full;
            }
            self.c.execute_batch("BEGIN")?;
            self.open = true;
        }
        pack_a(a, &mut self.buf);
        self.c
            .prepare_cached("INSERT INTO a VALUES (?1,?2,?3)")?
            .execute(params![a.w as i64, a.t0 as i64, &self.buf])?;
        self.wal_bytes += self.buf.len();
        pack_b(b, &mut self.buf);
        self.c
            .prepare_cached("INSERT INTO b VALUES (?1,?2,?3)")?
            .execute(params![b.w as i64, b.t0 as i64, &self.buf])?;
        self.wal_bytes += self.buf.len();
        pack_c(c, &mut self.buf);
        self.c
            .prepare_cached("INSERT INTO c VALUES (?1,?2,?3)")?
            .execute(params![c.w as i64, c.t0 as i64, &self.buf])?;
        self.wal_bytes += self.buf.len();
        Ok(())
    }
    fn will_publish(&mut self, durable: bool) {
        self.want_full = durable;
    }
    fn publish(&mut self, durable: bool) -> Result<()> {
        assert_eq!(durable, self.full, "durability announced at BEGIN");
        if self.open {
            self.c.execute_batch("COMMIT")?;
            self.open = false;
        }
        // Automatic checkpoints (every ~4 MB of WAL) sync the database
        // at arbitrary commits; keep the same WAL budget but checkpoint
        // only at commits that already pay a sync.
        if durable && self.wal_bytes >= 4 << 20 {
            self.c
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
            self.wal_bytes = 0;
        }
        Ok(())
    }
}
pub struct R {
    c: Connection,
    want: [bool; 3],
}
pub fn open(p: &Path, want: [bool; 3]) -> Result<Box<dyn AcqReader>> {
    Ok(Box::new(R {
        c: Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY)?,
        want,
    }))
}
impl AcqReader for R {
    fn poll(&mut self, s: &mut Sink) -> Result<bool> {
        // One read transaction = one consistent snapshot of all tables.
        self.c.execute_batch("BEGIN")?;
        let mut got = false;
        for (i, t) in ["a", "b", "c"].into_iter().enumerate() {
            if !self.want[i] {
                continue;
            }
            let mut st = self.c.prepare_cached(&format!(
                "SELECT w, t0, data FROM {t} WHERE w >= ?1 ORDER BY w"
            ))?;
            let mut rows = st.query([s.seen[i] as i64])?;
            while let Some(r) = rows.next()? {
                let (w, t0, d) = (
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u64,
                    r.get_ref(2)?.as_blob()?,
                );
                match i {
                    0 => s.a(w, t0, unpack_a(d)),
                    1 => s.b(w, t0, unpack_b(d)),
                    _ => s.c(w, t0, unpack_c(d).into_iter()),
                }
                got = true;
            }
        }
        self.c.execute_batch("COMMIT")?;
        Ok(got)
    }
}
