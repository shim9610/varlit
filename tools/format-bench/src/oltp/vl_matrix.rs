//! Varlit matrix, one account per cell: dense ids map straight to cells
//! (no key, no index), each cell the account's packed 44 bytes. The keyspace
//! is `COLS` cells wide so the default tile (32 x 32 cells of 48 bytes)
//! fills; account `id` is row `id / COLS`, column `id % COLS`, and runs
//! cross rows in that order. A transaction reads each touched account,
//! writes its cell back and `sync()`s. The branch index is CSR in two more
//! matrix blocks (ids and offsets, 1024 per cell), as in `vl_packed`.
//! app-built: CSR branch index in matrix cells (csr.rs)
use super::*;
use varlit::{MatrixDimensions, MatrixKey, VarlitFile, varlit_format};

const COLS: u64 = 1024;
const IDS: usize = 1024;

varlit_format! {
    pub format BankMatrix {
        magic: b"BANKMTX1";
        version: 1;
        limits {
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            matrix_dimension: 1_000_000;
            matrix_cells: 4_000_000_000;
            matrix_bitmap: 1_073_741_824;
            matrix_crc: 4_294_967_296;
            matrix_metadata: 1_073_741_824;
            matrix_slot_region: 68_719_476_736;
            sidecar: 1_073_741_824;
        }
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [acct, brid, broff]; };
        blocks {
            matrix Acct(id = 1, dims = [row, col], category = acct) {
                branch: u32,
                balance: i64,
                name: [u8; 24],
                created: u64,
            }
            matrix BrIds(id = 2, dims = [row, col], category = brid) { ids: [u64; 1024], }
            matrix BrOffs(id = 3, dims = [row, col], category = broff) { off: [u64; 1024], }
        }
    }
}

fn k(n: u64) -> MatrixKey {
    MatrixKey::new(n / COLS, n % COLS)
}
fn acct(id: u64) -> Acct {
    let r = initial_row(id);
    Acct {
        branch: r.branch,
        balance: r.balance,
        name: r.name,
        created: r.created,
    }
}
fn row(id: u64, a: &Acct) -> Row {
    Row {
        id,
        branch: a.branch,
        balance: a.balance,
        name: a.name,
        created: a.created,
    }
}

pub struct W {
    f: VarlitFile,
    a: Acct,
}
pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
    let f = BankMatrix::spec().create_with_dims(
        p,
        MatrixDimensions::from_pairs([("row", accounts().div_ceil(COLS)), ("col", COLS)]),
    )?;
    Ok(Box::new(W { f, a: acct(0) }))
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        // Whole rows of the keyspace at a time, as one run each: a cell's
        // payload is the account's packed 44 bytes (`pack::enc`, the
        // block's field order).
        let mut run = vec![0u8; COLS as usize * super::pack::AB];
        let mut id = 0;
        while id < accounts() {
            let n = (accounts() - id).min(COLS) as usize;
            for (i, cell) in run[..n * super::pack::AB]
                .chunks_exact_mut(super::pack::AB)
                .enumerate()
            {
                super::pack::enc(id + i as u64, START, cell);
            }
            self.f
                .write_matrix_run_bytes::<Acct>(k(id), &run[..n * super::pack::AB])?;
            id += n as u64;
        }
        // The CSR: ids in branch order, then the offset of each branch.
        let (mut ids, mut offs) = (BrIds { ids: [0; IDS] }, BrOffs { off: [0; IDS] });
        let (mut ni, mut no, mut cell_i, mut cell_o, mut pos) = (0usize, 1usize, 0u64, 0u64, 0u64);
        let f = &mut self.f;
        csr::by_branch(&std::env::temp_dir().join("csr"), |_, list| {
            for &id in list {
                ids.ids[ni] = id;
                ni += 1;
                if ni == IDS {
                    f.write_matrix_cell(k(cell_i), &ids)?;
                    f.commit_matrix_cell::<BrIds>(k(cell_i))?;
                    cell_i += 1;
                    ni = 0;
                }
            }
            pos += list.len() as u64;
            offs.off[no] = pos;
            no += 1;
            if no == IDS {
                f.write_matrix_cell(k(cell_o), &offs)?;
                f.commit_matrix_cell::<BrOffs>(k(cell_o))?;
                cell_o += 1;
                no = 0;
            }
            Ok(())
        })?;
        if ni > 0 {
            f.write_matrix_cell(k(cell_i), &ids)?;
            f.commit_matrix_cell::<BrIds>(k(cell_i))?;
        }
        if no > 0 {
            f.write_matrix_cell(k(cell_o), &offs)?;
            f.commit_matrix_cell::<BrOffs>(k(cell_o))?;
        }
        self.f.sync()?;
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        for &(id, d) in deltas {
            self.f.read_matrix_cell(k(id), &mut self.a)?;
            self.a.balance += d;
            self.f.write_matrix_cell(k(id), &self.a)?;
            self.f.commit_matrix_cell::<Acct>(k(id))?;
        }
        self.f.sync()?;
        Ok(())
    }
}

pub struct R {
    r: varlit::VarlitReader,
    a: Acct,
    ids: BrIds,
    offs: [BrOffs; 2],
}
pub fn open(p: &Path) -> Result<Box<dyn OltpRead>> {
    Ok(Box::new(R {
        r: vl_spec(BankMatrix::spec()).open_reader(p)?,
        a: acct(0),
        ids: BrIds { ids: [0; IDS] },
        offs: [BrOffs { off: [0; IDS] }, BrOffs { off: [0; IDS] }],
    }))
}
impl R {
    fn offset(&mut self, i: u64) -> Result<u64> {
        let c = i / IDS as u64;
        self.r.read_matrix_cell(k(c), &mut self.offs[0])?;
        Ok(self.offs[0].off[(i % IDS as u64) as usize])
    }
}
impl OltpRead for R {
    fn point(&mut self, id: u64) -> Result<Row> {
        self.r.read_matrix_cell(k(id), &mut self.a)?;
        Ok(row(id, &self.a))
    }
    fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
        let (from, to) = (self.offset(u64::from(b))?, self.offset(u64::from(b) + 1)?);
        let mut sum = 0;
        let mut cell = u64::MAX;
        for i in from..to {
            if i / IDS as u64 != cell {
                cell = i / IDS as u64;
                self.r.read_matrix_cell(k(cell), &mut self.ids)?;
            }
            let id = self.ids.ids[(i % IDS as u64) as usize];
            self.r.read_matrix_cell(k(id), &mut self.a)?;
            sum += self.a.balance;
        }
        Ok((sum, to - from))
    }
}
