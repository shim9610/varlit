//! Differential coverage of owned, indexed, streaming and mapped reads.
//! Mappings only exist while the test file is immutable, including after damage.
#![cfg(all(feature = "integrity", feature = "compression-zstd", feature = "mmap"))]

use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
};
use varlit::*;
#[allow(dead_code)]
#[path = "support/validation_cases.rs"]
mod cases;
#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable", key = "id")]
struct Blob {
    #[varlit(field_id = 1)]
    id: u64,
    #[varlit(field_id = 2)]
    bytes: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 2, version = 1, kind = "fixed")]
struct Empty {
    bytes: [u8; 0],
}
varlit_format! { pub struct Format {
    magic:b"ROUTES"; version:1; endian:little; schema_hash:computed; blocks:[Blob,Empty];
} }
static PLAN: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Blob>()];
fn rejected<T: std::fmt::Debug>(label: &str, result: varlit::Result<T>, checks: &mut usize) {
    assert!(
        result.is_err(),
        "corruption accepted by {label}: {result:?}"
    );
    *checks += 1;
}
#[test]
fn read_routes_agree_on_valid_and_corrupt_payloads()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut scenarios = 0;
    let mut checks = 0;
    let mut empty_cases = 0;
    let policies: Vec<_> = cases::checksum_policies().collect();
    let modes = cases::compression();
    for &policy in &policies {
        for footers in [false, true] {
            for &mode in &modes {
                for len in [0, 1, 65536] {
                    for &verification in cases::VERIFICATION {
                        let dir = tempfile::tempdir()?;
                        let path = dir.path().join("routes.varlit");
                        let spec = Format::spec()
                            .with_integrity_policy(policy)
                            .with_index_policy(
                                IndexPolicy::ScanOnOpen.with_block_offset_chain(footers),
                            )
                            .with_resource_limits(
                                ReadLimits::UNTRUSTED.with_integrity_verification(verification),
                            )
                            .with_compression_policy(mode)
                            .with_computed_schema_hash();
                        let plan = DiskIndexPlan::canonical(spec, PLAN)?;
                        let value = Blob {
                            id: 7,
                            bytes: vec![41; len],
                        };
                        let mut writer = VarlitIndexedWriter::create(
                            spec,
                            &path,
                            DiskIndexOptions::default(),
                            plan,
                        )?;
                        writer.push_info(&value)?;
                        writer.sync()?;
                        drop(writer);
                        let reader = spec.open_readonly(&path)?;
                        let entry = reader
                            .index_entries()
                            .iter()
                            .find(|e| e.block_id == Blob::ID)
                            .unwrap()
                            .clone();
                        let indexed = VarlitIndexedReader::open(
                            spec,
                            &path,
                            DiskIndexOptions::default(),
                            plan,
                        )?;
                        assert_eq!(reader.read_block_at::<Blob>(entry.record_offset)?, value);
                        assert_eq!(indexed.get::<Blob>(&7)?, Some(value.clone()));
                        assert_eq!(indexed.blocks::<Blob>()?.next().unwrap()?, value);
                        let mut stored = Vec::new();
                        reader.read_payload_into(&entry, &mut stored)?;
                        {
                            // No writer mutates this file while either mapping exists.
                            let mapped = unsafe { reader.mmap_payloads()? };
                            std::thread::scope(|scope| {
                                for _ in 0..8 {
                                    let mapped = &mapped;
                                    let entry = &entry;
                                    let stored = &stored;
                                    scope.spawn(move || {
                                        for _ in 0..20 {
                                            assert_eq!(
                                                mapped.payload_window(entry).unwrap(),
                                                stored
                                            );
                                        }
                                    });
                                }
                            });
                            let window = unsafe { reader.mmap_payload_window(&entry)? };
                            assert_eq!(window.as_bytes(), stored);
                        }
                        // Owned readers retain their snapshots; all mappings have been dropped.
                        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
                        let at = entry.payload_offset + entry.payload_len - 1;
                        file.seek(SeekFrom::Start(at))?;
                        let mut byte = [0];
                        file.read_exact(&mut byte)?;
                        byte[0] ^= 0x80;
                        file.seek(SeekFrom::Start(at))?;
                        file.write_all(&byte)?;
                        file.sync_all()?;
                        drop(file);
                        rejected(
                            "typed positional",
                            reader.read_block_at::<Blob>(entry.record_offset),
                            &mut checks,
                        );
                        rejected(
                            "typed collection",
                            reader.blocks::<Blob>()?.get(0),
                            &mut checks,
                        );
                        rejected(
                            "stored payload",
                            reader.read_payload_into(&entry, &mut Vec::new()),
                            &mut checks,
                        );
                        rejected(
                            "logical payload",
                            reader.read_logical_payload_into(&entry, &mut Vec::new()),
                            &mut checks,
                        );
                        rejected("indexed get", indexed.get::<Blob>(&7), &mut checks);
                        rejected("indexed lookup", indexed.lookup::<Blob>(&7), &mut checks);
                        rejected(
                            "streaming decode",
                            indexed.blocks::<Blob>()?.next().unwrap(),
                            &mut checks,
                        );
                        // The damaged bytes remain immutable for all subsequent mappings.
                        let mapped = unsafe { reader.mmap_payloads()? };
                        for _ in 0..2 {
                            rejected("mapped payload", mapped.payload_window(&entry), &mut checks);
                            rejected(
                                "mapped block",
                                mapped.block_payload_window::<Blob>(0),
                                &mut checks,
                            );
                        }
                        rejected(
                            "single mapping",
                            unsafe { reader.mmap_payload_window(&entry) },
                            &mut checks,
                        );
                        scenarios += 1;
                    }
                }
            }
        }
    }
    for &policy in &policies {
        for footers in [false, true] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("empty.varlit");
            let spec = Format::spec()
                .with_integrity_policy(policy)
                .with_index_policy(IndexPolicy::ScanOnOpen.with_block_offset_chain(footers));
            let mut writer = spec.create(&path)?;
            writer.push(&Empty { bytes: [] })?;
            writer.sync()?;
            drop(writer);
            let reader = spec.open_readonly(&path)?;
            let e = reader.index_entries()[0].clone();
            assert_eq!(e.payload_len, 0);
            let map = unsafe { reader.mmap_payloads()? };
            assert_eq!(map.payload_window(&e)?, []);
            let window = unsafe { reader.mmap_payload_window(&e)? };
            assert_eq!(window.as_bytes(), []);
            empty_cases += 1;
        }
    }
    println!(
        "healthy_and_corrupt_scenarios={scenarios} verified_rejections={checks} empty_payload_cases={empty_cases} concurrent_first_read_checks={}",
        scenarios * 8 * 20
    );
    Ok(())
}
