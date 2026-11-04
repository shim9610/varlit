//! SWMR datasets with an explicit application publication prefix; not SQL ACID.
use crate::{Result, model::*};
use std::{
    ffi::{CString, c_char, c_int, c_void},
    path::Path,
};
#[link(name = "hdf5_serial")]
unsafe extern "C" {
    static H5T_NATIVE_UCHAR_g: i64;
    static H5T_STD_U64LE_g: i64;
    static H5T_STD_U32LE_g: i64;
    static H5T_STD_I64LE_g: i64;
    static H5P_CLS_FILE_ACCESS_ID_g: i64;
    static H5P_CLS_DATASET_CREATE_ID_g: i64;
    static H5P_CLS_DATASET_ACCESS_ID_g: i64;
    fn H5open() -> c_int;
    fn H5get_libversion(a: *mut u32, b: *mut u32, c: *mut u32) -> c_int;
    fn H5is_library_threadsafe(b: *mut bool) -> c_int;
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
    fn H5Sselect_elements(s: i64, o: c_int, n: usize, c: *const u64) -> c_int;
    fn H5Sget_simple_extent_dims(s: i64, d: *mut u64, m: *mut u64) -> c_int;
    fn H5Dcreate2(f: i64, n: *const c_char, t: i64, s: i64, l: i64, c: i64, a: i64) -> i64;
    fn H5Dopen2(f: i64, n: *const c_char, a: i64) -> i64;
    fn H5Dclose(d: i64) -> c_int;
    fn H5Dget_space(d: i64) -> i64;
    fn H5Dset_extent(d: i64, n: *const u64) -> c_int;
    fn H5Drefresh(d: i64) -> c_int;
    fn H5Dwrite(d: i64, t: i64, m: i64, s: i64, p: i64, b: *const c_void) -> c_int;
    fn H5Dread(d: i64, t: i64, m: i64, s: i64, p: i64, b: *mut c_void) -> c_int;
    fn H5Tcreate(c: c_int, s: usize) -> i64;
    fn H5Tinsert(t: i64, n: *const c_char, o: usize, m: i64) -> c_int;
    fn H5Tclose(t: i64) -> c_int;
    fn H5Tarray_create2(t: i64, r: u32, d: *const u64) -> i64;
}
fn ok(n: c_int) {
    assert!(n >= 0, "HDF5 operation failed");
}
fn handle(n: i64) -> i64 {
    assert!(n >= 0, "HDF5 handle failed");
    n
}
struct Type(i64, bool);
impl Drop for Type {
    fn drop(&mut self) {
        if self.1 {
            ok(unsafe { H5Tclose(self.0) });
        }
    }
}
fn compound(size: usize, fields: &[(&str, usize, i64)]) -> Type {
    unsafe {
        let t = handle(H5Tcreate(6, size));
        for (n, o, f) in fields {
            ok(H5Tinsert(t, CString::new(*n).unwrap().as_ptr(), *o, *f));
        }
        Type(t, true)
    }
}
fn dtype(name: &str) -> (Type, usize) {
    unsafe {
        match name {
            "payload" => (Type(H5T_NATIVE_UCHAR_g, false), 1),
            "ids" => (Type(H5T_STD_U64LE_g, false), 8),
            "frames" => (
                compound(
                    32,
                    &[
                        ("id", 0, H5T_STD_U64LE_g),
                        ("scan", 8, H5T_STD_U32LE_g),
                        ("frame", 12, H5T_STD_U32LE_g),
                        ("offset", 16, H5T_STD_U64LE_g),
                        ("length", 24, H5T_STD_U64LE_g),
                    ],
                ),
                32,
            ),
            "metadata" => (
                compound(
                    16,
                    &[
                        ("txid", 0, H5T_STD_U64LE_g),
                        ("source", 8, H5T_STD_U32LE_g),
                        ("flags", 12, H5T_STD_U32LE_g),
                    ],
                ),
                16,
            ),
            "rows" => {
                let a = handle(H5Tarray_create2(H5T_NATIVE_UCHAR_g, 1, [32].as_ptr()));
                let t = compound(
                    64,
                    &[
                        ("id", 0, H5T_STD_U64LE_g),
                        ("tenant", 8, H5T_STD_U32LE_g),
                        ("status", 12, H5T_STD_U32LE_g),
                        ("created", 16, H5T_STD_U64LE_g),
                        ("amount", 24, H5T_STD_I64LE_g),
                        ("body", 32, a),
                    ],
                );
                ok(H5Tclose(a));
                (t, 64)
            }
            "commits" => (
                compound(
                    48,
                    &[
                        ("txns", 0, H5T_STD_U64LE_g),
                        ("frames", 8, H5T_STD_U64LE_g),
                        ("rows", 16, H5T_STD_U64LE_g),
                        ("metas", 24, H5T_STD_U64LE_g),
                        ("payload", 32, H5T_STD_U64LE_g),
                        ("check", 40, H5T_STD_U64LE_g),
                    ],
                ),
                48,
            ),
            _ => unreachable!(),
        }
    }
}
struct Dataset {
    id: i64,
    t: Type,
    width: usize,
    len: u64,
}
impl Dataset {
    fn open(f: i64, name: &str, create: bool, c: &Config) -> Self {
        unsafe {
            let (t, width) = dtype(name);
            let a = handle(H5Pcreate(H5P_CLS_DATASET_ACCESS_ID_g));
            ok(H5Pset_chunk_cache(a, 521, c.cache / 6, 0.75));
            let n = CString::new(name).unwrap();
            let id = if create {
                let initial = if name == "rows" { c.records as u64 } else { 0 };
                let s = handle(H5Screate_simple(1, [initial].as_ptr(), [u64::MAX].as_ptr()));
                let dc = handle(H5Pcreate(H5P_CLS_DATASET_CREATE_ID_g));
                let chunk = match name {
                    "commits" => 1,
                    "metadata" => 2,
                    "rows" | "ids" | "frames" => {
                        // Seal complete chunks at publication without making a
                        // large transaction's header chunk exceed its raw cache.
                        // A divisor preserves the no-rewrite SWMR invariant.
                        let mut rows = c.per_tx.min(4096);
                        while !c.per_tx.is_multiple_of(rows) {
                            rows -= 1;
                        }
                        rows as u64
                    }
                    "payload" => payload_chunk(c) as u64,
                    _ => unreachable!(),
                };
                ok(H5Pset_chunk(dc, 1, [chunk].as_ptr()));
                if name != "commits" {
                    ok(H5Pset_fill_time(dc, 1));
                }
                if c.fletcher {
                    ok(H5Pset_fletcher32(dc));
                }
                let id = handle(H5Dcreate2(f, n.as_ptr(), t.0, s, 0, dc, a));
                ok(H5Pclose(dc));
                ok(H5Sclose(s));
                id
            } else {
                handle(H5Dopen2(f, n.as_ptr(), a))
            };
            ok(H5Pclose(a));
            let mut d = Self {
                id,
                t,
                width,
                len: 0,
            };
            d.extent();
            d
        }
    }
    fn extent(&mut self) {
        unsafe {
            let s = handle(H5Dget_space(self.id));
            ok(H5Sget_simple_extent_dims(
                s,
                &mut self.len,
                std::ptr::null_mut(),
            ));
            ok(H5Sclose(s));
        }
    }
    fn refresh(&mut self) {
        ok(unsafe { H5Drefresh(self.id) });
        self.extent();
    }
    fn transfer(&self, start: u64, n: usize, points: Option<&[u64]>, bytes: *mut u8, write: bool) {
        if n == 0 {
            return;
        }
        unsafe {
            let s = handle(H5Dget_space(self.id));
            let m = handle(H5Screate_simple(1, [n as u64].as_ptr(), std::ptr::null()));
            if let Some(ids) = points {
                assert_eq!(ids.len(), n);
                assert!(ids.iter().all(|&i| i < self.len));
                ok(H5Sselect_elements(s, 0, n, ids.as_ptr()));
            } else {
                assert!(start + n as u64 <= self.len);
                ok(H5Sselect_hyperslab(
                    s,
                    0,
                    [start].as_ptr(),
                    std::ptr::null(),
                    [n as u64].as_ptr(),
                    std::ptr::null(),
                ));
            }
            if write {
                ok(H5Dwrite(self.id, self.t.0, m, s, 0, bytes.cast()));
            } else {
                ok(H5Dread(self.id, self.t.0, m, s, 0, bytes.cast()));
            }
            ok(H5Sclose(m));
            ok(H5Sclose(s));
        }
    }
    fn append(&mut self, b: &[u8]) {
        assert_eq!(b.len() % self.width, 0);
        let old = self.len;
        self.len += (b.len() / self.width) as u64;
        if old == self.len {
            return;
        }
        ok(unsafe { H5Dset_extent(self.id, [self.len].as_ptr()) });
        self.transfer(old, b.len() / self.width, None, b.as_ptr().cast_mut(), true);
    }
    fn read(&self, start: u64, n: usize) -> Vec<u8> {
        let mut b = vec![0; n * self.width];
        self.transfer(start, n, None, b.as_mut_ptr(), false);
        b
    }
}
impl Drop for Dataset {
    fn drop(&mut self) {
        ok(unsafe { H5Dclose(self.id) });
    }
}
struct File(i64);
impl Drop for File {
    fn drop(&mut self) {
        ok(unsafe { H5Fclose(self.0) });
    }
}
// Dataset fields drop before the file owner; the fsync handle remains alive through close.
pub struct Hdf {
    frames: Dataset,
    rows: Dataset,
    metadata: Dataset,
    payload: Dataset,
    ids: Dataset,
    commits: Dataset,
    file: File,
    fd: std::fs::File,
    write: bool,
    payload_len: u64,
    payload_chunk: usize,
    grouped: bool,
}
impl Hdf {
    pub fn open(p: &Path, c: &Config, create: bool) -> Result<Self> {
        if !(c.records / c.producers).is_multiple_of(c.per_tx) {
            return Err("HDF5 sealed-chunk profile requires complete transaction batches".into());
        }
        if c.lookups && c.scenario == "mixed" && c.group_items == 0 {
            return Err("HDF5 arbitrary composite-key index is not implemented; equal-work comparison refused".into());
        }
        unsafe {
            ok(H5open());
            let a = handle(H5Pcreate(H5P_CLS_FILE_ACCESS_ID_g));
            ok(H5Pset_libver_bounds(a, 2, 4));
            let n = CString::new(p.to_str().unwrap())?;
            let f = if create {
                handle(H5Fcreate(n.as_ptr(), 2, 0, a))
            } else {
                handle(H5Fopen(n.as_ptr(), 64, a))
            };
            ok(H5Pclose(a));
            let mut h = Self {
                frames: Dataset::open(f, "frames", create, c),
                rows: Dataset::open(f, "rows", create, c),
                metadata: Dataset::open(f, "metadata", create, c),
                payload: Dataset::open(f, "payload", create, c),
                ids: Dataset::open(f, "ids", create, c),
                commits: Dataset::open(f, "commits", create, c),
                file: File(f),
                fd: std::fs::File::open(p)?,
                write: create,
                payload_len: 0,
                payload_chunk: payload_chunk(c),
                grouped: c.group_items != 0 && c.lookups,
            };
            if create {
                h.publish(&Head::default());
                ok(H5Fstart_swmr_write(f));
                h.flush(true);
            }
            Ok(h)
        }
    }
    fn flush(&self, sync: bool) {
        ok(unsafe { H5Fflush(self.file.0, 1) });
        if sync {
            self.fd.sync_all().unwrap();
        }
    }
    fn publish(&mut self, h: &Head) {
        let fields = [h.txns, h.frames, h.rows, h.metas, self.payload_len];
        let mut b = Vec::with_capacity(48);
        for x in fields {
            b.extend(x.to_le_bytes());
        }
        b.extend(checksum(&fields).to_le_bytes());
        self.commits.append(&b);
        self.flush(true);
    }
    pub fn append(&mut self, t: &Transaction, h: &mut Head) {
        let mut headers = Vec::new();
        let mut payload = Vec::new();
        let mut offset = self.payload_len;
        for f in &t.frames {
            headers.extend(f.id.to_le_bytes());
            headers.extend(f.scan.to_le_bytes());
            headers.extend(f.frame.to_le_bytes());
            headers.extend(offset.to_le_bytes());
            headers.extend((f.payload.len() as u64).to_le_bytes());
            payload.extend_from_slice(&f.payload);
            offset += f.payload.len() as u64;
        }
        if self.grouped && !t.frames.is_empty() {
            // Dense group IDs are this workload's declared routing directory.
            // Persist it: do not use a sequential scan's known offset for point gets.
            let ids = t.frames.iter().map(|f| f.id).collect::<Vec<_>>();
            let mut index = Vec::with_capacity(ids.len() * 64);
            for b in headers.chunks_exact(32) {
                index.extend(encode_row(&Row {
                    id: u64_at(b, 0),
                    tenant: u32_at(b, 8),
                    status: u32_at(b, 12),
                    created: u64_at(b, 16),
                    amount: u64_at(b, 24) as i64,
                    body: [0; 32],
                }));
            }
            // Old non-grouped profiles retain their original storage contract.
            self.rows
                .transfer(0, ids.len(), Some(&ids), index.as_mut_ptr(), true);
        }
        // Seal payload chunks at publication: readers must never race an in-place
        // rewrite of an already published compressed/checksummed chunk. Padding is charged.
        if !payload.is_empty() {
            let padded = payload.len().div_ceil(self.payload_chunk) * self.payload_chunk;
            payload.resize(padded, 0);
            offset = self.payload_len + padded as u64;
        }
        self.payload.append(&payload);
        self.frames.append(&headers);
        self.payload_len = offset;
        let mut bytes = Vec::new();
        let mut ids = Vec::new();
        let mut ib = Vec::new();
        for r in &t.rows {
            bytes.extend(encode_row(r));
            ids.push(r.id);
            ib.extend(r.id.to_le_bytes());
        }
        self.rows
            .transfer(0, ids.len(), Some(&ids), bytes.as_mut_ptr(), true);
        self.ids.append(&ib);
        let mut mb = Vec::new();
        for flag in 0..2 {
            mb.extend(t.txid.to_le_bytes());
            mb.extend(t.source.to_le_bytes());
            mb.extend((flag as u32).to_le_bytes());
        }
        self.metadata.append(&mb);
        h.txns += 1;
        h.frames += t.frames.len() as u64;
        h.rows += t.rows.len() as u64;
        h.metas += 2;
        self.flush(false);
        crate::before_publish(t.txid);
        self.publish(h);
    }
    pub fn refresh(&mut self) -> Head {
        self.commits.refresh();
        assert!(self.commits.len > 0);
        let mut i = self.commits.len;
        let fields = loop {
            i -= 1;
            let b = self.commits.read(i, 1);
            let a = (0..6).map(|j| u64_at(&b, j * 8)).collect::<Vec<_>>();
            if checksum(&a[..5]) == a[5] {
                break a;
            }
            assert!(i > 0, "no valid publication marker");
        };
        self.frames.refresh();
        self.rows.refresh();
        self.metadata.refresh();
        self.payload.refresh();
        self.ids.refresh();
        assert!(
            fields[1] <= self.frames.len
                && fields[2] <= self.ids.len
                && fields[3] <= self.metadata.len
                && fields[4] <= self.payload.len
        );
        let h = Head {
            key: 0,
            txns: fields[0],
            frames: fields[1],
            rows: fields[2],
            metas: fields[3],
        };
        assert!(h.valid());
        h
    }
    pub fn consume(&self, old: &Head, h: &Head, o: &mut Oracle, c: &Config) {
        let mut first = old.frames;
        while first < h.frames {
            let n = (h.frames - first)
                .min(64)
                .min(((4 << 20) / c.payload).max(1) as u64) as usize;
            let headers = self.frames.read(first, n);
            let offset = u64_at(&headers, 16);
            let last = (n - 1) * 32;
            let end = u64_at(&headers, last + 16) + u64_at(&headers, last + 24);
            let data = self.payload.read(offset, (end - offset) as usize);
            for b in headers.chunks_exact(32) {
                let start = (u64_at(b, 16) - offset) as usize;
                let len = u64_at(b, 24) as usize;
                o.frame_view(
                    u64_at(b, 0),
                    u32_at(b, 8),
                    u32_at(b, 12),
                    &data[start..start + len],
                );
                if c.lookups && c.group_items != 0 {
                    let id = u64_at(b, 0);
                    let point = self.rows.read(id, 1);
                    let point = decode_row(&point);
                    assert_eq!(point.id, id);
                    assert_eq!((point.tenant, point.status), c.frame_key(id));
                    let bytes = self.payload.read(point.created, point.amount as usize);
                    assert_eq!(bytes, data[start..start + len]);
                    o.point_payload(id, &bytes);
                    o.lookup_count += 1;
                }
            }
            first += n as u64;
        }
        let mut first = old.rows;
        while first < h.rows {
            let n = (h.rows - first).min(1024) as usize;
            let bytes = self.ids.read(first, n);
            let ids = bytes
                .chunks_exact(8)
                .map(|b| u64_at(b, 0))
                .collect::<Vec<_>>();
            let mut rows = vec![0; n * 64];
            self.rows
                .transfer(0, n, Some(&ids), rows.as_mut_ptr(), false);
            for b in rows.chunks_exact(64) {
                let r = decode_row(b);
                o.row(&r);
                if c.lookups {
                    let mut point = [0; 64];
                    self.rows.transfer(r.id, 1, None, point.as_mut_ptr(), false);
                    assert_eq!(decode_row(&point), r);
                    o.lookup_count += 1;
                }
            }
            first += n as u64;
        }
        let mut first = old.metas;
        while first < h.metas {
            let n = (h.metas - first).min(4096) as usize;
            let b = self.metadata.read(first, n);
            for b in b.chunks_exact(16) {
                o.metadata(&Metadata {
                    txid: u64_at(b, 0),
                    source: u32_at(b, 8),
                    flags: u32_at(b, 12),
                });
            }
            first += n as u64;
        }
    }
    pub fn finish(&self) {
        if self.write {
            self.flush(true);
        }
    }
}
fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn checksum(a: &[u64]) -> u64 {
    a.iter().fold(0xb5fbac8c1a92e37du64, |x, &v| {
        x.rotate_left(17) ^ v.wrapping_mul(0x9e3779b97f4a7c15)
    })
}
fn encode_row(r: &Row) -> Vec<u8> {
    let mut b = Vec::with_capacity(64);
    b.extend(r.id.to_le_bytes());
    b.extend(r.tenant.to_le_bytes());
    b.extend(r.status.to_le_bytes());
    b.extend(r.created.to_le_bytes());
    b.extend(r.amount.to_le_bytes());
    b.extend(r.body);
    b
}
fn decode_row(b: &[u8]) -> Row {
    Row {
        id: u64_at(b, 0),
        tenant: u32_at(b, 8),
        status: u32_at(b, 12),
        created: u64_at(b, 16),
        amount: u64_at(b, 24) as i64,
        body: b[32..64].try_into().unwrap(),
    }
}
pub fn facts() -> serde_json::Value {
    unsafe {
        ok(H5open());
        let (mut a, mut b, mut c, mut t) = (0, 0, 0, false);
        ok(H5get_libversion(&mut a, &mut b, &mut c));
        ok(H5is_library_threadsafe(&mut t));
        serde_json::json!({"version":format!("{a}.{b}.{c}"),"threadsafe":t,"readers":"independent SWMR processes","publication":"application commit-prefix dataset; not SQL ACID"})
    }
}

fn payload_chunk(c: &Config) -> usize {
    if c.hdf_chunk > 0 {
        assert!(c.hdf_chunk <= 1 << 20);
        return c.hdf_chunk;
    }
    c.payload.saturating_mul(c.per_tx).clamp(4096, 1 << 20)
}
