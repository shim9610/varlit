use crate::TryClone;
#[path = "matrix_bulk.rs"]
mod matrix_bulk;

use crate::primary_file::PrimaryFile;
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions, remove_file};
use std::hash::Hash;
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{
    BlockKind, BlockVec, CommitPolicy, CompressionAlgorithm, CompressionHeaderMode,
    CompressionLevel, CompressionPolicy, Endian, Error, FormatSpec, HeaderSlotIntegrity,
    IndexPolicy, IntegrityPolicy, IntegrityVerification, KeyedBlockVec, LivenessPolicy,
    ManifestPolicy, MatrixCellStatus, MatrixCommitEvent, MatrixCursor, MatrixDimensions, MatrixKey,
    MatrixPage, MatrixRecoveryAction, MatrixRecoveryReport, MatrixRegion, MatrixResumeSignal,
    RecoveryPolicy, Result, SnapshotFile, VariableCompression, VarlitBlock, VarlitEncode,
    VarlitKeyedBlock, VarlitMatrixBlock, VarlitMerge, VarlitMigration, VarlitReplaceBlock,
    WireType,
    codec::encode_to_vec_limited,
    collections::MaterializationBudget,
    format::ReadLimitKey,
    native_layout::{
        decode_native_internal_key_envelope, decode_native_internal_op_envelope,
        decode_native_record_footer, encode_native_record_footer, encode_native_record_header,
        native_file_header_len, native_record_footer_len, native_record_header_len,
        read_native_file_header, read_native_record_header, write_native_file_header,
        write_native_record_header,
    },
    writer_permit::{GuardedWriter, MutationPermit, PoisonFlag},
};

pub const TOMBSTONE_BLOCK_ID: u32 = 0xFFFF_FFFE;
pub const OP_BLOCK_ID: u32 = 0xFFFF_FFFD;
pub const METADATA_BLOCK_ID: u32 = 0xFFFF_FFFC;
pub const INDEX_BLOCK_ID: u32 = 0xFFFF_FFFB;
pub const MANIFEST_BLOCK_ID: u32 = 0xFFFF_FFFA;
pub const COMMIT_BLOCK_ID: u32 = 0xFFFF_FFF9;
/// Carries the per-create nonce of a stream primary.
pub const CREATION_NONCE_BLOCK_ID: u32 = 0xFFFF_FFF8;
/// Carries one internal segment: the records a single commit point added.
///
/// A segment is varlit's own lookup unit and has no declaration surface. See
/// [`IndexPolicy::segment_on_flush`].
pub const SEGMENT_BLOCK_ID: u32 = 0xFFFF_FFF7;
const RESERVED_BLOCK_ID_START: u32 = 0xFFFF_FF00;

/// Internal record IDs. Header-tail capacity is derived from this list; compile-time
/// assertions require reserved-range membership and unique IDs. Source gates also
/// require every internal `*_BLOCK_ID` declaration to appear in the list.
pub(crate) const INTERNAL_BLOCK_IDS: &[u32] = &[
    TOMBSTONE_BLOCK_ID,
    OP_BLOCK_ID,
    METADATA_BLOCK_ID,
    INDEX_BLOCK_ID,
    MANIFEST_BLOCK_ID,
    COMMIT_BLOCK_ID,
    CREATION_NONCE_BLOCK_ID,
    SEGMENT_BLOCK_ID,
    MATRIX_CHUNK_BLOCK_ID,
    OPEN_DIGEST_BLOCK_ID,
    crate::sync_marker::SYNC_BLOCK_ID,
    MATRIX_TILE_BLOCK_ID,
];

/// Matrix storage records: a matrix's metadata, its write-ahead ring, tile
/// directory pages and tiles. The block version says which (see
/// `matrix_tiles::RecordKind`); every one is appended once and then changed
/// in place, so each carries the in-place record flag.
pub const MATRIX_TILE_BLOCK_ID: u32 = 0xFFFF_FFF3;

const _: () = {
    let mut index = 0;
    while index < INTERNAL_BLOCK_IDS.len() {
        assert!(
            INTERNAL_BLOCK_IDS[index] >= RESERVED_BLOCK_ID_START,
            "an internal block id must be inside the reserved range"
        );
        let mut other = index + 1;
        while other < INTERNAL_BLOCK_IDS.len() {
            assert!(
                INTERNAL_BLOCK_IDS[index] != INTERNAL_BLOCK_IDS[other],
                "internal block ids must be distinct"
            );
            other += 1;
        }
        index += 1;
    }
};
pub(crate) const RECORD_HEADER_LEN: u64 = 32;
pub(crate) const RECORD_FOOTER_LEN: u64 = 32;

/// The record is dead: the writer marked it as no longer wanted, and a
/// defragmenting rewrite may drop it.
///
/// Advisory by construction — the word it lives in is outside the record
/// checksum, which is what makes it writable after the fact. It says "the
/// writer marked this", never "these bytes are sound".
pub const RECORD_MUTABLE_FLAG_DEAD: u32 = 0x0000_0001;

/// The width of the footer's trailing mutable word — the `reserved` field that
/// [`LivenessPolicy::FooterFlags`] turns into record flags and takes out of the
/// checksum. Named here because two places need to agree on it and a footer
/// layout change has to break both.
pub(crate) const RECORD_FOOTER_MUTABLE_LEN: u64 = 4;
const RECORD_FLAG_COMPRESSED: u16 = 0x0001;
pub(crate) const RECORD_FLAG_INTERNAL: u16 = 0x8000;
/// The record's payload is rewritten in place after it is appended, so its
/// checksum covers the header (and footer) only; the payload carries its own
/// checksums. Only matrix storage records ([`MATRIX_TILE_BLOCK_ID`]) use it.
pub(crate) const RECORD_FLAG_IN_PLACE: u16 = 0x4000;
/// A variable record whose payload carries a CRC per 64 KiB chunk
/// (`record_chunks`). Set with [`RECORD_FLAG_COMPRESSED`] when its chunks are
/// compressed.
pub(crate) const RECORD_FLAG_CHUNKED: u16 = 0x0002;
const RECORD_KNOWN_FLAGS: u16 =
    RECORD_FLAG_COMPRESSED | RECORD_FLAG_CHUNKED | RECORD_FLAG_INTERNAL | RECORD_FLAG_IN_PLACE;
pub(crate) const RECORD_FOOTER_MAGIC: &[u8; 4] = b"VRF1";
pub(crate) const RECORD_FOOTER_VERSION: u16 = 1;
pub(crate) const RECORD_FOOTER_FLAG_PREV_SAME_BLOCK: u16 = 0x0001;
pub(crate) const RECORD_FOOTER_FLAG_PREV_SAME_KEY: u16 = 0x0002;
pub(crate) const RECORD_FOOTER_KNOWN_FLAGS: u16 =
    RECORD_FOOTER_FLAG_PREV_SAME_BLOCK | RECORD_FOOTER_FLAG_PREV_SAME_KEY;
const COMMIT_PAYLOAD_MAGIC: &[u8; 4] = b"VCMT";
const INDEX_CHECKPOINT_MAGIC: &[u8; 4] = b"VIDX";
const INDEX_CHECKPOINT_VERSION: u16 = 3;
const SEGMENT_MAGIC: &[u8; 4] = b"VSEG";
const SEGMENT_VERSION: u16 = 1;
/// magic(4) + version(2) + flags(2) + covered_start(8) + entry_count(8)
/// + preceding_records(8).
const SEGMENT_PREFIX_LEN: u64 = 4 + 2 + 2 + 8 + 8 + 8;
/// The segment record's own start offset, written at the end of its payload.
///
/// Open arrives at EOF holding a footer and nothing else: the footer carries
/// no self offset and the header that would give one is `payload_len` bytes
/// further back, a distance only the header states. This trailer is the one
/// value that closes that circle, and it costs eight bytes of payload rather
/// than a byte of format.
const SEGMENT_TRAILER_LEN: u64 = 8;
/// One serialized index entry, in the layout `read_index_entry_payload`
/// decodes at checkpoint version 3.
const SEGMENT_ENTRY_LEN: u64 = 4 + 2 + 2 + 8 + 8 + 8 + 8 + 4 + 4 + 8 + 8 + 8 + 1;
/// Carries the three facts an open needs that live in no single record.
///
/// A segment says *where every record is*, so it costs an entry per record. A
/// digest says only what an open cannot compute without walking: the sequence
/// the next append takes, and each block's newest record. It costs twelve bytes
/// per distinct block id and nothing per record. See
/// [`IndexPolicy::open_digest_on_flush`].
pub const OPEN_DIGEST_BLOCK_ID: u32 = 0xFFFF_FFF5;
const DIGEST_MAGIC: &[u8; 4] = b"VDIG";
const DIGEST_VERSION: u16 = 1;
/// magic(4) + version(2) + flags(2) + sequence_high_water(8) + tail_count(4).
///
/// `physical_end` is deliberately *not* in here. The digest is the last record
/// in the file, so the committed end is its own end — a number open already
/// holds once it has framed the digest, and one that cannot disagree with the
/// bytes the way a stored copy could.
const DIGEST_PREFIX_LEN: u64 = 4 + 2 + 2 + 8 + 4;
/// One block tail: the block id and its newest record's offset.
const DIGEST_TAIL_LEN: u64 = 4 + 8;
/// The digest record's own start offset, closing the same circle
/// [`SEGMENT_TRAILER_LEN`] closes: open arrives at EOF holding a footer, and
/// the header that would give the record's start is `payload_len` bytes back.
const DIGEST_TRAILER_LEN: u64 = 8;
/// Presence flag for the full-width sequence high-water mark. A clear flag means
/// no sequence exists; no `u64` value is reserved, including `u64::MAX`.
const DIGEST_FLAG_SEQUENCE: u16 = 0x0001;

/// Every flag bit this version defines. A digest that sets one outside this set
/// was written by something that means more by it than this build knows, and is
/// answered with the scan rather than half-understood.
const DIGEST_FLAGS_KNOWN: u16 = DIGEST_FLAG_SEQUENCE;

/// What one digest record says.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OpenDigest {
    /// The end of the digest record itself, which is the committed end of the
    /// file it describes.
    physical_end: u64,
    sequence_high_water: Option<u64>,
    block_tails: BlockTails,
}

/// The payload length a digest covering `tails` distinct blocks occupies.
fn digest_payload_len(tails: u64) -> Result<u64> {
    tails
        .checked_mul(DIGEST_TAIL_LEN)
        .and_then(|bytes| bytes.checked_add(DIGEST_PREFIX_LEN))
        .and_then(|bytes| bytes.checked_add(DIGEST_TRAILER_LEN))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "digest payload length",
        })
}

/// Serializes the digest for a file whose last committed record is already
/// down, and whose next record will be the digest itself at `record_offset`.
fn encode_digest_payload(
    spec: FormatSpec,
    sequence_high_water: Option<u64>,
    tails: &[(u32, u64)],
    record_offset: u64,
) -> Result<Vec<u8>> {
    let count = u32::try_from(tails.len()).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "digest block count",
    })?;
    let payload_len = digest_payload_len(u64::from(count))?;
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, payload_len)?;
    let capacity =
        usize::try_from(payload_len).map_err(|_| Error::LengthOverflow { value: payload_len })?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource: "digest payload",
            requested: payload_len,
        })?;
    payload.extend_from_slice(DIGEST_MAGIC);
    payload.extend_from_slice(&DIGEST_VERSION.to_le_bytes());
    let (flags, sequence) = match sequence_high_water {
        Some(mark) => (DIGEST_FLAG_SEQUENCE, mark),
        None => (0, 0),
    };
    payload.extend_from_slice(&flags.to_le_bytes());
    payload.extend_from_slice(&sequence.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    for (block_id, offset) in tails {
        payload.extend_from_slice(&block_id.to_le_bytes());
        payload.extend_from_slice(&offset.to_le_bytes());
    }
    payload.extend_from_slice(&record_offset.to_le_bytes());
    Ok(payload)
}

/// Reads a digest out of a payload the caller has already framed and
/// checksum-verified, or reports that these bytes are not one.
///
/// Every field is checked against the file rather than trusted: the tail
/// offsets must name records inside the append log and before the digest, and
/// the ids must be strictly ascending so a corrupt count cannot make the walk
/// revisit. A payload that fails any of it is not a digest, and the caller
/// answers that with the scan.
fn decode_digest_payload(
    payload: &[u8],
    append_start: u64,
    record_offset: u64,
    physical_end: u64,
) -> Result<OpenDigest> {
    let prefix_len = crate::width::small(DIGEST_PREFIX_LEN);
    let trailer_len = crate::width::small(DIGEST_TRAILER_LEN);
    if payload.len() < prefix_len + trailer_len {
        return Err(Error::InvalidIndexSegment);
    }
    if &payload[..4] != DIGEST_MAGIC {
        return Err(Error::InvalidIndexSegment);
    }
    let mut u16_buf = [0; 2];
    u16_buf.copy_from_slice(&payload[4..6]);
    if u16::from_le_bytes(u16_buf) != DIGEST_VERSION {
        return Err(Error::InvalidIndexSegment);
    }
    u16_buf.copy_from_slice(&payload[6..8]);
    let flags = u16::from_le_bytes(u16_buf);
    if flags & !DIGEST_FLAGS_KNOWN != 0 {
        return Err(Error::InvalidIndexSegment);
    }
    let mut u64_buf = [0; 8];
    u64_buf.copy_from_slice(&payload[8..16]);
    let stored_sequence = u64::from_le_bytes(u64_buf);
    let mut u32_buf = [0; 4];
    u32_buf.copy_from_slice(&payload[16..20]);
    let count = u32::from_le_bytes(u32_buf) as usize;
    let body_len = count
        .checked_mul(crate::width::small(DIGEST_TAIL_LEN))
        .ok_or(Error::InvalidIndexSegment)?;
    if payload.len() != prefix_len + body_len + trailer_len {
        return Err(Error::InvalidIndexSegment);
    }
    u64_buf.copy_from_slice(&payload[payload.len() - trailer_len..]);
    if u64::from_le_bytes(u64_buf) != record_offset {
        return Err(Error::InvalidIndexSegment);
    }
    let mut tails: Vec<(u32, u64)> = Vec::new();
    tails
        .try_reserve_exact(count)
        .map_err(|_| Error::AllocationFailed {
            resource: "digest block tails",
            requested: body_len as u64,
        })?;
    let mut previous: Option<u32> = None;
    for index in 0..count {
        let at = prefix_len + index * crate::width::small(DIGEST_TAIL_LEN);
        u32_buf.copy_from_slice(&payload[at..at + 4]);
        let block_id = u32::from_le_bytes(u32_buf);
        u64_buf.copy_from_slice(&payload[at + 4..at + 12]);
        let offset = u64::from_le_bytes(u64_buf);
        if previous.is_some_and(|last| block_id <= last) {
            return Err(Error::InvalidIndexSegment);
        }
        // A tail must name a record of this file that is behind the digest.
        // The digest is the last record, so "before it" and "inside the append
        // log" together are the whole of what an offset can be here.
        if offset < append_start || offset >= record_offset {
            return Err(Error::InvalidIndexSegment);
        }
        previous = Some(block_id);
        tails.push((block_id, offset));
    }
    Ok(OpenDigest {
        physical_end,
        sequence_high_water: (flags & DIGEST_FLAG_SEQUENCE != 0).then_some(stored_sequence),
        block_tails: BlockTails::from_sorted(tails),
    })
}
/// Carries one matrix chunk: `rows_per_chunk` rows of every matrix block.
///
/// Reserved. The chunked matrix layout wrote these records; tiled matrix
/// storage grows by adding tiles and writes none.
pub const MATRIX_CHUNK_BLOCK_ID: u32 = 0xFFFF_FFF6;
/// Smallest live-tail growth that forces a fresh full index checkpoint.
///
const MANIFEST_PAYLOAD_VERSION: u16 = 5;
const WRITER_LOCK_MAGIC: &str = "varve-lock-v1";
const COMPRESSION_ENVELOPE_MAGIC: &[u8; 4] = b"VCMP";
const COMPRESSION_ENVELOPE_VERSION: u8 = 1;
const FILE_COMPRESSION_MAGIC: &[u8; 4] = b"VCHD";
const FILE_COMPRESSION_VERSION: u8 = 1;
/// `VCHD`'s payload, excluding its magic.
///
/// `VCHD` predates the block framing below and carries no length prefix, so its
/// length is fixed by its own layout: version, algorithm, level kind, and the
/// `only_if_smaller` flag, then the exact level and the two length bounds.
const FILE_COMPRESSION_PAYLOAD_LEN: usize = 1 + 1 + 1 + 1 + 4 + 8 + 8;
/// The header block holding each block's newest record offset
/// ([`IndexPolicy::header_tails`]).
const HEADER_TAILS_MAGIC: &[u8; 4] = b"VBTT";

/// The liveness block: a writer's open/close mark, in the file header.
///
/// One bit of real content. A writer sets `DIRTY` when it takes the object and
/// clears it when it releases cleanly, so a later open that *can* take the
/// object and still finds the bit set is looking at a file whose last writer
/// died without releasing. The writer lock supplies the other half of that
/// question — "is anybody holding it now" — and it is an object lock, so hard
/// links and path aliases cannot dodge it.
///
/// This is the only durable record varlit keeps of a writer's lifecycle. The
/// alternative signal, "does the file end exactly at a commit point", is
/// positional: it can only ever describe a suffix, never an interior record,
/// and the next read-write open truncates that suffix away before anyone can
/// ask. A bit in the header is neither pushed along nor truncated.
const LIVENESS_MAGIC: &[u8; 4] = b"VLIV";
const LIVENESS_BLOCK_VERSION: u16 = 1;
/// `version u16 | flags u16 | crc32 u32`.
const LIVENESS_PAYLOAD_LEN: usize = 8;
/// The whole `VLIV` block a spec would write, framing included, or zero when
/// the format declares no liveness word.
///
/// Exists so the layout plan can add the same term the writer emits. It was
/// missing from `native_file_header_plan_extension_len`, which published a
/// header length 16 bytes short of the real one for every `footer_flags`
/// format — the plan described the first record as starting inside this block.
/// Nothing inside this crate read that number (`append_log_start_for_file`
/// derives the boundary from the file's own region), so no file was written
/// wrong; a tool reading `FormatSpec::effective_layout` was told wrong.
pub(crate) fn liveness_region_len(spec: FormatSpec) -> u64 {
    if !matches!(spec.liveness_policy, LivenessPolicy::FooterFlags) {
        return 0;
    }
    (8 + LIVENESS_PAYLOAD_LEN) as u64
}
/// The writer holds this object and has not released it cleanly.
const LIVENESS_FLAG_DIRTY: u16 = 0x0001;
use crate::published_end::{
    MAGIC as PUBLISHED_END_MAGIC, PAYLOAD_LEN as PUBLISHED_END_PAYLOAD_LEN,
};
const HEADER_TAILS_VERSION: u16 = 1;
/// Two slots, written alternately.
///
/// The update is an in-place overwrite of a live file, so there is a window in
/// which the bytes being written are neither the old table nor the new one. A
/// single slot would make that window a hole: a crash inside it leaves nothing
/// readable and the open falls all the way back to a full scan. Two slots make
/// it a step back — the slot *not* being written still carries the previous
/// commit point's table, so the fallback is an earlier commit boundary rather
/// than no boundary.
const HEADER_TAILS_SLOTS: usize = 2;
/// Entries reserved beyond the declared blocks: one per internal block id.
///
/// Derived from [`INTERNAL_BLOCK_IDS`] rather than chosen, because the capacity
/// is fixed at create and a table that outgrows it is not truncated — it is
/// dropped, with `HEADER_TAILS_FLAG_OVERFLOW` and `count = 0`, and the option
/// then does nothing for the life of that file. A hand-picked number is a
/// silent expiry date; a derived one widens the region when the list grows.
///
/// Reserving all ten is deliberate over-reserving. Three of them cannot
/// co-occur with this option — the open digest and matrix chunks are refused
/// alongside it, and the creation nonce belongs to a path that is — so the true
/// need today is seven. Twelve bytes each is not worth the reasoning it would
/// take to keep "which ids can co-occur" correct as the refusals change.
const HEADER_TAILS_RESERVED_ENTRIES: usize = 10;
// The capacity is part of every `header_tails` file already written, so it is
// pinned at the ten ids the list held when the option shipped. The sync marker
// added since is written only by checkpointed stream writers, which never track
// it as a tail, and matrix storage records exist only in matrix files, which
// refuse `header_tails`; the reservation still covers every id that can
// co-occur.
const _: () = assert!(INTERNAL_BLOCK_IDS.len() == HEADER_TAILS_RESERVED_ENTRIES + 2);
/// `version u16 | flags u16 | capacity u32 | count u32 | generation u64 |
/// commit_offset u64`.
///
/// `commit_offset` is what makes the table checkable. Every other field
/// describes the table; this one names the *commit marker record* the table was
/// written for, and a reader confirms it by framing that record and then
/// walking forward to the end of the file. See `adopt_header_tails`.
const HEADER_TAILS_SLOT_HEADER_LEN: usize = 2 + 2 + 4 + 4 + 8 + 8;
/// `block_id: u32 | tail_offset: u64`.
const HEADER_TAILS_ENTRY_LEN: usize = 4 + 8;
/// The slot's own crc32, over every byte of the slot before it.
const HEADER_TAILS_SLOT_TRAILER_LEN: usize = 4;
/// The table did not fit the slot's capacity: `count` is zero and the slot
/// claims nothing, rather than claiming a table that is silently partial.
const HEADER_TAILS_FLAG_OVERFLOW: u16 = 1;
const HEADER_TAILS_KNOWN_FLAGS: u16 = HEADER_TAILS_FLAG_OVERFLOW;
/// The block's own framing: `magic[4] | len: u32`.
const HEADER_TAILS_BLOCK_FRAMING_LEN: usize = 4 + 4;
const MATRIX_SIDECAR_MAGIC: &[u8; 4] = b"VSID";
// v2 bound the sidecar to the native file's OS-object identity and matrix
// layout generation (/05). v3 additionally binds it to the per-create
// creation nonce stamped into the native matrix file, so recreating a matrix
// on the same OS file object invalidates every earlier sidecar.
// v1/v2 envelopes are refused as stale/regenerable.
const MATRIX_SIDECAR_VERSION: u16 = 3;
// Fixed header: 48-byte v1 prefix + 32-byte native fingerprint + 8-byte matrix
// layout generation + 16-byte matrix creation nonce.
const MATRIX_SIDECAR_FIXED_LEN: usize = 104;
const MATRIX_SIDECAR_FINGERPRINT_OFFSET: usize = 48;
const MATRIX_SIDECAR_LAYOUT_GENERATION_OFFSET: usize = 80;
const MATRIX_SIDECAR_CREATION_NONCE_OFFSET: usize = 88;
const MATRIX_CREATION_NONCE_LEN: usize = 16;
#[cfg(feature = "integrity")]
const STREAM_BUFFER_LEN: usize = 64 * 1024;
const WRITER_LOCK_MAX_LEN: u64 = 16 * 1024;
const PHYSICAL_PAYLOAD_RESOURCE: &str = "payload";
const LOGICAL_PAYLOAD_RESOURCE: &str = "logical payload";
const WRITER_POISON_CONTEXT: &str = "file";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaManifest {
    pub payload_version: u16,
    pub format_version: u16,
    pub endian: Endian,
    pub schema_hash: u64,
    pub extension: Option<String>,
    pub index_policy: IndexPolicy,
    pub commit_policy: CommitPolicy,
    pub integrity_policy: IntegrityPolicy,
    pub recovery_policy: RecoveryPolicy,
    pub manifest_policy: ManifestPolicy,
    pub compression_policy: CompressionPolicy,
    pub blocks: Vec<SchemaBlockDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaBlockDescriptor {
    pub id: u32,
    pub name: String,
    pub version: u16,
    pub kind: BlockKind,
    pub fields: Vec<SchemaFieldDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaFieldDescriptor {
    pub id: u32,
    pub name: String,
    pub wire_type: WireType,
    pub presence: crate::FieldPresence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixSidecarManifest {
    pub format_magic: Vec<u8>,
    pub format_version: u16,
    pub schema_hash: u64,
    pub category: String,
    pub generation: u64,
    pub payload_len: u64,
    pub payload_crc32: u32,
    /// Fingerprint of the native matrix file's OS object (volume + file ID on
    /// Windows, device + inode on Unix) folded with the schema hash. Binds the
    /// sidecar to one specific native file so a same-spec sibling cannot adopt
    /// it.
    pub native_fingerprint: [u8; 32],
    /// The matrix layout generation the sidecar was published against, so a
    /// reader whose native layout has changed rejects the stale sidecar.
    pub matrix_layout_generation: u64,
    /// The per-create creation nonce of the native matrix file the sidecar was
    /// published against. Recreating a matrix on the same OS file object keeps
    /// the OS identity and layout offsets stable, so this nonce is what makes
    /// every pre-recreate sidecar refusable as stale.
    pub matrix_creation_nonce: [u8; MATRIX_CREATION_NONCE_LEN],
}

/// Identity of the native matrix file a sidecar is bound to.
///
/// Recomputed by every reader from its own open native file and matrix layout,
/// then compared against the values recorded in the sidecar envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MatrixNativeIdentity {
    fingerprint: [u8; 32],
    layout_generation: u64,
    creation_nonce: [u8; MATRIX_CREATION_NONCE_LEN],
}

pub trait MatrixNumeric: Copy {
    const BYTE_LEN: usize;

    fn read_matrix_numeric(bytes: &[u8], endian: Endian) -> Self;
}

macro_rules! impl_matrix_integer {
    ($ty:ty, $len:expr) => {
        impl MatrixNumeric for $ty {
            const BYTE_LEN: usize = $len;

            fn read_matrix_numeric(bytes: &[u8], endian: Endian) -> Self {
                let bytes: [u8; $len] = bytes.try_into().expect("validated matrix numeric width");
                match endian {
                    Endian::Little => <$ty>::from_le_bytes(bytes),
                    Endian::Big => <$ty>::from_be_bytes(bytes),
                }
            }
        }
    };
}

impl MatrixNumeric for u8 {
    const BYTE_LEN: usize = 1;

    fn read_matrix_numeric(bytes: &[u8], _endian: Endian) -> Self {
        bytes[0]
    }
}

impl MatrixNumeric for i8 {
    const BYTE_LEN: usize = 1;

    fn read_matrix_numeric(bytes: &[u8], _endian: Endian) -> Self {
        bytes[0] as i8
    }
}

impl_matrix_integer!(u16, 2);
impl_matrix_integer!(i16, 2);
impl_matrix_integer!(u32, 4);
impl_matrix_integer!(i32, 4);
impl_matrix_integer!(u64, 8);
impl_matrix_integer!(i64, 8);
impl_matrix_integer!(u128, 16);
impl_matrix_integer!(i128, 16);

impl MatrixNumeric for f32 {
    const BYTE_LEN: usize = 4;

    fn read_matrix_numeric(bytes: &[u8], endian: Endian) -> Self {
        let raw = u32::read_matrix_numeric(bytes, endian);
        Self::from_bits(raw)
    }
}

impl MatrixNumeric for f64 {
    const BYTE_LEN: usize = 8;

    fn read_matrix_numeric(bytes: &[u8], endian: Endian) -> Self {
        let raw = u64::read_matrix_numeric(bytes, endian);
        Self::from_bits(raw)
    }
}

/// Optional barriers for a durable matrix cell write. The file is the primary
/// file that holds the matrix: only synchronize it, do not seek or modify it.
/// Both callbacks run before the write is published. The notification hook
/// runs only after publication and sync.
pub trait MatrixDurabilityBarrier {
    fn sync_matrix_data(&mut self, file: &mut File) -> Result<()>;
    fn sync_matrix_commit(&mut self, file: &mut File) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FileMatrixDurabilityBarrier;

impl MatrixDurabilityBarrier for FileMatrixDurabilityBarrier {
    fn sync_matrix_data(&mut self, file: &mut File) -> Result<()> {
        file.sync_data()?;
        Ok(())
    }

    fn sync_matrix_commit(&mut self, file: &mut File) -> Result<()> {
        file.sync_all()?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    ReadOnly,
    ReadWrite,
}

/// Mechanical enforcement for : a replacement target cannot be addressed
/// without having been version-checked.
///
/// Every replacement entry point selects its target by block id and then
/// writes `T`'s payload into (or under) the stored record's header. Three of
/// the four copied the stored `block_version` while substituting the new
/// payload, which is a persistent type/version disagreement on disk whenever
/// `schema_hash = 0` lets a differently-versioned program open the file. Two
/// paths carried an ad-hoc check and two did not, and review alone had not
/// caught that in seven rounds.
///
/// The check is therefore no longer a step a path can forget: it is a
/// precondition of *writing* the target. [`ReplacementTarget`] has a private
/// field, lives in its own module so nothing else can name that field, and has
/// exactly one constructor - [`ReplacementTarget::resolve`], which performs
/// both the selection and the version refusal.
///
/// **What is and is not enforced, stated precisely.** Reading `self.index` is
/// not restricted: any code in this file may walk it and compute an ordinal,
/// and round 12's first attempt claimed otherwise. What a caller cannot do is
/// *act* on such an ordinal. The bytes of an already-indexed record are
/// reachable only through [`RecordFile::overwrite_indexed_record`], which takes
/// a [`RecordOverwrite`] by value; the only constructor of a [`RecordOverwrite`]
/// is [`RecordOverwrite::prepare`], which takes a [`ReplacementTarget`] by
/// value. So a replacement path added next month that resolves its own ordinal
/// over `self.index` has nothing it can hand to the writer, and does not
/// compile. See `mod record_file` for the other half (the raw handle is
/// unreachable, so the seek/write pair cannot be duplicated either).
///
/// Proved by `crates/varlit/tests/ui/fail_fabricated_replacement_target.rs` and
/// `fail_fabricated_record_overwrite.rs` (compile-fail), and by
/// `crates/varlit/tests/enforcement_gates.rs`.
#[path = "file/replacement_target.rs"]
pub(crate) mod replacement_target;

use replacement_target::{RecordOverwrite, ReplacementTarget};

#[path = "file/keyed_access.rs"]
mod keyed_access;
#[path = "file/matrix_tiles_io.rs"]
mod matrix_tiles_io;
/// Mechanical enforcement for shape A/B on the primary file handle: the record
/// bytes of an open generation cannot be written except through one of two
/// gated operations.
///
/// The round-12 review found that `overwrite_record_bytes_in_place` was "the
/// only function in the crate that rewrites the bytes of an already-indexed
/// record" — true of the code as written, and unenforced: a new
/// `self.file.seek(..)` / `self.file.write_all(..)` pair anywhere in this
/// ~12,000-line file compiled and skipped both the version refusal  and
/// the keyed-tail invalidation. That is the same "correct today,
/// described in prose" configuration that  was in.
///
/// [`RecordFile`] owns the handle in a module that keeps the field unnameable,
/// and exposes no general-purpose write. `Write`, `Seek` and `Read` are not
/// implemented for it and it hands out no `&File` (`impl Write for &File` would
/// make that equivalent to a mutable handle). The only two ways to put record
/// bytes on disk are:
///
/// * [`RecordFile::append_record_at_end`], which seeks to the end itself and
///   refuses if the resulting offset is not the one the caller budgeted for, so
///   it structurally cannot land inside an existing record; and
/// * [`RecordFile::overwrite_indexed_record`], which consumes a
///   [`RecordOverwrite`] — see `mod replacement_target` for what producing one
///   costs.
///
/// One deliberate exception, named here rather than left to be discovered:
/// [`RecordFile::matrix_region`] hands `crate::matrix` the `&mut File` its
/// functions take. The matrix region is disjoint from the record region and has
/// its own shape-A gate (`matrix.rs`, `mod page_index`). The gate that keeps
/// that exception honest is a source assertion, not the compiler:
/// `crates/varlit/tests/enforcement_gates.rs::the_primary_handle_escape_is_only_for_the_matrix_region`.
#[path = "file/record_file.rs"]
mod record_file;

use record_file::{AppendPayload, RecordFile};

/// Owner-local record directory with a bounded buffer and private spill file.
/// Reservation performs allocation and spill I/O before the authoritative
/// append. The append requires its token; installation afterwards is infallible.
/// Follow stages validated rows and rolls back the logical directory length on
/// a scratch error. Positional reads keep independent snapshots independent.
#[path = "native_directory.rs"]
pub(crate) mod resident_index;

use resident_index::{ReservedIndexSlot, ResidentIndex};
#[path = "native_scan.rs"]
mod native_scan;
use native_scan::{BufferedScan, FollowScan, ScanBuffer};
#[path = "native_rewrite.rs"]
mod native_rewrite;
use native_rewrite::{IndexPayload, OffsetRemap, RewriteDirectory};
#[path = "external_merge.rs"]
mod external_merge;

include!("file/record_maps.rs");

#[cfg(kani)]
#[path = "verification/record_extent.rs"]
mod record_extent_proofs;

include!("file/record_index_entry.rs");

include!("file/reports.rs");

include!("file/tails.rs");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordHeaderFields {
    pub(crate) block_id: u32,
    pub(crate) block_version: u16,
    pub(crate) flags: u16,
    pub(crate) sequence: u64,
    pub(crate) payload_len: u64,
    pub(crate) checksum: u32,
    pub(crate) uncompressed_len_hint: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecordFooterFields {
    pub(crate) prev_same_block_offset: Option<u64>,
    pub(crate) prev_same_key_offset: Option<u64>,
    /// The trailing footer word, which is the only part of a written record
    /// that can change afterwards.
    ///
    /// Zero under [`LivenessPolicy::None`], where it is the reserved word it has
    /// always been and a non-zero value is refused at decode. Under
    /// [`LivenessPolicy::FooterFlags`] it carries record flags and is excluded
    /// from the record checksum, which is what makes it writable in place
    /// without touching the payload.
    pub(crate) mutable_flags: u32,
}

/// A writer releases its crash verdict when it goes out of scope. Staged
/// matrix changes that were never published are dropped, as they would be by
/// a crash: publication (`sync`, `commit_durable`) is what makes them part of
/// the file.
impl Drop for VarlitFile {
    fn drop(&mut self) {
        // The clean-release half of the crash verdict. Best-effort by
        // necessity — `drop` cannot report — and that is the right shape here:
        // a clear that fails leaves the bit set, so the next open runs a
        // recovery pass it did not strictly need, which is the direction to
        // fail in. The opposite arrangement would let a crash read as clean.
        if self.mode == OpenMode::ReadWrite {
            // A release promises no durability, as `flush` does not: what
            // this writer appended becomes visible, as it was before the
            // published end existed.
            self.publish_visible_end();
            let _ = self.mark_writer_closed();
        }
    }
}

/// A file handle with owner-local caches. Send + !Sync, including when the
/// selected format contains only records. Each thread opens its own reader.
#[derive(Debug)]
pub struct VarlitFile {
    spec: FormatSpec,
    path: PathBuf,
    // Shape A/B, mechanical enforcement: the primary handle is not a `File`.
    // See `mod record_file` for what that forbids.
    file: RecordFile,
    snapshot: SnapshotFile,
    mode: OpenMode,
    // Shape A, mechanical enforcement: the mirror is not a `Vec`.
    // See `mod resident_index` for what that forbids.
    index: ResidentIndex,
    // The extension region this file actually carries, which is not always the
    // one `spec` would write: an unknown block is skipped at open, so a file
    // from a later release has a longer region than this build produces. Every
    // header-derived offset comes from here, and a rewrite writes it back
    // verbatim rather than regenerating it — regenerating would drop the
    // unknown block and silently move the append log.
    header_extensions: Vec<u8>,
    sequence_state: SequenceState,
    // O(1) append-side block-offset-chain predecessors; must equal
    // `BlockTails::from_index(&index)` at all times.
    block_tails: BlockTails,
    // The tiled matrix, when the spec declares matrix blocks: opened with the
    // handle (a writer's open finds what its ring holds; a reader's captures
    // its snapshot).
    tiles: std::cell::OnceCell<crate::matrix_tiles::api::TiledMatrix>,
    /// Keyed storage, when the format declares it: opened on first use.
    keyed: std::cell::OnceCell<keyed_access::KeyedEngine>,
    /// The last chunk a record read checked (`record_read`).
    record_cache: std::cell::RefCell<record_read::ChunkCache>,
    // O(1) commit-point state for the internal segment chain; must equal
    // `SegmentCursor::from_index(&index)` at all times.
    segment_cursor: SegmentCursor,
    // Whether a record that a commit marker would cover has been appended since
    // the last one. Tracked here rather than read off the resident index,
    // because a non-resident block's records are not in it: deriving the answer
    // from the index made `flush` decide there was nothing to commit and skip
    // the marker, which left every such record permanently uncommitted and
    // invisible to every reader.
    uncommitted_since_commit: bool,
    /// Scratch the logical payload is encoded into, reused across records.
    ///
    /// `StreamWriter` and `IndexedStreamWriter` have had this since their
    /// allocation contracts were written; `VarlitFile` never got it and
    /// allocated a fresh `Vec` per push through `encode_to_vec_limited`. The
    /// append hot path admits no per-record allocation, and this was one of the
    /// two payload-proportional ones the allocation-contract test records as
    /// outstanding.
    record_buffer: Vec<u8>,
    immediate_state: crate::immediate::ImmediateState,
    /// Whether any record has been appended since the last digest was written.
    ///
    /// `needs_open_digest` used to answer this with `last_resident_block_id`,
    /// which only moves for records the resident index carries. A workload
    /// flushing only non-resident records therefore left the flag reading
    /// "the digest is still last" while records piled up behind it — the
    /// digest went permanently stale and every `open_readonly_lazy` silently
    /// full-scanned. Non-resident blocks are exactly what the digest's tail
    /// table exists to reach, so that combination disabled the feature for its
    /// own purpose.
    appended_since_digest: bool,
    /// The records a single-barrier commit marker will name; see
    /// [`CommitScope`].
    commit_scope: CommitScope,
    /// The block id of the newest resident entry, maintained at the single
    /// index append site.
    ///
    /// Exists so that `needs_index_segment` and `needs_open_digest` can ask
    /// "is the last thing in this file already the record I am about to write"
    /// without faulting an entry off disk on every flush. A slot holds an
    /// offset and a committed bit, so the block id is not otherwise knowable
    /// without a read, and both predicates run on the append path.
    last_resident_block_id: Option<u32>,
    // Lazily built keyed-offset-chain predecessors for the generic keyed
    // append path; a cached map is either absent or exact.
    keyed_tails: KeyedTails,
    // True while this handle has created a pathname whose directory entry has
    // not been made durable yet. Cleared by the first successful durability
    // request, so the parent sync happens once per created file and never on
    // the append path.
    pending_pathname_parent_sync: bool,
    /// Whether the file's liveness block said `DIRTY` when this handle opened
    /// it — i.e. whether the previous writer died without releasing.
    ///
    /// Read *before* this open sets the bit for itself, so it describes the
    /// predecessor rather than this handle. Always `false` for a format without
    /// `liveness: footer_flags` and for every read-only handle, which never
    /// takes the object and so cannot tell "held" from "abandoned".
    opened_after_crash: bool,
    poison: PoisonFlag,
    _lock: Option<WriterLock>,
    // Keep pending discovery private to this reader; committed state above
    // remains usable after a failed follow or a discarded tentative scan.
    follow_scan: Option<Box<FollowScan>>,
}

include!("file/reader.rs");

include!("file/writer.rs");

/// The witness this file writer's disk-touching methods demand, typed by the
/// writer it speaks for. See `crate::writer_permit`.
pub(crate) type FileMutationPermit<'a> = MutationPermit<'a, VarlitFile>;

impl GuardedWriter for VarlitFile {
    fn poison_flag(&self) -> &PoisonFlag {
        &self.poison
    }
    fn poison_flag_mut(&mut self) -> &mut PoisonFlag {
        &mut self.poison
    }
}

// These implementation files share one module to preserve privacy and code generation.
include!("file/lifecycle.rs");
include!("file/append.rs");
include!("file/directory.rs");
include!("file/mutation.rs");
include!("file/follow.rs");
include!("file/matrix_access.rs");
include!("file/mapped_access.rs");
include!("file/header_slots.rs");

/// Newest-first walk of one block's footer chain.
///
/// Each step is two positional reads — the record header, then its footer — and
/// yields the entry it just framed. Nothing is retained: the walk holds one
/// offset, so a block with more records than memory is still walkable.
#[derive(Debug)]
pub struct BlockChain<'a> {
    spec: FormatSpec,
    snapshot: &'a SnapshotFile,
    block_id: u32,
    next: Option<u64>,
    visited: u64,
    /// The first byte a record may start at: a link below it is refused.
    floor: u64,
}

impl Iterator for BlockChain<'_> {
    type Item = Result<RecordIndexEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next?;
        match self.step(offset) {
            Ok(entry) => Some(Ok(entry)),
            Err(error) => {
                // A failed step ends the walk rather than repeating itself.
                self.next = None;
                Some(Err(error))
            }
        }
    }
}

impl BlockChain<'_> {
    fn step(&mut self, offset: u64) -> Result<RecordIndexEntry> {
        self.visited = self
            .visited
            .checked_add(1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Records, self.visited)?;
        if offset < self.floor || offset >= self.snapshot.len() {
            return Err(Error::InvalidCanonicalEncoding(
                "offset chain points outside the append log",
            ));
        }
        // Rebuilding an entry from its header, as a list fault does.
        scan::count_record_entry_fault();
        let entry = read_record_entry_positional(self.spec, self.snapshot, offset)?;
        if entry.block_id != self.block_id {
            return Err(Error::InvalidCanonicalEncoding(
                "block offset chain left its block",
            ));
        }
        // Strictly decreasing, so a crafted or damaged chain cannot loop: the
        // walk is bounded by the file whatever the footers claim. Defence in
        // depth rather than the first line - `decode_native_record_footer`
        // already refuses a predecessor at or past its own record, so a whole
        // file carrying one is refused at open and never reaches here. This
        // walk frames records that scan never looked at, which is why it
        // carries the check itself.
        self.next = match entry.prev_same_block_offset {
            Some(previous) if previous >= offset => {
                return Err(Error::InvalidCanonicalEncoding(
                    "block offset chain does not decrease",
                ));
            }
            previous => previous,
        };
        Ok(entry)
    }
}

/// One key's records, newest first, from [`VarlitFile::keyed_chain`].
///
/// The keyed counterpart of [`BlockChain`], and it holds what that holds: one
/// offset. Nothing is retained, so a key with more generations than memory is
/// still walkable.
///
/// It differs from `BlockChain` in one way, and that is the reason it is a
/// separate type rather than a parameter: the keyed chain legitimately runs
/// through [`TOMBSTONE_BLOCK_ID`], so there is no "left its block" refusal to
/// make. The caller filters on `block_id`. Records under [`OP_BLOCK_ID`] are
/// *not* on the chain -- `push_op` writes no keyed predecessor and moves no
/// tail.
#[derive(Debug)]
pub struct KeyedChain<'a> {
    spec: FormatSpec,
    snapshot: &'a SnapshotFile,
    next: Option<u64>,
    visited: u64,
    /// The first byte a record may start at: a link below it is refused.
    floor: u64,
}

impl Iterator for KeyedChain<'_> {
    type Item = Result<RecordIndexEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next?;
        match self.step(offset) {
            Ok(entry) => Some(Ok(entry)),
            Err(error) => {
                // A failed step ends the walk rather than repeating itself,
                // identically to `BlockChain`.
                self.next = None;
                Some(Err(error))
            }
        }
    }
}

impl KeyedChain<'_> {
    fn step(&mut self, offset: u64) -> Result<RecordIndexEntry> {
        self.visited = self
            .visited
            .checked_add(1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Records, self.visited)?;
        if offset < self.floor || offset >= self.snapshot.len() {
            return Err(Error::InvalidCanonicalEncoding(
                "offset chain points outside the append log",
            ));
        }
        let entry = read_record_entry_positional(self.spec, self.snapshot, offset)?;
        // Strictly decreasing, so a crafted or damaged chain cannot loop. The
        // same defence in depth `BlockChain` carries, and for the same reason:
        // this walk frames records that scan never looked at.
        self.next = match entry.prev_same_key_offset {
            Some(previous) if previous >= offset => {
                return Err(Error::InvalidCanonicalEncoding(
                    "keyed offset chain does not decrease",
                ));
            }
            previous => previous,
        };
        Ok(entry)
    }
}

pub(crate) fn read_record_entry_positional(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    record_offset: u64,
) -> Result<RecordIndexEntry> {
    let mut header = [0u8; crate::width::small(RECORD_HEADER_LEN)];
    snapshot.read_exact_at(record_offset, &mut header)?;
    let decoded = read_native_record_header(&mut &header[..], record_offset)?;
    let fields = decoded.fields;
    let payload_offset =
        record_offset
            .checked_add(decoded.lead_in_len)
            .ok_or(Error::CorruptTail {
                offset: record_offset,
            })?;
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, fields.payload_len)?;
    let payload_end = payload_offset
        .checked_add(fields.payload_len)
        .ok_or(Error::CorruptTail {
            offset: record_offset,
        })?;
    let mut entry = RecordIndexEntry {
        block_id: fields.block_id,
        block_version: fields.block_version,
        flags: fields.flags,
        sequence: fields.sequence,
        record_offset,
        payload_offset,
        payload_len: fields.payload_len,
        checksum: fields.checksum,
        uncompressed_len_hint: fields.uncompressed_len_hint,
        footer_offset: None,
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        mutable_flags: 0,
        committed: true,
    };
    validate_record_entry(&spec, &entry)?;
    if spec.spec_needs_record_footer() {
        let mut footer = [0u8; crate::width::small(RECORD_FOOTER_LEN)];
        snapshot.read_exact_at(payload_end, &mut footer)?;
        let decoded = decode_record_footer(spec, &footer, payload_end, record_offset)?;
        entry.footer_offset = Some(payload_end);
        entry.prev_same_block_offset = decoded.prev_same_block_offset;
        entry.prev_same_key_offset = decoded.prev_same_key_offset;
        entry.mutable_flags = decoded.mutable_flags;
    }
    entry.validate_payload_extent(snapshot.len())?;
    Ok(entry)
}

fn append_log_start_for_file(file: &VarlitFile) -> Result<u64> {
    // From the file's own region, not from `spec`: a block this build does not
    // know is skipped at open rather than refused, so a file can carry a longer
    // header than this build writes. Deriving the boundary from `spec` would
    // put the append log inside that block's bytes.
    let extension_len = u64::try_from(file.header_extensions.len()).map_err(|_| {
        Error::ResourceArithmeticOverflow {
            resource: "file-header length",
        }
    })?;
    Ok(native_file_header_len(file.spec, extension_len))
}

include!("file/rewrite.rs");

include!("file/merge.rs");

include!("file/key_payloads.rs");

include!("file/record_io.rs");

#[cfg(test)]
#[path = "file/tests.rs"]
mod tests;

/// Unit tests for private witness construction and refused reservations.
#[cfg(test)]
mod bypass_catalogue {
    use super::*;

    /// A directory over an empty file. The token tests never produce an entry,
    /// so what backs it never has to hold a record.
    fn empty_index() -> (tempfile::TempDir, ResidentIndex) {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("empty.varlit");
        std::fs::write(&path, b"").expect("create");
        let snapshot = SnapshotFile::new(File::open(&path).expect("open")).expect("snapshot");
        let spec = FormatSpec::new(
            b"VSBYPS",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            &[],
        );
        let index = ResidentIndex::empty(snapshot, spec);
        (directory, index)
    }

    /// The legitimate route still works, and still costs the reservation: the
    /// only producer of the token is the fallible half of the append.
    #[test]
    fn the_checked_route_still_produces_the_reservation_it_should() {
        let (_directory, mut index) = empty_index();
        let slot = index.reserve(|| Ok(64)).expect("reservation");
        index.install(slot, &sample_index_entry());
        assert_eq!(index.len(), 1);
    }

    /// And a charge that refuses yields no token at all, so the append that
    /// would have followed it cannot be spelled.
    #[test]
    fn a_refused_charge_yields_no_reservation() {
        let (_directory, mut index) = empty_index();
        assert!(
            index
                .reserve(|| Err(Error::AllocationFailed {
                    resource: "record index",
                    requested: 1,
                }))
                .is_err()
        );
        assert!(index.is_empty());
    }

    fn sample_index_entry() -> RecordIndexEntry {
        RecordIndexEntry {
            block_id: 1,
            block_version: 1,
            flags: 0,
            sequence: 1,
            record_offset: 0,
            payload_offset: 0,
            payload_len: 0,
            checksum: 0,
            uncompressed_len_hint: 0,
            footer_offset: None,
            prev_same_block_offset: None,
            prev_same_key_offset: None,
            mutable_flags: 0,
            committed: true,
        }
    }
}

impl FileMutationPermit<'_> {
    /// Overwrites an already-indexed record's header and payload in place.
    ///
    ///  and , mechanical enforcement. This function holds no addressing
    /// and no invalidation of its own: it takes a [`ReplacementTarget`], whose
    /// only constructor performed the `T::VERSION` refusal, and turns it into a
    /// [`RecordOverwrite`], whose only constructor drops the target block's
    /// resident keyed-tail map. The write is then performed by
    /// `RecordFile::overwrite_indexed_record`, which consumes the
    /// [`RecordOverwrite`] by value and is the only route to those bytes,
    /// because the raw handle is unnameable outside `mod record_file`.
    ///
    /// So the two properties below are not properties of this function that a
    /// sibling could fail to repeat; they are properties of *addressing an
    /// already-indexed record at all*:
    ///
    /// * The invalidation runs **before** the first byte reaches disk, so it
    ///   also covers a write that fails half-way, a poisoned writer that is
    ///   later inspected, and a caller that violates the same-key contract.
    /// * It is infallible and allocation-free (a `HashMap::remove`), so it
    ///   introduces no fallible step of its own, in either direction of
    ///   invariant 3.
    ///
    /// Round 12 stated the same guarantee for a function that a new seek/write
    /// pair elsewhere in this file could simply bypass, and re-verification
    /// compiled exactly that bypass. The bypass no longer builds.
    fn overwrite_record_bytes_in_place(
        &mut self,
        target: ReplacementTarget,
        header_bytes: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        self.check()?;
        // Both preconditions are discharged by constructing the permission:
        // the version refusal happened in `ReplacementTarget::resolve`, and
        // `prepare` drops the keyed-tail map before the write below.
        let writer: &mut VarlitFile = self;
        let write = RecordOverwrite::prepare(target, &mut writer.keyed_tails)?;
        writer
            .file
            .overwrite_indexed_record(write, header_bytes, payload)
    }
    /// Appends the user record encoded at `buffer[room..]`; see
    /// [`prepare_user_record_payload_in_room`] for what the room is for.
    fn write_user_record(
        &mut self,
        block_id: u32,
        block_version: u16,
        kind: BlockKind,
        buffer: &mut [u8],
        room: usize,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        self.check()?;
        let payload = prepare_user_record_payload_in_room(self.spec, block_id, kind, buffer, room)?;
        self.write_record_with_prev_key(
            block_id,
            block_version,
            payload.flags,
            payload.uncompressed_len_hint,
            &payload.bytes,
            prev_same_key_offset,
        )
    }
    /// The crate's single record-append core.
    ///
    /// Shape B, mechanical enforcement. It used to call
    /// [`VarlitFile::ensure_write`] itself, which is a convention: a sibling append
    /// helper written next month could seek and `write_all` without one, which
    /// is precisely the defect  was in `stream.rs`. It now demands the
    /// [`MutationPermit`] instead, so the check is the caller's precondition
    /// and the compiler enforces that every route to an append has made it.
    fn commit_scope_initialized(&self) -> bool {
        self.commit_scope.initialized
    }

    /// One barrier per handle: what an earlier handle left unsynced is made
    /// durable before the newest marker becomes this handle's anchor.
    fn initialize_commit_scope(&mut self) -> Result<()> {
        self.file.flush()?;
        self.file.sync_all()?;
        let anchor = self.block_tails.tail(COMMIT_BLOCK_ID);
        self.commit_scope.anchor_at(anchor);
        Ok(())
    }

    fn commit_scope_marker(&self) -> scan::CommitMarkerV2 {
        self.commit_scope.marker()
    }

    fn write_record_with_prev_key(
        &mut self,
        block_id: u32,
        block_version: u16,
        flags: u16,
        uncompressed_len_hint: u32,
        payload: &[u8],
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        self.write_record_payload(
            block_id,
            block_version,
            flags,
            uncompressed_len_hint,
            AppendPayload::Bytes(payload),
            prev_same_key_offset,
        )
    }

    /// Appends an in-place matrix record whose payload is `prefix` followed
    /// by zeros up to `len` bytes, written without a buffer of that size.
    pub(super) fn append_zeroed_matrix_record(
        &mut self,
        block_version: u16,
        prefix: &[u8],
        len: u64,
    ) -> Result<AppendInfo> {
        self.write_record_payload(
            MATRIX_TILE_BLOCK_ID,
            block_version,
            RECORD_FLAG_INTERNAL | RECORD_FLAG_IN_PLACE,
            0,
            AppendPayload::Zeroed { prefix, len },
            None,
        )
    }

    fn write_record_payload(
        &mut self,
        block_id: u32,
        block_version: u16,
        flags: u16,
        uncompressed_len_hint: u32,
        payload: AppendPayload<'_>,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        self.check()?;
        let payload_len = payload.len()?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
        let logical_len = match payload {
            AppendPayload::Bytes(bytes) => record_logical_len_from_parts(
                self.spec,
                block_id,
                flags,
                uncompressed_len_hint,
                bytes,
            )?,
            // Only in-place records, never compressed or chunked, are
            // appended zeroed: their logical length is their length.
            AppendPayload::Zeroed { .. } => {
                if flags & RECORD_FLAG_IN_PLACE == 0
                    || flags & (RECORD_FLAG_COMPRESSED | RECORD_FLAG_CHUNKED) != 0
                {
                    return Err(Error::InvalidCanonicalEncoding(
                        "zeroed append of a record that is not in place",
                    ));
                }
                payload_len
            }
        };
        self.spec
            .read_limits
            .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;
        if self.spec.commit_policy.single_barrier() && !self.commit_scope.initialized {
            self.initialize_commit_scope()?;
        }

        let record_count =
            self.index
                .len()
                .checked_add(1)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "record count",
                })?;
        self.spec.read_limits.check(
            ReadLimitKey::Records,
            u64::try_from(record_count).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?,
        )?;
        // Shape A, mechanical enforcement. The limit charge and the
        // mirror reservation are a precondition of the write below, and the
        // token they produce is the only thing that can install the entry
        // afterwards. See `mod reserved_index_slot`.
        let spec = self.spec;
        // H1, and the only per-block branch the index append has ever had.
        // Internal records are always resident: the manifest, commit markers,
        // checkpoints and segments are varlit's own bookkeeping and every open
        // path expects to find them.
        let resident = block_id >= RESERVED_BLOCK_ID_START || spec.block_is_resident(block_id);
        let index_slot = self.index.reserve(|| index_bytes_for_count(record_count))?;

        let sequence = self.sequence_state.available()?;
        let snapshot = AppendSnapshot {
            // PERF: the end of file is state this writer already maintains, so
            // asking the kernel for it once per record bought nothing. Every
            // construction of `self.snapshot` seeds it from the PHYSICAL length
            // (`SnapshotFile::new` at create, at `open_locked`, at
            // `open_recover_locked` and at both rebind sites) and every
            // successful append rewrites it from the witness the write itself
            // produced; `rollback_append` truncates back to the same `eof` it
            // started from and leaves `self.snapshot` alone.
            //
            // This is not blind trust. `RecordFile::append_record_at_end` still
            // performs its own `seek(SeekFrom::End(0))` and refuses any offset
            // that is not the one budgeted here, so a value that ever drifted
            // yields the existing typed refusal and a rollback — never a write
            // over live data. Removing that check along with the syscall is the
            // positional-write redesign, and is deliberately not what this is.
            eof: self.snapshot.len(),
            sequence_state: self.sequence_state,
            index_len: self.index.len(),
            segment_cursor: self.segment_cursor,
            block_id,
            previous_block_tail: self.block_tails.tail(block_id),
            uncommitted_since_commit: self.uncommitted_since_commit,
            appended_since_digest: self.appended_since_digest,
            commit_scope: self.commit_scope,
            last_resident_block_id: self.last_resident_block_id,
        };
        let record_offset = snapshot.eof;
        let payload_offset = record_offset.checked_add(RECORD_HEADER_LEN).ok_or(
            Error::ResourceArithmeticOverflow {
                resource: "file length",
            },
        )?;
        let payload_end =
            payload_offset
                .checked_add(payload_len)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "file length",
                })?;
        let prospective_len = payload_end
            .checked_add(record_footer_len(self.spec))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "file length",
            })?;
        // No file-length ceiling here. `max_file_len` refuses on a number
        // rather than on a resource: the scan is bounded incrementally by
        // `ScanBytes`, the index by `Records` and `IndexBytes`, and a mapping
        // by `MmapLen`, each against the thing it actually consumes. A length
        // check on top of those refuses to touch a file whose data is already
        // there, and on the append path it bounds nothing at all — appending to
        // a 1 TB file costs exactly what appending to a 1 GB file costs.
        // the predecessor comes from the maintained tail table, not
        // from a reverse scan of the resident index.
        let prev_same_block_offset = if self.spec.index_policy.block_offset_chain {
            self.block_tails.tail(block_id)
        } else {
            None
        };
        let prev_same_key_offset = if self.spec.index_policy.keyed_offset_chain {
            prev_same_key_offset
        } else {
            None
        };
        let footer = if self.spec.spec_needs_record_footer() {
            Some(encode_record_footer(RecordFooterFields {
                prev_same_block_offset,
                prev_same_key_offset,
                // A record being appended is alive by construction.
                mutable_flags: 0,
            })?)
        } else {
            None
        };
        let header = RecordHeaderFields {
            block_id,
            block_version,
            flags,
            sequence,
            payload_len,
            checksum: 0,
            uncompressed_len_hint,
        };
        let footer_bytes = footer.as_ref().map_or(&[][..], |bytes| &bytes[..]);
        // An in-place record's checksum leaves its payload out.
        let checksum = checksum_record_fields(
            self.spec,
            record_offset,
            header,
            payload.bytes(),
            footer_bytes,
        )?;
        let header_bytes = encode_native_record_header(
            RecordHeaderFields { checksum, ..header },
            record_offset,
            record_footer_len(self.spec),
        )?;
        let footer_offset = if footer.is_some() {
            payload_end
                .checked_add(RECORD_FOOTER_LEN)
                .ok_or(Error::LengthOverflow { value: payload_len })?;
            Some(payload_end)
        } else {
            None
        };
        // The append is the second and last route to record bytes on disk
        // (`mod record_file`). It seeks to the end itself and refuses an
        // offset that is not the one budgeted above, so this path cannot land
        // inside an already-indexed record even if `record_offset` were wrong.
        // What an anchor this record reaches records for an open to resume
        // from: the state before this record, known only to a handle that
        // knows the whole file's record count.
        let writer: &mut VarlitFile = &mut *self;
        let summary = WriterSummary {
            tails: &writer.block_tails,
            records_before: writer.index.whole_file_count(),
            sequence_high_water: sequence.checked_sub(1),
            segment: &writer.segment_cursor,
        };
        let write_result = writer.file.append_record_at_end(
            &index_slot,
            record_offset,
            &header_bytes,
            payload,
            footer.as_ref().map(|bytes| &bytes[..]),
            Some(&summary),
            || {
                #[cfg(test)]
                fail_append_after_header_if_requested()?;
                Ok(())
            },
        );
        let written = match write_result {
            Ok(written) => written,
            Err(error) => return Err(self.rollback_append(snapshot, error)),
        };

        // The budgeted extent and the extent actually written are computed
        // independently — one from the header/payload/footer lengths above, the
        // other from the offset `SEEK_END` returned plus the bytes `write_all`
        // took — so they agreeing is a real check on both.
        debug_assert_eq!(written.end_offset(), prospective_len);
        // The write above is the proof that the file reaches this offset, so
        // the rebind needs no `fstat` of its own — that was the second
        // per-record metadata syscall on this path. `with_written_len` is still
        // fallible and still growth-only, so the rollback arm is unchanged.
        let new_snapshot = match self.snapshot.with_written_len(written) {
            Ok(snapshot) => snapshot,
            Err(error) => return Err(self.rollback_append(snapshot, error)),
        };

        let committed =
            !self.spec.commit_policy.is_transaction_marker() || block_id == COMMIT_BLOCK_ID;
        let entry = RecordIndexEntry {
            block_id,
            block_version,
            flags,
            sequence,
            record_offset,
            payload_offset,
            payload_len,
            checksum,
            uncompressed_len_hint,
            footer_offset,
            prev_same_block_offset,
            prev_same_key_offset,
            // Appended, therefore alive.
            mutable_flags: 0,
            committed,
        };
        let info = AppendInfo::from(&entry);
        self.index
            .install_verified_boundary(record_offset, prospective_len);
        // A non-resident block's record is written, sequenced, chained and
        // recoverable exactly as any other; it is simply not mirrored in
        // memory, which is what stops resident cost tracking record count.
        //
        // Three things are deliberately *not* skipped with it. Sequences are
        // file-global and gapless, so `publish_sequence` below runs either way.
        // `BlockTails` is what makes the footer chain walkable and is
        // `O(blocks)`, not `O(records)` - it is the only way back to a
        // non-resident record, so it is maintained for every block. And the
        // record's own footer chain went to disk above, not to memory.
        if resident {
            self.last_resident_block_id = Some(block_id);
            self.index.install(index_slot, &entry);
            // Single index append site: keep the segment cursor in lockstep
            // with the resident index. It counts resident entries, so a
            // non-resident record must not advance it.
            let ordinal = self.index.len() - 1;
            self.segment_cursor
                .note_appended(ordinal, block_id, prospective_len);
        } else {
            drop(index_slot);
        }
        self.block_tails.note_appended(block_id, record_offset);
        if self.spec.commit_policy.single_barrier() {
            self.commit_scope.note(&entry);
        }
        // The marker itself clears the flag; a segment record is written after
        // the marker it certifies and must not set it again, or a repeated idle
        // flush would write marker, segment, marker, segment forever.
        self.uncommitted_since_commit = match block_id {
            COMMIT_BLOCK_ID => false,
            // A digest is written after the marker it describes, for the same
            // reason a segment is, so it needs the same exemption. Without it
            // the digest sets the flag, the next idle flush writes a marker
            // because the flag is set, and the file grows forever on a handle
            // nothing is appending to.
            SEGMENT_BLOCK_ID | OPEN_DIGEST_BLOCK_ID => self.uncommitted_since_commit,
            _ => true,
        };
        // Maintained for every record, resident or not — that is the whole
        // point of it. The digest is the one record that clears it: it is
        // written last and describes everything before it, including itself.
        self.appended_since_digest = block_id != OPEN_DIGEST_BLOCK_ID;
        self.snapshot = new_snapshot;
        // The directory's entries are rebuilt from the file, and the record
        // just installed is past the end of the snapshot this handle held a
        // moment ago — `SnapshotFile` refuses a read beyond its logical length
        // rather than serving it, so without this the entry for the newest
        // record would be unreadable until something else rebound. One `Arc`
        // refcount bump and a `u64` copy: no allocation and no syscall, which
        // is what lets it sit here.
        let snapshot = self.snapshot.clone();
        self.index.rebind(snapshot);
        self.publish_sequence(sequence);
        Ok(info)
    }
    /// The one place the region reaches the disk.
    ///
    /// Writes the payload at its fixed offset and syncs before updating the
    /// cached copy, so a crash mid-write leaves the file and this handle
    /// agreeing about which generation of the region is on disk.
    fn commit_header_slots(
        &mut self,
        extent: MutableHeaderExtent,
        used: usize,
        entries: &[u8],
        sealed: bool,
    ) -> Result<()> {
        self.check()?;
        let payload = encode_header_slots_payload(self.spec, used, entries, sealed)?;
        self.commit_header_slots_payload(extent, &payload)
    }

    fn commit_header_slots_payload(
        &mut self,
        extent: MutableHeaderExtent,
        payload: &[u8],
    ) -> Result<()> {
        self.check()?;
        if payload.len() != extent.payload_len {
            // A region this build did not write: its geometry is not the one
            // encoded here, and overwriting it would corrupt whatever it is.
            return Err(Error::InvalidFormatSpec(
                "header_slots region length disagrees with the declaration",
            ));
        }
        self.file
            .overwrite_header_region(extent.payload_offset, payload)?;
        self.file.sync_data()?;
        self.header_extensions
            [extent.payload_in_extensions..extent.payload_in_extensions + extent.payload_len]
            .copy_from_slice(payload);
        Ok(())
    }
}

#[cfg(test)]
#[path = "file/validation_tests.rs"]
mod validation_tests;

#[path = "file/header_codec.rs"]
mod header_codec;
pub(crate) mod record_chunks;
mod record_fields;
mod record_read;
mod record_view;
use header_codec::*;
pub(crate) use record_fields::decode_record_into;
#[doc(hidden)]
pub use record_fields::{FieldsCodec, RecordField, RecordFields};
pub(crate) use record_read::{
    ChunkCache, ScratchLoan, fill_record, lend_scratch, lend_window, return_scratch, return_window,
};
pub use record_read::{RecordCursor, RecordPage};
pub use record_view::{
    FIELD_INLINE, FieldSpan, FieldWalk, VarlitView, view_set, view_wire_mismatch,
};

pub(crate) use header_codec::file_header_extensions;
pub(crate) use header_codec::published_end_slots_at;

pub(crate) use header_codec::header_tails_region_len;

pub(crate) use header_codec::blank_mutable_header_bytes;

#[path = "file/matrix_sidecar.rs"]
mod matrix_sidecar;
use matrix_sidecar::*;

#[path = "file/record_checksum.rs"]
mod record_checksum;
use record_checksum::*;

#[path = "file/scan.rs"]
mod scan;
#[cfg(any(test, feature = "scalable-fault-injection"))]
pub(crate) use scan::take_open_scan_bytes;
#[cfg(any(test, feature = "scalable-fault-injection"))]
pub(crate) use scan::take_record_entry_faults;
use scan::*;

pub(crate) use scan::NativeStreamScanner;
#[cfg(test)]
pub(crate) use scan::reset_stream_io_counters;
#[cfg(test)]
pub(crate) use scan::stream_io_counters;

// Reads a record a pointer names, validated against the snapshot: the
// lookup path of bounded keyed storage (docs/design/bounded-keyed-storage.md).
#[allow(unused_imports)]
pub(crate) use scan::read_stream_entry_at;

pub(crate) use scan::PreparedStreamRecord;

#[cfg(test)]
pub(crate) use scan::prepare_stream_user_record;
pub(crate) use scan::prepare_stream_user_record_into;

pub(crate) use scan::prepare_stream_tombstone_record;

pub(crate) use scan::prepare_stream_manifest_record;

pub(crate) use scan::fresh_stream_creation_nonce;

pub(crate) use scan::prepare_stream_creation_nonce_record;
pub(crate) use scan::prepare_stream_sync_record;

pub(crate) use scan::read_stream_creation_nonce;

#[path = "file/publication.rs"]
mod publication;
#[cfg(unix)]
pub(crate) use publication::opened_file_identity;
#[cfg(windows)]
pub(crate) use publication::opened_file_identity;
use publication::*;

pub(crate) use publication::PinnedObject;

pub(crate) use publication::create_rewrite_temp_file;

pub use publication::ReplacePublicationFailure;

pub use publication::classify_replace_publication_error;

pub(crate) use publication::ReplaceDurability;

pub(crate) use publication::publish_temp_path_atomically;

pub(crate) use publication::publish_open_temp_path_atomically;
#[cfg(not(windows))]
pub(crate) use publication::replace_path_atomically;
#[cfg(windows)]
pub(crate) use publication::replace_path_atomically;
#[cfg(unix)]
pub(crate) use publication::sync_parent_directory;
#[cfg(windows)]
pub(crate) use publication::sync_parent_directory;

#[path = "file/locks.rs"]
mod locks;
#[cfg(unix)]
#[cfg(feature = "experimental-matrix-reuse")]
pub(crate) use locks::verify_dedicated_lock_marker;
#[cfg(windows)]
#[cfg(feature = "experimental-matrix-reuse")]
pub(crate) use locks::verify_dedicated_lock_marker;
#[cfg(not(any(unix, windows)))]
#[cfg(feature = "experimental-matrix-reuse")]
pub(crate) use locks::verify_dedicated_lock_marker;
use locks::*;

pub(crate) use locks::WriterLock;

pub(crate) use locks::WriterGuardLockError;
#[cfg(not(windows))]
pub(crate) use locks::try_lock_native_guard;
#[cfg(windows)]
pub(crate) use locks::try_lock_native_guard;
#[cfg(not(windows))]
pub(crate) use locks::unlock_native_guard;
#[cfg(windows)]
pub(crate) use locks::unlock_native_guard;

pub use locks::clear_stale_writer_lock;

pub(crate) use locks::with_writer_lock_value;

#[cfg(feature = "mmap")]
#[path = "file/mmap_views.rs"]
mod mmap_views;
#[cfg(feature = "zero-copy")]
pub use mmap_views::RawView;
#[cfg(feature = "mmap")]
use mmap_views::*;
#[cfg(feature = "mmap")]
pub use mmap_views::{MmapMatrix, MmapPayloadWindow, MmapPayloads};

impl crate::TryClone for RecordIndexEntry {
    fn try_clone(&self) -> Result<Self> {
        // All fields are integers, booleans, or Option<u64>; no owned heap data.
        Ok(self.clone())
    }
}
