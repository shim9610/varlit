//! Real-byte load study; driven by scripts/load_scalable.py. No library changes.
//! Every returned payload is compared in full against an independent oracle.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use varlit::{
    BatchOptions, BlockDescriptor, BlockKind, Endian, FormatSpec, IndexPolicy, IntegrityPolicy,
    ManifestPolicy, ReadLimits, RecoveryPolicy, StreamOptions, VarlitBlock, VarlitEmpty,
    VarlitStreamReader, VarlitStreamWriter,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// One generated record: what the oracle checks every read against.
#[derive(Clone, Debug)]
struct Record {
    key: u64,
    generation: u64,
    payload: Vec<u8>,
}

#[derive(Clone, Debug, VarlitBlock)]
#[varlit(id = 2, version = 1, kind = "variable")]
struct StreamRecord {
    key: u64,
    generation: u64,
    payload: Vec<u8>,
}

static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
    id: 2,
    name: "StreamRecord",
    version: 1,
    kind: BlockKind::Variable,
    fields: &[],
}];

#[derive(Clone)]
struct Config {
    phase: String,
    mode: String,
    root: PathBuf,
    total: u64,
    payload: usize,
    epoch_bytes: u64,
    batch_bytes: usize,
    readers: usize,
    crc: bool,
}

impl Config {
    fn parse() -> Result<Self> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        if !args.len().is_multiple_of(2) {
            return Err("arguments must be --name value pairs".into());
        }
        let mut options: BTreeMap<_, _> = args
            .chunks(2)
            .map(|a| (a[0].clone(), a[1].clone()))
            .collect();
        let mut get =
            |name: &str, default: &str| options.remove(name).unwrap_or_else(|| default.into());
        let config = Self {
            phase: get("--phase", "write"),
            mode: get("--mode", "stream"),
            root: PathBuf::from(get("--root", "")),
            total: get("--total-bytes", "21474836480").parse()?,
            payload: get("--record-bytes", "65536").parse()?,
            epoch_bytes: get("--epoch-bytes", "268435456").parse()?,
            batch_bytes: get("--batch-bytes", "4194304").parse()?,
            readers: get("--readers", "0").parse()?,
            crc: get("--crc", "true").parse()?,
        };
        if !options.is_empty() {
            return Err(format!("unknown options: {options:?}").into());
        }
        if config.root.as_os_str().is_empty()
            || config.payload < 16
            || config.payload > 8 * 1024 * 1024
            || config.epoch_bytes == 0
            || !config.epoch_bytes.is_multiple_of(config.payload as u64)
            || !config.total.is_multiple_of(config.epoch_bytes)
            || config.epochs() < 2
            || !config.epochs().is_multiple_of(2)
            || config.batch_bytes == 0
            || config.readers > 64
        {
            return Err(
                "invalid size/topology (need even epochs and exact record alignment)".into(),
            );
        }
        if !["raw", "stream"].contains(&config.mode.as_str()) {
            return Err("unknown mode".into());
        }
        Ok(config)
    }
    fn path(&self) -> PathBuf {
        self.root.join("data.varlit")
    }
    fn epochs(&self) -> u64 {
        self.total / self.epoch_bytes
    }
    fn per_epoch(&self) -> u64 {
        self.epoch_bytes / self.payload as u64
    }
    fn base_epochs(&self) -> u64 {
        self.epochs() / 2
    }
    fn keys(&self) -> u64 {
        self.base_epochs() * self.per_epoch()
    }
    fn key_at(&self, ordinal: u64) -> u64 {
        ordinal % self.keys()
    }
    fn spec(&self) -> FormatSpec {
        FormatSpec::new(
            b"VLOAD01",
            1,
            Endian::Little,
            0,
            IndexPolicy::BlockOffsetChain,
            if self.crc {
                IntegrityPolicy::Crc32
            } else {
                IntegrityPolicy::None
            },
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .with_block_identities(&[StreamRecord::IDENTITY])
        .with_read_limits(ReadLimits::STANDARD)
    }
    fn stream_options(&self) -> StreamOptions {
        StreamOptions::default()
    }
    fn batch(&self) -> BatchOptions {
        BatchOptions {
            max_bytes: self.batch_bytes,
            max_records: 16_384,
        }
    }
}

fn random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut x = *state;
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

struct Payloads {
    templates: Vec<Vec<u8>>,
}
impl Payloads {
    fn new(length: usize) -> Self {
        let mut state = 17;
        let templates = (0..64)
            .map(|_| {
                let mut bytes = vec![0u8; length];
                for chunk in bytes.chunks_mut(8) {
                    let value = random(&mut state).to_le_bytes();
                    chunk.copy_from_slice(&value[..chunk.len()]);
                }
                bytes
            })
            .collect();
        Self { templates }
    }
    fn template(&self, key: u64, generation: u64) -> &[u8] {
        &self.templates[((key.wrapping_mul(17) ^ generation) % 64) as usize]
    }
    fn record(&self, key: u64, generation: u64) -> Record {
        let mut payload = self.template(key, generation).to_vec();
        payload[..8].copy_from_slice(&key.to_le_bytes());
        payload[8..16].copy_from_slice(&generation.to_le_bytes());
        Record {
            key,
            generation,
            payload,
        }
    }
    fn check(&self, key: u64, generation: u64, payload: &[u8]) -> Result<()> {
        let template = self.template(key, generation);
        if payload.len() != template.len()
            || payload[..8] != key.to_le_bytes()
            || payload[8..16] != generation.to_le_bytes()
            || payload[16..] != template[16..]
        {
            return Err(format!("payload mismatch: key={key} generation={generation}").into());
        }
        Ok(())
    }
}

// Bounded logarithmic latency histogram: reported quantiles are bucket upper
// bounds (<= 12.5% width), not falsely precise sampled percentiles.
struct Latencies {
    buckets: [u64; 512],
    count: u64,
    total: u128,
    max: u64,
}
impl Default for Latencies {
    fn default() -> Self {
        Self {
            buckets: [0; 512],
            count: 0,
            total: 0,
            max: 0,
        }
    }
}
impl Latencies {
    fn observe(&mut self, elapsed: Duration) {
        let ns = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
        let power = 63 - ns.max(1).leading_zeros() as usize;
        let sub = ((ns.max(1) - (1u64 << power)) >> power.saturating_sub(3)).min(7) as usize;
        self.buckets[power * 8 + sub] += 1;
        self.count += 1;
        self.total += u128::from(ns);
        self.max = self.max.max(ns);
    }
    fn percentile(&self, percent: u64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = (self.count * percent).div_ceil(100);
        let mut seen = 0;
        for (index, count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= target {
                let power = index / 8;
                return (1u64 << power)
                    .saturating_add(((index % 8 + 1) as u64) << power.saturating_sub(3));
            }
        }
        self.max
    }
    fn json(&self) -> String {
        format!(
            "{{\"count\":{},\"total_ns\":{},\"p50_upper_ns\":{},\"p95_upper_ns\":{},\"p99_upper_ns\":{},\"max_ns\":{},\"histogram_buckets\":{:?}}}",
            self.count,
            self.total,
            self.percentile(50),
            self.percentile(95),
            self.percentile(99),
            self.max,
            self.buckets
        )
    }
}

fn stream_worker(
    id: usize,
    reader: VarlitStreamReader,
    stop: &AtomicBool,
    config: &Config,
    bank: &Payloads,
) -> Result<()> {
    let mut times = Latencies::default();
    let mut scans = 0;
    while !stop.load(Ordering::Acquire) {
        let mut ordinal = 0;
        let mut blocks = reader.blocks::<StreamRecord>()?;
        let mut record = StreamRecord::empty();
        while blocks.next_into(&mut record)? {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let now = Instant::now();
            if record.key != u64::MAX {
                if record.key != config.key_at(ordinal)
                    || record.generation != ordinal / config.per_epoch()
                {
                    return Err("stream snapshot order mismatch".into());
                }
                bank.check(record.key, record.generation, &record.payload)?;
                ordinal += 1;
                times.observe(now.elapsed());
            }
        }
        scans += 1;
    }
    println!(
        "{{\"event\":\"stream_reader\",\"id\":{id},\"scans_started\":{scans},\"validation_latency\":{}}}",
        times.json()
    );
    Ok(())
}

struct StopOnDrop<'a>(&'a AtomicBool);
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

enum Writer {
    Raw(File),
    Stream(Box<VarlitStreamWriter>),
}
impl Writer {
    fn new(config: &Config) -> Result<Self> {
        if config.path().exists() {
            return Err("refusing to overwrite an existing load file".into());
        }
        Ok(match config.mode.as_str() {
            "raw" => Self::Raw(
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(config.path())?,
            ),
            _ => Self::Stream(Box::new(VarlitStreamWriter::create(
                config.spec(),
                config.path(),
                config.stream_options(),
            )?)),
        })
    }
    fn append(&mut self, config: &Config, bank: &Payloads, epoch: u64) -> Result<u64> {
        let records = (0..config.per_epoch()).map(|n| {
            bank.record(
                (epoch % config.base_epochs()) * config.per_epoch() + n,
                epoch,
            )
        });
        Ok(match self {
            Self::Raw(file) => {
                let mut buffer = Vec::with_capacity(config.batch_bytes.max(config.payload));
                let mut writes = 0;
                for record in records {
                    buffer.extend_from_slice(&record.payload);
                    if buffer.len() >= config.batch_bytes {
                        file.write_all(&buffer)?;
                        buffer.clear();
                        writes += 1;
                    }
                }
                if !buffer.is_empty() {
                    file.write_all(&buffer)?;
                    writes += 1;
                }
                writes
            }
            Self::Stream(writer) => {
                writer
                    .push_iter::<StreamRecord, _>(
                        records.map(|r| StreamRecord {
                            key: r.key,
                            generation: r.generation,
                            payload: r.payload,
                        }),
                        config.batch(),
                    )
                    .map_err(|e| e.source)?
                    .write_calls
            }
        })
    }
    fn finish_epoch(&mut self, epoch: u64) -> Result<()> {
        if let Self::Stream(writer) = self {
            writer.push_info(&StreamRecord {
                key: u64::MAX,
                generation: epoch,
                payload: Vec::new(),
            })?;
        }
        Ok(())
    }
    fn sync(&mut self) -> Result<()> {
        match self {
            Self::Raw(file) => file.sync_all()?,
            Self::Stream(w) => w.sync()?,
        }
        Ok(())
    }
}

fn write(config: &Config, bank: &Payloads) -> Result<()> {
    let mut writer = Writer::new(config)?;
    let stop = AtomicBool::new(false);
    let started = Instant::now();
    std::thread::scope(|scope| -> Result<()> {
        let _stop_on_error = StopOnDrop(&stop);
        let mut workers = Vec::new();
        let mut append_latency = Latencies::default();
        let mut sync_latency = Latencies::default();
        for epoch in 0..config.epochs() {
            let append = Instant::now();
            let calls = writer.append(config, bank, epoch)?;
            let append_time = append.elapsed();
            append_latency.observe(append_time);
            let mutation = Instant::now();
            writer.finish_epoch(epoch)?;
            let mutation_time = mutation.elapsed();
            let sync = Instant::now();
            writer.sync()?;
            let sync_time = sync.elapsed();
            sync_latency.observe(sync_time);
            let handoff = Instant::now();
            // Stream readers start once the base epochs are published and scan
            // their snapshot over and over while the writer keeps appending.
            if epoch + 1 == config.base_epochs() && config.readers > 0 && config.mode == "stream" {
                for id in 0..config.readers {
                    let reader = VarlitStreamReader::open(
                        config.spec(),
                        config.path(),
                        config.stream_options(),
                    )?;
                    let stop = &stop;
                    workers
                        .push(scope.spawn(move || stream_worker(id, reader, stop, config, bank)));
                }
            }
            let native = fs::metadata(config.path())?.len();
            println!(
                "{{\"event\":\"epoch\",\"epoch\":{epoch},\"elapsed_seconds\":{},\"payload_bytes\":{},\"native_bytes\":{native},\"append_ns\":{},\"mutation_ns\":{},\"sync_ns\":{},\"handoff_ns\":{},\"write_calls\":{calls}}}",
                started.elapsed().as_secs_f64(),
                config.epoch_bytes,
                append_time.as_nanos(),
                mutation_time.as_nanos(),
                sync_time.as_nanos(),
                handoff.elapsed().as_nanos(),
            );
        }
        stop.store(true, Ordering::Release);
        for worker in workers {
            worker.join().map_err(|_| "reader thread panicked")??;
        }
        println!(
            "{{\"event\":\"write_complete\",\"payload_bytes\":{},\"seconds\":{},\"append_latency\":{},\"sync_latency\":{}}}",
            config.total,
            started.elapsed().as_secs_f64(),
            append_latency.json(),
            sync_latency.json()
        );
        Ok(())
    })
}

fn verify(config: &Config, bank: &Payloads) -> Result<()> {
    let started = Instant::now();
    let mut records = 0;
    let mut markers = 0;
    let mut check = |record: Record| -> Result<()> {
        if record.key == u64::MAX {
            if records != (markers + 1) * config.per_epoch()
                || record.generation != markers
                || !record.payload.is_empty()
            {
                return Err(
                    format!("invalid publication marker {markers} at record {records}").into(),
                );
            }
            markers += 1;
        } else {
            if record.key != config.key_at(records)
                || record.generation != records / config.per_epoch()
            {
                return Err(format!("out-of-order physical record {records}").into());
            }
            bank.check(record.key, record.generation, &record.payload)?;
            records += 1;
        }
        Ok(())
    };
    match config.mode.as_str() {
        "raw" => {
            let mut file = File::open(config.path())?;
            let mut buffer = vec![0; config.payload];
            for ordinal in 0..config.total / config.payload as u64 {
                file.read_exact(&mut buffer)?;
                bank.check(
                    config.key_at(ordinal),
                    ordinal / config.per_epoch(),
                    &buffer,
                )?;
                records += 1;
            }
            if file.read(&mut [0])? != 0 {
                return Err("unexpected raw tail".into());
            }
        }
        _ => {
            let reader =
                VarlitStreamReader::open(config.spec(), config.path(), config.stream_options())?;
            let mut blocks = reader.blocks::<StreamRecord>()?;
            let mut r = StreamRecord::empty();
            while blocks.next_into(&mut r)? {
                check(Record {
                    key: r.key,
                    generation: r.generation,
                    payload: r.payload.clone(),
                })?;
            }
        }
    }
    if records != config.total / config.payload as u64
        || (config.mode != "raw" && markers != config.epochs())
    {
        return Err(format!("scan count mismatch records={records} markers={markers}").into());
    }
    println!(
        "{{\"event\":\"verify_complete\",\"records\":{records},\"markers\":{markers},\"payload_bytes\":{},\"seconds\":{}}}",
        config.total,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("LOAD_FAILURE: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let config = Config::parse()?;
    let bank = Payloads::new(config.payload);
    match config.phase.as_str() {
        "write" => write(&config, &bank),
        "verify" => verify(&config, &bank),
        _ => Err("unknown phase".into()),
    }
}
