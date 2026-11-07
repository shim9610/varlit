//! Identical fixture for paired index validation/cache comparisons.
use std::{hint::black_box, time::Instant};
use varlit::*;
varlit_format! {
    pub format Load {
        magic:b"VIXLOAD1";version:1;schema_hash:computed;index:keyed_offset_chain;integrity:crc32;
        blocks {
            fixed Slot(id=1,key=[code],key_index=disk,key_domain=[code=0..4096]) {code:u16,value:u64}
            fixed Number(id=2,key=[id],key_index=disk) {id:u64,value:u64}
            variable Text(id=3,key=[id],key_index=disk) {id:String,value:u64}
        }
    }
}
fn text_key(id: usize) -> String {
    format!("{:0256}-{id:016}", 0)
}
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).ok_or("mode required")?;
    let count: usize = args.get(2).ok_or("count required")?.parse()?;
    let operations: usize = args.get(3).ok_or("operations required")?.parse()?;
    let cache: usize = args.get(4).ok_or("cache bytes required")?.parse()?;
    let options = DiskIndexOptions {
        cache_bytes: cache,
        ..DiskIndexOptions::default()
    };
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("index.varlit");
    let mut writer = Load::create_indexed_writer(&path, options)?;
    if mode == "finite" {
        for code in (0..4096).step_by(2) {
            writer.push_slot(&Slot {
                key: SlotKey::from_code(code).unwrap(),
                value: u64::from(code),
            })?;
        }
    } else if mode == "long" || mode == "compact-long" {
        writer
            .push_texts(
                (0..count).map(|id| Text {
                    id: text_key(id * 2),
                    value: id as u64,
                }),
                BatchOptions::default(),
            )
            .map_err(|e| e.source)?;
    } else {
        writer
            .push_numbers(
                (0..count).map(|id| Number {
                    id: id as u64 * 2,
                    value: id as u64,
                }),
                BatchOptions::default(),
            )
            .map_err(|e| e.source)?;
    }
    writer.sync()?;
    let before = std::fs::metadata(disk_index_sidecar_path(&path))?.len();
    let reader = Load::open_indexed_reader(&path, options)?;
    // Prime exactly the same cache once before the timed repeated misses.
    if mode == "finite" {
        for code in (1..4096).step_by(2) {
            assert!(
                reader
                    .get_slot(&SlotKey::from_code(code).unwrap())?
                    .is_none()
            );
        }
    }
    let keys: Vec<_> = if mode == "long" {
        (0..count.min(4096))
            .map(|id| text_key(id * 2 + 1))
            .collect()
    } else {
        Vec::new()
    };
    let start = Instant::now();
    let mut missing = 0usize;
    if mode.starts_with("compact") {
        for _ in 0..operations {
            black_box(writer.compact_index()?);
        }
    } else {
        for i in 0..operations {
            let none = if mode == "finite" {
                reader
                    .get_slot(&SlotKey::from_code(((i * 137) % 2048 * 2 + 1) as u16).unwrap())?
                    .is_none()
            } else if mode == "long" {
                reader.get_text(&keys[(i * 137) % keys.len()])?.is_none()
            } else {
                reader
                    .get_number(&((i * 137 % count * 2 + 1) as u64))?
                    .is_none()
            };
            missing += usize::from(black_box(none));
        }
        assert_eq!(missing, operations);
    }
    let seconds = start.elapsed().as_secs_f64();
    let after = std::fs::metadata(disk_index_sidecar_path(&path))?.len();
    let records = if mode == "finite" { 2048 } else { count };
    // Report the input-derived oracle after checking the observed read count.
    let expected_missing = if mode.starts_with("compact") {
        0
    } else {
        operations
    };
    println!(
        "{{\"mode\":\"{mode}\",\"records\":{records},\"operations\":{operations},\"missing\":{expected_missing},\"cache\":{cache},\"before_bytes\":{before},\"after_bytes\":{after},\"seconds\":{seconds}}}"
    );
    Ok(())
}
