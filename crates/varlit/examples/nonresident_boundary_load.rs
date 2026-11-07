//! Nonresident writer/open boundary discovery and warmed random membership.
//! Common benchmark copied unchanged to each compared runtime. The optional
//! trailing argument `warm` establishes the first boundary before timing.
use std::time::Instant;
use varlit::*;
#[derive(VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "fixed")]
struct Item {
    value: u64,
}
#[derive(VarlitBlock)]
#[varlit(id = 2, version = 1, kind = "variable")]
struct Bytes {
    #[varlit(field_id = 1)]
    bytes: Vec<u8>,
}
varlit_format! { struct Normal {
    magic: b"VMUTLOAD"; version: 1; endian: little; schema_hash: computed;
    integrity: crc32; index: block_offset_chain;
    commit: transaction_marker(on_flush); liveness: footer_flags;
    blocks: [Item, Bytes];
} }
varlit_format! { struct Recovery {
    magic: b"VRECLOAD"; version: 1; endian: little; schema_hash: computed;
    integrity: crc32; index: block_offset_chain;
    commit: transaction_marker(explicit); liveness: footer_flags;
    recovery: mark_tail; blocks: [Item, Bytes];
} }
// Diagnostic only: sum lengths of still-open anonymous regular files on Linux.
// Kept outside the named-operation timer; both binaries run the same observer.
fn temporary_bytes() -> u64 {
    std::fs::read_dir("/proc/self/fd")
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let target = std::fs::read_link(&path).ok()?;
            if !target.to_string_lossy().ends_with(" (deleted)") {
                return None;
            }
            let meta = std::fs::metadata(path).ok()?;
            meta.is_file().then_some(meta.len())
        })
        .sum()
}
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).ok_or("mode")?.as_str();
    let layout = args.get(2).ok_or("layout")?.as_str();
    let order = args.get(3).ok_or("order")?.as_str();
    let count: usize = args.get(4).ok_or("count")?.parse()?;
    let marks: usize = args.get(5).ok_or("marks")?.parse()?;
    if count == 0 || marks == 0 || marks > count {
        return Err("require 0 < marks <= count".into());
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("data");
    let mut spec = if mode == "recovery" {
        Recovery::spec()
    } else {
        Normal::spec().with_index_policy(
            IndexPolicy::BlockOffsetChain
                .with_open_digest_on_flush(mode != "nonresident-open")
                .with_segment_on_flush(mode == "segment"),
        )
    };
    if mode.starts_with("nonresident") {
        spec = spec.with_block_residency(&[
            BlockResidencyDescriptor {
                block_id: 1,
                resident: false,
            },
            BlockResidencyDescriptor {
                block_id: 2,
                resident: false,
            },
        ]);
    }
    let mut writer = spec.create(&path)?;
    if mode == "recovery" {
        writer.push(&Item { value: u64::MAX })?;
        writer.commit()?;
    }
    let mut offsets = Vec::with_capacity(marks);
    for n in 0..count {
        let at = match layout {
            "fixed" => writer.push_info(&Item { value: n as u64 })?.record_offset,
            "variable" | "large" | "mixed" => {
                let len = if layout == "large" {
                    65536
                } else if layout == "mixed" {
                    n * 8191 % 65536
                } else {
                    n * 37 % 271
                };
                writer
                    .push_info(&Bytes {
                        bytes: vec![(n % 251) as u8; len],
                    })?
                    .record_offset
            }
            _ => return Err("layout".into()),
        };
        if mode == "epochs" && (n + 1).is_multiple_of(1024) {
            writer.flush()?;
        }
        if n == ((offsets.len() + 1) * count / marks) - 1 {
            offsets.push(at);
        }
    }
    writer.flush()?;
    if mode == "rewrite" {
        // No dead/replaced records: this fixture preserves user offsets.
        writer.defragment()?;
    }
    let source_bytes = std::fs::metadata(&path)?.len();
    if mode == "recovery" {
        drop(writer);
        // Simulate only the persisted dirty-header state, with no live writer.
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut raw = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;
        let mut header = [0; 4096];
        let len = raw.read(&mut header)?;
        let at = header[..len]
            .windows(4)
            .position(|b| b == b"VLIV")
            .ok_or("liveness header")?
            + 8;
        header[at + 2..at + 4].copy_from_slice(&1u16.to_le_bytes());
        let crc = header[at..at + 4].iter().fold(2_166_136_261u32, |v, b| {
            (v ^ u32::from(*b)).wrapping_mul(16_777_619)
        });
        header[at + 4..at + 8].copy_from_slice(&crc.to_le_bytes());
        raw.seek(SeekFrom::Start(at as u64))?;
        raw.write_all(&header[at..at + 8])?;
        drop(raw);
        VarlitFile::take_record_entry_faults();
        let start = Instant::now();
        writer = spec.open(&path)?;
        let seconds = start.elapsed().as_secs_f64();
        let frames = VarlitFile::take_record_entry_faults();
        let temporary_bytes = temporary_bytes();
        for &at in &offsets {
            assert!(writer.record_is_dead(at)?);
        }
        println!(
            "{{\"mode\":\"{mode}\",\"layout\":\"{layout}\",\"order\":\"{order}\",\"count\":{count},\"marks\":{marks},\"source_bytes\":{source_bytes},\"seconds\":{seconds},\"frames\":{frames},\"temporary_bytes\":{temporary_bytes}}}"
        );
        return Ok(());
    }
    if mode != "writer" && mode != "nonresident" && mode != "epochs" && mode != "rewrite" {
        drop(writer);
        writer = if mode == "lazy" {
            VarlitFile::open_lazy(spec, &path)?
        } else {
            spec.open(&path)?
        };
    }
    match order {
        "forward" | "single" => (),
        "reverse" => offsets.reverse(),
        "random" => {
            let mut state = 0x1fbb_3171_519a_3927u64;
            for i in (1..offsets.len()).rev() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                offsets.swap(i, state as usize % (i + 1));
            }
        }
        "repeat" => {
            let tail = *offsets.last().unwrap();
            offsets.fill(tail);
        }
        _ => return Err("order".into()),
    }
    if args.get(6).is_some_and(|s| s == "warm") {
        writer.mark_record_dead(*offsets.iter().max().unwrap())?;
    }
    VarlitFile::take_record_entry_faults();
    let start = Instant::now();
    for &at in &offsets {
        writer.mark_record_dead(at)?;
    }
    let seconds = start.elapsed().as_secs_f64();
    let frames = VarlitFile::take_record_entry_faults();
    let temporary_bytes = temporary_bytes();
    for &at in &offsets {
        assert!(writer.record_is_dead(at)?);
    }
    println!(
        "{{\"mode\":\"{mode}\",\"layout\":\"{layout}\",\"order\":\"{order}\",\"count\":{count},\"marks\":{marks},\"source_bytes\":{source_bytes},\"seconds\":{seconds},\"frames\":{frames},\"temporary_bytes\":{temporary_bytes}}}"
    );
    Ok(())
}
