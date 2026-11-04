//! Native prepared statements; one connection per worker, no Rust-wide database lock.
use crate::{Result, model::*};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    path::Path,
    ptr,
};
enum Db {}
enum Stmt {}
#[link(name = "sqlite3")]
unsafe extern "C" {
    fn sqlite3_libversion() -> *const c_char;
    fn sqlite3_threadsafe() -> c_int;
    fn sqlite3_open_v2(p: *const c_char, d: *mut *mut Db, f: c_int, v: *const c_char) -> c_int;
    fn sqlite3_close(d: *mut Db) -> c_int;
    fn sqlite3_busy_timeout(d: *mut Db, ms: c_int) -> c_int;
    fn sqlite3_exec(
        d: *mut Db,
        s: *const c_char,
        c: *mut c_void,
        a: *mut c_void,
        e: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_errmsg(d: *mut Db) -> *const c_char;
    fn sqlite3_prepare_v2(
        d: *mut Db,
        q: *const c_char,
        n: c_int,
        s: *mut *mut Stmt,
        t: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_finalize(s: *mut Stmt) -> c_int;
    fn sqlite3_reset(s: *mut Stmt) -> c_int;
    fn sqlite3_clear_bindings(s: *mut Stmt) -> c_int;
    fn sqlite3_step(s: *mut Stmt) -> c_int;
    fn sqlite3_bind_int64(s: *mut Stmt, i: c_int, v: i64) -> c_int;
    fn sqlite3_bind_blob64(
        s: *mut Stmt,
        i: c_int,
        p: *const c_void,
        n: u64,
        d: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_column_int64(s: *mut Stmt, i: c_int) -> i64;
    fn sqlite3_column_text(s: *mut Stmt, i: c_int) -> *const c_char;
    fn sqlite3_column_blob(s: *mut Stmt, i: c_int) -> *const c_void;
    fn sqlite3_column_bytes(s: *mut Stmt, i: c_int) -> c_int;
}
fn check(x: c_int) {
    assert_eq!(x, 0, "SQLite result code");
}
struct Statement(*mut Stmt);
impl Statement {
    fn new(db: *mut Db, q: &str) -> Self {
        let mut p = ptr::null_mut();
        check(unsafe {
            sqlite3_prepare_v2(
                db,
                CString::new(q).unwrap().as_ptr(),
                -1,
                &mut p,
                ptr::null_mut(),
            )
        });
        Self(p)
    }
    fn integer(&self, i: i32, v: u64) {
        check(unsafe { sqlite3_bind_int64(self.0, i, v as i64) });
    }
    // SQLITE_STATIC: the caller's slice stays alive through step, reset and clear_bindings.
    fn blob(&self, i: i32, v: &[u8]) {
        check(unsafe { sqlite3_bind_blob64(self.0, i, v.as_ptr().cast(), v.len() as u64, None) });
    }
    fn step(&self) -> c_int {
        unsafe { sqlite3_step(self.0) }
    }
    fn done(&self) {
        assert_eq!(self.step(), 101);
        self.reset();
    }
    fn reset(&self) {
        check(unsafe { sqlite3_reset(self.0) });
        check(unsafe { sqlite3_clear_bindings(self.0) });
    }
    fn number(&self, i: i32) -> u64 {
        unsafe { sqlite3_column_int64(self.0, i) as u64 }
    }
    fn bytes(&self, i: i32) -> &[u8] {
        unsafe {
            let n = sqlite3_column_bytes(self.0, i);
            assert!(n > 0);
            std::slice::from_raw_parts(sqlite3_column_blob(self.0, i).cast(), n as usize)
        }
    }
}
impl Drop for Statement {
    fn drop(&mut self) {
        check(unsafe { sqlite3_finalize(self.0) });
    }
}
pub struct Sql {
    frame: Statement,
    row: Statement,
    meta: Statement,
    head: Statement,
    set: Statement,
    fp: Statement,
    rp: Statement,
    db: *mut Db,
    write: bool,
}
impl Sql {
    pub fn open(p: &Path, c: &Config, write: bool, create: bool) -> Result<Self> {
        if unsafe { sqlite3_threadsafe() } == 0 {
            return Err("SQLite was compiled SQLITE_THREADSAFE=0".into());
        }
        let mut db = ptr::null_mut();
        let flags = if write { 2 | 4 } else { 1 };
        // FULLMUTEX explicitly requests serialized connections even for THREADSAFE=2 builds.
        check(unsafe {
            sqlite3_open_v2(
                CString::new(p.to_str().unwrap())?.as_ptr(),
                &mut db,
                flags | 0x10000,
                ptr::null(),
            )
        });
        check(unsafe { sqlite3_busy_timeout(db, 60000) });
        let exec = |q: &str| {
            let x = unsafe {
                sqlite3_exec(
                    db,
                    CString::new(q).unwrap().as_ptr(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            assert_eq!(x, 0, "{}: {}", q, unsafe {
                CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy()
            });
        };
        if create {
            exec(&format!(
                "PRAGMA page_size={}; PRAGMA journal_mode=WAL;",
                c.page
            ));
        }
        exec(&format!(
            "PRAGMA synchronous=FULL;PRAGMA cache_size=-{};PRAGMA mmap_size={};",
            c.cache / 1024,
            c.mmap
        ));
        if create {
            exec(
                "CREATE TABLE frames(seq INTEGER PRIMARY KEY,scan INTEGER NOT NULL,frame INTEGER NOT NULL,id INTEGER NOT NULL,payload BLOB NOT NULL);CREATE TABLE rows(id INTEGER PRIMARY KEY,seq INTEGER NOT NULL UNIQUE,tenant INTEGER NOT NULL,status INTEGER NOT NULL,created INTEGER NOT NULL,amount INTEGER NOT NULL,body BLOB NOT NULL);CREATE TABLE metadata(seq INTEGER PRIMARY KEY,txid INTEGER NOT NULL,source INTEGER NOT NULL,flags INTEGER NOT NULL);CREATE TABLE head(id INTEGER PRIMARY KEY CHECK(id=1),txns INTEGER NOT NULL,frames INTEGER NOT NULL,rows INTEGER NOT NULL,metas INTEGER NOT NULL);INSERT INTO head VALUES(1,0,0,0,0);",
            );
        }
        if create && c.lookups && c.scenario == "mixed" {
            exec("CREATE UNIQUE INDEX frame_key ON frames(scan,frame)");
        }
        let frame = Statement::new(
            db,
            if write {
                "INSERT INTO frames VALUES(?1,?2,?3,?4,?5)"
            } else {
                "SELECT id,scan,frame,payload FROM frames WHERE seq>=?1 AND seq<?2 ORDER BY seq"
            },
        );
        let row = Statement::new(
            db,
            if write {
                "INSERT INTO rows VALUES(?1,?2,?3,?4,?5,?6,?7)"
            } else {
                "SELECT id,tenant,status,created,amount,body FROM rows WHERE seq>=?1 AND seq<?2 ORDER BY seq"
            },
        );
        let meta = Statement::new(
            db,
            if write {
                "INSERT INTO metadata VALUES(?1,?2,?3,?4)"
            } else {
                "SELECT txid,source,flags FROM metadata WHERE seq>=?1 AND seq<?2 ORDER BY seq"
            },
        );
        let head = Statement::new(db, "SELECT txns,frames,rows,metas FROM head WHERE id=1");
        let set = Statement::new(
            db,
            "UPDATE head SET txns=?1,frames=?2,rows=?3,metas=?4 WHERE id=1",
        );
        let fp = Statement::new(
            db,
            "SELECT id,payload FROM frames WHERE scan=?1 AND frame=?2",
        );
        let rp = Statement::new(
            db,
            "SELECT id,tenant,status,created,amount,body FROM rows WHERE id=?1",
        );
        Ok(Self {
            frame,
            row,
            meta,
            head,
            set,
            fp,
            rp,
            db,
            write,
        })
    }
    pub fn exec(&self, q: &str) {
        check(unsafe {
            sqlite3_exec(
                self.db,
                CString::new(q).unwrap().as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        });
    }
    pub fn head(&self) -> Head {
        assert_eq!(self.head.step(), 100);
        let h = Head {
            key: 0,
            txns: self.head.number(0),
            frames: self.head.number(1),
            rows: self.head.number(2),
            metas: self.head.number(3),
        };
        self.head.reset();
        assert!(h.valid());
        h
    }
    pub fn append(&self, t: &Transaction) -> Head {
        self.exec("BEGIN IMMEDIATE");
        let mut h = self.head();
        let meta = |flag| {
            let m = t.metadata(flag);
            for (i, v) in [
                h.metas + flag as u64,
                m.txid,
                m.source as u64,
                m.flags as u64,
            ]
            .iter()
            .enumerate()
            {
                self.meta.integer(i as i32 + 1, *v);
            }
            self.meta.done();
        };
        meta(0);
        for f in &t.frames {
            for (i, v) in [h.frames, f.scan as u64, f.frame as u64, f.id]
                .iter()
                .enumerate()
            {
                self.frame.integer(i as i32 + 1, *v);
            }
            self.frame.blob(5, &f.payload);
            self.frame.done();
            h.frames += 1;
        }
        for r in &t.rows {
            for (i, v) in [
                r.id,
                h.rows,
                r.tenant as u64,
                r.status as u64,
                r.created,
                r.amount as u64,
            ]
            .iter()
            .enumerate()
            {
                self.row.integer(i as i32 + 1, *v);
            }
            self.row.blob(7, &r.body);
            self.row.done();
            h.rows += 1;
        }
        meta(1);
        h.metas += 2;
        h.txns += 1;
        for (i, v) in [h.txns, h.frames, h.rows, h.metas].iter().enumerate() {
            self.set.integer(i as i32 + 1, *v);
        }
        self.set.done();
        crate::before_publish(t.txid);
        self.exec("COMMIT");
        h
    }
    pub fn refresh(&self) -> Head {
        self.exec("COMMIT;BEGIN");
        self.head()
    }
    pub fn consume(&self, old: &Head, h: &Head, o: &mut Oracle, c: &Config) {
        self.frame.integer(1, old.frames);
        self.frame.integer(2, h.frames);
        loop {
            let x = self.frame.step();
            if x == 101 {
                break;
            }
            assert_eq!(x, 100);
            let (id, scan, frame) = (
                self.frame.number(0),
                self.frame.number(1) as u32,
                self.frame.number(2) as u32,
            );
            let p = self.frame.bytes(3);
            o.frame_view(id, scan, frame, p);
            if c.lookups {
                self.fp.integer(1, scan as u64);
                self.fp.integer(2, frame as u64);
                assert_eq!(self.fp.step(), 100);
                assert_eq!(self.fp.number(0), id);
                assert_eq!(self.fp.bytes(1), p);
                o.point_payload(id, self.fp.bytes(1));
                self.fp.reset();
                o.lookup_count += 1;
            }
        }
        self.frame.reset();
        self.row.integer(1, old.rows);
        self.row.integer(2, h.rows);
        loop {
            let x = self.row.step();
            if x == 101 {
                break;
            }
            assert_eq!(x, 100);
            let r = read_row(&self.row);
            o.row(&r);
            if c.lookups {
                self.rp.integer(1, r.id);
                assert_eq!(self.rp.step(), 100);
                assert_eq!(read_row(&self.rp), r);
                self.rp.reset();
                o.lookup_count += 1;
            }
        }
        self.row.reset();
        self.meta.integer(1, old.metas);
        self.meta.integer(2, h.metas);
        loop {
            let x = self.meta.step();
            if x == 101 {
                break;
            }
            assert_eq!(x, 100);
            o.metadata(&Metadata {
                txid: self.meta.number(0),
                source: self.meta.number(1) as u32,
                flags: self.meta.number(2) as u32,
            });
        }
        self.meta.reset();
    }
    pub fn settings(&self) -> serde_json::Value {
        let integer = |name: &str| {
            let s = Statement::new(self.db, &format!("PRAGMA {name}"));
            assert_eq!(s.step(), 100);
            s.number(0) as i64
        };
        let mode = Statement::new(self.db, "PRAGMA journal_mode");
        assert_eq!(mode.step(), 100);
        let journal = unsafe {
            CStr::from_ptr(sqlite3_column_text(mode.0, 0))
                .to_string_lossy()
                .into_owned()
        };
        serde_json::json!({"page_size":integer("page_size"),"cache_size":integer("cache_size"),"mmap_upper_limit":integer("mmap_size"),"synchronous":integer("synchronous"),"journal_mode":journal})
    }
    pub fn finish(&self) {
        if self.write {
            self.exec("PRAGMA wal_checkpoint(TRUNCATE)");
        }
    }
}
fn read_row(s: &Statement) -> Row {
    Row {
        id: s.number(0),
        tenant: s.number(1) as u32,
        status: s.number(2) as u32,
        created: s.number(3),
        amount: s.number(4) as i64,
        body: s.bytes(5).try_into().unwrap(),
    }
}
// Finalize statements explicitly before closing their connection; null finalization is a no-op.
impl Drop for Sql {
    fn drop(&mut self) {
        for p in [
            &mut self.frame,
            &mut self.row,
            &mut self.meta,
            &mut self.head,
            &mut self.set,
            &mut self.fp,
            &mut self.rp,
        ] {
            let old = std::mem::replace(&mut p.0, ptr::null_mut());
            check(unsafe { sqlite3_finalize(old) });
        }
        check(unsafe { sqlite3_close(self.db) });
    }
}
pub fn facts() -> serde_json::Value {
    serde_json::json!({"version":unsafe{CStr::from_ptr(sqlite3_libversion()).to_str().unwrap()},"compile_threadsafe":unsafe{sqlite3_threadsafe()},"connection_mode":"SQLITE_OPEN_FULLMUTEX","writers":"multiple connections; one active write transaction per database"})
}
