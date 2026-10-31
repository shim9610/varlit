//! Equal-work rewrite/codec benchmark. Copy this exact fixture to both revisions.
use std::{collections::HashMap, hint::black_box, time::Instant};
use varlit::*;
#[derive(Clone, Debug, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable", key = "id")]
struct Item {
    id: u64,
    value: u64,
    bytes: Vec<u8>,
}
varlit_format! { struct Format {
    magic: b"VOOMLOAD"; version: 1; endian: little; schema_hash: computed;
    integrity: crc32; blocks: [Item];
} }
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).ok_or("mode required")?;
    let count: usize = args.get(2).ok_or("count required")?.parse()?;
    let size: usize = args.get(3).ok_or("payload size required")?.parse()?;
    if count == 0 {
        return Err("count must be positive".into());
    }
    if mode == "hash" {
        let map: HashMap<u64, u64> = (0..count as u64).map(|n| (n, n)).collect();
        let start = Instant::now();
        for _ in 0..20 {
            black_box(encode_to_vec(&map, Endian::Little)?);
        }
        println!(
            "{{\"mode\":\"hash\",\"records\":{count},\"work\":20,\"seconds\":{}}}",
            start.elapsed().as_secs_f64()
        );
        return Ok(());
    }
    if matches!(
        mode.as_str(),
        "chunks" | "decode" | "decode-after-encode" | "prepare-decode"
    ) {
        let bytes: Vec<_> = (0..count).map(|n| (n % 251) as u8).collect();
        // Both decoder revisions must start from the same stored bytes and
        // allocator state. Running their different constructors here would
        // give only the old implementation a freed output-sized heap arena.
        // decode-after-encode preserves that distinct composite workload.
        let encoded = if mode == "decode" {
            ChunkedBytes::from_encoded(std::fs::read(args.get(4).ok_or("encoded file required")?)?)?
        } else {
            ChunkedBytes::from_zstd_chunks(&bytes, size, CompressionLevel::Fast)?
        };
        if mode == "prepare-decode" {
            std::fs::write(
                args.get(4).ok_or("encoded file required")?,
                encoded.encoded(),
            )?;
            return Ok(());
        }
        let start = Instant::now();
        for _ in 0..20 {
            if mode == "chunks" {
                black_box(ChunkedBytes::from_zstd_chunks(
                    &bytes,
                    size,
                    CompressionLevel::Fast,
                )?);
            } else {
                let value = encoded.decode_to_vec()?;
                assert_eq!(value, bytes);
                black_box(value);
            }
        }
        println!(
            "{{\"mode\":\"{mode}\",\"records\":{count},\"payload\":{size},\"work\":20,\"seconds\":{}}}",
            start.elapsed().as_secs_f64()
        );
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("maintenance.varlit");
    let complex = mode == "complex" || mode == "checkpoint";
    let mut index = IndexPolicy::new(true, mode == "checkpoint", true, complex);
    if mode == "complex" {
        index = index
            .with_segment_on_flush(true)
            .with_open_digest_on_flush(true);
    }
    let spec = Format::spec()
        .with_index_policy(index)
        .with_commit_policy(CommitPolicy::RecordFooter)
        .with_liveness_policy(LivenessPolicy::FooterFlags)
        .with_computed_schema_hash();
    let mut item = Item {
        id: 0,
        value: 0,
        bytes: vec![7; size],
    };
    let mut dead = Vec::new();
    let first;
    {
        let mut writer = spec.create(&path)?;
        first = writer.push_keyed_info(&item)?.record_offset;
        for n in 1..count {
            item.id = n as u64 % 2048;
            item.value = n as u64;
            let info = writer.push_keyed_info(&item)?;
            if complex && n % 7 == 0 {
                dead.push(info.record_offset);
            }
            if complex && n % 10000 == 0 {
                writer.flush()?;
            }
        }
        for offset in dead {
            writer.mark_record_dead(offset)?;
        }
        writer.sync()?;
    }
    let before = std::fs::metadata(&path)?.len();
    let mut writer = spec.open(&path)?;
    let held = spec.open_readonly(&path)?;
    let start = Instant::now();
    let dropped = if mode == "replace" {
        item.id = 0;
        item.value = u64::MAX;
        item.bytes.push(8);
        writer.replace_block(0, &item)?;
        0
    } else {
        writer.defragment()?.records_dropped
    };
    let seconds = start.elapsed().as_secs_f64();
    assert_eq!(held.read_block_at::<Item>(first)?.value, 0);
    let expected = if mode == "replace" { u64::MAX } else { 0 };
    assert_eq!(writer.read_block_at::<Item>(first)?.value, expected);
    drop(writer);
    let reopened = spec.open_readonly(&path)?;
    assert_eq!(reopened.read_block_at::<Item>(first)?.value, expected);
    if complex {
        assert_eq!(dropped, (count - 1) / 7);
    }
    println!(
        "{{\"mode\":\"{mode}\",\"records\":{count},\"payload\":{size},\"file_bytes\":{before},\"dropped\":{dropped},\"seconds\":{seconds}}}"
    );
    Ok(())
}
