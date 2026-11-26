//! Stream creation identity: every create stamps a distinct nonce inside the
//! primary, so two files created with identical content still differ.

use std::fs;
use std::path::Path;

use varlit::{
    BlockDescriptor, BlockKind, Decoder, Encoder, Endian, FormatSpec, IndexPolicy, IntegrityPolicy,
    ManifestPolicy, ReadLimits, RecoveryPolicy, Result, VarlitBlock, VarlitDecode, VarlitEncode,
    VarlitKeyedBlock, VarlitStreamWriter, WireType,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    key: u64,
    value: String,
}

impl VarlitEncode for Item {
    const WIRE_TYPE: WireType = <(u64, String) as VarlitEncode>::WIRE_TYPE;

    fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
        (self.key, self.value.clone()).encode_varlit(encoder)
    }
}

impl VarlitDecode for Item {
    const WIRE_TYPE: WireType = <(u64, String) as VarlitDecode>::WIRE_TYPE;

    fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
        let (key, value) = <(u64, String)>::decode_varlit(decoder)?;
        Ok(Self { key, value })
    }
}

impl varlit::VarlitEmpty for Item {
    fn empty() -> Self {
        Self {
            key: 0,
            value: String::new(),
        }
    }
}

impl VarlitBlock for Item {
    const ID: u32 = 71;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Variable;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x4944_454E_5449_5459;
    const IS_KEYED: bool = true;
}

impl VarlitKeyedBlock for Item {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.key
    }
}

static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
    id: Item::ID,
    name: "IdentityItem",
    version: Item::VERSION,
    kind: Item::KIND,
    fields: &[],
}];

static IDENTITIES: &[(u32, Option<Endian>, bool, u64)] = &[(
    Item::ID,
    Item::ENDIAN,
    Item::IS_KEYED,
    Item::SCHEMA_FINGERPRINT,
)];

/// Stream state sidecars index no keys, so the stream probe uses a spec whose
/// index policy the streaming writer supports for this block.
fn stream_spec() -> FormatSpec {
    FormatSpec::new(
        b"VIDS",
        1,
        Endian::Little,
        0,
        IndexPolicy::BlockOffsetChain,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(ReadLimits::STANDARD)
    .with_block_identities(IDENTITIES)
}

fn item(key: u64, value: &str) -> Item {
    Item {
        key,
        value: value.into(),
    }
}

/// Creates a stream primary holding `keys`, byte-identical across calls except
/// for the per-create nonce.
fn build_primary(path: &Path, keys: [u64; 2]) -> Result<()> {
    let mut writer =
        VarlitStreamWriter::create(stream_spec(), path, varlit::StreamOptions::default())?;
    for key in keys {
        writer.push_info(&item(key, "fixed-width-value"))?;
    }
    writer.sync()?;
    Ok(())
}

#[test]
fn every_create_stamps_a_distinct_nonce_inside_the_primary() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("nonce-first.varlit");
    let second = directory.path().join("nonce-second.varlit");

    build_primary(&first, [7, 8])?;
    build_primary(&second, [7, 8])?;

    let first_bytes = fs::read(&first)?;
    let second_bytes = fs::read(&second)?;
    assert_eq!(
        first_bytes.len(),
        second_bytes.len(),
        "the nonce record must be fixed width so it cannot be told apart by length"
    );
    assert_ne!(
        first_bytes, second_bytes,
        "two creates with identical content produced identical primaries: no creation nonce"
    );

    // This format carries no per-record checksum, so with identical content the
    // only bytes that may differ are the nonce itself: a run confined to 16
    // contiguous bytes. (Two random nonces may
    // coincide on individual bytes, so the span is bounded, not the count.)
    let differing: Vec<usize> = first_bytes
        .iter()
        .zip(second_bytes.iter())
        .enumerate()
        .filter(|(_, (left, right))| left != right)
        .map(|(index, _)| index)
        .collect();
    assert!(!differing.is_empty());
    let start = differing[0];
    let end = differing[differing.len() - 1] + 1;
    assert!(
        end - start <= 16,
        "two identical creates differ outside a single 16-byte nonce, at {start}..{end}"
    );

    // Uniqueness is per create, not merely "the two samples differ": build a
    // batch and require every file to be distinct.
    let mut windows = Vec::new();
    for index in 0..8 {
        let path = directory.path().join(format!("nonce-{index}.varlit"));
        build_primary(&path, [7, 8])?;
        windows.push(fs::read(&path)?);
    }
    let mut sorted = windows.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        windows.len(),
        "two creates produced the same creation nonce"
    );
    Ok(())
}

/// Runs a caller-owned-output read that always fills `out`, returning the value.
#[allow(dead_code)]
fn filled_out<T: varlit::VarlitEmpty, R>(
    read: impl FnOnce(&mut T) -> varlit::Result<R>,
) -> varlit::Result<T> {
    let mut out = T::empty();
    read(&mut out)?;
    Ok(out)
}

/// Runs a caller-owned-output read that reports presence; `None` when absent.
#[allow(dead_code)]
fn filled<T: varlit::VarlitEmpty>(
    read: impl FnOnce(&mut T) -> varlit::Result<bool>,
) -> varlit::Result<Option<T>> {
    let mut out = T::empty();
    Ok(read(&mut out)?.then_some(out))
}
