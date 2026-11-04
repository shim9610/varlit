use crate::Result;
use varlit::*;
#[derive(Clone, Debug, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "fixed")]
pub struct Metadata {
    pub txid: u64,
    pub source: u32,
    pub flags: u32,
}
#[derive(Clone, Debug, VarlitBlock)]
#[varlit(id = 2, version = 1, kind = "variable", key = "scan, frame")]
pub struct Frame {
    #[varlit(field_id = 1)]
    pub scan: u32,
    #[varlit(field_id = 2)]
    pub frame: u32,
    #[varlit(field_id = 3)]
    pub id: u64,
    #[varlit(field_id = 4)]
    pub payload: Vec<u8>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, VarlitBlock)]
#[varlit(id = 3, version = 1, kind = "fixed", key = "id")]
pub struct Row {
    pub id: u64,
    pub tenant: u32,
    pub status: u32,
    pub created: u64,
    pub amount: i64,
    pub body: [u8; 32],
}
#[derive(Clone, Debug, Default, VarlitBlock)]
#[varlit(id = 4, version = 1, kind = "variable", key = "key")]
pub struct Head {
    #[varlit(field_id = 1)]
    pub key: u8,
    #[varlit(field_id = 2)]
    pub txns: u64,
    #[varlit(field_id = 3)]
    pub frames: u64,
    #[varlit(field_id = 4)]
    pub rows: u64,
    #[varlit(field_id = 5)]
    pub metas: u64,
}
impl Head {
    pub fn valid(&self) -> bool {
        self.key == 0 && self.metas == self.txns * 2
    }
}
static BLOCKS: &[BlockDescriptor] = &[
    BlockDescriptor {
        id: 1,
        name: "Metadata",
        version: 1,
        kind: BlockKind::Fixed,
        fields: &[],
    },
    BlockDescriptor {
        id: 2,
        name: "Frame",
        version: 1,
        kind: BlockKind::Variable,
        fields: &[],
    },
    BlockDescriptor {
        id: 3,
        name: "Row",
        version: 1,
        kind: BlockKind::Fixed,
        fields: &[],
    },
    BlockDescriptor {
        id: 4,
        name: "Head",
        version: 1,
        kind: BlockKind::Variable,
        fields: &[],
    },
];
static INDEXED: &[DiskIndexedBlock] = &[
    DiskIndexedBlock::of::<Frame>(),
    DiskIndexedBlock::of::<Row>(),
    DiskIndexedBlock::of::<Head>(),
];
pub fn spec(indexed: bool) -> FormatSpec {
    FormatSpec::new(
        b"MIXBEN01",
        1,
        Endian::Little,
        0,
        if indexed {
            IndexPolicy::KeyedOffsetChain
        } else {
            IndexPolicy::BlockOffsetChain
        },
        IntegrityPolicy::Crc32,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_block_identities(&[
        Metadata::IDENTITY,
        Frame::IDENTITY,
        Row::IDENTITY,
        Head::IDENTITY,
    ])
}
pub fn plan() -> DiskIndexPlan {
    DiskIndexPlan::canonical(spec(true), INDEXED).unwrap()
}
pub fn options(cache: usize) -> DiskIndexOptions {
    DiskIndexOptions {
        cache_bytes: cache,
        ..Default::default()
    }
}
pub fn stream_options(cache: usize) -> StreamOptions {
    StreamOptions {
        state: options(cache),
        ..Default::default()
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub scenario: String,
    pub records: usize,
    pub payload: usize,
    pub variable: bool,
    pub per_tx: usize,
    pub producers: usize,
    pub readers: usize,
    pub lookups: bool,
    pub scalar: bool,
    pub cache: usize,
    pub page: usize,
    pub mmap: usize,
    pub fletcher: bool,
    #[serde(default)]
    pub compact_bytes: u64,
    #[serde(default)]
    pub hdf_chunk: usize,
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if !["mixed", "table"].contains(&self.scenario.as_str())
            || self.records == 0
            || self.payload < 8
            || self.per_tx == 0
            || self.producers == 0
            || self.readers == 0
            || !self.records.is_multiple_of(self.producers)
            || self.producers > 64
            || self.readers > 64
            || self.cache < 65536
        {
            return Err("invalid benchmark configuration".into());
        }
        Ok(())
    }
    pub fn txns_per_producer(&self) -> usize {
        (self.records / self.producers).div_ceil(self.per_tx)
    }
    pub fn transactions(&self) -> usize {
        self.txns_per_producer() * self.producers
    }
    pub fn transaction(&self, source: usize, local: usize) -> Transaction {
        let txid = (local * self.producers + source) as u64;
        let start = local * self.per_tx;
        let n = self.per_tx.min(self.records / self.producers - start);
        let mut frames = Vec::new();
        let mut rows = Vec::new();
        for j in 0..n {
            let id = (source * (self.records / self.producers) + start + j) as u64;
            if self.scenario == "mixed" {
                let size = if self.variable && id.is_multiple_of(3) {
                    (self.payload / 4).max(8)
                } else {
                    self.payload
                };
                let mut data = vec![(id % 251) as u8; size];
                data[..8].copy_from_slice(&id.to_le_bytes());
                let (scan, frame) = key(id);
                frames.push(Frame {
                    scan,
                    frame,
                    id,
                    payload: data,
                });
            } else {
                rows.push(row(id));
            }
        }
        Transaction {
            txid,
            source: source as u32,
            frames,
            rows,
        }
    }
    pub fn expected_payload(&self, id: u64) -> usize {
        if self.variable && id.is_multiple_of(3) {
            (self.payload / 4).max(8)
        } else {
            self.payload
        }
    }
}
#[derive(Debug)]
pub struct Transaction {
    pub txid: u64,
    pub source: u32,
    pub frames: Vec<Frame>,
    pub rows: Vec<Row>,
}
impl Transaction {
    pub fn metadata(&self, flags: u32) -> Metadata {
        Metadata {
            txid: self.txid,
            source: self.source,
            flags,
        }
    }
    pub fn ids(&self) -> Vec<u64> {
        self.frames
            .iter()
            .map(|x| x.id)
            .chain(self.rows.iter().map(|x| x.id))
            .collect()
    }
}
pub fn key(id: u64) -> (u32, u32) {
    (
        (id as u32).rotate_left(13) ^ 0x6187a42d,
        (id >> 32) as u32 ^ (id as u32).wrapping_mul(2654435761),
    )
}
pub fn row(id: u64) -> Row {
    let mut body = [(id % 251) as u8; 32];
    body[..8].copy_from_slice(&id.to_le_bytes());
    Row {
        id,
        tenant: (id % 64) as u32,
        status: ((id / 64) % 4) as u32,
        created: id * 1000,
        amount: (id % 1000) as i64 - 500,
        body,
    }
}
pub struct Oracle {
    cfg: Config,
    seen: Vec<u64>,
    meta: Vec<u8>,
    pub data_count: usize,
    pub meta_count: usize,
    pub bytes: u64,
    pub lookup_count: usize,
    pub order_hash: u64,
    pub metadata_hash: u64,
}
impl Oracle {
    pub fn new(c: &Config) -> Self {
        Self {
            cfg: c.clone(),
            seen: vec![0; c.records.div_ceil(64)],
            meta: vec![0; c.transactions()],
            data_count: 0,
            meta_count: 0,
            bytes: 0,
            lookup_count: 0,
            order_hash: 0,
            metadata_hash: 0,
        }
    }
    fn id(&mut self, id: u64) {
        assert!((id as usize) < self.cfg.records);
        let (i, b) = (id as usize / 64, 1u64 << (id % 64));
        assert_eq!(self.seen[i] & b, 0, "duplicate logical record");
        self.seen[i] |= b;
        self.data_count += 1;
        self.order_hash = ordered_hash(self.order_hash, id);
    }
    pub fn frame(&mut self, f: &Frame) {
        self.frame_view(f.id, f.scan, f.frame, &f.payload);
    }
    pub fn frame_view(&mut self, id: u64, scan: u32, frame: u32, p: &[u8]) {
        assert_eq!(key(id), (scan, frame));
        assert_eq!(p.len(), self.cfg.expected_payload(id));
        assert_eq!(&p[..8], &id.to_le_bytes());
        assert!(p[8..].iter().all(|&x| x == (id % 251) as u8));
        self.id(id);
        self.bytes += p.len() as u64;
    }
    pub fn row(&mut self, r: &Row) {
        assert_eq!(*r, row(r.id));
        self.id(r.id);
        self.bytes += 64;
    }
    pub fn metadata(&mut self, m: &Metadata) {
        assert!((m.txid as usize) < self.meta.len());
        assert_eq!(m.source as usize, m.txid as usize % self.cfg.producers);
        assert!(m.flags < 2);
        let b = 1 << m.flags;
        assert_eq!(self.meta[m.txid as usize] & b, 0);
        self.meta[m.txid as usize] |= b;
        self.meta_count += 1;
        self.metadata_hash = ordered_hash(ordered_hash(self.metadata_hash, m.txid), m.flags as u64);
    }
    pub fn finish(&self) {
        assert_eq!(self.data_count, self.cfg.records);
        assert_eq!(self.meta_count, self.cfg.transactions() * 2);
        assert!(self.meta.iter().all(|&m| m == 3));
    }
}

pub fn ordered_hash(state: u64, value: u64) -> u64 {
    state
        .wrapping_mul(0x9e3779b97f4a7c15)
        .wrapping_add(value.wrapping_add(1))
}
