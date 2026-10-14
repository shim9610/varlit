#![forbid(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod adapter;
mod chunks;
mod codec;
mod collections;
mod diagnostics;

mod disk_index;
mod error;
mod file;
mod format;
mod immediate;

#[cfg(feature = "experimental-matrix-reuse")]
pub mod experimental_matrix;
mod indexed;
mod layout;
mod matrix;
mod matrix_generation;
mod merge;
mod native_layout;
#[cfg(test)]
mod pib_probe;
mod scalable_extent;
#[cfg(feature = "scalable-fault-injection")]
#[doc(hidden)]
pub mod scalable_fault;

mod scan_control;
mod snapshot;
mod spill_rows;

mod stream;
mod traits;
mod writer_permit;

/// Test-only window onto the crate's mechanical-enforcement types.
///
/// Round 12's mandate was that the two recurring defect shapes be made
/// impossible to express rather than enumerated again, and that the enforcement
/// be *proved* to bind. A proof that the compiler rejects the mistake has to
/// live in a crate that is not this one, because a compile-fail test cannot be
/// written inside the crate under test — so the enforcement types are exposed
/// here, unchanged and by re-export rather than by copy, for the `trybuild`
/// fixtures in `crates/stratava/tests/ui/`.
///
/// This is not API. It is `#[doc(hidden)]`, gated behind the same test-only
/// feature as the fault injectors, and carries no method that can weaken an
/// invariant: the constructors that matter (`PoisonFlag::issue`, which is
/// private even to the rest of `stratava-core`,
/// `ReservedIndexSlot::reserve`, `ReplacementTarget::resolve` and
/// `RecordOverwrite::prepare`) stay crate-private, which is precisely the
/// property the fixtures assert.
#[cfg(feature = "scalable-fault-injection")]
#[doc(hidden)]
pub mod enforcement_probe {
    pub use crate::file::replacement_target::{RecordOverwrite, ReplacementTarget};
    pub use crate::file::resident_index::{ReservedIndexSlot, ResidentIndex};
    pub use crate::matrix::crc_valid_evidence::{CompleteCrcValidEvidence, CrcValidEvidence};
    pub use crate::matrix::fatal_access::{FatalAccessAllowed, FatalAccessGate};
    pub use crate::writer_permit::{MutationInFlight, MutationPermit, PoisonFlag};

    /// Checked borrow for compile-fail instance-binding proofs, never a raw flag.
    pub fn borrow_stream_writer(
        writer: &mut crate::StratavaStreamWriter,
    ) -> crate::Result<MutationPermit<'_, crate::StratavaStreamWriter>> {
        writer.mutation_permit("instance binding probe")
    }
}

pub use adapter::{
    AdapterCheckReport, AdapterCheckStatus, AdapterDiagnostic, AdapterDiagnosticDomain,
    AdapterInputFile, AdapterTailStatus, BinaryCursor, BinaryWriter, ChunkEntry, ChunkIndex,
    ChunkIndexBuilder, ChunkIndexEntry, ChunkLayout, DEFAULT_SIDECAR_FINGERPRINT_SCAN_LIMIT,
    LengthPrefix, SegmentReducer, SegmentReductionReport, SidecarIdentity, SidecarMode,
    SidecarPolicy, SidecarReport, TaggedValue, TaggedValueCodec, reduce_segments,
    reduce_segments_by_ref,
};
pub use chunks::ChunkedBytes;
pub use codec::{
    Decoder, Encoder, FieldHeader, StratavaDecode, StratavaEncode, WireType, decode_from_slice,
    encode_to_vec, read_field_header, write_field,
};
pub use collections::{BlockIter, BlockVec, KeyedBlockVec};
pub use diagnostics::{
    Diagnostic, DiagnosticDomain, DiagnosticSeverity, FormatDiagnostics, FormatSelfTest,
    FormatSelfTestReport, SelfTestStepReport, SelfTestStepStatus, classify_error, diagnose_file,
    diagnose_spec, error_hint,
};

pub use disk_index::{
    DiskIndexBatchOptions, DiskIndexDescriptor as DiskIndexedBlock, DiskIndexDigest,
    DiskIndexEntry, DiskIndexError, DiskIndexKeyTag, DiskIndexMode, DiskIndexOptions,
    DiskIndexPlan, IndexCompaction, SnapshotStatus, StratavaDiskKey,
    sidecar_path as disk_index_sidecar_path,
};
pub use error::{Error, Result};
pub use file::{
    AppendInfo, BlockChain, BlockEvent, COMMIT_BLOCK_ID, CREATION_NONCE_BLOCK_ID, DefragmentReport,
    DirectoryRead, FileMatrixDurabilityBarrier, INDEX_BLOCK_ID, IndexEntries, KeyedChain,
    KeyedMergeEstimate, LazyOpenSource, MANIFEST_BLOCK_ID, MATRIX_CHUNK_BLOCK_ID,
    METADATA_BLOCK_ID, MatrixDurabilityBarrier, MatrixNumeric, MatrixSidecarManifest, OP_BLOCK_ID,
    OPEN_DIGEST_BLOCK_ID, OpenMode, RECORD_MUTABLE_FLAG_DEAD, RecordDirectory, RecordIndexEntry,
    RecordMap, RecoveryReport, ReplacePublicationFailure, ReplacementInfo, SEGMENT_BLOCK_ID,
    SchemaBlockDescriptor, SchemaFieldDescriptor, SchemaManifest, StratavaFile, StratavaReader,
    StratavaWriter, TOMBSTONE_BLOCK_ID, WriterLockBreakPolicy, WriterLockInfo,
    classify_replace_publication_error, clear_stale_writer_lock, compact_keyed_files,
    compact_keyed_files_with_key_limit, estimate_keyed_merge, merge_keyed_files,
    merge_keyed_files_with_key_limit,
};
#[cfg(feature = "mmap")]
pub use file::{MmapMatrix, MmapPayloadWindow, MmapPayloads};
pub use format::{
    BlockCompressionDescriptor, BlockDescriptor, BlockKind, BlockResidencyDescriptor, CommitPolicy,
    CompressionAlgorithm, CompressionHeaderMode, CompressionLevel, CompressionPolicy, Endian,
    FieldDescriptor, FieldPresence, FileHeaderDescriptor, FooterDescriptor, FormatSpec,
    FormatSpecBuilder, GrowingMatrixDimension, HeaderSlotIntegrity, HeaderSlots, IndexPolicy,
    IntegrityPolicy, IntegrityVerification, LayoutAnchor, LayoutBytesSource, LayoutFieldDescriptor,
    LayoutFieldSource, LayoutFieldType, LayoutFinalize, LayoutPartDescriptor, LayoutPartKind,
    LayoutPlan, LayoutPlanField, LayoutPlanFieldGroup, LayoutPlanFieldSource, LayoutPlanFieldType,
    LayoutPlanLen, LayoutPlanPartDescriptor, LayoutPlanPartKind, LayoutPlanRegion,
    LayoutPlanRegionSource, LayoutPlanSegment, LayoutPreset, LayoutSpec, LeadInDescriptor,
    LivenessPolicy, ManifestPolicy, MatrixAuxDescriptor, MatrixBlockDescriptor,
    MatrixCommitDescriptor, MatrixCommitKind, MatrixDimensionDescriptor, MatrixMetadataResidency,
    MatrixMetadataVerification, MetadataDescriptor, RawRegionDescriptor, ReadLimit, ReadLimits,
    RecoveryPolicy, ResourceLimits, SegmentDescriptor, SegmentRepeat, TransactionMarkerMode,
    VariableCompression,
};
pub use immediate::{ImmediateEvent, ImmediateOperation, ImmediatePolicy};

pub use indexed::{
    DiskIndexRebuildReport, StratavaIndexedReader, StratavaIndexedWriter, rebuild_disk_index,
    rebuild_disk_index_with_progress,
};
pub use layout::{
    LayoutFieldValue, LayoutFileInfo, LayoutReader, LayoutScanReport, LayoutSegmentInfo,
    LayoutTailInfo, LayoutTailKind, LayoutValue, LayoutWriter, SegmentWrite, SegmentWriteStream,
};
pub use matrix::{
    MatrixCellStatus, MatrixCommitEvent, MatrixCorruptionKind, MatrixCorruptionSeverity,
    MatrixDimensionValue, MatrixDimensions, MatrixKey, MatrixRecoveryAction, MatrixRecoveryFinding,
    MatrixRecoveryReport, MatrixResumeSignal, MatrixStorageDiagnostics, PackedBitmap,
};
pub use merge::{
    MergeAction, SequencedMergeAction, StratavaMerge, compact_keyed_file,
    compact_keyed_file_with_key_limit,
};

pub use scan_control::{
    ScanCancellationToken, ScanOptions, ScanProgress, ScanProgressOptions, ScanProgressPhase,
};
#[allow(unused_imports)]
pub(crate) use snapshot::SnapshotFile;

pub use stream::{
    BatchAppendError, BatchAppendInfo, BatchOptions, StratavaStreamReader, StratavaStreamWriter,
    StreamBootstrapReport, StreamEvents, StreamOptions, StreamResidentState, StreamingBlocks,
    bootstrap_stream_checkpoint, bootstrap_stream_checkpoint_with_progress,
};
pub use traits::{
    StratavaBlock, StratavaKey, StratavaKeyedBlock, StratavaMatrixBlock, StratavaMigration,
    StratavaReplaceBlock,
};
#[cfg(feature = "zero-copy")]
pub use traits::{StratavaRawFixedBlock, StratavaRawMatrixBlock};
#[cfg(feature = "zero-copy")]
pub use zerocopy;

#[inline(always)]
pub(crate) fn scalable_fault_point(point: &'static str) {
    #[cfg(feature = "scalable-fault-injection")]
    scalable_fault::fault_point(point);
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = point;
}
