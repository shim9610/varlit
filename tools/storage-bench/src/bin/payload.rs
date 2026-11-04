//! Captured-metadata payload API timing, with independent full-byte validation.
use std::{path::Path, time::Instant};
use varlit::*;
use varlit_storage_bench::{Result, file_usage, model::selected_read_spec, thread_cpu};

#[derive(VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable")]
struct Blob {
    ordinal: u64,
    bytes: Vec<u8>,
}
static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
    id: 1,
    name: "Blob",
    version: 1,
    kind: BlockKind::Variable,
    fields: Blob::FIELDS,
}];
fn spec() -> FormatSpec {
    FormatSpec::new(
        b"RDAPIB01",
        1,
        Endian::Little,
        0,
        IndexPolicy::BlockOffsetChain,
        IntegrityPolicy::Crc32WithHeader,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_block_identities(&[Blob::IDENTITY])
}
fn hash(bytes: &[u8]) -> u64 {
    let mut value = 0xcbf29ce484222325u64;
    for word in bytes.chunks(8) {
        let mut padded = [0; 8];
        padded[..word.len()].copy_from_slice(word);
        value = (value ^ u64::from_le_bytes(padded)).wrapping_mul(0x100000001b3);
    }
    value
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let root = Path::new(&args[1]);
    let total: usize = args[2].parse()?;
    let group: usize = args[3].parse()?;
    let passes: usize = args[4].parse()?;
    assert!(total > 0 && group >= 8 && total.is_multiple_of(group) && passes > 0);
    let all = Instant::now();
    let path = root.join("payload.varlit");
    let t = Instant::now();
    let mut expected = Vec::new();
    let mut blob = Blob {
        ordinal: 0,
        bytes: (0..group).map(|i| i as u8).collect(),
    };
    {
        let mut writer = spec().create_writer(&path)?;
        for i in 0..total / group {
            blob.ordinal = i as u64;
            blob.bytes[..8].copy_from_slice(&(i as u64).to_le_bytes());
            expected.push(hash(&encode_to_vec(&blob, Endian::Little)?));
            writer.push_info(&blob)?;
        }
        writer.sync()?;
    }
    let write = t.elapsed().as_secs_f64();
    drop(blob);
    let t = Instant::now();
    let reader = selected_read_spec(spec())?.open_readonly(&path)?;
    let mut entries = Vec::new();
    reader.block_entries_into::<Blob>(&mut entries)?;
    assert_eq!(entries.len(), expected.len());
    let mut output = Vec::new();
    output.try_reserve_exact(
        entries
            .iter()
            .map(|e| e.payload_len as usize)
            .max()
            .unwrap(),
    )?;
    let open = t.elapsed().as_secs_f64();
    let (mut api_wall, mut api_cpu, mut oracle, mut encoded) = (0.0, 0.0, 0.0, 0u64);
    for _ in 0..passes {
        for (entry, expected) in entries.iter().zip(&expected) {
            let cpu = thread_cpu();
            let t = Instant::now();
            reader.read_payload_into(entry, &mut output)?;
            api_wall += t.elapsed().as_secs_f64();
            api_cpu += thread_cpu() - cpu;
            let t = Instant::now();
            assert_eq!(hash(&output), *expected);
            oracle += t.elapsed().as_secs_f64();
            encoded += output.len() as u64;
        }
    }
    let (logical, allocated) = file_usage(root)?;
    println!(
        "{}",
        serde_json::json!({"verified":true,"payload_bytes":total,"block_bytes":group,
        "records":entries.len(),"passes":passes,"encoded_bytes_read":encoded,
        "write_seconds":write,"open_seconds":open,"api_wall_seconds":api_wall,
        "api_cpu_seconds":api_cpu,"oracle_seconds":oracle,"seconds":all.elapsed().as_secs_f64(),
        "file_bytes":logical,"allocated_bytes":allocated})
    );
    Ok(())
}
