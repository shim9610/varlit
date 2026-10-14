use varlit::{ReadLimits, VarlitBlock, VarlitFile, varlit_format};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 17, version = 1, kind = "fixed")]
struct Point {
    value: u64,
}
varlit_format! {
    pub struct Budgeted {
        magic: b"NPAGED";
        version: 1;
        endian: little;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks: [Point];
    }
}
fn values(file: &VarlitFile) -> varlit::Result<Vec<u64>> {
    let mut out = Vec::new();
    for event in file.scan() {
        let event = event?;
        if event.block_id == Point::ID {
            out.push(file.read_block_at::<Point>(event.record_offset)?.value);
        }
    }
    Ok(out)
}
#[test]
fn tiny_directory_budget_covers_append_dirty_open_follow_and_reopen() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("paged.varlit");
    let spec =
        Budgeted::spec().with_resource_limits(ReadLimits::STANDARD.with_max_index_bytes(160));
    let mut writer = VarlitFile::create(spec, &path)?;
    for value in 0..100 {
        writer.push(&Point { value })?;
    }
    let dirty = VarlitFile::open_readonly(spec, &path)?;
    assert!(values(&dirty)?.is_empty());
    writer.flush()?;
    let mut reader = VarlitFile::open_readonly(spec, &path)?;
    assert_eq!(values(&reader)?, (0..100).collect::<Vec<_>>());
    for value in 100..300 {
        writer.push(&Point { value })?;
    }
    assert_eq!(reader.follow()?, 0);
    writer.flush()?;
    reader.follow()?;
    assert_eq!(values(&reader)?, (0..300).collect::<Vec<_>>());
    assert!(values(&dirty)?.is_empty());
    // Explicitly requesting an entire owned result still checks its size.
    assert!(reader.index_entries_into(&mut Vec::new()).is_err());
    drop(writer);
    let mut writer = VarlitFile::open(spec, &path)?;
    writer.push(&Point { value: 300 })?;
    writer.flush()?;
    reader.follow()?;
    assert_eq!(values(&reader)?, (0..301).collect::<Vec<_>>());
    Ok(())
}
