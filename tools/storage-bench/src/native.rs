use crate::{Result, model::*};
use std::path::Path;
use varlit::*;

pub struct Writer(VarlitStreamWriter);
impl Writer {
    pub fn create(p: &Path, c: &Config) -> Result<Self> {
        Ok(Self(VarlitStreamWriter::create(
            spec(),
            p,
            stream_options(c.cache),
        )?))
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
        let f = &mut self.0;
        f.push_info(&t.metadata(0))?;
        batch!(f, Frame, &t.frames[..mid]);
        batch!(f, Row, &t.rows[..rm]);
        f.push_info(&t.metadata(1))?;
        batch!(f, Frame, &t.frames[mid..]);
        batch!(f, Row, &t.rows[rm..]);
        if std::env::var_os("VARLIT_BENCH_VISIBILITY_GATE").is_some() {
            f.sync()?;
        }
        crate::before_publish(h.txns);
        h.txns += 1;
        h.frames += t.frames.len() as u64;
        h.rows += t.rows.len() as u64;
        h.metas += 2;
        f.push_info(h)?;
        f.sync()?;
        Ok(())
    }
    pub fn initial(&mut self) -> Result<()> {
        self.0.push_info(&Head::default())?;
        self.0.sync()?;
        Ok(())
    }
}
pub struct Reader {
    f: VarlitStreamReader,
    frames: StreamingBlocks<Frame>,
    rows: StreamingBlocks<Row>,
    metas: StreamingBlocks<Metadata>,
    heads: StreamingBlocks<Head>,
    head: Head,
    // Caller-owned decode targets, reused across every record of the hot loops.
    frame: Frame,
    row: Row,
    meta: Metadata,
    fc: u64,
    rc: u64,
    mc: u64,
}
impl Reader {
    pub fn open(p: &Path, c: &Config) -> Result<Self> {
        let mut options = stream_options(c.cache);
        options.limits = options.limits.overlay(selected_read_limits()?);
        let f = VarlitStreamReader::open(spec(), p, options)?;
        let (frames, rows, metas, heads) = (
            f.blocks::<Frame>()?,
            f.blocks::<Row>()?,
            f.blocks::<Metadata>()?,
            f.blocks::<Head>()?,
        );
        Ok(Self {
            f,
            frames,
            rows,
            metas,
            heads,
            head: Head::default(),
            frame: Frame::empty(),
            row: Row::empty(),
            meta: Metadata::empty(),
            fc: 0,
            rc: 0,
            mc: 0,
        })
    }
    pub fn refresh(&mut self) -> Result<Head> {
        self.f.follow_blocks(&mut self.heads)?;
        while self.heads.next_into(&mut self.head)? {}
        assert!(self.head.valid());
        Ok(self.head.clone())
    }
    pub fn consume(&mut self, h: &Head, o: &mut Oracle, c: &Config) -> Result<()> {
        // Capture the publication boundary before advancing individual typed cursors.
        // Later roots may be visible; unique immutable records through this boundary remain valid.
        self.f.follow_blocks(&mut self.frames)?;
        self.f.follow_blocks(&mut self.rows)?;
        self.f.follow_blocks(&mut self.metas)?;
        while self.fc < h.frames {
            if !self.frames.next_into(&mut self.frame)? {
                return Err("short frame prefix".into());
            }
            o.frame(&self.frame);
            if c.lookups {
                return Err("stream has no arbitrary-key lookup".into());
            }
            self.fc += 1;
        }
        while self.rc < h.rows {
            if !self.rows.next_into(&mut self.row)? {
                return Err("short row prefix".into());
            }
            o.row(&self.row);
            if c.lookups {
                return Err("stream has no arbitrary-key lookup".into());
            }
            self.rc += 1;
        }
        while self.mc < h.metas {
            if !self.metas.next_into(&mut self.meta)? {
                return Err("short metadata prefix".into());
            }
            o.metadata(&self.meta);
            self.mc += 1;
        }
        Ok(())
    }
}
