use crate::TryClone;
#[path = "matrix_bulk.rs"]
mod matrix_bulk;

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
    ManifestPolicy, MatrixCellStatus, MatrixCommitEvent, MatrixDimensions, MatrixKey,
    MatrixRecoveryAction, MatrixRecoveryReport, MatrixResumeSignal, RecoveryPolicy, Result,
    SnapshotFile, VariableCompression, VarlitBlock, VarlitEncode, VarlitKeyedBlock,
    VarlitMatrixBlock, VarlitMerge, VarlitMigration, VarlitReplaceBlock, WireType,
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
/// Carries the per-create nonce of a stream/indexed primary.
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
];

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
const RECORD_KNOWN_FLAGS: u16 = RECORD_FLAG_COMPRESSED | RECORD_FLAG_INTERNAL;
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
    let prefix_len = DIGEST_PREFIX_LEN as usize;
    let trailer_len = DIGEST_TRAILER_LEN as usize;
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
        .checked_mul(DIGEST_TAIL_LEN as usize)
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
        let at = prefix_len + index * DIGEST_TAIL_LEN as usize;
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
/// A chunk is what makes a matrix dimension grow past the extent declared at
/// create. Chunk 0 *is* the matrix region; chunk `k > 0` is a record with the
/// region's layout. See [`crate::GrowingMatrixDimension`].
pub const MATRIX_CHUNK_BLOCK_ID: u32 = 0xFFFF_FFF6;
const MATRIX_CHUNK_MAGIC: &[u8; 4] = b"VMCK";
const MATRIX_CHUNK_VERSION: u16 = 1;
/// Prefix flag: every block region is `commit map | crc table | slots`.
///
/// Set when the format's integrity policy is a crc32 one. A chunk read
/// otherwise has nothing to check: the record footer's crc covers the whole
/// payload and is verified on a *record* read, which a positional cell read is
/// not, so a single flipped bit in a written chunk came back as data. The matrix
/// region solves this with a per-cell checksum and so does a chunk.
const MATRIX_CHUNK_FLAG_CELL_CRC: u16 = 0x0001;
/// Prefix flag: every block's slot region is a sub-block index followed by
/// compressed sub-blocks, rather than plain cells.
///
/// A chunk is written whole, so an unfilled one costs its full size — measured
/// identical at 5% and 100% filled. Only the slot regions are compressed: the
/// prefix, the descriptors, the commit maps and the checksum tables stay plain,
/// so addressing a cell, reading its commit bit and checking its checksum are
/// unchanged, and only the `stride`-byte positional read becomes a sub-block
/// decode.
const MATRIX_CHUNK_FLAG_COMPRESSED_SLOTS: u16 = 0x0002;
const MATRIX_CHUNK_KNOWN_FLAGS: u16 =
    MATRIX_CHUNK_FLAG_CELL_CRC | MATRIX_CHUNK_FLAG_COMPRESSED_SLOTS;
/// Uncompressed bytes a slot sub-block covers, before rounding to whole cells.
///
/// A cell's sub-block is `ordinal / cells_per_sub_block(stride)` — arithmetic,
/// not a search — and both sides derive the divisor from `stride` alone so they
/// cannot disagree.
const MATRIX_CHUNK_SUB_BLOCK_BYTES: u64 = 8192;
/// One `u32` offset per sub-block, plus a terminator, so a sub-block's extent
/// is two reads and no prefix sum.
const MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN: u64 = 4;
/// Bytes of stored checksum per cell.
const MATRIX_CHUNK_CRC_LEN: u64 = 4;
/// magic(4) + version(2) + flags(2) + chunk_index(8) + first_row(8) + rows(8)
/// + block_count(4) + reserved(4).
const MATRIX_CHUNK_PREFIX_LEN: u64 = 4 + 2 + 2 + 8 + 8 + 8 + 4 + 4;
/// block_id(4) + reserved(4) + stride(8) + cells(8) + commit_len(8).
///
/// Every block's descriptor precedes every block's data, so a reader computes
/// any block's slot offset from the descriptors alone — one small positional
/// read, whatever the chunk's size. That is what keeps a cell read from
/// materialising the chunk.
const MATRIX_CHUNK_BLOCK_DESC_LEN: u64 = 4 + 4 + 8 + 8 + 8;
/// Descriptor bytes a cell read keeps on the stack.
///
/// Eighteen blocks. A chunked cell read must not allocate — that is the whole
/// point of addressing a chunk by arithmetic — and every format anyone has
/// declared fits well inside this.
const MATRIX_CHUNK_INLINE_DESCRIPTOR_BYTES: usize = 512;
/// Smallest live-tail growth that forces a fresh full index checkpoint.
///
/// Below this floor the geometric-spacing rule in [`VarlitFile::needs_index_checkpoint`]
/// would fire on nearly every flush, reintroducing the cumulative O(N^2)
/// checkpoint bytes that  removes. The floor keeps small files cheap to
/// recover while capping the per-flush overhead of a rapidly growing append log.
const INDEX_CHECKPOINT_MIN_RECORDS: usize = 16;
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
const HEADER_TAILS_RESERVED_ENTRIES: usize = INTERNAL_BLOCK_IDS.len();
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
// Per-create identity region stamped between the native file header and the
// matrix layout header of every matrix file. `create_with_dims`
// reuses the OS file object when the pathname already exists, so OS identity,
// schema hash and layout offsets are all stable across a same-dims recreate;
// the nonce is the only value that distinguishes the new logical matrix from
// the one every existing sidecar was published against.
const MATRIX_CREATION_NONCE_MAGIC: &[u8; 4] = b"VMNC";
const MATRIX_CREATION_NONCE_VERSION: u16 = 1;
const MATRIX_CREATION_NONCE_LEN: usize = 16;
// Magic (4) + version (2) + reserved (2) + nonce (16).
const MATRIX_CREATION_NONCE_REGION_LEN: usize = 8 + MATRIX_CREATION_NONCE_LEN;
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

/// Optional barriers for a durable matrix cell write. The file is the physical
/// generation store, not the logical matrix image: only synchronize it, do not
/// seek to matrix offsets or modify it. Both callbacks precede generation-head
/// publication. The notification hook runs only after publication and sync.
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

use record_file::RecordFile;

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

/// Which route [`VarlitFile::open_readonly_lazy`] actually took.
///
/// Both open the same file and answer the same reads. They differ by roughly
/// four orders of magnitude in syscalls, which is a difference a caller that
/// asked for the lazy open is entitled to notice rather than infer from a
/// stopwatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LazyOpenSource {
    /// The tail table in the *file header* was read and the file corroborated
    /// it. A handful of records were framed — the commit marker it names, what
    /// follows that marker, and one per block tail — and no more.
    ///
    /// The route [`IndexPolicy::header_tails`] exists for: unlike the digest,
    /// nothing appended to the file can move this table or bury it.
    HeaderTails,
    /// The digest at the end of the file was read. No record was framed.
    Digest,
    /// No usable digest, so every record was framed. Correct, and expensive.
    FullScan,
}

/// Whether an open keeps a record directory of its own.
///
/// Not a policy on the format: two callers of the same file want different
/// answers, and the one that wants none has to take the directory in the same
/// call or it could not read positionally at all. See
/// [`VarlitFile::open_readonly_without_directory`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectoryRetention {
    Keep,
    None,
}

/// Where a read resolves a record *position* to a record.
///
/// Every position-based read — `blocks`, `scan`, `keyed_blocks`, `metadata`,
/// `verify_all` and the rest — needs exactly two things from a directory: how
/// many records there are, and the entry at position `i`. Nothing else. Making
/// that a parameter rather than a field is what lets the directory stop being
/// the handle's business without any of those reads stopping working.
///
/// Two implementations, and they are the two answers to "who holds it":
///
/// * `ResidentIndex` — varlit holds it, and `record_at` faults the entry off
///   disk from a 16-byte slot.
/// * `[RecordIndexEntry]` — the host holds it, as
///   [`VarlitFile::index_entries_into`] or
///   [`VarlitFile::open_readonly_with_scratch`] left it. `record_at` is a
///   memcpy.
///
/// The second is the point, and it is stated plainly because the alternative
/// was nearly shipped: a handle that keeps no directory must not *lose* these
/// reads, it must *demand the directory from whoever has it*. That is the
/// difference between a capability with a cost and a capability withdrawn.
pub trait RecordDirectory {
    /// How many records this directory describes.
    fn record_count(&self) -> usize;

    /// The entry at `position`, or `UnexpectedEof` if there is none.
    fn record_at(&self, position: usize) -> Result<RecordIndexEntry>;

    /// The block id at `position`, when it can be told without building the
    /// entry. `None` means "ask `record_at`" and is always a correct answer.
    ///
    /// This exists for walks that want one block out of a directory. Producing
    /// an entry can read the file — the resident index rebuilds each one from
    /// its record header — so a walk that filters *after* `record_at` pays for
    /// every record in the file to keep the few that match. Directories that
    /// hold whole entries answer from those; the resident index answers from
    /// the field its slot carries; the default answers `None`, which costs a
    /// filtering caller nothing beyond what it already did.
    fn block_id_at(&self, _position: usize) -> Option<u32> {
        None
    }
}

/// Every entry of `dir` in order, fallible per item for the same reason
/// [`ResidentIndex::iter`] is: producing one may read a file.
///
/// A free function rather than a trait method with a default body, because a
/// `-> impl Iterator` method forces `Self: Sized` and the most useful directory
/// of all is `&[RecordIndexEntry]`, which is not.
pub fn directory_records<D: RecordDirectory + ?Sized>(
    dir: &D,
) -> impl Iterator<Item = Result<RecordIndexEntry>> + '_ {
    (0..dir.record_count()).map(|position| dir.record_at(position))
}

/// Whether the record at `position` can be passed over without building its
/// entry, given the block ids the caller is looking for.
///
/// Every walk that wants one block used to filter *after* `record_at`, which
/// was free when the resident index held whole entries and stopped being free
/// when it became one offset and one block id per record: producing an entry
/// is now a positional read of that record's header. A walk over a 600,000
/// record file that keeps five of them was reading 600,000 headers.
///
/// `false` when the directory cannot say (`block_id_at` returned `None`) —
/// the caller then does exactly what it did before, and its own check after
/// `record_at` is still the one that decides. This is an optimisation with no
/// authority: it never admits a record the caller's own filter would reject.
fn directory_skips<D: RecordDirectory + ?Sized>(dir: &D, position: usize, wanted: &[u32]) -> bool {
    match dir.block_id_at(position) {
        Some(id) => !wanted.contains(&id),
        None => false,
    }
}

impl RecordDirectory for ResidentIndex {
    fn record_count(&self) -> usize {
        self.len()
    }

    fn record_at(&self, position: usize) -> Result<RecordIndexEntry> {
        self.entry_at(position)
    }

    fn block_id_at(&self, position: usize) -> Option<u32> {
        ResidentIndex::block_id_at(self, position)
    }
}

impl RecordDirectory for [RecordIndexEntry] {
    fn record_count(&self) -> usize {
        self.len()
    }

    fn record_at(&self, position: usize) -> Result<RecordIndexEntry> {
        self.get(position).cloned().ok_or(Error::UnexpectedEof)
    }

    fn block_id_at(&self, position: usize) -> Option<u32> {
        self.get(position).map(|entry| entry.block_id)
    }
}

impl RecordDirectory for Vec<RecordIndexEntry> {
    fn record_count(&self) -> usize {
        self.len()
    }

    fn record_at(&self, position: usize) -> Result<RecordIndexEntry> {
        self.as_slice().record_at(position)
    }
}

/// A record index the caller owns the memory for, walked only as far as the
/// caller's question needed.
///
/// [`VarlitFile::index_entries_into`] answers one question — "give me the whole
/// index" — and it answers it the same way whatever was actually being looked
/// for: `N` entries and two positional reads per record, to the end of the
/// file. This answers "walk until you find it, then stop". A map that finds its
/// record at position 12 of a million has read twelve records.
///
/// Three things make it the caller's rather than varlit's:
///
/// * **The buffer comes in.** `record_map` takes `&mut Vec<RecordIndexEntry>`
///   and never allocates one. Nothing is copied out of it either — the map
///   *is* the buffer, viewed.
/// * **The walk resumes.** A second question continues from where the first
///   stopped, so asking twice costs the records between the two answers, not
///   two walks. [`find`](Self::find) searches what is already framed before it
///   reads anything at all.
/// * **The caller decides when it goes away.** [`clear`](Self::clear) empties
///   it and rewinds; [`release`](Self::release) empties it and keeps walking
///   forward, which is how a bounded-memory pass over a file larger than RAM is
///   written. Dropping the map hands the buffer straight back.
///
/// It implements [`RecordDirectory`], so a map is a directory: everything
/// [`VarlitFile::with_directory`] serves — `blocks`, `scan`, `keyed_blocks`, the
/// rest — is answered against the prefix walked so far. That is what the trait
/// being the currency, rather than `Vec`, buys.
///
/// ```ignore
/// let mut buffer = Vec::new();
/// let mut map = file.record_map(&mut buffer)?;         // no read yet
/// let hit = map.find(|entry| entry.block_id == Note::ID)?;
/// if let Some(entry) = hit {
///     let note: Note = file.decode_block_into(&entry, &mut payload)?;
/// }
/// let points = file.with_directory(&map)?.block_entries_into::<Note>(&mut out)?;
/// map.clear();                                         // the caller's call
/// ```
///
/// **What it walks, and what it therefore cannot see.** Records in file order,
/// from the start of the append log to the handle's snapshot end. That end is
/// already the committed, complete one — the open scan established it — so a
/// torn trailing record, and under a transaction-marker policy an uncommitted
/// tail, are outside the walk rather than filtered out of it. Non-resident
/// blocks are framed and skipped, exactly as the open scan skips them, so a
/// completed map equals [`VarlitFile::index_entries`] entry for entry.
///
/// **What a partial index does not check.** A full open validates sequence
/// uniqueness across every record in the file. A map validates nothing beyond
/// the prefix it has read, because that is all it has read: a duplicate
/// sequence past the stopping point is not detected until a walk reaches it.
/// That is the cost of not reading the rest, and it is the only correctness
/// difference between a completed map and an open scan.
pub struct RecordMap<'a> {
    entries: &'a mut Vec<RecordIndexEntry>,
    spec: FormatSpec,
    snapshot: SnapshotFile,
    /// Where the append log begins, and where [`Self::clear`] rewinds to.
    start: u64,
    /// The offset the next [`Self::advance`] will frame.
    next: u64,
    /// One past the last byte the walk may touch: the handle's snapshot end.
    end: u64,
    accounting: ScanAccounting,
}

impl<'a> RecordMap<'a> {
    fn new(file: &VarlitFile, entries: &'a mut Vec<RecordIndexEntry>) -> Result<Self> {
        let start = append_log_start_for_file(file)?;
        entries.clear();
        let mut accounting = ScanAccounting::default();
        // The header bytes the walk skips are charged the way the open scan
        // charges them, so a `ScanBytes` ceiling means the same number of bytes
        // whichever route reached the record.
        accounting.advance(file.spec, start)?;
        Ok(Self {
            entries,
            spec: file.spec,
            snapshot: file.snapshot.clone(),
            start,
            next: start,
            end: file.snapshot.len(),
            accounting,
        })
    }

    /// Frames the next record, appends it, and returns it; `None` at the end of
    /// the walk.
    ///
    /// One record, two positional reads — the header, and the footer if the
    /// format has one. Nothing else is touched: a record's payload is not read
    /// to frame it, whatever its size.
    pub fn advance(&mut self) -> Result<Option<RecordIndexEntry>> {
        loop {
            if self.next >= self.end {
                return Ok(None);
            }
            // Counted the way the open scan counts, because this is the same
            // unit of work: a header, an extent and a footer read to *build* an
            // index. `ResidentIndex::fault` deliberately does not count — it
            // rebuilds an entry of an index that already exists, and conflating
            // the two would make every `index_entries()` call look like a scan.
            note_record_framed();
            let entry = fault_record_entry(&self.snapshot, self.spec, self.next, true)?;
            let record_end =
                entry
                    .checked_physical_end()
                    .map_err(|_| Error::ResourceArithmeticOverflow {
                        resource: "record extent",
                    })?;
            // A record whose extent runs past the snapshot end is not a record
            // this handle may see. The open scan cannot produce one — its end
            // *is* a record boundary — so this is a torn-file guard, not a
            // routine branch, and it stops rather than reporting corruption
            // the scan already decided to tolerate.
            if record_end > self.end {
                self.next = self.end;
                return Ok(None);
            }
            self.accounting
                .advance(self.spec, record_end.saturating_sub(self.next))?;
            self.next = record_end;
            // Framed and skipped, exactly as the open scan skips it: a
            // non-resident block's records are on disk and not in the index.
            if !record_is_resident(self.spec, entry.block_id) {
                continue;
            }
            reserve_scanned_entry(self.spec, self.entries)?;
            self.entries.push(entry.clone());
            return Ok(Some(entry));
        }
    }

    /// Walks forward until `want` accepts a record, and stops there.
    ///
    /// Searches what is already framed first, so a question the last one
    /// already paid for costs no read at all.
    pub fn find(
        &mut self,
        mut want: impl FnMut(&RecordIndexEntry) -> bool,
    ) -> Result<Option<RecordIndexEntry>> {
        if let Some(found) = self.entries.iter().find(|entry| want(entry)) {
            return Ok(Some(found.clone()));
        }
        while let Some(entry) = self.advance()? {
            if want(&entry) {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    /// Walks until the map holds `count` entries, or the file ends first.
    ///
    /// Returns how many it holds, which is `count` unless the file ran out.
    pub fn fill_to(&mut self, count: usize) -> Result<usize> {
        while self.entries.len() < count {
            if self.advance()?.is_none() {
                break;
            }
        }
        Ok(self.entries.len())
    }

    /// Walks to the end of the file.
    ///
    /// This is [`VarlitFile::index_entries_into`] spelled as the case it is —
    /// the one where the answer really is every record — and it costs the same.
    pub fn fill(&mut self) -> Result<usize> {
        while self.advance()?.is_some() {}
        Ok(self.entries.len())
    }

    /// The records framed so far, in file order.
    pub fn entries(&self) -> &[RecordIndexEntry] {
        self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the walk has reached the end of the file.
    ///
    /// A complete map is the whole index; an incomplete one is a prefix, and
    /// every read answered against it is answered against that prefix.
    pub fn is_complete(&self) -> bool {
        self.next >= self.end
    }

    /// The file offset the next [`Self::advance`] will frame.
    pub fn resume_offset(&self) -> u64 {
        self.next
    }

    /// Empties the buffer and rewinds the walk to the start of the file.
    ///
    /// The map is then what [`VarlitFile::record_map`] just returned. Use
    /// [`release`](Self::release) to free the entries without giving up the
    /// ground already covered.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.next = self.start;
    }

    /// Empties the buffer but keeps the walk where it is.
    ///
    /// This is the bounded-memory pass: `fill_to(k)`, use them, `release()`,
    /// repeat. Memory is the caller's `k` entries however long the file is, and
    /// no record is read twice.
    pub fn release(&mut self) {
        self.entries.clear();
    }
}

impl RecordDirectory for RecordMap<'_> {
    fn record_count(&self) -> usize {
        self.entries.len()
    }

    fn record_at(&self, position: usize) -> Result<RecordIndexEntry> {
        self.entries.as_slice().record_at(position)
    }
}

impl std::fmt::Debug for RecordMap<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecordMap")
            .field("framed", &self.entries.len())
            .field("resume_offset", &self.next)
            .field("complete", &self.is_complete())
            .finish()
    }
}

/// An owned snapshot of a file's record index.
///
/// Derefs to `[RecordIndexEntry]`, so it reads exactly like the borrowed slice
/// this replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntries {
    entries: Vec<RecordIndexEntry>,
}

impl core::ops::Deref for IndexEntries {
    type Target = [RecordIndexEntry];

    fn deref(&self) -> &[RecordIndexEntry] {
        &self.entries
    }
}

impl IntoIterator for IndexEntries {
    type Item = RecordIndexEntry;
    type IntoIter = std::vec::IntoIter<RecordIndexEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a> IntoIterator for &'a IndexEntries {
    type Item = &'a RecordIndexEntry;
    type IntoIter = std::slice::Iter<'a, RecordIndexEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

/// A [`VarlitFile`]'s reads, resolved through a caller-supplied directory.
///
/// Built by [`VarlitFile::with_directory`]. Every method is the handle's method
/// of the same name, answered against the borrowed directory; nothing here
/// allocates a directory of its own.
#[derive(Debug)]
pub struct DirectoryRead<'a, D: RecordDirectory + ?Sized> {
    file: &'a VarlitFile,
    directory: &'a D,
}

impl<D: RecordDirectory + ?Sized> DirectoryRead<'_, D> {
    /// How many records the supplied directory describes.
    pub fn record_count(&self) -> usize {
        self.directory.record_count()
    }

    pub fn scan(&self) -> impl Iterator<Item = Result<BlockEvent>> + '_ {
        VarlitFile::scan_in(self.directory)
    }

    pub fn metadata(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.file.metadata_in(self.directory, key)
    }

    pub fn all_metadata_into(&self, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
        self.file.all_metadata_into_in(self.directory, out)
    }

    pub fn schema_manifest(&self) -> Result<Option<SchemaManifest>> {
        self.file.schema_manifest_in(self.directory)
    }

    pub fn blocks<T: VarlitBlock>(&self) -> Result<BlockVec<T>> {
        self.file.blocks_in(self.directory)
    }

    pub fn block_entries_into<T: VarlitBlock>(
        &self,
        out: &mut Vec<RecordIndexEntry>,
    ) -> Result<()> {
        self.file.block_entries_into_in::<T, _>(self.directory, out)
    }

    pub fn decode_blocks_into<T: VarlitBlock>(&self, out: &mut Vec<T>) -> Result<()> {
        self.file.decode_blocks_into_in::<T, _>(self.directory, out)
    }

    pub fn blocks_migrated_into<From, To, M>(&self, out: &mut Vec<To>) -> Result<()>
    where
        From: VarlitBlock,
        To: VarlitBlock,
        M: VarlitMigration<From, To>,
    {
        self.file
            .blocks_migrated_into_in::<From, To, M, _>(self.directory, out)
    }

    pub fn keyed_blocks_into<T>(
        &self,
        entries: &mut Vec<RecordIndexEntry>,
        by_key: &mut HashMap<T::Key, RecordIndexEntry>,
    ) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash + Clone,
    {
        self.file
            .keyed_blocks_into_in::<T, _>(self.directory, entries, by_key)
    }

    pub fn materialized_keyed_blocks_into<T>(&self, out: &mut HashMap<T::Key, T>) -> Result<()>
    where
        T: VarlitMerge,
        T::Key: Eq + Hash,
    {
        self.file
            .materialized_keyed_blocks_into_in::<T, _>(self.directory, out)
    }

    pub fn key_tail_offsets_into<T>(&self, out: &mut HashMap<T::Key, u64>) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file
            .key_tail_offsets_into_in::<T, _>(self.directory, out)
    }

    pub fn index_entries_into(&self, out: &mut Vec<RecordIndexEntry>) -> Result<()> {
        self.file.index_entries_into_in(self.directory, out)
    }

    pub fn verify_all(&self) -> Result<usize> {
        self.file.verify_all_in(self.directory)
    }

    /// Reads through an entry the caller holds; see
    /// [`VarlitFile::decode_block_into`]. Present here so a directory-supplied
    /// walk needs no second handle.
    pub fn decode_block_into<T: VarlitBlock>(
        &self,
        entry: &RecordIndexEntry,
        scratch: &mut Vec<u8>,
    ) -> Result<T> {
        self.file.decode_block_into::<T>(entry, scratch)
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RecordIndexEntry {
    pub block_id: u32,
    pub block_version: u16,
    pub flags: u16,
    pub sequence: u64,
    pub record_offset: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub checksum: u32,
    pub uncompressed_len_hint: u32,
    pub footer_offset: Option<u64>,
    pub prev_same_block_offset: Option<u64>,
    pub prev_same_key_offset: Option<u64>,
    /// The trailing footer word this record carries.
    ///
    /// Always `0` under [`LivenessPolicy::None`], which is every format that
    /// did not opt in. Under [`LivenessPolicy::FooterFlags`] it is the record's
    /// mutable flags — the only part of a written record that can change
    /// without rewriting it, and the only field here that is not covered by the
    /// record's checksum.
    pub mutable_flags: u32,
    pub committed: bool,
}

#[cfg(kani)]
#[path = "verification/record_extent.rs"]
mod record_extent_proofs;

impl RecordIndexEntry {
    /// Reads stored payload bytes by reopening the object currently named by
    /// `path`.
    ///
    /// This is a low-level, non-snapshot convenience API. It does not prove
    /// that the path still names the object from which this entry was indexed,
    /// does not establish index membership or checksum validity, and applies
    /// the standard physical payload ceiling. Prefer a typed
    /// reader/collection, which remains bound to its opened snapshot. For an
    /// explicitly path-based untrusted-input tool, use
    /// [`Self::read_payload_limited`] after establishing the entry's provenance.
    pub fn read_payload(&self, path: &Path) -> Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        self.validate_payload_extent(file_len)?;
        crate::ReadLimits::STANDARD.check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        self.read_payload_file_validated(&mut file)
    }

    /// Reads stored payload bytes from the object currently named by `path`
    /// after applying an allocation ceiling.
    ///
    /// This remains path-based and non-snapshot: pathname replacement can make
    /// it read a different object with a compatible extent. The limit does not
    /// establish index membership or checksum validity. High-level readers do
    /// not use this method.
    pub fn read_payload_limited(&self, path: &Path, limit: u64) -> Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        self.validate_payload_extent(file_len)?;
        ensure_payload_limit(PHYSICAL_PAYLOAD_RESOURCE, self.payload_len, limit)?;
        self.read_payload_file_validated(&mut file)
    }

    pub fn is_compressed(&self) -> bool {
        self.flags & RECORD_FLAG_COMPRESSED != 0
    }

    /// Reads and decodes logical payload bytes through the current pathname.
    ///
    /// This low-level method is non-snapshot and resolves the format's runtime
    /// physical and logical allocation ceilings. It does not replace index-membership or record
    /// integrity validation. Use a typed snapshot reader for ordinary reads,
    /// or [`Self::read_logical_payload_limited`] for explicitly path-based
    /// tooling that has separately established entry provenance.
    pub fn read_logical_payload(&self, spec: FormatSpec, path: &Path) -> Result<Vec<u8>> {
        self.read_logical_payload_limited(spec, path, u64::MAX, u64::MAX)
    }

    /// Reads and decodes through the current pathname with separate stored and
    /// logical allocation ceilings.
    ///
    /// The limits bound allocation but do not turn this path-based helper into
    /// an opened-object snapshot API.
    pub fn read_logical_payload_limited(
        &self,
        spec: FormatSpec,
        path: &Path,
        physical_limit: u64,
        logical_limit: u64,
    ) -> Result<Vec<u8>> {
        let spec = spec.resolve_entrypoint();
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        self.validate_payload_extent(file_len)?;
        spec.read_limits
            .check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        ensure_payload_limit(PHYSICAL_PAYLOAD_RESOURCE, self.payload_len, physical_limit)?;
        let logical_len = self.logical_payload_len_before_allocation(spec, &mut file)?;
        spec.read_limits
            .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;
        ensure_payload_limit(LOGICAL_PAYLOAD_RESOURCE, logical_len, logical_limit)?;
        let payload = self.read_payload_file_validated(&mut file)?;
        decode_record_payload(spec, self, payload)
    }

    pub(crate) fn logical_payload_len_snapshot(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
    ) -> Result<u64> {
        spec.read_limits
            .check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        self.validate_payload_extent(snapshot.len())?;
        let logical_len = if !self.is_compressed() {
            self.payload_len
        } else {
            self.logical_payload_len_from_snapshot(spec, snapshot)?
        };
        spec.read_limits
            .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;
        Ok(logical_len)
    }

    /// **This allocates.**
    /// [`read_payload_snapshot_into`](Self::read_payload_snapshot_into) takes
    /// the caller's buffer, which is what the per-record loops use.
    pub(crate) fn read_payload_snapshot(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
    ) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        self.read_payload_snapshot_into(spec, snapshot, &mut payload)?;
        Ok(payload)
    }

    /// Reads stored bytes into the caller's buffer, applying selected read checks.
    ///
    /// `out` ends exactly `payload_len` bytes long; see
    /// [`SnapshotFile::read_into_at`] for why that matters to the checksum.
    pub(crate) fn read_payload_snapshot_into(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        spec.read_limits
            .check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        self.validate_payload_extent(snapshot.len())?;
        snapshot.read_into_at(
            self.payload_offset,
            self.payload_len,
            limit_or_max(spec.read_limits.require(ReadLimitKey::RecordPayloadLen)?),
            ReadLimitKey::RecordPayloadLen.resource(),
            out,
        )?;
        self.verify_snapshot_record(spec, snapshot, out)
    }

    /// **This allocates.**
    /// [`read_logical_payload_snapshot_into`](Self::read_logical_payload_snapshot_into)
    /// takes the caller's buffer.
    pub(crate) fn read_logical_payload_snapshot(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
    ) -> Result<Vec<u8>> {
        let mut decoded = Vec::new();
        self.read_logical_payload_snapshot_into(spec, snapshot, &mut decoded)?;
        Ok(decoded)
    }

    /// Reads this record's logical bytes into the caller's buffer.
    ///
    /// **Uncompressed records cost no copy.** That is the case worth designing
    /// for — it is every record of a format that declares no compression — and
    /// the obvious `_into` gets it wrong: reading into a scratch and copying
    /// into `out` adds a full `memcpy` per record to a path that today moves
    /// the buffer straight out. So the uncompressed arm reads directly into
    /// `out` and stops there.
    ///
    /// **Compressed records are unchanged, and no better.** The decompressor
    /// produces its own buffer, so `out`'s allocation is replaced rather than
    /// reused. That is what happens today too; this does not regress it, and it
    /// does not improve it either. Making it reuse `out` needs a decompression
    /// entry point that writes into a caller buffer, which is a change to
    /// `decompress_with_algorithm` and is not made here.
    pub(crate) fn read_logical_payload_snapshot_into(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let logical_len = self.logical_payload_len_snapshot(spec, snapshot)?;
        if self.is_compressed() {
            let physical = self.read_payload_snapshot(spec, snapshot)?;
            *out = decode_record_payload(spec, self, physical)?;
        } else {
            self.read_payload_snapshot_into(spec, snapshot, out)?;
        }
        let actual =
            u64::try_from(out.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        if actual != logical_len {
            return Err(Error::DecompressedLengthMismatch {
                expected: logical_len,
                actual,
            });
        }
        Ok(())
    }

    /// Whether the writer marked this record dead.
    ///
    /// Always `false` for a format without `liveness: footer_flags`, where the
    /// word is a reserved field every reader requires to be zero.
    ///
    /// **Advisory.** The record is still here, still framed, and still read
    /// back by every collection; a defragmenting rewrite is what acts on this.
    /// Unknown bits in the word are ignored rather than refused — the word is
    /// outside the checksum, so there is nothing to tell a newer build's flag
    /// from a flipped bit, and refusing would turn a forward-compatible mark
    /// into an unreadable file.
    pub const fn is_dead(&self) -> bool {
        self.mutable_flags & RECORD_MUTABLE_FLAG_DEAD != 0
    }

    pub fn checked_physical_end(&self) -> Result<u64> {
        self.payload_offset
            .checked_add(self.payload_len)
            .and_then(|end| {
                end.checked_add(u64::from(self.footer_offset.is_some()) * RECORD_FOOTER_LEN)
            })
            .ok_or(Error::LengthOverflow {
                value: self.payload_len,
            })
    }

    pub fn physical_end(&self) -> u64 {
        self.payload_offset
            .saturating_add(self.payload_len)
            .saturating_add(u64::from(self.footer_offset.is_some()) * RECORD_FOOTER_LEN)
    }
}

impl From<&RecordIndexEntry> for AppendInfo {
    fn from(entry: &RecordIndexEntry) -> Self {
        Self {
            sequence: entry.sequence,
            record_offset: entry.record_offset,
            payload_offset: entry.payload_offset,
            payload_len: entry.payload_len,
            footer_offset: entry.footer_offset,
            prev_same_block_offset: entry.prev_same_block_offset,
            prev_same_key_offset: entry.prev_same_key_offset,
            committed: entry.committed,
        }
    }
}

#[derive(Clone, Debug)]
pub struct BlockEvent {
    pub block_id: u32,
    pub block_version: u16,
    pub sequence: u64,
    pub record_offset: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendInfo {
    pub sequence: u64,
    pub record_offset: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub footer_offset: Option<u64>,
    pub prev_same_block_offset: Option<u64>,
    pub prev_same_key_offset: Option<u64>,
    pub committed: bool,
}

/// What a [`VarlitFile::defragment`] actually did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefragmentReport {
    /// Records marked dead that the new generation does not contain.
    pub records_dropped: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplacementInfo {
    pub sequence: u64,
    pub record_offset: u64,
    pub old_payload_len: u64,
    pub new_payload_len: u64,
    pub old_physical_len: u64,
    pub new_physical_len: u64,
}

impl ReplacementInfo {
    pub fn translate_record_offset(&self, old: u64) -> Result<u64> {
        if old <= self.record_offset {
            return Ok(old);
        }
        if self.new_physical_len >= self.old_physical_len {
            old.checked_add(self.new_physical_len - self.old_physical_len)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "replacement record offset",
                })
        } else {
            old.checked_sub(self.old_physical_len - self.new_physical_len)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "replacement record offset",
                })
        }
    }
}

impl From<&RecordIndexEntry> for BlockEvent {
    fn from(entry: &RecordIndexEntry) -> Self {
        Self {
            block_id: entry.block_id,
            block_version: entry.block_version,
            sequence: entry.sequence,
            record_offset: entry.record_offset,
            payload_offset: entry.payload_offset,
            payload_len: entry.payload_len,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    pub original_len: u64,
    pub recovered_len: u64,
    pub records_preserved: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterLockInfo {
    pub path: PathBuf,
    pub target_path: PathBuf,
    pub process_id: u32,
    pub created_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterLockBreakPolicy {
    Refuse,
    BreakIfProcessAbsent,
    BreakIfOlderThan(Duration),
    BreakIfProcessAbsentAndOlderThan(Duration),
}

/// What one user record actually stores, and where those bytes live.
///
/// `bytes` is a [`Cow`] rather than a `Vec` because the four uncompressed
/// routes through [`prepare_user_record_payload`] — no compression declared for
/// the block (the default), a non-`Variable` block kind, a payload under
/// `min_uncompressed_len`, and `only_if_smaller` losing — store the caller's
/// bytes unchanged. Owning them meant one payload-sized allocation, memcpy and
/// free on every append of every default configuration, for a buffer every
/// caller immediately reborrows as `&payload.bytes` and drops. Only the arm
/// that actually produces new bytes (compression, and the envelope around it)
/// owns them.
#[derive(Clone, Debug)]
struct StoredPayload<'a> {
    flags: u16,
    uncompressed_len_hint: u32,
    bytes: Cow<'a, [u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SequenceState {
    Available(u64),
    Exhausted,
}

impl SequenceState {
    fn available(self) -> Result<u64> {
        match self {
            Self::Available(sequence) => Ok(sequence),
            Self::Exhausted => Err(Error::SequenceExhausted),
        }
    }

    fn after_publishing(sequence: u64) -> Self {
        match sequence.checked_add(1) {
            Some(next) => Self::Available(next),
            None => Self::Exhausted,
        }
    }
}

/// O(1) writer-side state behind [`VarlitFile::needs_index_checkpoint`]
///
///
/// The original predicate reverse-searched the resident index for the last
/// checkpoint and re-counted the suffix on every flush, which made the
/// cumulative flush CPU O(N^2) for flush-per-record workloads even after the
/// checkpoint *bytes* were made amortized O(N). This state mirrors exactly
/// what that scan recomputed and is maintained incrementally at the single
/// index append site, restored on append rollback, and recovered with one
/// bounded reverse walk wherever the resident index is loaded or replaced
/// wholesale (open, recovery, and generation rebinds).
///
/// Invariant: at all times this equals `CheckpointCadence::from_index(&index)`
/// for the current resident index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CheckpointCadence {
    /// Entries appended since the last full checkpoint whose presence would
    /// change the serialized checkpoint (everything but commit markers).
    eligible_since_checkpoint: usize,
    /// Geometric growth threshold derived from the last checkpoint's index
    /// position: `max(INDEX_CHECKPOINT_MIN_RECORDS, records_at_last_checkpoint / 2)`.
    next_threshold: usize,
}

impl CheckpointCadence {
    /// Cadence for a freshly created file with an empty resident index.
    fn new_empty() -> Self {
        Self {
            eligible_since_checkpoint: 0,
            next_threshold: INDEX_CHECKPOINT_MIN_RECORDS,
        }
    }

    /// Recovers the cadence with a single reverse walk that stops at the last
    /// checkpoint record, so reopen pays the live-tail length exactly once
    /// instead of every flush paying it again.
    /// The rule, kept as the specification `derive_index_state` must match.
    ///
    /// No longer called in production — one forward pass now derives this and
    /// the two structures beside it — but it is what the invariant above names,
    /// and `derived_state_matches_the_three_rules_it_replaced` compares the two
    /// on every shape that distinguishes them.
    #[cfg(test)]
    fn from_index(index: &[RecordIndexEntry]) -> Self {
        let mut eligible: usize = 0;
        let mut touched: u64 = 0;
        for (position, entry) in index.iter().enumerate().rev() {
            touched = touched.saturating_add(1);
            if entry.block_id == INDEX_BLOCK_ID {
                note_checkpoint_cadence_index_touches(touched);
                return Self {
                    eligible_since_checkpoint: eligible,
                    next_threshold: core::cmp::max(INDEX_CHECKPOINT_MIN_RECORDS, position / 2),
                };
            }
            if !matches!(entry.block_id, COMMIT_BLOCK_ID | SEGMENT_BLOCK_ID) {
                eligible = eligible.saturating_add(1);
            }
        }
        note_checkpoint_cadence_index_touches(touched);
        Self {
            eligible_since_checkpoint: eligible,
            next_threshold: INDEX_CHECKPOINT_MIN_RECORDS,
        }
    }

    /// Advances the cadence for the entry just pushed at `position` in the
    /// resident index. A checkpoint record resets the tail count and derives
    /// the next geometric threshold from its own position (the number of
    /// entries it serialized); commit markers, segment records and open digests
    /// never alter checkpoint identity; every other record grows the eligible
    /// tail by one.
    ///
    /// The digest belongs in that exemption for the reason the other two do: it
    /// is written *because* a commit point closed, not because a record was
    /// added, so counting it would make the checkpoint cadence a function of how
    /// often the file is flushed rather than of how much was written.
    fn note_appended(&mut self, position: usize, block_id: u32) {
        note_checkpoint_cadence_index_touches(1);
        if block_id == INDEX_BLOCK_ID {
            self.eligible_since_checkpoint = 0;
            self.next_threshold = core::cmp::max(INDEX_CHECKPOINT_MIN_RECORDS, position / 2);
        } else if !matches!(
            block_id,
            COMMIT_BLOCK_ID | SEGMENT_BLOCK_ID | OPEN_DIGEST_BLOCK_ID
        ) {
            self.eligible_since_checkpoint = self.eligible_since_checkpoint.saturating_add(1);
        }
    }
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    static CHECKPOINT_CADENCE_INDEX_TOUCHES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Counts resident index entries examined by the checkpoint flush-cadence
/// machinery on this thread ( regression evidence). Inert without the
/// `scalable-fault-injection` feature.
#[inline]
fn note_checkpoint_cadence_index_touches(count: u64) {
    #[cfg(feature = "scalable-fault-injection")]
    CHECKPOINT_CADENCE_INDEX_TOUCHES
        .with(|touches| touches.set(touches.get().saturating_add(count)));
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = count;
}

/// The payload one written chunk occupies, given the declared dimensions.
///
/// Computed at create so the two ceilings a chunk crosses — the buffer it is
/// held in and the record it is written as — are reconciled before any write is
/// accepted rather than at the first write.
fn chunk_payload_len_for(spec: FormatSpec, dims: &MatrixDimensions) -> Result<u64> {
    let Some(growing) = spec.growing_matrix else {
        return Ok(0);
    };
    let cell_crc = !matches!(spec.integrity_policy, IntegrityPolicy::None);
    let mut len = MATRIX_CHUNK_PREFIX_LEN
        .checked_add(
            MATRIX_CHUNK_BLOCK_DESC_LEN
                .checked_mul(spec.matrix_blocks.len() as u64)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk descriptors",
                })?,
        )
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "matrix chunk prefix",
        })?;
    for block in spec.matrix_blocks {
        let width = dims
            .get(block.dimensions[1])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[1].to_string()))?;
        let cells =
            growing
                .rows_per_chunk
                .checked_mul(width)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk cells",
                })?;
        let slots =
            cells
                .checked_mul(block.slot_stride)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk slot region",
                })?;
        let crc = if cell_crc {
            cells
                .checked_mul(MATRIX_CHUNK_CRC_LEN)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk checksum table",
                })?
        } else {
            0
        };
        // The uncompressed size plus the sub-block index. Compression can only
        // make the written record smaller, so bounding the uncompressed form is
        // the conservative check — a spec must not depend on its data
        // compressing in order to fit.
        let index = if spec.chunk_compression.is_some() {
            sub_block_count(cells, compressed_cell_width(block.slot_stride, cell_crc))
                .checked_add(1)
                .and_then(|entries| entries.checked_mul(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN))
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk sub-block index",
                })?
        } else {
            0
        };
        len = len
            .checked_add(cells.div_ceil(8))
            .and_then(|len| len.checked_add(crc))
            .and_then(|len| len.checked_add(index))
            .and_then(|len| len.checked_add(slots))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "matrix chunk payload length",
            })?;
    }
    Ok(len)
}

/// Writes one block's slot region as a sub-block index plus compressed
/// sub-blocks.
///
/// The index is `sub_block_count + 1` little-endian `u32` offsets relative to
/// the end of the index, so a sub-block's extent is `off[i]..off[i + 1]` — two
/// four-byte reads and no prefix sum.
///
/// **A sub-block is stored raw when compressing it does not shrink it**, and
/// the reader tells the two apart by length alone: stored length equal to the
/// uncompressed length means raw. The writer therefore never emits a compressed
/// sub-block of exactly that length, which costs nothing — such a sub-block is
/// stored raw instead, byte for byte the same thing.
fn encode_compressed_slots(
    payload: &mut Vec<u8>,
    block: &OpenChunkBlock,
    compression: VariableCompression,
) -> Result<()> {
    let cell_crc = !block.crc.is_empty();
    let width = compressed_cell_width(block.stride, cell_crc);
    let per = cells_per_sub_block(width);
    let count = sub_block_count(block.cells, width);
    let count_usize = usize::try_from(count).map_err(|_| Error::InvalidMatrixChunk)?;
    let stride = usize::try_from(block.stride).map_err(|_| Error::InvalidMatrixChunk)?;
    let width_usize = usize::try_from(width).map_err(|_| Error::InvalidMatrixChunk)?;
    let per_usize = usize::try_from(per).map_err(|_| Error::InvalidMatrixChunk)?;
    let cells = usize::try_from(block.cells).map_err(|_| Error::InvalidMatrixChunk)?;

    let mut stored: Vec<Vec<u8>> = Vec::new();
    stored
        .try_reserve_exact(count_usize)
        .map_err(|_| Error::AllocationFailed {
            resource: "matrix chunk sub-blocks",
            requested: count,
        })?;
    let mut packed = Vec::new();
    for index in 0..count_usize {
        let first = index * per_usize;
        let last = ((index + 1) * per_usize).min(cells);
        packed.clear();
        packed
            .try_reserve((last.saturating_sub(first)) * width_usize)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix chunk sub-block",
                requested: width,
            })?;
        for cell in first..last {
            packed.extend_from_slice(&block.slots[cell * stride..(cell + 1) * stride]);
            if cell_crc {
                packed.extend_from_slice(&block.crc[cell * 4..(cell + 1) * 4]);
            }
        }
        let compressed =
            compress_with_algorithm(compression.algorithm, compression.level, &packed)?;
        if compressed.len() >= packed.len() {
            stored.push(packed.try_clone()?);
        } else {
            stored.push(compressed);
        }
    }

    let mut offset = 0u32;
    for bytes in &stored {
        payload.extend_from_slice(&offset.to_le_bytes());
        offset = offset
            .checked_add(u32::try_from(bytes.len()).map_err(|_| Error::InvalidMatrixChunk)?)
            .ok_or(Error::InvalidMatrixChunk)?;
    }
    payload.extend_from_slice(&offset.to_le_bytes());
    for bytes in &stored {
        payload.extend_from_slice(bytes);
    }
    Ok(())
}

/// Bytes a compressed sub-block holds per cell.
///
/// **The checksum travels with its cell.** Compressing only the slot region
/// left the per-cell checksum table — four bytes per cell, all zeros for every
/// never-written cell — as the dominant term: measured 1,606,396 B plain
/// against 1,083,604 B with slots-only compression, a 33% saving where the
/// slots alone are half the file. Packing each cell beside its checksum makes
/// one decode yield both and lets the zeros collapse together.
const fn compressed_cell_width(stride: u64, cell_crc: bool) -> u64 {
    if cell_crc {
        stride + MATRIX_CHUNK_CRC_LEN
    } else {
        stride
    }
}

/// Cells one compressed sub-block covers, for a given packed cell width.
///
/// Derived from the width alone so the writer and the reader cannot disagree
/// about where a cell lives.
const fn cells_per_sub_block(stride: u64) -> u64 {
    if stride == 0 || stride >= MATRIX_CHUNK_SUB_BLOCK_BYTES {
        1
    } else {
        MATRIX_CHUNK_SUB_BLOCK_BYTES / stride
    }
}

/// Sub-blocks a block's slot region is cut into.
fn sub_block_count(cells: u64, stride: u64) -> u64 {
    cells.div_ceil(cells_per_sub_block(stride)).max(1)
}

/// A zeroed buffer of `len` bytes, allocated fallibly.
///
/// A chunk's buffers are sized by a declared knob, so an over-large
/// `rows_per_chunk` must report `AllocationFailed` rather than abort.
fn try_zeroed_vec(len: u64, resource: &'static str) -> Result<Vec<u8>> {
    let capacity = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested: len,
        })?;
    buffer.resize(capacity, 0);
    Ok(buffer)
}

/// Rebuilds a reloaded chunk block's write bitmap from what the record does
/// carry.
///
/// A cell counts as written if it is committed — a commit cannot have happened
/// without one — or if its slot bytes are not all zero. That second clause is
/// the region's own test (`matrix::commit_cell` scans the slot when its
/// session bitmap says nothing), so a reloaded chunk and a region row answer
/// "was this written" identically.
fn seed_written_map(commit: &[u8], slots: &[u8], cells: u64, stride: u64) -> Result<Vec<u8>> {
    let len = u64::try_from(commit.len()).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "matrix chunk write map",
    })?;
    let mut written = try_zeroed_vec(len, "matrix chunk write map")?;
    let stride = usize::try_from(stride).map_err(|_| Error::InvalidMatrixLayout)?;
    for ordinal in 0..cells {
        let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
        let bit = 1u8 << (ordinal % 8);
        if commit
            .get(byte)
            .is_some_and(|committed| committed & bit != 0)
        {
            written[byte] |= bit;
            continue;
        }
        let start = usize::try_from(ordinal)
            .ok()
            .and_then(|ordinal| ordinal.checked_mul(stride))
            .ok_or(Error::InvalidMatrixLayout)?;
        let slot = slots
            .get(
                start
                    ..start
                        .checked_add(stride)
                        .ok_or(Error::InvalidMatrixLayout)?,
            )
            .ok_or(Error::InvalidMatrixChunk)?;
        if slot.iter().any(|byte| *byte != 0) {
            written[byte] |= bit;
        }
    }
    Ok(written)
}

/// One matrix block's slice of the chunk being filled.
#[derive(Debug)]
struct OpenChunkBlock {
    block_id: u32,
    stride: u64,
    /// `rows * dimension 1`, the cells this chunk holds for this block.
    cells: u64,
    /// One bit per cell: written this session, whatever the bytes say.
    ///
    /// The region keeps the same thing (`current_write_bits`) for the same
    /// reason. Without it, `commit_matrix_cell` decided "was this written" by
    /// testing whether the slot read as all zeros, so a legitimate value that
    /// encodes to zeros — `0u32`, an empty flag word, a zeroed struct — could
    /// never be committed in a chunk while committing fine in the region.
    /// Not written to disk: the commit map is what a reader consults.
    written: Vec<u8>,
    /// One bit per cell, the chunk-local form of the region's commit map.
    commit: Vec<u8>,
    /// `crc32` of each cell's slot bytes, or empty when the format declares no
    /// integrity policy. The chunk-local form of the region's checksum table.
    crc: Vec<u8>,
    /// `cells * stride` bytes, addressed exactly as the region's slot region is.
    slots: Vec<u8>,
}

/// A chunk record's prefix, decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChunkPrefix {
    index: u64,
    first_row: u64,
    rows: u64,
    block_count: u32,
    cell_crc: bool,
    compressed_slots: bool,
}

/// One written chunk, located.
///
/// `block_count` is carried so a cell lookup does not re-read the prefix that
/// finding the chunk already read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChunkLocator {
    index: u64,
    record_offset: u64,
    payload_len: u64,
    block_count: u32,
    cell_crc: bool,
    compressed_slots: bool,
    /// This record's position among the chunk records, counted in the order the
    /// resident index holds them — which is file order, not chunk-index order.
    ///
    /// It is what addresses the record for a rewrite, and it is stored rather
    /// than derived because the two orders are no longer the same: a chunk that
    /// was skipped and is written later sits at the end of the file with a
    /// lower index than its neighbours, and the directory is sorted by index.
    ordinal: usize,
}

/// Every written chunk, by chunk index, ordered once.
///
/// **This exists because the first version did not have it, and that is the
/// mistake this project keeps making.** `find_chunk_record` filtered the whole
/// resident index and collected it into a fresh `Vec` on *every chunked cell
/// read* — `O(total records)` plus a heap allocation per read, behind a doc
/// comment that claimed `O(log chunks)` and "no state built at open". It is the
/// same shape [`CheckpointCadence`] , [`BlockTails`]  and
/// [`SegmentCursor`] each exist to remove, in a file where all three are
/// already written down.
///
/// Built at most once per handle, on the first chunked access rather than at
/// open, and extended in place when the writer writes one. Lookup is a binary search
/// over memory: no walk, no allocation, no read.
///
/// Invariant: equals the `MATRIX_CHUNK_BLOCK_ID` entries of the resident index,
/// in index order, at all times.
#[derive(Debug, Default)]
struct ChunkDirectory {
    chunks: Vec<ChunkLocator>,
    next_ordinal: usize,
}

impl ChunkDirectory {
    fn find(&self, index: u64) -> Option<ChunkLocator> {
        self.chunks
            .binary_search_by_key(&index, |locator| locator.index)
            .ok()
            .map(|position| self.chunks[position])
    }

    /// Files a chunk record that was just appended.
    ///
    /// Inserted at its index position rather than pushed, because a chunk is no
    /// longer always written in ascending order: a chunk that committed nothing
    /// is never written, and a later write to one of its rows appends its
    /// record after higher-indexed ones. Ascending is still the common case and
    /// costs an insert at the end.
    ///
    /// Returns the number of chunk records now filed, which is the ordinal the
    /// *next* appended one will carry.
    fn note_written(&mut self, locator: ChunkLocator) {
        let position = self
            .chunks
            .partition_point(|filed| filed.index < locator.index);
        self.next_ordinal = self.next_ordinal.max(locator.ordinal.saturating_add(1));
        if self
            .chunks
            .get(position)
            .is_some_and(|old| old.index == locator.index)
        {
            self.chunks[position] = locator;
        } else {
            self.chunks.insert(position, locator);
        }
    }

    /// The ordinal an appended chunk record would carry: one past the last,
    /// because a record is appended at the end of the file and the ordinals
    /// count file order.
    fn next_ordinal(&self) -> usize {
        self.next_ordinal
    }
}

/// The chunk a growing matrix is currently filling.
///
/// Held in memory until it is written, which is the one cost this design has and the
/// reason `rows_per_chunk` is a declared knob rather than a constant. Sealing
/// writes it as one ordinary record, so nothing else in the file format learns
/// that chunks exist.
///
/// One chunk is open at a time, and which one is not fixed: writing a row that
/// belongs to an already-written chunk writes the open one out and loads that
/// chunk back — see `VarlitFile::reopen_written_chunk`.
#[derive(Debug)]
struct OpenChunk {
    index: u64,
    first_row: u64,
    rows: u64,
    /// How the slot regions are stored when this chunk is written.
    compression: Option<VariableCompression>,
    blocks: Vec<OpenChunkBlock>,
    /// Whether any cell has been committed since the chunk was opened. A chunk
    /// nothing committed is not written: an empty chunk and an absent chunk
    /// answer every read identically, and writing one would grow the file on
    /// every idle flush the way an empty segment once did.
    dirty: bool,
    /// The chunk-record ordinal this chunk was loaded from, when it was loaded
    /// rather than started empty.
    ///
    /// `Some` means a record for this chunk index already exists and holds
    /// these same bytes, so writing the chunk out must rewrite *that* record
    /// rather than append a second one. Two records claiming one chunk index is
    /// not a thing the directory can order, and `build_chunk_directory` refuses
    /// a file that carries them.
    ///
    /// An ordinal rather than an offset because that is what
    /// `ReplacementTarget` selects on: the nth record of `MATRIX_CHUNK_BLOCK_ID`
    /// is the nth entry of the chunk directory, which is built by walking the
    /// resident index in the same order.
    backing: Option<usize>,
}

impl OpenChunk {
    fn block_position(&self, block_id: u32) -> Option<usize> {
        self.blocks
            .iter()
            .position(|block| block.block_id == block_id)
    }

    fn payload_len(&self) -> Result<u64> {
        let mut len = MATRIX_CHUNK_PREFIX_LEN
            .checked_add(
                MATRIX_CHUNK_BLOCK_DESC_LEN
                    .checked_mul(u64::try_from(self.blocks.len()).map_err(|_| {
                        Error::ResourceArithmeticOverflow {
                            resource: "matrix chunk block count",
                        }
                    })?)
                    .ok_or(Error::ResourceArithmeticOverflow {
                        resource: "matrix chunk descriptors",
                    })?,
            )
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "matrix chunk prefix",
            })?;
        for block in &self.blocks {
            let commit = u64::try_from(block.commit.len()).map_err(|_| {
                Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk commit map",
                }
            })?;
            let slots = u64::try_from(block.slots.len()).map_err(|_| {
                Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk slot region",
                }
            })?;
            let crc =
                u64::try_from(block.crc.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk checksum table",
                })?;
            let index = if self.compression.is_some() {
                sub_block_count(
                    block.cells,
                    compressed_cell_width(block.stride, !block.crc.is_empty()),
                )
                .checked_add(1)
                .and_then(|entries| entries.checked_mul(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN))
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk sub-block index",
                })?
            } else {
                0
            };
            len = len
                .checked_add(commit)
                .and_then(|len| len.checked_add(crc))
                .and_then(|len| len.checked_add(index))
                .and_then(|len| len.checked_add(slots))
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk payload length",
                })?;
        }
        Ok(len)
    }

    fn encode(&self) -> Result<Vec<u8>> {
        // The uncompressed size, which is exact when plain and an upper bound
        // when compressing. Reserving the bound is the point: a compressed
        // chunk must never need more than the ceiling `create` checked.
        let payload_len = self.payload_len()?;
        let capacity = usize::try_from(payload_len)
            .map_err(|_| Error::LengthOverflow { value: payload_len })?;
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(capacity)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix chunk payload",
                requested: payload_len,
            })?;
        let block_count =
            u32::try_from(self.blocks.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "matrix chunk block count",
            })?;
        let mut flags = if self.blocks.iter().any(|block| !block.crc.is_empty()) {
            MATRIX_CHUNK_FLAG_CELL_CRC
        } else {
            0
        };
        if self.compression.is_some() {
            flags |= MATRIX_CHUNK_FLAG_COMPRESSED_SLOTS;
        }
        payload.extend_from_slice(MATRIX_CHUNK_MAGIC);
        payload.extend_from_slice(&MATRIX_CHUNK_VERSION.to_le_bytes());
        payload.extend_from_slice(&flags.to_le_bytes());
        payload.extend_from_slice(&self.index.to_le_bytes());
        payload.extend_from_slice(&self.first_row.to_le_bytes());
        payload.extend_from_slice(&self.rows.to_le_bytes());
        payload.extend_from_slice(&block_count.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        for block in &self.blocks {
            let commit_len = u64::try_from(block.commit.len()).map_err(|_| {
                Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk commit map",
                }
            })?;
            payload.extend_from_slice(&block.block_id.to_le_bytes());
            payload.extend_from_slice(&0u32.to_le_bytes());
            payload.extend_from_slice(&block.stride.to_le_bytes());
            payload.extend_from_slice(&block.cells.to_le_bytes());
            payload.extend_from_slice(&commit_len.to_le_bytes());
        }
        for block in &self.blocks {
            payload.extend_from_slice(&block.commit);
            match self.compression {
                Some(compression) => {
                    // No separate checksum table: each cell is packed beside
                    // its own, inside the sub-block.
                    encode_compressed_slots(&mut payload, block, compression)?;
                }
                None => {
                    payload.extend_from_slice(&block.crc);
                    payload.extend_from_slice(&block.slots);
                }
            }
        }
        Ok(payload)
    }
}

/// Where one block's bytes sit inside a chunk record's payload.
///
/// Produced by reading the prefix and the descriptors — a fixed, small read
/// whatever the chunk holds — so a cell read is that read plus one `stride`-byte
/// positional read, and never the chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChunkBlockLocation {
    commit_offset: u64,
    /// `None` when the chunk carries no per-cell checksums.
    crc_offset: Option<u64>,
    slots_offset: u64,
    /// One past the last byte of this block's slot region, so a compressed
    /// sub-block's extent is bounded without consulting the record again.
    slots_end: u64,
    compressed_slots: bool,
    /// Whether cells carry a checksum at all, which a compressed block needs
    /// even though it has no separate table.
    cell_crc: bool,
    stride: u64,
    cells: u64,
}

/// O(1) writer-side state for the internal segment chain.
///
/// A segment covers the records one commit point added, so writing one needs
/// two things: where in the resident index that run begins, and where it begins
/// on disk. Both are derivable by walking back to the newest segment record,
/// and neither is derived that way — the walk would be `Theta(g)` per commit
/// point and `Theta(N)` on a file that has records but no segment record yet,
/// which is the same shape  and  removed from the flush and
/// append paths.
///
/// Invariant: at all times this equals `SegmentCursor::from_index(&index)` for
/// the current resident index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SegmentCursor {
    /// Resident-index position just past the newest segment record: the first
    /// entry the next segment covers.
    next_position: usize,
    /// File offset just past the newest segment record, or `None` when the file
    /// holds no segment record and coverage therefore starts at the append log.
    covered_start: Option<u64>,
}

impl SegmentCursor {
    fn new_empty() -> Self {
        Self {
            next_position: 0,
            covered_start: None,
        }
    }

    /// Recovers the cursor with one reverse walk that stops at the newest
    /// segment record, so a reopen or a generation rebind pays the live tail
    /// once instead of every commit point paying it again.
    ///
    /// `physical_end` saturates rather than overflowing. The entries here come
    /// from a completed scan or a validated generation, so it cannot saturate
    /// in practice; if it ever did, the next segment would record a coverage
    /// start the following open cannot match, and that open falls back to the
    /// full scan rather than trusting the chain.
    /// The rule, kept as the specification `derive_index_state` must match.
    #[cfg(test)]
    fn from_index(entries: &[RecordIndexEntry]) -> Self {
        match entries
            .iter()
            .rposition(|entry| entry.block_id == SEGMENT_BLOCK_ID)
        {
            Some(position) => Self {
                next_position: position + 1,
                covered_start: Some(entries[position].physical_end()),
            },
            None => Self::new_empty(),
        }
    }

    /// Advances the cursor for the entry just pushed at `position`.
    ///
    /// `record_end` is the physical end the append path budgeted and checked
    /// before writing, so this stays infallible on the far side of the
    /// authoritative write (INVARIANT 3).
    fn note_appended(&mut self, position: usize, block_id: u32, record_end: u64) {
        if block_id == SEGMENT_BLOCK_ID {
            self.next_position = position + 1;
            self.covered_start = Some(record_end);
        }
    }
}

/// O(1)-append / O(log B) resident block-offset tails.
///
/// The append path used to reverse-scan the resident index for the newest
/// record of the same block, costing `Theta(g)` per append (`g` = distance
/// back to that record) and `O(N*B)` overall, degenerating to `Theta(N^2)`
/// when every record carries a distinct block id. This mirrors exactly what
/// that scan recomputed - the record offset of the newest entry per block id -
/// and never reads the resident index on the append path.
///
/// `B` is the number of distinct block ids present, which is bounded by the
/// format's declared block set plus varlit's internal ids, so the sorted vector
/// stays tiny and stops allocating once every block id has appeared.
///
/// Invariant: at all times this equals `BlockTails::from_index(&index)` for
/// the current resident index. It is maintained at the single index append
/// site and rebuilt wherever the resident index is replaced or truncated
/// wholesale (open, recovery, generation rebinds, append rollback).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct BlockTails {
    /// Sorted by block id.
    tails: Vec<(u32, u64)>,
}

impl BlockTails {
    fn new_empty() -> Self {
        Self { tails: Vec::new() }
    }

    /// Recovers the tails with one forward pass over the resident index, so a
    /// reopen pays the pass exactly once instead of every append paying its own
    /// reverse scan.
    ///
    /// The pass keeps the newest offset per block id in a hash map and orders
    /// the distinct ids exactly once, so construction is `O(N + B log B)` time
    /// and `O(B)` memory. Replaying the pass through [`Self::note_appended`]
    /// instead - which is what this did originally - inserts every first-seen
    /// id into the sorted vector, and an index whose first appearances are in
    /// descending id order moves `0 + 1 + ... + (B - 1)` tuples. That made the
    /// real bound `O(N log B + B^2)`, not the `O(N log B)` the comment here
    /// used to claim. The append path keeps the sorted-vector
    /// insertion: it pays at most one insertion per distinct id over the whole
    /// life of the file and buys `O(log B)` lookups with no hashing on the hot
    /// path.
    /// The rule, kept as the specification `derive_index_state` must match.
    #[cfg(test)]
    fn from_index(index: &[RecordIndexEntry]) -> Self {
        note_block_tail_index_touches(index.len() as u64);
        let mut newest: HashMap<u32, u64> = HashMap::new();
        for entry in index {
            newest.insert(entry.block_id, entry.record_offset);
        }
        Self::from_newest(&newest).expect("test tail allocation")
    }

    /// Orders an already-collected newest-per-block map.
    ///
    /// The scan collects into a map rather than replaying `note_appended`, for
    /// the reason `from_index` does: replaying inserts every first-seen id into
    /// the sorted vector, and an index whose first appearances are in descending
    /// id order moves `0 + 1 + ... + (B - 1)` tuples.
    /// Adopts a table a digest already ordered.
    ///
    /// `decode_digest_payload` rejects a non-ascending id, so this cannot adopt
    /// an out-of-order table — which matters, because every lookup here is a
    /// binary search and an unsorted table would answer wrong rather than
    /// slowly.
    fn from_sorted(tails: Vec<(u32, u64)>) -> Self {
        debug_assert!(tails.windows(2).all(|pair| pair[0].0 < pair[1].0));
        Self { tails }
    }

    fn from_newest(newest: &HashMap<u32, u64>) -> Result<Self> {
        let mut tails = Vec::new();
        tails
            .try_reserve_exact(newest.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "block tails",
                requested: (newest.len() as u64).saturating_mul(16),
            })?;
        tails.extend(newest.iter().map(|(id, offset)| (*id, *offset)));
        tails.sort_unstable_by_key(|(block_id, _)| *block_id);
        Ok(Self { tails })
    }

    fn tail(&self, block_id: u32) -> Option<u64> {
        self.position(block_id)
            .map(|position| self.tails[position].1)
    }

    /// The table itself, sorted by block id, for the digest encoder.
    fn as_slice(&self) -> &[(u32, u64)] {
        &self.tails
    }

    fn position(&self, block_id: u32) -> Option<usize> {
        self.tails
            .binary_search_by_key(&block_id, |(id, _)| *id)
            .ok()
    }

    /// Puts a block's tail back where it was before an append that rolled
    /// back. `None` means the block had no record yet, so the entry is removed.
    fn restore(&mut self, block_id: u32, previous: Option<u64>) {
        match (self.position(block_id), previous) {
            (Some(position), Some(offset)) => self.tails[position].1 = offset,
            (Some(position), None) => {
                self.tails.remove(position);
            }
            (None, _) => {}
        }
    }

    /// Advances the tail for the entry just pushed. Updating a block id the
    /// table already holds writes one `u64` and allocates nothing; a block id
    /// is inserted at most once per distinct id in the file.
    fn note_appended(&mut self, block_id: u32, record_offset: u64) {
        match self.tails.binary_search_by_key(&block_id, |(id, _)| *id) {
            Ok(position) => self.tails[position].1 = record_offset,
            Err(position) => {
                // Every insertion shifts the tuples above it; counting them is
                // what makes the historical quadratic construction term
                // observable to a regression test.
                note_block_tail_entries_moved((self.tails.len() - position) as u64);
                self.tails.insert(position, (block_id, record_offset));
            }
        }
    }
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    static BLOCK_TAIL_INDEX_TOUCHES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Counts resident index entries examined by the block-offset-chain
/// predecessor machinery on this thread ( regression evidence). The
/// append path must never advance this counter. Inert without the
/// `scalable-fault-injection` feature.
#[inline]
fn note_block_tail_index_touches(count: u64) {
    #[cfg(feature = "scalable-fault-injection")]
    BLOCK_TAIL_INDEX_TOUCHES.with(|touches| touches.set(touches.get().saturating_add(count)));
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = count;
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    static BLOCK_TAIL_ENTRIES_MOVED: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Counts tuples displaced by sorted-vector insertion in the block-tail table
/// on this thread ( regression evidence). Index *visits* cannot see
/// this cost, which is why the quadratic construction term survived the
/// previous review. Inert without the `scalable-fault-injection` feature.
#[inline]
fn note_block_tail_entries_moved(count: u64) {
    #[cfg(feature = "scalable-fault-injection")]
    BLOCK_TAIL_ENTRIES_MOVED.with(|moved| moved.set(moved.get().saturating_add(count)));
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = count;
}

/// Lazily built resident keyed tails for the generic keyed append path
///
///
/// One tail map per keyed block id. The resident file is not generic over its
/// block types, so keys are held as their canonical internal key payload -
/// the same encoding tombstone records carry - rather than as `T::Key`. A map
/// is built on first use from [`VarlitFile::key_tail_offsets`] and then
/// maintained in O(1) per append, so the generic keyed entry points link
/// predecessors exactly like the generated keyed writer does. Any
/// caller-supplied predecessor (the generated writer's own path) invalidates
/// the cached map for that block id, because the file cannot learn which key
/// that record carried.
#[derive(Debug, Default)]
struct KeyedTails {
    tails: HashMap<u32, KeyedTailMap>,
}

/// One block id's tail map plus the running total of the key payload bytes it
/// owns.
///
/// The total is maintained incrementally - one addition on the append that
/// introduces a key, nothing at all when an existing key is overwritten - so
/// charging the cache against [`ReadLimits::max_keyed_tail_bytes`] stays O(1)
/// per append. Recomputing it by summing key lengths would be Theta(distinct
/// keys) on the append hot path and is exactly what this field exists to
/// avoid.
#[derive(Debug, Default)]
struct KeyedTailMap {
    offsets: HashMap<Vec<u8>, u64>,
    key_payload_bytes: u64,
    spilled: Option<crate::disk_index::ScratchIndex>,
}

impl KeyedTailMap {
    fn get(&self, key: &[u8]) -> Result<Option<u64>> {
        if let Some(&offset) = self.offsets.get(key) {
            return Ok(Some(offset));
        }
        match &self.spilled {
            Some(index) => Ok(index
                .get(key)?
                .map(|row| u64::from_le_bytes(row[..8].try_into().unwrap()))),
            None => Ok(None),
        }
    }
    fn spill(&mut self) -> Result<()> {
        if self.offsets.is_empty() {
            return Ok(());
        }
        if self.spilled.is_none() {
            self.spilled = Some(crate::disk_index::ScratchIndex::new(0)?);
        }
        let updates = self
            .offsets
            .iter()
            .map(|(key, &offset)| {
                let mut row = [0; 52];
                row[..8].copy_from_slice(&offset.to_le_bytes());
                Ok((key.try_clone()?, row))
            })
            .collect::<Result<_>>()?;
        self.spilled.as_mut().unwrap().apply(&updates)?;
        self.offsets.clear();
        self.key_payload_bytes = 0;
        Ok(())
    }
    fn reserve(&mut self, spec: FormatSpec, key: &[u8]) -> Result<()> {
        let limit = spec
            .read_limits
            .require(ReadLimitKey::KeyedTailBytes)?
            .unwrap_or(2 * 1024 * 1024)
            .min(2 * 1024 * 1024);
        if !self.offsets.contains_key(key) && self.charge_for(1, key.len() as u64)? > limit {
            self.spill()?;
        }
        // One bounded key is the minimum work unit even with a zero cache.
        self.offsets
            .try_reserve(1)
            .map_err(|_| Error::AllocationFailed {
                resource: "keyed tail offsets",
                requested: key.len() as u64,
            })?;
        Ok(())
    }
    fn insert(&mut self, key: Vec<u8>, offset: u64) {
        let bytes = key.len() as u64;
        if self.offsets.insert(key, offset).is_none() {
            self.key_payload_bytes += bytes;
        }
    }
    /// Bytes this map is accountable for: its inline storage plus the key
    /// payload heap it owns. Excludes `HashMap` control bytes and load-factor
    /// slack, which the charge deliberately does not claim to cover.
    fn charge_for(&self, additional_entries: usize, additional_key_bytes: u64) -> Result<u64> {
        let inline = allocation_bytes::<(Vec<u8>, u64)>(
            self.offsets.len().saturating_add(additional_entries),
            "keyed tail offsets",
        )?;
        inline
            .checked_add(self.key_payload_bytes)
            .and_then(|total| total.checked_add(additional_key_bytes))
            .ok_or(Error::AllocationFailed {
                resource: "keyed tail offsets",
                requested: u64::MAX,
            })
    }
}

impl KeyedTails {
    fn new_empty() -> Self {
        Self {
            tails: HashMap::new(),
        }
    }

    fn invalidate(&mut self, block_id: u32) {
        self.tails.remove(&block_id);
    }

    fn invalidate_all(&mut self) {
        self.tails.clear();
    }
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    /// Armed pre-append keyed-tail reservation failures (API3-01).
    static INJECTED_KEYED_TAIL_RESERVATION_FAILURES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
    /// Armed post-append keyed-tail reservation losses (API3-01).
    static INJECTED_KEYED_TAIL_COMMIT_LOSSES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
    /// Armed post-commit-marker durability failures (round 10, invariant 3).
    static INJECTED_COMMIT_DURABILITY_FAILURES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Consumes one armed keyed-tail reservation failure.
///
/// Inert without the `scalable-fault-injection` feature. Thread-local so
/// concurrently running tests cannot arm each other's writers.
#[inline]
fn take_injected_keyed_tail_reservation_failure(requested: u64) -> Result<()> {
    #[cfg(feature = "scalable-fault-injection")]
    {
        let armed = INJECTED_KEYED_TAIL_RESERVATION_FAILURES.with(|count| {
            let current = count.get();
            if current != 0 {
                count.set(current - 1);
            }
            current != 0
        });
        if armed {
            return Err(Error::AllocationFailed {
                resource: "keyed tail offsets",
                requested,
            });
        }
    }
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = requested;
    Ok(())
}

/// Consumes one armed post-append keyed-tail reservation loss.
///
/// Models the otherwise unreachable state in which the slot reserved before
/// the append is no longer usable afterwards, so the infallible commit's
/// discard-the-cache fallback can be exercised deterministically. Inert
/// without the `scalable-fault-injection` feature.
#[inline]
fn take_injected_keyed_tail_commit_loss() -> bool {
    #[cfg(feature = "scalable-fault-injection")]
    {
        INJECTED_KEYED_TAIL_COMMIT_LOSSES.with(|count| {
            let current = count.get();
            if current != 0 {
                count.set(current - 1);
            }
            current != 0
        })
    }
    #[cfg(not(feature = "scalable-fault-injection"))]
    {
        false
    }
}

/// Consumes one armed post-commit-marker durability failure.
///
/// Models a `flush`/`fsync` that fails *after* the commit marker has been
/// appended, which a real filesystem will not produce on demand. Inert without
/// the `scalable-fault-injection` feature.
#[inline]
fn take_injected_commit_durability_failure() -> Result<()> {
    #[cfg(feature = "scalable-fault-injection")]
    {
        let armed = INJECTED_COMMIT_DURABILITY_FAILURES.with(|count| {
            let current = count.get();
            if current != 0 {
                count.set(current - 1);
            }
            current != 0
        });
        if armed {
            return Err(Error::Io(std::io::Error::other(
                "injected post-commit durability failure",
            )));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct AppendSnapshot {
    /// The end of file this append starts from, and the offset a rollback both
    /// truncates to and leaves the handle at.
    ///
    /// It used to carry a second number beside this one — the handle's actual
    /// offset, read with a per-record `stream_position` — so that a rollback
    /// could put the cursor back exactly where it found it. Reading it was an
    /// `lseek` per record; caching it instead was unsound, because the offset
    /// is shared with `VarlitFile::snapshot` (see `RecordFile`). Restoring `eof`
    /// costs neither: it is this field, and after any successful append the two
    /// numbers are the same one.
    eof: u64,
    sequence_state: SequenceState,
    index_len: usize,
    checkpoint_cadence: CheckpointCadence,
    segment_cursor: SegmentCursor,
    /// The block this append is about to move the tail of, and where that tail
    /// pointed before. Two words, so the rollback needs no allocation.
    block_id: u32,
    previous_block_tail: Option<u64>,
    uncommitted_since_commit: bool,
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
    /// The block id of the newest resident entry, maintained at the single
    /// index append site.
    ///
    /// Exists so that `needs_index_segment` and `needs_open_digest` can ask
    /// "is the last thing in this file already the record I am about to write"
    /// without faulting an entry off disk on every flush. A slot holds an
    /// offset and a committed bit, so the block id is not otherwise knowable
    /// without a read, and both predicates run on the append path.
    last_resident_block_id: Option<u32>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WriteFault {
    #[default]
    None,
    AppendAfterHeader,
    AppendAfterHeaderWithRollbackFailure,
    RollbackFailure,
    RebindAfterPublish,
    ///  double fault: the parent-directory sync fails *and* the rebind
    /// that follows the same publication fails.
    ///
    /// One arming produces both, because taking the parent-sync failure re-arms
    /// the injector as [`WriteFault::RebindAfterPublish`]. That ordering is the
    /// one the code under test sees: `replace_path_atomically` syncs the parent
    /// directory before it returns, and the writer rebinds afterwards.
    ParentSyncThenRebindAfterPublish,
}

#[cfg(test)]
std::thread_local! {
    static WRITE_FAULT: std::cell::Cell<WriteFault> = const {
        std::cell::Cell::new(WriteFault::None)
    };
}

#[cfg(test)]
fn inject_write_fault(fault: WriteFault) {
    WRITE_FAULT.set(fault);
}

#[cfg(test)]
fn fail_append_after_header_if_requested() -> std::io::Result<()> {
    WRITE_FAULT.with(|fault| match fault.get() {
        WriteFault::AppendAfterHeader => {
            fault.set(WriteFault::None);
            Err(std::io::Error::other("injected append write failure"))
        }
        WriteFault::AppendAfterHeaderWithRollbackFailure => {
            fault.set(WriteFault::RollbackFailure);
            Err(std::io::Error::other("injected append write failure"))
        }
        _ => Ok(()),
    })
}

#[cfg(test)]
fn fail_rollback_if_requested() -> std::io::Result<()> {
    WRITE_FAULT.with(|fault| {
        if fault.get() == WriteFault::RollbackFailure {
            fault.set(WriteFault::None);
            Err(std::io::Error::other("injected rollback failure"))
        } else {
            Ok(())
        }
    })
}

/// injects a parent-directory sync failure and arms the rebind failure
/// that must follow it in the same publication.
#[cfg(test)]
fn fail_parent_sync_if_requested() -> Result<()> {
    WRITE_FAULT.with(|fault| {
        if fault.get() == WriteFault::ParentSyncThenRebindAfterPublish {
            fault.set(WriteFault::RebindAfterPublish);
            Err(Error::Io(std::io::Error::other(
                "injected parent-directory sync failure",
            )))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
fn fail_rebind_after_publish_if_requested() -> std::io::Result<()> {
    WRITE_FAULT.with(|fault| {
        if fault.get() == WriteFault::RebindAfterPublish {
            fault.set(WriteFault::None);
            Err(std::io::Error::other(
                "injected post-publication rebind failure",
            ))
        } else {
            Ok(())
        }
    })
}

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

/// Seals the open chunk on the way out.
///
/// **Every other byte a writer accepts is on disk before the call returns.** A
/// chunked cell is the one exception: it lives in the open chunk until it is written.
/// Without this, dropping a writer without `flush` lost every committed cell in
/// that chunk, silently, and this library had no other way to lose committed
/// data.
///
/// Best effort, and that is a real limitation rather than a hedge: `drop`
/// cannot report a failure, so a caller who needs to know the write succeeded
/// must call `flush`, `commit` or `sync` and read the error. What this
/// guarantees is that the ordinary case — a writer that goes out of scope —
/// does not lose data.
impl Drop for VarlitFile {
    fn drop(&mut self) {
        if self.mode == OpenMode::ReadWrite && self.open_chunk.is_some() {
            let _ = self.write_open_chunk_record();
        }
        // The clean-release half of the crash verdict. Best-effort by
        // necessity — `drop` cannot report — and that is the right shape here:
        // a clear that fails leaves the bit set, so the next open runs a
        // recovery pass it did not strictly need, which is the direction to
        // fail in. The opposite arrangement would let a crash read as clean.
        if self.mode == OpenMode::ReadWrite {
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
    matrix: Option<crate::matrix::MatrixLayout>,
    // Present exactly when `matrix` is present: read once at open/create time
    // so sidecar identity checks never touch the file per operation.
    matrix_creation_nonce: Option<[u8; MATRIX_CREATION_NONCE_LEN]>,
    sequence_state: SequenceState,
    // O(1) flush-cadence state for `needs_index_checkpoint`; must equal
    // `CheckpointCadence::from_index(&index)` at all times.
    checkpoint_cadence: CheckpointCadence,
    // O(1) append-side block-offset-chain predecessors; must equal
    // `BlockTails::from_index(&index)` at all times.
    block_tails: BlockTails,
    // The matrix chunk being filled, when a growing dimension is declared. See
    // `OpenChunk`; `None` until a write lands past the declared extent.
    open_chunk: Option<OpenChunk>,
    // Every written chunk, ordered once and searched in memory. Empty until the
    // first chunked access, so a handle that never touches a chunk builds
    // nothing. See `ChunkDirectory` for why this is not derived per read.
    //
    // `OnceCell` rather than `Option`, because building it happens on a read
    // path and every read entry point takes `&self` — that is a standing design
    // policy, not a convenience.
    chunk_directory: std::cell::OnceCell<ChunkDirectory>,
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

/// A reader with private caches: Send + !Sync. Move ownership to a worker
/// or open a separate reader for each thread; ordinary reads take `&self`.
#[derive(Debug)]
pub struct VarlitReader {
    file: VarlitFile,
}

impl VarlitReader {
    pub fn matrix_generation(&self) -> Option<u64> {
        self.file.matrix_generation()
    }
    /// Required companion file for matrix storage; copy/rename it with the primary.
    pub fn matrix_generation_path(&self) -> Option<&Path> {
        self.file.matrix_generation_path()
    }

    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_readonly(spec, path)?,
        })
    }

    /// See [`VarlitFile::follow`].
    ///
    /// The one `&mut self` method on this type, and deliberately so: every read
    /// takes `&self` and stays available to concurrent callers, and advancing
    /// the handle to a later state is the one operation that is not a read.
    pub fn follow(&mut self) -> Result<u64> {
        self.file.follow()
    }

    /// See [`VarlitFile::is_current`].
    pub fn is_current(&self) -> Result<bool> {
        self.file.is_current()
    }

    /// A fresh reader on whatever the pathname resolves to now. See
    /// [`VarlitFile::reopen_readonly`].
    ///
    /// Takes `&self` and returns a separate reader with its own caches.
    pub fn reopen(&self) -> Result<Self> {
        Ok(Self {
            file: self.file.reopen_readonly()?,
        })
    }

    /// See [`VarlitFile::open_readonly_with_scratch`].
    pub fn open_with_scratch<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        scratch: &mut Vec<RecordIndexEntry>,
    ) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_readonly_with_scratch(spec, path, scratch)?,
        })
    }

    /// See [`VarlitFile::open_readonly_without_directory`].
    pub fn open_without_directory<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        index: &mut Vec<RecordIndexEntry>,
    ) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_readonly_without_directory(spec, path, index)?,
        })
    }

    pub fn into_inner(self) -> VarlitFile {
        self.file
    }

    pub fn spec(&self) -> FormatSpec {
        self.file.spec()
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// The open file this reader holds, borrowed rather than consumed.
    pub fn file(&self) -> &VarlitFile {
        &self.file
    }

    /// Reads a block out of the editable header region.
    /// See [`VarlitFile::read_header_block`].
    ///
    /// `&self` and no I/O: the region is part of the header this handle already
    /// read at open, which is what makes it usable as a seed for a walk.
    pub fn read_header_block<T: VarlitBlock>(&self) -> Result<Option<T>> {
        self.file.read_header_block::<T>()
    }

    /// The declared size of the editable header region; `0` when undeclared.
    pub fn header_slots_capacity(&self) -> usize {
        self.file.header_slots_capacity()
    }

    /// How much of the region the current blocks occupy.
    pub fn header_slots_used(&self) -> Result<usize> {
        self.file.header_slots_used()
    }

    /// How much of the region is left.
    pub fn header_slots_free_bytes(&self) -> Result<usize> {
        self.file.header_slots_free_bytes()
    }

    /// Whether the region's seal is down.
    pub fn header_slots_sealed(&self) -> Result<bool> {
        self.file.header_slots_sealed()
    }

    pub fn mode(&self) -> OpenMode {
        self.file.mode()
    }

    pub fn metadata(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.file.metadata(key)
    }

    pub fn all_metadata(&self) -> Result<Vec<(String, Vec<u8>)>> {
        self.file.all_metadata()
    }

    /// See [`VarlitFile::all_metadata_into`].
    pub fn all_metadata_into(&self, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
        self.file.all_metadata_into(out)
    }

    pub fn schema_manifest(&self) -> Result<Option<SchemaManifest>> {
        self.file.schema_manifest()
    }

    pub fn blocks<T: VarlitBlock>(&self) -> Result<BlockVec<T>> {
        self.file.blocks::<T>()
    }

    /// See [`VarlitFile::block_entries_into`].
    pub fn block_entries_into<T: VarlitBlock>(
        &self,
        out: &mut Vec<RecordIndexEntry>,
    ) -> Result<()> {
        self.file.block_entries_into::<T>(out)
    }

    /// See [`VarlitFile::decode_blocks_into`] — including why it is not the
    /// `_into` twin of [`blocks`](Self::blocks).
    pub fn decode_blocks_into<T: VarlitBlock>(&self, out: &mut Vec<T>) -> Result<()> {
        self.file.decode_blocks_into::<T>(out)
    }

    pub fn blocks_migrated<From, To, M>(&self) -> Result<Vec<To>>
    where
        From: VarlitBlock,
        To: VarlitBlock,
        M: VarlitMigration<From, To>,
    {
        self.file.blocks_migrated::<From, To, M>()
    }

    /// See [`VarlitFile::blocks_migrated_into`].
    pub fn blocks_migrated_into<From, To, M>(&self, out: &mut Vec<To>) -> Result<()>
    where
        From: VarlitBlock,
        To: VarlitBlock,
        M: VarlitMigration<From, To>,
    {
        self.file.blocks_migrated_into::<From, To, M>(out)
    }

    pub fn keyed_blocks<T>(&self) -> Result<KeyedBlockVec<T::Key, T>>
    where
        T: VarlitKeyedBlock,
    {
        self.file.keyed_blocks::<T>()
    }

    /// See [`VarlitFile::keyed_blocks_into`], including which of the three
    /// collections it does not hand over.
    pub fn keyed_blocks_into<T>(
        &self,
        entries: &mut Vec<RecordIndexEntry>,
        by_key: &mut HashMap<T::Key, RecordIndexEntry>,
    ) -> Result<()>
    where
        T: VarlitKeyedBlock,
    {
        self.file.keyed_blocks_into::<T>(entries, by_key)
    }

    pub fn materialized_keyed_blocks<T>(&self) -> Result<HashMap<T::Key, T>>
    where
        T: VarlitMerge,
        T::Key: Eq + Hash,
    {
        self.file.materialized_keyed_blocks::<T>()
    }

    /// See [`VarlitFile::materialized_keyed_blocks_into`].
    pub fn materialized_keyed_blocks_into<T>(&self, out: &mut HashMap<T::Key, T>) -> Result<()>
    where
        T: VarlitMerge,
        T::Key: Eq + Hash,
    {
        self.file.materialized_keyed_blocks_into::<T>(out)
    }

    pub fn scan(&self) -> impl Iterator<Item = Result<BlockEvent>> + '_ {
        self.file.scan()
    }

    /// Verifies every record's stored checksum. See [`VarlitFile::verify_all`].
    pub fn verify_all(&self) -> Result<usize> {
        self.file.verify_all()
    }

    pub fn index_entries(&self) -> IndexEntries {
        self.file.index_entries()
    }

    /// Fills the caller's buffer with the record index; see
    /// [`VarlitFile::index_entries_into`].
    pub fn index_entries_into(&self, out: &mut Vec<RecordIndexEntry>) -> Result<()> {
        self.file.index_entries_into(out)
    }

    /// See [`VarlitFile::with_directory`].
    pub fn with_directory<'a, D: RecordDirectory + ?Sized>(
        &'a self,
        directory: &'a D,
    ) -> Result<DirectoryRead<'a, D>> {
        self.file.with_directory(directory)
    }

    /// See [`VarlitFile::record_map`].
    pub fn record_map<'a>(&self, buffer: &'a mut Vec<RecordIndexEntry>) -> Result<RecordMap<'a>> {
        self.file.record_map(buffer)
    }

    /// See [`VarlitFile::read_payload_into`].
    pub fn read_payload_into(&self, entry: &RecordIndexEntry, out: &mut Vec<u8>) -> Result<()> {
        self.file.read_payload_into(entry, out)
    }

    /// See [`VarlitFile::read_logical_payload_into`].
    pub fn read_logical_payload_into(
        &self,
        entry: &RecordIndexEntry,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        self.file.read_logical_payload_into(entry, out)
    }

    /// See [`VarlitFile::decode_block_into`].
    pub fn decode_block_into<T: VarlitBlock>(
        &self,
        entry: &RecordIndexEntry,
        scratch: &mut Vec<u8>,
    ) -> Result<T> {
        self.file.decode_block_into::<T>(entry, scratch)
    }

    pub fn key_tail_offsets<T>(&self) -> Result<HashMap<T::Key, u64>>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.key_tail_offsets::<T>()
    }

    /// See [`VarlitFile::key_tail_offsets_into`].
    pub fn key_tail_offsets_into<T>(&self, out: &mut HashMap<T::Key, u64>) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.key_tail_offsets_into::<T>(out)
    }

    pub fn read_matrix_cell<T: VarlitMatrixBlock>(&self, key: MatrixKey) -> Result<T> {
        self.file.read_matrix_cell(key)
    }

    /// See [`VarlitFile::read_matrix_run_into`].
    pub fn read_matrix_run_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [T],
    ) -> Result<()> {
        self.file.read_matrix_run_into(start, out)
    }

    /// See [`VarlitFile::read_matrix_run_bytes_into`].
    pub fn read_matrix_run_bytes_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        self.file.read_matrix_run_bytes_into::<T>(start, out)
    }

    /// See [`VarlitFile::read_matrix_column_into`].
    pub fn read_matrix_column_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [T],
    ) -> Result<()> {
        self.file.read_matrix_column_into(start, out)
    }

    /// See [`VarlitFile::read_matrix_column_bytes_into`].
    pub fn read_matrix_column_bytes_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        self.file.read_matrix_column_bytes_into::<T>(start, out)
    }

    /// See [`VarlitFile::read_matrix_row_into`].
    pub fn read_matrix_row_into<T: VarlitMatrixBlock>(
        &self,
        row: u64,
        out: &mut [T],
    ) -> Result<()> {
        self.file.read_matrix_row_into(row, out)
    }

    pub fn matrix_cell_payload<T: VarlitMatrixBlock>(&self, key: MatrixKey) -> Result<Vec<u8>> {
        self.file.matrix_cell_payload::<T>(key)
    }

    pub fn matrix_aux_len(&self, name: &str) -> Result<u64> {
        self.file.matrix_aux_len(name)
    }

    pub fn read_matrix_aux(&self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        self.file.read_matrix_aux(name, offset, len)
    }

    pub fn matrix_cell_status<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        self.file.matrix_cell_status::<T>(key)
    }

    pub fn is_matrix_single_committed(&self, name: &str) -> Result<bool> {
        self.file.is_matrix_single_committed(name)
    }

    pub fn is_matrix_channel_committed(&self, name: &str, channel: u64) -> Result<bool> {
        self.file.is_matrix_channel_committed(name, channel)
    }

    pub fn matrix_resume_signal(&self, category: &str) -> Result<MatrixResumeSignal> {
        self.file.matrix_resume_signal(category)
    }

    pub fn matrix_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_sidecar_resume_signal(category, sidecar_path)
    }

    pub fn matrix_verified_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_verified_sidecar_resume_signal(category, sidecar_path)
    }

    pub fn matrix_verified_sidecar_resume_signal_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_verified_sidecar_resume_signal_with_generation(
                category,
                sidecar_path,
                expected_generation,
            )
    }

    pub fn read_matrix_sidecar<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        self.file.read_matrix_sidecar(category, sidecar_path)
    }

    pub fn read_matrix_sidecar_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        self.file
            .read_matrix_sidecar_with_generation(category, sidecar_path, expected_generation)
    }

    pub fn matrix_recovery_report(&self) -> MatrixRecoveryReport {
        self.file.matrix_recovery_report()
    }
    /// Per-file sparse/allocation observations captured at create/open.
    pub fn matrix_storage_diagnostics(&self) -> Option<crate::MatrixStorageDiagnostics> {
        self.file.matrix_storage_diagnostics()
    }

    /// Verifies this matrix's commit metadata now, and reports what was found.
    ///
    /// See [`VarlitFile::verify_matrix_metadata`]: the same pass a
    /// `MatrixMetadataVerification::AtOpen` open runs, on demand, retaining one
    /// page buffer. It reports; it does not arm the quarantine.
    pub fn verify_matrix_metadata(&self) -> Result<MatrixRecoveryReport> {
        self.file.verify_matrix_metadata()
    }

    #[cfg(feature = "mmap")]
    /// Maps the file's indexed record payloads as a read-only snapshot.
    ///
    /// # Safety
    ///
    /// The caller must prevent mutation, truncation, replacement, backing-file
    /// invalidation, and any other modification through every handle, thread,
    /// and process for the full lifetime of the returned mapping.
    ///
    /// Once that condition holds, Varlit keeps safe accessors sound by cloning
    /// the existing backing handle, creating a read-only mapping, validating
    /// every indexed extent with checked arithmetic, rejecting entries outside
    /// the copied snapshot, and tying every returned slice to the mapping owner.
    /// Use owned reads instead when external immutability cannot be guaranteed.
    ///
    /// **The file header is not mapped.** The mapping begins at the start of
    /// the append log, so the bytes a commit can rewrite — the
    /// `IndexPolicy::header_tails` region among them — are outside every
    /// reference this type constructs. Current growing chunks append immutable
    /// record versions rather than rewriting an existing chunk record. This
    /// does not waive the safety condition: exclusive in-place replacement,
    /// liveness updates, truncation and external writes can still change the
    /// mapped backing object. Use owned reads when those operations cannot be
    /// excluded for the mapping's lifetime.
    pub unsafe fn mmap_payloads(&self) -> Result<MmapPayloads> {
        // SAFETY: The caller accepts the complete file-backed mapping contract.
        unsafe { self.file.mmap_payloads() }
    }

    #[cfg(feature = "mmap")]
    /// Maps one committed cell; see [`VarlitFile::mmap_matrix_cell`].
    ///
    /// # Safety
    ///
    /// The backing object must remain immutable and valid for the mapping's lifetime.
    pub unsafe fn mmap_matrix_cell<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MmapPayloadWindow> {
        // SAFETY: the caller upholds the underlying constructor's contract.
        unsafe { self.file.mmap_matrix_cell::<T>(key) }
    }

    #[cfg(feature = "mmap")]
    /// Maps the file's matrix regions as a read-only snapshot.
    ///
    /// # Safety
    ///
    /// The caller must prevent mutation, truncation, replacement, backing-file
    /// invalidation, and any other modification through every handle, thread,
    /// and process for the full lifetime of the returned mapping.
    ///
    /// Once that condition holds, Varlit keeps safe accessors sound by cloning
    /// the existing backing handle, creating a read-only mapping, validating
    /// the complete matrix extent, checking every requested slot, and tying
    /// every returned slice to the mapping owner. Use owned reads instead when
    /// external immutability cannot be guaranteed.
    ///
    /// Unlike [`Self::mmap_payloads`] this mapping *does* start at byte 0 and so
    /// covers the file header: the matrix region it exists to serve sits between
    /// the header and the append log, so there is no base to move it to. That is
    /// sound because the one option that rewrites header bytes,
    /// `IndexPolicy::header_tails`, is refused for a format declaring matrix
    /// blocks — the two cannot meet.
    pub unsafe fn mmap_matrix(&self) -> Result<MmapMatrix> {
        // SAFETY: The caller accepts the complete file-backed mapping contract.
        unsafe { self.file.mmap_matrix() }
    }
}

#[derive(Debug)]
pub struct VarlitWriter {
    file: VarlitFile,
}

impl VarlitWriter {
    pub fn compact_matrix(&mut self) -> Result<()> {
        self.file.compact_matrix()
    }

    pub fn matrix_generation(&self) -> Option<u64> {
        self.file.matrix_generation()
    }
    /// Required companion file for matrix storage; copy/rename it with the primary.
    pub fn matrix_generation_path(&self) -> Option<&Path> {
        self.file.matrix_generation_path()
    }

    pub fn create<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::create(spec, path)?,
        })
    }

    /// Creates a new native file, failing if `path` already exists.
    ///
    /// See [`VarlitFile::create_new`].
    pub fn create_new<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::create_new(spec, path)?,
        })
    }

    pub fn create_with_dims<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::create_with_dims(spec, path, dims)?,
        })
    }

    /// Creates a new native matrix file, failing if `path` already exists.
    ///
    /// See [`VarlitFile::create_new_with_dims`].
    pub fn create_new_with_dims<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::create_new_with_dims(spec, path, dims)?,
        })
    }

    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open(spec, path)?,
        })
    }

    /// See [`VarlitFile::open_lazy`].
    pub fn open_lazy<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_lazy(spec, path)?,
        })
    }

    /// See [`VarlitFile::open_lazy_with_report`].
    pub fn open_lazy_with_report<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
    ) -> Result<(Self, LazyOpenSource)> {
        let (file, source) = VarlitFile::open_lazy_with_report(spec, path)?;
        Ok((Self { file }, source))
    }

    /// See [`VarlitFile::open_with_scratch`].
    pub fn open_with_scratch<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        scratch: &mut Vec<RecordIndexEntry>,
    ) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_with_scratch(spec, path, scratch)?,
        })
    }

    pub fn open_with_lock_policy<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        policy: WriterLockBreakPolicy,
    ) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_with_lock_policy(spec, path, policy)?,
        })
    }

    pub fn open_recover<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_recover(spec, path)?,
        })
    }

    pub fn open_recover_with_report<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
    ) -> Result<(Self, RecoveryReport)> {
        let (file, report) = VarlitFile::open_recover_with_report(spec, path)?;
        Ok((Self { file }, report))
    }

    pub fn into_inner(self) -> VarlitFile {
        self.file
    }

    pub fn spec(&self) -> FormatSpec {
        self.file.spec()
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn mode(&self) -> OpenMode {
        self.file.mode()
    }

    /// Verifies every record's stored checksum. See [`VarlitFile::verify_all`].
    pub fn verify_all(&self) -> Result<usize> {
        self.file.verify_all()
    }

    /// The open file this writer holds, borrowed rather than consumed.
    ///
    /// `into_inner` is the only other route to it and it takes `self`, which
    /// makes every `&mut self` entry point on [`VarlitFile`] unreachable from a
    /// writer that is still being written through. Reaching one is what a
    /// caller wants when the capability has no forwarding method here yet;
    /// the header-slot family below is forwarded precisely so that this is not
    /// needed for it.
    pub fn file_mut(&mut self) -> &mut VarlitFile {
        &mut self.file
    }

    /// The open file this writer holds, borrowed for reading.
    pub fn file(&self) -> &VarlitFile {
        &self.file
    }

    /// Reads a block out of the editable header region.
    /// See [`VarlitFile::read_header_block`].
    ///
    /// Forwarded because the writer is the handle that knows what to record
    /// there: an append returns its own `record_offset`, and writing that
    /// offset into the region is what lets a later reader seed a walk without
    /// rebuilding an index. Reaching `write_header_block` used to mean
    /// `into_inner`, which ends the writer.
    pub fn read_header_block<T: VarlitBlock>(&self) -> Result<Option<T>> {
        self.file.read_header_block::<T>()
    }

    /// Writes a block into the editable header region, replacing any earlier
    /// value for the same block id. See [`VarlitFile::write_header_block`].
    pub fn write_header_block<T: VarlitBlock>(&mut self, block: &T) -> Result<()> {
        self.file.write_header_block(block)
    }

    /// Removes a block from the editable header region, freeing its bytes.
    /// See [`VarlitFile::remove_header_block`].
    pub fn remove_header_block<T: VarlitBlock>(&mut self) -> Result<()> {
        self.file.remove_header_block::<T>()
    }

    /// Fixes the region's checksum and refuses every later write.
    /// See [`VarlitFile::seal_header_slots`].
    pub fn seal_header_slots(&mut self) -> Result<()> {
        self.file.seal_header_slots()
    }

    /// The declared size of the editable header region; `0` when undeclared.
    pub fn header_slots_capacity(&self) -> usize {
        self.file.header_slots_capacity()
    }

    /// How much of the region the current blocks occupy.
    pub fn header_slots_used(&self) -> Result<usize> {
        self.file.header_slots_used()
    }

    /// How much of the region is left.
    pub fn header_slots_free_bytes(&self) -> Result<usize> {
        self.file.header_slots_free_bytes()
    }

    /// Whether the region's seal is down.
    pub fn header_slots_sealed(&self) -> Result<bool> {
        self.file.header_slots_sealed()
    }

    pub fn index_entries(&self) -> IndexEntries {
        self.file.index_entries()
    }

    /// Fills the caller's buffer with the record index; see
    /// [`VarlitFile::index_entries_into`].
    pub fn index_entries_into(&self, out: &mut Vec<RecordIndexEntry>) -> Result<()> {
        self.file.index_entries_into(out)
    }

    /// See [`VarlitFile::with_directory`].
    pub fn with_directory<'a, D: RecordDirectory + ?Sized>(
        &'a self,
        directory: &'a D,
    ) -> Result<DirectoryRead<'a, D>> {
        self.file.with_directory(directory)
    }

    /// See [`VarlitFile::record_map`].
    pub fn record_map<'a>(&self, buffer: &'a mut Vec<RecordIndexEntry>) -> Result<RecordMap<'a>> {
        self.file.record_map(buffer)
    }

    /// See [`VarlitFile::read_payload_into`].
    pub fn read_payload_into(&self, entry: &RecordIndexEntry, out: &mut Vec<u8>) -> Result<()> {
        self.file.read_payload_into(entry, out)
    }

    /// See [`VarlitFile::read_logical_payload_into`].
    pub fn read_logical_payload_into(
        &self,
        entry: &RecordIndexEntry,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        self.file.read_logical_payload_into(entry, out)
    }

    /// See [`VarlitFile::decode_block_into`].
    pub fn decode_block_into<T: VarlitBlock>(
        &self,
        entry: &RecordIndexEntry,
        scratch: &mut Vec<u8>,
    ) -> Result<T> {
        self.file.decode_block_into::<T>(entry, scratch)
    }

    pub fn key_tail_offsets<T>(&self) -> Result<HashMap<T::Key, u64>>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.key_tail_offsets::<T>()
    }

    /// See [`VarlitFile::key_tail_offsets_into`].
    pub fn key_tail_offsets_into<T>(&self, out: &mut HashMap<T::Key, u64>) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.key_tail_offsets_into::<T>(out)
    }

    // `VarlitWriter::reserve_keyed_tail_slot` has been
    // removed.
    //
    // It existed so a generated keyed writer could reserve a slot in its own
    // `HashMap<T::Key, u64>` before the append that would fill it. That
    // reserved the map's inline table, but it could not stop the *later*
    // `HashMap::insert` from invoking the caller's `Hash` or `Eq` after the
    // record was already authoritative - a stateful or panicking
    // implementation could therefore fail after publication and leave the
    // writer holding a stale predecessor, which makes the next same-key
    // mutation link *around* the committed event.
    //
    // The generated writers no longer own a typed tail map at all. They route
    // through `VarlitWriter::push_keyed_info` and `VarlitWriter::delete_info`,
    // which maintain the same byte-keyed resident cache the generic keyed API
    // uses (`VarlitFile::push_keyed_info`): its keys are the canonical
    // internal key payload, so the only step after the authoritative append is
    // a `HashMap<Vec<u8>, u64>` insert into a slot reserved beforehand. No
    // user-defined trait method can run there.
    //
    pub fn push<T: VarlitBlock>(&mut self, block: &T) -> Result<u64> {
        self.file.push(block)
    }

    pub fn push_info<T: VarlitBlock>(&mut self, block: &T) -> Result<AppendInfo> {
        self.file.push_info(block)
    }

    /// Appends a keyed block through the generic API while maintaining the
    /// keyed predecessor chain. See [`VarlitFile::push_keyed`].
    pub fn push_keyed<T>(&mut self, block: &T) -> Result<u64>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.push_keyed(block)
    }

    /// Appends a keyed block through the generic API while maintaining the
    /// keyed predecessor chain. See [`VarlitFile::push_keyed_info`].
    pub fn push_keyed_info<T>(&mut self, block: &T) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.push_keyed_info(block)
    }

    pub fn push_with_prev_key_info<T: VarlitBlock>(
        &mut self,
        block: &T,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        self.file
            .push_with_prev_key_info(block, prev_same_key_offset)
    }

    pub fn delete<T>(&mut self, key: &T::Key) -> Result<u64>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.delete::<T>(key)
    }

    /// Appends a tombstone through the maintained keyed path. See
    /// [`VarlitFile::delete_info`]. This is the entry point the generated
    /// `delete_<block>` writer methods use.
    pub fn delete_info<T>(&mut self, key: &T::Key) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.delete_info::<T>(key)
    }

    /// Builds `T`'s keyed tail map now. See [`VarlitFile::prime_keyed_tails`].
    /// The generated keyed writers call this once per keyed block at
    /// construction.
    #[doc(hidden)]
    pub fn prime_keyed_tails<T>(&mut self) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.prime_keyed_tails::<T>()
    }

    pub fn delete_with_prev_key_info<T>(
        &mut self,
        key: &T::Key,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
    {
        self.file
            .delete_with_prev_key_info::<T>(key, prev_same_key_offset)
    }

    pub fn push_op<T>(&mut self, key: &T::Key, op: &T::Op) -> Result<u64>
    where
        T: VarlitMerge,
    {
        self.file.push_op::<T>(key, op)
    }

    pub fn write_metadata(&mut self, key: &str, value: &[u8]) -> Result<u64> {
        self.file.write_metadata(key, value)
    }

    /// Replaces a native user record while preserving its sequence number.
    ///
    /// [`Error::PublishedButRebindFailed`] means publication already succeeded.
    /// Do not retry blindly; discard this poisoned writer and reopen the path.
    ///
    /// [`Error::PublishedButParentSyncPending`] also means publication already
    /// succeeded; the writer was rebound to the published generation and stays
    /// usable, but the rename is not yet guaranteed durable against power loss
    /// until the parent directory is synced (for example by a later successful
    /// publication or an explicit directory sync).
    pub fn replace_block<T: VarlitReplaceBlock>(
        &mut self,
        index: usize,
        block: &T,
    ) -> Result<ReplacementInfo> {
        self.file.replace_block(index, block)
    }

    /// Publishes a same-size fixed replacement through a copy-on-write file
    /// generation.
    ///
    /// [`Error::PublishedButRebindFailed`] means publication already succeeded.
    /// Do not retry blindly; discard this poisoned writer and reopen the path.
    ///
    /// [`Error::PublishedButParentSyncPending`] also means publication already
    /// succeeded; the writer was rebound to the published generation and stays
    /// usable, but the rename is not yet guaranteed durable against power loss
    /// until the parent directory is synced (for example by a later successful
    /// publication or an explicit directory sync).
    /// See [`VarlitFile::defragment`].
    pub fn defragment(&mut self) -> Result<DefragmentReport> {
        self.file.defragment()
    }

    /// See [`VarlitFile::mark_record_dead`].
    pub fn mark_record_dead(&mut self, record_offset: u64) -> Result<()> {
        self.file.mark_record_dead(record_offset)
    }

    /// See [`VarlitFile::record_is_dead`].
    pub fn record_is_dead(&self, record_offset: u64) -> Result<bool> {
        self.file.record_is_dead(record_offset)
    }

    /// See [`VarlitFile::opened_after_crash`].
    pub const fn opened_after_crash(&self) -> bool {
        self.file.opened_after_crash()
    }

    pub fn replace_fixed<T: VarlitBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        self.file.replace_fixed(index, block)
    }

    /// Replaces a fixed record by mutating the backing object directly.
    ///
    /// # Errors
    ///
    /// See [`VarlitFile::replace_fixed_in_place_exclusive`]: a stored record
    /// whose version differs from `T::VERSION` is refused with
    /// [`Error::BlockVersionMismatch`] .
    ///
    /// # Safety
    ///
    /// See [`VarlitFile::replace_fixed_in_place_exclusive`] for the full
    /// contract, including the requirement that a keyed record's key must not
    /// change.
    ///
    /// The caller must exclude every reader, writer, mapping, raw reference,
    /// handle, thread, and process for this operation and for the lifetime of
    /// every view that could observe the affected object.
    pub unsafe fn replace_fixed_in_place_exclusive<T: VarlitBlock>(
        &mut self,
        index: usize,
        block: &T,
    ) -> Result<u64> {
        // SAFETY: The caller accepts the exclusivity contract documented above.
        unsafe { self.file.replace_fixed_in_place_exclusive(index, block) }
    }

    /// Publishes a replacement by rewriting the complete file generation.
    ///
    /// # Not for a format that writes a record footer
    ///
    /// Refused with `InvalidFormatSpec("replace is not supported for
    /// record-footer formats")`, which is every format declaring an offset
    /// chain, `segment_on_flush`, or any commit policy other than
    /// [`CommitPolicy::None`]. Use [`replace_block`](Self::replace_block)
    /// there: it does the same whole-file republish and does maintain the
    /// footer, translating both chain offsets and checking each predecessor
    /// against the prefix it has already written.
    ///
    /// The refusal is a statement about *this* routine rather than about the
    /// operation. Its record writer emits header and payload and stops — no
    /// footer, and both chain fields cleared — so a file it produced for such a
    /// format would frame perfectly and carry no chain at all.
    ///
    /// [`Error::PublishedButRebindFailed`] means publication already succeeded.
    /// Do not retry blindly; discard this poisoned writer and reopen the path.
    ///
    /// [`Error::PublishedButParentSyncPending`] also means publication already
    /// succeeded; the writer was rebound to the published generation and stays
    /// usable, but the rename is not yet guaranteed durable against power loss
    /// until the parent directory is synced (for example by a later successful
    /// publication or an explicit directory sync).
    pub fn replace_rewrite<T: VarlitBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        self.file.replace_rewrite(index, block)
    }

    /// See [`VarlitFile::replace`].
    pub fn replace<T: VarlitReplaceBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        self.file.replace(index, block)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()
    }

    pub fn commit(&mut self) -> Result<AppendInfo> {
        self.file.commit()
    }

    pub fn commit_durable(&mut self) -> Result<AppendInfo> {
        self.file.commit_durable()
    }

    pub fn sync(&mut self) -> Result<()> {
        self.file.sync()
    }

    pub fn immediate(&mut self) -> Result<()> {
        self.file.immediate()
    }

    pub fn set_immediate_policy(&mut self, policy: crate::ImmediatePolicy) -> Result<()> {
        self.file.set_immediate_policy(policy)
    }

    pub fn write_matrix_cell<T: VarlitMatrixBlock>(
        &mut self,
        key: MatrixKey,
        value: &T,
    ) -> Result<()> {
        self.file.write_matrix_cell(key, value)
    }

    /// Copies one cell's bytes out of `source`.
    ///
    /// `source` is a **shared** borrow: the read side of a byte copy needs no
    /// exclusive handle, so a copy may run while other threads are reading the
    /// same source handle. Callers that were passing `&mut VarlitReader` still
    /// compile — `&mut T` coerces to `&T`.
    pub fn copy_matrix_cell_bytes_from<From, To>(
        &mut self,
        source: &VarlitReader,
        key: MatrixKey,
    ) -> Result<()>
    where
        From: VarlitMatrixBlock,
        To: VarlitMatrixBlock,
    {
        ensure_matrix_byte_copy_compatible::<From, To>()?;
        let payload = source.matrix_cell_payload::<From>(key)?;
        self.file.write_matrix_cell_payload::<To>(key, &payload)?;
        self.file.commit_matrix_cell::<To>(key)
    }

    /// Writes a cell, runs the barriers, publishes and syncs its generation,
    /// then calls `hook`. Other pending matrix changes are published too.
    ///
    /// Barrier failures are pre-publication errors and poison the writer.
    /// [`Error::MatrixPublicationUncertain`] requires reopening and inspecting
    /// the confirmed generation before retrying. A complete head whose final
    /// durability request failed yields [`Error::MatrixCommittedButDurabilityUnproven`].
    /// Both skip the hook. After successful durable publication, hook failure
    /// yields [`Error::MatrixCommittedButHookFailed`]; retry only the notification.
    /// Outcome boxes are reserved fallibly before mutation; a late error needs
    /// no further allocation. See "Allocator Failure And Published Outcomes"
    /// in `docs/durability-model.md`.
    pub fn write_matrix_cell_durable<T, F>(
        &mut self,
        key: MatrixKey,
        value: &T,
        hook: F,
    ) -> Result<()>
    where
        T: VarlitMatrixBlock,
        F: FnOnce(MatrixCommitEvent) -> Result<()>,
    {
        self.file.write_matrix_cell_durable(key, value, hook)
    }

    /// [`Self::write_matrix_cell_durable`] with a caller-supplied durability
    /// barrier; the same post-commit contract applies, including
    /// [`Error::MatrixCommittedButHookFailed`] and
    /// [`Error::MatrixCommittedButDurabilityUnproven`]. Both barrier callbacks
    /// precede generation publication and receive the physical generation file.
    pub fn write_matrix_cell_durable_with_barrier<T, B, F>(
        &mut self,
        key: MatrixKey,
        value: &T,
        barrier: &mut B,
        hook: F,
    ) -> Result<()>
    where
        T: VarlitMatrixBlock,
        B: MatrixDurabilityBarrier + ?Sized,
        F: FnOnce(MatrixCommitEvent) -> Result<()>,
    {
        self.file
            .write_matrix_cell_durable_with_barrier(key, value, barrier, hook)
    }

    pub fn read_matrix_cell<T: VarlitMatrixBlock>(&self, key: MatrixKey) -> Result<T> {
        self.file.read_matrix_cell(key)
    }

    /// See [`VarlitFile::read_matrix_run_into`].
    pub fn read_matrix_run_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [T],
    ) -> Result<()> {
        self.file.read_matrix_run_into(start, out)
    }

    /// See [`VarlitFile::read_matrix_run_bytes_into`].
    pub fn read_matrix_run_bytes_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        self.file.read_matrix_run_bytes_into::<T>(start, out)
    }

    /// See [`VarlitFile::read_matrix_column_into`].
    pub fn read_matrix_column_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [T],
    ) -> Result<()> {
        self.file.read_matrix_column_into(start, out)
    }

    /// See [`VarlitFile::read_matrix_column_bytes_into`].
    pub fn read_matrix_column_bytes_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        self.file.read_matrix_column_bytes_into::<T>(start, out)
    }

    /// See [`VarlitFile::read_matrix_row_into`].
    pub fn read_matrix_row_into<T: VarlitMatrixBlock>(
        &self,
        row: u64,
        out: &mut [T],
    ) -> Result<()> {
        self.file.read_matrix_row_into(row, out)
    }

    pub fn matrix_cell_payload<T: VarlitMatrixBlock>(&self, key: MatrixKey) -> Result<Vec<u8>> {
        self.file.matrix_cell_payload::<T>(key)
    }

    pub fn matrix_aux_len(&self, name: &str) -> Result<u64> {
        self.file.matrix_aux_len(name)
    }

    pub fn read_matrix_aux(&self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        self.file.read_matrix_aux(name, offset, len)
    }

    pub fn write_matrix_aux(&mut self, name: &str, offset: u64, payload: &[u8]) -> Result<()> {
        self.file.write_matrix_aux(name, offset, payload)
    }

    pub fn matrix_cell_status<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        self.file.matrix_cell_status::<T>(key)
    }

    pub fn commit_matrix_cell<T: VarlitMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        self.file.commit_matrix_cell::<T>(key)
    }

    pub fn clear_matrix_cell<T: VarlitMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        self.file.clear_matrix_cell::<T>(key)
    }

    pub fn clear_matrix_cell_by_category(&mut self, category: &str, key: MatrixKey) -> Result<()> {
        self.file.clear_matrix_cell_by_category(category, key)
    }

    pub fn clear_matrix_category(&mut self, category: &str) -> Result<u64> {
        self.file.clear_matrix_category(category)
    }

    pub fn apply_matrix_recovery_action(&mut self, action: &MatrixRecoveryAction) -> Result<()> {
        self.file.apply_matrix_recovery_action(action)
    }

    pub fn rebuild_matrix_commit_from_crc<T: VarlitMatrixBlock>(&mut self) -> Result<u64> {
        self.file.rebuild_matrix_commit_from_crc::<T>()
    }

    pub fn is_matrix_single_committed(&self, name: &str) -> Result<bool> {
        self.file.is_matrix_single_committed(name)
    }

    pub fn set_matrix_single_committed(&mut self, name: &str, value: bool) -> Result<()> {
        self.file.set_matrix_single_committed(name, value)
    }

    pub fn is_matrix_channel_committed(&self, name: &str, channel: u64) -> Result<bool> {
        self.file.is_matrix_channel_committed(name, channel)
    }

    pub fn set_matrix_channel_committed(
        &mut self,
        name: &str,
        channel: u64,
        value: bool,
    ) -> Result<()> {
        self.file.set_matrix_channel_committed(name, channel, value)
    }

    pub fn matrix_resume_signal(&self, category: &str) -> Result<MatrixResumeSignal> {
        self.file.matrix_resume_signal(category)
    }

    pub fn matrix_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_sidecar_resume_signal(category, sidecar_path)
    }

    pub fn matrix_verified_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_verified_sidecar_resume_signal(category, sidecar_path)
    }

    pub fn matrix_verified_sidecar_resume_signal_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_verified_sidecar_resume_signal_with_generation(
                category,
                sidecar_path,
                expected_generation,
            )
    }

    pub fn write_matrix_sidecar<P: AsRef<Path>>(
        &mut self,
        category: &str,
        sidecar_path: P,
        generation: u64,
        payload: &[u8],
    ) -> Result<MatrixSidecarManifest> {
        self.file
            .write_matrix_sidecar(category, sidecar_path, generation, payload)
    }

    pub fn read_matrix_sidecar<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        self.file.read_matrix_sidecar(category, sidecar_path)
    }

    pub fn read_matrix_sidecar_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        self.file
            .read_matrix_sidecar_with_generation(category, sidecar_path, expected_generation)
    }

    pub fn matrix_recovery_report(&self) -> MatrixRecoveryReport {
        self.file.matrix_recovery_report()
    }
    /// Per-file sparse/allocation observations captured at create/open.
    pub fn matrix_storage_diagnostics(&self) -> Option<crate::MatrixStorageDiagnostics> {
        self.file.matrix_storage_diagnostics()
    }

    /// Verifies this matrix's commit metadata now, and reports what was found.
    ///
    /// See [`VarlitFile::verify_matrix_metadata`]: the same pass a
    /// `MatrixMetadataVerification::AtOpen` open runs, on demand, retaining one
    /// page buffer. It reports; it does not arm the quarantine.
    pub fn verify_matrix_metadata(&self) -> Result<MatrixRecoveryReport> {
        self.file.verify_matrix_metadata()
    }
}

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

/// Frames one record through the snapshot, without touching the resident index.
pub(crate) fn read_index_entry_from_descriptor(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    record_offset: u64,
    sequence: u64,
    physical: crate::disk_index::DiskIndexPhysicalRecord,
) -> Result<RecordIndexEntry> {
    // The sidecar lacks the compression-length hint. Read the native frame once
    // for compressed records; payload access still avoids a second frame read.
    if physical.flags & RECORD_FLAG_COMPRESSED != 0 {
        return read_record_entry_positional(spec, snapshot, record_offset);
    }
    let pointer = snapshot.validate(
        crate::scalable_extent::UntrustedRecordPointer::new(record_offset, physical.physical_len),
        spec.spec_needs_record_footer(),
    )?;
    let span = pointer.span();
    let mut entry = RecordIndexEntry {
        block_id: physical.block_id,
        block_version: physical.block_version,
        flags: physical.flags,
        sequence,
        record_offset,
        payload_offset: span.payload_offset().get(),
        payload_len: span.payload_len().get(),
        checksum: physical.native_crc.unwrap_or(0),
        uncompressed_len_hint: 0,
        footer_offset: spec
            .spec_needs_record_footer()
            .then_some(span.footer_offset().get()),
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        mutable_flags: 0,
        committed: true,
    };
    validate_record_entry(&spec, &entry)?;
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, entry.payload_len)?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, entry.payload_len)?;
    if spec.read_validation().data_crc {
        if physical.native_crc.is_none() {
            return Err(Error::InvalidIndexCheckpoint);
        }
        // Footer bytes participate in data CRC. The sidecar does not store
        // chain links, so CRC-only mode still needs this one footer read.
        if let Some(at) = entry.footer_offset {
            let mut bytes = [0; RECORD_FOOTER_LEN as usize];
            snapshot.read_exact_at(at, &mut bytes)?;
            let footer = decode_record_footer(spec, &bytes, at, record_offset)?;
            entry.prev_same_block_offset = footer.prev_same_block_offset;
            entry.prev_same_key_offset = footer.prev_same_key_offset;
            entry.mutable_flags = footer.mutable_flags;
        }
    }
    Ok(entry)
}

pub(crate) fn read_record_entry_positional(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    record_offset: u64,
) -> Result<RecordIndexEntry> {
    let mut header = [0u8; RECORD_HEADER_LEN as usize];
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
        let mut footer = [0u8; RECORD_FOOTER_LEN as usize];
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
    if let Some(matrix) = &file.matrix {
        return Ok(matrix.append_log_start());
    }
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

/// Checks a live file against the record directory that indexes it.
///
/// **What this can still prove, and what it no longer can.** The directory
/// holds each record's offset and nothing else; the entry it produces is
/// rebuilt from the record's own header. So `physical_record_headers_match`
/// below compares a header against itself here and cannot fail — the header
/// fields of an indexed record are no longer a fact the index could disagree
/// with the file about, because the index does not store them.
///
/// What the walk still establishes is everything that was ever load-bearing on
/// this path: the directory's offsets are contiguous from `append_start`, they
/// cover the file exactly (`offset != file_len` is the tail refusal), every
/// record's stored checksum verifies against its bytes, every checkpoint's
/// declared ceilings hold, and no sequence is reused. A file replaced or
/// truncated under the writer fails the first or the second of those.
fn validate_generation_index(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    index: &ResidentIndex,
) -> Result<()> {
    validate_generation_file_inner(spec, file, append_start, index, false)
}

/// The same walk against entries the caller is still holding — the rewrite
/// paths, which validate the generation they have just built before it is
/// published and so have no directory for it yet.
fn validate_generation_file(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    expected_entries: &dyn RecordDirectory,
) -> Result<()> {
    validate_generation_file_inner(spec, file, append_start, expected_entries, false)
}

fn validate_replacement_generation_file(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    expected_entries: &dyn RecordDirectory,
) -> Result<()> {
    validate_generation_file_inner(spec, file, append_start, expected_entries, true)
}

/// Strict replacement validation compares checkpoint rows incrementally with
/// the already validated prefix. Other callers screen only the fixed header.
fn validate_generation_file_inner(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    expected_entries: &dyn RecordDirectory,
    strict_prefix: bool,
) -> Result<()> {
    let file_len = check_open_file_len(spec, file)?;
    let header_len = read_file_header(spec, file)?;
    if append_start < header_len || append_start > file_len {
        return Err(Error::InvalidCanonicalEncoding(
            "invalid native append-log boundary",
        ));
    }
    let mut accounting = ScanAccounting::default();
    accounting.advance(spec, append_start)?;
    let mut offset = append_start;
    // Increasing runs with disjoint ranges prove uniqueness without scratch.
    // Overlapping ranges take the bounded external-sort fallback.
    let mut sequences = native_rewrite::SequenceRanges::default();
    let mut walked = 0usize;
    for (position, expected) in directory_records(expected_entries).enumerate() {
        let expected = expected?;
        let expected = &expected;
        walked += 1;
        if expected.record_offset != offset {
            return Err(Error::InvalidCanonicalEncoding(
                "native record offsets are not contiguous",
            ));
        }
        let actual = match read_record_entry_at(
            spec,
            file,
            file_len,
            offset,
            // Generation validation proves a just-published file matches what
            // was indexed; verifying is the point of this walk, so it does not
            // consult the open-time policy.
            ScanChecks {
                partial_boundary: None,
                checksum_boundary: None,
                verify_checksums: true,
            },
            &mut accounting,
        )? {
            RecordRead::Entry(entry) => entry,
            RecordRead::RecoverableTail(tail) => {
                return Err(Error::CorruptTail {
                    offset: tail.offset,
                });
            }
        };
        if !physical_record_headers_match(expected, &actual) {
            return Err(Error::InvalidCanonicalEncoding(
                "native record changed after it was indexed",
            ));
        }
        if actual.block_id == INDEX_BLOCK_ID
            && (1..=INDEX_CHECKPOINT_VERSION).contains(&actual.block_version)
        {
            if strict_prefix {
                native_rewrite::validate_checkpoint(
                    spec,
                    file,
                    &actual,
                    expected_entries,
                    position,
                    append_start,
                )?;
            } else {
                let prefix = actual.read_payload_prefix_file_with_len(
                    file,
                    file_len,
                    INDEX_CHECKPOINT_PREFIX_LEN,
                )?;
                check_index_checkpoint_limits_for(spec, &prefix, false)?;
            }
        }
        sequences.note(actual.sequence);
        offset = actual.checked_physical_end()?;
    }
    debug_assert_eq!(
        walked,
        expected_entries.record_count(),
        "declared entry count did not hold"
    );
    if offset != file_len {
        return Err(Error::CorruptTail { offset });
    }
    if sequences.proves_unique() {
        return Ok(());
    }
    native_rewrite::validate_sequences(expected_entries, resident_index::buffer_bytes(spec) / 2)
}

fn physical_record_headers_match(left: &RecordIndexEntry, right: &RecordIndexEntry) -> bool {
    left.block_id == right.block_id
        && left.block_version == right.block_version
        && left.flags == right.flags
        && left.sequence == right.sequence
        && left.record_offset == right.record_offset
        && left.payload_offset == right.payload_offset
        && left.payload_len == right.payload_len
        && left.checksum == right.checksum
        && left.uncompressed_len_hint == right.uncompressed_len_hint
        && left.footer_offset == right.footer_offset
        && left.prev_same_block_offset == right.prev_same_block_offset
        && left.prev_same_key_offset == right.prev_same_key_offset
}

#[derive(Clone, Copy)]
enum RewritePayload<'a> {
    Bytes(&'a [u8]),
    Snapshot { offset: u64, len: u64 },
    Index(IndexPayload<'a>),
}

fn rewrite_record_streaming(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    output: &mut File,
    mut entry: RecordIndexEntry,
    payload: RewritePayload<'_>,
) -> Result<RecordIndexEntry> {
    let payload_len = match payload {
        RewritePayload::Bytes(bytes) => {
            u64::try_from(bytes.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?
        }
        RewritePayload::Snapshot { len, .. } => len,
        RewritePayload::Index(index) => index.len(spec)?,
    };
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    let logical_len = match payload {
        RewritePayload::Bytes(bytes) => record_logical_len_from_parts(
            spec,
            entry.block_id,
            entry.flags,
            entry.uncompressed_len_hint,
            bytes,
        )?,
        RewritePayload::Snapshot { .. } => entry.logical_payload_len_snapshot(spec, snapshot)?,
        RewritePayload::Index(_) => payload_len,
    };
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;

    let record_offset = output.stream_position()?;
    let payload_offset =
        record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "rewrite file length",
            })?;
    let record_end =
        payload_offset
            .checked_add(payload_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "rewrite file length",
            })?;
    let header = RecordHeaderFields {
        block_id: entry.block_id,
        block_version: entry.block_version,
        flags: entry.flags,
        sequence: entry.sequence,
        payload_len,
        checksum: 0,
        uncompressed_len_hint: entry.uncompressed_len_hint,
    };
    write_record_header(output, spec, record_offset, header)?;
    match payload {
        RewritePayload::Bytes(bytes) => output.write_all(bytes)?,
        RewritePayload::Index(index) => index.write(output)?,
        RewritePayload::Snapshot { offset, len } => {
            snapshot.copy_range_to(offset, len, output)?;
        }
    }

    entry.record_offset = record_offset;
    entry.payload_offset = payload_offset;
    entry.payload_len = payload_len;
    entry.footer_offset = None;
    entry.prev_same_block_offset = None;
    entry.prev_same_key_offset = None;
    entry.checksum = match spec.integrity_policy {
        IntegrityPolicy::None => 0,
        IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader => {
            checksum_record_file(spec, output, &entry, &[])?
        }
    };
    output.seek(SeekFrom::Start(record_offset))?;
    write_record_header(
        output,
        spec,
        record_offset,
        RecordHeaderFields {
            checksum: entry.checksum,
            ..header
        },
    )?;
    output.seek(SeekFrom::Start(record_end))?;
    Ok(entry)
}

/// Which backward chain a link belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chain {
    Block,
    Key,
}

/// How a whole-file rewrite maps an old record offset to a new one.
///
/// The two rewrites in this crate move records for different reasons, and the
/// arithmetic that is correct for one is silently wrong for the other.
///
/// `replace_block` resizes **exactly one** record and removes none, so every
/// record after the target moves by the same amount and every record before it
/// does not move at all. That is `Uniform`, and it is why
/// [`ReplacementInfo::translate_record_offset`] can be one subtraction.
///
/// A defragmenting rewrite drops records, so each survivor moves by a different
/// amount and some chain predecessors are not in the new file at all. A uniform
/// delta there points a link at a byte offset that is not where that record
/// went.
///
/// **How badly that fails was measured rather than assumed.** Substituting
/// `Uniform` into the defragmenting rewrite does not publish a corrupt file: it
/// is refused at write time with `InvalidRecordFooter { offset: 616 }`, because
/// `validate_replacement_predecessors` resolves every link against the prefix
/// already written and requires it to name an *earlier record of the same
/// block*. Two of the four defragment tests fail that way, and nothing reaches
/// the pathname. So the honest claim is that this type is required for the
/// operation to work at all, not that it is the last thing between a caller and
/// silent corruption — that guard is the predecessor check, and it is older
/// than this.
///
/// `Tables` stores both chain resolutions in one sorted, spillable row per
/// source record. Dropped records inherit their predecessor in each chain;
/// zero means that no predecessor in that chain survived.
enum ChainRemap<'a> {
    Uniform(ReplacementInfo),
    Tables(&'a OffsetRemap),
}

impl ChainRemap<'_> {
    fn translate(&self, chain: Chain, old: u64) -> Result<Option<u64>> {
        match self {
            Self::Uniform(info) => info.translate_record_offset(old).map(Some),
            Self::Tables(remap) => remap.get(old, chain),
        }
    }
}

fn rewrite_replacement_record_streaming(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    output: &mut File,
    mut entry: RecordIndexEntry,
    payload: RewritePayload<'_>,
    replacement: ChainRemap<'_>,
    rewritten_prefix: &RewriteDirectory,
) -> Result<RecordIndexEntry> {
    let source_entry = matches!(payload, RewritePayload::Snapshot { .. }).then(|| entry.clone());
    let payload_len = match payload {
        RewritePayload::Bytes(bytes) => {
            u64::try_from(bytes.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?
        }
        RewritePayload::Snapshot { len, .. } => len,
        RewritePayload::Index(index) => index.len(spec)?,
    };
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    let logical_len = match payload {
        RewritePayload::Bytes(bytes) => record_logical_len_from_parts(
            spec,
            entry.block_id,
            entry.flags,
            entry.uncompressed_len_hint,
            bytes,
        )?,
        RewritePayload::Snapshot { .. } => entry.logical_payload_len_snapshot(spec, snapshot)?,
        RewritePayload::Index(_) => payload_len,
    };
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;

    let record_offset = output.stream_position()?;
    let payload_offset =
        record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "replacement file length",
            })?;
    let payload_end =
        payload_offset
            .checked_add(payload_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "replacement file length",
            })?;
    let record_end = payload_end.checked_add(record_footer_len(spec)).ok_or(
        Error::ResourceArithmeticOverflow {
            resource: "replacement file length",
        },
    )?;

    entry.record_offset = record_offset;
    entry.payload_offset = payload_offset;
    entry.payload_len = payload_len;
    entry.footer_offset = spec.spec_needs_record_footer().then_some(payload_end);
    entry.prev_same_block_offset = entry
        .prev_same_block_offset
        .map(|offset| replacement.translate(Chain::Block, offset))
        .transpose()?
        .flatten();
    entry.prev_same_key_offset = entry
        .prev_same_key_offset
        .map(|offset| replacement.translate(Chain::Key, offset))
        .transpose()?
        .flatten();

    let header = RecordHeaderFields {
        block_id: entry.block_id,
        block_version: entry.block_version,
        flags: entry.flags,
        sequence: entry.sequence,
        payload_len,
        checksum: 0,
        uncompressed_len_hint: entry.uncompressed_len_hint,
    };
    write_record_header(output, spec, record_offset, header)?;
    let mut copied_crc = None;
    match payload {
        RewritePayload::Bytes(bytes) => output.write_all(bytes)?,
        RewritePayload::Index(index) => index.write(output)?,
        RewritePayload::Snapshot { offset, len } => {
            if spec.integrity_policy == IntegrityPolicy::None {
                snapshot.copy_range_to(offset, len, output)?;
            } else {
                copied_crc = Some(copy_verified_snapshot_record(
                    spec,
                    snapshot,
                    output,
                    source_entry.as_ref().unwrap(),
                    offset,
                    len,
                )?);
            }
        }
    }
    let encoded_footer = if spec.spec_needs_record_footer() {
        let footer = encode_record_footer(RecordFooterFields {
            prev_same_block_offset: entry.prev_same_block_offset,
            prev_same_key_offset: entry.prev_same_key_offset,
            // Carried, not reset: a republish moves a record, it does not bring
            // it back to life.
            mutable_flags: entry.mutable_flags,
        })?;
        output.write_all(&footer)?;
        Some(footer)
    } else {
        None
    };
    let footer = encoded_footer.as_ref().map_or(&[][..], |bytes| &bytes[..]);
    validate_replacement_predecessors(output, &entry, rewritten_prefix)?;

    entry.checksum = match spec.integrity_policy {
        IntegrityPolicy::None => 0,
        IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader => {
            if let Some(mut hash) = copied_crc {
                hash.update(checksummed_footer(spec, footer));
                hash.finalize()
            } else {
                checksum_record_file(spec, output, &entry, footer)?
            }
        }
    };
    output.seek(SeekFrom::Start(record_offset))?;
    write_record_header(
        output,
        spec,
        record_offset,
        RecordHeaderFields {
            checksum: entry.checksum,
            ..header
        },
    )?;
    output.seek(SeekFrom::Start(record_end))?;
    Ok(entry)
}

/// Validate the copied source and checksum the replacement in one payload pass.
/// Snapshot copies keep the header fields intact. Forking the CRC state before
/// the changed footer avoids a second read or hash of the payload.
#[cfg(feature = "integrity")]
fn copy_verified_snapshot_record(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    output: &mut File,
    source: &RecordIndexEntry,
    offset: u64,
    len: u64,
) -> Result<crc32fast::Hasher> {
    if offset != source.payload_offset || len != source.payload_len {
        return Err(Error::InvalidCanonicalEncoding(
            "rewrite source payload changed",
        ));
    }
    // Snapshot copies preserve all header fields covered by the CRC. Only
    // physical offsets (not stored in the header) and footer links change.
    let mut payload = crc32fast::Hasher::new();
    if spec.integrity_policy == IntegrityPolicy::Crc32WithHeader {
        payload.update(&encode_native_record_header(
            RecordHeaderFields {
                block_id: source.block_id,
                block_version: source.block_version,
                flags: source.flags,
                sequence: source.sequence,
                payload_len: source.payload_len,
                checksum: 0,
                uncompressed_len_hint: source.uncompressed_len_hint,
            },
            source.record_offset,
            record_footer_len(spec),
        )?);
    }
    if len <= 1024 {
        copy_snapshot_payload_crc::<1024>(&mut payload, snapshot, output, offset, len)?;
    } else {
        copy_snapshot_payload_crc::<STREAM_BUFFER_LEN>(
            &mut payload,
            snapshot,
            output,
            offset,
            len,
        )?;
    }
    let checksum = |footer: &[u8]| {
        let mut hash = payload.clone();
        hash.update(checksummed_footer(spec, footer));
        hash.finalize()
    };
    let source_footer = if spec.spec_needs_record_footer() {
        Some(encode_record_footer(RecordFooterFields {
            prev_same_block_offset: source.prev_same_block_offset,
            prev_same_key_offset: source.prev_same_key_offset,
            mutable_flags: source.mutable_flags,
        })?)
    } else {
        None
    };
    let actual = checksum(source_footer.as_ref().map_or(&[][..], |bytes| &bytes[..]));
    if actual != source.checksum {
        return Err(Error::ChecksumMismatch {
            offset: source.record_offset,
        });
    }
    Ok(payload)
}

// Keeping each sized array in its own call frame avoids reserving the wide
// frame when a small record is copied. Source CRC and footer validation remain
// in the caller and still precede publication.
#[cfg(feature = "integrity")]
#[inline(never)]
fn copy_snapshot_payload_crc<const BUFFER: usize>(
    payload: &mut crc32fast::Hasher,
    snapshot: &SnapshotFile,
    output: &mut File,
    offset: u64,
    len: u64,
) -> Result<()> {
    const {
        assert!(BUFFER > 0);
    }
    let mut remaining = len;
    let mut at = offset;
    let mut buffer = [0; BUFFER];
    while remaining != 0 {
        let count = remaining.min(buffer.len() as u64) as usize;
        snapshot.read_exact_at(at, &mut buffer[..count])?;
        output.write_all(&buffer[..count])?;
        payload.update(&buffer[..count]);
        remaining -= count as u64;
        at = at
            .checked_add(count as u64)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "rewrite payload offset",
            })?;
    }
    Ok(())
}

#[cfg(not(feature = "integrity"))]
fn copy_verified_snapshot_record(
    _spec: FormatSpec,
    _snapshot: &SnapshotFile,
    _output: &mut File,
    _source: &RecordIndexEntry,
    _offset: u64,
    _len: u64,
) -> Result<crc32fast::Hasher> {
    Err(Error::IntegrityFeatureDisabled)
}

fn validate_replacement_predecessors(
    output: &mut File,
    entry: &RecordIndexEntry,
    rewritten_prefix: &RewriteDirectory,
) -> Result<()> {
    if let Some(offset) = entry.prev_same_block_offset {
        let Some(previous) = rewritten_prefix.find_offset(offset)? else {
            return Err(Error::InvalidRecordFooter { offset });
        };
        if offset >= entry.record_offset || previous.block_id != entry.block_id {
            return Err(Error::InvalidRecordFooter { offset });
        }
    }
    if let Some(offset) = entry.prev_same_key_offset {
        let Some(previous) = rewritten_prefix.find_offset(offset)? else {
            return Err(Error::InvalidRecordFooter { offset });
        };
        if offset >= entry.record_offset
            || replacement_chain_block_id(output, &previous)?
                != replacement_chain_block_id(output, entry)?
        {
            return Err(Error::InvalidRecordFooter { offset });
        }
    }
    Ok(())
}

fn replacement_chain_block_id(file: &mut File, entry: &RecordIndexEntry) -> Result<u32> {
    if !matches!(entry.block_id, TOMBSTONE_BLOCK_ID | OP_BLOCK_ID) {
        return Ok(entry.block_id);
    }
    if entry.payload_len < 4 {
        return Err(Error::InvalidRecordFooter {
            offset: entry.record_offset,
        });
    }
    file.seek(SeekFrom::Start(entry.payload_offset))?;
    let mut block_id = [0; 4];
    file.read_exact(&mut block_id)?;
    Ok(u32::from_le_bytes(block_id))
}

/// Record counts and a hypothetical fully resident merge size.
///
/// Merge/compact now use temporary disk indexes. The count fields still size
/// logical work and bound distinct keys. The byte fields retain the historical
/// `count * size_of` model for an equivalent fully resident implementation;
/// they do **not** estimate the external implementation's RAM or scratch disk.
/// They exclude heap-owned payloads and allocator overhead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyedMergeEstimate {
    /// Records across all inputs. An exact count.
    pub input_records: u64,
    /// Records that can introduce a distinct key: values of `T`, tombstones,
    /// and merge ops. An exact count.
    pub key_bearing_records: u64,
    /// Upper bound on `K-ever`, the number of distinct keys the merge state
    /// retains. Tombstoned keys count: the collector keeps their entries.
    pub max_distinct_keys: u64,
    /// Resident index bytes for the largest single input, which is opened
    /// whole while its records are applied:
    /// `records * size_of::<RecordIndexEntry>()`.
    pub largest_input_index_bytes: u64,
    /// Transient bytes the largest single input's open can allocate *on top of*
    /// its resident index, and which are live at the same time as that index:
    /// the sequence-uniqueness witness copies one `u64` per record when the
    /// file's sequences are not already ascending in offset order.
    pub largest_input_open_transient_bytes: u64,
    /// Structural bytes of the merge-state map's entries at
    /// [`Self::max_distinct_keys`] keys:
    /// `max_distinct_keys * size_of::<(Key, (MergeOrder, Option<T>))>()`.
    ///
    /// This is not a bound on the map: it excludes hash-table load-factor
    /// slack, control bytes and bucket rounding, and all heap owned by
    /// individual keys and values.
    pub max_state_bytes: u64,
    /// Structural bytes of the output vector, which is reserved for the
    /// surviving entries while the merge state and its values are still alive
    /// `max_distinct_keys * size_of::<(MergeOrder, T)>()`.
    ///
    /// Like [`Self::max_state_bytes`] this counts inline storage only.
    pub max_output_values_bytes: u64,
}

impl KeyedMergeEstimate {
    /// Sum of the hypothetical fully resident implementation's structural
    /// terms. This is neither current merge memory usage nor an upper bound.
    #[must_use]
    pub fn peak_resident_structural_bytes(&self) -> u64 {
        self.max_state_bytes
            .saturating_add(self.max_output_values_bytes)
            .saturating_add(self.largest_input_index_bytes)
            .saturating_add(self.largest_input_open_transient_bytes)
    }
}

/// Counts merge inputs without decoding values. Directory buffers are bounded;
/// large inputs use temporary storage. Byte fields describe a hypothetical
/// fully resident implementation; see [`KeyedMergeEstimate`].
pub fn estimate_keyed_merge<T, P>(
    spec: FormatSpec,
    base: P,
    deltas: &[P],
) -> Result<KeyedMergeEstimate>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let spec = spec.ordinary_read();
    spec.validate()?;
    crate::collections::ensure_registered_block::<T>(spec)?;
    let mut estimate = KeyedMergeEstimate::default();
    accumulate_keyed_merge_estimate::<T, _>(spec, base, &mut estimate)?;
    for delta in deltas {
        accumulate_keyed_merge_estimate::<T, _>(spec, delta, &mut estimate)?;
    }
    estimate.max_distinct_keys = estimate.key_bearing_records;
    let keys = usize::try_from(estimate.max_distinct_keys).unwrap_or(usize::MAX);
    estimate.max_state_bytes =
        allocation_bytes::<(T::Key, (MergeOrder, Option<T>))>(keys, "merge state")?;
    // Historical fully resident model, retained independently of execution.
    estimate.max_output_values_bytes = allocation_bytes::<(MergeOrder, T)>(keys, "merged values")?;
    Ok(estimate)
}

fn accumulate_keyed_merge_estimate<T, P>(
    spec: FormatSpec,
    path: P,
    estimate: &mut KeyedMergeEstimate,
) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let file = VarlitFile::open_readonly(spec, path)?;
    let records =
        u64::try_from(file.index.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "record count",
        })?;
    let mut key_bearing = 0usize;
    for entry in file.index.iter() {
        let block_id = entry?.block_id;
        if block_id == T::ID || block_id == TOMBSTONE_BLOCK_ID || block_id == OP_BLOCK_ID {
            key_bearing += 1;
        }
    }
    let key_bearing =
        u64::try_from(key_bearing).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "record count",
        })?;
    estimate.input_records = estimate.input_records.saturating_add(records);
    estimate.key_bearing_records = estimate.key_bearing_records.saturating_add(key_bearing);
    estimate.largest_input_index_bytes = estimate
        .largest_input_index_bytes
        .max(index_bytes_for_count(file.index.len())?);
    // the open that produced `file` also validates sequence
    // uniqueness, which can copy every sequence into an `8N` temporary held
    // alongside the resident index. Charging it here keeps
    // `peak_resident_structural_bytes` from ignoring a live allocation it can
    // actually see.
    estimate.largest_input_open_transient_bytes = estimate
        .largest_input_open_transient_bytes
        .max(sequence_uniqueness_transient_bytes(file.index.len())?);
    Ok(())
}

/// Merges `base` with `deltas` into `output`, keeping the last write per key.
///
/// Inputs are processed in shard, sequence and physical-ordinal order. Per-key
/// state, values and output ordering stay in bounded RAM buffers and spill to
/// private files only when full. Explicit finite `max_index_bytes` is honored;
/// the unbounded default uses 2 MiB per buffer. Per-operation
/// decoding uses `max_materialized_bytes`. One record/key and fixed tree-page
/// workspace remain minimum working storage. Buffers are capped independently;
/// they do not grow without bound with the total key count.
///
/// Scratch disk grows with history and must have free space. Output replacement
/// happens only after the complete result has been written and synced. Spilling
/// does not publish intermediate output. Custom key equality/hash semantics are
/// preserved, including hash collisions. Individual oversized records still
/// fail their decode/encode limits. Explicit key-count guards remain available.
pub fn merge_keyed_files<T, P>(spec: FormatSpec, base: P, deltas: &[P], output: P) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    merge_keyed_files_with_key_limit::<T, P>(spec, base, deltas, output, u64::MAX)
}

/// [`merge_keyed_files`] with an explicit ceiling on distinct retained keys.
///
/// The collector checks `max_distinct_keys` before it admits each new key, so
/// an input whose cardinality exceeds the declared work limit fails with
/// [`Error::LimitExceeded`] (`resource = "merge distinct keys"`) at the
/// boundary instead of being discovered by the allocator. The ceiling counts
/// tombstoned keys, matching what the state actually retains.
pub fn merge_keyed_files_with_key_limit<T, P>(
    spec: FormatSpec,
    base: P,
    deltas: &[P],
    output: P,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let spec = spec.ordinary_read();
    spec.validate()?;
    let merged =
        external_merge::ExternalMerge::<T>::collect(spec, base, deltas, max_distinct_keys)?;
    write_keyed_values_atomically(spec, output.as_ref(), |out| merged.write(out))
}

/// Compacts `base` plus `deltas` into `output`, dropping superseded records.
/// Uses the bounded buffers and temporary files described by [`merge_keyed_files`].
/// [`compact_keyed_files_with_key_limit`] additionally bounds distinct keys.
pub fn compact_keyed_files<T, P>(spec: FormatSpec, base: P, deltas: &[P], output: P) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    compact_keyed_files_with_key_limit::<T, P>(spec, base, deltas, output, u64::MAX)
}

/// [`compact_keyed_files`] with an explicit ceiling on distinct retained keys.
///
/// See [`merge_keyed_files_with_key_limit`] for the guard semantics.
pub fn compact_keyed_files_with_key_limit<T, P>(
    spec: FormatSpec,
    base: P,
    deltas: &[P],
    output: P,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let spec = spec.ordinary_read();
    spec.validate()?;
    let merged =
        external_merge::ExternalMerge::<T>::collect(spec, base, deltas, max_distinct_keys)?;
    write_keyed_values_atomically(spec, output.as_ref(), |out| merged.write(out))
}

fn write_keyed_values_atomically(
    spec: FormatSpec,
    output: &Path,
    populate: impl FnOnce(&mut VarlitFile) -> Result<()>,
) -> Result<()> {
    let _lock = WriterLock::acquire(output)?;
    // STO4-P2: the permission copy below is fallible and used to sit between
    // the temp's creation and the first scope that cleaned it up, so its `?`
    // could leave an empty temp behind. The guard owns the temp from creation,
    // so *every* early return from here on unlinks it; the deliberate
    // retentions below disarm it explicitly.
    let mut temp = RewriteTemp::create(output)?;
    take_injected_rewrite_temp_preparation_failure()?;
    if let Ok(metadata) = std::fs::metadata(output) {
        temp.file().set_permissions(metadata.permissions())?;
    }
    temp.close();
    let temp_path = temp.path().to_path_buf();

    let result = (|| {
        let mut out = VarlitFile::create(spec, &temp_path)?;
        populate(&mut out)?;
        out.flush()?;
        out.sync()
    })();
    if let Err(error) = result {
        // Delete eagerly rather than at drop: the marker cleanup below is only
        // correct once the temp itself is gone.
        temp.cleanup_now();
        remove_rewrite_temp_lock_marker(&temp_path);
        return Err(error);
    }

    let published = match replace_path_atomically(&temp_path, output) {
        // The rename consumed the temp pathname; there is nothing left to
        // unlink and the guard must not try.
        Ok(ReplaceDurability::Durable) => {
            temp.retain();
            Ok(())
        }
        // Publication already happened; the temp file no longer exists and the
        // target pathname resolves to the merged generation, so the caller
        // must not treat this as "target unchanged".
        Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
            temp.retain();
            Err(crate::fallible::boxed_path_error(
                sync_error,
                output,
                Error::ErrorContextAllocationFailed {
                    context: crate::AllocationFailureContext::ParentSyncPending,
                },
                |path, source| Error::PublishedButParentSyncPending { path, source },
            ))
        }
        Err(error) => {
            if matches!(error, Error::ReplacePublicationIndeterminate { .. }) {
                // the temp may be the only surviving copy of the new
                // generation. Preserving it is the whole point of this arm, so
                // the guard is disarmed rather than allowed to unlink it.
                temp.retain();
            } else {
                temp.cleanup_now();
            }
            Err(error)
        }
    };
    remove_rewrite_temp_lock_marker(&temp_path);
    published
}

/// Removes the writer-lock marker that an internal rewrite temp left behind
///
///
/// Opening the temp through [`VarlitFile::create`] creates `<temp>.lock`.
/// Dropping that writer clears the marker's *contents* but deliberately keeps
/// the file: for a real user path the marker is a stable identity, and
/// unlinking it while another process may already hold the OS lock on that
/// inode would let a third process create a fresh marker at the same pathname
/// and "acquire" a lock on a different inode - exactly the race the persistent
/// marker exists to prevent. None of that applies to this pathname:
///
/// - it was generated by [`create_rewrite_temp_file`] with `create_new`, so
///   this operation exclusively owns it;
/// - the only way to regenerate the same name is another rewrite of the same
///   output in the same process, and the caller holds the output's writer lock
///   across the whole publication, so no such rewrite can be in flight;
/// - by the time this runs the temp has been published or deleted, so the
///   marker names a file that no longer exists.
///
/// The caller must therefore have dropped the temp's `VarlitFile` (releasing its
/// [`WriterLock`]) and must still hold the target's writer lock. If the temp is
/// still present - the indeterminate-publication case, where it is preserved
/// for out-of-band reconciliation - its marker is left with it.
fn remove_rewrite_temp_lock_marker(temp_path: &Path) {
    if temp_path.exists() {
        return;
    }
    let _ = remove_file(lock_path(temp_path));
}

/// Deletes a publication temp file after a pre-publication failure, but
/// preserves it after [`Error::ReplacePublicationIndeterminate`]: in that
/// state the temp may be the only surviving copy of the new generation and is
/// required for out-of-band reconciliation.
fn preserve_temp_on_indeterminate(temp_path: &Path, error: Error) -> Error {
    if !matches!(error, Error::ReplacePublicationIndeterminate { .. }) {
        let _ = remove_file(temp_path);
    }
    error
}

/// Refuses a key the merge state has not seen once the caller's `K-ever`
/// ceiling is reached , so a cardinality the caller cannot afford
/// fails typed at the boundary instead of in the allocator.
fn admit_merge_key<T>(
    state: &HashMap<T::Key, (MergeOrder, Option<T>)>,
    key: &T::Key,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
{
    if state.contains_key(key) {
        return Ok(());
    }
    let admitted = u64::try_from(state.len()).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "merge distinct keys",
    })?;
    if admitted >= max_distinct_keys {
        return Err(Error::LimitExceeded {
            resource: "merge distinct keys",
            actual: admitted.saturating_add(1),
            limit: max_distinct_keys,
        });
    }
    Ok(())
}

fn apply_merge_entries<T, D: RecordDirectory + ?Sized>(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    entries: &D,
    shard: MergeShard,
    state: &mut HashMap<T::Key, (MergeOrder, Option<T>)>,
    budget: &mut MaterializationBudget,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarlitMerge,
    T::Key: Eq + Hash,
{
    // One payload buffer for the whole merge walk, not one per record.
    let mut payload = Vec::new();
    for (record_ordinal, entry) in directory_records(entries).enumerate() {
        let entry = entry?;
        let order = MergeOrder::for_record(shard.ordinal, entry.sequence, record_ordinal);
        match entry.block_id {
            id if id == T::ID => {
                if entry.block_version != T::VERSION {
                    return Err(Error::BlockVersionMismatch {
                        block_id: T::ID,
                        expected: T::VERSION,
                        actual: entry.block_version,
                    });
                }
                let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
                budget.consume(logical_len)?;
                entry.read_logical_payload_snapshot_into(spec, snapshot, &mut payload)?;
                let block: T = budget.decode(&payload, T::ENDIAN.unwrap_or(spec.endian))?;
                let key = block.key();
                if should_apply(state.get(&key), order) {
                    admit_merge_key::<T>(state, &key, max_distinct_keys)?;
                    state.try_reserve(1).map_err(|_| Error::AllocationFailed {
                        resource: "merge state",
                        requested: logical_len,
                    })?;
                    state.insert(key, (order, Some(block)));
                }
            }
            TOMBSTONE_BLOCK_ID => {
                let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
                budget.consume(logical_len)?;
                entry.read_payload_snapshot_into(spec, snapshot, &mut payload)?;
                let Some(key) = decode_internal_key_payload::<T>(spec.endian, &payload, budget)?
                else {
                    continue;
                };
                if should_apply(state.get(&key), order) {
                    admit_merge_key::<T>(state, &key, max_distinct_keys)?;
                    state.try_reserve(1).map_err(|_| Error::AllocationFailed {
                        resource: "merge state",
                        requested: logical_len,
                    })?;
                    state.insert(key, (order, None));
                }
            }
            OP_BLOCK_ID => {
                let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
                budget.consume(logical_len)?;
                entry.read_payload_snapshot_into(spec, snapshot, &mut payload)?;
                let Some((key, op)) =
                    decode_internal_op_payload::<T>(spec.endian, &payload, budget)?
                else {
                    continue;
                };
                if !should_apply(state.get(&key), order) {
                    continue;
                }
                let Some((_, Some(mut value))) = state.remove(&key) else {
                    return Err(Error::MissingMergeTarget);
                };
                value.apply_op(op)?;
                state.insert(key, (order, Some(value)));
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MergeShard {
    ordinal: usize,
}

impl MergeShard {
    fn single_file() -> Self {
        Self { ordinal: 0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct MergeOrder {
    shard_ordinal: usize,
    sequence: u64,
    record_ordinal: usize,
}

impl MergeOrder {
    const fn for_record(shard_ordinal: usize, sequence: u64, record_ordinal: usize) -> Self {
        Self {
            shard_ordinal,
            sequence,
            record_ordinal,
        }
    }
}

fn should_apply<T>(current: Option<&(MergeOrder, Option<T>)>, order: MergeOrder) -> bool {
    current.is_none_or(|(old_order, _)| order >= *old_order)
}

/// Frames a metadata record's payload as `(key, value)` without copying either.
///
/// [`VarlitFile::write_metadata`] encodes `(String, Vec<u8>)` through the tuple
/// codec, which is exactly `u64 key_len | key | u64 value_len | value`, and
/// metadata goes through `write_record` rather than `write_user_record`, so it
/// is never compressed. That framing is what the borrowed pair reads back.
///
/// It exists so a lookup can compare the key *before* deciding to materialize
/// the value: the decoder allocates a `String` and a `Vec<u8>` per record,
/// which for a lookup is one copy per record that is thrown away. The errors
/// are the ones `decode_complete` raises on the same bytes —
/// [`Error::UnexpectedEof`], [`Error::LengthOverflow`], [`Error::InvalidUtf8`]
/// and [`Error::TrailingBytes`] — because the byte-for-byte answer must not
/// depend on which of the two readers looked.
///
/// The `0` materialization allowance is not a limit being enforced: nothing is
/// materialized here, so nothing is charged. Callers charge the record they
/// then decode.
fn split_metadata_payload(payload: &[u8], endian: Endian) -> Result<(&str, &[u8])> {
    let mut decoder = crate::Decoder::new_limited(payload, endian, 0);
    let key_len = decoder.read_len()?;
    if key_len > decoder.remaining() {
        return Err(Error::UnexpectedEof);
    }
    let key = decoder.read_exact(key_len)?;
    let key = std::str::from_utf8(key).map_err(|_| Error::InvalidUtf8)?;
    let value_len = decoder.read_len()?;
    if value_len > decoder.remaining() {
        return Err(Error::UnexpectedEof);
    }
    let value = decoder.read_exact(value_len)?;
    if decoder.remaining() != 0 {
        return Err(Error::TrailingBytes {
            remaining: decoder.remaining(),
        });
    }
    Ok((key, value))
}

/// Encodes a writer-supplied value with the format's logical-payload limit as
/// the encode hard bound.
///
/// Every resident writer entry point routes its value encoding through this
/// bound so an oversized value fails with the typed limit error while the
/// encoder stops buffering at the limit, instead of materializing an
/// arbitrarily large encoding first and only then failing the limit check.
fn encode_logical_payload_limited<T: VarlitEncode>(
    spec: FormatSpec,
    value: &T,
    endian: Endian,
) -> Result<Vec<u8>> {
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    encode_to_vec_limited(
        value,
        endian,
        logical_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )
}

/// [`encode_logical_payload_limited`] into a caller-owned buffer.
fn encode_logical_payload_limited_into<T: VarlitEncode>(
    spec: FormatSpec,
    value: &T,
    endian: Endian,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    crate::codec::encode_into_limited(
        value,
        endian,
        logical_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
        buffer,
    )
}

fn encode_internal_key_payload<T>(spec: FormatSpec, key: &T::Key) -> Result<Vec<u8>>
where
    T: VarlitKeyedBlock,
{
    const ENVELOPE_LEN: u64 = 12;
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    let key_limit = logical_limit
        .checked_sub(ENVELOPE_LEN)
        .ok_or(Error::LimitExceeded {
            resource: ReadLimitKey::LogicalPayloadLen.resource(),
            actual: ENVELOPE_LEN,
            limit: logical_limit,
        })?;
    let key_payload = encode_to_vec_limited(
        key,
        spec.endian,
        key_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )?;
    let total_len = ENVELOPE_LEN.checked_add(key_payload.len() as u64).ok_or(
        Error::ResourceArithmeticOverflow {
            resource: "internal key payload length",
        },
    )?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, total_len)?;
    let total_len_usize =
        usize::try_from(total_len).map_err(|_| Error::LengthOverflow { value: total_len })?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(total_len_usize)
        .map_err(|_| Error::AllocationFailed {
            resource: "internal key payload",
            requested: total_len,
        })?;
    payload.extend_from_slice(&T::ID.to_le_bytes());
    payload.extend_from_slice(&(key_payload.len() as u64).to_le_bytes());
    payload.extend_from_slice(&key_payload);
    Ok(payload)
}

fn decode_internal_key_payload<T>(
    endian: Endian,
    payload: &[u8],
    budget: &mut MaterializationBudget,
) -> Result<Option<T::Key>>
where
    T: VarlitKeyedBlock,
{
    let Some(key) = decode_native_internal_key_envelope(payload, T::ID)? else {
        return Ok(None);
    };
    Ok(Some(budget.decode(key, endian)?))
}

pub(crate) fn decode_stream_tombstone_key<T: VarlitKeyedBlock>(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    entry: &RecordIndexEntry,
) -> Result<Option<T::Key>> {
    if entry.block_id != TOMBSTONE_BLOCK_ID {
        return Ok(None);
    }
    if entry.block_version != 1 || entry.flags != RECORD_FLAG_INTERNAL {
        return Err(Error::CorruptTail {
            offset: entry.record_offset,
        });
    }
    let logical_len = entry.logical_payload_len_snapshot(spec, snapshot)?;
    let mut budget = MaterializationBudget::new(spec);
    budget.consume(logical_len)?;
    let payload = entry.read_logical_payload_snapshot(spec, snapshot)?;
    decode_internal_key_payload::<T>(spec.endian, &payload, &mut budget)
}

fn encode_internal_op_payload<T>(spec: FormatSpec, key: &T::Key, op: &T::Op) -> Result<Vec<u8>>
where
    T: VarlitMerge,
{
    const ENVELOPE_LEN: u64 = 20;
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    let key_limit = logical_limit
        .checked_sub(ENVELOPE_LEN)
        .ok_or(Error::LimitExceeded {
            resource: ReadLimitKey::LogicalPayloadLen.resource(),
            actual: ENVELOPE_LEN,
            limit: logical_limit,
        })?;
    let key_payload = encode_to_vec_limited(
        key,
        spec.endian,
        key_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )?;
    // the op encoder inherits whatever budget the key left over, so
    // the combined encoding can never buffer past the logical payload limit.
    let op_limit = key_limit - key_payload.len() as u64;
    let op_payload = encode_to_vec_limited(
        op,
        spec.endian,
        op_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )?;
    let total_len = ENVELOPE_LEN
        .checked_add(key_payload.len() as u64)
        .and_then(|len| len.checked_add(op_payload.len() as u64))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "internal op payload length",
        })?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, total_len)?;
    let total_len_usize =
        usize::try_from(total_len).map_err(|_| Error::LengthOverflow { value: total_len })?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(total_len_usize)
        .map_err(|_| Error::AllocationFailed {
            resource: "internal op payload",
            requested: total_len,
        })?;
    payload.extend_from_slice(&T::ID.to_le_bytes());
    payload.extend_from_slice(&(key_payload.len() as u64).to_le_bytes());
    payload.extend_from_slice(&key_payload);
    payload.extend_from_slice(&(op_payload.len() as u64).to_le_bytes());
    payload.extend_from_slice(&op_payload);
    Ok(payload)
}

fn decode_internal_op_payload<T>(
    endian: Endian,
    payload: &[u8],
    budget: &mut MaterializationBudget,
) -> Result<Option<(T::Key, T::Op)>>
where
    T: VarlitMerge,
{
    let Some((key, op)) = decode_native_internal_op_envelope(payload, T::ID)? else {
        return Ok(None);
    };
    let key = budget.decode(key, endian)?;
    let op = budget.decode(op, endian)?;
    Ok(Some((key, op)))
}

/// Write the header this spec describes, and hand back the extension region it
/// put there so the caller can record what this file actually carries.
pub(crate) fn write_file_header(spec: FormatSpec, file: &mut File) -> Result<Vec<u8>> {
    let extensions = file_header_extensions(spec)?;
    write_native_file_header(file, spec, &extensions)?;
    Ok(extensions)
}

pub(crate) fn read_file_header(spec: FormatSpec, file: &mut (impl Read + Seek)) -> Result<u64> {
    Ok(read_file_header_parts(spec, file)?.0)
}

/// The header length *and* the extension region the file carries.
///
/// The two are not interchangeable with what `spec` would produce: once an
/// unknown block is skippable rather than refused, a file's region can be
/// longer than this build would write, and every offset derived from the header
/// has to come from the file rather than from the spec.
pub(crate) fn read_file_header_parts(
    spec: FormatSpec,
    file: &mut (impl Read + Seek),
) -> Result<(u64, Vec<u8>)> {
    file.seek(SeekFrom::Start(0))?;
    let header = read_native_file_header(file, spec)?;
    let hash = header.schema_hash;
    if spec.schema_hash != 0 && hash != spec.schema_hash {
        return Err(Error::SchemaHashMismatch {
            expected: spec.schema_hash,
            actual: hash,
        });
    }
    if uses_format_contract_compression(spec) {
        let computed = spec.computed_schema_hash();
        if hash != computed {
            return Err(Error::SchemaHashMismatch {
                expected: computed,
                actual: hash,
            });
        }
    }
    if header.has_extension_len {
        validate_file_header_extensions(spec, &header.extensions)?;
        Ok((header.header_len, header.extensions))
    } else {
        if uses_file_explicit_compression(spec) {
            return Err(Error::InvalidCompressionHeader);
        }
        Ok((native_file_header_len(spec, 0), Vec::new()))
    }
}

fn matrix_pending_limit(spec: FormatSpec) -> usize {
    // Charge a conservative 128 bytes of directory metadata per dirty page.
    spec.read_limits
        .require(ReadLimitKey::MaterializedBytes)
        .ok()
        .flatten()
        .map_or(65_536, |n| {
            usize::try_from(n / 128).unwrap_or(usize::MAX).max(1)
        })
}
fn read_matrix_layout_if_needed(
    path: &Path,
    writable: bool,
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    _captured_len: u64,
) -> Result<
    Option<(
        crate::matrix::MatrixLayout,
        [u8; MATRIX_CREATION_NONCE_LEN],
        crate::matrix_generation::MatrixFile,
    )>,
> {
    if !spec.has_matrix_blocks() {
        return Ok(None);
    }
    let nonce = read_matrix_creation_nonce_region(file, header_len)?;
    let mut pages = crate::matrix_generation::MatrixFile::open(
        path,
        file,
        nonce,
        writable,
        matrix_pending_limit(spec),
    )?;
    pages.set_cache_bytes(spec.read_limits.matrix_generation_cache_budget());
    pages.set_traversal_bytes(limit_or_max(
        spec.read_limits.require(ReadLimitKey::ScanBytes)?,
    ));
    if pages.head().native_eof > file.metadata()?.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    let layout_start = header_len
        .checked_add(MATRIX_CREATION_NONCE_REGION_LEN as u64)
        .ok_or(Error::InvalidMatrixLayout)?;
    let confirmed_len = pages.head().native_eof;
    let layout = crate::matrix::read_layout_at_len(spec, &mut pages, layout_start, confirmed_len)?;
    if writable {
        file.set_len(pages.head().native_eof)?;
    }
    Ok(Some((layout, nonce, pages)))
}
#[allow(clippy::type_complexity)]
fn split_matrix_state(
    state: Option<(
        crate::matrix::MatrixLayout,
        [u8; MATRIX_CREATION_NONCE_LEN],
        crate::matrix_generation::MatrixFile,
    )>,
) -> (
    Option<crate::matrix::MatrixLayout>,
    Option<[u8; MATRIX_CREATION_NONCE_LEN]>,
    Option<crate::matrix_generation::MatrixFile>,
) {
    match state {
        Some((layout, nonce, pages)) => (Some(layout), Some(nonce), Some(pages)),
        None => (None, None, None),
    }
}

fn append_log_start(header_len: u64, matrix: Option<&crate::matrix::MatrixLayout>) -> u64 {
    matrix.map_or(header_len, crate::matrix::MatrixLayout::append_log_start)
}

/// Produces a fresh 128-bit matrix creation nonce.
///
/// This is a uniqueness value, not a cryptographic secret: it only has to
/// differ between two `create_with_dims` calls that reuse the same OS file
/// object. Wall-clock nanoseconds, the process id and a per-process counter
/// are mixed through FNV-1a into two independent 64-bit lanes.
fn fresh_matrix_creation_nonce() -> [u8; MATRIX_CREATION_NONCE_LEN] {
    use std::sync::atomic::{AtomicU64, Ordering};

    static CREATION_COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = CREATION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let pid = u64::from(std::process::id());

    const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
    const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
    let mut nonce = [0u8; MATRIX_CREATION_NONCE_LEN];
    for lane in 0u64..2 {
        let mut hash = FNV_OFFSET_BASIS;
        for word in [lane, nanos as u64, (nanos >> 64) as u64, pid, counter] {
            for byte in word.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
        }
        let lane = lane as usize;
        nonce[lane * 8..(lane + 1) * 8].copy_from_slice(&hash.to_le_bytes());
    }
    // The raw entropy words survive the mix: XOR them back in so even a
    // degenerate clock (nanos == 0 on every call) cannot collapse two creates
    // in the same process to the same nonce (the counter still differs).
    for (index, byte) in nanos
        .to_le_bytes()
        .iter()
        .chain(counter.to_le_bytes().iter())
        .take(MATRIX_CREATION_NONCE_LEN)
        .enumerate()
    {
        nonce[index] ^= byte.rotate_left((index % 7) as u32);
    }
    nonce
}

fn write_matrix_creation_nonce_region(
    file: &mut File,
    creation_id: [u8; MATRIX_CREATION_NONCE_LEN],
) -> Result<()> {
    let mut region = [0u8; MATRIX_CREATION_NONCE_REGION_LEN];
    region[0..4].copy_from_slice(MATRIX_CREATION_NONCE_MAGIC);
    region[4..6].copy_from_slice(&MATRIX_CREATION_NONCE_VERSION.to_le_bytes());
    // region[6..8] stays zero (reserved).
    region[8..].copy_from_slice(&creation_id);
    file.write_all(&region)?;
    Ok(())
}

fn read_matrix_creation_nonce_region(
    file: &mut File,
    offset: u64,
) -> Result<[u8; MATRIX_CREATION_NONCE_LEN]> {
    file.seek(SeekFrom::Start(offset))?;
    let mut region = [0u8; MATRIX_CREATION_NONCE_REGION_LEN];
    file.read_exact(&mut region)?;
    if &region[0..4] != MATRIX_CREATION_NONCE_MAGIC
        || region[4..6] != MATRIX_CREATION_NONCE_VERSION.to_le_bytes()
        || region[6..8] != [0; 2]
    {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut nonce = [0u8; MATRIX_CREATION_NONCE_LEN];
    nonce.copy_from_slice(&region[8..]);
    Ok(nonce)
}

/// The limits a native format must declare before it may be written or opened.
///
/// **`FileLen` is not among them, and that is the point.** Requiring a
/// declaration for a ceiling that nothing enforces is ceremony: the twenty-one
/// enforcement sites were removed in this round, and a required declaration
/// that no check consults refuses a format for a number that could not have
/// changed any outcome. `max_file_len` remains on `FormatSpec` and the DSL
/// still accepts `limits { file_len: .. }`, so no format definition breaks —
/// the value is simply inert.
///
/// The four that remain each bound a real allocation, and each has a check that
/// consults it.
pub(crate) fn ensure_native_write_limits(spec: FormatSpec) -> Result<()> {
    for key in [
        ReadLimitKey::Records,
        ReadLimitKey::IndexBytes,
        ReadLimitKey::RecordPayloadLen,
        ReadLimitKey::LogicalPayloadLen,
    ] {
        spec.read_limits.require(key)?;
    }
    Ok(())
}

pub(crate) fn ensure_native_open_limits(spec: FormatSpec) -> Result<()> {
    ensure_native_write_limits(spec)?;
    spec.read_limits.require(ReadLimitKey::ScanBytes)?;
    Ok(())
}

pub(crate) fn check_open_file_len(_spec: FormatSpec, file: &File) -> Result<u64> {
    Ok(file.metadata()?.len())
}

fn ensure_matrix_byte_copy_compatible<From, To>() -> Result<()>
where
    From: VarlitMatrixBlock,
    To: VarlitMatrixBlock,
{
    if From::DIMENSIONS != To::DIMENSIONS || From::SLOT_STRIDE != To::SLOT_STRIDE {
        return Err(Error::InvalidFormatSpec(
            "matrix byte-copy requires matching dimensions and slot_stride",
        ));
    }
    Ok(())
}

fn record_footer_len(spec: FormatSpec) -> u64 {
    if spec.spec_needs_record_footer() {
        native_record_footer_len()
    } else {
        0
    }
}

fn nonzero_offset(offset: u64) -> Option<u64> {
    if offset == 0 { None } else { Some(offset) }
}

const fn limit_or_max(limit: Option<u64>) -> u64 {
    match limit {
        Some(limit) => limit,
        None => u64::MAX,
    }
}

fn ensure_payload_limit(resource: &'static str, actual: u64, limit: u64) -> Result<()> {
    if actual > limit {
        Err(Error::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

fn index_bytes_for_count(count: usize) -> Result<u64> {
    let count = u64::try_from(count).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "index bytes",
    })?;
    let entry_size = u64::try_from(size_of::<RecordIndexEntry>()).map_err(|_| {
        Error::ResourceArithmeticOverflow {
            resource: "index bytes",
        }
    })?;
    count
        .checked_mul(entry_size)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "index bytes",
        })
}

fn allocation_bytes<T>(count: usize, resource: &'static str) -> Result<u64> {
    u64::try_from(count)
        .map_err(|_| Error::ResourceArithmeticOverflow { resource })?
        .checked_mul(size_of::<T>() as u64)
        .ok_or(Error::ResourceArithmeticOverflow { resource })
}

#[cfg(feature = "mmap")]
fn mmap_index_bytes_for_count(count: usize, verify: bool) -> Result<u64> {
    let count = u64::try_from(count).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "mmap index bytes",
    })?;
    // One index entry copy plus one `by_block` position per record; the
    // former second full entry copy for the membership hash set is gone
    //
    let entry_bytes = u64::try_from(size_of::<RecordIndexEntry>())
        .map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "mmap index bytes",
        })?
        .checked_add(size_of::<usize>() as u64)
        .and_then(|bytes| bytes.checked_add(u64::from(verify)))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "mmap index bytes",
        })?;
    count
        .checked_mul(entry_bytes)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "mmap index bytes",
        })
}

/// Bytes the sequence-uniqueness witness allocates for `records` entries.
///
/// This is the transient every resident input open can pay on top of its
/// resident index, so [`KeyedMergeEstimate`] has to charge it.
fn sequence_uniqueness_transient_bytes(records: usize) -> Result<u64> {
    u64::try_from(records)
        .map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "sequence uniqueness index",
        })?
        .checked_mul(size_of::<u64>() as u64)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "sequence uniqueness index",
        })
}

/// Rejects a native file that reuses a record sequence.
///
/// Fast path : the writer hands out sequences from a monotonically
/// increasing counter, so an ordinary append-only file scanned in offset order
/// is strictly increasing. Verifying that costs one allocation-free pass and
/// proves uniqueness outright. Only an input whose records are *not* in
/// ascending sequence order - a rewritten or externally reordered generation -
/// falls back to the sorted copy, which costs `O(N log N)` time and an explicit
/// `8N` temporary. The fallback is still the published worst case, so the
/// resident merge estimate charges it unconditionally rather than assuming the
/// fast path.
fn validate_unique_sequences(entries: &[RecordIndexEntry]) -> Result<()> {
    if entries
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence)
    {
        return Ok(());
    }
    let requested = sequence_uniqueness_transient_bytes(entries.len())?;
    let mut sequences = Vec::new();
    sequences
        .try_reserve_exact(entries.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "sequence uniqueness index",
            requested,
        })?;
    sequences.extend(entries.iter().map(|entry| entry.sequence));
    sequences.sort_unstable();
    if sequences.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(Error::InvalidCanonicalEncoding(
            "duplicate native record sequence",
        ));
    }
    Ok(())
}

/// The three per-handle structures every open and every generation rebind
/// derives from the resident index.
struct DerivedIndexState {
    checkpoint_cadence: CheckpointCadence,
    segment_cursor: SegmentCursor,
    /// `None` when the caller did not ask for tails, which an open does not:
    /// it already has them from the scan or the chain walk, and building the
    /// newest-per-block map here would be work thrown away.
    block_tails: Option<BlockTails>,
}

/// Derives all three in ONE forward pass.
///
/// They used to be three separate walks of the same entries — one forward
/// (`BlockTails`) and two reverse (`CheckpointCadence`, `SegmentCursor`) — run
/// back to back at every open and at both generation rebinds. The two reverse
/// walks stop at the newest checkpoint and the newest segment respectively, so
/// on a file that carries neither they each walk the whole index; the forward
/// one has no early exit at all. Three traversals, and a single forward pass
/// answers all of them, because "the last entry with this block id" is just the
/// last one a forward pass saw.
///
/// Each field is still computed by exactly the rule its own `from_index`
/// states, so the invariants those doc comments assert are unchanged. The unit
/// tests below compare this against all three.
fn derive_index_state(
    entries: impl Iterator<Item = Result<RecordIndexEntry>>,
    with_tails: bool,
) -> Result<DerivedIndexState> {
    let mut touched: u64 = 0;
    let mut newest: HashMap<u32, u64> = HashMap::new();
    // Position of, and entry at, the newest record of each kind the derived
    // structures key on.
    let mut newest_checkpoint: Option<usize> = None;
    let mut newest_segment: Option<(usize, u64)> = None;
    // Eligible records since the newest checkpoint, counted forward: reset each
    // time a checkpoint is seen, which leaves exactly the tail the reverse walk
    // used to accumulate.
    let mut eligible: usize = 0;
    for (position, entry) in entries.enumerate() {
        let entry = entry?;
        touched = touched.saturating_add(1);
        if with_tails {
            if newest.len() == newest.capacity() && !newest.contains_key(&entry.block_id) {
                newest.try_reserve(1).map_err(|_| Error::AllocationFailed {
                    resource: "derived block tails",
                    requested: (newest.len() as u64 + 1).saturating_mul(32),
                })?;
            }
            newest.insert(entry.block_id, entry.record_offset);
        }
        match entry.block_id {
            INDEX_BLOCK_ID => {
                newest_checkpoint = Some(position);
                eligible = 0;
            }
            SEGMENT_BLOCK_ID => newest_segment = Some((position, entry.physical_end())),
            COMMIT_BLOCK_ID => {}
            _ => eligible = eligible.saturating_add(1),
        }
    }
    let state = DerivedIndexState {
        checkpoint_cadence: CheckpointCadence {
            eligible_since_checkpoint: eligible,
            next_threshold: match newest_checkpoint {
                Some(position) => core::cmp::max(INDEX_CHECKPOINT_MIN_RECORDS, position / 2),
                None => INDEX_CHECKPOINT_MIN_RECORDS,
            },
        },
        segment_cursor: match newest_segment {
            Some((position, physical_end)) => SegmentCursor {
                next_position: position + 1,
                covered_start: Some(physical_end),
            },
            None => SegmentCursor::new_empty(),
        },
        block_tails: with_tails
            .then(|| BlockTails::from_newest(&newest))
            .transpose()?,
    };
    if with_tails {
        note_block_tail_index_touches(touched);
    }
    note_checkpoint_cadence_index_touches(touched);
    Ok(state)
}

/// Borrowed entries as the fallible stream [`derive_index_state`] consumes.
///
/// The open paths still hold a plain `Vec` at the point they derive from it —
/// they built it moments earlier — so their entries cannot fail to be produced.
/// This says that in one place instead of at each call site.
#[cfg(test)]
fn infallible_entries(
    entries: &[RecordIndexEntry],
) -> impl DoubleEndedIterator<Item = Result<RecordIndexEntry>> + ExactSizeIterator + '_ {
    entries.iter().cloned().map(Ok)
}

fn read_record_header(
    file: &mut (impl Read + Seek),
    _spec: FormatSpec,
    offset: u64,
) -> Result<RecordIndexEntry> {
    let decoded = read_native_record_header(file, offset)?;
    debug_assert_eq!(decoded.lead_in_len, RECORD_HEADER_LEN);
    let header = decoded.fields;
    Ok(RecordIndexEntry {
        block_id: header.block_id,
        block_version: header.block_version,
        flags: header.flags,
        sequence: header.sequence,
        record_offset: offset,
        payload_offset: offset
            .checked_add(decoded.lead_in_len)
            .ok_or(Error::CorruptTail { offset })?,
        payload_len: header.payload_len,
        checksum: header.checksum,
        uncompressed_len_hint: header.uncompressed_len_hint,
        footer_offset: None,
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        mutable_flags: 0,
        committed: true,
    })
}

fn write_record_header(
    file: &mut File,
    spec: FormatSpec,
    record_offset: u64,
    header: RecordHeaderFields,
) -> Result<()> {
    let footer_len = record_footer_len(spec);
    debug_assert_eq!(native_record_header_len(), RECORD_HEADER_LEN);
    write_native_record_header(file, header, record_offset, footer_len)
}

fn encode_record_footer(footer: RecordFooterFields) -> Result<[u8; RECORD_FOOTER_LEN as usize]> {
    encode_native_record_footer(footer)
}

/// Rebuilds a record's index entry from the record itself, positionally.
///
/// **`&self` throughout, which is the whole point.** `read_record_entry_at`
/// takes `&mut File` and seeks, so it cannot serve an `&self` read path.
/// A demand-filled index uses that path: every read of an entry the index no
/// longer holds has to come
/// from the file, under `&self`. This reads the two fixed-size regions through
/// `SnapshotFile::read_exact_at` and hands them to the same decoders the
/// scanning reader uses, so there is one decode of a record header in the
/// crate and not two.
///
/// `committed` is the caller's, because it is writer state with no
/// representation in the record — proved, not assumed, by
/// `every_index_entry_can_be_rebuilt_from_its_own_record`.
///
/// This is what `ResidentIndex` calls for every entry it produces.
/// `every_index_entry_can_be_rebuilt_from_its_own_record` is the proof that
/// what comes back matches what the scanning reader would have built.
/// Whether the record header actually at an offset is the record an entry
/// claims is there.
///
/// Two callers ask exactly this, under opposite conditions, which is why the
/// seven fields are listed once rather than twice.
/// [`RecordIndexEntry::verify_snapshot_record`] asks it as part of integrity
/// verification and returns early when no checksum is declared;
/// [`VarlitFile::check_directory_describes_this_file`] asks it of a
/// caller-supplied directory and exists *above all* for the no-checksum case,
/// where nothing else would notice. A field added to the record header has to
/// arrive here or both of them silently stop covering it.
fn record_header_describes(actual: &RecordHeaderFields, stated: &RecordIndexEntry) -> bool {
    actual.block_id == stated.block_id
        && actual.block_version == stated.block_version
        && actual.flags == stated.flags
        && actual.sequence == stated.sequence
        && actual.payload_len == stated.payload_len
        && actual.checksum == stated.checksum
        && actual.uncompressed_len_hint == stated.uncompressed_len_hint
}

pub(crate) fn fault_record_entry(
    snapshot: &SnapshotFile,
    spec: FormatSpec,
    record_offset: u64,
    committed: bool,
) -> Result<RecordIndexEntry> {
    fault_record_entry_prefixed::<{ RECORD_HEADER_LEN as usize }>(
        snapshot,
        spec,
        record_offset,
        committed,
    )
}

/// A mutation's small fixed prefix can frame the header and footer with one
/// positional read. Ordinary directory reads retain their header-only I/O;
/// large records use the existing separate footer read. No heap buffer or
/// persistent cache is added, and every prefix stays inside the snapshot.
fn fault_record_entry_prefixed<const PREFIX: usize>(
    snapshot: &SnapshotFile,
    spec: FormatSpec,
    record_offset: u64,
    committed: bool,
) -> Result<RecordIndexEntry> {
    const {
        assert!(PREFIX >= RECORD_HEADER_LEN as usize);
    }
    fault_record_entry_with::<PREFIX>(
        snapshot.len(),
        spec,
        record_offset,
        committed,
        |at, bytes| snapshot.read_exact_at(at, bytes),
    )
}

#[inline]
fn mark_prefers_header_only(snapshot_len: u64, records: usize) -> bool {
    // An I/O hint, never a boundary witness. Avoid copying an unused payload
    // prefix when observed file geometry is dominated by larger records.
    // Small directories keep the prefix because container metadata can dominate
    // their average. Saturation only selects the existing wider read route.
    records >= 32 && snapshot_len > (records as u64).saturating_mul(512)
}

fn fault_record_entry_for_mark_with(
    snapshot_len: u64,
    records: usize,
    spec: FormatSpec,
    record_offset: u64,
    read: impl FnMut(u64, &mut [u8]) -> Result<()>,
) -> Result<RecordIndexEntry> {
    if mark_prefers_header_only(snapshot_len, records) {
        fault_record_entry_with::<{ RECORD_HEADER_LEN as usize }>(
            snapshot_len,
            spec,
            record_offset,
            false,
            read,
        )
    } else {
        fault_record_entry_with::<512>(snapshot_len, spec, record_offset, false, read)
    }
}

fn fault_record_entry_with<const PREFIX: usize>(
    snapshot_len: u64,
    spec: FormatSpec,
    record_offset: u64,
    committed: bool,
    mut read: impl FnMut(u64, &mut [u8]) -> Result<()>,
) -> Result<RecordIndexEntry> {
    count_record_entry_fault();
    let mut header_bytes = [0; PREFIX];
    let prefix_len = if PREFIX == RECORD_HEADER_LEN as usize {
        RECORD_HEADER_LEN as usize
    } else {
        snapshot_len
            .saturating_sub(record_offset)
            .min(PREFIX as u64)
            .max(RECORD_HEADER_LEN) as usize
    };
    read(record_offset, &mut header_bytes[..prefix_len])?;
    let decoded = read_native_record_header(
        &mut &header_bytes[..RECORD_HEADER_LEN as usize],
        record_offset,
    )?;
    debug_assert_eq!(decoded.lead_in_len, RECORD_HEADER_LEN);
    let header = decoded.fields;
    let Some(payload_offset) = record_offset.checked_add(RECORD_HEADER_LEN) else {
        return Err(Error::ResourceArithmeticOverflow {
            resource: "record payload offset",
        });
    };
    let mut entry = RecordIndexEntry {
        block_id: header.block_id,
        block_version: header.block_version,
        flags: header.flags,
        sequence: header.sequence,
        record_offset,
        payload_offset,
        payload_len: header.payload_len,
        checksum: header.checksum,
        uncompressed_len_hint: header.uncompressed_len_hint,
        footer_offset: None,
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        mutable_flags: 0,
        committed,
    };
    if spec.spec_needs_record_footer() {
        let Some(footer_offset) = payload_offset.checked_add(entry.payload_len) else {
            return Err(Error::ResourceArithmeticOverflow {
                resource: "record footer offset",
            });
        };
        let mut footer_bytes = [0; RECORD_FOOTER_LEN as usize];
        let within_prefix = if PREFIX > RECORD_HEADER_LEN as usize {
            usize::try_from(footer_offset - record_offset)
                .ok()
                .and_then(|start| {
                    start
                        .checked_add(RECORD_FOOTER_LEN as usize)
                        .filter(|&end| end <= prefix_len)
                        .map(|end| (start, end))
                })
        } else {
            None
        };
        let bytes = match within_prefix {
            Some((start, end)) => &header_bytes[start..end],
            None => {
                read(footer_offset, &mut footer_bytes)?;
                &footer_bytes[..]
            }
        };
        let footer = decode_record_footer(spec, bytes, footer_offset, record_offset)?;
        entry.footer_offset = Some(footer_offset);
        entry.prev_same_block_offset = footer.prev_same_block_offset;
        entry.prev_same_key_offset = footer.prev_same_key_offset;
        entry.mutable_flags = footer.mutable_flags;
    }
    Ok(entry)
}

fn read_record_footer_bytes(
    file: &mut (impl Read + Seek),
    offset: u64,
) -> Result<[u8; RECORD_FOOTER_LEN as usize]> {
    file.seek(SeekFrom::Start(offset))?;
    let mut footer = [0; RECORD_FOOTER_LEN as usize];
    file.read_exact(&mut footer)?;
    Ok(footer)
}

/// Decodes a footer and holds its trailing word to the spec's policy.
///
/// The native decoder hands the word back rather than judging it, because
/// whether a non-zero value is legitimate is a property of the *spec* and not
/// of the bytes. Under [`LivenessPolicy::None`] it is the reserved field it has
/// always been and a non-zero value is a corrupt footer — refusing it here is
/// what keeps a format that did not opt in exactly as strict as it was before
/// the policy existed, which matters because these four bytes are the ones the
/// checksum does not cover under the other policy.
fn decode_record_footer(
    spec: FormatSpec,
    bytes: &[u8],
    footer_offset: u64,
    record_offset: u64,
) -> Result<RecordFooterFields> {
    let footer = decode_native_record_footer(bytes, footer_offset, record_offset)?;
    if spec.liveness_policy == LivenessPolicy::None && footer.mutable_flags != 0 {
        return Err(Error::InvalidRecordFooter {
            offset: footer_offset,
        });
    }
    Ok(footer)
}

fn checksum_record_fields(
    spec: FormatSpec,
    record_offset: u64,
    mut header: RecordHeaderFields,
    payload: &[u8],
    footer: &[u8],
) -> Result<u32> {
    header.checksum = 0;
    let header_bytes = if spec.integrity_policy == IntegrityPolicy::Crc32WithHeader {
        Some(encode_native_record_header(
            header,
            record_offset,
            record_footer_len(spec),
        )?)
    } else {
        None
    };
    checksum_record_bytes(
        spec,
        header_bytes.as_ref().map(|bytes| &bytes[..]).unwrap_or(&[]),
        payload,
        footer,
    )
}

fn checksum_record_file(
    spec: FormatSpec,
    file: &mut (impl Read + Seek),
    entry: &RecordIndexEntry,
    footer: &[u8],
) -> Result<u32> {
    let header = RecordHeaderFields {
        block_id: entry.block_id,
        block_version: entry.block_version,
        flags: entry.flags,
        sequence: entry.sequence,
        payload_len: entry.payload_len,
        checksum: 0,
        uncompressed_len_hint: entry.uncompressed_len_hint,
    };
    let header_bytes = if spec.integrity_policy == IntegrityPolicy::Crc32WithHeader {
        Some(encode_native_record_header(
            header,
            entry.record_offset,
            record_footer_len(spec),
        )?)
    } else {
        None
    };
    crc32_record_file(
        file,
        entry.payload_offset,
        entry.payload_len,
        header_bytes.as_ref().map(|bytes| &bytes[..]).unwrap_or(&[]),
        checksummed_footer(spec, footer),
    )
}

#[cfg(feature = "integrity")]
fn crc32_record_file(
    file: &mut (impl Read + Seek),
    payload_offset: u64,
    payload_len: u64,
    header: &[u8],
    footer: &[u8],
) -> Result<u32> {
    if payload_len <= 1024 {
        crc32_record_file_buffered::<1024>(file, payload_offset, payload_len, header, footer)
    } else {
        crc32_record_file_buffered::<STREAM_BUFFER_LEN>(
            file,
            payload_offset,
            payload_len,
            header,
            footer,
        )
    }
}

#[cfg(feature = "integrity")]
#[inline(never)]
fn crc32_record_file_buffered<const BUFFER: usize>(
    file: &mut (impl Read + Seek),
    payload_offset: u64,
    payload_len: u64,
    header: &[u8],
    footer: &[u8],
) -> Result<u32> {
    const {
        assert!(BUFFER > 0);
    }
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(header);
    file.seek(SeekFrom::Start(payload_offset))?;
    let mut remaining = payload_len;
    let mut buffer = [0; BUFFER];
    while remaining != 0 {
        let chunk_len = usize::try_from(remaining.min(BUFFER as u64)).map_err(|_| {
            Error::ResourceArithmeticOverflow {
                resource: "checksum buffer length",
            }
        })?;
        file.read_exact(&mut buffer[..chunk_len])?;
        hasher.update(&buffer[..chunk_len]);
        remaining -= chunk_len as u64;
    }
    hasher.update(footer);
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
fn crc32_record_file(
    _file: &mut (impl Read + Seek),
    _payload_offset: u64,
    _payload_len: u64,
    _header: &[u8],
    _footer: &[u8],
) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

/// How much of a record footer the checksum covers.
///
/// The whole thing under [`LivenessPolicy::None`], which is what every file
/// written before that policy existed carries. Under
/// [`LivenessPolicy::FooterFlags`] the trailing word is excluded, and that
/// exclusion is the entire mechanism: it is what lets a writer set a flag there
/// afterwards without re-reading the payload to recompute a CRC, and without
/// making every reader that already framed the record disagree with the disk.
///
/// The price, stated once: those four bytes are unauthenticated. A flipped bit
/// in them is not detected by anything, so the flags are advisory — a set flag
/// says "the writer marked this", never "these bytes are sound".
fn checksummed_footer(spec: FormatSpec, footer: &[u8]) -> &[u8] {
    match spec.liveness_policy {
        LivenessPolicy::None => footer,
        LivenessPolicy::FooterFlags => footer
            .len()
            .checked_sub(RECORD_FOOTER_MUTABLE_LEN as usize)
            .map_or(footer, |end| &footer[..end]),
    }
}

fn checksum_record_bytes(
    spec: FormatSpec,
    header: &[u8],
    payload: &[u8],
    footer: &[u8],
) -> Result<u32> {
    match spec.integrity_policy {
        IntegrityPolicy::None => Ok(0),
        IntegrityPolicy::Crc32 => {
            crc32_record_bytes(&[], payload, checksummed_footer(spec, footer))
        }
        IntegrityPolicy::Crc32WithHeader => {
            crc32_record_bytes(header, payload, checksummed_footer(spec, footer))
        }
    }
}

fn write_matrix_sidecar_file<P: AsRef<Path>>(
    spec: FormatSpec,
    category: &str,
    sidecar_path: P,
    generation: u64,
    payload: &[u8],
    identity: MatrixNativeIdentity,
) -> Result<MatrixSidecarManifest> {
    let payload_len =
        u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
    let format_magic_len =
        u64::try_from(spec.magic.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "matrix sidecar",
        })?;
    let category_len_u64 =
        u64::try_from(category.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "matrix sidecar",
        })?;
    crate::matrix::matrix_sidecar_write_len(
        spec,
        MATRIX_SIDECAR_FIXED_LEN as u64,
        format_magic_len,
        category_len_u64,
        payload_len,
    )?;
    let manifest = matrix_sidecar_manifest_for(spec, category, generation, payload, identity)?;
    let magic_len =
        u16::try_from(manifest.format_magic.len()).map_err(|_| Error::InvalidMatrixSidecar)?;
    let category_len =
        u16::try_from(manifest.category.len()).map_err(|_| Error::InvalidMatrixSidecar)?;
    let header_len = MATRIX_SIDECAR_FIXED_LEN
        .checked_add(magic_len as usize)
        .and_then(|len| len.checked_add(category_len as usize))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "sidecar header length",
        })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(header_len)
        .map_err(|_| Error::AllocationFailed {
            resource: "sidecar header",
            requested: header_len as u64,
        })?;
    bytes.extend_from_slice(MATRIX_SIDECAR_MAGIC);
    bytes.extend_from_slice(&MATRIX_SIDECAR_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&manifest.format_version.to_le_bytes());
    bytes.extend_from_slice(&magic_len.to_le_bytes());
    bytes.extend_from_slice(&category_len.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&manifest.schema_hash.to_le_bytes());
    bytes.extend_from_slice(&manifest.generation.to_le_bytes());
    bytes.extend_from_slice(&manifest.payload_len.to_le_bytes());
    bytes.extend_from_slice(&manifest.payload_crc32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&manifest.native_fingerprint);
    bytes.extend_from_slice(&manifest.matrix_layout_generation.to_le_bytes());
    bytes.extend_from_slice(&manifest.matrix_creation_nonce);
    debug_assert_eq!(bytes.len(), MATRIX_SIDECAR_FIXED_LEN);
    bytes.extend_from_slice(&manifest.format_magic);
    bytes.extend_from_slice(manifest.category.as_bytes());

    // Publish atomically via a same-directory RAII temp: write the full sidecar
    // into a private temp, fsync it, then atomically replace the destination and
    // sync the parent directory. A crash before the replace leaves the previous
    // sidecar untouched; the old in-place `File::create` truncated it eagerly
    // and could expose a partial sidecar.
    let sidecar_path = sidecar_path.as_ref();
    let (temp_path, mut temp_file) = create_rewrite_temp_file(sidecar_path)?;
    let write_result = (|| -> Result<()> {
        temp_file.write_all(&bytes)?;
        temp_file.write_all(payload)?;
        temp_file.flush()?;
        temp_file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        drop(temp_file);
        let _ = remove_file(&temp_path);
        return Err(error);
    }
    drop(temp_file);

    match replace_path_atomically(&temp_path, sidecar_path) {
        Ok(ReplaceDurability::Durable) => Ok(manifest),
        Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
            // The replacement is already visible at `sidecar_path`; only the
            // parent-directory entry's durability is pending. Do not delete the
            // now-published file, and surface the durability gap so a caller
            // that needs a hard guarantee can react (a regenerable sidecar may
            // simply be rewritten).
            Err(crate::fallible::boxed_path_error(
                sync_error,
                sidecar_path,
                Error::ErrorContextAllocationFailed {
                    context: crate::AllocationFailureContext::ParentSyncPending,
                },
                |path, source| Error::PublishedButParentSyncPending { path, source },
            ))
        }
        Err(error) => Err(preserve_temp_on_indeterminate(&temp_path, error)),
    }
}

fn read_matrix_sidecar_file<P: AsRef<Path>>(
    spec: FormatSpec,
    category: &str,
    sidecar_path: P,
    expected_generation: Option<u64>,
    identity: MatrixNativeIdentity,
) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
    let mut file = File::open(sidecar_path)?;
    let file_len = file.metadata()?.len();
    crate::matrix::check_matrix_sidecar_file_len(spec, file_len)?;
    if file_len < MATRIX_SIDECAR_FIXED_LEN as u64 {
        return Err(Error::InvalidMatrixSidecar);
    }
    let mut fixed = [0; MATRIX_SIDECAR_FIXED_LEN];
    file.read_exact(&mut fixed)?;
    if &fixed[0..4] != MATRIX_SIDECAR_MAGIC {
        return Err(Error::InvalidMatrixSidecar);
    }
    let version = u16::from_le_bytes(fixed[4..6].try_into().expect("slice"));
    if version != MATRIX_SIDECAR_VERSION {
        // A different envelope version (legacy v1 lacked native identity,
        // legacy v2 lacked the matrix creation nonce) is refused as stale and
        // regenerable rather than trusted.
        return Err(Error::MatrixSidecarMismatch("sidecar version"));
    }
    let format_version = u16::from_le_bytes(fixed[8..10].try_into().expect("slice"));
    let flags = u16::from_le_bytes(fixed[6..8].try_into().expect("slice"));
    let magic_len = u64::from(u16::from_le_bytes(fixed[10..12].try_into().expect("slice")));
    let category_len = u64::from(u16::from_le_bytes(fixed[12..14].try_into().expect("slice")));
    let reserved = u16::from_le_bytes(fixed[14..16].try_into().expect("slice"));
    let schema_hash = u64::from_le_bytes(fixed[16..24].try_into().expect("slice"));
    let generation = u64::from_le_bytes(fixed[24..32].try_into().expect("slice"));
    let payload_len = u64::from_le_bytes(fixed[32..40].try_into().expect("slice"));
    let payload_crc32 = u32::from_le_bytes(fixed[40..44].try_into().expect("slice"));
    let trailing_reserved = u32::from_le_bytes(fixed[44..48].try_into().expect("slice"));
    let mut native_fingerprint = [0u8; 32];
    native_fingerprint.copy_from_slice(
        &fixed[MATRIX_SIDECAR_FINGERPRINT_OFFSET..MATRIX_SIDECAR_LAYOUT_GENERATION_OFFSET],
    );
    let matrix_layout_generation = u64::from_le_bytes(
        fixed[MATRIX_SIDECAR_LAYOUT_GENERATION_OFFSET..MATRIX_SIDECAR_CREATION_NONCE_OFFSET]
            .try_into()
            .expect("slice"),
    );
    let mut matrix_creation_nonce = [0u8; MATRIX_CREATION_NONCE_LEN];
    matrix_creation_nonce
        .copy_from_slice(&fixed[MATRIX_SIDECAR_CREATION_NONCE_OFFSET..MATRIX_SIDECAR_FIXED_LEN]);
    let plan = crate::matrix::matrix_sidecar_read_plan(
        spec,
        file_len,
        MATRIX_SIDECAR_FIXED_LEN as u64,
        magic_len,
        category_len,
        payload_len,
        flags,
        reserved,
        trailing_reserved,
    )?;
    // only the small fixed header and the bounded magic/category
    // prefix are read before identity validation. A sidecar that belongs to a
    // different native file, schema, category or generation is rejected here,
    // before the payload is ever allocated, read or hashed.
    file.seek(SeekFrom::Start(plan.format_magic_offset))?;
    let mut format_magic = try_alloc_bytes(magic_len, "sidecar format magic")?;
    file.read_exact(&mut format_magic)?;
    file.seek(SeekFrom::Start(plan.category_offset))?;
    let mut category_bytes = try_alloc_bytes(category_len, "sidecar category")?;
    file.read_exact(&mut category_bytes)?;
    let category_text =
        String::from_utf8(category_bytes).map_err(|_| Error::InvalidMatrixSidecar)?;
    let manifest = MatrixSidecarManifest {
        format_magic,
        format_version,
        schema_hash,
        category: category_text,
        generation,
        payload_len,
        payload_crc32,
        native_fingerprint,
        matrix_layout_generation,
        matrix_creation_nonce,
    };
    validate_matrix_sidecar_manifest(spec, category, expected_generation, identity, &manifest)?;
    file.seek(SeekFrom::Start(plan.payload_offset))?;
    let mut payload = try_alloc_bytes(plan.payload_len, "sidecar payload")?;
    file.read_exact(&mut payload)?;
    let actual = crc32_bytes(&payload)?;
    if actual != payload_crc32 {
        return Err(Error::MatrixSidecarChecksumMismatch {
            expected: payload_crc32,
            actual,
        });
    }
    Ok((manifest, payload))
}

fn try_alloc_bytes(len: u64, resource: &'static str) -> Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested: u64::try_from(len).unwrap_or(u64::MAX),
        })?;
    bytes.resize(len, 0);
    Ok(bytes)
}

fn matrix_sidecar_manifest_for(
    spec: FormatSpec,
    category: &str,
    generation: u64,
    payload: &[u8],
    identity: MatrixNativeIdentity,
) -> Result<MatrixSidecarManifest> {
    if category.is_empty() {
        return Err(Error::InvalidMatrixSidecar);
    }
    let mut format_magic = try_alloc_bytes(
        u64::try_from(spec.magic.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "matrix sidecar",
        })?,
        "sidecar format magic",
    )?;
    format_magic.copy_from_slice(spec.magic);
    let category_len =
        u64::try_from(category.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "matrix sidecar",
        })?;
    let mut category_text = String::new();
    category_text
        .try_reserve_exact(category.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "sidecar category",
            requested: category_len,
        })?;
    category_text.push_str(category);
    Ok(MatrixSidecarManifest {
        format_magic,
        format_version: spec.version,
        schema_hash: spec.computed_schema_hash(),
        category: category_text,
        generation,
        payload_len: payload
            .len()
            .try_into()
            .map_err(|_| Error::InvalidMatrixSidecar)?,
        payload_crc32: crc32_bytes(payload)?,
        native_fingerprint: identity.fingerprint,
        matrix_layout_generation: identity.layout_generation,
        matrix_creation_nonce: identity.creation_nonce,
    })
}

fn validate_matrix_sidecar_manifest(
    spec: FormatSpec,
    category: &str,
    expected_generation: Option<u64>,
    identity: MatrixNativeIdentity,
    manifest: &MatrixSidecarManifest,
) -> Result<()> {
    if manifest.format_magic != spec.magic {
        return Err(Error::MatrixSidecarMismatch("format magic"));
    }
    if manifest.format_version != spec.version {
        return Err(Error::MatrixSidecarMismatch("format version"));
    }
    if manifest.schema_hash != spec.computed_schema_hash() {
        return Err(Error::MatrixSidecarMismatch("schema hash"));
    }
    if manifest.category != category {
        return Err(Error::MatrixSidecarMismatch("category"));
    }
    // Bind to a specific native file and its matrix layout: a same-spec,
    // same-generation sibling produces a different OS-object fingerprint and is
    // rejected.
    if manifest.native_fingerprint != identity.fingerprint {
        return Err(Error::MatrixSidecarMismatch("native identity"));
    }
    if manifest.matrix_layout_generation != identity.layout_generation {
        return Err(Error::MatrixSidecarMismatch("matrix layout generation"));
    }
    // Same OS object, schema and layout offsets are not enough: a matrix
    // recreated in the same file object gets a fresh creation nonce, so every
    // sidecar published against the previous logical matrix is refused as
    // stale and regenerable.
    if manifest.matrix_creation_nonce != identity.creation_nonce {
        return Err(Error::MatrixSidecarMismatch("creation nonce"));
    }
    if let Some(expected_generation) = expected_generation
        && manifest.generation != expected_generation
    {
        return Err(Error::MatrixSidecarMismatch("generation"));
    }
    Ok(())
}

#[cfg(feature = "integrity")]
fn crc32_bytes(payload: &[u8]) -> Result<u32> {
    Ok(crc32fast::hash(payload))
}

#[cfg(not(feature = "integrity"))]
fn crc32_bytes(_payload: &[u8]) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

#[cfg(feature = "integrity")]
fn crc32_record_bytes(header: &[u8], payload: &[u8], footer: &[u8]) -> Result<u32> {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(header);
    hasher.update(payload);
    hasher.update(footer);
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
fn crc32_record_bytes(_header: &[u8], _payload: &[u8], _footer: &[u8]) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

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
        let write = RecordOverwrite::prepare(target, &writer.index, &mut writer.keyed_tails)?;
        writer
            .file
            .overwrite_indexed_record(write, header_bytes, payload)
    }
    fn write_user_record(
        &mut self,
        block_id: u32,
        block_version: u16,
        kind: BlockKind,
        payload: &[u8],
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        self.check()?;
        let payload = prepare_user_record_payload(self.spec, block_id, kind, payload)?;
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
    fn write_record_with_prev_key(
        &mut self,
        block_id: u32,
        block_version: u16,
        flags: u16,
        uncompressed_len_hint: u32,
        payload: &[u8],
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        self.check()?;
        let payload_len =
            u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
        let logical_len = record_logical_len_from_parts(
            self.spec,
            block_id,
            flags,
            uncompressed_len_hint,
            payload,
        )?;
        self.spec
            .read_limits
            .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;

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
            checkpoint_cadence: self.checkpoint_cadence,
            segment_cursor: self.segment_cursor,
            block_id,
            previous_block_tail: self.block_tails.tail(block_id),
            uncommitted_since_commit: self.uncommitted_since_commit,
            appended_since_digest: self.appended_since_digest,
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
        let checksum =
            checksum_record_fields(self.spec, record_offset, header, payload, footer_bytes)?;
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
        let write_result = self.file.append_record_at_end(
            &index_slot,
            record_offset,
            &header_bytes,
            payload,
            footer.as_ref().map(|bytes| &bytes[..]),
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
            // Single index append site: keep the O(1) flush-cadence state in
            // lockstep with the resident index. It counts resident
            // entries, so a non-resident record must not advance it.
            let ordinal = self.index.len() - 1;
            self.checkpoint_cadence.note_appended(ordinal, block_id);
            self.segment_cursor
                .note_appended(ordinal, block_id, prospective_len);
        } else {
            drop(index_slot);
        }
        self.block_tails.note_appended(block_id, record_offset);
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
use header_codec::*;

pub(crate) use header_codec::file_header_extensions;

pub(crate) use header_codec::header_tails_region_len;

pub(crate) use header_codec::blank_mutable_header_bytes;

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

pub(crate) use scan::read_stream_entry_at;

pub(crate) use scan::PreparedStreamRecord;

pub(crate) use scan::PreparedRecordParts;

#[cfg(test)]
pub(crate) use scan::prepare_stream_user_record;
pub(crate) use scan::prepare_stream_user_record_into;

pub(crate) use scan::prepare_stream_tombstone_record;

pub(crate) use scan::prepare_stream_manifest_record;

pub(crate) use scan::fresh_stream_creation_nonce;

pub(crate) use scan::prepare_stream_creation_nonce_record;

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
