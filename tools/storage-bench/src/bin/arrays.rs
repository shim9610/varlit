// Handles live on worker stacks; avoid an extra benchmark-only allocation/indirection.
#![allow(clippy::large_enum_variant)]
//! Logical samples, independently optimized physical representations, identical query outputs.
use std::{
    ffi::{CString, c_char, c_int, c_void},
    path::{Path, PathBuf},
    time::Instant,
};
use varlit::*;
type R<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const CHANNELS: usize = 16;
const SAMPLE: usize = 128;
const EPOCH: usize = 16 << 20;
const OUTPUT: usize = 4 << 20;
#[derive(VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable")]
struct Chunk {
    #[varlit(field_id = 1)]
    first: u64,
    #[varlit(field_id = 2)]
    data: Vec<u8>,
}
#[derive(VarlitBlock)]
#[varlit(id = 2, version = 1, kind = "variable", key = "first")]
struct KeyChunk {
    #[varlit(field_id = 1)]
    first: u64,
    #[varlit(field_id = 2)]
    data: Vec<u8>,
}
varlit_format! {pub format Matrix64{magic:b"OPTMA64";version:1;integrity:crc32_with_header;dims{chunk:u32,slot:u32,}commit:cell_bitmap{keyspace=[chunk,slot];categories=[data];};blocks{matrix Cell64(id=11,dims=[chunk,slot],category=data){data:[u8;65536]}}}}
varlit_format! {pub format Matrix1m{magic:b"OPTMA1M";version:1;integrity:crc32_with_header;dims{chunk:u32,slot:u32,}commit:cell_bitmap{keyspace=[chunk,slot];categories=[data];};blocks{matrix Cell1m(id=12,dims=[chunk,slot],category=data){data:[u8;1048576]}}}}
static BLOCKS: &[BlockDescriptor] = &[
    BlockDescriptor {
        id: 1,
        name: "Chunk",
        version: 1,
        kind: BlockKind::Variable,
        fields: &[],
    },
    BlockDescriptor {
        id: 2,
        name: "KeyChunk",
        version: 1,
        kind: BlockKind::Variable,
        fields: &[],
    },
];
static KEYS: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<KeyChunk>()];
fn spec(index: bool, native: bool) -> FormatSpec {
    let s = FormatSpec::new(
        b"SAMPLE16",
        1,
        Endian::Little,
        0,
        if index {
            IndexPolicy::KeyedOffsetChain
        } else {
            IndexPolicy::BlockOffsetChain
        },
        IntegrityPolicy::Crc32,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_block_identities(&[Chunk::IDENTITY, KeyChunk::IDENTITY]);
    if native {
        s.with_commit_policy(CommitPolicy::TransactionMarker(
            TransactionMarkerMode::Explicit,
        ))
    } else {
        s
    }
}
fn opts() -> DiskIndexOptions {
    DiskIndexOptions {
        cache_bytes: 8 << 20,
        ..Default::default()
    }
}
fn sopts() -> StreamOptions {
    StreamOptions {
        state: opts(),
        ..Default::default()
    }
}
fn plan() -> DiskIndexPlan {
    DiskIndexPlan::canonical(spec(true, false), KEYS).unwrap()
}
#[inline]
fn bits(id: usize, col: usize) -> u64 {
    0x3ff0000000000000 | ((id as u64 * 131 + col as u64 * 17) & 0x000fffffffffffff)
}
fn fill(first: usize, out: &mut [u8]) {
    assert!(out.len().is_multiple_of(SAMPLE));
    for (j, s) in out.chunks_exact_mut(SAMPLE).enumerate() {
        for c in 0..CHANNELS {
            s[c * 8..c * 8 + 8].copy_from_slice(&bits(first + j, c).to_le_bytes());
        }
    }
}
fn verify(first: usize, out: &[u8]) {
    assert!(out.len().is_multiple_of(SAMPLE));
    for (j, s) in out.chunks_exact(SAMPLE).enumerate() {
        for c in 0..CHANNELS {
            assert_eq!(
                u64::from_le_bytes(s[c * 8..c * 8 + 8].try_into().unwrap()),
                bits(first + j, c)
            );
        }
    }
    std::hint::black_box(out);
}
fn borrowed(bytes: &[u8]) -> R<(usize, &[u8])> {
    let mut d = Decoder::new(bytes, Endian::Little);
    let (mut first, mut data) = (None, None);
    while d.remaining() > 0 {
        let h = read_field_header(&mut d)?;
        let b = d.read_exact(h.payload_len as usize)?;
        match h.field_id {
            1 => {
                assert_eq!(h.wire_type, WireType::U64);
                assert_eq!(b.len(), 8);
                first = Some(u64::from_le_bytes(b.try_into().unwrap()) as usize);
            }
            2 => {
                assert_eq!(h.wire_type, WireType::Bytes);
                let mut v = Decoder::new(b, Endian::Little);
                let n = v.read_len()?;
                let p = v.read_exact(n)?;
                assert_eq!(v.remaining(), 0);
                data = Some(p);
            }
            _ => return Err("unexpected field in pinned benchmark schema".into()),
        }
    }
    Ok((first.ok_or("missing first")?, data.ok_or("missing data")?))
}
enum Db {}
enum Stmt {}
enum SqlBlob {}
#[link(name = "sqlite3")]
unsafe extern "C" {
    fn sqlite3_libversion() -> *const c_char;
    fn sqlite3_open(p: *const c_char, d: *mut *mut Db) -> c_int;
    fn sqlite3_close(d: *mut Db) -> c_int;
    fn sqlite3_busy_timeout(d: *mut Db, ms: c_int) -> c_int;
    fn sqlite3_exec(
        d: *mut Db,
        s: *const c_char,
        cb: *mut c_void,
        a: *mut c_void,
        e: *mut *mut c_char,
    ) -> c_int;
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
    fn sqlite3_bind_double(s: *mut Stmt, i: c_int, v: f64) -> c_int;
    fn sqlite3_bind_blob64(
        s: *mut Stmt,
        i: c_int,
        b: *const c_void,
        n: u64,
        d: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_column_double(s: *mut Stmt, i: c_int) -> f64;
    fn sqlite3_column_int64(s: *mut Stmt, i: c_int) -> i64;
    fn sqlite3_column_text(s: *mut Stmt, i: c_int) -> *const c_char;
    fn sqlite3_column_blob(s: *mut Stmt, i: c_int) -> *const c_void;
    fn sqlite3_column_bytes(s: *mut Stmt, i: c_int) -> c_int;
    fn sqlite3_blob_open(
        d: *mut Db,
        db: *const c_char,
        t: *const c_char,
        c: *const c_char,
        row: i64,
        w: c_int,
        b: *mut *mut SqlBlob,
    ) -> c_int;
    fn sqlite3_blob_reopen(b: *mut SqlBlob, row: i64) -> c_int;
    fn sqlite3_blob_read(b: *mut SqlBlob, out: *mut c_void, n: c_int, o: c_int) -> c_int;
    fn sqlite3_blob_close(b: *mut SqlBlob) -> c_int;
}
fn ok(x: c_int) {
    assert_eq!(x, 0, "SQLite result code");
}
struct Sql {
    db: *mut Db,
    stmt: *mut Stmt,
    blob: *mut SqlBlob,
    columns: bool,
    group: usize,
    point: bool,
    blob_row: Option<usize>,
}
impl Sql {
    fn open(p: &Path, columns: bool, group: usize, page: usize, mmap: usize, write: bool) -> Self {
        unsafe {
            let name = CString::new(p.to_str().unwrap()).unwrap();
            let mut db = std::ptr::null_mut();
            ok(sqlite3_open(name.as_ptr(), &mut db));
            ok(sqlite3_busy_timeout(db, 5000));
            let mut s = Self {
                db,
                stmt: std::ptr::null_mut(),
                blob: std::ptr::null_mut(),
                columns,
                group,
                point: false,
                blob_row: None,
            };
            if write {
                s.exec(&format!(
                    "PRAGMA page_size={page}; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;"
                ));
            }
            s.exec(&format!(
                "PRAGMA cache_size=-8192; PRAGMA mmap_size={mmap};"
            ));
            if write {
                if columns {
                    let cols = (0..16)
                        .map(|i| format!(",v{i} REAL NOT NULL"))
                        .collect::<String>();
                    s.exec(&format!(
                        "CREATE TABLE samples(id INTEGER PRIMARY KEY{cols});"
                    ));
                    let qs = (1..=17)
                        .map(|i| format!("?{i}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    s.prepare(&format!("INSERT INTO samples VALUES({qs})"));
                } else {
                    s.exec("CREATE TABLE chunks(first INTEGER PRIMARY KEY, data BLOB NOT NULL);");
                    s.prepare("INSERT INTO chunks VALUES(?1,?2)");
                }
            } else {
                s.exec("BEGIN");
            }
            s
        }
    }
    fn exec(&self, q: &str) {
        unsafe {
            let q = CString::new(q).unwrap();
            ok(sqlite3_exec(
                self.db,
                q.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ));
        }
    }
    fn prepare(&mut self, q: &str) {
        unsafe {
            if !self.stmt.is_null() {
                ok(sqlite3_finalize(self.stmt));
            }
            self.stmt = std::ptr::null_mut();
            let q = CString::new(q).unwrap();
            ok(sqlite3_prepare_v2(
                self.db,
                q.as_ptr(),
                -1,
                &mut self.stmt,
                std::ptr::null_mut(),
            ));
        }
    }
    fn put(&mut self, first: usize, b: &[u8]) {
        unsafe {
            if self.columns {
                for (j, s) in b.chunks_exact(SAMPLE).enumerate() {
                    ok(sqlite3_bind_int64(self.stmt, 1, (first + j) as i64));
                    for c in 0..16 {
                        let v = f64::from_le_bytes(s[c * 8..c * 8 + 8].try_into().unwrap());
                        ok(sqlite3_bind_double(self.stmt, (c + 2) as c_int, v));
                    }
                    assert_eq!(sqlite3_step(self.stmt), 101);
                    ok(sqlite3_reset(self.stmt));
                }
            } else {
                ok(sqlite3_bind_int64(self.stmt, 1, first as i64));
                ok(sqlite3_bind_blob64(
                    self.stmt,
                    2,
                    b.as_ptr().cast(),
                    b.len() as u64,
                    None,
                ));
                assert_eq!(sqlite3_step(self.stmt), 101);
                ok(sqlite3_reset(self.stmt));
                ok(sqlite3_clear_bindings(self.stmt));
            }
        }
    }
    fn selected_columns() -> String {
        (0..16)
            .map(|i| format!("v{i}"))
            .collect::<Vec<_>>()
            .join(",")
    }
    fn scan_begin(&mut self) {
        if self.columns {
            self.prepare(&format!(
                "SELECT {} FROM samples ORDER BY id",
                Self::selected_columns()
            ));
        } else {
            self.prepare("SELECT data FROM chunks ORDER BY first");
        }
    }
    fn scan_batch(&mut self, out: &mut [u8]) {
        unsafe {
            if self.columns {
                for s in out.chunks_exact_mut(SAMPLE) {
                    assert_eq!(sqlite3_step(self.stmt), 100);
                    for c in 0..16 {
                        s[c * 8..c * 8 + 8].copy_from_slice(
                            &sqlite3_column_double(self.stmt, c as c_int).to_le_bytes(),
                        );
                    }
                }
            } else {
                for b in out.chunks_mut(self.group) {
                    assert_eq!(sqlite3_step(self.stmt), 100);
                    let n = sqlite3_column_bytes(self.stmt, 0) as usize;
                    assert_eq!(n, b.len());
                    let p = std::slice::from_raw_parts(
                        sqlite3_column_blob(self.stmt, 0).cast::<u8>(),
                        n,
                    );
                    b.copy_from_slice(p);
                }
            }
        }
    }
    fn point_begin(&mut self) {
        self.point = true;
        if self.columns {
            self.prepare(&format!(
                "SELECT {} FROM samples WHERE id=?1",
                Self::selected_columns()
            ));
        }
    }
    fn query_begin(&mut self) {
        self.point = false;
        if self.columns {
            self.prepare(&format!(
                "SELECT {} FROM samples WHERE id>=?1 AND id<?2 ORDER BY id",
                Self::selected_columns()
            ));
        } else if !self.stmt.is_null() {
            unsafe {
                ok(sqlite3_finalize(self.stmt));
            }
            self.stmt = std::ptr::null_mut();
        }
    }
    fn range(&mut self, first: usize, out: &mut [u8]) {
        unsafe {
            if self.columns {
                ok(sqlite3_bind_int64(self.stmt, 1, first as i64));
                if !self.point {
                    ok(sqlite3_bind_int64(
                        self.stmt,
                        2,
                        (first + out.len() / SAMPLE) as i64,
                    ));
                } else {
                    assert_eq!(out.len(), SAMPLE);
                }
                for s in out.chunks_exact_mut(SAMPLE) {
                    assert_eq!(sqlite3_step(self.stmt), 100);
                    for c in 0..16 {
                        s[c * 8..c * 8 + 8].copy_from_slice(
                            &sqlite3_column_double(self.stmt, c as c_int).to_le_bytes(),
                        );
                    }
                }
                assert_eq!(sqlite3_step(self.stmt), 101);
                ok(sqlite3_reset(self.stmt));
            } else {
                let mut pos = first * SAMPLE;
                let mut done = 0;
                while done < out.len() {
                    let base = pos / self.group * self.group;
                    let offset = pos - base;
                    let n = (self.group - offset).min(out.len() - done);
                    if self.blob.is_null() {
                        ok(sqlite3_blob_open(
                            self.db,
                            c"main".as_ptr(),
                            c"chunks".as_ptr(),
                            c"data".as_ptr(),
                            (base / SAMPLE) as i64,
                            0,
                            &mut self.blob,
                        ));
                    } else if self.blob_row != Some(base / SAMPLE) {
                        ok(sqlite3_blob_reopen(self.blob, (base / SAMPLE) as i64));
                    }
                    self.blob_row = Some(base / SAMPLE);
                    ok(sqlite3_blob_read(
                        self.blob,
                        out[done..].as_mut_ptr().cast(),
                        n as c_int,
                        offset as c_int,
                    ));
                    done += n;
                    pos += n;
                }
            }
        }
    }
    fn facts(&mut self) -> serde_json::Value {
        let mut facts = serde_json::Map::new();
        for q in [
            "PRAGMA page_size",
            "PRAGMA cache_size",
            "PRAGMA mmap_size",
            "PRAGMA journal_mode",
            "PRAGMA synchronous",
        ] {
            self.prepare(q);
            assert_eq!(unsafe { sqlite3_step(self.stmt) }, 100);
            let text = unsafe { std::ffi::CStr::from_ptr(sqlite3_column_text(self.stmt, 0)) }
                .to_string_lossy()
                .into_owned();
            facts.insert(q.into(), text.into());
        }
        let q = if self.columns {
            format!(
                "EXPLAIN QUERY PLAN SELECT {} FROM samples WHERE id>=12 AND id<4108 ORDER BY id",
                Self::selected_columns()
            )
        } else {
            "EXPLAIN QUERY PLAN SELECT data FROM chunks WHERE first=512".into()
        };
        self.prepare(&q);
        let mut plans = Vec::new();
        unsafe {
            while sqlite3_step(self.stmt) == 100 {
                plans.push(
                    std::ffi::CStr::from_ptr(sqlite3_column_text(self.stmt, 3))
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        facts.insert("query_plan".into(), serde_json::json!(plans));
        facts.into()
    }
}
impl Drop for Sql {
    fn drop(&mut self) {
        unsafe {
            if !self.blob.is_null() {
                ok(sqlite3_blob_close(self.blob));
            }
            if !self.stmt.is_null() {
                ok(sqlite3_finalize(self.stmt));
            }
            ok(sqlite3_close(self.db));
        }
    }
}
enum Writer {
    Hdf(Hdf),
    Matrix(VarlitFile, usize),
    Native(VarlitFile),
    Stream(VarlitStreamWriter),
    Index(VarlitIndexedWriter),
    Sql(Sql),
}
impl Writer {
    fn open(mode: &str, p: &Path, g: usize, page: usize, mmap: usize, total: usize) -> R<Self> {
        Ok(match mode {
            mode if mode.starts_with("hdf-") => Self::Hdf(Hdf::create(p, mode, g, total)),
            "matrix-mmap" | "matrix-owned" => {
                let sp = if g == 65536 {
                    Matrix64::spec()
                } else {
                    Matrix1m::spec()
                };
                Self::Matrix(
                    sp.create_with_dims(
                        p,
                        MatrixDimensions::from_pairs([("chunk", (total / g) as u64), ("slot", 1)]),
                    )?,
                    g,
                )
            }
            "native-view" | "native-owned" | "native-mmap" => {
                Self::Native(spec(false, true).create(p)?)
            }
            "stream" => Self::Stream(VarlitStreamWriter::create(spec(false, false), p, sopts())?),
            "indexed" => Self::Index(VarlitIndexedWriter::create(
                spec(true, false),
                p,
                opts(),
                plan(),
            )?),
            "sql-columns" | "sql-chunks" => {
                Self::Sql(Sql::open(p, mode == "sql-columns", g, page, mmap, true))
            }
            _ => panic!("mode"),
        })
    }
    fn epoch(&mut self, bank: &[Chunk], keyed: &[KeyChunk]) -> R<()> {
        match self {
            Self::Hdf(h) => h.epoch(bank),
            Self::Matrix(f, g) => {
                for c in bank {
                    let key = MatrixKey::new(c.first * SAMPLE as u64 / (*g as u64), 0);
                    if *g == 65536 {
                        f.write_matrix_cell_payload::<Cell64>(key, &c.data)?;
                        f.commit_matrix_cell::<Cell64>(key)?;
                    } else {
                        f.write_matrix_cell_payload::<Cell1m>(key, &c.data)?;
                        f.commit_matrix_cell::<Cell1m>(key)?;
                    }
                }
                f.sync()?;
            }
            Self::Native(f) => {
                for c in bank {
                    f.push_info(c)?;
                }
                f.commit_durable()?;
            }
            Self::Stream(f) => {
                f.push_iter::<Chunk, _>(bank, BatchOptions::default())?;
                f.sync()?;
            }
            Self::Index(f) => {
                f.push_iter::<KeyChunk, _>(keyed, BatchOptions::default())?;
                f.sync()?;
            }
            Self::Sql(s) => {
                s.exec("BEGIN IMMEDIATE");
                for c in bank {
                    s.put(c.first as usize, &c.data);
                }
                s.exec("COMMIT");
            }
        }
        Ok(())
    }
    fn finish(&mut self) {
        if let Self::Sql(s) = self {
            s.exec("PRAGMA wal_checkpoint(TRUNCATE)");
        }
    }
}
enum Reader {
    NativeMap(MmapPayloads),
    MatrixOwned(VarlitFile),
    Hdf(Hdf),
    Matrix(MmapMatrix),
    Native(VarlitFile, Vec<RecordIndexEntry>, bool),
    Stream(VarlitStreamReader, StreamingBlocks<Chunk>),
    Index(VarlitIndexedReader, Option<StreamingBlocks<KeyChunk>>),
    Sql(Sql),
}
impl Reader {
    fn open(mode: &str, p: &Path, g: usize, page: usize, mmap: usize) -> R<Self> {
        Ok(match mode {
            mode if mode.starts_with("hdf-") => Self::Hdf(Hdf::open(p, mode)),
            "matrix-owned" => {
                let sp = if g == 65536 {
                    Matrix64::spec()
                } else {
                    Matrix1m::spec()
                };
                Self::MatrixOwned(VarlitFile::open_readonly(sp, p)?)
            }
            "matrix-mmap" => {
                let sp = if g == 65536 {
                    Matrix64::spec()
                } else {
                    Matrix1m::spec()
                };
                let f = VarlitFile::open_readonly(sp, p)?;
                // SAFETY: every writer is closed before constructing this read-only mapping;
                // the backing files remain immutable until all reader threads and mappings end.
                Self::Matrix(unsafe { f.mmap_matrix()? })
            }
            "native-mmap" => {
                let f = VarlitFile::open_readonly(spec(false, true), p)?;
                // SAFETY: all writers are closed before opening this mapping;
                // its private workload files are immutable until every reader exits.
                Self::NativeMap(unsafe { f.mmap_payloads()? })
            }
            "native-view" | "native-owned" => {
                let f = VarlitFile::open_readonly(spec(false, true), p)?;
                let mut entries = Vec::new();
                f.block_entries_into::<Chunk>(&mut entries)?;
                Self::Native(f, entries, mode == "native-view")
            }
            "stream" => {
                let f = VarlitStreamReader::open(spec(false, false), p, sopts())?;
                {
                    let iter = f.blocks::<Chunk>()?;
                    Self::Stream(f, iter)
                }
            }
            "indexed" => Self::Index(
                VarlitIndexedReader::open(spec(true, false), p, opts(), plan())?,
                None,
            ),
            "sql-columns" | "sql-chunks" => {
                Self::Sql(Sql::open(p, mode == "sql-columns", g, page, mmap, false))
            }
            _ => panic!("mode"),
        })
    }
    fn scan_begin(&mut self) {
        match self {
            Self::Stream(f, iter) => {
                *iter = f.blocks::<Chunk>().expect("reset immutable stream cursor");
            }
            Self::Sql(s) => s.scan_begin(),
            Self::Index(f, iter) => {
                *iter = Some(f.blocks::<KeyChunk>().expect("indexed scan cursor"))
            }
            _ => {}
        }
    }
    fn range(&mut self, first: usize, out: &mut [u8], g: usize, scratch: &mut Vec<u8>) -> R<()> {
        if let Self::Hdf(h) = self {
            h.range(first, out);
            return Ok(());
        }
        if let Self::Sql(s) = self {
            s.range(first, out);
            return Ok(());
        }
        if let Self::Stream(_, iter) = self {
            let mut done = 0;
            while done < out.len() {
                let c = iter.next().ok_or("short stream")??;
                assert_eq!(c.first as usize, first + done / SAMPLE);
                let n = c.data.len();
                assert!(n <= out.len() - done);
                out[done..done + n].copy_from_slice(&c.data);
                done += n;
            }
            return Ok(());
        }
        let mut pos = first * SAMPLE;
        let mut done = 0;
        while done < out.len() {
            let chunk = pos / g;
            let start = chunk * g / SAMPLE;
            let offset = pos % g;
            let n = (g - offset).min(out.len() - done);
            match self {
                Self::NativeMap(m) => {
                    let encoded = m
                        .block_payload_window::<Chunk>(chunk)?
                        .ok_or("missing mapped chunk")?;
                    let (first, b) = borrowed(encoded)?;
                    assert_eq!(first, start);
                    assert_eq!(b.len(), g);
                    out[done..done + n].copy_from_slice(&b[offset..offset + n]);
                }
                Self::MatrixOwned(f) => {
                    scratch.resize(g, 0);
                    let key = MatrixKey::new(chunk as u64, 0);
                    if g == 65536 {
                        f.read_matrix_run_bytes_into::<Cell64>(key, scratch)?;
                    } else {
                        f.read_matrix_run_bytes_into::<Cell1m>(key, scratch)?;
                    }
                    out[done..done + n].copy_from_slice(&scratch[offset..offset + n]);
                }
                Self::Matrix(m) => {
                    let key = MatrixKey::new(chunk as u64, 0);
                    let b = if g == 65536 {
                        m.cell_payload_window::<Cell64>(key)?
                    } else {
                        m.cell_payload_window::<Cell1m>(key)?
                    };
                    assert_eq!(b.len(), g);
                    out[done..done + n].copy_from_slice(&b[offset..offset + n]);
                }
                Self::Native(f, e, view) => {
                    if *view {
                        f.read_logical_payload_into(&e[chunk], scratch)?;
                        let (key, b) = borrowed(scratch)?;
                        assert_eq!(key, start);
                        assert_eq!(b.len(), g);
                        out[done..done + n].copy_from_slice(&b[offset..offset + n]);
                    } else {
                        let c: Chunk = f.decode_block_into(&e[chunk], scratch)?;
                        assert_eq!(c.first as usize, start);
                        assert_eq!(c.data.len(), g);
                        out[done..done + n].copy_from_slice(&c.data[offset..offset + n]);
                    }
                }
                Self::Index(f, _) => {
                    let c: KeyChunk = f.get(&(start as u64))?.ok_or("missing chunk")?;
                    assert_eq!(c.first as usize, start);
                    out[done..done + n].copy_from_slice(&c.data[offset..offset + n]);
                }
                _ => unreachable!(),
            }
            done += n;
            pos += n;
        }
        Ok(())
    }
    fn scan_batch(
        &mut self,
        first: usize,
        out: &mut [u8],
        g: usize,
        scratch: &mut Vec<u8>,
    ) -> R<()> {
        if let Self::Hdf(h) = self {
            h.range(first, out);
            return Ok(());
        }
        if let Self::Sql(s) = self {
            s.scan_batch(out);
            return Ok(());
        }
        if let Self::MatrixOwned(f) = self {
            let key = MatrixKey::new((first * SAMPLE / g) as u64, 0);
            if g == 65536 {
                f.read_matrix_run_bytes_into::<Cell64>(key, out)?;
            } else {
                f.read_matrix_run_bytes_into::<Cell1m>(key, out)?;
            }
            return Ok(());
        }
        if let Self::Index(_, Some(iter)) = self {
            let mut done = 0;
            while done < out.len() {
                let c = iter.next().ok_or("short indexed scan")??;
                assert_eq!(c.first as usize, first + done / SAMPLE);
                let n = c.data.len();
                out[done..done + n].copy_from_slice(&c.data);
                done += n;
            }
            return Ok(());
        }
        self.range(first, out, g, scratch)
    }
    fn point_begin(&mut self) {
        if let Self::Sql(s) = self {
            s.point_begin();
        }
    }
    fn query_begin(&mut self) {
        if let Self::Sql(s) = self {
            s.query_begin();
        }
        if let Self::Index(_, iter) = self {
            *iter = None;
        }
    }
}
fn rand(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}
#[repr(C)]
struct Ts {
    s: std::ffi::c_long,
    n: std::ffi::c_long,
}
unsafe extern "C" {
    fn clock_gettime(i: c_int, t: *mut Ts) -> c_int;
}
fn cpu() -> f64 {
    let mut t = Ts { s: 0, n: 0 };
    assert_eq!(unsafe { clock_gettime(2, &mut t) }, 0);
    t.s as f64 + t.n as f64 / 1e9
}
fn main() -> R<()> {
    let a: Vec<_> = std::env::args().collect();
    let mode = &a[1];
    let path = PathBuf::from(&a[2]).join("samples.bin");
    let total: usize = a[3].parse()?;
    let g: usize = a[4].parse()?;
    let page: usize = a[5].parse()?;
    let mmap: usize = a[6].parse()?;
    let points: usize = a[7].parse()?;
    let ranges: usize = a[8].parse()?;
    let readers: usize = a[9].parse()?;
    assert!(total.is_multiple_of(EPOCH) && EPOCH.is_multiple_of(g) && OUTPUT.is_multiple_of(g));
    if a.get(10).is_some_and(|x| x == "live") {
        return live(mode, &path, total, g, page, mmap, readers);
    }
    let init = Instant::now();
    let c = cpu();
    let mut w = Writer::open(mode, &path, g, page, mmap, total)?;
    let mut write = init.elapsed().as_secs_f64();
    let mut writecpu = cpu() - c;
    let mut generation = 0.0;
    let mut bank = Vec::new();
    let mut keyed = Vec::new();
    for k in 0..EPOCH / g {
        if mode == "indexed" {
            keyed.push(KeyChunk {
                first: (k * g / SAMPLE) as u64,
                data: vec![0; g],
            });
        } else {
            bank.push(Chunk {
                first: (k * g / SAMPLE) as u64,
                data: vec![0; g],
            });
        }
    }
    for off in (0..total).step_by(EPOCH) {
        let t = Instant::now();
        for (k, c) in bank.iter_mut().enumerate() {
            c.first = ((off + k * g) / SAMPLE) as u64;
            fill(c.first as usize, &mut c.data);
        }
        for (k, c) in keyed.iter_mut().enumerate() {
            c.first = ((off + k * g) / SAMPLE) as u64;
            fill(c.first as usize, &mut c.data);
        }
        generation += t.elapsed().as_secs_f64();
        let c = cpu();
        let t = Instant::now();
        w.epoch(&bank, &keyed)?;
        write += t.elapsed().as_secs_f64();
        writecpu += cpu() - c;
    }
    let c = cpu();
    let t = Instant::now();
    w.finish();
    drop(w);
    write += t.elapsed().as_secs_f64();
    writecpu += cpu() - c;
    drop(bank);
    drop(keyed);
    let t = Instant::now();
    let mut r = Reader::open(mode, &path, g, page, mmap)?;
    let open = t.elapsed().as_secs_f64();
    let facts = if let Reader::Sql(s) = &mut r {
        Some(s.facts())
    } else {
        None
    };
    r.scan_begin();
    let mut out = vec![0; OUTPUT];
    let mut scratch = Vec::new();
    let mut scan = 0.0;
    let mut scancpu = 0.0;
    let outer = Instant::now();
    for off in (0..total).step_by(OUTPUT) {
        let c = cpu();
        let t = Instant::now();
        r.scan_batch(off / SAMPLE, &mut out, g, &mut scratch)?;
        scan += t.elapsed().as_secs_f64();
        scancpu += cpu() - c;
        verify(off / SAMPLE, &out);
    }
    let scan_e2e = outer.elapsed().as_secs_f64();
    let mut warm_scan = None;
    let mut warm_scan_cpu = None;
    let repeat_scan = a.get(10).is_some_and(|x| x == "warm-scan");
    if repeat_scan {
        r.scan_begin();
        let (mut elapsed, mut active_cpu) = (0.0, 0.0);
        for off in (0..total).step_by(OUTPUT) {
            let c = cpu();
            let t = Instant::now();
            r.scan_batch(off / SAMPLE, &mut out, g, &mut scratch)?;
            elapsed += t.elapsed().as_secs_f64();
            active_cpu += cpu() - c;
            verify(off / SAMPLE, &out);
        }
        warm_scan = Some(elapsed);
        warm_scan_cpu = Some(active_cpu);
    }
    let (mut point, mut window) = (None, None);
    if mode != "stream" {
        r.point_begin();
        let mut seed = 9610;
        let mut p = vec![0; SAMPLE];
        let mut ttotal = 0.0;
        for _ in 0..points {
            let id = rand(&mut seed) as usize % (total / SAMPLE);
            let t = Instant::now();
            r.range(id, &mut p, g, &mut scratch)?;
            ttotal += t.elapsed().as_secs_f64();
            verify(id, &p);
        }
        point = Some(ttotal);
        r.query_begin();
        let mut q = vec![0; 4096 * SAMPLE];
        let mut ttotal = 0.0;
        for _ in 0..ranges {
            let id = rand(&mut seed) as usize % (total / SAMPLE - 4096);
            let t = Instant::now();
            r.range(id, &mut q, g, &mut scratch)?;
            ttotal += t.elapsed().as_secs_f64();
            verify(id, &q);
        }
        window = Some(ttotal);
    }
    drop(r);
    let mut parallel = None;
    if readers > 0 {
        let t = Instant::now();
        std::thread::scope(|scope| -> R<()> {
            let mut joins = Vec::new();
            for _ in 0..readers {
                let path = &path;
                joins.push(scope.spawn(move || -> R<()> {
                    let mut r = Reader::open(mode, path, g, page, mmap)?;
                    r.scan_begin();
                    let mut b = vec![0; OUTPUT];
                    let mut scratch = Vec::new();
                    for off in (0..total).step_by(OUTPUT) {
                        r.scan_batch(off / SAMPLE, &mut b, g, &mut scratch)?;
                        verify(off / SAMPLE, &b);
                    }
                    Ok(())
                }));
            }
            for j in joins {
                j.join().expect("reader thread panicked")?;
            }
            Ok(())
        })?;
        parallel = Some(t.elapsed().as_secs_f64());
    }
    use std::os::unix::fs::MetadataExt;
    let mut size = 0;
    let mut allocated = 0;
    for p in std::fs::read_dir(&a[2])? {
        let p = p?;
        let m = p.metadata()?;
        if m.is_file() {
            size += m.len();
            allocated += m.blocks() * 512;
        }
    }
    println!(
        "{}",
        serde_json::json!({"mode":mode,"payload_bytes":total,"logical_samples":total/SAMPLE,"channels":16,"sample_bytes":128,"group_bytes":g,"data_units":if mode=="sql-columns"{total/SAMPLE}else{total/g},"page":page,"mmap_requested":mmap,"sqlite_facts":facts,"points":points,"ranges":ranges,"range_samples":4096,"readers":readers,"write_seconds":write,"write_cpu":writecpu,"input_generation_seconds":generation,"open_seconds":open,"scan_seconds":scan,"scan_cpu":scancpu,"scan_e2e_seconds":scan_e2e,"scan_passes":if repeat_scan{2}else{1},"warm_scan_seconds":warm_scan,"warm_scan_cpu":warm_scan_cpu,"point_seconds":point,"range_seconds":window,"parallel_seconds":parallel,"file_bytes":size,"allocated_bytes":allocated,"verified":true,"sqlite_version":unsafe{std::ffi::CStr::from_ptr(sqlite3_libversion()).to_str()?}})
    );
    Ok(())
}

fn thread_cpu() -> f64 {
    let mut t = Ts { s: 0, n: 0 };
    assert_eq!(unsafe { clock_gettime(3, &mut t) }, 0);
    t.s as f64 + t.n as f64 / 1e9
}
fn live(
    mode: &str,
    path: &Path,
    total: usize,
    g: usize,
    page: usize,
    mmap: usize,
    readers: usize,
) -> R<()> {
    assert!(
        (matches!(mode, "stream" | "sql-chunks" | "sql-columns") || mode.starts_with("hdf-swmr"))
            && readers > 0
    );
    let mut writer = Writer::open(mode, path, g, page, mmap, total)?;
    let mut bank: Vec<Chunk> = (0..EPOCH / g)
        .map(|k| Chunk {
            first: (k * g / SAMPLE) as u64,
            data: vec![0; g],
        })
        .collect();
    let ready = std::sync::mpsc::channel();
    let mut backend = 0.0;
    let mut writer_cpu = 0.0;
    let mut generation = 0.0;
    let mut ingest = 0.0;
    let all = Instant::now();
    let bytes = std::thread::scope(|scope| -> R<usize> {
        let mut joins = Vec::new();
        let mut senders = Vec::new();
        for _ in 0..readers {
            let (tx, rx) = std::sync::mpsc::channel::<usize>();
            senders.push(tx);
            let ready = ready.0.clone();
            joins.push(scope.spawn(move || -> R<usize> {
                let mut r = Reader::open(mode, path, g, page, mmap)?;
                match &mut r {
                    Reader::Hdf(h) => assert_eq!(h.rows, 0),
                    Reader::Stream(_, iter) => assert!(iter.next().is_none()),
                    Reader::Sql(s) => {
                        s.prepare(if s.columns {
                            "SELECT count(*) FROM samples"
                        } else {
                            "SELECT count(*) FROM chunks"
                        });
                        unsafe {
                            assert_eq!(sqlite3_step(s.stmt), 100);
                            assert_eq!(sqlite3_column_int64(s.stmt, 0), 0);
                            ok(sqlite3_reset(s.stmt));
                        }
                    }
                    _ => unreachable!(),
                }
                r.query_begin();
                ready.send(())?;
                let mut out = vec![0; OUTPUT];
                let mut scratch = Vec::new();
                let mut processed = 0;
                for off in rx {
                    match &mut r {
                        Reader::Hdf(h) => h.follow(),
                        Reader::Stream(f, iter) => {
                            f.follow_blocks(iter)?;
                        }
                        Reader::Sql(s) => {
                            unsafe {
                                if !s.blob.is_null() {
                                    ok(sqlite3_blob_close(s.blob));
                                    s.blob = std::ptr::null_mut();
                                    s.blob_row = None;
                                }
                                if !s.stmt.is_null() {
                                    ok(sqlite3_reset(s.stmt));
                                }
                            }
                            s.exec("COMMIT; BEGIN");
                        }
                        _ => unreachable!(),
                    }
                    for j in (0..EPOCH).step_by(OUTPUT) {
                        r.range((off + j) / SAMPLE, &mut out, g, &mut scratch)?;
                        verify((off + j) / SAMPLE, &out);
                        processed += out.len();
                    }
                }
                assert_eq!(processed, total);
                Ok(processed)
            }));
        }
        for _ in 0..readers {
            ready.1.recv_timeout(std::time::Duration::from_secs(60))?;
        }
        let start = Instant::now();
        for off in (0..total).step_by(EPOCH) {
            let t = Instant::now();
            for (k, c) in bank.iter_mut().enumerate() {
                c.first = ((off + k * g) / SAMPLE) as u64;
                fill(c.first as usize, &mut c.data);
            }
            generation += t.elapsed().as_secs_f64();
            let tc = thread_cpu();
            let t = Instant::now();
            writer.epoch(&bank, &[])?;
            backend += t.elapsed().as_secs_f64();
            writer_cpu += thread_cpu() - tc;
            for tx in &senders {
                tx.send(off)?;
            }
        }
        ingest = start.elapsed().as_secs_f64();
        drop(senders);
        let mut bytes = 0;
        for j in joins {
            bytes += j.join().expect("live reader panicked")?;
        }
        Ok(bytes)
    })?;
    writer.finish();
    drop(writer);
    let seconds = all.elapsed().as_secs_f64();
    assert_eq!(bytes, readers * total);
    use std::os::unix::fs::MetadataExt;
    let mut allocated = 0;
    let mut file_bytes = 0;
    for p in std::fs::read_dir(path.parent().unwrap())? {
        let m = p?.metadata()?;
        if m.is_file() {
            allocated += m.blocks() * 512;
            file_bytes += m.len();
        }
    }
    println!(
        "{}",
        serde_json::json!({"kind":"live","mode":mode,"payload_bytes":total,"logical_samples":total/SAMPLE,"channels":16,"sample_bytes":SAMPLE,"group_bytes":g,"data_units":if mode=="sql-columns"{total/SAMPLE}else{total/g},"page":page,"mmap_requested":mmap,"readers":readers,"read_bytes":bytes,"commits":total/EPOCH,"writer_seconds":ingest,"writer_backend_seconds":backend,"writer_cpu_seconds":writer_cpu,"input_generation_seconds":generation,"seconds":seconds,"file_bytes":file_bytes,"allocated_bytes":allocated,"verified":true})
    );
    Ok(())
}

#[link(name = "hdf5_serial")]
unsafe extern "C" {
    static H5T_NATIVE_DOUBLE_g: i64;
    static H5T_IEEE_F64LE_g: i64;
    static H5P_CLS_FILE_ACCESS_ID_g: i64;
    static H5P_CLS_DATASET_CREATE_ID_g: i64;
    static H5P_CLS_DATASET_ACCESS_ID_g: i64;
    fn H5open() -> c_int;
    fn H5Fcreate(n: *const c_char, f: u32, c: i64, a: i64) -> i64;
    fn H5Fopen(n: *const c_char, f: u32, a: i64) -> i64;
    fn H5Fclose(f: i64) -> c_int;
    fn H5Fflush(f: i64, s: c_int) -> c_int;
    fn H5Fstart_swmr_write(f: i64) -> c_int;
    fn H5Pcreate(c: i64) -> i64;
    fn H5Pclose(p: i64) -> c_int;
    fn H5Pset_libver_bounds(p: i64, l: c_int, h: c_int) -> c_int;
    fn H5Pset_chunk(p: i64, r: c_int, d: *const u64) -> c_int;
    fn H5Pset_fletcher32(p: i64) -> c_int;
    fn H5Pset_chunk_cache(p: i64, n: usize, b: usize, w: f64) -> c_int;
    fn H5Pset_fill_time(p: i64, t: c_int) -> c_int;
    fn H5Screate_simple(r: c_int, d: *const u64, m: *const u64) -> i64;
    fn H5Sclose(s: i64) -> c_int;
    fn H5Sselect_hyperslab(
        s: i64,
        o: c_int,
        b: *const u64,
        t: *const u64,
        c: *const u64,
        k: *const u64,
    ) -> c_int;
    fn H5Sget_simple_extent_dims(s: i64, d: *mut u64, m: *mut u64) -> c_int;
    fn H5Dcreate2(f: i64, n: *const c_char, t: i64, s: i64, l: i64, c: i64, a: i64) -> i64;
    fn H5Dopen2(f: i64, n: *const c_char, a: i64) -> i64;
    fn H5Dclose(d: i64) -> c_int;
    fn H5Dget_space(d: i64) -> i64;
    fn H5Dset_extent(d: i64, n: *const u64) -> c_int;
    fn H5Drefresh(d: i64) -> c_int;
    fn H5Dwrite(d: i64, t: i64, m: i64, s: i64, p: i64, b: *const c_void) -> c_int;
    fn H5Dread(d: i64, t: i64, m: i64, s: i64, p: i64, b: *mut c_void) -> c_int;
}
fn hcheck(n: c_int) {
    assert!(n >= 0, "HDF5 operation error");
}
fn hhandle(n: i64) -> i64 {
    assert!(n >= 0, "HDF5 handle error");
    n
}
struct Hdf {
    file: i64,
    data: i64,
    space: i64,
    memory: i64,
    memory_rows: usize,
    rows: usize,
    fd: std::fs::File,
    writable: bool,
    chunked: bool,
    bulk: bool,
    coalesced: Vec<u8>,
}
impl Hdf {
    fn create(path: &Path, mode: &str, g: usize, total: usize) -> Self {
        unsafe {
            hcheck(H5open());
            let chunked = !mode.contains("contiguous");
            let fapl = hhandle(H5Pcreate(H5P_CLS_FILE_ACCESS_ID_g));
            hcheck(H5Pset_libver_bounds(fapl, 2, 4));
            let name = CString::new(path.to_str().unwrap()).unwrap();
            let file = hhandle(H5Fcreate(name.as_ptr(), 2, 0, fapl));
            hcheck(H5Pclose(fapl));
            let dims = [
                if chunked { 0 } else { (total / SAMPLE) as u64 },
                CHANNELS as u64,
            ];
            let max = [u64::MAX, CHANNELS as u64];
            let space = hhandle(H5Screate_simple(
                2,
                dims.as_ptr(),
                if chunked {
                    max.as_ptr()
                } else {
                    std::ptr::null()
                },
            ));
            let dc = hhandle(H5Pcreate(H5P_CLS_DATASET_CREATE_ID_g));
            hcheck(H5Pset_fill_time(dc, 1));
            if chunked {
                hcheck(H5Pset_chunk(
                    dc,
                    2,
                    [(g / SAMPLE) as u64, CHANNELS as u64].as_ptr(),
                ));
                if mode.contains("fletcher") {
                    hcheck(H5Pset_fletcher32(dc));
                }
            }
            let da = hhandle(H5Pcreate(H5P_CLS_DATASET_ACCESS_ID_g));
            hcheck(H5Pset_chunk_cache(da, 521, 8 << 20, 0.75));
            let data = hhandle(H5Dcreate2(
                file,
                c"samples".as_ptr(),
                H5T_IEEE_F64LE_g,
                space,
                0,
                dc,
                da,
            ));
            hcheck(H5Pclose(dc));
            hcheck(H5Pclose(da));
            let fd = std::fs::File::open(path).unwrap();
            let mut h = Self {
                file,
                data,
                space,
                memory: -1,
                memory_rows: 0,
                rows: dims[0] as usize,
                fd,
                writable: true,
                chunked,
                bulk: mode.ends_with("bulk"),
                coalesced: Vec::new(),
            };
            if mode.contains("swmr") {
                hcheck(H5Fstart_swmr_write(file));
                h.flush();
            }
            h
        }
    }
    fn open(path: &Path, mode: &str) -> Self {
        unsafe {
            hcheck(H5open());
            let name = CString::new(path.to_str().unwrap()).unwrap();
            let file = hhandle(H5Fopen(
                name.as_ptr(),
                if mode.contains("swmr") { 64 } else { 0 },
                0,
            ));
            let da = hhandle(H5Pcreate(H5P_CLS_DATASET_ACCESS_ID_g));
            hcheck(H5Pset_chunk_cache(da, 521, 8 << 20, 0.75));
            let data = hhandle(H5Dopen2(file, c"samples".as_ptr(), da));
            hcheck(H5Pclose(da));
            let space = hhandle(H5Dget_space(data));
            let mut dims = [0; 2];
            hcheck(H5Sget_simple_extent_dims(
                space,
                dims.as_mut_ptr(),
                std::ptr::null_mut(),
            ));
            assert_eq!(dims[1], 16);
            Self {
                file,
                data,
                space,
                memory: -1,
                memory_rows: 0,
                rows: dims[0] as usize,
                fd: std::fs::File::open(path).unwrap(),
                writable: false,
                chunked: !mode.contains("contiguous"),
                bulk: false,
                coalesced: Vec::new(),
            }
        }
    }
    fn selection(&mut self, first: usize, n: usize) {
        assert!(n > 0 && first + n <= self.rows);
        unsafe {
            if n != self.memory_rows {
                if self.memory >= 0 {
                    hcheck(H5Sclose(self.memory));
                }
                self.memory = hhandle(H5Screate_simple(
                    2,
                    [n as u64, 16].as_ptr(),
                    std::ptr::null(),
                ));
                self.memory_rows = n;
            }
            hcheck(H5Sselect_hyperslab(
                self.space,
                0,
                [first as u64, 0].as_ptr(),
                std::ptr::null(),
                [n as u64, 16].as_ptr(),
                std::ptr::null(),
            ));
        }
    }
    fn extend(&mut self, rows: usize) {
        unsafe {
            hcheck(H5Dset_extent(self.data, [rows as u64, 16].as_ptr()));
            hcheck(H5Sclose(self.space));
            self.space = hhandle(H5Dget_space(self.data));
            self.rows = rows;
        }
    }
    fn flush(&mut self) {
        unsafe {
            hcheck(H5Fflush(self.file, 1));
        }
        self.fd.sync_all().unwrap();
    }
    fn epoch(&mut self, bank: &[Chunk]) {
        let first = bank[0].first as usize;
        let rows = bank.iter().map(|c| c.data.len() / SAMPLE).sum::<usize>();
        if self.chunked {
            self.extend(first + rows);
        }
        if self.bulk {
            self.coalesced.clear();
            for c in bank {
                self.coalesced.extend_from_slice(&c.data);
            }
            self.selection(first, rows);
            assert_eq!(self.coalesced.as_ptr() as usize % 8, 0);
            unsafe {
                hcheck(H5Dwrite(
                    self.data,
                    H5T_NATIVE_DOUBLE_g,
                    self.memory,
                    self.space,
                    0,
                    self.coalesced.as_ptr().cast(),
                ));
            }
        } else {
            for c in bank {
                self.selection(c.first as usize, c.data.len() / SAMPLE);
                assert_eq!(c.data.as_ptr() as usize % 8, 0);
                unsafe {
                    hcheck(H5Dwrite(
                        self.data,
                        H5T_NATIVE_DOUBLE_g,
                        self.memory,
                        self.space,
                        0,
                        c.data.as_ptr().cast(),
                    ));
                }
            }
        }
        self.flush();
    }
    fn range(&mut self, first: usize, out: &mut [u8]) {
        assert_eq!(out.len() % SAMPLE, 0);
        assert_eq!(out.as_ptr() as usize % 8, 0);
        self.selection(first, out.len() / SAMPLE);
        unsafe {
            hcheck(H5Dread(
                self.data,
                H5T_NATIVE_DOUBLE_g,
                self.memory,
                self.space,
                0,
                out.as_mut_ptr().cast(),
            ));
        }
    }
    fn follow(&mut self) {
        unsafe {
            hcheck(H5Drefresh(self.data));
            hcheck(H5Sclose(self.space));
            self.space = hhandle(H5Dget_space(self.data));
            let mut dims = [0; 2];
            hcheck(H5Sget_simple_extent_dims(
                self.space,
                dims.as_mut_ptr(),
                std::ptr::null_mut(),
            ));
            self.rows = dims[0] as usize;
        }
    }
}
impl Drop for Hdf {
    fn drop(&mut self) {
        unsafe {
            if self.memory >= 0 {
                hcheck(H5Sclose(self.memory));
            }
            hcheck(H5Sclose(self.space));
            hcheck(H5Dclose(self.data));
            hcheck(H5Fclose(self.file));
        }
        if self.writable {
            self.fd.sync_all().unwrap();
        }
    }
}
