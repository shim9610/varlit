#![cfg(all(feature = "compression-zstd", feature = "integrity"))]
#[path = "support/pages.rs"]
mod pages;

use std::io::{Read, Seek, SeekFrom, Write};

use varlit::{
    CommitPolicy, CompressionHeaderMode, CompressionPolicy, Error, FormatSpec, IntegrityPolicy,
    StreamOptions, VariableCompression, VarlitBlock, VarlitEmpty, VarlitStreamReader,
    VarlitStreamWriter, varlit_format,
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
        let mut blocks = reader.blocks::<Item>()?;
        let mut item = Item::empty();
        let mut all = Vec::new();
        while blocks.next_into(&mut item)? {
            all.push(item.clone());
        }
        assert_eq!(all, [value]);
        // The value, the creation nonce and the sync marker.
        assert_eq!(reader.verify_all()?, 3);
        // The last user record: the sync marker after it is internal.
        let event = reader
            .events()?
            .filter(|event| event.as_ref().is_ok_and(|event| event.block_id == Item::ID))
            .last()
            .unwrap()?;
        // Only owned reads are live; corrupt after opening so the per-record
        // check, rather than the sidecar's generation witness, sees the damage.
        corrupt_payload(&path, event.payload_offset + event.payload_len - 1)?;
        let mut damaged = Item::empty();
        assert!(matches!(
            reader.blocks::<Item>()?.next_into(&mut damaged),
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
