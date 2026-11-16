use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt, fs,
    hash::Hash,
    path::{Path, PathBuf},
};

#[cfg(test)]
mod oracle;
#[cfg(kani)]
mod publication_proofs;
mod slots;
mod storage;
mod tree;
use storage::{Checkpoint as IndexCheckpoint, Publication, Store as IndexStorage};

use crate::{Error, FormatSpec, ResourceLimits, TOMBSTONE_BLOCK_ID};

const DEFAULT_CACHE_BYTES: usize = 8 * 1024 * 1024;
const MIN_CACHE_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_KEY_BYTES: usize = 1024 * 1024;
const DEFAULT_BATCH_RECORDS: usize = 16_384;
const DEFAULT_BATCH_BYTES: usize = 64 * 1024 * 1024;

const META_MAGIC: [u8; 8] = *b"VARVEVKI";
// v3 adds the primary generation witness. A v2 sidecar is 260 bytes
// where this record is 300, so it is refused with the typed `MetadataLength`
// error before the version field is read, and is stale-regenerable by rebuild.
const META_VERSION: u16 = 3;
const META_LEN: usize = 300;
const LATEST_LEN: usize = 52;
const TAIL_LEN: usize = 32;
const BATCH_UPDATE_FIXED_BYTES: usize = 32 + DiskIndexKeyTag::ENCODED_LEN + LATEST_LEN;
const BATCH_TAIL_BYTES: usize = 4 + TAIL_LEN;

/// Fixed schema key-kind tag, not an ID assigned to a runtime key value.
/// The kind code is the declaring block's stable schema ID. A composite key
/// declaration is one kind regardless of the number of component fields.
/// Absence is independent of deletion and of an empty encoded key value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiskIndexKeyTag {
    kind: Option<u32>,
}

impl DiskIndexKeyTag {
    pub const ENCODED_LEN: usize = 5;
}

const SEQUENCE_COMMITTED_EXHAUSTED: u8 = 1 << 0;
const SEQUENCE_WORKING_EXHAUSTED: u8 = 1 << 1;
const SEQUENCE_BASE_EXHAUSTED: u8 = 1 << 2;
const SEQUENCE_FLAGS: u8 =
    SEQUENCE_COMMITTED_EXHAUSTED | SEQUENCE_WORKING_EXHAUSTED | SEQUENCE_BASE_EXHAUSTED;

const LATEST_HAS_PHYSICAL: u8 = 1 << 0;
const LATEST_HAS_NATIVE_CRC: u8 = 1 << 1;
const LATEST_FLAGS: u8 = LATEST_HAS_PHYSICAL | LATEST_HAS_NATIVE_CRC;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexBatchOptions {
    pub max_records: usize,
    pub max_bytes: usize,
}

impl Default for DiskIndexBatchOptions {
    fn default() -> Self {
        Self {
            max_records: DEFAULT_BATCH_RECORDS,
            max_bytes: DEFAULT_BATCH_BYTES,
        }
    }
}

/// How far a checkpointed writer lets its sidecar trail the primary.
///
/// A sync is published with one storage barrier, on the primary, and closed by
/// a sync marker there. The sidecar is confirmed — written and synced — only
/// once the primary has grown `max_bytes`, or `max_records` records, past the
/// last confirmed EOF, and by compaction. Readers and recovery replay what the
/// sidecar does not cover from the primary, so these bound the bytes a
/// recovery reads and the records a reader holds in its tail map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexConfirmPolicy {
    pub max_bytes: u64,
    pub max_records: u64,
}

impl Default for DiskIndexConfirmPolicy {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            max_records: 65_536,
        }
    }
}

impl DiskIndexConfirmPolicy {
    /// Confirms on every sync: the sidecar never trails the primary.
    pub const EVERY_SYNC: Self = Self {
        max_bytes: 0,
        max_records: 0,
    };

    pub(crate) const fn due(self, bytes: u64, records: u64) -> bool {
        bytes >= self.max_bytes || records >= self.max_records
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexOptions {
    /// Index-cache accounting budget per reader/snapshot, in bytes.
    /// Defaults to 8 MiB; the minimum accepted value is 1 MiB.
    /// Set this before opening a reader. Independent readers can use different
    /// budgets. This excludes OS file cache, temporary buffers and other heap
    /// overhead, so it is not a process RSS limit.
    pub cache_bytes: usize,
    pub max_key_bytes: usize,
    pub batch: DiskIndexBatchOptions,
    pub limits: ResourceLimits,
    /// Writer only: when a sync also confirms the sidecar.
    pub confirm: DiskIndexConfirmPolicy,
    /// Writer only: how many further confirmed generations a page that a
    /// confirmed generation stopped referencing waits before it is reused.
    /// Readers that `follow` at least that often never meet a reclaimed page;
    /// one that falls further behind would see a damaged-sidecar error.
    /// Crash safety does not depend on it: recovery only ever returns to the
    /// newest confirmed generation.
    pub reclaim_after: u64,
}

impl Default for DiskIndexOptions {
    fn default() -> Self {
        Self {
            cache_bytes: DEFAULT_CACHE_BYTES,
            max_key_bytes: DEFAULT_MAX_KEY_BYTES,
            batch: DiskIndexBatchOptions::default(),
            // The native index performs paged access; its file length is not an allocation request.
            // Callers can opt into a finite sidecar policy per open when required.
            limits: ResourceLimits::MISSING.with_max_sidecar_len(u64::MAX),
            confirm: DiskIndexConfirmPolicy::default(),
            reclaim_after: 2,
        }
    }
}

impl DiskIndexOptions {
    fn validate(self) -> DiskIndexResult<Self> {
        if self.cache_bytes < MIN_CACHE_BYTES {
            return Err(DiskIndexError::CacheTooSmall {
                actual: self.cache_bytes,
                minimum: MIN_CACHE_BYTES,
            });
        }
        if self.max_key_bytes == 0 {
            return Err(DiskIndexError::InvalidBatchOptions(
                "max_key_bytes must be nonzero",
            ));
        }
        if self.max_key_bytes > u32::MAX as usize {
            return Err(DiskIndexError::InvalidBatchOptions(
                "max_key_bytes exceeds the sidecar key representation",
            ));
        }
        if self.batch.max_records == 0 {
            return Err(DiskIndexError::InvalidBatchOptions(
                "max_records must be nonzero",
            ));
        }
        if self.batch.max_bytes == 0 {
            return Err(DiskIndexError::InvalidBatchOptions(
                "max_bytes must be nonzero",
            ));
        }
        if self.batch.max_bytes < batch_item_fixed_bytes(true) {
            return Err(DiskIndexError::InvalidBatchOptions(
                "max_bytes is too small for one checkpoint item",
            ));
        }
        if self.max_key_bytes > self.batch.max_bytes {
            return Err(DiskIndexError::InvalidBatchOptions(
                "max_key_bytes must not exceed max_bytes",
            ));
        }
        Ok(self)
    }
}

pub type DiskIndexResult<T> = std::result::Result<T, DiskIndexError>;

/// Observability counter for the scaling contract the release gates pin
/// (primary generation scans). Thread-local and only compiled under
/// `scalable-fault-injection`; the release build keeps the call site as an
/// empty inline function.
#[cfg(feature = "scalable-fault-injection")]
pub(crate) mod scaling_counters {
    use std::cell::Cell;

    thread_local! {
        static PRIMARY_GENERATION_SCANS: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn add_primary_generation_scan() {
        PRIMARY_GENERATION_SCANS.with(|value| value.set(value.get().saturating_add(1)));
    }

    pub(crate) fn primary_generation_scans() -> u64 {
        PRIMARY_GENERATION_SCANS.with(Cell::get)
    }

    pub(crate) fn reset() {
        PRIMARY_GENERATION_SCANS.with(|value| value.set(0));
    }
}

#[inline(always)]
pub(crate) fn primary_generation_scan() {
    #[cfg(feature = "scalable-fault-injection")]
    scaling_counters::add_primary_generation_scan();
}

#[derive(Debug)]
#[non_exhaustive]
pub enum DiskIndexError {
    CacheTooSmall {
        actual: usize,
        minimum: usize,
    },
    InvalidBatchOptions(&'static str),
    KeyTooLong {
        actual: usize,
        limit: usize,
    },
    MetadataMissing,
    MetadataLength {
        actual: usize,
        expected: usize,
    },
    MetadataMagic,
    MetadataVersion {
        actual: u16,
    },
    MetadataChecksum,
    MetadataReserved,
    MetadataState {
        actual: u8,
    },
    MetadataMode {
        actual: u8,
    },
    MetadataFlags {
        actual: u8,
    },
    MetadataInvariant(&'static str),
    LatestLength {
        actual: usize,
        expected: usize,
    },
    LatestChecksum,
    LatestReserved,
    LatestInvariant(&'static str),
    TailLength {
        actual: usize,
        expected: usize,
    },
    TailChecksum,
    TailReserved,
    TailKeyMismatch {
        key: u32,
        encoded: u32,
    },
    TailLimitExceeded {
        actual: u64,
        limit: u32,
    },
    TailLimitMismatch {
        expected: u32,
        actual: u32,
    },
    TailCountMismatch {
        expected: u32,
        actual: u32,
    },
    TailDigestMismatch,
    InvalidTail(&'static str),
    IdentityMismatch,
    /// The primary no longer matches the generation the sidecar was published
    /// against: the leading bytes of the file changed underneath it, outside a
    /// declared header tail region, which is excluded from the witness.
    PrimaryGenerationMismatch,
    ModeMismatch {
        expected: DiskIndexMode,
        actual: DiskIndexMode,
    },
    PlanEmpty,
    PlanNotCanonical,
    PlanCodecIdentityMissing {
        block_id: u32,
    },
    PlanBlockMissing {
        block_id: u32,
    },
    PlanBlockVersionMismatch {
        block_id: u32,
        expected: u16,
        actual: u16,
    },
    PlanDigestMismatch,
    PlanDigestIsZero,
    CleanStateRequired,
    DirtyStateRequired,
    CheckpointMismatch(&'static str),
    CoverageMismatch {
        expected: u64,
        actual: u64,
    },
    NativeTooShort {
        required: u64,
        actual: u64,
    },
    NativeLengthMismatch {
        expected: u64,
        actual: u64,
    },
    SequenceMismatch {
        expected: Option<u64>,
        actual: u64,
    },
    RecordCountExhausted,
    GenerationExhausted,
    RecordExtentMismatch {
        expected_end: u64,
        actual_end: u64,
    },
    BatchFull {
        records: usize,
        bytes: usize,
        max_records: usize,
        max_bytes: usize,
    },
    KeyTableUnavailable,
    SnapshotReleased,
    Busy,
    Storage(String),
    Primary(Error),
    Io(std::io::Error),
}

impl fmt::Display for DiskIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CacheTooSmall { actual, minimum } => write!(
                f,
                "disk index cache size {actual} is below the {minimum} byte minimum"
            ),
            Self::InvalidBatchOptions(reason) => {
                write!(f, "invalid disk index batch options: {reason}")
            }
            Self::KeyTooLong { actual, limit } => write!(
                f,
                "canonical disk index key is too long: {actual} bytes exceeds {limit}"
            ),
            Self::MetadataMissing => write!(f, "disk index metadata row is missing"),
            Self::MetadataLength { actual, expected } => write!(
                f,
                "disk index metadata has length {actual}; expected {expected}"
            ),
            Self::MetadataMagic => write!(f, "disk index metadata has the wrong magic"),
            Self::MetadataVersion { actual } => write!(
                f,
                "disk index metadata version {actual} is unsupported; expected {META_VERSION}"
            ),
            Self::MetadataChecksum => write!(f, "disk index metadata checksum mismatch"),
            Self::MetadataReserved => {
                write!(f, "disk index metadata reserved bytes are nonzero")
            }
            Self::MetadataState { actual } => {
                write!(f, "disk index metadata has unknown state {actual}")
            }
            Self::MetadataMode { actual } => {
                write!(f, "disk index metadata has unknown mode {actual}")
            }
            Self::MetadataFlags { actual } => {
                write!(f, "disk index metadata has unknown flags {actual:#04x}")
            }
            Self::MetadataInvariant(reason) => {
                write!(f, "disk index metadata invariant failed: {reason}")
            }
            Self::LatestLength { actual, expected } => write!(
                f,
                "disk index entry has length {actual}; expected {expected}"
            ),
            Self::LatestChecksum => write!(f, "disk index entry checksum mismatch"),
            Self::LatestReserved => write!(f, "disk index entry reserved bytes are nonzero"),
            Self::LatestInvariant(reason) => {
                write!(f, "disk index entry invariant failed: {reason}")
            }
            Self::TailLength { actual, expected } => write!(
                f,
                "disk index tail has length {actual}; expected {expected}"
            ),
            Self::TailChecksum => write!(f, "disk index tail checksum mismatch"),
            Self::TailReserved => write!(f, "disk index tail reserved bytes are nonzero"),
            Self::TailKeyMismatch { key, encoded } => write!(
                f,
                "disk index tail key {key} does not match encoded block {encoded}"
            ),
            Self::TailLimitExceeded { actual, limit } => write!(
                f,
                "disk index has {actual} tails, exceeding declared bound {limit}"
            ),
            Self::TailLimitMismatch { expected, actual } => write!(
                f,
                "disk index tail bound mismatch: expected {expected}, got {actual}"
            ),
            Self::TailCountMismatch { expected, actual } => write!(
                f,
                "disk index tail count mismatch: expected {expected}, got {actual}"
            ),
            Self::TailDigestMismatch => write!(f, "disk index tail digest mismatch"),
            Self::InvalidTail(reason) => write!(f, "invalid disk index tail: {reason}"),
            Self::IdentityMismatch => write!(f, "disk index does not match the primary file"),
            Self::PrimaryGenerationMismatch => write!(
                f,
                "disk index was published against a different generation of the primary file"
            ),
            Self::ModeMismatch { expected, actual } => write!(
                f,
                "disk index mode mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::PlanEmpty => write!(f, "disk index plan has no descriptors"),
            Self::PlanNotCanonical => {
                write!(f, "disk index plan descriptors are not strictly ordered")
            }
            Self::PlanCodecIdentityMissing { block_id } => write!(
                f,
                "disk index descriptor {block_id} has no key codec identity"
            ),
            Self::PlanBlockMissing { block_id } => write!(
                f,
                "disk index descriptor references undeclared block {block_id}"
            ),
            Self::PlanBlockVersionMismatch {
                block_id,
                expected,
                actual,
            } => write!(
                f,
                "disk index block {block_id} version mismatch: expected {expected}, got {actual}"
            ),
            Self::PlanDigestMismatch => write!(f, "disk index plan digest mismatch"),
            Self::PlanDigestIsZero => write!(f, "disk index plan digest may not be zero"),
            Self::CleanStateRequired => write!(f, "disk index clean state is required"),
            Self::DirtyStateRequired => write!(f, "disk index dirty state is required"),
            Self::CheckpointMismatch(reason) => {
                write!(f, "disk index checkpoint mismatch: {reason}")
            }
            Self::CoverageMismatch { expected, actual } => write!(
                f,
                "disk index coverage mismatch: expected {expected}, got {actual}"
            ),
            Self::NativeTooShort { required, actual } => write!(
                f,
                "native file length {actual} is below required checkpoint EOF {required}"
            ),
            Self::NativeLengthMismatch { expected, actual } => write!(
                f,
                "native file length mismatch: expected {expected}, got {actual}"
            ),
            Self::SequenceMismatch { expected, actual } => write!(
                f,
                "disk index sequence mismatch: expected {expected:?}, got {actual}"
            ),
            Self::RecordCountExhausted => write!(f, "disk index record count is exhausted"),
            Self::GenerationExhausted => write!(f, "disk index generation is exhausted"),
            Self::RecordExtentMismatch {
                expected_end,
                actual_end,
            } => write!(
                f,
                "disk index record extent mismatch: expected end {expected_end}, got {actual_end}"
            ),
            Self::BatchFull {
                records,
                bytes,
                max_records,
                max_bytes,
            } => write!(
                f,
                "disk index batch would reach {records} records/{bytes} bytes, exceeding {max_records} records/{max_bytes} bytes"
            ),
            Self::KeyTableUnavailable => {
                write!(f, "disk key lookup is unavailable in state-only mode")
            }
            Self::SnapshotReleased => write!(
                f,
                "reader snapshot released; call follow before indexed reads"
            ),
            Self::Busy => write!(f, "disk index database is already open for writing"),
            Self::Storage(error) => write!(f, "disk index storage error: {error}"),
            Self::Primary(error) => write!(f, "primary file error during index operation: {error}"),
            Self::Io(error) => write!(f, "disk index I/O error: {error}"),
        }
    }
}

impl std::error::Error for DiskIndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Primary(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

// Allocation failures must not allocate again while crossing the public API.
/// Whether a writer open failed only because the sidecar trails the primary,
/// which recovery resolves without losing an acknowledged sync.
pub(crate) fn requires_recovery(error: &Error) -> bool {
    matches!(error, Error::DiskIndex(inner) if matches!(**inner, DiskIndexError::CleanStateRequired))
}

pub(crate) fn index_error(error: DiskIndexError) -> Error {
    match error {
        DiskIndexError::Busy => Error::IndexBusy,
        DiskIndexError::Io(error) if error.kind() == std::io::ErrorKind::OutOfMemory => {
            Error::Io(error)
        }
        DiskIndexError::Primary(error @ Error::AllocationFailed { .. }) => error,
        DiskIndexError::Primary(Error::Io(error))
            if error.kind() == std::io::ErrorKind::OutOfMemory =>
        {
            Error::Io(error)
        }
        error => match crate::fallible::try_box(DiskIndexError::Busy) {
            Ok(mut slot) => {
                *slot = error;
                Error::DiskIndex(slot)
            }
            Err(refusal) => match error {
                // Preserve any completed/uncertain publication facts even if
                // the extra disk-index wrapper cannot be allocated.
                DiskIndexError::Primary(source) => source,
                _ => refusal,
            },
        },
    }
}

impl From<std::io::Error> for DiskIndexError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<Error> for DiskIndexError {
    fn from(value: Error) -> Self {
        Self::Primary(value)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiskIndexDigest([u8; 32]);

impl DiskIndexDigest {
    pub const ZERO: Self = Self([0; 32]);

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub const fn is_zero(self) -> bool {
        let mut index = 0;
        while index < self.0.len() {
            if self.0[index] != 0 {
                return false;
            }
            index += 1;
        }
        true
    }
}

impl fmt::Debug for DiskIndexDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DiskIndexDigest(")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        f.write_str(")")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskIndexMode {
    StateOnly,
    DiskPlan(DiskIndexDigest),
}

impl DiskIndexMode {
    pub const fn plan_digest(self) -> Option<DiskIndexDigest> {
        match self {
            Self::StateOnly => None,
            Self::DiskPlan(digest) => Some(digest),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexIdentity {
    pub schema_hash: u64,
    pub primary_fingerprint: [u8; 32],
}

/// Bytes of the primary covered by the generation witness.
///
/// The window is a small constant: the witness must catch a rewrite of the
/// primary's leading records, and verifying it costs one bounded read at open
/// regardless of file size. Growing it would not change the class of rewrite
/// it detects, only how far into the file the check reaches.
pub(crate) const PRIMARY_GENERATION_WINDOW: u64 = 4096;

/// Witness that the sidecar still belongs to the primary generation it was
/// published against.
///
/// Schema hash, OS object identity and the deterministic file header are all
/// stable across an in-place rewrite of a primary by an equal-length primary
/// of the same format, so they cannot distinguish two logical generations of
/// one file object. `digest` covers the first `len` bytes of the primary as of
/// the recorded commit, which no such rewrite preserves unless it reproduces
/// those bytes exactly.
///
/// One exclusion, and it is the only part of a primary that may change without
/// a new generation: a declared `IndexPolicy::header_tails` region's payload is
/// blanked before the digest is taken, because a commit rewrites it in place
/// and the witness would otherwise become a commit counter. The region's magic
/// and declared length stay covered, which is what still catches a region that
/// changed size — the change that moves every record offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexPrimaryGeneration {
    pub len: u64,
    pub digest: DiskIndexDigest,
}

impl DiskIndexPrimaryGeneration {
    pub const EMPTY: Self = Self {
        len: 0,
        digest: DiskIndexDigest([0u8; 32]),
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskIndexState {
    Dirty,
    Clean,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexFrontier {
    pub eof: u64,
    pub record_count: u64,
    pub next_sequence: Option<u64>,
}

impl DiskIndexFrontier {
    pub const fn new(eof: u64, record_count: u64, next_sequence: Option<u64>) -> Self {
        Self {
            eof,
            record_count,
            next_sequence,
        }
    }

    #[cfg(test)]
    pub const fn empty(eof: u64) -> Self {
        Self::new(eof, 0, Some(0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexCheckpoint {
    pub checkpoint_offset: u64,
    pub base: DiskIndexFrontier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexMetadata {
    pub identity: DiskIndexIdentity,
    pub primary_generation: DiskIndexPrimaryGeneration,
    pub mode: DiskIndexMode,
    pub state: DiskIndexState,
    pub generation: u64,
    pub committed: DiskIndexFrontier,
    pub working: DiskIndexFrontier,
    pub checkpoint: Option<DiskIndexCheckpoint>,
    pub tail_limit: u32,
    pub committed_tail_count: u32,
    pub working_tail_count: u32,
    pub committed_tail_digest: DiskIndexDigest,
    pub working_tail_digest: DiskIndexDigest,
}

impl DiskIndexMetadata {
    pub fn new_state(
        identity: DiskIndexIdentity,
        frontier: DiskIndexFrontier,
        tail_limit: u32,
    ) -> Self {
        Self::new_with_mode(identity, DiskIndexMode::StateOnly, frontier, tail_limit)
    }

    fn new_with_mode(
        identity: DiskIndexIdentity,
        mode: DiskIndexMode,
        frontier: DiskIndexFrontier,
        tail_limit: u32,
    ) -> Self {
        let empty_digest = tail_digest(std::iter::empty());
        Self {
            identity,
            primary_generation: DiskIndexPrimaryGeneration::EMPTY,
            mode,
            state: DiskIndexState::Clean,
            generation: 0,
            committed: frontier,
            working: frontier,
            checkpoint: None,
            tail_limit,
            committed_tail_count: 0,
            working_tail_count: 0,
            committed_tail_digest: empty_digest,
            working_tail_digest: empty_digest,
        }
    }

    #[must_use]
    pub fn with_primary_generation(mut self, generation: DiskIndexPrimaryGeneration) -> Self {
        self.primary_generation = generation;
        self
    }
}

pub fn tail_limit_for_spec(spec: FormatSpec) -> DiskIndexResult<u32> {
    if !spec.index_policy.block_offset_chain {
        return Ok(0);
    }
    let internal = crate::stream::INTERNAL_BLOCK_IDS
        .iter()
        .filter(|&&id| {
            id != crate::MANIFEST_BLOCK_ID
                || spec.manifest_policy == crate::ManifestPolicy::Embedded
        })
        .count();
    let count =
        spec.blocks
            .len()
            .checked_add(internal)
            .ok_or(DiskIndexError::MetadataInvariant(
                "tail declaration count overflow",
            ))?;
    u32::try_from(count)
        .map_err(|_| DiskIndexError::MetadataInvariant("tail declaration count exceeds u32"))
}

/// The exact old bound is accepted only for formats whose corrected bound is
/// larger. It still bounds tail allocation before any persisted count is used.
#[derive(Clone, Copy)]
struct TailLimitPolicy {
    current: u32,
    legacy: Option<u32>,
}

impl TailLimitPolicy {
    fn for_spec(spec: FormatSpec) -> DiskIndexResult<Self> {
        let current = tail_limit_for_spec(spec)?;
        let legacy = spec
            .blocks
            .len()
            .checked_add(2)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n < current);
        Ok(Self { current, legacy })
    }

    fn validate(self, metadata: DiskIndexMetadata) -> DiskIndexResult<u32> {
        if metadata.tail_limit != self.current && Some(metadata.tail_limit) != self.legacy {
            return Err(DiskIndexError::TailLimitMismatch {
                expected: self.current,
                actual: metadata.tail_limit,
            });
        }
        Ok(metadata.tail_limit)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexPhysicalRecord {
    pub physical_len: u64,
    pub block_id: u32,
    pub block_version: u16,
    pub flags: u16,
    pub native_crc: Option<u32>,
}

#[cfg(test)]
std::thread_local! {
    static DISK_KEY_ENCODE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskIndexTail {
    pub block_id: u32,
    pub record_offset: u64,
    pub sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskIndexPersistentState {
    pub metadata: DiskIndexMetadata,
    pub tails: Vec<DiskIndexTail>,
}

#[cfg(test)]
std::thread_local! {
    static TOMBSTONE_KEY_DECODE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PLAN_PAYLOAD_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Physical sidecar sizes around an explicit compaction. Old open readers may
/// retain the previous file until they close; tombstones remain indexed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexCompaction {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub historical_distinct_keys: u64,
}

/// This reader's last adopted generation and lag behind the confirmed frontier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotStatus {
    pub generation: u64,
    pub confirmed_generation: u64,
    pub pinned: bool,
    pub records_behind: u64,
    pub bytes_behind: u64,
}

// Each snapshot owns its read-only file description and bounded private caches.
// No process-wide reader registration is needed to retain an old file generation.
struct SnapshotPin {
    // Held, not read: they keep the confirmed generation's file description
    // and root alive for as long as the snapshot.
    _reader: storage::Reader,
    _root: storage::Root,
}

// Mirrors the private `stream::opened_file_identity` helper; the sidecar
// handle identity must match the identity the sidecar fingerprint uses.
#[cfg(unix)]
fn opened_sidecar_identity(file: &fs::File) -> DiskIndexResult<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&metadata.dev().to_le_bytes());
    bytes.extend_from_slice(&metadata.ino().to_le_bytes());
    Ok(bytes)
}

#[cfg(windows)]
#[allow(unsafe_code, reason = "a Windows file identity call")]
fn opened_sidecar_identity(file: &fs::File) -> DiskIndexResult<Vec<u8>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // The handle belongs to `file`, the output points to initialized writable
    // storage, and the OS call does not outlive either value.
    let ok =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, information.as_mut_ptr()) };
    if ok == 0 {
        return Err(DiskIndexError::Io(std::io::Error::last_os_error()));
    }
    // A successful call initializes every field of BY_HANDLE_FILE_INFORMATION.
    let information = unsafe { information.assume_init() };
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    let mut bytes = Vec::with_capacity(12);
    bytes.extend_from_slice(&information.dwVolumeSerialNumber.to_le_bytes());
    bytes.extend_from_slice(&file_index.to_le_bytes());
    Ok(bytes)
}

/// Last committed tail collection of this handle, keyed by the metadata tail
/// digest it was committed under. Lets the next write batch skip re-reading
/// and re-validating every tail row when nothing else committed in between:
/// the digest comparison against the freshly read metadata proves the cached
/// map is exactly the persisted collection.
struct TailCache {
    digest: DiskIndexDigest,
    tails: BTreeMap<u32, DiskIndexTail>,
}

/// Takes the cached tail map when it provably matches the current working
/// root. Ownership moves into the batch; the cache is refilled on commit, so
/// an aborted or failed batch can never leak staged tails back into it.
fn take_tail_cache(
    cache: &mut Option<TailCache>,
    metadata: DiskIndexMetadata,
) -> Option<BTreeMap<u32, DiskIndexTail>> {
    let cached = cache.take()?;
    if cached.digest == metadata.working_tail_digest
        && u32::try_from(cached.tails.len()).ok() == Some(metadata.working_tail_count)
    {
        Some(cached.tails)
    } else {
        None
    }
}

pub struct DiskIndexStore {
    storage: IndexStorage,
    identity: Vec<u8>,
    options: DiskIndexOptions,
    tail_cache: Option<TailCache>,
    slots_cache: Option<slots::Working>,
    finite_layout: Vec<(u32, u32)>,
    /// Corrected capacity for a legacy companion, installed in the next
    /// ordinary dirty-generation commit without an extra write at open.
    write_tail_limit: Option<u32>,
    pub(crate) batch: Option<DiskIndexWriteBatch>,
}

impl DiskIndexStore {
    pub fn create_with_tails(
        path: impl AsRef<Path>,
        options: DiskIndexOptions,
        metadata: DiskIndexMetadata,
        tails: &[DiskIndexTail],
    ) -> DiskIndexResult<Self> {
        let options = options.validate()?;
        let path = canonical_sidecar_path(path.as_ref())?;
        let storage = IndexStorage::open(&path, options, true, true)?;
        let mut store = Self::from_storage(storage, options)?;
        store.initialize(metadata, tails)?;
        Ok(store)
    }
    fn from_storage(storage: IndexStorage, options: DiskIndexOptions) -> DiskIndexResult<Self> {
        let identity = opened_sidecar_identity(&storage.file)?;
        Ok(Self {
            storage,
            identity,
            options,
            tail_cache: None,
            slots_cache: None,
            finite_layout: Vec::new(),
            write_tail_limit: None,
            batch: None,
        })
    }
    /// Opens only read-only file descriptions. Each snapshot has its own bounded cache.
    pub fn open(path: impl AsRef<Path>, options: DiskIndexOptions) -> DiskIndexResult<Self> {
        Self::open_with_role(path.as_ref(), options, false)
    }
    /// The sole writer holds a nonblocking OS writer guard; readers never acquire it.
    pub fn open_writer(path: impl AsRef<Path>, options: DiskIndexOptions) -> DiskIndexResult<Self> {
        Self::open_with_role(path.as_ref(), options, true)
    }
    fn open_with_role(
        path: &Path,
        options: DiskIndexOptions,
        writer: bool,
    ) -> DiskIndexResult<Self> {
        let options = options.validate()?;
        let path = canonical_sidecar_path(path)?;
        let storage = IndexStorage::open(&path, options, false, writer)?;
        let store = Self::from_storage(storage, options)?;
        if writer {
            store.validate_envelope()?;
        } else {
            store.storage.confirmed()?;
        }
        Ok(store)
    }
    pub(crate) fn open_writer_validated(
        path: impl AsRef<Path>,
        options: DiskIndexOptions,
        identity: DiskIndexIdentity,
        mode: DiskIndexMode,
    ) -> DiskIndexResult<Self> {
        let store = Self::open_writer(path, options)?;
        validate_identity_mode(store.read_metadata()?, identity, mode)?;
        Ok(store)
    }
    pub(crate) const fn options(&self) -> DiskIndexOptions {
        self.options
    }

    pub fn read_metadata(&self) -> DiskIndexResult<DiskIndexMetadata> {
        Ok(self.storage.current()?.metadata)
    }
    fn validate_checkpoint(&self, current: &IndexCheckpoint) -> DiskIndexResult<()> {
        if let Some(checkpoint) = current.metadata.checkpoint {
            let base = self.storage.at(checkpoint.checkpoint_offset)?;
            validate_checkpoint_metadata(&current.metadata, base.metadata)?;
        }
        Ok(())
    }
    fn validate_envelope(&self) -> DiskIndexResult<DiskIndexMetadata> {
        let current = self.storage.current()?;
        self.validate_checkpoint(&current)?;
        Ok(current.metadata)
    }
    fn validate_protocol_bounded(
        &self,
        expected_tail_limit: u32,
    ) -> DiskIndexResult<DiskIndexPersistentState> {
        let current = self.storage.current()?;
        self.validate_checkpoint(&current)?;
        self.storage.state(&current, Some(expected_tail_limit))
    }
    pub(crate) fn begin_committed_snapshot(
        &self,
        identity: DiskIndexIdentity,
        mode: DiskIndexMode,
    ) -> DiskIndexResult<DiskIndexSnapshot> {
        let confirmed = self.storage.confirmed()?;
        let metadata = confirmed.metadata;
        validate_identity_mode(metadata, identity, mode)?;
        Ok(DiskIndexSnapshot {
            pin: Some(SnapshotPin {
                _reader: self.storage.reader(&confirmed),
                _root: confirmed.root,
            }),
            metadata,
        })
    }
    pub(crate) fn follow_snapshot(
        &self,
        previous: &DiskIndexSnapshot,
    ) -> DiskIndexResult<DiskIndexSnapshot> {
        self.followed_handle(previous).map(|(_, snapshot)| snapshot)
    }
    pub(crate) fn same_file(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
    pub(crate) fn followed_handle(
        &self,
        previous: &DiskIndexSnapshot,
    ) -> DiskIndexResult<(Self, DiskIndexSnapshot)> {
        // Reopen read-only so an explicitly compacted file generation can be adopted.
        let current = Self::open(&self.storage.path, self.options)?;
        let next =
            current.begin_committed_snapshot(previous.metadata.identity, previous.metadata.mode)?;
        if next.metadata.generation < previous.metadata.generation
            || next.metadata.committed.eof < previous.metadata.committed.eof
            || next.metadata.committed.record_count < previous.metadata.committed.record_count
            || (next.metadata.generation == previous.metadata.generation
                && next.metadata != previous.metadata)
        {
            return Err(DiskIndexError::CheckpointMismatch(
                "committed reader generation regressed or changed in place",
            ));
        }
        Ok((current, next))
    }
    pub fn validate_clean_writer(
        &self,
        identity: DiskIndexIdentity,
        mode: DiskIndexMode,
        physical_native_eof: u64,
        expected_tail_limit: u32,
    ) -> DiskIndexResult<DiskIndexPersistentState> {
        let state = self.validate_protocol_bounded(expected_tail_limit)?;
        validate_identity_mode(state.metadata, identity, mode)?;
        if state.metadata.state != DiskIndexState::Clean {
            return Err(DiskIndexError::CleanStateRequired);
        }
        // A generation's dirty checkpoint is not a barrier, so a crash can
        // leave the newest durable state clean with the primary longer than it
        // covers: an interrupted generation, recovered by restore exactly as a
        // surviving dirty checkpoint is.
        if physical_native_eof > state.metadata.committed.eof {
            return Err(DiskIndexError::CleanStateRequired);
        }
        if physical_native_eof != state.metadata.committed.eof {
            return Err(DiskIndexError::NativeLengthMismatch {
                expected: state.metadata.committed.eof,
                actual: physical_native_eof,
            });
        }
        Ok(state)
    }

    pub(crate) fn validate_clean_writer_for_spec(
        &mut self,
        identity: DiskIndexIdentity,
        mode: DiskIndexMode,
        physical_native_eof: u64,
        spec: FormatSpec,
    ) -> DiskIndexResult<DiskIndexPersistentState> {
        let policy = TailLimitPolicy::for_spec(spec)?;
        let actual = policy.validate(self.read_metadata()?)?;
        let state = self.validate_clean_writer(identity, mode, physical_native_eof, actual)?;
        // A previous writer can die after exposing one clean head but before
        // syncing it. Clean sync is otherwise a no-op for the sidecar. Reconcile
        // only divergent/torn heads; normal matching copies need no extra fsync.
        self.storage.reconcile_confirmed_heads()?;
        self.write_tail_limit = Some(policy.current);
        Ok(state)
    }
    /// Copy the current clean tree into a new file generation. Old readers keep
    /// their open file and never delay the writer. Call sync explicitly first.
    pub(crate) fn compact(&mut self) -> DiskIndexResult<IndexCompaction> {
        self.storage.writer()?;
        let base = self.storage.current()?;
        if base.metadata.state != DiskIndexState::Clean {
            return Err(DiskIndexError::CleanStateRequired);
        }
        let state = self.storage.state(&base, None)?;
        let before_bytes = self.storage.file.metadata()?.len();
        let target =
            crate::TryClone::try_clone(&self.storage.path).map_err(DiskIndexError::Primary)?;
        let temp = tempfile::Builder::new()
            .prefix(".varlit-compact-")
            .tempfile_in(target.parent().unwrap_or(Path::new(".")))?
            .into_temp_path();
        let mut replacement = IndexStorage::open(&temp, self.options, true, true)?;
        // The rebuilt pages belong to the generation the replacement
        // republishes, so none of them reads as reusable once it is published.
        replacement.set_building(base.metadata.generation.max(1))?;
        let mut root = storage::Root::default();
        let mut updates = BTreeMap::new();
        let mut bytes = 0usize;
        let max_key = self
            .options
            .max_key_bytes
            .saturating_add(DiskIndexKeyTag::ENCODED_LEN);
        let cache_bytes = self.options.cache_bytes;
        let mut flush = |root: &mut storage::Root,
                         updates: &mut BTreeMap<Vec<u8>, [u8; LATEST_LEN]>|
         -> DiskIndexResult<()> {
            if updates.is_empty() {
                return Ok(());
            }
            root.tree = replacement.apply_unpublished(root.tree, updates, max_key, cache_bytes)?;
            updates.clear();
            Ok(())
        };
        self.storage
            .reader(&base)
            .tree
            .visit(base.root.tree, &mut |key, value| {
                let charge = key.len().saturating_add(LATEST_LEN + 128);
                if !updates.is_empty()
                    && (updates.len() >= self.options.batch.max_records
                        || bytes.saturating_add(charge) > self.options.batch.max_bytes)
                {
                    flush(&mut root, &mut updates)?;
                    bytes = 0;
                }
                bytes = bytes.saturating_add(charge);
                updates.insert(key, value);
                Ok(())
            })?;
        flush(&mut root, &mut updates)?;
        if base.root.slots.offset != 0 {
            let reader = self.storage.reader(&base);
            let layout = slots::Working::layout(&reader.slots)?;
            let mut working = slots::Working::load(
                &reader.slots,
                &layout,
                self.options.batch.max_bytes.max(self.options.cache_bytes),
            )?;
            working.compact();
            let file = replacement.writer()?.try_clone()?;
            root.slots =
                working.publish(&file, true, replacement.alloc()?, slots::Root::default())?;
        }
        if root.count() != base.root.count() {
            return Err(DiskIndexError::CheckpointMismatch(
                "compaction key count changed",
            ));
        }
        replacement.publish(state.metadata, &state.tails, root, Publication::Confirmed)?;
        let after_bytes = replacement.file.metadata()?.len();
        // Construct all fallible local state before publishing the replacement.
        let identity = opened_sidecar_identity(&replacement.file)?;
        replacement.path = crate::TryClone::try_clone(&target).map_err(DiskIndexError::Primary)?;
        let new_slots = if !self.finite_layout.is_empty() {
            let checkpoint = replacement.current()?;
            Some(slots::Working::load(
                &replacement.reader(&checkpoint).slots,
                &self.finite_layout,
                self.options.batch.max_bytes.max(self.options.cache_bytes),
            )?)
        } else {
            None
        };
        let publication = crate::file::publish_open_temp_path_atomically(temp, &target)
            .map_err(DiskIndexError::Primary)?;
        self.slots_cache = new_slots;
        self.storage = replacement;
        self.identity = identity;
        match publication {
            crate::file::ReplaceDurability::Durable => {}
            crate::file::ReplaceDurability::ParentSyncPending(source) => {
                return Err(DiskIndexError::Primary(crate::fallible::boxed_path_error(
                    source,
                    &target,
                    Error::ErrorContextAllocationFailed {
                        context: crate::AllocationFailureContext::ParentSyncPending,
                    },
                    |path, source| Error::PublishedButParentSyncPending { path, source },
                )));
            }
        }
        Ok(IndexCompaction {
            before_bytes,
            after_bytes,
            historical_distinct_keys: root.count(),
        })
    }
    pub fn begin_generation(&mut self) -> DiskIndexResult<DiskIndexMetadata> {
        self.storage.writer()?;
        let clean = self.storage.current()?;
        if clean.metadata.state != DiskIndexState::Clean {
            return Err(DiskIndexError::CleanStateRequired);
        }
        let state = self.storage.state(&clean, None)?;
        let mut dirty = clean.metadata;
        dirty.tail_limit = self.write_tail_limit.unwrap_or(dirty.tail_limit);
        dirty.state = DiskIndexState::Dirty;
        dirty.checkpoint = Some(DiskIndexCheckpoint {
            checkpoint_offset: clean.offset,
            base: dirty.committed,
        });
        crate::scalable_fault_point("generation.commit");
        let result = self
            .storage
            .publish(dirty, &state.tails, clean.root, Publication::Dirty);
        crate::scalable_fault_point("generation.commit");
        result?;
        Ok(dirty)
    }
    pub(crate) fn begin_write_batch(&mut self) -> DiskIndexResult<&mut DiskIndexWriteBatch> {
        // The store owns the only staged batch. It never lends a writable file
        // capability to that batch; publication requires exclusive store access.
        if self.batch.is_none() {
            if self.read_metadata()?.state == DiskIndexState::Clean {
                self.begin_generation()?;
            }
            self.storage.writer()?;
            let base = self.storage.current()?;
            self.validate_checkpoint(&base)?;
            let metadata = base.metadata;
            if metadata.state != DiskIndexState::Dirty {
                return Err(DiskIndexError::DirtyStateRequired);
            }
            let tails = match take_tail_cache(&mut self.tail_cache, metadata) {
                Some(tails) => tails,
                None => self
                    .storage
                    .state(&base, None)?
                    .tails
                    .into_iter()
                    .map(|t| (t.block_id, t))
                    .collect(),
            };
            self.batch = Some(DiskIndexWriteBatch {
                base,
                metadata,
                tails,
                changed_tails: BTreeSet::new(),
                pending_latest: BTreeMap::new(),
                slots: if self.finite_layout.is_empty() {
                    None
                } else {
                    Some(
                        self.slots_cache
                            .take()
                            .ok_or(DiskIndexError::CheckpointMismatch(
                                "finite slot working state is unavailable",
                            ))?,
                    )
                },
                options: self.options,
                records: 0,
                bytes: 0,
            });
        }
        Ok(self.batch.as_mut().expect("batch initialized"))
    }
    pub(crate) fn commit_batch(&mut self) -> DiskIndexResult<()> {
        if let Some(batch) = self.batch.take() {
            batch.commit(self)?;
        }
        Ok(())
    }
    pub(crate) fn publish_clean(
        &mut self,
        expected_working: DiskIndexFrontier,
    ) -> DiskIndexResult<DiskIndexMetadata> {
        self.storage.writer()?;
        let current = self.storage.current()?;
        if current.metadata.state != DiskIndexState::Dirty {
            return Err(DiskIndexError::DirtyStateRequired);
        }
        if current.metadata.working != expected_working {
            return Err(DiskIndexError::CheckpointMismatch(
                "working frontier changed before clean publication",
            ));
        }
        self.validate_checkpoint(&current)?;
        let state = self.storage.state(&current, None)?;
        let mut clean = current.metadata;
        clean.generation = clean
            .generation
            .checked_add(1)
            .ok_or(DiskIndexError::GenerationExhausted)?;
        clean.state = DiskIndexState::Clean;
        clean.committed = clean.working;
        clean.committed_tail_count = clean.working_tail_count;
        clean.committed_tail_digest = clean.working_tail_digest;
        clean.checkpoint = None;
        let mut root = current.root;
        if !self.finite_layout.is_empty() {
            let mut working = self
                .slots_cache
                .take()
                .ok_or(DiskIndexError::CheckpointMismatch(
                    "finite slot working state is unavailable",
                ))?;
            let file = self.storage.writer()?.try_clone()?;
            let previous = current.root.slots;
            let published = working.publish(&file, false, self.storage.alloc()?, previous);
            self.slots_cache = Some(working);
            root.slots = published?;
        }
        crate::scalable_fault_point("publish.clean_commit");
        let result = self
            .storage
            .publish(clean, &state.tails, root, Publication::Confirmed);
        crate::scalable_fault_point("publish.clean_commit");
        result?;
        Ok(clean)
    }

    pub(crate) fn stage_restore_for_spec(
        &mut self,
        expected_identity: DiskIndexIdentity,
        expected_mode: DiskIndexMode,
        physical_native_eof: u64,
        spec: FormatSpec,
    ) -> DiskIndexResult<DiskIndexRestoreGuard<'_>> {
        self.stage_restore_bounded(
            expected_identity,
            expected_mode,
            physical_native_eof,
            TailLimitPolicy::for_spec(spec)?,
        )
    }

    fn stage_restore_bounded(
        &mut self,
        expected_identity: DiskIndexIdentity,
        expected_mode: DiskIndexMode,
        physical_native_eof: u64,
        policy: TailLimitPolicy,
    ) -> DiskIndexResult<DiskIndexRestoreGuard<'_>> {
        self.storage.writer()?;
        let mut dirty = self.storage.current()?;
        validate_identity_mode(dirty.metadata, expected_identity, expected_mode)?;
        policy.validate(dirty.metadata)?;
        if dirty.metadata.state == DiskIndexState::Clean {
            // An interrupted generation whose dirty checkpoint did not reach
            // storage: the newest clean checkpoint is its own rollback point,
            // and only a primary longer than it has anything to restore.
            if physical_native_eof <= dirty.metadata.committed.eof {
                return Err(DiskIndexError::DirtyStateRequired);
            }
            dirty.metadata.state = DiskIndexState::Dirty;
            dirty.metadata.checkpoint = Some(DiskIndexCheckpoint {
                checkpoint_offset: dirty.offset,
                base: dirty.metadata.committed,
            });
        }
        if dirty.metadata.state != DiskIndexState::Dirty {
            return Err(DiskIndexError::DirtyStateRequired);
        }
        let checkpoint = dirty
            .metadata
            .checkpoint
            .ok_or(DiskIndexError::CheckpointMismatch(
                "missing rollback checkpoint",
            ))?;
        if physical_native_eof < checkpoint.base.eof {
            return Err(DiskIndexError::NativeTooShort {
                required: checkpoint.base.eof,
                actual: physical_native_eof,
            });
        }
        crate::scalable_fault_point("restore.stage");
        let base = self.storage.at(checkpoint.checkpoint_offset)?;
        let base_limit = policy.validate(base.metadata)?;
        let staged = self.storage.state(&base, Some(base_limit))?;
        validate_restored_root(&dirty.metadata, &staged)?;
        crate::scalable_fault_point("restore.stage");
        self.write_tail_limit = Some(policy.current);
        Ok(DiskIndexRestoreGuard {
            storage: &mut self.storage,
            root: base.root,
            staged,
        })
    }
    fn initialize(
        &mut self,
        mut metadata: DiskIndexMetadata,
        tails: &[DiskIndexTail],
    ) -> DiskIndexResult<()> {
        if metadata.state != DiskIndexState::Clean || metadata.checkpoint.is_some() {
            return Err(DiskIndexError::CleanStateRequired);
        }
        self.storage.writer()?;
        let tails = canonical_tail_map(tails, metadata.tail_limit, metadata.committed)?;
        let digest = tail_digest(tails.values().copied());
        // canonical_tail_map refused more than `tail_limit`, a u32.
        let tail_count = crate::width::field32(tails.len())?;
        metadata.committed_tail_count = tail_count;
        metadata.working_tail_count = tail_count;
        metadata.committed_tail_digest = digest;
        metadata.working_tail_digest = digest;
        let rows: Vec<_> = tails.values().copied().collect();
        self.storage.publish(
            metadata,
            &rows,
            storage::Root::default(),
            Publication::Confirmed,
        )?;
        self.tail_cache = Some(TailCache { digest, tails });
        Ok(())
    }
}

pub(crate) struct DiskIndexWriteBatch {
    base: IndexCheckpoint,
    metadata: DiskIndexMetadata,
    tails: BTreeMap<u32, DiskIndexTail>,
    changed_tails: BTreeSet<u32>,
    /// Latest-table rows staged by this batch. Buffering the encoded rows in
    /// memory keeps the per-record hot path free of page reads: commit
    /// opens `LATEST_TABLE` once and drains the map. Memory stays bounded by
    /// the batch byte budget, whose per-item accounting covers each staged
    /// key and encoded value.
    pending_latest: BTreeMap<Vec<u8>, [u8; LATEST_LEN]>,
    slots: Option<slots::Working>,
    options: DiskIndexOptions,
    records: usize,
    bytes: usize,
}

impl DiskIndexWriteBatch {
    pub(crate) fn advance_coverage_with_tail(
        &mut self,
        expected_covered_eof: u64,
        new_covered_eof: u64,
        sequence: u64,
        tail: Option<DiskIndexTail>,
    ) -> DiskIndexResult<DiskIndexMetadata> {
        let next = self.next_frontier(expected_covered_eof, new_covered_eof, sequence)?;
        if let Some(tail) = tail {
            validate_tail(tail, next)?;
            self.validate_tail_capacity(tail.block_id)?;
        }
        let item_bytes = checked_item_bytes(0, tail.is_some())?;
        self.ensure_batch_capacity(item_bytes)?;
        self.metadata.working = next;
        if let Some(tail) = tail {
            self.tails.insert(tail.block_id, tail);
            self.changed_tails.insert(tail.block_id);
        }
        self.records += 1;
        self.bytes += item_bytes;
        Ok(self.metadata)
    }

    /// Re-stamps the primary generation witness for this batch.
    ///
    /// Only called while the witness window is still growing — once it covers
    /// [`PRIMARY_GENERATION_WINDOW`] bytes it is frozen for the file's life, so
    /// steady-state appends do no witness work at all.
    pub(crate) fn set_primary_generation(
        &mut self,
        generation: DiskIndexPrimaryGeneration,
    ) -> DiskIndexResult<()> {
        self.metadata.primary_generation = generation;
        Ok(())
    }

    pub(crate) const fn primary_generation(&self) -> DiskIndexPrimaryGeneration {
        self.metadata.primary_generation
    }

    pub(crate) const fn working_frontier(&self) -> DiskIndexFrontier {
        self.metadata.working
    }

    fn commit(mut self, store: &mut DiskIndexStore) -> DiskIndexResult<()> {
        if self.records == 0 && self.changed_tails.is_empty() {
            debug_assert!(self.pending_latest.is_empty());
            // Nothing was staged: the map is still exactly the committed
            // collection, so hand it back for the next batch.
            let digest = self.metadata.working_tail_digest;
            let tails = std::mem::take(&mut self.tails);
            store.tail_cache = Some(TailCache { digest, tails });
            store.slots_cache = self.slots;
            return Ok(());
        }

        let tail_count =
            u32::try_from(self.tails.len()).map_err(|_| DiskIndexError::TailLimitExceeded {
                actual: self.tails.len() as u64,
                limit: self.metadata.tail_limit,
            })?;
        let digest = if self.changed_tails.is_empty() {
            self.metadata.working_tail_digest
        } else {
            tail_digest(self.tails.values().copied())
        };
        self.metadata.working_tail_count = tail_count;
        self.metadata.working_tail_digest = digest;
        validate_metadata(self.metadata)?;
        // Only tails staged by this batch need revalidation against the final
        // frontier: every unchanged tail was validated when its map was read
        // (or previously committed), against a frontier this batch has only
        // advanced monotonically.
        for block_id in &self.changed_tails {
            let tail = self
                .tails
                .get(block_id)
                .ok_or(DiskIndexError::InvalidTail("changed tail disappeared"))?;
            validate_tail(*tail, self.metadata.working)?;
        }

        let root = store.storage.apply(&self.base, &self.pending_latest)?;
        let tails: Vec<_> = self.tails.values().copied().collect();
        crate::scalable_fault_point("append.sidecar_batch_commit");
        let commit = store
            .storage
            .publish(self.metadata, &tails, root, Publication::Working);
        crate::scalable_fault_point("append.sidecar_batch_commit");
        commit?;
        store.slots_cache = self.slots;
        store.tail_cache = Some(TailCache {
            digest,
            tails: self.tails,
        });
        Ok(())
    }

    fn next_frontier(
        &self,
        expected_eof: u64,
        new_eof: u64,
        sequence: u64,
    ) -> DiskIndexResult<DiskIndexFrontier> {
        if self.metadata.working.eof != expected_eof {
            return Err(DiskIndexError::CoverageMismatch {
                expected: expected_eof,
                actual: self.metadata.working.eof,
            });
        }
        if new_eof <= expected_eof {
            return Err(DiskIndexError::MetadataInvariant(
                "working EOF must advance for every record",
            ));
        }
        if self.metadata.working.next_sequence != Some(sequence) {
            return Err(DiskIndexError::SequenceMismatch {
                expected: self.metadata.working.next_sequence,
                actual: sequence,
            });
        }
        let record_count = self
            .metadata
            .working
            .record_count
            .checked_add(1)
            .ok_or(DiskIndexError::RecordCountExhausted)?;
        Ok(DiskIndexFrontier {
            eof: new_eof,
            record_count,
            next_sequence: sequence.checked_add(1),
        })
    }

    fn validate_tail_capacity(&self, block_id: u32) -> DiskIndexResult<()> {
        if self.tails.contains_key(&block_id) {
            return Ok(());
        }
        let actual = self.tails.len().saturating_add(1) as u64;
        if actual > u64::from(self.metadata.tail_limit) {
            return Err(DiskIndexError::TailLimitExceeded {
                actual,
                limit: self.metadata.tail_limit,
            });
        }
        Ok(())
    }

    fn ensure_batch_capacity(&self, item_bytes: usize) -> DiskIndexResult<()> {
        let records = self.records.saturating_add(1);
        let bytes = self.bytes.saturating_add(item_bytes);
        if records > self.options.batch.max_records || bytes > self.options.batch.max_bytes {
            return Err(DiskIndexError::BatchFull {
                records,
                bytes,
                max_records: self.options.batch.max_records,
                max_bytes: self.options.batch.max_bytes,
            });
        }
        Ok(())
    }
}

pub struct DiskIndexRestoreGuard<'a> {
    storage: &'a mut IndexStorage,
    root: storage::Root,
    staged: DiskIndexPersistentState,
}
impl DiskIndexRestoreGuard<'_> {
    pub const fn base_eof(&self) -> u64 {
        self.staged.metadata.committed.eof
    }
    pub const fn frontier(&self) -> DiskIndexFrontier {
        self.staged.metadata.committed
    }
    pub fn tails(&self) -> &[DiskIndexTail] {
        &self.staged.tails
    }
    pub const fn primary_generation(&self) -> DiskIndexPrimaryGeneration {
        self.staged.metadata.primary_generation
    }

    /// Publishes the restored base once the primary has been cut back and
    /// synced to `expected_native_eof`: the base EOF, or the end of the
    /// verified sync-marker prefix after it that the caller will replay.
    pub(crate) fn commit_after_native_sync_at(
        self,
        observed_native_eof: u64,
        expected_native_eof: u64,
    ) -> DiskIndexResult<DiskIndexMetadata> {
        if expected_native_eof < self.base_eof() || observed_native_eof != expected_native_eof {
            return Err(DiskIndexError::NativeLengthMismatch {
                expected: expected_native_eof,
                actual: observed_native_eof,
            });
        }
        crate::scalable_fault_point("restore.commit");
        let result = self.storage.publish(
            self.staged.metadata,
            &self.staged.tails,
            self.root,
            Publication::Confirmed,
        );
        crate::scalable_fault_point("restore.commit");
        result?;
        Ok(self.staged.metadata)
    }
}

pub struct DiskIndexSnapshot {
    pin: Option<SnapshotPin>,
    metadata: DiskIndexMetadata,
}

impl DiskIndexSnapshot {
    pub(crate) fn is_pinned(&self) -> bool {
        self.pin.is_some()
    }

    pub(crate) fn release(&mut self) -> bool {
        self.pin.take().is_some()
    }

    pub(crate) fn status(&self, store: &DiskIndexStore) -> DiskIndexResult<SnapshotStatus> {
        let latest = store.follow_snapshot(self)?;
        Ok(SnapshotStatus {
            generation: self.generation(),
            confirmed_generation: latest.generation(),
            pinned: self.is_pinned(),
            records_behind: latest.record_count() - self.record_count(),
            bytes_behind: latest.committed_eof() - self.committed_eof(),
        })
    }

    pub(crate) const fn record_count(&self) -> u64 {
        self.metadata.committed.record_count
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.metadata.generation
    }

    pub const fn committed_eof(&self) -> u64 {
        self.metadata.committed.eof
    }

    pub const fn primary_generation(&self) -> DiskIndexPrimaryGeneration {
        self.metadata.primary_generation
    }
}

fn validate_identity_mode(
    metadata: DiskIndexMetadata,
    identity: DiskIndexIdentity,
    mode: DiskIndexMode,
) -> DiskIndexResult<()> {
    if metadata.identity != identity {
        return Err(DiskIndexError::IdentityMismatch);
    }
    if metadata.mode != mode {
        return Err(DiskIndexError::ModeMismatch {
            expected: mode,
            actual: metadata.mode,
        });
    }
    Ok(())
}

fn validate_restored_root(
    dirty: &DiskIndexMetadata,
    staged: &DiskIndexPersistentState,
) -> DiskIndexResult<()> {
    validate_checkpoint_metadata(dirty, staged.metadata)
}

fn validate_checkpoint_metadata(
    dirty: &DiskIndexMetadata,
    clean: DiskIndexMetadata,
) -> DiskIndexResult<()> {
    let checkpoint = dirty.checkpoint.ok_or(DiskIndexError::CheckpointMismatch(
        "dirty root has no checkpoint",
    ))?;
    if clean.state != DiskIndexState::Clean {
        return Err(DiskIndexError::CheckpointMismatch(
            "restored root is not clean",
        ));
    }
    if clean.identity != dirty.identity {
        return Err(DiskIndexError::CheckpointMismatch(
            "restored identity differs from dirty root",
        ));
    }
    if clean.mode != dirty.mode {
        return Err(DiskIndexError::CheckpointMismatch(
            "restored mode differs from dirty root",
        ));
    }
    if clean.generation != dirty.generation {
        return Err(DiskIndexError::CheckpointMismatch(
            "restored generation differs from dirty root",
        ));
    }
    if clean.committed != checkpoint.base || clean.working != checkpoint.base {
        return Err(DiskIndexError::CheckpointMismatch(
            "restored frontier differs from checkpoint base",
        ));
    }
    if clean.committed != dirty.committed {
        return Err(DiskIndexError::CheckpointMismatch(
            "dirty committed frontier differs from restored root",
        ));
    }
    if clean.committed_tail_count != dirty.committed_tail_count
        || clean.committed_tail_digest != dirty.committed_tail_digest
    {
        return Err(DiskIndexError::CheckpointMismatch(
            "restored tails differ from committed checkpoint tails",
        ));
    }
    Ok(())
}

fn canonical_sidecar_path(path: &Path) -> DiskIndexResult<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent)?;
    let name = path.file_name().ok_or(DiskIndexError::MetadataInvariant(
        "sidecar path has no file name",
    ))?;
    Ok(parent.join(name))
}

pub fn state_sidecar_path(native_path: impl AsRef<Path>) -> PathBuf {
    append_sidecar_extension(native_path.as_ref(), ".vks")
}

fn append_sidecar_extension(path: &Path, extension: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_owned();
    value.push(extension);
    PathBuf::from(value)
}

fn validate_expected_tail_limit(metadata: DiskIndexMetadata, expected: u32) -> DiskIndexResult<()> {
    if metadata.tail_limit != expected {
        return Err(DiskIndexError::TailLimitMismatch {
            expected,
            actual: metadata.tail_limit,
        });
    }
    Ok(())
}

fn validate_working_tails(
    metadata: DiskIndexMetadata,
    tails: &[DiskIndexTail],
) -> DiskIndexResult<()> {
    let actual_count =
        u32::try_from(tails.len()).map_err(|_| DiskIndexError::TailLimitExceeded {
            actual: tails.len() as u64,
            limit: metadata.tail_limit,
        })?;
    if actual_count != metadata.working_tail_count {
        return Err(DiskIndexError::TailCountMismatch {
            expected: metadata.working_tail_count,
            actual: actual_count,
        });
    }
    if tail_digest(tails.iter().copied()) != metadata.working_tail_digest {
        return Err(DiskIndexError::TailDigestMismatch);
    }
    for tail in tails {
        validate_tail(*tail, metadata.working)?;
    }
    Ok(())
}

fn canonical_tail_map(
    tails: &[DiskIndexTail],
    limit: u32,
    frontier: DiskIndexFrontier,
) -> DiskIndexResult<BTreeMap<u32, DiskIndexTail>> {
    if tails.len() as u64 > u64::from(limit) {
        return Err(DiskIndexError::TailLimitExceeded {
            actual: tails.len() as u64,
            limit,
        });
    }
    let mut map = BTreeMap::new();
    for tail in tails {
        validate_tail(*tail, frontier)?;
        if map.insert(tail.block_id, *tail).is_some() {
            return Err(DiskIndexError::InvalidTail("duplicate block id"));
        }
    }
    Ok(map)
}

fn validate_tail(tail: DiskIndexTail, frontier: DiskIndexFrontier) -> DiskIndexResult<()> {
    if tail.record_offset >= frontier.eof {
        return Err(DiskIndexError::InvalidTail(
            "record offset is outside the working snapshot",
        ));
    }
    if frontier
        .next_sequence
        .is_some_and(|next| tail.sequence >= next)
    {
        return Err(DiskIndexError::InvalidTail(
            "sequence is not before the working frontier",
        ));
    }
    Ok(())
}

fn validate_metadata(metadata: DiskIndexMetadata) -> DiskIndexResult<()> {
    if metadata.primary_generation.len > PRIMARY_GENERATION_WINDOW
        || metadata.primary_generation.len > metadata.working.eof
    {
        return Err(DiskIndexError::MetadataInvariant(
            "primary generation window exceeds its bound",
        ));
    }
    match metadata.mode {
        DiskIndexMode::StateOnly => {}
        DiskIndexMode::DiskPlan(digest) if digest.is_zero() => {
            return Err(DiskIndexError::PlanDigestIsZero);
        }
        DiskIndexMode::DiskPlan(_) => {}
    }
    if metadata.committed.eof > metadata.working.eof {
        return Err(DiskIndexError::MetadataInvariant(
            "working EOF precedes committed EOF",
        ));
    }
    if metadata.committed.record_count > metadata.working.record_count {
        return Err(DiskIndexError::MetadataInvariant(
            "working record count precedes committed count",
        ));
    }
    match (
        metadata.committed.next_sequence,
        metadata.working.next_sequence,
    ) {
        (Some(committed), Some(working)) if working < committed => {
            return Err(DiskIndexError::MetadataInvariant(
                "working sequence precedes committed sequence",
            ));
        }
        (None, Some(_)) => {
            return Err(DiskIndexError::MetadataInvariant(
                "working sequence resumed after exhaustion",
            ));
        }
        _ => {}
    }
    if metadata.committed_tail_count > metadata.tail_limit
        || metadata.working_tail_count > metadata.tail_limit
    {
        return Err(DiskIndexError::TailLimitExceeded {
            actual: u64::from(
                metadata
                    .committed_tail_count
                    .max(metadata.working_tail_count),
            ),
            limit: metadata.tail_limit,
        });
    }
    match metadata.state {
        DiskIndexState::Clean => {
            if metadata.checkpoint.is_some() {
                return Err(DiskIndexError::MetadataInvariant(
                    "clean metadata carries a checkpoint",
                ));
            }
            if metadata.committed != metadata.working {
                return Err(DiskIndexError::MetadataInvariant(
                    "clean committed and working frontiers differ",
                ));
            }
            if metadata.committed_tail_count != metadata.working_tail_count
                || metadata.committed_tail_digest != metadata.working_tail_digest
            {
                return Err(DiskIndexError::MetadataInvariant(
                    "clean committed and working tails differ",
                ));
            }
        }
        DiskIndexState::Dirty => {
            let checkpoint = metadata
                .checkpoint
                .ok_or(DiskIndexError::MetadataInvariant(
                    "dirty metadata has no checkpoint",
                ))?;
            if checkpoint.checkpoint_offset == 0 {
                return Err(DiskIndexError::MetadataInvariant(
                    "dirty checkpoint has checkpoint offset zero",
                ));
            }
            if checkpoint.base != metadata.committed {
                return Err(DiskIndexError::MetadataInvariant(
                    "checkpoint base differs from committed frontier",
                ));
            }
        }
    }
    Ok(())
}

fn checked_item_bytes(key_len: usize, has_tail: bool) -> DiskIndexResult<usize> {
    BATCH_UPDATE_FIXED_BYTES
        .checked_add(key_len)
        .and_then(|value| {
            if has_tail {
                value.checked_add(BATCH_TAIL_BYTES)
            } else {
                Some(value)
            }
        })
        .ok_or(DiskIndexError::MetadataInvariant(
            "batch byte accounting overflow",
        ))
}

pub(crate) const fn batch_item_fixed_bytes(has_tail: bool) -> usize {
    if has_tail {
        BATCH_UPDATE_FIXED_BYTES + BATCH_TAIL_BYTES
    } else {
        BATCH_UPDATE_FIXED_BYTES
    }
}

fn encode_metadata(metadata: DiskIndexMetadata) -> [u8; META_LEN] {
    let mut bytes = [0u8; META_LEN];
    bytes[0..8].copy_from_slice(&META_MAGIC);
    bytes[8..10].copy_from_slice(&META_VERSION.to_le_bytes());
    bytes[10] = match metadata.state {
        DiskIndexState::Dirty => 0,
        DiskIndexState::Clean => 1,
    };
    bytes[11] = match metadata.mode {
        DiskIndexMode::StateOnly => 0,
        DiskIndexMode::DiskPlan(_) => 1,
    };
    let mut sequence_flags = 0u8;
    if metadata.committed.next_sequence.is_none() {
        sequence_flags |= SEQUENCE_COMMITTED_EXHAUSTED;
    }
    if metadata.working.next_sequence.is_none() {
        sequence_flags |= SEQUENCE_WORKING_EXHAUSTED;
    }
    if metadata
        .checkpoint
        .is_some_and(|checkpoint| checkpoint.base.next_sequence.is_none())
    {
        sequence_flags |= SEQUENCE_BASE_EXHAUSTED;
    }
    bytes[12] = sequence_flags;
    bytes[16..24].copy_from_slice(&metadata.identity.schema_hash.to_le_bytes());
    bytes[24..56].copy_from_slice(&metadata.identity.primary_fingerprint);
    if let DiskIndexMode::DiskPlan(digest) = metadata.mode {
        bytes[56..88].copy_from_slice(digest.as_bytes());
    }
    bytes[88..96].copy_from_slice(&metadata.generation.to_le_bytes());
    encode_frontier(&mut bytes[96..120], metadata.committed);
    encode_frontier(&mut bytes[120..144], metadata.working);
    if let Some(checkpoint) = metadata.checkpoint {
        bytes[144..152].copy_from_slice(&checkpoint.checkpoint_offset.to_le_bytes());
        encode_frontier(&mut bytes[152..176], checkpoint.base);
    }
    bytes[176..180].copy_from_slice(&metadata.tail_limit.to_le_bytes());
    bytes[180..184].copy_from_slice(&metadata.committed_tail_count.to_le_bytes());
    bytes[184..188].copy_from_slice(&metadata.working_tail_count.to_le_bytes());
    bytes[192..224].copy_from_slice(metadata.committed_tail_digest.as_bytes());
    bytes[224..256].copy_from_slice(metadata.working_tail_digest.as_bytes());
    bytes[256..264].copy_from_slice(&metadata.primary_generation.len.to_le_bytes());
    bytes[264..296].copy_from_slice(metadata.primary_generation.digest.as_bytes());
    let crc = crc32fast::hash(&bytes[..296]);
    bytes[296..300].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn decode_metadata(bytes: &[u8]) -> DiskIndexResult<DiskIndexMetadata> {
    if bytes.len() != META_LEN {
        return Err(DiskIndexError::MetadataLength {
            actual: bytes.len(),
            expected: META_LEN,
        });
    }
    if bytes[0..8] != META_MAGIC {
        return Err(DiskIndexError::MetadataMagic);
    }
    let version = read_u16(bytes, 8);
    if version != META_VERSION {
        return Err(DiskIndexError::MetadataVersion { actual: version });
    }
    let stored_crc = read_u32(bytes, 296);
    if crc32fast::hash(&bytes[..296]) != stored_crc {
        return Err(DiskIndexError::MetadataChecksum);
    }
    if bytes[13..16] != [0; 3] || bytes[188..192] != [0; 4] {
        return Err(DiskIndexError::MetadataReserved);
    }
    let sequence_flags = bytes[12];
    if sequence_flags & !SEQUENCE_FLAGS != 0 {
        return Err(DiskIndexError::MetadataFlags {
            actual: sequence_flags,
        });
    }
    let state = match bytes[10] {
        0 => DiskIndexState::Dirty,
        1 => DiskIndexState::Clean,
        actual => return Err(DiskIndexError::MetadataState { actual }),
    };
    let plan_digest = DiskIndexDigest(bytes[56..88].try_into().expect("fixed metadata slice"));
    let mode = match bytes[11] {
        0 if plan_digest.is_zero() => DiskIndexMode::StateOnly,
        0 => {
            return Err(DiskIndexError::MetadataInvariant(
                "state-only metadata carries a plan digest",
            ));
        }
        1 if !plan_digest.is_zero() => DiskIndexMode::DiskPlan(plan_digest),
        1 => return Err(DiskIndexError::PlanDigestIsZero),
        actual => return Err(DiskIndexError::MetadataMode { actual }),
    };
    let committed = decode_frontier(
        &bytes[96..120],
        sequence_flags & SEQUENCE_COMMITTED_EXHAUSTED != 0,
    )?;
    let working = decode_frontier(
        &bytes[120..144],
        sequence_flags & SEQUENCE_WORKING_EXHAUSTED != 0,
    )?;
    let checkpoint = match state {
        DiskIndexState::Dirty => Some(DiskIndexCheckpoint {
            checkpoint_offset: read_u64(bytes, 144),
            base: decode_frontier(
                &bytes[152..176],
                sequence_flags & SEQUENCE_BASE_EXHAUSTED != 0,
            )?,
        }),
        DiskIndexState::Clean => {
            if bytes[144..176] != [0; 32] || sequence_flags & SEQUENCE_BASE_EXHAUSTED != 0 {
                return Err(DiskIndexError::MetadataInvariant(
                    "clean metadata carries checkpoint bytes",
                ));
            }
            None
        }
    };
    let metadata = DiskIndexMetadata {
        identity: DiskIndexIdentity {
            schema_hash: read_u64(bytes, 16),
            primary_fingerprint: bytes[24..56].try_into().expect("fixed metadata slice"),
        },
        primary_generation: DiskIndexPrimaryGeneration {
            len: read_u64(bytes, 256),
            digest: DiskIndexDigest(bytes[264..296].try_into().expect("fixed metadata slice")),
        },
        mode,
        state,
        generation: read_u64(bytes, 88),
        committed,
        working,
        checkpoint,
        tail_limit: read_u32(bytes, 176),
        committed_tail_count: read_u32(bytes, 180),
        working_tail_count: read_u32(bytes, 184),
        committed_tail_digest: DiskIndexDigest(
            bytes[192..224].try_into().expect("fixed metadata slice"),
        ),
        working_tail_digest: DiskIndexDigest(
            bytes[224..256].try_into().expect("fixed metadata slice"),
        ),
    };
    validate_metadata(metadata)?;
    Ok(metadata)
}

fn encode_frontier(bytes: &mut [u8], frontier: DiskIndexFrontier) {
    bytes[0..8].copy_from_slice(&frontier.eof.to_le_bytes());
    bytes[8..16].copy_from_slice(&frontier.record_count.to_le_bytes());
    bytes[16..24].copy_from_slice(&frontier.next_sequence.unwrap_or(0).to_le_bytes());
}

fn decode_frontier(bytes: &[u8], exhausted: bool) -> DiskIndexResult<DiskIndexFrontier> {
    let encoded_sequence = read_u64(bytes, 16);
    if exhausted && encoded_sequence != 0 {
        return Err(DiskIndexError::MetadataInvariant(
            "exhausted sequence carries a nonzero value",
        ));
    }
    Ok(DiskIndexFrontier {
        eof: read_u64(bytes, 0),
        record_count: read_u64(bytes, 8),
        next_sequence: (!exhausted).then_some(encoded_sequence),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LatestKind {
    Put,
    Tombstone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LatestValue {
    kind: LatestKind,
    record_offset: u64,
    sequence: u64,
    target_block_id: u32,
    physical: Option<DiskIndexPhysicalRecord>,
}

fn decode_latest(bytes: &[u8]) -> DiskIndexResult<LatestValue> {
    if bytes.len() != LATEST_LEN {
        return Err(DiskIndexError::LatestLength {
            actual: bytes.len(),
            expected: LATEST_LEN,
        });
    }
    if crc32fast::hash(&bytes[..48]) != read_u32(bytes, 48) {
        return Err(DiskIndexError::LatestChecksum);
    }
    if bytes[6..8] != [0; 2] || bytes[44..48] != [0; 4] {
        return Err(DiskIndexError::LatestReserved);
    }
    let flags = bytes[1];
    if flags & !LATEST_FLAGS != 0 {
        return Err(DiskIndexError::LatestInvariant("unknown flags"));
    }
    let kind = match bytes[0] {
        0 => LatestKind::Put,
        1 => LatestKind::Tombstone,
        _ => return Err(DiskIndexError::LatestInvariant("unknown entry kind")),
    };
    let has_physical = flags & LATEST_HAS_PHYSICAL != 0;
    let has_native_crc = flags & LATEST_HAS_NATIVE_CRC != 0;
    if has_native_crc && !has_physical {
        return Err(DiskIndexError::LatestInvariant(
            "native CRC exists without a physical descriptor",
        ));
    }
    let physical = if has_physical {
        let value = DiskIndexPhysicalRecord {
            physical_len: read_u64(bytes, 16),
            block_id: read_u32(bytes, 36),
            block_version: read_u16(bytes, 2),
            flags: read_u16(bytes, 4),
            native_crc: has_native_crc.then(|| read_u32(bytes, 40)),
        };
        if value.physical_len == 0 || value.block_version == 0 {
            return Err(DiskIndexError::LatestInvariant(
                "physical descriptor has zero length or version",
            ));
        }
        Some(value)
    } else {
        if bytes[2..6] != [0; 4] || bytes[16..24] != [0; 8] || bytes[36..44] != [0; 8] {
            return Err(DiskIndexError::LatestReserved);
        }
        None
    };
    Ok(LatestValue {
        kind,
        record_offset: read_u64(bytes, 8),
        sequence: read_u64(bytes, 24),
        target_block_id: read_u32(bytes, 32),
        physical,
    })
}

fn encode_tail(tail: DiskIndexTail) -> [u8; TAIL_LEN] {
    let mut bytes = [0u8; TAIL_LEN];
    bytes[0..4].copy_from_slice(&tail.block_id.to_le_bytes());
    bytes[8..16].copy_from_slice(&tail.record_offset.to_le_bytes());
    bytes[16..24].copy_from_slice(&tail.sequence.to_le_bytes());
    let crc = crc32fast::hash(&bytes[..28]);
    bytes[28..32].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn decode_tail(key: u32, bytes: &[u8]) -> DiskIndexResult<DiskIndexTail> {
    if bytes.len() != TAIL_LEN {
        return Err(DiskIndexError::TailLength {
            actual: bytes.len(),
            expected: TAIL_LEN,
        });
    }
    if crc32fast::hash(&bytes[..28]) != read_u32(bytes, 28) {
        return Err(DiskIndexError::TailChecksum);
    }
    if bytes[4..8] != [0; 4] || bytes[24..28] != [0; 4] {
        return Err(DiskIndexError::TailReserved);
    }
    let encoded = read_u32(bytes, 0);
    if encoded != key {
        return Err(DiskIndexError::TailKeyMismatch { key, encoded });
    }
    Ok(DiskIndexTail {
        block_id: key,
        record_offset: read_u64(bytes, 8),
        sequence: read_u64(bytes, 16),
    })
}

fn tail_digest(tails: impl IntoIterator<Item = DiskIndexTail> + Clone) -> DiskIndexDigest {
    let mut digest = [0u8; 32];
    for lane in 0..8u32 {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(b"varve-disk-index-tails-v1");
        hasher.update(&lane.to_le_bytes());
        for tail in tails.clone() {
            hasher.update(&tail.block_id.to_le_bytes());
            hasher.update(&tail.record_offset.to_le_bytes());
            hasher.update(&tail.sequence.to_le_bytes());
        }
        digest[(lane as usize) * 4..(lane as usize + 1) * 4]
            .copy_from_slice(&hasher.finalize().to_le_bytes());
    }
    DiskIndexDigest(digest)
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("fixed slice"))
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed slice"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("fixed slice"))
}

/// Private temporary index used when an in-memory operation outgrows its
/// buffer. No writer admission, shared cache, publication or durability I/O:
/// this file is disposable scratch owned by a single handle.
pub(crate) struct ScratchIndex {
    file: crate::fallible::Arc<std::fs::File>,
    root: tree::Root,
    reader: tree::Reader,
    cache_bytes: usize,
    end: u64,
    maximum: Option<u64>,
    /// Pages the copy-on-write updates leave behind, reused by the next ones.
    /// Nothing but this handle reads the file, so a page the previous root
    /// referenced is free the moment the new root is in place: the file grows
    /// with the tree, not with the number of updates.
    alloc: tree::PageAlloc,
    /// One per handle sharing this file: clones share it, so a count above
    /// one means another root may still reference a released page.
    sharers: crate::fallible::Arc<()>,
}
impl Clone for ScratchIndex {
    fn clone(&self) -> Self {
        Self {
            file: self.file.clone(),
            root: self.root,
            cache_bytes: self.cache_bytes,
            end: self.end,
            maximum: self.maximum,
            // A clone starts with nothing to reuse; neither side releases a
            // page while the file is shared (see `apply_iter`).
            alloc: tree::PageAlloc::new(self.alloc.building),
            sharers: self.sharers.clone(),
            reader: tree::Reader::new(
                self.file.clone(),
                self.end,
                u32::MAX as usize,
                self.cache_bytes,
            )
            .with_format(tree::PageFormat::Reuse),
        }
    }
}
impl std::fmt::Debug for ScratchIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScratchIndex")
            .field("rows", &self.root.count)
            .finish()
    }
}
impl ScratchIndex {
    pub(crate) fn new(cache_bytes: usize) -> crate::Result<Self> {
        let (file, maximum) = crate::spill_file::SpillFile::new()?.into_parts();
        crate::spill_file::check_extent(maximum, tree::PAGE as u64)?;
        let file = crate::fallible::try_arc(file)?;
        file.set_len(tree::PAGE as u64)?;
        let reader = tree::Reader::new(
            file.clone(),
            tree::PAGE as u64,
            u32::MAX as usize,
            cache_bytes,
        )
        .with_format(tree::PageFormat::Reuse);
        Ok(Self {
            file,
            root: tree::Root::default(),
            reader,
            cache_bytes,
            end: tree::PAGE as u64,
            maximum,
            alloc: tree::PageAlloc::new(1),
            sharers: crate::fallible::try_arc(())?,
        })
    }
    pub(crate) fn get(&self, key: &[u8]) -> crate::Result<Option<[u8; 52]>> {
        self.reader.get(self.root, key).map_err(index_error)
    }
    pub(crate) fn visit(
        &self,
        mut visitor: impl FnMut(Vec<u8>, [u8; 52]) -> crate::Result<()>,
    ) -> crate::Result<()> {
        let mut failure = None;
        let result = self.reader.visit(self.root, &mut |key, value| {
            if let Err(error) = visitor(key, value) {
                failure = Some(error);
                return Err(DiskIndexError::Io(std::io::ErrorKind::Other.into()));
            }
            Ok(())
        });
        if let Some(error) = failure {
            return Err(error);
        }
        result.map_err(index_error)
    }
    pub(crate) fn apply(&mut self, values: &BTreeMap<Vec<u8>, [u8; 52]>) -> crate::Result<()> {
        self.apply_iter(values.iter())
    }
    /// Updates must be unique and in ascending key order.
    pub(crate) fn apply_iter<'a>(
        &mut self,
        values: impl ExactSizeIterator<Item = (&'a Vec<u8>, &'a [u8; 52])>,
    ) -> crate::Result<()> {
        // A refused limited batch must not consume the remaining scratch
        // allowance with unpublished nodes. Preserve all pre-existing clones.
        let prior = self
            .maximum
            .map(|_| self.file.metadata().map(|m| m.len()))
            .transpose()?;
        let building = self.alloc.building;
        let root = match tree::apply_with_limit(
            &self.file,
            &self.reader,
            self.root,
            values,
            self.maximum,
            Some(&mut self.alloc),
        ) {
            Ok(root) => root,
            Err(error) => {
                // The old root still stands: pages it references that this
                // attempt released go back to pending-forever rather than
                // reusable, and nothing they hold was overwritten (pages of an
                // older generation are only ever released, never reused,
                // within one update).
                self.alloc.pending.retain(|&(_, tag)| tag < building);
                if let Some(prior) = prior {
                    self.file.set_len(prior)?;
                }
                return Err(index_error(error));
            }
        };
        // The new root is in place. If this handle is the file's only reader,
        // every page the update released is free now; while a clone shares
        // the file, its root may still reference them, so they stay retired.
        if crate::fallible::Arc::strong_count(&self.sharers) == 1 {
            self.alloc.release(building).map_err(index_error)?;
        } else {
            self.alloc.pending.clear();
        }
        self.alloc.building = building + 1;
        self.end = self.file.metadata()?.len();
        self.reader = tree::Reader::new(
            self.file.clone(),
            self.end,
            u32::MAX as usize,
            self.cache_bytes,
        )
        .with_format(tree::PageFormat::Reuse);
        self.root = root;
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use crate::Result;

    use super::*;

    /// Repeated updates of the same keys must not grow the scratch file: the
    /// pages each copy-on-write update leaves behind are reused by the next.
    #[test]
    fn scratch_updates_reuse_released_pages() -> Result<()> {
        let mut index = ScratchIndex::new(64 * 1024)?;
        let batch = |round: u64| -> BTreeMap<Vec<u8>, [u8; 52]> {
            (0..2_000u64)
                .map(|key| {
                    let mut row = [0; 52];
                    row[..8].copy_from_slice(&round.to_le_bytes());
                    (key.to_be_bytes().to_vec(), row)
                })
                .collect()
        };
        index.apply(&batch(0))?;
        let after_first = index.file.metadata()?.len();
        for round in 1..200 {
            index.apply(&batch(round))?;
        }
        let after_all = index.file.metadata()?.len();
        assert!(
            after_all <= after_first * 3,
            "200 rewrites grew the scratch file from {after_first} to {after_all} bytes"
        );
        let row = index.get(&7u64.to_be_bytes())?.expect("key 7");
        assert_eq!(u64::from_le_bytes(row[..8].try_into().unwrap()), 199);
        Ok(())
    }

    #[test]
    fn arbitrary_sidecar_codec_bytes_never_panic() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for case in 0..4_096u32 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let len = (state as usize) % (META_LEN + 33);
            let mut bytes = vec![0u8; len];
            for (index, byte) in bytes.iter_mut().enumerate() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = (state as u8) ^ (index as u8) ^ (case as u8);
            }
            let result = std::panic::catch_unwind(|| {
                let _ = decode_metadata(&bytes);
                let _ = decode_latest(&bytes);
                let _ = decode_tail(case, &bytes);
            });
            assert!(result.is_ok(), "sidecar codec panicked for case {case}");
        }
    }
}
