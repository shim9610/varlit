//! Writer never waits for reader acknowledgements during ingestion. All readers
//! consume identical committed records and perform a fixed number of follows.
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
    magic: b"VSTREAMB"; version: 1; endian: little; schema_hash: computed;
    integrity: crc32; index: block_offset_chain;
    commit: transaction_marker(explicit); blocks: [Blob];
} }
struct Expected {
    offset: u64,
    sequence: usize,
}
#[derive(Default)]
struct Reply {
    id: usize,
    bytes: u64,
    records: u64,
    trace: u64,
    follows: u64,
    frames: u64,
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
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let total: usize = args.get(1).ok_or("bytes")?.parse()?;
    let payload: usize = args.get(2).ok_or("payload")?.parse()?;
    let epoch: usize = args.get(3).ok_or("epoch bytes")?.parse()?;
    let readers: usize = args.get(4).ok_or("readers")?.parse()?;
    let polls: usize = args.get(5).ok_or("follows per epoch")?.parse()?;
    let rate: f64 = args.get(6).ok_or("offered MiB/s; 0 unlimited")?.parse()?;
    let residency = args.get(7).ok_or("residency")?;
    if payload == 0
        || epoch == 0
        || readers == 0
        || polls == 0
        || !total.is_multiple_of(epoch)
        || !epoch.is_multiple_of(payload)
        || !rate.is_finite()
        || rate < 0.0
    {
        return Err("invalid work shape".into());
    }
    let spec = if residency == "nonresident" {
        Format::spec().with_block_residency(&[BlockResidencyDescriptor {
            block_id: 1,
            resident: false,
        }])
    } else if residency == "resident" {
        Format::spec()
    } else {
        return Err("residency".into());
    };
    let setup = Instant::now();
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("stream.varlit");
    let mut writer = spec.create(&path)?;
    writer.commit()?;
    let pin = VarlitFile::open_readonly(spec, &path)?;
    let (answers, receive) = mpsc::channel::<Reply>();
    let mut sends = Vec::new();
    let mut blob = Blob {
        bytes: vec![0; payload],
    };
    let mut ingestion = 0.0;
    let mut initial_seconds = 0.0;
    let mut seconds = 0.0;
    let mut max_lag = 0.0f64;
    let mut max_push_seconds = 0.0f64;
    let mut max_commit_seconds = 0.0f64;
    let mut replies = Vec::new();
    std::thread::scope(|scope| -> Result<()> {
        for id in 0..readers {
            let path = &path;
            let answers = answers.clone();
            let (send, chunks) = mpsc::channel::<Arc<Vec<Expected>>>();
            sends.push(send);
            scope.spawn(move || {
                let mut reader = VarlitFile::open_readonly(spec, path).expect("reader open");
                answers
                    .send(Reply {
                        id,
                        ..Reply::default()
                    })
                    .unwrap();
                let mut reply = Reply {
                    id,
                    ..Reply::default()
                };
                let mut scratch = Vec::new();
                let mut data = Blob::empty();
                let before = frames();
                for chunk in chunks {
                    // The writer has committed this chunk already. follow may
                    // also adopt later commits while ingestion keeps running.
                    for _ in 0..polls {
                        reader.follow().expect("follow");
                        reply.follows += 1;
                    }
                    for item in chunk.iter() {
                        let entry = reader
                            .record_entry_at(item.offset)
                            .expect("published record");
                        reader
                            .decode_block_into(&entry, &mut scratch, &mut data)
                            .expect("decode");
                        assert_eq!(data.bytes.len(), payload);
                        assert!(data.bytes.iter().all(|&b| b == (item.sequence % 251) as u8));
                        reply.bytes += data.bytes.len() as u64;
                        reply.records += 1;
                        reply.trace = reply
                            .trace
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(item.offset ^ item.sequence as u64);
                    }
                }
                reply.frames = frames() - before;
                answers.send(reply).unwrap();
            });
        }
        for _ in 0..readers {
            receive.recv_timeout(Duration::from_secs(120)).unwrap();
        }
        initial_seconds = setup.elapsed().as_secs_f64();
        let start = Instant::now();
        for phase in 0..total / epoch {
            let mut chunk = Vec::with_capacity(epoch / payload);
            for n in 0..epoch / payload {
                let sequence = phase * (epoch / payload) + n;
                blob.bytes.fill((sequence % 251) as u8);
                let push_start = Instant::now();
                let info = writer.push_info(&blob)?;
                max_push_seconds = max_push_seconds.max(push_start.elapsed().as_secs_f64());
                chunk.push(Expected {
                    offset: info.record_offset,
                    sequence,
                });
            }
            let commit_start = Instant::now();
            writer.commit()?;
            max_commit_seconds = max_commit_seconds.max(commit_start.elapsed().as_secs_f64());
            assert!(
                pin.record_entry_at(chunk[0].offset).is_err(),
                "pinned snapshot drift"
            );
            let chunk = Arc::new(chunk);
            for send in &sends {
                send.send(Arc::clone(&chunk)).unwrap();
            }
            if rate > 0.0 {
                let target =
                    Duration::from_secs_f64(((phase + 1) * epoch) as f64 / (rate * 1048576.0));
                let elapsed = start.elapsed();
                max_lag = max_lag.max(elapsed.as_secs_f64() - target.as_secs_f64());
                if target > elapsed {
                    std::thread::sleep(target - elapsed);
                }
            }
        }
        ingestion = start.elapsed().as_secs_f64();
        sends.clear();
        for _ in 0..readers {
            replies.push(receive.recv_timeout(Duration::from_secs(120)).unwrap());
        }
        seconds = start.elapsed().as_secs_f64();
        Ok(())
    })?;
    replies.sort_by_key(|r| r.id);
    let trace = replies[0].trace;
    for (id, reply) in replies.iter().enumerate() {
        assert_eq!(reply.id, id);
        assert_eq!(reply.trace, trace);
        assert_eq!(reply.bytes, total as u64);
        assert_eq!(reply.records, (total / payload) as u64);
        assert_eq!(reply.follows, (total / epoch * polls) as u64);
    }
    let frames: u64 = replies.iter().map(|r| r.frames).sum();
    let source_bytes = std::fs::metadata(path)?.len();
    let ingest_mib_s = total as f64 / 1048576.0 / ingestion;
    println!(
        "{{\"bytes\":{total},\"payload\":{payload},\"epoch_bytes\":{epoch},\"readers\":{readers},\"follows\":{},\"records\":{},\"read_bytes\":{},\"trace\":{trace},\"source_bytes\":{source_bytes},\"residency\":\"{residency}\",\"seconds\":{seconds},\"writer_seconds\":{ingestion},\"initial_seconds\":{initial_seconds},\"offered_mib_s\":{rate},\"ingest_mib_s\":{ingest_mib_s},\"max_lag_seconds\":{max_lag},\"max_push_seconds\":{max_push_seconds},\"max_commit_seconds\":{max_commit_seconds},\"frames\":{frames}}}",
        total / epoch * polls * readers,
        total / payload * readers,
        total * readers
    );
    Ok(())
}
