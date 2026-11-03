//! Equal-work native logging/follow benchmark. Copy unchanged to each revision.
//! mode bytes payload epochs readers polls commit-every rate-mib residency
//! modes: pending (poll a fixed tail), concurrent (poll during append), committed.
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use varlit::*;
#[derive(VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable")]
struct Blob {
    #[varlit(field_id = 1)]
    bytes: Vec<u8>,
}
varlit_format! { struct Format {
    magic: b"VFOLLOWB"; version: 1; endian: little; schema_hash: computed;
    integrity: crc32; index: block_offset_chain;
    commit: transaction_marker(explicit); blocks: [Blob];
} }
#[derive(Clone, Copy)]
struct Expected {
    offset: u64,
    bytes: usize,
    value: u8,
}
enum Command {
    Poll { count: usize, hidden: u64 },
    Read(Arc<Vec<Expected>>),
}
#[derive(Default)]
struct Reply {
    id: usize,
    frames: u64,
    bytes: u64,
    records: u64,
    trace: u64,
    polls: u64,
    follow_seconds: f64,
}
fn frames() -> u64 {
    #[cfg(feature = "scalable-fault-injection")]
    {
        VarlitFile::records_framed()
    }
    #[cfg(not(feature = "scalable-fault-injection"))]
    {
        0
    }
}
fn temporary_bytes() -> u64 {
    std::fs::read_dir("/proc/self/fd")
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let p = e.ok()?.path();
            let target = std::fs::read_link(&p).ok()?;
            if !target.to_string_lossy().ends_with(" (deleted)") {
                return None;
            }
            let m = std::fs::metadata(p).ok()?;
            m.is_file().then_some(m.len())
        })
        .sum()
}
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).ok_or("mode")?;
    let total: usize = args.get(2).ok_or("bytes")?.parse()?;
    let payload: usize = args.get(3).ok_or("payload")?.parse()?;
    let epochs: usize = args.get(4).ok_or("epochs")?.parse()?;
    let readers: usize = args.get(5).ok_or("readers")?.parse()?;
    let polls: usize = args.get(6).ok_or("polls")?.parse()?;
    let commit_every: usize = args.get(7).ok_or("commit-every")?.parse()?;
    let rate: f64 = args.get(8).ok_or("rate-mib")?.parse()?;
    let residency = args.get(9).ok_or("residency")?;
    if !["pending", "concurrent", "committed"].contains(&mode.as_str())
        || payload == 0
        || epochs == 0
        || readers == 0
        || commit_every == 0
        || !total.is_multiple_of(payload)
        || !(total / payload).is_multiple_of(epochs)
        || !epochs.is_multiple_of(commit_every)
        || !rate.is_finite()
        || rate < 0.0
    {
        return Err("invalid work shape".into());
    }
    let mut spec = Format::spec();
    if residency == "nonresident" {
        spec = spec.with_block_residency(&[BlockResidencyDescriptor {
            block_id: 1,
            resident: false,
        }]);
    } else if residency != "resident" {
        return Err("residency".into());
    }
    let varying = args.get(10).is_some_and(|s| s == "varying");
    let shape = if varying { "varying" } else { "fixed" };
    if varying && (!(total / payload / epochs).is_multiple_of(2) || payload > usize::MAX / 2) {
        return Err("varying shape requires complete pairs".into());
    }
    let setup_start = Instant::now();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("data.varlit");
    let mut writer = spec.create(&path)?;
    writer.commit()?;
    let pinned = VarlitFile::open_readonly(spec, &path)?;
    let mut commands = Vec::new();
    let (answers, receive) = mpsc::channel::<Reply>();
    let mut blob = Blob {
        bytes: vec![0; payload],
    };
    let mut all_frames = 0;
    let mut all_bytes = 0;
    let mut all_records = 0;
    let mut all_polls = 0;
    let mut follow_seconds = 0.0;
    let mut trace = 0u64;
    let mut peak_temporary = 0;
    let mut max_lag = 0.0f64;
    let per_epoch = total / payload / epochs;
    let mut start = Instant::now();
    let mut initial_seconds = None;
    std::thread::scope(|scope| -> Result<()> {
        for id in 0..readers {
            let path = &path;
            let answers = answers.clone();
            let (send, control) = mpsc::channel();
            commands.push(send);
            scope.spawn(move || {
                let mut reader = VarlitFile::open_readonly(spec, path).expect("reader open");
                let mut scratch = Vec::new();
                answers
                    .send(Reply {
                        id,
                        ..Reply::default()
                    })
                    .unwrap();
                for command in control {
                    let before = frames();
                    let mut reply = Reply {
                        id,
                        ..Reply::default()
                    };
                    match command {
                        Command::Poll { count, hidden } => {
                            let timer = Instant::now();
                            for _ in 0..count {
                                assert_eq!(reader.follow().unwrap(), 0);
                            }
                            reply.follow_seconds = timer.elapsed().as_secs_f64();
                            assert!(
                                reader.record_entry_at(hidden).is_err(),
                                "uncommitted data became readable"
                            );
                            reply.polls = count as u64;
                        }
                        Command::Read(expected) => {
                            let timer = Instant::now();
                            assert!(reader.follow().unwrap() > 0);
                            reply.follow_seconds = timer.elapsed().as_secs_f64();
                            for e in expected.iter() {
                                let entry = reader.record_entry_at(e.offset).unwrap();
                                let value = reader
                                    .decode_block_into::<Blob>(&entry, &mut scratch)
                                    .unwrap();
                                assert_eq!(value.bytes.len(), e.bytes);
                                assert!(value.bytes.iter().all(|&b| b == e.value));
                                reply.bytes += value.bytes.len() as u64;
                                reply.records += 1;
                                reply.trace = reply
                                    .trace
                                    .wrapping_mul(6364136223846793005)
                                    .wrapping_add(e.offset ^ e.bytes as u64 ^ u64::from(e.value));
                            }
                        }
                    }
                    reply.frames = frames() - before;
                    answers.send(reply).unwrap();
                }
            });
        }
        for _ in 0..readers {
            receive.recv_timeout(Duration::from_secs(120)).unwrap();
        }
        initial_seconds = Some(setup_start.elapsed().as_secs_f64());
        start = Instant::now();
        let mut pending = Vec::new();
        for epoch in 0..epochs {
            let mut poll_sent = false;
            for n in 0..per_epoch {
                let sequence = epoch * per_epoch + n;
                let value = (sequence % 251) as u8;
                let size = if varying {
                    let pair = sequence / 2;
                    let first = (pair as u64).wrapping_mul(8191) as usize % (2 * payload - 1) + 1;
                    if sequence.is_multiple_of(2) {
                        first
                    } else {
                        2 * payload - first
                    }
                } else {
                    payload
                };
                blob.bytes.resize(size, value);
                blob.bytes.fill(value);
                let info = writer.push_info(&blob)?;
                pending.push(Expected {
                    offset: info.record_offset,
                    bytes: size,
                    value,
                });
                if mode == "concurrent" && n == per_epoch / 2 {
                    let hidden = pending[0].offset;
                    for send in &commands {
                        send.send(Command::Poll {
                            count: polls,
                            hidden,
                        })
                        .unwrap();
                    }
                    poll_sent = true;
                }
            }
            writer.flush()?;
            if mode == "pending" {
                let hidden = pending[0].offset;
                for send in &commands {
                    send.send(Command::Poll {
                        count: polls,
                        hidden,
                    })
                    .unwrap();
                }
                poll_sent = true;
            }
            if poll_sent {
                for _ in 0..readers {
                    let reply = receive.recv_timeout(Duration::from_secs(120)).unwrap();
                    all_frames += reply.frames;
                    all_polls += reply.polls;
                    follow_seconds += reply.follow_seconds;
                }
            }
            if mode == "committed" || (epoch + 1).is_multiple_of(commit_every) {
                writer.commit()?;
                assert!(
                    pinned.record_entry_at(pending[0].offset).is_err(),
                    "pinned snapshot drifted"
                );
                let expected = Arc::new(std::mem::take(&mut pending));
                for send in &commands {
                    send.send(Command::Read(Arc::clone(&expected))).unwrap();
                }
                let mut phase_trace = None;
                let mut seen = vec![false; readers];
                for _ in 0..readers {
                    let reply = receive.recv_timeout(Duration::from_secs(120)).unwrap();
                    assert!(!seen[reply.id]);
                    seen[reply.id] = true;
                    assert_eq!(reply.records, expected.len() as u64);
                    assert_eq!(*phase_trace.get_or_insert(reply.trace), reply.trace);
                    all_frames += reply.frames;
                    all_bytes += reply.bytes;
                    all_records += reply.records;
                    follow_seconds += reply.follow_seconds;
                }
                trace = trace
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(phase_trace.unwrap());
            }
            peak_temporary = peak_temporary.max(temporary_bytes());
            if rate > 0.0 {
                let target = Duration::from_secs_f64(
                    ((epoch + 1) * per_epoch * payload) as f64 / (rate * 1048576.0),
                );
                let elapsed = start.elapsed();
                max_lag = max_lag.max(elapsed.as_secs_f64() - target.as_secs_f64());
                if target > elapsed {
                    std::thread::sleep(target - elapsed);
                }
            }
        }
        assert_eq!(all_bytes, total as u64 * readers as u64);
        assert_eq!(all_records, (total / payload) as u64 * readers as u64);
        commands.clear();
        Ok(())
    })?;
    let seconds = start.elapsed().as_secs_f64();
    let initial_seconds = initial_seconds.expect("readers initialized");
    let source_bytes = std::fs::metadata(&path)?.len();
    println!(
        "{{\"mode\":\"{mode}\",\"residency\":\"{residency}\",\"shape\":\"{shape}\",\"initial_seconds\":{initial_seconds},\"bytes\":{total},\"payload\":{payload},\"epochs\":{epochs},\"readers\":{readers},\"polls\":{all_polls},\"records\":{all_records},\"read_bytes\":{all_bytes},\"trace\":{trace},\"source_bytes\":{source_bytes},\"seconds\":{seconds},\"follow_seconds\":{follow_seconds},\"frames\":{all_frames},\"temporary_bytes\":{peak_temporary},\"offered_mib_s\":{rate},\"max_lag_seconds\":{max_lag},\"ingest_mib_s\":{}}}",
        total as f64 / 1048576.0 / seconds
    );
    Ok(())
}
