//! Equal-work liveness changes, including the first lazy historical mutation.
use std::time::Instant;
use varlit::*;
#[derive(VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "fixed")]
struct Item {
    value: u64,
}
varlit_format! {
    struct Format {
        magic: b"MARKLOAD"; version: 1; endian: little; schema_hash: computed;
        integrity: crc32; index: block_offset_chain;
        commit: transaction_marker(on_flush); liveness: footer_flags;
        blocks: [Item];
    }
}
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).ok_or("mode required")?;
    let count: usize = args.get(2).ok_or("count required")?.parse()?;
    let marks: usize = args.get(3).ok_or("marks required")?.parse()?;
    if marks == 0 || marks > count {
        return Err("require 0 < marks <= count".into());
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("records");
    let spec = Format::spec()
        .with_index_policy(
            IndexPolicy::BlockOffsetChain
                .with_open_digest_on_flush(true)
                .with_segment_on_flush(mode == "segment"),
        )
        .with_computed_schema_hash();
    let mut writer = spec.create(&path)?;
    let mut offsets = Vec::with_capacity(marks);
    for n in 0..count {
        let offset = writer.push_info(&Item { value: n as u64 })?.record_offset;
        if n == ((offsets.len() + 1) * count / marks) - 1 {
            offsets.push(offset);
        }
    }
    writer.flush()?;
    if mode != "writer" {
        drop(writer);
        writer = if mode == "lazy" {
            let (file, source) = VarlitFile::open_lazy_with_report(spec, &path)?;
            assert_eq!(source, LazyOpenSource::Digest);
            file
        } else if mode == "reopen" || mode == "segment" {
            spec.open(&path)?
        } else {
            return Err("unknown mode".into());
        };
    }
    let start = Instant::now();
    for &offset in &offsets {
        writer.mark_record_dead(offset)?;
    }
    let seconds = start.elapsed().as_secs_f64();
    for &offset in &offsets {
        assert!(writer.record_is_dead(offset)?);
    }
    assert_eq!(offsets.len(), marks);
    println!("{{\"mode\":\"{mode}\",\"records\":{count},\"marks\":{marks},\"seconds\":{seconds}}}");
    Ok(())
}
