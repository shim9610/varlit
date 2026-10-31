//! Failed batches must not advance readers, resurrect deletes, or poison a
//! subsequently recovered writer. A plain map owns the expected state.
#![cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]
use std::collections::BTreeMap;
use varlit::*;

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable", key = "id")]
struct Item {
    id: u64,
    bytes: Vec<u8>,
}
varlit_format! { struct Format {
    magic: b"FAULTSEQ"; version: 1; endian: little; integrity: crc32;
    index: keyed_offset_chain; blocks: [Item];
} }
fn item(id: u64, epoch: u64) -> Item {
    Item {
        id,
        bytes: vec![epoch as u8; 4096 + id as usize],
    }
}
fn check(reader: &VarlitIndexedReader, expected: &BTreeMap<u64, Item>) -> Result<()> {
    for id in 0..33 {
        assert_eq!(
            reader.get::<Item>(&id)?,
            expected.get(&id).cloned(),
            "key {id}"
        );
    }
    assert_eq!(reader.resident_state().retained_key_entries, 0);
    Ok(())
}

#[test]
fn failed_batches_recover_through_delete_reinsert_follow_and_compaction()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("history.varlit");
    let spec = Format::spec();
    let options = DiskIndexOptions {
        cache_bytes: 1024 * 1024,
        ..DiskIndexOptions::default()
    };
    const BLOCKS: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Item>()];
    let plan = DiskIndexPlan::canonical(spec, BLOCKS)?;
    let mut writer = VarlitIndexedWriter::create(spec, &path, options, plan)?;
    let mut expected = BTreeMap::new();
    for id in 0..32 {
        writer.push_info(&item(id, 50))?;
        expected.insert(id, item(id, 50));
    }
    writer.sync()?;
    for epoch in 0..24 {
        let mut pin = VarlitIndexedReader::open(spec, &path, options, plan)?;
        let old = expected.clone();
        // Accepted-but-unconfirmed edits followed by a native write failure.
        writer.push_info(&item(epoch % 8, epoch + 100))?;
        writer.delete_info::<Item>(&((epoch + 1) % 32))?;
        VarlitStreamWriter::inject_append_rollback_failures(1);
        let failed = writer.push_info(&item((epoch + 2) % 32, epoch));
        assert!(
            matches!(failed, Err(Error::WriteRollbackFailed { .. })),
            "{failed:?}"
        );
        assert!(matches!(
            writer.push_info(&item(0, 0)),
            Err(Error::WriterPoisoned(_))
        ));
        check(&pin, &old)?;
        pin.follow()?;
        check(&pin, &old)?;
        drop(writer);
        writer = VarlitIndexedWriter::restore_checkpoint_and_open(spec, &path, options, plan)?;
        check(
            &VarlitIndexedReader::open(spec, &path, options, plan)?,
            &old,
        )?;
        let id = epoch % 8;
        writer.delete_info::<Item>(&id)?;
        writer.push_info(&item(id, epoch))?;
        expected.insert(id, item(id, epoch));
        let deleted = (epoch + 17) % 32;
        writer.delete_info::<Item>(&deleted)?;
        expected.remove(&deleted);
        writer.sync()?;
        // Readers opened before publication remain pinned until follow.
        check(&pin, &old)?;
        writer.compact_index()?;
        check(&pin, &old)?;
        pin.follow()?;
        check(&pin, &expected)?;
        drop(writer);
        writer = VarlitIndexedWriter::open(spec, &path, options, plan)?;
        check(
            &VarlitIndexedReader::open(spec, &path, options, plan)?,
            &expected,
        )?;
    }
    println!("stateful failure epochs=24; independent keys checked after every boundary");
    Ok(())
}
