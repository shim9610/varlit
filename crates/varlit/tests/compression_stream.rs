#![cfg(all(feature = "compression-zstd", feature = "integrity"))]

use std::io::{Read, Seek, SeekFrom, Write};

use varlit::{
    CommitPolicy, CompressionHeaderMode, CompressionPolicy, DiskIndexOptions, DiskIndexPlan,
    DiskIndexedBlock, Error, FormatSpec, IntegrityPolicy, StreamOptions, VariableCompression,
    VarlitBlock, VarlitIndexedReader, VarlitIndexedWriter, VarlitStreamReader, VarlitStreamWriter,
    disk_index_sidecar_path, rebuild_disk_index, varlit_format,
};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable", key = "id")]
struct Item {
    #[varlit(field_id = 1)]
    id: u64,
    #[varlit(field_id = 2)]
    payload: Vec<u8>,
}

varlit_format! {
    struct Format {
        magic: b"CMPSTR";
        version: 1;
        endian: little;
        blocks: [Item];
    }
}

fn specs() -> impl Iterator<Item = FormatSpec> {
    [IntegrityPolicy::Crc32, IntegrityPolicy::Crc32WithHeader]
        .into_iter()
        .flat_map(|integrity| {
            [CommitPolicy::None, CommitPolicy::RecordFooter]
                .into_iter()
                .flat_map(move |commit| {
                    [
                        CompressionHeaderMode::FormatContract,
                        CompressionHeaderMode::RecordExplicit,
                        CompressionHeaderMode::FileExplicit,
                    ]
                    .into_iter()
                    .map(move |mode| {
                        Format::spec()
                            .with_integrity_policy(integrity)
                            .with_commit_policy(commit)
                            .with_compression_policy(CompressionPolicy::VariableBlocks(
                                VariableCompression::zstd(mode),
                            ))
                            .with_computed_schema_hash()
                    })
                })
        })
}

fn corrupt_payload(path: &std::path::Path, offset: u64) -> varlit::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut byte = [0];
    file.read_exact(&mut byte)?;
    byte[0] ^= 0x80;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&byte)?;
    file.sync_all()?;
    Ok(())
}

#[test]
fn compressed_crc_stream_preserves_format_contract_and_checks_payloads() -> varlit::Result<()> {
    for spec in specs() {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("stream.varlit");
        let value = Item {
            id: 7,
            payload: vec![41; 16 * 1024],
        };
        {
            let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
            writer.push_info(&value)?;
            writer.sync()?;
        }
        let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
        assert_eq!(
            reader
                .blocks::<Item>()?
                .collect::<varlit::Result<Vec<_>>>()?,
            [value]
        );
        assert_eq!(reader.verify_all()?, 2);
        let event = reader.events()?.last().unwrap()?;
        // Only owned reads are live; corrupt after opening so the per-record
        // check, rather than the sidecar's generation witness, sees the damage.
        corrupt_payload(&path, event.payload_offset + event.payload_len - 1)?;
        assert!(matches!(
            reader.blocks::<Item>()?.next().unwrap(),
            Err(Error::ChecksumMismatch { .. })
        ));
        assert!(matches!(
            reader.verify_all(),
            Err(Error::ChecksumMismatch { .. })
        ));
        assert!(matches!(
            reader.events()?.last().unwrap(),
            Err(Error::ChecksumMismatch { .. })
        ));
    }
    Ok(())
}

#[test]
fn compressed_crc_rebuild_preserves_format_contract_and_checks_payloads() -> varlit::Result<()> {
    const BLOCKS: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Item>()];
    for spec in specs() {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("indexed.varlit");
        let options = DiskIndexOptions::default();
        let plan = DiskIndexPlan::canonical(spec, BLOCKS).unwrap();
        let value = Item {
            id: 7,
            payload: vec![41; 16 * 1024],
        };
        let payload_offset;
        {
            let mut writer = VarlitIndexedWriter::create(spec, &path, options, plan)?;
            let info = writer.push_info(&value)?;
            payload_offset = info.payload_offset + info.payload_len - 1;
            writer.push_info(&Item {
                id: 8,
                payload: vec![42; 16 * 1024],
            })?;
            writer.delete_info::<Item>(&8)?;
            writer.sync()?;
        }
        std::fs::remove_file(disk_index_sidecar_path(&path))?;
        assert_eq!(rebuild_disk_index(spec, &path, options, plan)?.records, 4);
        {
            let reader = VarlitIndexedReader::open(spec, &path, options, plan)?;
            assert_eq!(reader.get::<Item>(&7)?, Some(value));
            assert_eq!(reader.get::<Item>(&8)?, None);
        }
        std::fs::remove_file(disk_index_sidecar_path(&path))?;
        corrupt_payload(&path, payload_offset)?;
        let error = rebuild_disk_index(spec, &path, options, plan).unwrap_err();
        assert!(matches!(error, Error::ChecksumMismatch { .. }), "{error:?}");
        assert!(!disk_index_sidecar_path(&path).exists());
    }
    Ok(())
}
