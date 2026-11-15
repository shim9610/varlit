//! Fixed-work native scan benchmark, including many block IDs and commit markers.
use std::{
    hint::black_box,
    io::{Seek, SeekFrom, Write},
    time::Instant,
};
use varlit::{VarlitFile, varlit_format};
varlit_format! {
    pub format Scan {
        magic: b"SCLOAD01"; version: 1; integrity: crc32;
        index: block_offset_chain; commit: transaction_marker(on_flush);
        blocks { fixed Value(id=1) { value: u64, } }
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let count: u32 = args[1].parse()?;
    let diverse: bool = args[2].parse()?;
    let repeats: u64 = args[3].parse()?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("scan");
    let spec = Scan::spec();
    let mut writer = spec.create(&path)?;
    let mut offsets = Vec::new();
    for value in 0..count {
        offsets.push(
            writer
                .push_info(&Value {
                    value: value as u64,
                })?
                .record_offset,
        );
        if diverse {
            writer.flush()?;
        }
    }
    writer.flush()?;
    drop(writer);
    if diverse {
        // Unknown extension IDs are accepted by native scans. CRC32 excludes
        // the header, allowing the same fixture without enormous Rust schemas.
        let mut file = std::fs::OpenOptions::new().write(true).open(&path)?;
        for (i, offset) in offsets.into_iter().enumerate() {
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&(1000 + i as u32).to_le_bytes())?;
        }
    }
    let start = Instant::now();
    for _ in 0..repeats {
        let reader = VarlitFile::open_readonly(spec, &path)?;
        assert!(
            reader
                .block_tail_offset(if diverse { 999 + count } else { 1 })
                .is_some()
        );
        black_box(reader);
    }
    println!(
        "{{\"records\":{count},\"diverse\":{diverse},\"repeats\":{repeats},\"seconds\":{}}}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
