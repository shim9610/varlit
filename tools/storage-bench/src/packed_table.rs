//! Dense IDs with transaction-aligned 100-row cells; no application point-result cache.
use crate::{Result, model::*};
use std::path::Path;
use varlit::*;
varlit_format! {
 pub format PackedTableMatrix{
  magic:b"BTPACK01";version:1;integrity:crc32_with_header;
  dims{entry:u32,slot:u32,}
  commit:cell_bitmap{keyspace=[entry,slot];categories=[data,log,head];};
  blocks{
   matrix Cell(id=11,dims=[entry,slot],category=data){data:[u8;6400]}
   matrix Log(id=12,dims=[entry,slot],category=log){txid:u64,source:u32,flags0:u32,txid1:u64,source1:u32,flags1:u32,first:u64,count:u64}
   matrix Root(id=13,dims=[entry,slot],category=head){txns:u64,rows:u64,metas:u64}
  }
 }
}
fn k(n: u64) -> MatrixKey {
    MatrixKey::new(n, 0)
}
pub struct Matrix {
    f: VarlitFile,
}
impl Matrix {
    pub fn open(p: &Path, c: &Config, create: bool) -> Result<Self> {
        if c.scenario != "table"
            || c.per_tx != 100
            || !(c.records / c.producers).is_multiple_of(100)
        {
            return Err(
                "packed matrix profile requires complete, aligned 100-row transactions".into(),
            );
        }
        let f = if create {
            PackedTableMatrix::spec().create_with_dims(
                p,
                MatrixDimensions::from_pairs([("entry", (c.records / 100) as u64), ("slot", 1)]),
            )?
        } else {
            VarlitFile::open_readonly(PackedTableMatrix::spec(), p)?
        };
        let mut r = Self { f };
        if create {
            r.root(&Head::default())?;
        }
        Ok(r)
    }
    fn root(&mut self, h: &Head) -> Result<()> {
        self.f.write_matrix_cell(
            k(0),
            &Root {
                txns: h.txns,
                rows: h.rows,
                metas: h.metas,
            },
        )?;
        self.f.commit_matrix_cell::<Root>(k(0))?;
        self.f.sync()?;
        Ok(())
    }
    pub fn append(&mut self, t: &Transaction, h: &mut Head) -> Result<()> {
        assert!(t.frames.is_empty());
        assert!(t.rows.windows(2).all(|r| r[1].id == r[0].id + 1));
        assert_eq!(t.rows.len(), 100);
        assert!(t.rows[0].id.is_multiple_of(100));
        let mut payload = [0u8; 6400];
        for (r, out) in t.rows.iter().zip(payload.chunks_exact_mut(64)) {
            out[..8].copy_from_slice(&r.id.to_le_bytes());
            out[8..12].copy_from_slice(&r.tenant.to_le_bytes());
            out[12..16].copy_from_slice(&r.status.to_le_bytes());
            out[16..24].copy_from_slice(&r.created.to_le_bytes());
            out[24..32].copy_from_slice(&r.amount.to_le_bytes());
            out[32..].copy_from_slice(&r.body);
        }
        self.f
            .write_matrix_cell_payload::<Cell>(k(t.rows[0].id / 100), &payload)?;
        self.f.commit_matrix_cell::<Cell>(k(t.rows[0].id / 100))?;
        self.f.write_matrix_cell(
            k(h.txns),
            &Log {
                txid: t.txid,
                source: t.source,
                flags0: 0,
                txid1: t.txid,
                source1: t.source,
                flags1: 1,
                first: t.rows[0].id,
                count: t.rows.len() as u64,
            },
        )?;
        self.f.commit_matrix_cell::<Log>(k(h.txns))?;
        if std::env::var_os("VARLIT_BENCH_VISIBILITY_GATE").is_some() {
            self.f.sync()?;
        }
        crate::before_publish(t.txid);
        h.txns += 1;
        h.rows += t.rows.len() as u64;
        h.metas += 2;
        self.root(h)
    }
    pub fn generation_bytes(&self) -> Result<u64> {
        Ok(std::fs::metadata(
            self.f
                .matrix_generation_path()
                .ok_or("missing matrix generation")?,
        )?
        .len())
    }
    pub fn compact(&mut self) -> Result<()> {
        self.f.compact_matrix()?;
        Ok(())
    }
    pub fn refresh(&mut self) -> Result<Head> {
        self.f.follow()?;
        let r = self.f.read_matrix_cell::<Root>(k(0))?;
        let h = Head {
            key: 0,
            txns: r.txns,
            rows: r.rows,
            metas: r.metas,
            frames: 0,
        };
        assert!(h.valid());
        Ok(h)
    }
    pub fn consume(&self, old: &Head, h: &Head, o: &mut Oracle, c: &Config) -> Result<()> {
        for n in old.txns..h.txns {
            let l = self.f.read_matrix_cell::<Log>(k(n))?;
            o.metadata(&Metadata {
                txid: l.txid,
                source: l.source,
                flags: l.flags0,
            });
            o.metadata(&Metadata {
                txid: l.txid1,
                source: l.source1,
                flags: l.flags1,
            });
            assert_eq!(l.count, 100);
            let mut payload = [0u8; 6400];
            self.f
                .read_matrix_run_bytes_into::<Cell>(k(l.first / 100), &mut payload)?;
            let mut point = [0u8; 6400];
            for bytes in payload.chunks_exact(64) {
                let r = decode(bytes);
                o.row(&r);
                if c.lookups {
                    // Execute a real matrix read for each point, including its
                    // whole-cell integrity check. Never return the just-scanned row as a cache hit.
                    self.f
                        .read_matrix_run_bytes_into::<Cell>(k(r.id / 100), &mut point)?;
                    let offset = (r.id % 100) as usize * 64;
                    assert_eq!(decode(&point[offset..offset + 64]), r);
                    o.lookup_count += 1;
                }
            }
        }
        Ok(())
    }
}

fn decode(b: &[u8]) -> Row {
    Row {
        id: u64::from_le_bytes(b[..8].try_into().unwrap()),
        tenant: u32::from_le_bytes(b[8..12].try_into().unwrap()),
        status: u32::from_le_bytes(b[12..16].try_into().unwrap()),
        created: u64::from_le_bytes(b[16..24].try_into().unwrap()),
        amount: i64::from_le_bytes(b[24..32].try_into().unwrap()),
        body: b[32..64].try_into().unwrap(),
    }
}
