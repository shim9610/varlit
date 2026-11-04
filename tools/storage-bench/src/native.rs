// Handles live on worker stacks; avoid an extra benchmark-only allocation/indirection.
#![allow(clippy::large_enum_variant)]
use crate::{Result, model::*};
use std::path::Path;
use varlit::*;
pub enum Writer {
    Indexed(VarlitIndexedWriter),
    Stream(VarlitStreamWriter),
}
impl Writer {
    pub fn create(p: &Path, c: &Config, indexed: bool) -> Result<Self> {
        Ok(if indexed {
            Self::Indexed(VarlitIndexedWriter::create(
                spec(true),
                p,
                options(c.cache),
                plan(),
            )?)
        } else {
            Self::Stream(VarlitStreamWriter::create(
                spec(false),
                p,
                stream_options(c.cache),
            )?)
        })
    }
    pub fn append(&mut self, t: &Transaction, h: &mut Head, c: &Config) -> Result<()> {
        macro_rules! batch {
            ($f:expr,$ty:ty,$v:expr) => {
                if c.scalar {
                    for x in $v {
                        $f.push_info(x)?;
                    }
                } else if !$v.is_empty() {
                    $f.push_iter::<$ty, _>($v, BatchOptions::default())?;
                }
            };
        }
        let mid = t.frames.len() / 2;
        let rm = t.rows.len() / 2;
        match self {
            Self::Indexed(f) => {
                f.push_unindexed_info(&t.metadata(0))?;
                batch!(f, Frame, &t.frames[..mid]);
                batch!(f, Row, &t.rows[..rm]);
                f.push_unindexed_info(&t.metadata(1))?;
                batch!(f, Frame, &t.frames[mid..]);
                batch!(f, Row, &t.rows[rm..]);
            }
            Self::Stream(f) => {
                f.push_info(&t.metadata(0))?;
                batch!(f, Frame, &t.frames[..mid]);
                batch!(f, Row, &t.rows[..rm]);
                f.push_info(&t.metadata(1))?;
                batch!(f, Frame, &t.frames[mid..]);
                batch!(f, Row, &t.rows[rm..]);
            }
        }
        if std::env::var_os("VARLIT_BENCH_VISIBILITY_GATE").is_some() {
            match self {
                Self::Indexed(f) => f.sync()?,
                Self::Stream(f) => f.sync()?,
            }
        }
        crate::before_publish(t.txid);
        h.txns += 1;
        h.frames += t.frames.len() as u64;
        h.rows += t.rows.len() as u64;
        h.metas += 2;
        match self {
            Self::Indexed(f) => {
                f.push_info(h)?;
                f.sync()?;
            }
            Self::Stream(f) => {
                f.push_info(h)?;
                f.sync()?;
            }
        }
        Ok(())
    }
    pub fn initial(&mut self) -> Result<()> {
        let h = Head::default();
        match self {
            Self::Indexed(f) => {
                f.push_info(&h)?;
                f.sync()?;
            }
            Self::Stream(f) => {
                f.push_info(&h)?;
                f.sync()?;
            }
        }
        Ok(())
    }
}
pub enum File {
    Indexed(VarlitIndexedReader),
    Stream(VarlitStreamReader),
}
pub struct Reader {
    f: File,
    frames: StreamingBlocks<Frame>,
    rows: StreamingBlocks<Row>,
    metas: StreamingBlocks<Metadata>,
    heads: StreamingBlocks<Head>,
    head: Head,
    fc: u64,
    rc: u64,
    mc: u64,
}
impl Reader {
    pub fn open(p: &Path, c: &Config, indexed: bool) -> Result<Self> {
        let f = if indexed {
            File::Indexed(VarlitIndexedReader::open(
                spec(true),
                p,
                options(c.cache),
                plan(),
            )?)
        } else {
            File::Stream(VarlitStreamReader::open(
                spec(false),
                p,
                stream_options(c.cache),
            )?)
        };
        macro_rules! blocks {
            ($t:ty) => {
                match &f {
                    File::Indexed(r) => r.blocks::<$t>()?,
                    File::Stream(r) => r.blocks::<$t>()?,
                }
            };
        }
        let (frames, rows, metas, heads) = (
            blocks!(Frame),
            blocks!(Row),
            blocks!(Metadata),
            blocks!(Head),
        );
        Ok(Self {
            f,
            frames,
            rows,
            metas,
            heads,
            head: Head::default(),
            fc: 0,
            rc: 0,
            mc: 0,
        })
    }
    pub fn refresh(&mut self) -> Result<Head> {
        match &mut self.f {
            File::Indexed(f) => {
                f.follow()?;
                self.head = f.get::<Head>(&0)?.ok_or("missing head")?;
            }
            File::Stream(f) => {
                f.follow_blocks(&mut self.heads)?;
                for h in self.heads.by_ref() {
                    self.head = h?;
                }
            }
        }
        assert!(self.head.valid());
        Ok(self.head.clone())
    }
    pub fn consume(&mut self, h: &Head, o: &mut Oracle, c: &Config) -> Result<()> {
        // Capture the publication boundary before advancing individual typed cursors.
        // Later roots may be visible; unique immutable records through this boundary remain valid.
        macro_rules! follow {
            ($cursor:expr) => {
                match &mut self.f {
                    File::Indexed(f) => {
                        f.follow_blocks($cursor)?;
                    }
                    File::Stream(f) => {
                        f.follow_blocks($cursor)?;
                    }
                }
            };
        }
        follow!(&mut self.frames);
        follow!(&mut self.rows);
        follow!(&mut self.metas);
        while self.fc < h.frames {
            let v = self.frames.next().ok_or("short frame prefix")??;
            o.frame(&v);
            if c.lookups {
                if let File::Indexed(r) = &self.f {
                    let k = (v.scan, v.frame);
                    let found = r.get::<Frame>(&k)?.ok_or("missing key")?;
                    assert_eq!(found.id, v.id);
                    assert_eq!(found.payload, v.payload);
                    o.point_payload(v.id, &found.payload);
                    o.lookup_count += 1;
                } else {
                    return Err("stream has no arbitrary-key lookup".into());
                }
            }
            self.fc += 1;
        }
        while self.rc < h.rows {
            let v = self.rows.next().ok_or("short row prefix")??;
            o.row(&v);
            if c.lookups {
                if let File::Indexed(r) = &self.f {
                    assert_eq!(r.get::<Row>(&v.id)?.ok_or("missing row")?, v);
                    o.lookup_count += 1;
                } else {
                    return Err("stream has no arbitrary-key lookup".into());
                }
            }
            self.rc += 1;
        }
        while self.mc < h.metas {
            let v = self.metas.next().ok_or("short metadata prefix")??;
            o.metadata(&v);
            self.mc += 1;
        }
        Ok(())
    }
}
