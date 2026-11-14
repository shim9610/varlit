//! Varlit packed matrix: 256 accounts per 11 KiB matrix cell (dense ids map
//! straight to cells, no key index); the branch secondary index is CSR in two
//! more matrix blocks (ids and offsets, 1024 per cell). A transaction rewrites
//! the touched cells and `sync()`s; `compact_matrix()` reclaims superseded
//! generation pages at the end.
//! app-built: CSR branch index in matrix cells (csr.rs); packing in pack.rs
use super::pack::*;
use super::*;
use varlit::{MatrixDimensions, MatrixKey, VarlitFile, varlit_format};

const IDS: usize = 1024;
const CB: usize = G * AB; // 11264 bytes per cell

varlit_format! {
    pub format BankPacked {
        magic: b"BANKPACK";
        version: 1;
        limits {
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            matrix_dimension: 1_000_000;
            matrix_cells: 4_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 1_073_741_824;
            matrix_slot_region: 34_359_738_368;
            sidecar: 1_073_741_824;
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
fn k(n: usize) -> MatrixKey {
    MatrixKey::new(n as u64, 0)
}
fn u64s(b: &[u8]) -> impl Iterator<Item = u64> + '_ {
    b.chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
}

pub struct W(VarlitFile);
pub fn create(p: &Path) -> Result<Box<dyn Oltp>> {
    let f = BankPacked::spec().create_with_dims(
        p,
        MatrixDimensions::from_pairs([("entry", ngroups() as u64), ("slot", 1)]),
    )?;
    Ok(Box::new(W(f)))
}
impl W {
    fn put_u64_cell(&mut self, brid: bool, c: usize, v: &[u64]) -> Result<()> {
        let mut b = vec![0u8; IDS * 8];
        for (i, x) in v.iter().enumerate() {
            b[i * 8..i * 8 + 8].copy_from_slice(&x.to_le_bytes());
        }
        if brid {
            self.0.write_matrix_cell_payload::<BrIdCell>(k(c), &b)?;
            self.0.commit_matrix_cell::<BrIdCell>(k(c))?;
        } else {
            self.0.write_matrix_cell_payload::<BrOffCell>(k(c), &b)?;
            self.0.commit_matrix_cell::<BrOffCell>(k(c))?;
        }
        Ok(())
    }
}
impl Oltp for W {
    fn load(&mut self) -> Result<()> {
        let mut buf = vec![0u8; CB];
        for g in 0..ngroups() {
            initial_group(g, &mut buf);
            self.0.write_matrix_cell_payload::<AcctCell>(k(g), &buf)?;
            self.0.commit_matrix_cell::<AcctCell>(k(g))?;
        }
        // Stream the CSR into cells.
        let (mut ids, mut offs) = (Vec::with_capacity(IDS), vec![0u64]);
        let (mut idc, mut offc, mut pos) = (0usize, 0usize, 0u64);
        let mut pend: Vec<(bool, usize, Vec<u64>)> = Vec::new();
        csr::by_branch(&std::env::temp_dir().join("csr"), |_, list| {
            for &id in list {
                ids.push(id);
                if ids.len() == IDS {
                    pend.push((true, idc, std::mem::take(&mut ids)));
                    idc += 1;
                }
            }
            pos += list.len() as u64;
            offs.push(pos);
            if offs.len() == IDS {
                pend.push((false, offc, std::mem::take(&mut offs)));
                offc += 1;
            }
            Ok(())
        })?;
        pend.push((true, idc, ids));
        pend.push((false, offc, offs));
        for (brid, c, v) in pend {
            if !v.is_empty() {
                self.put_u64_cell(brid, c, &v)?;
            }
        }
        self.0.sync()?;
        Ok(())
    }
    fn txn(&mut self, deltas: &[(u64, i64)]) -> Result<()> {
        let mut buf = vec![0u8; CB];
        for (g, ups) in by_group(deltas) {
            self.0
                .read_matrix_run_bytes_into::<AcctCell>(k(g), &mut buf)?;
            for (slot, d) in ups {
                add_bal(&mut buf, slot, d);
            }
            self.0.write_matrix_cell_payload::<AcctCell>(k(g), &buf)?;
            self.0.commit_matrix_cell::<AcctCell>(k(g))?;
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
    Ok(Box::new(R {
        r: vl_spec(BankPacked::spec()).open_reader(p)?,
        buf: vec![0; CB],
    }))
}
impl R {
    fn u64_range<T: varlit::VarlitMatrixBlock>(&self, a: usize, z: usize) -> Result<Vec<u64>> {
        if z == a {
            return Ok(Vec::new());
        }
        let (c0, c1) = (a / IDS, (z - 1) / IDS);
        let mut b = vec![0u8; (c1 - c0 + 1) * IDS * 8];
        self.r.read_matrix_run_bytes_into::<T>(k(c0), &mut b)?;
        Ok(u64s(&b).skip(a - c0 * IDS).take(z - a).collect())
    }
}
impl OltpRead for R {
    fn point(&mut self, id: u64) -> Result<Row> {
        self.r
            .read_matrix_run_bytes_into::<AcctCell>(k(id as usize / G), &mut self.buf)?;
        let s = id as usize % G;
        Ok(dec(id, &self.buf[s * AB..(s + 1) * AB]))
    }
    fn branch(&mut self, b: u32) -> Result<(i64, u64)> {
        let off = self.u64_range::<BrOffCell>(b as usize, b as usize + 2)?;
        let ids = self.u64_range::<BrIdCell>(off[0] as usize, off[1] as usize)?;
        let mut sum = 0;
        for (g, slots) in by_group(&ids.iter().map(|&id| (id, 0)).collect::<Vec<_>>()) {
            self.r
                .read_matrix_run_bytes_into::<AcctCell>(k(g), &mut self.buf)?;
            for (slot, _) in slots {
                sum += bal_at(&self.buf, slot);
            }
        }
        Ok((sum, ids.len() as u64))
    }
}
