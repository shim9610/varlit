//! Scalable primary/sidecar identity contracts.
//!
//! A stream or indexed sidecar must belong to the exact logical generation of
//! the primary it was published against. Schema hash, OS object identity and
//! the deterministic file header are all preserved by an in-place rewrite of a
//! primary with an equal-length primary of the same format, so those alone
//! cannot separate two generations of one file object; the sidecar must refuse
//! such a primary at open and force a rebuild.
//!
//! A disk-index descriptor must likewise carry the block schema fingerprint of
//! the concrete type whose decoder it captured, and that identity must be
//! validated at plan construction — before any captured codec can be handed
//! primary bytes.

use std::fs;
use std::path::Path;

use varlit::{
    BlockDescriptor, BlockKind, Decoder, Encoder, Endian, Error, FormatSpec, IndexPolicy,
    IntegrityPolicy, ManifestPolicy, ReadLimits, RecoveryPolicy, Result, VarlitBlock, VarlitDecode,
    VarlitEncode, VarlitKeyedBlock, VarlitStreamWriter, WireType,
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

/// Records enough leading filler that the two primaries share far more than
/// [`PRIMARY_GENERATION_WINDOW`](varlit) bytes (4096) of common content.
const WINDOW_DEFEATING_FILLER: u64 = 400;

fn rewrite_in_place(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    let mut file = fs::OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
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

    let window = 4096.min(first_bytes.len());
    assert_ne!(
        first_bytes[..window],
        second_bytes[..window],
        "the creation nonce must sit inside the bounded generation window"
    );

    // This format carries no per-record checksum, so with identical content the
    // only bytes that may differ are the nonce itself: a run confined to 16
    // contiguous bytes, inside the generation window. (Two random nonces may
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
    assert!(
        end <= window,
        "the creation nonce must sit inside the generation window, got {start}..{end}"
    );

    // Uniqueness is per create, not merely "the two samples differ": build a
    // batch and require every leading window to be distinct.
    let mut windows = Vec::new();
    for index in 0..8 {
        let path = directory.path().join(format!("nonce-{index}.varlit"));
        build_primary(&path, [7, 8])?;
        windows.push(fs::read(&path)?[..window].to_vec());
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

#[test]
fn stream_state_sidecar_refuses_a_rewritten_primary() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("stream-first.varlit");
    let second = directory.path().join("stream-second.varlit");
    let options = varlit::StreamOptions::default();

    for (path, keys) in [(&first, [1u64, 2u64]), (&second, [3, 4])] {
        let mut writer = VarlitStreamWriter::create(stream_spec(), path, options)?;
        for key in keys {
            writer.push_info(&item(key, "fixed-width-value"))?;
        }
        writer.sync()?;
    }

    let second_bytes = fs::read(&second)?;
    assert_eq!(fs::read(&first)?.len(), second_bytes.len());
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new().write(true).open(&first)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&second_bytes)?;
        file.sync_all()?;
    }

    let opened = VarlitStreamWriter::open(stream_spec(), &first, options);
    match opened {
        Err(Error::DiskIndex(error))
            if matches!(*error, varlit::DiskIndexError::PrimaryGenerationMismatch) => {}
        other => panic!("stale stream state sidecar accepted: {:?}", other.err()),
    }
    Ok(())
}

/// The consumer half. Sidecar publication cannot be made atomic against a
/// process that replaces the primary *pathname* without taking the writer lock,
/// so the library narrows the interval and retires a sidecar it published for a
/// replaced primary. What keeps that a rebuild-cost problem rather than a
/// wrong-data problem is this: a sidecar whose primary was swapped wholesale for
/// a different file object is refused by consumers on identity, before any
/// indexed lookup can be answered from it.
///
/// The existing rewrite tests replace a primary's *contents* in place, keeping
/// the OS object. This one replaces the object itself, which is the shape a
/// pathname mutator produces.
#[test]
fn stream_state_sidecar_refuses_a_wholesale_primary_replacement() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("swap-first.varlit");
    let second = directory.path().join("swap-second.varlit");
    let options = varlit::StreamOptions::default();

    for (path, keys) in [(&first, [1u64, 2u64]), (&second, [3, 4])] {
        let mut writer = VarlitStreamWriter::create(stream_spec(), path, options)?;
        for key in keys {
            writer.push_info(&item(key, "fixed-width-value"))?;
        }
        writer.sync()?;
    }

    // The whole file object behind the pathname is replaced; the state sidecar
    // published for the original object stays where it was.
    fs::rename(&second, &first)?;

    match VarlitStreamWriter::open(stream_spec(), &first, options) {
        Err(Error::DiskIndex(error)) => assert!(
            matches!(*error, varlit::DiskIndexError::IdentityMismatch)
                || matches!(*error, varlit::DiskIndexError::PrimaryGenerationMismatch),
            "unexpected sidecar rejection: {error:?}"
        ),
        other => panic!(
            "a sidecar for a replaced primary was accepted: {:?}",
            other.err()
        ),
    }
    Ok(())
}

/// The stream half of the same residual gap.
#[test]
fn stream_state_sidecar_refuses_a_rewrite_beyond_the_generation_window() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("wide-stream-first.varlit");
    let second = directory.path().join("wide-stream-second.varlit");
    let options = varlit::StreamOptions::default();

    for (path, keys) in [(&first, [1u64, 2u64]), (&second, [3, 4])] {
        let mut writer = VarlitStreamWriter::create(stream_spec(), path, options)?;
        for index in 0..WINDOW_DEFEATING_FILLER {
            writer.push_info(&item(1_000_000 + index, "fixed-width-value"))?;
        }
        for key in keys {
            writer.push_info(&item(key, "fixed-width-value"))?;
        }
        writer.sync()?;
    }

    let first_bytes = fs::read(&first)?;
    let second_bytes = fs::read(&second)?;
    assert_eq!(first_bytes.len(), second_bytes.len());
    assert!(first_bytes.len() > 4 * 4096);
    let divergence = first_bytes
        .iter()
        .zip(second_bytes.iter())
        .enumerate()
        .skip(4096)
        .find(|(_, (left, right))| left != right)
        .map_or(first_bytes.len(), |(index, _)| index);
    assert!(
        divergence > 4 * 4096,
        "probe requires divergence past the generation window, got {divergence}"
    );

    rewrite_in_place(&first, &second_bytes)?;

    match VarlitStreamWriter::open(stream_spec(), &first, options) {
        Err(Error::DiskIndex(error))
            if matches!(*error, varlit::DiskIndexError::PrimaryGenerationMismatch) => {}
        other => panic!(
            "stale stream state sidecar accepted past the generation window: {:?}",
            other.err()
        ),
    }
    Ok(())
}

#[test]
fn stream_restore_refuses_missing_confirmed_heads_without_truncating_the_primary() -> Result<()> {
    for dirty in [false, true] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("head-damage.varlit");
        let options = varlit::StreamOptions::default();
        let mut writer = VarlitStreamWriter::create(stream_spec(), &path, options)?;
        for key in [10, 11] {
            writer.push_info(&item(key, "confirmed"))?;
        }
        writer.sync()?;
        let pinned = varlit::VarlitStreamReader::open(stream_spec(), &path, options)?;
        if dirty {
            writer.push_info(&item(12, "unconfirmed"))?;
            writer.flush()?;
        }
        drop(writer);
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".vks");
        let sidecar = std::path::PathBuf::from(sidecar);
        let primary_before = fs::read(&path)?;
        let mut damaged = fs::read(&sidecar)?;
        // VIX v5 has independently checksummed mirrored confirmed heads.
        damaged[4096 + 8] ^= 1;
        fs::write(&sidecar, &damaged)?;
        assert_eq!(
            varlit::VarlitStreamReader::open(stream_spec(), &path, options)?.verify_all()?,
            // The creation nonce, two records and their sync marker.
            4
        );
        damaged[8192 + 8] ^= 1;
        fs::write(&sidecar, &damaged)?;
        assert!(varlit::VarlitStreamReader::open(stream_spec(), &path, options).is_err());
        assert!(VarlitStreamWriter::open(stream_spec(), &path, options).is_err());
        assert!(
            VarlitStreamWriter::restore_checkpoint_and_open(stream_spec(), &path, options).is_err()
        );
        assert_eq!(
            fs::read(&path)?,
            primary_before,
            "recovery truncated confirmed records"
        );
        assert_eq!(
            fs::read(&sidecar)?,
            damaged,
            "recovery republished a stale checkpoint"
        );
        assert_eq!(pinned.verify_all()?, 4);
    }
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
