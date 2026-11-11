//! Scenario 3 (SQLite-favored): OLTP-style records.
//!
//! ACCOUNTS rows (dense ids) with a random `branch` secondary attribute.
//! Load, then TXNS durable transactions; each applies the net balance deltas
//! of TRANSFERS random transfers by reading the current balance from storage
//! and writing it back (read-modify-write). Then a fresh reader runs POINTS
//! random point lookups and BRANCH_Q secondary-key aggregates
//! (sum/count of balances of one branch). Everything is checked against an
//! in-memory oracle that is computed outside the timed sections.
use crate::common::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const ACCOUNTS: u64 = 1_000_000;
pub const BRANCHES: u32 = 10_000;
pub const TXNS: usize = 1000;
pub const TRANSFERS: usize = 10;
pub const POINTS: usize = 20_000;
pub const BRANCH_Q: usize = 200;
const START: i64 = 1000;

pub fn branch_of(id: u64) -> u32 {
    (splitmix(id ^ 0xB0B) % BRANCHES as u64) as u32
}
pub fn name_of(id: u64) -> [u8; 24] {
    let mut n = [0u8; 24];
    fill_payload(id ^ 0xAA, &mut n);
    n
}
pub fn created_of(id: u64) -> u64 {
    id.wrapping_mul(7) + 1_700_000_000
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    pub id: u64,
    pub branch: u32,
    pub balance: i64,
    pub name: [u8; 24],
    pub created: u64,
}

pub trait Oltp {
    fn load(&mut self) -> Result<()>;
    /// Apply `(id, delta)` (distinct ids) as one durable transaction.
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()>;
    /// Optional maintenance after the workload (timed separately).
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}
pub trait OltpRead {
    fn point(&mut self, id: u64) -> Result<Row>;
    /// (sum of balances, number of accounts) for a branch.
    fn branch(&mut self, b: u32) -> Result<(i64, u64)>;
}

fn branch_lists() -> Vec<Vec<u64>> {
    let mut v = vec![Vec::new(); BRANCHES as usize];
    for id in 0..ACCOUNTS {
        v[branch_of(id) as usize].push(id);
    }
    v
}

// ---------------------------------------------------------------- Varlit indexed
mod vl {
    use super::*;
    use varlit::{BatchOptions, DiskIndexOptions, VarlitEmpty, varlit_format};

    varlit_format! {
        pub format Bank {
            magic: b"BANKOLTP";
            version: 1;
            endian: little;
            schema_hash: computed;
            integrity: crc32_with_header;
            index: keyed_offset_chain;
            blocks {
                fixed Account(id = 1, key = [id], key_index = disk) {
                    id: u64,
                    branch: u32,
                    balance: i64,
                    name: [u8; 24],
                    created: u64,
                }
                // Secondary index maintained by the application: branch -> ids.
                variable Branch(id = 2, key = [branch], key_index = disk) {
                    branch: u32,
                    ids: Vec<u64>,
                }
            }
        }
    }

    pub struct W {
        w: BankIndexedWriter,
        r: Option<BankIndexedReader>,
        p: PathBuf,
    }
    fn opts() -> DiskIndexOptions {
        // 64 MiB private index cache, matching SQLite's cache_size below.
        vl_dio(DiskIndexOptions { cache_bytes: 64 << 20, ..DiskIndexOptions::default() })
    }
    pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
        Ok(Box::new(W {
            w: Bank::create_indexed_writer(p, opts())?,
            r: None,
            p: p.to_path_buf(),
        }))
    }
    impl Oltp for W {
        fn load(&mut self) -> Result<()> {
            self.w
                .push_accounts(
                    (0..ACCOUNTS).map(|id| Account {
                        id,
                        branch: branch_of(id),
                        balance: START,
                        name: name_of(id),
                        created: created_of(id),
                    }),
                    BatchOptions::default(),
                )
                .map_err(|f| f.source)?;
            self.w
                .push_branchs(
                    branch_lists()
                        .into_iter()
                        .enumerate()
                        .map(|(b, ids)| Branch { branch: b as u32, ids }),
                    BatchOptions::default(),
                )
                .map_err(|f| f.source)?;
            self.w.sync()?;
            // The .vki is an append-only COW B-tree; reclaim the pages the bulk
            // load superseded (documented explicit maintenance).
            self.w.compact_index()?;
            // The writer has no read path; RMW reads go through a reader that
            // follows each published transaction.
            self.r = Some(Bank::open_indexed_reader(&self.p, opts())?);
            Ok(())
        }
        fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
            let r = self.r.as_mut().unwrap();
            let mut rows = Vec::with_capacity(deltas.len());
            for &(id, d) in deltas {
                let mut a = got(|o| r.get_account(&id, o), "missing account")?;
                a.balance += d;
                rows.push(a);
            }
            self.w.push_accounts(rows, BatchOptions::default()).map_err(|f| f.source)?;
            self.w.sync()?;
            r.follow()?;
            Ok(())
        }
        fn finish(&mut self) -> Result<()> {
            self.w.compact_index()?;
            Ok(())
        }
    }
    pub struct R(BankIndexedReader);
    pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
        Ok(Box::new(R(Bank::open_indexed_reader(p, opts())?)))
    }
    impl OltpRead for R {
        fn point(&mut self, id: u64) -> Result<Row> {
            let a = got(|o| self.0.get_account(&id, o), "missing")?;
            Ok(Row { id: a.id, branch: a.branch, balance: a.balance, name: a.name, created: a.created })
        }
        fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
            let br = got(|o| self.0.get_branch(&b, o), "missing branch")?;
            let mut s = 0;
            let mut a = Account::empty();
            for id in &br.ids {
                if !self.0.get_account(id, &mut a)? {
                    return Err("missing".into());
                }
                s += a.balance;
            }
            Ok((s, br.ids.len() as u64))
        }
    }
}

// ---------------------------------------------------------------- Varlit matrix (dense ids)
// Dense ids map directly to matrix cells: no key index at all. The branch
// secondary index is CSR in two more matrix blocks. One sync per transaction.
mod vm {
    use super::*;
    use varlit::{MatrixDimensions, MatrixKey, VarlitEmpty, VarlitFile, varlit_format};

    varlit_format! {
        pub format BankMatrix {
            magic: b"BANKMATX";
            version: 1;
            limits {
                record_payload: 67_108_864;
                logical_payload: 268_435_456;
                materialized_bytes: 1_073_741_824;
                matrix_dimension: 16_000_000;
                matrix_cells: 16_000_000;
                matrix_bitmap: 64_000_000;
                matrix_crc: 128_000_000;
                matrix_metadata: 268_435_456;
                matrix_slot_region: 8_589_934_592;
                sidecar: 268_435_456;
            }
            endian: little;
            schema_hash: computed;
            integrity: crc32_with_header;
            dims { entry: u32, slot: u32, }
            commit: cell_bitmap { keyspace = [entry, slot]; categories = [acct, brid, broff]; };
            blocks {
                matrix Acct(id = 1, dims = [entry, slot], category = acct) {
                    branch: u32, balance: i64, name: [u8; 24], created: u64,
                }
                matrix BrId(id = 2, dims = [entry, slot], category = brid) { id: u64, }
                matrix BrOff(id = 3, dims = [entry, slot], category = broff) { off: u64, }
            }
        }
    }
    fn k(n: u64) -> MatrixKey {
        MatrixKey::new(n, 0)
    }

    pub struct W(VarlitFile);
    pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
        let f = BankMatrix::spec().create_with_dims(
            p,
            MatrixDimensions::from_pairs([("entry", ACCOUNTS + 1), ("slot", 1)]),
        )?;
        Ok(Box::new(W(f)))
    }
    impl Oltp for W {
        fn load(&mut self) -> Result<()> {
            for id in 0..ACCOUNTS {
                let a = Acct { branch: branch_of(id), balance: START, name: name_of(id), created: created_of(id) };
                self.0.write_matrix_cell(k(id), &a)?;
                self.0.commit_matrix_cell::<Acct>(k(id))?;
            }
            let lists = branch_lists();
            let mut pos = 0u64;
            for (b, l) in lists.iter().enumerate() {
                self.0.write_matrix_cell(k(b as u64), &BrOff { off: pos })?;
                self.0.commit_matrix_cell::<BrOff>(k(b as u64))?;
                for &id in l {
                    self.0.write_matrix_cell(k(pos), &BrId { id })?;
                    self.0.commit_matrix_cell::<BrId>(k(pos))?;
                    pos += 1;
                }
            }
            self.0.write_matrix_cell(k(BRANCHES as u64), &BrOff { off: pos })?;
            self.0.commit_matrix_cell::<BrOff>(k(BRANCHES as u64))?;
            self.0.sync()?;
            Ok(())
        }
        fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
            for &(id, d) in deltas {
                let mut a = Acct::empty();
                self.0.read_matrix_cell::<Acct>(k(id), &mut a)?;
                a.balance += d;
                self.0.write_matrix_cell(k(id), &a)?;
                self.0.commit_matrix_cell::<Acct>(k(id))?;
            }
            self.0.sync()?;
            Ok(())
        }
        fn finish(&mut self) -> Result<()> {
            // Reclaim superseded COW generation pages (documented maintenance).
            self.0.compact_matrix()?;
            Ok(())
        }
    }
    pub struct R(varlit::VarlitReader);
    pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
        Ok(Box::new(R(vl_spec(BankMatrix::spec()).open_reader(p)?)))
    }
    impl OltpRead for R {
        fn point(&mut self, id: u64) -> Result<Row> {
            let mut a = Acct::empty();
            self.0.read_matrix_cell::<Acct>(k(id), &mut a)?;
            Ok(Row { id, branch: a.branch, balance: a.balance, name: a.name, created: a.created })
        }
        fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
            let mut off = vec![BrOff { off: 0 }; 2];
            self.0.read_matrix_run_into(k(b as u64), &mut off)?;
            let n = (off[1].off - off[0].off) as usize;
            let mut ids = vec![BrId { id: 0 }; n];
            self.0.read_matrix_run_into(k(off[0].off), &mut ids)?;
            let mut s = 0;
            let mut a = Acct::empty();
            for i in ids {
                self.0.read_matrix_cell::<Acct>(k(i.id), &mut a)?;
                s += a.balance;
            }
            Ok((s, n as u64))
        }
    }
}

// ---------------------------------------------------------------- Varlit grouped (packed accounts)
// Accounts packed G per physical block, as the scalable-io guide recommends for
// small items: the block key is the group, the slot is id % G.
/// Accounts per group; `FB_GROUP` env (multiple of 256), default 256.
pub fn grp() -> usize {
    static G: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        let g = std::env::var("FB_GROUP").ok().map(|v| v.parse().unwrap()).unwrap_or(256);
        assert!(g % CELL_ACCTS == 0 && (ACCOUNTS as usize).div_ceil(g) <= 3907);
        g
    })
}
pub fn ng() -> usize {
    (ACCOUNTS as usize).div_ceil(grp())
}
const CELL_ACCTS: usize = 256; // accounts per 11 KiB matrix cell
const AB: usize = 44; // branch u32 | balance i64 | name [u8;24] | created u64

fn enc(id: u64, balance: i64, out: &mut [u8]) {
    out[0..4].copy_from_slice(&branch_of(id).to_le_bytes());
    out[4..12].copy_from_slice(&balance.to_le_bytes());
    out[12..36].copy_from_slice(&name_of(id));
    out[36..44].copy_from_slice(&created_of(id).to_le_bytes());
}
fn dec(id: u64, b: &[u8]) -> Row {
    Row {
        id,
        branch: u32::from_le_bytes(b[0..4].try_into().unwrap()),
        balance: i64::from_le_bytes(b[4..12].try_into().unwrap()),
        name: b[12..36].try_into().unwrap(),
        created: u64::from_le_bytes(b[36..44].try_into().unwrap()),
    }
}
fn bal_at(group: &[u8], slot: usize) -> i64 {
    i64::from_le_bytes(group[slot * AB + 4..slot * AB + 12].try_into().unwrap())
}
fn add_bal(group: &mut [u8], slot: usize, d: i64) {
    let v = bal_at(group, slot) + d;
    group[slot * AB + 4..slot * AB + 12].copy_from_slice(&v.to_le_bytes());
}
fn initial_group(g: usize, out: &mut [u8]) {
    out.fill(0);
    for slot in 0..grp() {
        let id = (g * grp() + slot) as u64;
        if id < ACCOUNTS {
            enc(id, START, &mut out[slot * AB..(slot + 1) * AB]);
        }
    }
}
/// Distinct groups touched by a transaction, with (slot, delta) lists.
fn by_group(deltas: &[(u64, i64)]) -> std::collections::BTreeMap<usize, Vec<(usize, i64)>> {
    let mut m: std::collections::BTreeMap<usize, Vec<(usize, i64)>> = Default::default();
    for &(id, d) in deltas {
        m.entry(id as usize / grp()).or_default().push((id as usize % grp(), d));
    }
    m
}

mod vg {
    use super::*;
    use varlit::{BatchOptions, DiskIndexOptions, VarlitEmpty, varlit_format};

    varlit_format! {
        pub format BankGrouped {
            magic: b"BANKGRP1";
            version: 1;
            endian: little;
            schema_hash: computed;
            integrity: crc32_with_header;
            index: keyed_offset_chain;
            blocks {
                // grp() accounts per record, application layout AB bytes per slot.
                variable AcctGroup(id = 1, key = [g], key_index = disk, key_domain = [g = 0..3907]) {
                    g: u16,
                    data: Vec<u8>,
                }
                variable Branch(id = 2, key = [branch], key_index = disk) {
                    branch: u32,
                    ids: Vec<u64>,
                }
            }
        }
    }
    fn opts() -> DiskIndexOptions {
        vl_dio(DiskIndexOptions { cache_bytes: 64 << 20, ..DiskIndexOptions::default() })
    }
    fn key(g: usize) -> AcctGroupKey {
        AcctGroupKey::from_values(g as u16).unwrap()
    }

    pub struct W {
        w: BankGroupedIndexedWriter,
        r: Option<BankGroupedIndexedReader>,
        p: PathBuf,
    }
    pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
        Ok(Box::new(W { w: BankGrouped::create_indexed_writer(p, opts())?, r: None, p: p.to_path_buf() }))
    }
    impl Oltp for W {
        fn load(&mut self) -> Result<()> {
            let groups = (0..ng()).map(|g| {
                let mut data = vec![0u8; grp() * AB];
                initial_group(g, &mut data);
                AcctGroup { key: key(g), data }
            });
            self.w.push_acct_groups(groups, BatchOptions::default()).map_err(|f| f.source)?;
            self.w
                .push_branchs(
                    branch_lists().into_iter().enumerate().map(|(b, ids)| Branch { branch: b as u32, ids }),
                    BatchOptions::default(),
                )
                .map_err(|f| f.source)?;
            self.w.sync()?;
            self.w.compact_index()?;
            self.r = Some(BankGrouped::open_indexed_reader(&self.p, opts())?);
            Ok(())
        }
        fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
            let r = self.r.as_mut().unwrap();
            let mut out = Vec::new();
            for (g, ups) in by_group(deltas) {
                let mut blk = got(|o| r.get_acct_group(&key(g), o), "missing group")?;
                for (slot, d) in ups {
                    add_bal(&mut blk.data, slot, d);
                }
                out.push(blk);
            }
            self.w.push_acct_groups(out, BatchOptions::default()).map_err(|f| f.source)?;
            self.w.sync()?;
            r.follow()?;
            Ok(())
        }
        fn finish(&mut self) -> Result<()> {
            self.w.compact_index()?;
            Ok(())
        }
    }
    pub struct R(pub BankGroupedIndexedReader);
    pub fn open_reader(p: &Path) -> Result<R> {
        Ok(R(BankGrouped::open_indexed_reader(p, opts())?))
    }
    pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
        Ok(Box::new(open_reader(p)?))
    }
    impl R {
        /// Ids of a branch, from the stored secondary-index record.
        pub fn branch_ids(&self, b: u32) -> Result<Vec<u64>> {
            Ok(got(|o| self.0.get_branch(&b, o), "missing branch")?.ids)
        }
    }
    impl OltpRead for R {
        fn point(&mut self, id: u64) -> Result<Row> {
            let blk = got(|o| self.0.get_acct_group(&key(id as usize / grp()), o), "missing")?;
            let s = id as usize % grp();
            Ok(dec(id, &blk.data[s * AB..(s + 1) * AB]))
        }
        fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
            let br = got(|o| self.0.get_branch(&b, o), "missing branch")?;
            let mut sum = 0;
            // Fetch each touched group once.
            let mut blk = AcctGroup::empty();
            for (g, slots) in by_group(&br.ids.iter().map(|&id| (id, 0)).collect::<Vec<_>>()) {
                if !self.0.get_acct_group(&key(g), &mut blk)? {
                    return Err("missing".into());
                }
                for (slot, _) in slots {
                    sum += bal_at(&blk.data, slot);
                }
            }
            Ok((sum, br.ids.len() as u64))
        }
    }
}

mod vp {
    use super::*;
    use varlit::{MatrixDimensions, MatrixKey, VarlitFile, varlit_format};
    const IDS: usize = 1024; // branch-index ids per cell

    varlit_format! {
        pub format BankPacked {
            magic: b"BANKPACK";
            version: 1;
            limits {
                record_payload: 67_108_864;
                logical_payload: 268_435_456;
                materialized_bytes: 1_073_741_824;
                matrix_dimension: 16_000_000;
                matrix_cells: 16_000_000;
                matrix_bitmap: 64_000_000;
                matrix_crc: 128_000_000;
                matrix_metadata: 268_435_456;
                matrix_slot_region: 8_589_934_592;
                sidecar: 268_435_456;
            }
            endian: little;
            schema_hash: computed;
            integrity: crc32_with_header;
            dims { entry: u32, slot: u32, }
            commit: cell_bitmap { keyspace = [entry, slot]; categories = [acct, brid, broff]; };
            blocks {
                matrix AcctCell(id = 1, dims = [entry, slot], category = acct) { data: [u8; 11264], }
                matrix BrIdCell(id = 2, dims = [entry, slot], category = brid) { ids: [u64; 1024], }
                matrix BrOffCell(id = 3, dims = [entry, slot], category = broff) { off: [u64; 1024], }
            }
        }
    }
    const CB: usize = CELL_ACCTS * AB; // 11264 bytes per cell
    fn k(n: usize) -> MatrixKey {
        MatrixKey::new(n as u64, 0)
    }
    fn u64s(b: &[u8]) -> impl Iterator<Item = u64> + '_ {
        b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap()))
    }

    pub struct W(VarlitFile);
    /// A group = grp()/256 consecutive cells (the last group may be shorter).
    fn cell_range(g: usize) -> std::ops::Range<usize> {
        let per = grp() / CELL_ACCTS;
        let total = ACCOUNTS.div_ceil(CELL_ACCTS as u64) as usize;
        g * per..((g + 1) * per).min(total)
    }
    impl W {
        fn cells(&self, g: usize) -> std::ops::Range<usize> {
            cell_range(g)
        }
        fn put(&mut self, g: usize, buf: &[u8]) -> Result<()> {
            for (i, c) in cell_range(g).enumerate() {
                self.0.write_matrix_cell_payload::<AcctCell>(k(c), &buf[i * CB..(i + 1) * CB])?;
                self.0.commit_matrix_cell::<AcctCell>(k(c))?;
            }
            Ok(())
        }
    }
    pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
        let f = BankPacked::spec()
            .create_with_dims(p, MatrixDimensions::from_pairs([("entry", ACCOUNTS.div_ceil(CELL_ACCTS as u64)), ("slot", 1)]))?;
        Ok(Box::new(W(f)))
    }
    impl Oltp for W {
        fn load(&mut self) -> Result<()> {
            let mut buf = vec![0u8; grp() * AB];
            for g in 0..ng() {
                initial_group(g, &mut buf);
                self.put(g, &buf)?;
            }
            let lists = branch_lists();
            let ids: Vec<u64> = lists.iter().flatten().copied().collect();
            let mut off = vec![0u64];
            for l in &lists {
                off.push(off.last().unwrap() + l.len() as u64);
            }
            for (cat, v) in [(0, &ids), (1, &off)] {
                let mut b = vec![0u8; IDS * 8];
                for (c, chunk) in v.chunks(IDS).enumerate() {
                    b.fill(0);
                    for (i, x) in chunk.iter().enumerate() {
                        b[i * 8..i * 8 + 8].copy_from_slice(&x.to_le_bytes());
                    }
                    if cat == 0 {
                        self.0.write_matrix_cell_payload::<BrIdCell>(k(c), &b)?;
                        self.0.commit_matrix_cell::<BrIdCell>(k(c))?;
                    } else {
                        self.0.write_matrix_cell_payload::<BrOffCell>(k(c), &b)?;
                        self.0.commit_matrix_cell::<BrOffCell>(k(c))?;
                    }
                }
            }
            self.0.sync()?;
            Ok(())
        }
        fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
            let mut buf = vec![0u8; grp() * AB];
            for (g, ups) in by_group(deltas) {
                let cells = self.cells(g);
                self.0.read_matrix_run_bytes_into::<AcctCell>(k(cells.start), &mut buf[..cells.len() * CB])?;
                for (slot, d) in ups {
                    add_bal(&mut buf, slot, d);
                }
                self.put(g, &buf)?;
            }
            self.0.sync()?;
            Ok(())
        }
        fn finish(&mut self) -> Result<()> {
            self.0.compact_matrix()?;
            Ok(())
        }
    }
    pub struct R {
        r: varlit::VarlitReader,
        buf: Vec<u8>,
    }
    pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
        Ok(Box::new(R { r: vl_spec(BankPacked::spec()).open_reader(p)?, buf: vec![0; grp() * AB] }))
    }
    impl R {
        fn u64_range<T: varlit::VarlitMatrixBlock>(&self, a: usize, z: usize) -> Result<Vec<u64>> {
            let (c0, c1) = (a / IDS, (z - 1) / IDS);
            let mut b = vec![0u8; (c1 - c0 + 1) * IDS * 8];
            self.r.read_matrix_run_bytes_into::<T>(k(c0), &mut b)?;
            Ok(u64s(&b).skip(a - c0 * IDS).take(z - a).collect())
        }
    }
    impl OltpRead for R {
        fn point(&mut self, id: u64) -> Result<Row> {
            let c = cell_range(id as usize / grp());
            self.r.read_matrix_run_bytes_into::<AcctCell>(k(c.start), &mut self.buf[..c.len() * CB])?;
            let s = id as usize % grp();
            Ok(dec(id, &self.buf[s * AB..(s + 1) * AB]))
        }
        fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
            let off = self.u64_range::<BrOffCell>(b as usize, b as usize + 2)?;
            let ids = self.u64_range::<BrIdCell>(off[0] as usize, off[1] as usize)?;
            let mut sum = 0;
            for (g, slots) in by_group(&ids.iter().map(|&id| (id, 0)).collect::<Vec<_>>()) {
                let c = cell_range(g);
                self.r.read_matrix_run_bytes_into::<AcctCell>(k(c.start), &mut self.buf[..c.len() * CB])?;
                for (slot, _) in slots {
                    sum += bal_at(&self.buf, slot);
                }
            }
            Ok((sum, ids.len() as u64))
        }
    }
}

// ---------------------------------------------------------------- Varlit append (snapshot + delta log)
// Append-native design: the grouped base snapshot is written once; every
// transaction appends ONE small delta record to a native log and commits it
// durably (commit_durable: data barrier + marker barrier). Readers follow the
// log and keep the folded deltas in memory (LSM style, like the Parquet deltas).
mod va {
    use super::*;
    use varlit::{RecordIndexEntry, VarlitBlock, VarlitEmpty, VarlitFile, varlit_format};

    varlit_format! {
        pub format BankLog {
            magic: b"BANKLOG1";
            version: 1;
            endian: little;
            integrity: crc32_with_header;
            index: block_offset_chain;
            commit: transaction_marker(explicit);
            blocks {
                variable TxDelta(id = 1) {
                    ids: Vec<u64>,
                    deltas: Vec<i64>,
                }
            }
        }
    }
    fn log_path(p: &Path) -> PathBuf {
        p.join("deltas.varlit")
    }

    pub struct W {
        base: Box<dyn Oltp>,
        log: Option<VarlitFile>,
        p: PathBuf,
    }
    pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
        std::fs::create_dir_all(p)?;
        Ok(Box::new(W { base: vg::create(&p.join("base.varlit"))?, log: None, p: p.to_path_buf() }))
    }
    impl Oltp for W {
        fn load(&mut self) -> Result<()> {
            self.base.load()?;
            let mut f = VarlitFile::create(BankLog::spec(), log_path(&self.p))?;
            f.commit_durable()?;
            self.log = Some(f);
            Ok(())
        }
        fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
            let f = self.log.as_mut().unwrap();
            f.push_info(&TxDelta {
                ids: deltas.iter().map(|d| d.0).collect(),
                deltas: deltas.iter().map(|d| d.1).collect(),
            })?;
            f.commit_durable()?;
            Ok(())
        }
    }

    pub struct R {
        base: vg::R,
        log: VarlitFile,
        seen: Option<u64>,
        overlay: HashMap<u64, i64>,
    }
    pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
        let mut r = R {
            base: vg::open_reader(&p.join("base.varlit"))?,
            log: VarlitFile::open_readonly(vl_spec(BankLog::spec()), log_path(p))?,
            seen: None,
            overlay: HashMap::new(),
        };
        r.catch_up()?;
        Ok(Box::new(r))
    }
    impl R {
        /// Follow the log and fold only records appended since the last call.
        fn catch_up(&mut self) -> Result<()> {
            self.log.follow()?;
            let tail = self.log.block_tail_offset(TxDelta::ID);
            if tail == self.seen {
                return Ok(());
            }
            let mut fresh: Vec<RecordIndexEntry> = Vec::new();
            for e in self.log.block_chain(TxDelta::ID)? {
                let e = e?;
                if Some(e.record_offset) <= self.seen {
                    break;
                }
                fresh.push(e);
            }
            let mut scratch = Vec::new();
            let mut t = TxDelta::empty();
            for e in fresh.iter().rev() {
                self.log.decode_block_into(e, &mut scratch, &mut t)?;
                for (&id, &d) in t.ids.iter().zip(&t.deltas) {
                    *self.overlay.entry(id).or_default() += d;
                }
            }
            self.seen = tail;
            Ok(())
        }
    }
    impl OltpRead for R {
        fn point(&mut self, id: u64) -> Result<Row> {
            let mut row = self.base.point(id)?;
            row.balance += self.overlay.get(&id).copied().unwrap_or(0);
            Ok(row)
        }
        fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
            let (s, n) = self.base.branch(b)?;
            // Membership comes from the stored branch record; add folded deltas.
            let extra: i64 = self.base.branch_ids(b)?.iter().filter_map(|id| self.overlay.get(id)).sum();
            Ok((s + extra, n))
        }
    }
}

// ---------------------------------------------------------------- SQLite
mod sq {
    use super::*;
    use rusqlite::{Connection, OpenFlags, params};

    pub struct W(Connection);
    pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
        let c = Connection::open(p)?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA cache_size=-65536;
             CREATE TABLE accounts(id INTEGER PRIMARY KEY, branch INTEGER NOT NULL,
               balance INTEGER NOT NULL, name BLOB NOT NULL, created INTEGER NOT NULL);",
        )?;
        Ok(Box::new(W(c)))
    }
    impl Oltp for W {
        fn load(&mut self) -> Result<()> {
            let tx = self.0.transaction()?;
            {
                let mut st = tx.prepare("INSERT INTO accounts VALUES (?1, ?2, ?3, ?4, ?5)")?;
                for id in 0..ACCOUNTS {
                    st.execute(params![id as i64, branch_of(id), START, &name_of(id)[..], created_of(id) as i64])?;
                }
            }
            // Build the secondary index after the bulk load (cheaper than incremental).
            tx.execute_batch("CREATE INDEX accounts_branch ON accounts(branch);")?;
            tx.commit()?;
            self.0.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            Ok(())
        }
        fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
            let tx = self.0.transaction()?;
            {
                let mut st = tx.prepare_cached("UPDATE accounts SET balance = balance + ?2 WHERE id = ?1")?;
                for &(id, d) in deltas {
                    assert_eq!(st.execute(params![id as i64, d])?, 1);
                }
            }
            tx.commit()?;
            Ok(())
        }
    }
    pub struct R(Connection);
    pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
        let c = Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        c.execute_batch("PRAGMA cache_size=-65536; PRAGMA mmap_size=1073741824;")?;
        Ok(Box::new(R(c)))
    }
    impl OltpRead for R {
        fn point(&mut self, id: u64) -> Result<Row> {
            let mut st = self.0.prepare_cached("SELECT branch, balance, name, created FROM accounts WHERE id = ?1")?;
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
            let mut st = self.0.prepare_cached("SELECT total(balance), count(*) FROM accounts WHERE branch = ?1")?;
            Ok(st.query_row([b], |r| Ok((r.get::<_, f64>(0)? as i64, r.get::<_, i64>(1)? as u64)))?)
        }
    }
}

// ---------------------------------------------------------------- HDF5 columns
mod h5 {
    use super::*;
    use hdf5::{File, Selection};
    use ndarray::{Array1, Array2, s};
    const CH: usize = 4096;

    fn points(ids: impl Iterator<Item = u64>) -> Selection {
        let v: Vec<usize> = ids.map(|i| i as usize).collect();
        let n = v.len();
        Selection::Points(Array2::from_shape_vec((n, 1), v).unwrap())
    }

    pub struct W {
        f: File,
        bal: hdf5::Dataset,
        p: PathBuf,
    }
    pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
        let f = File::with_options().with_fapl(|a| a.libver_latest()).create(p)?;
        let bal = f.new_dataset::<i64>().chunk(CH).shape(ACCOUNTS as usize).create("balance")?;
        Ok(Box::new(W { f, bal, p: p.to_path_buf() }))
    }
    impl Oltp for W {
        fn load(&mut self) -> Result<()> {
            let n = ACCOUNTS as usize;
            self.bal.write(&Array1::from_elem(n, START))?;
            let br = Array1::from_iter((0..ACCOUNTS).map(branch_of));
            self.f.new_dataset_builder().chunk(CH).with_data(&br).create("branch")?;
            let mut names = Array2::<u8>::zeros((n, 24));
            for id in 0..n {
                names.row_mut(id).as_slice_mut().unwrap().copy_from_slice(&name_of(id as u64));
            }
            self.f.new_dataset_builder().chunk((CH, 24)).with_data(&names).create("name")?;
            let cr = Array1::from_iter((0..ACCOUNTS).map(created_of));
            self.f.new_dataset_builder().chunk(CH).with_data(&cr).create("created")?;
            // Secondary index: ids grouped by branch + CSR offsets.
            let lists = branch_lists();
            let mut off = Vec::with_capacity(BRANCHES as usize + 1);
            let mut ids = Vec::with_capacity(n);
            off.push(0u64);
            for l in &lists {
                ids.extend_from_slice(l);
                off.push(ids.len() as u64);
            }
            self.f.new_dataset_builder().chunk(CH).with_data(&Array1::from(ids)).create("br_ids")?;
            self.f.new_dataset_builder().with_data(&Array1::from(off)).create("br_off")?;
            self.f.flush()?;
            fsync_file(&self.p)?;
            Ok(())
        }
        fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
            // One point-selection read and one point-selection write.
            let sel = points(deltas.iter().map(|d| d.0));
            let mut cur: Array1<i64> = self.bal.read_slice_1d(sel.clone())?;
            for (c, d) in cur.iter_mut().zip(deltas) {
                *c += d.1;
            }
            self.bal.write_slice(&cur, sel)?;
            // No transactional atomicity in HDF5: flush + fsync is the strongest
            // available durability point.
            self.f.flush()?;
            fsync_file(&self.p)?;
            Ok(())
        }
    }
    pub struct R {
        _f: File,
        bal: hdf5::Dataset,
        br: hdf5::Dataset,
        name: hdf5::Dataset,
        cr: hdf5::Dataset,
        ids: hdf5::Dataset,
        off: Vec<u64>,
    }
    pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
        let f = File::open(p)?;
        let off = f.dataset("br_off")?.read_raw::<u64>()?;
        Ok(Box::new(R {
            bal: f.dataset("balance")?,
            br: f.dataset("branch")?,
            name: f.dataset("name")?,
            cr: f.dataset("created")?,
            ids: f.dataset("br_ids")?,
            off,
            _f: f,
        }))
    }
    impl OltpRead for R {
        fn point(&mut self, id: u64) -> Result<Row> {
            let i = id as usize;
            let nm: Array2<u8> = self.name.read_slice(s![i..i + 1, ..])?;
            Ok(Row {
                id,
                balance: self.bal.read_slice_1d::<i64, _>(i..i + 1)?[0],
                branch: self.br.read_slice_1d::<u32, _>(i..i + 1)?[0],
                name: nm.as_slice().unwrap().try_into().unwrap(),
                created: self.cr.read_slice_1d::<u64, _>(i..i + 1)?[0],
            })
        }
        fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
            let (a, z) = (self.off[b as usize] as usize, self.off[b as usize + 1] as usize);
            let ids: Array1<u64> = self.ids.read_slice_1d(a..z)?;
            let bal: Array1<i64> = self.bal.read_slice_1d(points(ids.iter().copied()))?;
            Ok((bal.sum(), (z - a) as u64))
        }
    }
}

// ---------------------------------------------------------------- Parquet + delta files
mod pq {
    use super::*;
    use arrow_array::{Array, ArrayRef, FixedSizeBinaryArray, Int64Array, RecordBatch, UInt32Array, UInt64Array};
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::arrow_reader::{
        ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection, RowSelectionPolicy,
        RowSelector,
    };
    use parquet::arrow::{ArrowWriter, ProjectionMask};
    use parquet::file::metadata::PageIndexPolicy;
    use parquet::file::properties::{EnabledStatistics, WriterProperties};
    use std::sync::Arc;
    const RG: usize = 65_536;
    const PAGE: usize = 1024;

    fn props(rg: usize) -> WriterProperties {
        WriterProperties::builder()
            .set_compression(parquet::basic::Compression::UNCOMPRESSED)
            .set_dictionary_enabled(false)
            .set_max_row_group_row_count(Some(rg))
            .set_data_page_row_count_limit(PAGE)
            .set_write_batch_size(PAGE)
            .set_statistics_enabled(EnabledStatistics::Page)
            .build()
    }
    fn write_atomic(dir: &Path, name: &str, batch: RecordBatch, props: WriterProperties) -> Result<()> {
        let tmp = dir.join("tmp.parquet");
        let mut w = ArrowWriter::try_new(std::fs::File::create(&tmp)?, batch.schema(), Some(props))?;
        w.write(&batch)?;
        w.into_inner()?.sync_all()?;
        std::fs::rename(&tmp, dir.join(name))?;
        fsync_dir(dir)?;
        Ok(())
    }
    fn delta_schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("balance", DataType::Int64, false),
        ]))
    }

    struct Base {
        f: std::fs::File,
        meta: ArrowReaderMetadata,
    }
    impl Base {
        fn open(p: &Path) -> Result<Self> {
            let f = std::fs::File::open(p)?;
            let meta = ArrowReaderMetadata::load(
                &f,
                ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Required),
            )?;
            Ok(Self { f, meta })
        }
        /// Read `cols` for the sorted global row positions `rows` in one pass.
        fn rows(&self, rows: &[usize], cols: &[usize], mut each: impl FnMut(&RecordBatch, usize)) -> Result<()> {
            let total = self.meta.metadata().file_metadata().num_rows() as usize;
            let mut sel = Vec::new();
            let mut pos = 0;
            for &r in rows {
                if r > pos {
                    sel.push(RowSelector::skip(r - pos));
                }
                sel.push(RowSelector::select(1));
                pos = r + 1;
            }
            if pos < total {
                sel.push(RowSelector::skip(total - pos));
            }
            let mask = ProjectionMask::roots(self.meta.parquet_schema(), cols.iter().copied());
            let r = ParquetRecordBatchReaderBuilder::new_with_metadata(self.f.try_clone()?, self.meta.clone())
                .with_projection(mask)
                .with_row_selection(RowSelection::from(sel))
                .with_row_selection_policy(RowSelectionPolicy::Selectors)
                .build()?;
            for b in r {
                let b = b?;
                for i in 0..b.num_rows() {
                    each(&b, i);
                }
            }
            Ok(())
        }
        fn row_range(&self, a: usize, z: usize, cols: &[usize], mut each: impl FnMut(&RecordBatch, usize)) -> Result<()> {
            let total = self.meta.metadata().file_metadata().num_rows() as usize;
            let sel = vec![RowSelector::skip(a), RowSelector::select(z - a), RowSelector::skip(total - z)];
            let mask = ProjectionMask::roots(self.meta.parquet_schema(), cols.iter().copied());
            let r = ParquetRecordBatchReaderBuilder::new_with_metadata(self.f.try_clone()?, self.meta.clone())
                .with_projection(mask)
                .with_row_selection(RowSelection::from(sel))
                .with_row_selection_policy(RowSelectionPolicy::Selectors)
                .build()?;
            for b in r {
                let b = b?;
                for i in 0..b.num_rows() {
                    each(&b, i);
                }
            }
            Ok(())
        }
    }
    fn u64c(b: &RecordBatch, c: usize) -> &UInt64Array {
        b.column(c).as_any().downcast_ref().unwrap()
    }
    fn i64c(b: &RecordBatch, c: usize) -> &Int64Array {
        b.column(c).as_any().downcast_ref().unwrap()
    }

    // Parquet is immutable: updates go to small delta files (LSM style). The
    // writer keeps its own memtable of updated balances for RMW.
    pub struct W {
        dir: PathBuf,
        base: Option<Base>,
        mem: HashMap<u64, i64>,
        n: usize,
    }
    pub fn create(dir: &Path) -> Result<Box<dyn Oltp>> {
        std::fs::create_dir_all(dir)?;
        Ok(Box::new(W { dir: dir.to_path_buf(), base: None, mem: HashMap::new(), n: 0 }))
    }
    impl Oltp for W {
        fn load(&mut self) -> Result<()> {
            let n = ACCOUNTS as usize;
            let schema = Arc::new(Schema::new(vec![
                Field::new("id", DataType::UInt64, false),
                Field::new("branch", DataType::UInt32, false),
                Field::new("balance", DataType::Int64, false),
                Field::new("name", DataType::FixedSizeBinary(24), false),
                Field::new("created", DataType::UInt64, false),
            ]));
            let mut names = Vec::with_capacity(n * 24);
            for id in 0..ACCOUNTS {
                names.extend_from_slice(&name_of(id));
            }
            let cols: Vec<ArrayRef> = vec![
                Arc::new(UInt64Array::from_iter_values(0..ACCOUNTS)),
                Arc::new(UInt32Array::from_iter_values((0..ACCOUNTS).map(branch_of))),
                Arc::new(Int64Array::from_iter_values(std::iter::repeat_n(START, n))),
                Arc::new(FixedSizeBinaryArray::try_new(24, names.into(), None)?),
                Arc::new(UInt64Array::from_iter_values((0..ACCOUNTS).map(created_of))),
            ];
            write_atomic(&self.dir, "base.parquet", RecordBatch::try_new(schema, cols)?, props(RG))?;
            // Secondary index file: ids sorted by branch, one row group, 1024-row
            // pages; branch b occupies rows off[b]..off[b+1].
            let lists = branch_lists();
            let ids: Vec<u64> = lists.iter().flatten().copied().collect();
            let mut off = vec![0u64];
            for l in &lists {
                off.push(off.last().unwrap() + l.len() as u64);
            }
            let s2 = Arc::new(Schema::new(vec![Field::new("id", DataType::UInt64, false)]));
            write_atomic(
                &self.dir,
                "branch_ids.parquet",
                RecordBatch::try_new(s2, vec![Arc::new(UInt64Array::from(ids))])?,
                props(n),
            )?;
            let s3 = Arc::new(Schema::new(vec![Field::new("off", DataType::UInt64, false)]));
            write_atomic(
                &self.dir,
                "branch_off.parquet",
                RecordBatch::try_new(s3, vec![Arc::new(UInt64Array::from(off))])?,
                props(n),
            )?;
            self.base = Some(Base::open(&self.dir.join("base.parquet"))?);
            Ok(())
        }
        fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
            let mut need: Vec<usize> = deltas.iter().filter(|d| !self.mem.contains_key(&d.0)).map(|d| d.0 as usize).collect();
            need.sort_unstable();
            let mut cur: HashMap<u64, i64> = HashMap::new();
            if !need.is_empty() {
                self.base.as_ref().unwrap().rows(&need, &[0, 2], |b, i| {
                    cur.insert(u64c(b, 0).value(i), i64c(b, 1).value(i));
                })?;
            }
            let mut ids = Vec::new();
            let mut bal = Vec::new();
            for &(id, d) in deltas {
                let v = self.mem.get(&id).or(cur.get(&id)).copied().ok_or("missing")? + d;
                ids.push(id);
                bal.push(v);
            }
            let batch = RecordBatch::try_new(
                delta_schema(),
                vec![Arc::new(UInt64Array::from(ids.clone())), Arc::new(Int64Array::from(bal.clone()))],
            )?;
            write_atomic(&self.dir, &format!("delta_{:06}.parquet", self.n), batch, props(RG))?;
            self.n += 1;
            for (id, v) in ids.into_iter().zip(bal) {
                self.mem.insert(id, v);
            }
            Ok(())
        }
    }

    pub struct R {
        base: Base,
        ids: Base,
        off: Vec<u64>,
        mem: HashMap<u64, i64>,
    }
    pub fn open(dir: &Path) -> Result<Box<dyn OltpRead>> {
        // Replay all delta files (newest wins) into a lookup table.
        let mut mem = HashMap::new();
        let mut names: Vec<_> = std::fs::read_dir(dir)?
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.starts_with("delta_"))
            .collect();
        names.sort();
        for n in names {
            let r = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(dir.join(n))?)?.build()?;
            for b in r {
                let b = b?;
                for i in 0..b.num_rows() {
                    mem.insert(u64c(&b, 0).value(i), i64c(&b, 1).value(i));
                }
            }
        }
        let mut off = Vec::new();
        for b in ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(dir.join("branch_off.parquet"))?)?.build()? {
            off.extend(u64c(&b?, 0).values().iter().copied());
        }
        Ok(Box::new(R {
            base: Base::open(&dir.join("base.parquet"))?,
            ids: Base::open(&dir.join("branch_ids.parquet"))?,
            off,
            mem,
        }))
    }
    impl OltpRead for R {
        fn point(&mut self, id: u64) -> Result<Row> {
            let mut row = None;
            self.base.rows(&[id as usize], &[0, 1, 2, 3, 4], |b, i| {
                let name = b.column(3).as_any().downcast_ref::<FixedSizeBinaryArray>().unwrap();
                row = Some(Row {
                    id: u64c(b, 0).value(i),
                    branch: b.column(1).as_any().downcast_ref::<UInt32Array>().unwrap().value(i),
                    balance: i64c(b, 2).value(i),
                    name: name.value(i).try_into().unwrap(),
                    created: u64c(b, 4).value(i),
                });
            })?;
            let mut row = row.ok_or("missing")?;
            if let Some(&v) = self.mem.get(&id) {
                row.balance = v;
            }
            Ok(row)
        }
        fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
            let (a, z) = (self.off[b as usize] as usize, self.off[b as usize + 1] as usize);
            let mut ids = Vec::new();
            self.ids.row_range(a, z, &[0], |bt, i| ids.push(u64c(bt, 0).value(i) as usize))?;
            ids.sort_unstable();
            let mut s = 0;
            let mem = &self.mem;
            self.base.rows(&ids, &[0, 2], |bt, i| {
                let id = u64c(bt, 0).value(i);
                s += mem.get(&id).copied().unwrap_or(i64c(bt, 1).value(i));
            })?;
            Ok((s, ids.len() as u64))
        }
    }
}

// ---------------------------------------------------------------- driver
pub fn run(fmt: &str, dir: &Path) -> Result<Metrics> {
    let p = match fmt {
        "parquet" => dir.join("pq"),
        f => dir.join(format!("bank.{f}")),
    };
    // Plan + oracle (untimed).
    let mut rng = 0xBA4Cu64;
    let mut next = |n: u64| {
        rng = splitmix(rng);
        rng % n
    };
    let mut bal = vec![START; ACCOUNTS as usize];
    let mut plan: Vec<Vec<(u64, i64)>> = Vec::with_capacity(TXNS);
    for _ in 0..TXNS {
        let mut net: std::collections::BTreeMap<u64, i64> = Default::default();
        for _ in 0..TRANSFERS {
            let (a, b, amt) = (next(ACCOUNTS), next(ACCOUNTS), next(500) as i64 + 1);
            *net.entry(a).or_default() -= amt;
            *net.entry(b).or_default() += amt;
        }
        net.retain(|_, d| *d != 0);
        for (&id, &d) in &net {
            bal[id as usize] += d;
        }
        plan.push(net.into_iter().collect());
    }
    let pts: Vec<u64> = (0..POINTS).map(|_| next(ACCOUNTS)).collect();
    let brs: Vec<u32> = (0..BRANCH_Q).map(|_| next(BRANCHES as u64) as u32).collect();
    let mut bsum = vec![(0i64, 0u64); BRANCHES as usize];
    for id in 0..ACCOUNTS {
        let e = &mut bsum[branch_of(id) as usize];
        e.0 += bal[id as usize];
        e.1 += 1;
    }

    let mut m = Metrics::default();
    let mut w = match fmt {
        "varlit" => vl::create(&p)?,
        "varlit-matrix" => vm::create(&p)?,
        "varlit-grouped" => vg::create(&p)?,
        "varlit-append" => va::create(&p)?,
        "varlit-matrix-packed" => vp::create(&p)?,
        "sqlite" => sq::create(&p)?,
        "hdf5" => h5::create(&p)?,
        "parquet" => pq::create(&p)?,
        _ => return Err("format".into()),
    };
    let t = Timer::start();
    w.load()?;
    m.set("load_s", t.secs());
    let mut lat = Vec::with_capacity(TXNS);
    let t = Timer::start();
    for d in &plan {
        let a = Timer::start();
        w.txn(d)?;
        lat.push(a.secs() * 1e3);
    }
    m.set("txn_s", t.secs());
    lat.sort_by(f64::total_cmp);
    m.set("txn_p50_ms", percentile(&lat, 0.5));
    m.set("txn_p99_ms", percentile(&lat, 0.99));
    let t = Timer::start();
    w.finish()?;
    m.set("maint_s", t.secs());
    drop(w);
    m.set("bytes", dir_bytes(dir));

    for pass in ["cold", "warm"] {
        if pass == "cold" {
            evict(dir);
        }
        let t = Timer::start();
        let mut r = match fmt {
            "varlit" => vl::open(&p)?,
            "varlit-matrix" => vm::open(&p)?,
            "varlit-grouped" => vg::open(&p)?,
            "varlit-append" => va::open(&p)?,
            "varlit-matrix-packed" => vp::open(&p)?,
            "sqlite" => sq::open(&p)?,
            "hdf5" => h5::open(&p)?,
            _ => pq::open(&p)?,
        };
        m.set(&format!("{pass}_open_ms"), t.secs() * 1e3);
        let t = Timer::start();
        let mut rows = Vec::with_capacity(POINTS);
        for &id in &pts {
            rows.push(r.point(id)?);
        }
        m.set(&format!("{pass}_points_s"), t.secs());
        for row in rows {
            let id = row.id;
            assert_eq!(
                row,
                Row { id, branch: branch_of(id), balance: bal[id as usize], name: name_of(id), created: created_of(id) },
                "point mismatch"
            );
        }
        let t = Timer::start();
        let mut got = Vec::with_capacity(BRANCH_Q);
        for &b in &brs {
            got.push(r.branch(b)?);
        }
        m.set(&format!("{pass}_branch_s"), t.secs());
        for (b, g) in brs.iter().zip(got) {
            assert_eq!(g, bsum[*b as usize], "branch mismatch");
        }
    }
    m.set("verified", true);
    Ok(m)
}
