//! Equal-work in-process facade comparison; raw mode also builds on the baseline.
use super::*;
use std::sync::mpsc;

enum Sink {
    Raw(VarlitFile),
    #[cfg(feature = "guarded-mmap")]
    Guarded(AppendOnlyWriter),
}
enum Factory {
    Raw(PathBuf, FormatSpec),
    #[cfg(feature = "guarded-mmap")]
    Guarded(AppendOnlyReaderToken, ReadLimits),
}
enum Follower {
    Raw(VarlitFile),
    #[cfg(feature = "guarded-mmap")]
    Guarded(AppendOnlyReader),
}
enum Window {
    Raw(MmapPayloadWindow),
    #[cfg(feature = "guarded-mmap")]
    Guarded(GuardedMmapWindow),
}
enum Snapshot {
    Raw(MmapPayloads),
    #[cfg(feature = "guarded-mmap")]
    Guarded(GuardedMmapPayloads),
}
impl Sink {
    fn append<T: VarlitBlock>(&mut self, value: &T) -> R<()> {
        match self {
            Self::Raw(f) => {
                f.push_info(value)?;
            }
            #[cfg(feature = "guarded-mmap")]
            Self::Guarded(f) => {
                f.push_info(value)?;
            }
        }
        Ok(())
    }
    fn commit(&mut self) -> R<()> {
        match self {
            Self::Raw(f) => {
                f.commit_durable()?;
            }
            #[cfg(feature = "guarded-mmap")]
            Self::Guarded(f) => {
                f.commit_durable()?;
            }
        }
        Ok(())
    }
}
impl Factory {
    fn open(&self) -> R<Follower> {
        Ok(match self {
            Self::Raw(path, spec) => Follower::Raw(VarlitFile::open_readonly(*spec, path)?),
            #[cfg(feature = "guarded-mmap")]
            Self::Guarded(token, limits) => {
                Follower::Guarded(token.open_reader_with_resource_limits(*limits)?)
            }
        })
    }
}
impl Follower {
    fn follow(&mut self) -> R<()> {
        match self {
            Self::Raw(f) => {
                f.follow()?;
            }
            #[cfg(feature = "guarded-mmap")]
            Self::Guarded(f) => {
                f.follow()?;
            }
        }
        Ok(())
    }
    fn chain(&self, id: u32) -> R<BlockChain<'_>> {
        Ok(match self {
            Self::Raw(f) => f.block_chain(id)?,
            #[cfg(feature = "guarded-mmap")]
            Self::Guarded(f) => f.block_chain(id)?,
        })
    }
    fn window(&self, entry: &RecordIndexEntry) -> R<Window> {
        Ok(match self {
            // SAFETY: private writer only appends, snapshots are marker-bounded;
            // no path replacement, matrix update or captured-byte mutation occurs.
            Self::Raw(f) => Window::Raw(unsafe { f.mmap_payload_window(entry)? }),
            #[cfg(feature = "guarded-mmap")]
            Self::Guarded(f) => Window::Guarded(f.mmap_payload_window(entry)?),
        })
    }
    fn snapshot(&self) -> R<Snapshot> {
        Ok(match self {
            // SAFETY: the static phase starts only after writing ends and this
            // private backing object stays unchanged throughout all reads.
            Self::Raw(f) => Snapshot::Raw(unsafe { f.mmap_payloads()? }),
            #[cfg(feature = "guarded-mmap")]
            Self::Guarded(f) => Snapshot::Guarded(f.mmap_payloads()?),
        })
    }
}
impl Window {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Raw(m) => m.as_bytes(),
            #[cfg(feature = "guarded-mmap")]
            Self::Guarded(m) => m.as_bytes(),
        }
    }
}
impl Snapshot {
    fn chunk(&self, index: usize) -> R<&[u8]> {
        Ok(match self {
            Self::Raw(m) => m.block_payload_window::<Chunk>(index)?,
            #[cfg(feature = "guarded-mmap")]
            Self::Guarded(m) => m.block_payload_window::<Chunk>(index)?,
        }
        .ok_or("missing static chunk")?)
    }
}

fn extend(reader: &Follower, id: u32, entries: &mut Vec<RecordIndexEntry>) -> R<()> {
    let anchor = entries.last().map(|e| e.record_offset);
    let mut added = Vec::new();
    let mut found = anchor.is_none();
    for entry in reader.chain(id)? {
        let entry = entry?;
        if Some(entry.record_offset) == anchor {
            found = true;
            break;
        }
        assert!(entry.committed);
        added.push(entry);
    }
    assert!(found);
    entries.extend(added.into_iter().rev());
    Ok(())
}
#[derive(serde::Serialize)]
struct Stats {
    bytes: usize,
    frames: usize,
    metadata: usize,
    active: f64,
    cpu: f64,
    intervals: Vec<(u64, u64, usize)>,
}

fn consume(
    factory: &Factory,
    receive: mpsc::Receiver<(usize, usize)>,
    ready: mpsc::Sender<()>,
    total: usize,
    group: usize,
    complex: bool,
) -> R<Stats> {
    let mut reader = factory.open()?;
    let mut chunks = Vec::new();
    let mut metadata = Vec::new();
    let mut out = vec![0; OUTPUT];
    let mut seen = vec![false; total / EPOCH];
    let mut stats = Stats {
        bytes: 0,
        frames: 0,
        metadata: 0,
        active: 0.0,
        cpu: 0.0,
        intervals: Vec::new(),
    };
    ready.send(())?;
    for (epoch, expected) in receive {
        let begin = varlit_storage_bench::monotonic_ns();
        let t = Instant::now();
        let c = thread_cpu();
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while chunks.len() < expected
            || (complex && metadata.len() < expected / (EPOCH / group) * 2)
        {
            reader.follow()?;
            extend(&reader, Chunk::ID, &mut chunks)?;
            if complex {
                extend(&reader, KeyChunk::ID, &mut metadata)?;
            }
            if Instant::now() > deadline {
                return Err("session follow timed out".into());
            }
        }
        assert!(!seen[epoch]);
        seen[epoch] = true;
        let old_bytes = stats.bytes;
        for (i, entry) in chunks[stats.frames..expected].iter().enumerate() {
            let mapped = reader.window(entry)?;
            let (first, bytes) = borrowed(mapped.bytes())?;
            assert_eq!(
                first,
                (epoch * EPOCH + block_offset(i, group, complex)) / SAMPLE
            );
            assert_eq!(bytes.len(), block_len(i, group, complex));
            out[..bytes.len()].copy_from_slice(bytes);
            verify(first, &out[..bytes.len()]);
            stats.bytes += bytes.len();
        }
        if complex {
            for (i, entry) in metadata[stats.metadata..stats.metadata + 2]
                .iter()
                .enumerate()
            {
                let mapped = reader.window(entry)?;
                let (first, bytes) = borrowed(mapped.bytes())?;
                assert_eq!(first, epoch * 2 + i);
                assert_eq!(bytes, (epoch as u64).to_le_bytes());
            }
            stats.metadata += 2;
        }
        stats.frames = expected;
        stats.active += t.elapsed().as_secs_f64();
        stats.cpu += thread_cpu() - c;
        stats.intervals.push((
            begin,
            varlit_storage_bench::monotonic_ns(),
            stats.bytes - old_bytes,
        ));
    }
    assert_eq!(stats.bytes, total);
    assert!(seen.iter().all(|v| *v));
    Ok(stats)
}
fn block_len(i: usize, group: usize, complex: bool) -> usize {
    if complex {
        group / 2 * [1, 2, 2, 3][i % 4]
    } else {
        group
    }
}
fn block_offset(i: usize, group: usize, complex: bool) -> usize {
    if complex {
        i / 4 * group * 4 + group / 2 * [0, 1, 3, 5][i % 4]
    } else {
        i * group
    }
}

pub(super) fn run(
    mode: &str,
    path: &Path,
    total: usize,
    group: usize,
    readers: usize,
    phase: &str,
    scenario: (usize, bool),
) -> R<()> {
    let (producers, complex) = scenario;
    assert!(
        readers > 0
            && producers > 0
            && total.is_multiple_of(EPOCH)
            && EPOCH.is_multiple_of(group)
            && group <= 1 << 20
    );
    assert!(matches!(phase, "session-live" | "session-warm"));
    assert!(phase != "session-warm" || (producers == 1 && !complex));
    let all = Instant::now();
    let selected = varlit_storage_bench::model::selected_read_spec(spec(false, true))?;
    let file = spec(false, true).create(path)?;
    let (mut sink, factory) = match mode {
        "session-raw" => (Sink::Raw(file), Factory::Raw(path.into(), selected)),
        #[cfg(feature = "guarded-mmap")]
        "session-guarded" => {
            // SAFETY: only this writer touches the private backing object; the
            // benchmark never rewrites/truncates it, even after writing finishes.
            let writer = unsafe { AppendOnlyWriter::new(file)? };
            let factory = Factory::Guarded(writer.reader_token(), selected.read_limits);
            (Sink::Guarded(writer), factory)
        }
        _ => return Err("unsupported session benchmark mode".into()),
    };
    let mut writer_seconds = 0.0;
    let mut writer_cpu = 0.0;
    let mut intervals = (0, 0);
    let mut stats: Vec<Stats> = Vec::new();
    let mut warm_seconds = Vec::new();
    std::thread::scope(|scope| -> R<()> {
        let mut sends = Vec::new();
        let mut joins = Vec::new();
        let (ready, started) = mpsc::channel();
        if phase == "session-live" {
            for _ in 0..readers {
                let (send, receive) = mpsc::channel();
                sends.push(send);
                let factory = &factory;
                let ready = ready.clone();
                joins.push(
                    scope.spawn(move || consume(factory, receive, ready, total, group, complex)),
                );
            }
            drop(ready);
            for _ in 0..readers {
                started.recv_timeout(std::time::Duration::from_secs(60))?;
            }
        }
        let (producer_send, producer_receive) = mpsc::sync_channel(2);
        let start = Instant::now();
        let c = thread_cpu();
        intervals.0 = varlit_storage_bench::monotonic_ns();
        let mut workers = Vec::new();
        for producer in 0..producers {
            let send = producer_send.clone();
            workers.push(scope.spawn(move || -> R<()> {
                for epoch in (producer..total / EPOCH).step_by(producers) {
                    let mut bank = Vec::new();
                    for i in 0..EPOCH / group {
                        let first = (epoch * EPOCH + block_offset(i, group, complex)) / SAMPLE;
                        let mut data = vec![0; block_len(i, group, complex)];
                        fill(first, &mut data);
                        bank.push(Chunk {
                            first: first as u64,
                            data,
                        });
                    }
                    send.send((epoch, bank))?;
                }
                Ok(())
            }));
        }
        drop(producer_send);
        let mut written = 0;
        for (epoch, bank) in producer_receive {
            if complex {
                for i in 0..2 {
                    sink.append(&KeyChunk {
                        first: (epoch * 2 + i) as u64,
                        data: (epoch as u64).to_le_bytes().to_vec(),
                    })?;
                }
            }
            for chunk in &bank {
                sink.append(chunk)?;
                written += 1;
            }
            sink.commit()?;
            for send in &sends {
                send.send((epoch, written))?;
            }
        }
        for worker in workers {
            worker.join().expect("producer panic")?;
        }
        intervals.1 = varlit_storage_bench::monotonic_ns();
        writer_seconds = start.elapsed().as_secs_f64();
        writer_cpu = thread_cpu() - c;
        assert_eq!(written, total / group);
        drop(sends);
        for join in joins {
            stats.push(join.join().expect("reader panic")?);
        }
        Ok(())
    })?;
    drop(sink);
    if phase == "session-warm" {
        std::thread::scope(|scope| -> R<()> {
            let mut joins = Vec::new();
            for _ in 0..readers {
                let factory = &factory;
                joins.push(scope.spawn(move || -> R<(Stats, f64)> {
                    let reader = factory.open()?;
                    let mapped = reader.snapshot()?;
                    let mut out = vec![0; OUTPUT];
                    let mut stats = Stats {
                        bytes: 0,
                        frames: 0,
                        metadata: 0,
                        active: 0.0,
                        cpu: 0.0,
                        intervals: Vec::new(),
                    };
                    let mut repeated = 0.0;
                    for pass in 0..2 {
                        let t = Instant::now();
                        let c = thread_cpu();
                        for index in 0..total / group {
                            let (first, bytes) = borrowed(mapped.chunk(index)?)?;
                            assert_eq!(first, index * group / SAMPLE);
                            assert_eq!(bytes.len(), group);
                            out[..bytes.len()].copy_from_slice(bytes);
                            verify(first, &out[..bytes.len()]);
                            stats.bytes += bytes.len();
                            stats.frames += 1;
                        }
                        let elapsed = t.elapsed().as_secs_f64();
                        stats.active += elapsed;
                        stats.cpu += thread_cpu() - c;
                        if pass == 1 {
                            repeated = elapsed;
                        }
                    }
                    assert_eq!(stats.bytes, total * 2);
                    Ok((stats, repeated))
                }));
            }
            for join in joins {
                let (s, t) = join.join().expect("warm reader panic")?;
                stats.push(s);
                warm_seconds.push(t);
            }
            Ok(())
        })?;
    }
    drop(factory);
    let during: usize = stats
        .iter()
        .flat_map(|s| &s.intervals)
        .filter(|(a, b, _)| *a >= intervals.0 && *b <= intervals.1)
        .map(|(_, _, n)| n)
        .sum();
    let active = stats.iter().map(|s| s.active).fold(0.0_f64, f64::max);
    let mut file_bytes = 0;
    let mut allocated_bytes = 0;
    use std::os::unix::fs::MetadataExt;
    for entry in std::fs::read_dir(path.parent().unwrap())? {
        let m = entry?.metadata()?;
        if m.is_file() {
            file_bytes += m.len();
            allocated_bytes += m.blocks() * 512;
        }
    }
    println!(
        "{}",
        serde_json::json!({"kind":"session","mode":mode,"phase":phase,"producers":producers,"readers":readers,"complex":complex,"payload_bytes":total,"logical_samples":total/SAMPLE,"physical_frames":total/group,"publications":total/EPOCH,"metadata_records":if complex{total/EPOCH*2}else{0},"writer_seconds":writer_seconds,"write_seconds":writer_seconds,"writer_cpu_seconds":writer_cpu,"reader_active_seconds":active,"warm_seconds":warm_seconds,"reader_stats":stats,"reader_fully_during_write_bytes":during,"writer_start_ns":intervals.0,"writer_end_ns":intervals.1,"seconds":all.elapsed().as_secs_f64(),"file_bytes":file_bytes,"allocated_bytes":allocated_bytes,"verified":true})
    );
    Ok(())
}
