#[path = "support/pages.rs"]
mod pages;

use varlit::{
    BatchOptions, BlockDescriptor, BlockKind, CommitPolicy, Decoder, Encoder, Endian, FormatSpec,
    IndexPolicy, IntegrityPolicy, ManifestPolicy, ReadLimits, RecoveryPolicy, Result,
    StreamOptions, StreamingBlocks, VarlitBlock, VarlitDecode, VarlitEmpty, VarlitEncode,
    VarlitStreamReader, VarlitStreamWriter, WireType,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Value(u64);

impl VarlitEncode for Value {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_varlit(encoder)
    }
}

impl VarlitDecode for Value {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_varlit(decoder)?))
    }
}

impl VarlitBlock for Value {
    const ID: u32 = 61;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 0x5045_5246_4354_0061;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extra(u64);

impl VarlitEncode for Extra {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_varlit(encoder)
    }
}

impl VarlitDecode for Extra {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_varlit(decoder)?))
    }
}

impl VarlitBlock for Extra {
    const ID: u32 = 62;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 0x5045_5246_4354_0062;
}

impl VarlitEmpty for Value {
    fn empty() -> Self {
        Self(0)
    }
}

impl VarlitEmpty for Extra {
    fn empty() -> Self {
        Self(0)
    }
}

/// Drains a streaming walk into a `Vec`; the first error ends it.
fn collect_blocks<T: VarlitBlock + VarlitEmpty + Clone>(
    mut blocks: StreamingBlocks<T>,
) -> Result<Vec<T>> {
    let mut out = T::empty();
    let mut all = Vec::new();
    while blocks.next_into(&mut out)? {
        all.push(out.clone());
    }
    Ok(all)
}

static BLOCKS: &[BlockDescriptor] = &[
    BlockDescriptor {
        id: Value::ID,
        name: "PerfContractValue",
        version: Value::VERSION,
        kind: Value::KIND,
        fields: &[],
    },
    BlockDescriptor {
        id: Extra::ID,
        name: "PerfContractExtra",
        version: Extra::VERSION,
        kind: Extra::KIND,
        fields: &[],
    },
];

fn chain_spec(integrity: IntegrityPolicy) -> FormatSpec {
    FormatSpec::new(
        b"VPERFCT",
        1,
        Endian::Little,
        0,
        IndexPolicy::BlockOffsetChain,
        integrity,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_block_identities(&[Value::IDENTITY, Extra::IDENTITY])
    .with_commit_policy(CommitPolicy::RecordFooter)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

fn last_offset_of_block(reader: &VarlitStreamReader, block_id: u32) -> Result<Option<u64>> {
    let mut last = None;
    for event in reader.events()? {
        let event = event?;
        if event.block_id == block_id {
            last = Some(event.record_offset);
        }
    }
    Ok(last)
}

#[test]
fn scalar_pushes_survive_sync_and_reopen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("scalar-batched.varlit");
    let spec = chain_spec(IntegrityPolicy::None);
    let mut previous = None;
    {
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        for value in 0..5 {
            let info = writer.push_info(&Value(value))?;
            assert_eq!(info.prev_same_block_offset, previous);
            previous = Some(info.record_offset);
        }
        writer.sync()?;
        for value in 5..7 {
            let info = writer.push_info(&Value(value))?;
            assert_eq!(info.prev_same_block_offset, previous);
            previous = Some(info.record_offset);
        }
        writer.sync()?;
    }
    {
        let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
        let values = collect_blocks(reader.blocks::<Value>()?)?;
        assert_eq!(values, (0..7).map(Value).collect::<Vec<_>>());
        assert_eq!(last_offset_of_block(&reader, Value::ID)?, previous);
    }
    {
        // The reopened writer rebuilds its block tails from the file, so the
        // tails must still name the true final record.
        let mut writer = VarlitStreamWriter::open(spec, &path, StreamOptions::default())?;
        let info = writer.push_info(&Value(7))?;
        assert_eq!(info.prev_same_block_offset, previous);
        writer.sync()?;
    }
    let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
    let values = collect_blocks(reader.blocks::<Value>()?)?;
    assert_eq!(values, (0..8).map(Value).collect::<Vec<_>>());
    Ok(())
}

#[test]
fn interleaved_scalar_and_plural_appends_stay_consistent() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("interleaved.varlit");
    let spec = chain_spec(IntegrityPolicy::None);
    {
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        writer.push_info(&Value(0))?;
        // Five records with a bound of two per chunk force the native batch
        // to split into multiple prepared chunks.
        let report = writer
            .push_iter::<Value, _>(
                (1..=5).map(Value).collect::<Vec<_>>(),
                BatchOptions {
                    max_records: 2,
                    max_bytes: usize::MAX,
                },
            )
            .map_err(|error| error.source)?;
        assert_eq!(report.records, 5);
        assert!(report.write_calls >= 3);
        writer.push_info(&Extra(100))?;
        writer.push_info(&Value(6))?;
        writer.sync()?;
    }
    let (last_value, last_extra) = {
        let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
        let values = collect_blocks(reader.blocks::<Value>()?)?;
        assert_eq!(values, (0..7).map(Value).collect::<Vec<_>>());
        let extras = collect_blocks(reader.blocks::<Extra>()?)?;
        assert_eq!(extras, [Extra(100)]);
        (
            last_offset_of_block(&reader, Value::ID)?,
            last_offset_of_block(&reader, Extra::ID)?,
        )
    };
    let mut writer = VarlitStreamWriter::open(spec, &path, StreamOptions::default())?;
    assert_eq!(
        writer.push_info(&Value(7))?.prev_same_block_offset,
        last_value
    );
    assert_eq!(
        writer.push_info(&Extra(101))?.prev_same_block_offset,
        last_extra
    );
    writer.sync()?;
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn crc_typed_scan_decodes_all_values() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("crc-scan.varlit");
    let spec = chain_spec(IntegrityPolicy::Crc32WithHeader);
    {
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        for value in 0..4 {
            writer.push_info(&Value(value))?;
        }
        writer
            .push_iter::<Value, _>(
                (4..20).map(Value).collect::<Vec<_>>(),
                BatchOptions::default(),
            )
            .map_err(|error| error.source)?;
        writer.sync()?;
    }
    let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
    // 20 user records, the internal creation-nonce record and the sync
    // marker that closed them.
    assert_eq!(reader.verify_all()?, 22);
    let values = collect_blocks(reader.blocks::<Value>()?)?;
    assert_eq!(values, (0..20).map(Value).collect::<Vec<_>>());
    Ok(())
}

/// The CRC typed scan must read each decoded payload exactly once: the frame
/// scan performs no payload checksum pre-pass, and the one payload read that
/// feeds typed decode is the read that verifies the record checksum.
#[cfg(feature = "integrity")]
#[test]
fn crc_typed_scan_skips_foreign_payloads_and_gates_decode() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("crc-single-read.varlit");
    let spec = chain_spec(IntegrityPolicy::Crc32WithHeader);
    {
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        // The corrupted record must sit past the primary generation witness
        // window ; a byte flipped inside that window is a generation
        // change refused at open, which is a different contract from the
        // per-record checksum this test pins. Assertions are unchanged.
        for value in 0..400 {
            writer.push_info(&Value(value))?;
        }
        writer.push_info(&Extra(100))?;
        for value in 400..406 {
            writer.push_info(&Value(value))?;
        }
        writer.sync()?;
    }
    let extra_payload_offset = {
        let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
        let mut offset = None;
        for event in reader.events()? {
            let event = event?;
            if event.block_id == Extra::ID {
                offset = Some(event.payload_offset);
            }
        }
        offset.expect("extra record present")
    };

    // Corrupt one payload byte of the Extra record in place. Frames stay
    // intact, so only a payload checksum computation can observe the damage.
    {
        use std::io::{Read, Seek, SeekFrom, Write};

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;
        let mut byte = [0u8; 1];
        file.seek(SeekFrom::Start(extra_payload_offset))?;
        file.read_exact(&mut byte)?;
        byte[0] ^= 0xFF;
        file.seek(SeekFrom::Start(extra_payload_offset))?;
        file.write_all(&byte)?;
        file.sync_all()?;
    }

    let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
    // A typed scan of another block must not read (or checksum) the corrupt
    // foreign payload; before the single-read contract this scan failed with
    // a checksum mismatch on the skipped record.
    let values = collect_blocks(reader.blocks::<Value>()?)?;
    assert_eq!(values, (0..406).map(Value).collect::<Vec<_>>());
    // Decoding the corrupt record itself must fail: the one payload read that
    // feeds typed decode is also the checksum verification.
    let extra_error = collect_blocks(reader.blocks::<Extra>()?)
        .expect_err("decoding the corrupted record must fail");
    assert!(matches!(
        extra_error,
        varlit::Error::ChecksumMismatch { .. }
    ));
    // Whole-file verification keeps its payload checksum pre-pass.
    let verify_error = reader
        .verify_all()
        .expect_err("verify_all must observe the corrupted payload");
    assert!(matches!(
        verify_error,
        varlit::Error::ChecksumMismatch { .. }
    ));
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
mod commit_contract {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use varlit::scalable_fault::{FAULT_ENV, TRACE_ENV, arm_from_env};

    use super::*;

    const CHILD_ENV: &str = "VARLIT_STREAM_PERF_CONTRACT_CHILD";
    const ROOT_ENV: &str = "VARLIT_STREAM_PERF_CONTRACT_ROOT";
    const CHILD_RECORDS: u64 = 10;

    fn native_path(root: &Path) -> PathBuf {
        root.join("commit-contract.varlit")
    }

    /// N scalar pushes and one sync must reach storage with exactly one
    /// barrier: the pushes issue none, and the sync issues one.
    #[test]
    fn scalar_appends_and_one_sync_issue_one_barrier() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let trace = directory.path().join("trace.tsv");
        let status = Command::new(env::current_exe().expect("locate test executable"))
            .args([
                "--ignored",
                "--exact",
                "commit_contract::commit_contract_child",
            ])
            .env(CHILD_ENV, "1")
            .env(ROOT_ENV, directory.path())
            .env(FAULT_ENV, "trace")
            .env(TRACE_ENV, &trace)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run commit contract child");
        assert!(status.success(), "commit contract child failed: {status}");

        let mut sync_events = 0u64;
        let mut append_events = 0u64;
        for line in fs::read_to_string(&trace)
            .expect("read fault trace")
            .lines()
        {
            let mut fields = line.split('\t');
            assert_eq!(fields.next(), Some("v1"), "unexpected trace line {line:?}");
            match fields.next() {
                Some("sync.native_sync") => sync_events += 1,
                Some("append.native_chunk_write") => append_events += 1,
                _ => {}
            }
        }
        // Every point is bracketed by one before-hook and one after-hook.
        assert_eq!(sync_events, 2, "expected one barrier for one sync");
        // The pushes and the marker that closes them.
        assert_eq!(append_events, 2 * (CHILD_RECORDS + 1));

        // The file must still be fully readable.
        let reader = VarlitStreamReader::open(
            chain_spec(IntegrityPolicy::None),
            native_path(directory.path()),
            StreamOptions::default(),
        )?;
        let values = collect_blocks(reader.blocks::<Value>()?)?;
        assert_eq!(values, (0..CHILD_RECORDS).map(Value).collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    #[ignore = "invoked only as the commit-contract subprocess"]
    fn commit_contract_child() {
        if env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let root = PathBuf::from(env::var_os(ROOT_ENV).expect("child root"));
        let mut writer = VarlitStreamWriter::create(
            chain_spec(IntegrityPolicy::None),
            native_path(&root),
            StreamOptions::default(),
        )
        .expect("create commit contract writer");
        let guard = arm_from_env().expect("arm scalable fault tracing");
        for value in 0..CHILD_RECORDS {
            writer
                .push_info(&Value(value))
                .expect("append scalar record");
        }
        writer.sync().expect("sync commit contract writer");
        drop(writer);
        drop(guard);
    }
}
