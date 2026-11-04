//! Dense bounded IDs: compare Varlit's matrix API as well as its general-key index.
use crate::{Result, model::*};
use std::path::Path;
use varlit::*;
varlit_format! {
 pub format TableMatrix{
  magic:b"BTABLE01";version:1;integrity:crc32_with_header;
  dims{entry:u32,slot:u32,}
  commit:cell_bitmap{keyspace=[entry,slot];categories=[data,log,head];};
  blocks{
   matrix Cell(id=11,dims=[entry,slot],category=data){id:u64,tenant:u32,status:u32,created:u64,amount:i64,body:[u8;32]}
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
        if c.scenario != "table" {
            return Err("matrix table profile requires table workload".into());
        }
        let f = if create {
            TableMatrix::spec().create_with_dims(
                p,
                MatrixDimensions::from_pairs([("entry", c.records as u64), ("slot", 1)]),
            )?
        } else {
            VarlitFile::open_readonly(TableMatrix::spec(), p)?
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
        for r in &t.rows {
            self.f.write_matrix_cell(
                k(r.id),
                &Cell {
                    id: r.id,
                    tenant: r.tenant,
                    status: r.status,
                    created: r.created,
                    amount: r.amount,
                    body: r.body,
                },
            )?;
            self.f.commit_matrix_cell::<Cell>(k(r.id))?;
        }
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
            let mut batch = vec![
                Cell {
                    id: 0,
                    tenant: 0,
                    status: 0,
                    created: 0,
                    amount: 0,
                    body: [0; 32]
                };
                l.count as usize
            ];
            self.f.read_matrix_run_into(k(l.first), &mut batch)?;
            for v in batch {
                let r = Row {
                    id: v.id,
                    tenant: v.tenant,
                    status: v.status,
                    created: v.created,
                    amount: v.amount,
                    body: v.body,
                };
                o.row(&r);
                if c.lookups {
                    let found = self.f.read_matrix_cell::<Cell>(k(r.id))?;
                    assert_eq!(found, v);
                    o.lookup_count += 1;
                }
            }
        }
        Ok(())
    }
}
