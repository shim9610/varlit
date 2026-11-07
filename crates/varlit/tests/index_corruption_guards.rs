#![cfg(feature = "integrity")]
use varlit::{DiskIndexOptions, varlit_format};
varlit_format! {
    pub format AuditFinite {
        magic:b"AUDITFIN";version:1;schema_hash:computed;index:keyed_offset_chain;integrity:crc32;
        blocks {
            fixed Short(id=1,key=[code],key_index=disk,key_domain=[code=0..1]) {code:u16,value:u64}
            fixed Long(id=2,key=[code],key_index=disk,key_domain=[code=0..64]) {code:u16,value:u64}
        }
    }
}
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320u32 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}
#[test]
fn aliased_finite_extents_return_errors_through_public_reads() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.varlit");
    let options = DiskIndexOptions::default();
    let mut writer = AuditFinite::create_indexed_writer(&path, options)?;
    writer.push_short(&Short {
        key: ShortKey::from_code(0).unwrap(),
        value: 10,
    })?;
    writer.push_long(&Long {
        key: LongKey::from_code(0).unwrap(),
        value: 20,
    })?;
    writer.sync()?;
    drop(writer);
    let sidecar = varlit::disk_index_sidecar_path(&path);
    let mut bytes = std::fs::read(&sidecar)?;
    let cp = [4096usize, 8192]
        .into_iter()
        .map(|at| word(&bytes, at + 8) as usize)
        .max()
        .unwrap();
    let olddir = word(&bytes, cp + 336) as usize;
    let oldchunk = word(&bytes, olddir + 40) as usize;
    let mut data = vec![0u8; 64 * 40 + 4];
    data[..44].copy_from_slice(&bytes[oldchunk..oldchunk + 44]);
    let crc = crc32(&data[..2560]);
    data[2560..].copy_from_slice(&crc.to_le_bytes());
    let data_at = bytes.len() as u64;
    bytes.extend_from_slice(&data);
    let directory_at = bytes.len() as u64;
    let mut directory = vec![0u8; 76];
    directory[..8].copy_from_slice(b"VIXSLOT1");
    directory[8..12].copy_from_slice(&76u32.to_le_bytes());
    directory[12..16].copy_from_slice(&2u32.to_le_bytes());
    directory[16..24].copy_from_slice(&2u64.to_le_bytes());
    for (i, count) in [1u32, 64].into_iter().enumerate() {
        let at = 24 + i * 24;
        directory[at..at + 4].copy_from_slice(&((i + 1) as u32).to_le_bytes());
        directory[at + 4..at + 8].copy_from_slice(&count.to_le_bytes());
        directory[at + 8..at + 12].copy_from_slice(&1u32.to_le_bytes());
        directory[at + 12..at + 16].copy_from_slice(&1u32.to_le_bytes());
        directory[at + 16..at + 24].copy_from_slice(&data_at.to_le_bytes());
    }
    let crc = crc32(&directory[..72]);
    directory[72..].copy_from_slice(&crc.to_le_bytes());
    bytes.extend_from_slice(&directory);
    let mut checkpoint = bytes[cp..cp + word(&bytes, cp + 8) as usize].to_vec();
    checkpoint[336..344].copy_from_slice(&directory_at.to_le_bytes());
    let crc = crc32(&checkpoint[..348]);
    checkpoint[348..352].copy_from_slice(&crc.to_le_bytes());
    let checkpoint_at = bytes.len() as u64;
    bytes.extend_from_slice(&checkpoint);
    let end = bytes.len() as u64;
    for at in [4096usize, 8192] {
        bytes[at + 8..at + 16].copy_from_slice(&checkpoint_at.to_le_bytes());
        bytes[at + 16..at + 24].copy_from_slice(&end.to_le_bytes());
        let crc = crc32(&bytes[at..at + 28]);
        bytes[at + 28..at + 32].copy_from_slice(&crc.to_le_bytes());
    }
    std::fs::write(&sidecar, bytes)?;
    let reader = AuditFinite::open_indexed_reader(&path, options)?;
    assert_eq!(
        reader
            .get_short(&ShortKey::from_code(0).unwrap())?
            .unwrap()
            .value,
        10
    );
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reader.get_long(&LongKey::from_code(1).unwrap())
    }));
    assert!(outcome.is_ok(), "malformed slot references must not panic");
    assert!(
        outcome.unwrap().is_err(),
        "inconsistent cached extent must be rejected"
    );
    assert_eq!(
        reader
            .get_short(&ShortKey::from_code(0).unwrap())?
            .unwrap()
            .value,
        10
    );
    // The reverse cache warm-up must reject the inconsistent shape too.
    let reverse = AuditFinite::open_indexed_reader(&path, options)?;
    assert!(reverse.get_long(&LongKey::from_code(2).unwrap())?.is_none());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reverse.get_short(&ShortKey::from_code(0).unwrap())
    }));
    assert!(result.is_ok());
    assert!(result.unwrap().is_err());
    Ok(())
}

#[test]
fn failed_follow_of_checksums_valid_metadata_preserves_real_readers()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    use varlit::{DiskIndexPlan, DiskIndexedBlock, VarlitIndexedReader, VarlitIndexedWriter};
    for field in [120usize, 128, 256] {
        // working EOF/count, primary witness length
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("follow.varlit");
        let spec = AuditFinite::spec();
        let options = DiskIndexOptions {
            cache_bytes: 1024 * 1024,
            ..DiskIndexOptions::default()
        };
        const BLOCKS: &[DiskIndexedBlock] = &[
            DiskIndexedBlock::of::<Short>(),
            DiskIndexedBlock::of::<Long>(),
        ];
        let plan = DiskIndexPlan::canonical(spec, BLOCKS)?;
        let key = ShortKey::from_code(0).unwrap();
        let mut writer = VarlitIndexedWriter::create(spec, &path, options, plan)?;
        writer.push_info(&Short { key, value: 10 })?;
        writer.sync()?;
        let mut readers = (0..8)
            .map(|_| VarlitIndexedReader::open(spec, &path, options, plan))
            .collect::<varlit::Result<Vec<_>>>()?;
        let pin = VarlitIndexedReader::open(spec, &path, options, plan)?;
        let held = pin.committed_len();
        writer.push_info(&Short { key, value: 20 })?;
        writer.sync()?;
        drop(writer);
        let sidecar = varlit::disk_index_sidecar_path(&path);
        let valid = std::fs::read(&sidecar)?;
        let mut damaged = valid.clone();
        let cp = [4096usize, 8192]
            .map(|at| word(&valid, at + 8) as usize)
            .into_iter()
            .max()
            .unwrap();
        damaged[cp + 32 + field..cp + 40 + field].copy_from_slice(&u64::MAX.to_le_bytes());
        let crc = crc32(&damaged[cp + 32..cp + 328]);
        damaged[cp + 328..cp + 332].copy_from_slice(&crc.to_le_bytes());
        let crc = crc32(&damaged[cp..cp + 348]);
        damaged[cp + 348..cp + 352].copy_from_slice(&crc.to_le_bytes());
        let statuses = readers
            .iter()
            .map(|r| r.snapshot_status())
            .collect::<varlit::Result<Vec<_>>>()?;
        let states = readers
            .iter()
            .map(|r| r.resident_state())
            .collect::<Vec<_>>();
        let cursors = readers
            .iter()
            .map(|r| r.blocks::<Short>())
            .collect::<varlit::Result<Vec<_>>>()?;
        std::fs::write(&sidecar, damaged)?;
        for (reader, state) in readers.iter_mut().zip(&states) {
            assert!(reader.follow().is_err());
            assert_eq!(reader.committed_len(), held);
            assert_eq!(reader.resident_state(), *state);
            assert_eq!(reader.get::<Short>(&key)?.unwrap().value, 10);
        }
        std::fs::write(&sidecar, valid)?;
        for (reader, status) in readers.iter_mut().zip(statuses) {
            assert_eq!(reader.snapshot_status()?, status);
            reader.follow()?;
            assert_eq!(reader.get::<Short>(&key)?.unwrap().value, 20);
        }
        for cursor in cursors {
            let items = cursor.collect::<varlit::Result<Vec<_>>>()?;
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].value, 10);
        }
        assert_eq!(pin.committed_len(), held);
        assert_eq!(pin.get::<Short>(&key)?.unwrap().value, 10);
    }
    println!("HOSTILE_FOLLOW valid_crc_fields=3 followers=8 pinned=1 retries=24");
    Ok(())
}
