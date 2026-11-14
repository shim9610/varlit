#![forbid(unsafe_op_in_unsafe_fn)]
#![cfg_attr(kani, feature(allocator_api))]

extern crate alloc;

#[cfg(test)]
#[macro_use]
mod test_pages;

mod adapter;
mod anchor;
mod chunks;
mod codec;
mod collections;
mod compression_buffers;
mod conformance;
mod decode_state;
mod diagnostics;
mod primary_file;

mod disk_index;
mod error;
mod fallible;
mod file;
mod format;
mod immediate;

#[cfg(feature = "experimental-matrix-reuse")]
pub mod experimental_matrix;
mod indexed;
mod layout;
mod matrix;
mod matrix_tiles;
mod merge;
mod native_layout;
#[cfg(test)]
mod pib_probe;
mod scalable_extent;
#[cfg(feature = "scalable-fault-injection")]
#[doc(hidden)]
pub mod scalable_fault;

mod packed_rows;
mod publication_protocol;
mod scan_control;
mod snapshot;
mod spill_file;
mod spill_rows;

mod stream;
mod sync_marker;
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
/// fixtures in `crates/varlit/tests/ui/`.
///
/// This is not API. It is `#[doc(hidden)]`, gated behind the same test-only
/// feature as the fault injectors, and carries no method that can weaken an
/// invariant: the constructors that matter (`PoisonFlag::issue`, which is
/// private even to the rest of `varlit-core`,
/// `ReservedIndexSlot::reserve`, `ReplacementTarget::resolve` and
/// `RecordOverwrite::prepare`) stay crate-private, which is precisely the
/// property the fixtures assert.
#[cfg(feature = "scalable-fault-injection")]
#[doc(hidden)]
pub mod enforcement_probe {
    pub use crate::file::replacement_target::{RecordOverwrite, ReplacementTarget};
    pub use crate::file::resident_index::{ReservedIndexSlot, ResidentIndex};
    pub use crate::writer_permit::{MutationInFlight, MutationPermit, PoisonFlag};

    /// Checked borrow for compile-fail instance-binding proofs, never a raw flag.
    pub fn borrow_stream_writer(
        writer: &mut crate::VarlitStreamWriter,
    ) -> crate::Result<MutationPermit<'_, crate::VarlitStreamWriter>> {
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
    Decoder, Encoder, FieldHeader, VarlitDecode, VarlitEmpty, VarlitEncode, WireType,
    decode_from_slice, decode_from_slice_into, encode_field_in_place, encode_to_vec,
    read_field_header, write_field,
};
pub use collections::{BlockCursor, BlockVec, KeyedBlockVec};
#[doc(hidden)]
pub use conformance::RecordValidator;
pub use conformance::{FileCheck, FileCheckReport, FileCheckStatus, check_file};
pub use diagnostics::{
    Diagnostic, DiagnosticDomain, DiagnosticSeverity, FormatDiagnostics, FormatSelfTest,
    FormatSelfTestReport, SelfTestStepReport, SelfTestStepStatus, classify_error, diagnose_file,
    diagnose_spec, error_hint,
};

pub use anchor::{Anchor, AnchorLayout, AnchorPolicy};
pub use disk_index::{
    DiskIndexBatchOptions, DiskIndexConfirmPolicy, DiskIndexDescriptor as DiskIndexedBlock,
    DiskIndexDigest, DiskIndexEntry, DiskIndexError, DiskIndexKeyTag, DiskIndexMode,
    DiskIndexOptions, DiskIndexPlan, IndexCompaction, SnapshotStatus, VarlitDiskKey,
    sidecar_path as disk_index_sidecar_path,
};
pub use error::{AllocationFailureContext, Error, Result};
pub use fallible::{TryClone, try_box};
pub use fallible::{try_box_uninit, try_box_with};
pub use file::{
    AppendInfo, BlockChain, BlockEvent, COMMIT_BLOCK_ID, CREATION_NONCE_BLOCK_ID, DefragmentReport,
    DirectoryRead, FIELD_INLINE, FieldSpan, FieldWalk, FileMatrixDurabilityBarrier, INDEX_BLOCK_ID,
    KeyedChain, KeyedMergeEstimate, LazyOpenSource, MANIFEST_BLOCK_ID, MATRIX_CHUNK_BLOCK_ID,
    MATRIX_TILE_BLOCK_ID, METADATA_BLOCK_ID, MatrixDurabilityBarrier, MatrixNumeric,
    MatrixSidecarManifest, OP_BLOCK_ID, OPEN_DIGEST_BLOCK_ID, OpenMode, Page, PageCursor,
    RECORD_MUTABLE_FLAG_DEAD, RecordCursor, RecordDirectory, RecordIndexEntry, RecordMap,
    RecordPage, RecoveryReport, ReplacePublicationFailure, ReplacementInfo, SEGMENT_BLOCK_ID,
    SchemaBlockDescriptor, SchemaFieldDescriptor, SchemaManifest, TOMBSTONE_BLOCK_ID, VarlitFile,
    VarlitReader, VarlitView, VarlitWriter, WriterLockBreakPolicy, WriterLockInfo,
    classify_replace_publication_error, clear_stale_writer_lock, compact_keyed_files,
    compact_keyed_files_with_key_limit, estimate_keyed_merge, merge_keyed_files,
    merge_keyed_files_with_key_limit, view_set, view_wire_mismatch,
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
    LivenessPolicy, MATRIX_DEFAULT_TILE_BYTES, MATRIX_MAX_TILE_BYTES, MATRIX_MAX_TILE_CELLS,
    ManifestPolicy, MatrixAuxDescriptor, MatrixBlockDescriptor, MatrixCommitDescriptor,
    MatrixCommitKind, MatrixDimensionDescriptor, MatrixMetadataResidency,
    MatrixMetadataVerification, MetadataDescriptor, RawRegionDescriptor, ReadLimit, ReadLimits,
    ReadValidation, RecoveryPolicy, ResourceLimits, SegmentDescriptor, SegmentRepeat,
    TransactionMarkerMode, VariableCompression, matrix_default_tile,
};
pub use immediate::{ImmediateEvent, ImmediateOperation, ImmediatePolicy};
pub use sync_marker::SYNC_BLOCK_ID;

pub use indexed::{
    DiskIndexRebuildReport, VarlitIndexedReader, VarlitIndexedWriter, rebuild_disk_index,
    rebuild_disk_index_with_progress,
};
pub use layout::{
    LayoutFieldValue, LayoutFileInfo, LayoutReader, LayoutScanReport, LayoutSegmentInfo,
    LayoutTailInfo, LayoutTailKind, LayoutValue, LayoutWriter, SegmentWrite, SegmentWriteStream,
};
pub use matrix::{
    MatrixCellStatus, MatrixCommitEvent, MatrixCorruptionKind, MatrixCorruptionSeverity,
    MatrixCursor, MatrixDimensionValue, MatrixDimensions, MatrixKey, MatrixPage,
    MatrixRecoveryAction, MatrixRecoveryFinding, MatrixRecoveryReport, MatrixRegion,
    MatrixResumeSignal, MatrixStorageDiagnostics, PackedBitmap,
};
pub use merge::{
    MergeAction, SequencedMergeAction, VarlitMerge, compact_keyed_file,
    compact_keyed_file_with_key_limit,
};

pub use scan_control::{
    ScanCancellationToken, ScanOptions, ScanProgress, ScanProgressOptions, ScanProgressPhase,
};
#[allow(unused_imports)]
pub(crate) use snapshot::SnapshotFile;

pub use stream::{
    BatchAppendError, BatchAppendInfo, BatchOptions, StreamBootstrapReport, StreamEvents,
    StreamOptions, StreamResidentState, StreamingBlocks, VarlitStreamReader, VarlitStreamWriter,
    bootstrap_stream_checkpoint, bootstrap_stream_checkpoint_with_progress,
};
pub use traits::{
    VarlitBlock, VarlitKey, VarlitKeyedBlock, VarlitMatrixBlock, VarlitMigration,
    VarlitReplaceBlock,
};
#[cfg(feature = "zero-copy")]
pub use traits::{VarlitRawFixedBlock, VarlitRawMatrixBlock};
#[cfg(feature = "zero-copy")]
pub use zerocopy;

#[inline(always)]
pub(crate) fn scalable_fault_point(point: &'static str) {
    #[cfg(feature = "scalable-fault-injection")]
    scalable_fault::fault_point(point);
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = point;
}

#[doc(hidden)]
pub use codec::BuiltinCodec;
#[doc(hidden)]
pub use codec::{BulkCodec, BulkField};
#[doc(hidden)]
pub use file::{FieldsCodec, RecordField, RecordFields};
