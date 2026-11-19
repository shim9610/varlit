use std::path::Path;

use crate::{
    Error, MatrixDimensions, Result, VarlitFile, VarlitReader, VarlitWriter, WireType,
    WriterLockBreakPolicy, WriterLockInfo,
};

const RESERVED_BLOCK_ID_START: u32 = 0xFFFF_FF00;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadLimit {
    Missing,
    Finite(u64),
    TrustedUnbounded,
}

impl ReadLimit {
    pub const fn meet(self, other: Self) -> Self {
        match (self, other) {
            (Self::Finite(left), Self::Finite(right)) => {
                Self::Finite(if left < right { left } else { right })
            }
            (Self::Finite(value), _) | (_, Self::Finite(value)) => Self::Finite(value),
            (Self::Missing, _) | (_, Self::Missing) => Self::Missing,
            (Self::TrustedUnbounded, Self::TrustedUnbounded) => Self::TrustedUnbounded,
        }
    }

    pub fn require_finite(self, resource: &'static str) -> Result<u64> {
        match self {
            Self::Finite(value) => Ok(value),
            Self::Missing => Err(Error::MissingResourceLimit { resource }),
            Self::TrustedUnbounded => Err(Error::TrustedUnboundedRequiresExplicitApi { resource }),
        }
    }

    pub const fn trusted_ceiling(self) -> Option<u64> {
        match self {
            Self::Finite(value) => Some(value),
            Self::Missing | Self::TrustedUnbounded => None,
        }
    }

    const fn overlay(self, value: Self) -> Self {
        match value {
            Self::Missing => self,
            value => value,
        }
    }

    const fn tighten(self, value: Self) -> Self {
        match value {
            Self::Missing => self,
            value => self.meet(value),
        }
    }
}

/// When record integrity is verified.
///
/// Separate from [`IntegrityPolicy`], which says *whether* a checksum exists,
/// because the two answer different questions and conflating them is what made
/// open cost scale with the file. This is the same split `matrix.rs` made in
/// 0.5.0 between residency and verification, for the same defect and with the
/// same standing: **a choice about when damage is announced, not about whether
/// bytes are checked.**
///
/// Every typed read already re-verifies the record it returns —
/// `read_payload_snapshot` re-reads the header, compares all seven fields
/// against the index entry, checks the footer, and recomputes the payload CRC.
/// So verifying every record again while scanning at open is redundant for
/// everything a caller actually reads, and it is what makes open read the whole
/// file instead of its framing.
///
/// These scheduling descriptions assume the compatibility default for
/// [`ReadLimits::read_validation`]. An explicit [`ReadValidation`] override
/// independently chooses which optional checks run; with both off, `AtOpen`
/// does not force an otherwise unnecessary verification scan. Recovery and
/// explicit verification still use their strict contracts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IntegrityVerification {
    /// "Nobody declared a policy." Never overwrites a declaration, and resolves
    /// to [`Self::DEFAULT`] in [`ReadLimits::effective_integrity_verification`]
    /// and only there.
    Missing,
    /// Verify every record while scanning at open.
    ///
    /// The behaviour before this policy existed. Costs a full read of every
    /// payload in the file, and detects damage in records the caller never
    /// touches.
    AtOpen,
    /// Do not verify while scanning at open. Verify on the read that returns the
    /// record, which already happens, and on an explicit `verify_all`.
    ///
    /// **This is [`Self::DEFAULT`].** What it gives up is the *announcement*, not
    /// the check: damage in a record is reported when that record is read rather
    /// than when the file is opened, and damage in a record that is never read is
    /// never reported unless `verify_all` is called.
    ///
    /// One thing does not follow this policy, deliberately: a **recovery** scan
    /// verifies unconditionally, because a checksum mismatch is the evidence
    /// `RecoveryPolicy::TruncateTail` truncates on. Losing it there would turn a
    /// recoverable tail into a silently accepted one.
    OnDemand,
}

impl IntegrityVerification {
    /// The policy [`Self::Missing`] resolves to, and the one line that changes it.
    ///
    /// [`Self::OnDemand`], because the standing requirement is that open reads a
    /// header and work is bounded by what is used, and because the read path
    /// already performs the identical check on every record it returns. A caller
    /// that wants damage announced up front declares [`Self::AtOpen`] or calls
    /// `verify_all` — both are one line, and neither is silently lost.
    pub const DEFAULT: Self = Self::OnDemand;

    /// A runtime declaration replaces the format's; a runtime silence does not.
    const fn overlay(self, value: Self) -> Self {
        match value {
            Self::Missing => self,
            value => value,
        }
    }

    /// A declaration wins over the unset state, and only over that, so a
    /// runtime `ReadLimits` cannot silently drop a policy the format declared.
    const fn tighten(self, value: Self) -> Self {
        match self {
            Self::Missing => value,
            declared => declared,
        }
    }
}

/// The unset state is not a policy: [`IntegrityVerification::DEFAULT`] is what
/// resolves it, so it must itself be resolved.
const _: () = assert!(
    !matches!(
        IntegrityVerification::DEFAULT,
        IntegrityVerification::Missing
    ),
    "IntegrityVerification::DEFAULT must be a resolved policy"
);

/// Optional checks on returned data, independent of the checksums stored in
/// the file. Configure with [`ReadLimits::with_read_validation`].
///
/// Bounds, resource limits, decoder validity, finite traversal and committed
/// publication checks always apply. Trusting metadata may return incorrect but
/// well-formed data if the file violates that trust; skipped damage cannot be
/// promised an error. Protocol/publication checksums remain enabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadValidation {
    /// Recompute stored record/cell checksums. Requires a CRC-bearing format.
    pub data_crc: bool,
    /// Additional record/index metadata consistency checks and eager matrix
    /// metadata reporting. Required parsing and publication guards remain.
    pub structure: bool,
}

impl ReadValidation {
    pub const FULL: Self = Self {
        data_crc: true,
        structure: true,
    };
    pub const CRC_ONLY: Self = Self {
        data_crc: true,
        structure: false,
    };
    pub const STRUCTURE_ONLY: Self = Self {
        data_crc: false,
        structure: true,
    };
    /// Trust captured framing; read payload without rereading its frame or CRC.
    /// Does not relax resource limits or authorize unsafe memory accesses.
    pub const TRUSTED: Self = Self {
        data_crc: false,
        structure: false,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadLimits {
    pub max_file_len: ReadLimit,
    pub max_records: ReadLimit,
    pub max_index_bytes: ReadLimit,
    pub max_scan_bytes: ReadLimit,
    pub max_record_payload_len: ReadLimit,
    pub max_logical_payload_len: ReadLimit,
    pub max_materialized_bytes: ReadLimit,
    pub max_segments: ReadLimit,
    pub max_matrix_dimension: ReadLimit,
    pub max_matrix_cells: ReadLimit,
    pub max_matrix_bitmap_bytes: ReadLimit,
    pub max_matrix_crc_bytes: ReadLimit,
    pub max_matrix_metadata_bytes: ReadLimit,
    pub max_matrix_slot_region_len: ReadLimit,
    pub max_sidecar_len: ReadLimit,
    pub max_mmap_len: ReadLimit,
    /// Ceiling on the file-header extension region a reader will accept, and
    /// on the region a format may reserve.
    ///
    /// The region's length field on disk is a `u32`, so an undeclared ceiling
    /// would let an untrusted header name a 4 GiB region and have open allocate
    /// it before a single block is parsed. It is a ceiling on one open's
    /// allocation, not a property of the file, which is why it lives here and
    /// not on `FormatSpec`: it changes no byte and is not hashed.
    ///
    /// Unset resolves to 64 KiB — see
    /// [`Self::effective_max_file_header_extension_len`]. Declare a larger one
    /// (`limits { header_extension: .. }`) when a `header_slots` region needs
    /// more room than that; both the reader's acceptance check and the
    /// `header_slots`/`header_tails` reservation checks read this value.
    pub max_file_header_extension_len: ReadLimit,
    /// Nominal per-block writer tail-cache budget. A full cache spills to a
    /// private temporary paged index before append, without sync/publication.
    /// The in-memory batch is capped at 2 MiB; one encoded key and fixed page
    /// scratch are minimum working storage even with a smaller cache budget.
    /// Allocator overhead and temporary serialization are additional.
    /// Explicit owned-map requests (`key_tail_offsets`) still enforce this
    /// limit on their result instead of spilling a `HashMap` behind the caller.
    pub max_keyed_tail_bytes: ReadLimit,
    /// Declared record-integrity verification policy; see
    /// [`IntegrityVerification`]. It lives on
    /// `ReadLimits` rather than on `FormatSpec`, because it is a property of
    /// one open and not of the file: it changes no byte, so it is neither
    /// hashed nor persisted.
    pub integrity_verification: IntegrityVerification,
    /// Optional read-side override. `None` preserves legacy checking according
    /// to the stored integrity policy. Does not alter file bytes or write CRCs.
    pub read_validation: Option<ReadValidation>,
    // Authorization state, not a secret payload. Keep the boolean name explicit.
    is_trusted_api: bool,
}

macro_rules! read_limit_setters {
    ($(($method:ident, $field:ident)),+ $(,)?) => {
        $(
            pub const fn $method(mut self, value: u64) -> Self {
                self.$field = ReadLimit::Finite(value);
                self
            }
        )+
    };
}

include!("format/read_limits.rs");

impl Default for ReadLimits {
    fn default() -> Self {
        Self::MISSING
    }
}

/// Runtime resource policy for readers and writers.
///
/// `ReadLimits` remains the canonical name for source compatibility.
pub type ResourceLimits = ReadLimits;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadLimitKey {
    /// **Deliberately never constructed outside tests.**
    ///
    /// A file-length ceiling bounded nothing: it refused a whole file for its
    /// size while every allocation the reader actually makes is bounded by
    /// `Records`, `IndexBytes`, `ScanBytes`, `RecordPayloadLen` and
    /// `LogicalPayloadLen` — each of which has a check that consults it. All
    /// twenty-two enforcement sites (twenty-one native, one layout) were
    /// removed, and neither `ensure_native_write_limits` nor
    /// `ensure_layout_open_limits` requires it to be declared any more.
    ///
    /// The variant and `ReadLimits::max_file_len` stay so that
    /// `limits { file_len: .. }` keeps parsing in every format already written.
    /// The value is inert. `no_file_length_ceiling_is_enforced` in
    /// `tests/enforcement_gates.rs` is what keeps it that way.
    #[allow(
        dead_code,
        reason = "kept so existing `file_len:` declarations parse; inert"
    )]
    FileLen,
    Records,
    IndexBytes,
    ScanBytes,
    RecordPayloadLen,
    LogicalPayloadLen,
    MaterializedBytes,
    Segments,
    MatrixDimension,
    MatrixCells,
    MatrixBitmapBytes,
    MatrixCrcBytes,
    MatrixMetadataBytes,
    MatrixSlotRegionLen,
    SidecarLen,
    KeyedTailBytes,
    #[cfg(feature = "mmap")]
    MmapLen,
}

impl ReadLimitKey {
    pub(crate) const fn resource(self) -> &'static str {
        match self {
            Self::FileLen => "file length",
            Self::Records => "record count",
            Self::IndexBytes => "index bytes",
            Self::ScanBytes => "scan bytes",
            Self::RecordPayloadLen => "record payload length",
            Self::LogicalPayloadLen => "logical payload length",
            Self::MaterializedBytes => "materialized bytes",
            Self::Segments => "segment count",
            Self::MatrixDimension => "matrix dimension",
            Self::MatrixCells => "matrix cells",
            Self::MatrixBitmapBytes => "matrix bitmap bytes",
            Self::MatrixCrcBytes => "matrix checksum bytes",
            Self::MatrixMetadataBytes => "matrix metadata bytes",
            Self::MatrixSlotRegionLen => "matrix slot region length",
            Self::SidecarLen => "sidecar length",
            Self::KeyedTailBytes => "keyed tail bytes",
            #[cfg(feature = "mmap")]
            Self::MmapLen => "mmap length",
        }
    }

    const fn value(self, limits: ReadLimits) -> ReadLimit {
        match self {
            Self::FileLen => limits.max_file_len,
            Self::Records => limits.max_records,
            Self::IndexBytes => limits.max_index_bytes,
            Self::ScanBytes => limits.max_scan_bytes,
            Self::RecordPayloadLen => limits.max_record_payload_len,
            Self::LogicalPayloadLen => limits.max_logical_payload_len,
            Self::MaterializedBytes => limits.max_materialized_bytes,
            Self::Segments => limits.max_segments,
            Self::MatrixDimension => limits.max_matrix_dimension,
            Self::MatrixCells => limits.max_matrix_cells,
            Self::MatrixBitmapBytes => limits.max_matrix_bitmap_bytes,
            Self::MatrixCrcBytes => limits.max_matrix_crc_bytes,
            Self::MatrixMetadataBytes => limits.max_matrix_metadata_bytes,
            Self::MatrixSlotRegionLen => limits.max_matrix_slot_region_len,
            Self::SidecarLen => limits.max_sidecar_len,
            Self::KeyedTailBytes => limits.max_keyed_tail_bytes,
            #[cfg(feature = "mmap")]
            Self::MmapLen => limits.max_mmap_len,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    /// The byte order of the machine the code is compiled for.
    ///
    /// A raw zero-copy type whose fields are plain integers or floats holds
    /// them in this order, so it declares `RAW_ENDIAN = Endian::NATIVE`: its
    /// views then open on a host whose order is the file's and are refused
    /// with [`Error::ZeroCopyEndianMismatch`](crate::Error::ZeroCopyEndianMismatch)
    /// on any other, instead of reading swapped values. A type built from
    /// explicit-endian fields (for example `zerocopy::byteorder::U32<LE>`)
    /// holds the same bytes on every host and declares that order instead.
    pub const NATIVE: Self = if cfg!(target_endian = "little") {
        Self::Little
    } else {
        Self::Big
    };

    pub const fn to_byte(self) -> u8 {
        match self {
            Self::Little => 1,
            Self::Big => 2,
        }
    }

    pub const fn from_byte(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Little),
            2 => Some(Self::Big),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Fixed,
    Variable,
    Matrix,
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixDimensionDescriptor {
    pub name: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixCommitKind {
    Cell,
    Single,
    PerChannel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixCommitDescriptor {
    pub name: &'static str,
    pub kind: MatrixCommitKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixBlockDescriptor {
    pub block_id: u32,
    pub dimensions: [&'static str; 2],
    pub category: &'static str,
    pub slot_stride: u64,
    /// `[rows, cols]` of the tiles this block's cells are stored in: the unit
    /// a cell lives in, is allocated with, and is read back with. `[0, 0]`
    /// asks for [`matrix_default_tile`] of the slot stride.
    pub tile: [u64; 2],
}

impl MatrixBlockDescriptor {
    /// The tile shape in effect: the declared one, or the default for the
    /// slot stride when none was declared.
    pub const fn effective_tile(&self) -> [u64; 2] {
        if self.tile[0] == 0 && self.tile[1] == 0 {
            matrix_default_tile(self.slot_stride)
        } else {
            self.tile
        }
    }
}

/// Bytes a tile of a matrix block aims for when its shape is not declared.
pub const MATRIX_DEFAULT_TILE_BYTES: u64 = 64 * 1024;
/// The most cells one tile holds.
pub const MATRIX_MAX_TILE_CELLS: u64 = 1 << 20;
/// The most bytes one tile's record holds.
pub const MATRIX_MAX_TILE_BYTES: u64 = 64 << 20;

/// The tile shape a block of `slot_stride` gets when it declares none: as
/// many cells as fit [`MATRIX_DEFAULT_TILE_BYTES`] (a power of two, at least
/// one), arranged as close to square as powers of two allow, wider than tall.
pub const fn matrix_default_tile(slot_stride: u64) -> [u64; 2] {
    // Each slot carries its checksum.
    let per_cell = slot_stride.saturating_add(4);
    let fit = MATRIX_DEFAULT_TILE_BYTES / if per_cell == 0 { 1 } else { per_cell };
    let cells = if fit <= 1 {
        1
    } else {
        // The largest power of two not above `fit`.
        1u64 << (63 - fit.leading_zeros())
    };
    let log = cells.trailing_zeros();
    let cols = 1u64 << log.div_ceil(2);
    [cells / cols, cols]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixAuxDescriptor {
    pub name: &'static str,
    pub byte_len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrityPolicy {
    None,
    Crc32,
    Crc32WithHeader,
}

/// Whether a record can be marked dead after it was written.
///
/// Every other way a varlit record stops being the answer is either keyed — a
/// tombstone names a block id and a *key*, never an offset — or destructive,
/// where a `replace_*` publishes a whole new generation in which the record
/// simply is not there. A block with no key had no way to say "this record is
/// dead" at all, and that is what a defragmenting rewrite needs to be told.
///
/// # `FooterFlags`: the last word of the record footer, outside the checksum
///
/// The footer already ends with two fields that are written as zero and
/// validated as zero — a `crc32` placeholder that never held a checksum, and a
/// `reserved` word. This policy turns the trailing `reserved` word into a
/// mutable flag word, and takes it **out of the record checksum**, which is
/// what makes it writable after the fact:
///
/// - the record's own checksum stays valid when a flag is set, so nothing has
///   to re-read the payload to recompute a CRC;
/// - `IntegrityPolicy::Crc32WithHeader` is unaffected, because the mutable word
///   is in the footer and that policy covers the *header*;
/// - a reader holding a resident index does not disagree with the disk, because
///   the seven fields that establish a record's identity are all header fields.
///
/// The cost is that the word is unauthenticated: a flipped bit there is not
/// detected by anything. That is the honest trade for a field that must be
/// writable without rewriting the record, and it is why the flags are advisory
/// — a set flag says "the writer marked this dead", never "these bytes are
/// corrupt".
///
/// # What it costs to turn on
///
/// **No bytes.** The word is already in every footer this format writes; only
/// its meaning and its checksum coverage change. But turning it on changes the
/// schema hash, because excluding four bytes from the checksum changes the
/// checksum of every record — an existing file does not open under it.
///
/// Requires a record footer, for the obvious reason that there is nowhere to
/// put the word without one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LivenessPolicy {
    /// The trailing footer word stays reserved, is written as zero, and is
    /// refused if it is not zero. Byte-identical to every file written before
    /// this policy existed.
    None,
    /// The trailing footer word carries mutable record flags and is excluded
    /// from the record checksum.
    FooterFlags,
}

/// What, if anything, checksums the editable header region.
///
/// The region is the one part of a varlit file that is *meant* to change without
/// the file becoming a new generation, which puts it outside every guarantee the
/// append log gets for free: a record is checksummed once and never rewritten,
/// so a torn write lands past the committed end and open truncates it. A header
/// write has no such luxury --- it overwrites the only copy, in place, at a
/// fixed offset. This is the knob that decides what that costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderSlotIntegrity {
    /// No checksum, no verification, editable for the life of the file.
    ///
    /// The region is excluded from every checksum the format computes, which is
    /// the point: a write is one `write_all` and nothing else in the file has to
    /// be recomputed. What you give up is detection --- a torn or rotted region
    /// reads back as whatever the bytes say, and the entry framing is the only
    /// thing that would notice.
    None,
    /// A checksum rewritten on every write and verified on every read.
    ///
    /// Costs one checksum over the region per write and per read; catches a
    /// torn write, which on a fixed-offset in-place rewrite is the failure that
    /// actually happens. The region stays editable for the life of the file.
    Rolling,
    /// A checksum written once, by an explicit seal, after which the region is
    /// frozen.
    ///
    /// Until the seal the region is editable and unverified; the seal computes
    /// the checksum and sets a flag, and from then on every write is refused and
    /// every read verifies. For the shape where a header is assembled during
    /// commissioning and must not move afterwards --- the seal is the moment the
    /// file stops being configurable, and it is recorded in the file rather than
    /// remembered by a caller.
    Sealed,
}

/// A fixed-size region of the file header that user blocks can be written into
/// after the file exists.
///
/// # Why the size is fixed and cannot be otherwise
///
/// Everything in a varlit file after the header is addressed by an absolute
/// offset --- `append_log_start`, every record offset, every chain link, the
/// matrix region. The header's *length* is therefore part of the file's
/// geometry, and a region that could grow would move every one of those. So the
/// capacity is declared once, reserved at create, and never changes; a write
/// that would not fit is refused rather than served by growing.
///
/// That is also why enabling this changes the schema hash: it moves
/// `append_log_start`, so a build that did not know about the region would
/// compute a different one and misread the file. Declaring it means existing
/// files of the old spec do not open, and there is no way to add the region to a
/// file in place --- write a new one.
///
/// # What it holds
///
/// Ordinary declared blocks, named by id in [`Self::blocks`], each stored at
/// most once. Writing the same block again replaces it. They are *header* blocks
/// rather than append-log records: they have no sequence, they are not part of
/// any chain, no commit marker covers them, and `blocks::<T>()` does not return
/// them.
///
/// # Cost
///
/// `capacity` bytes in every file of the format, whether written or not, plus 16
/// bytes of region header and 8 of block framing. One `write_all` per write, of
/// the whole region, at a fixed offset --- no record is appended and the file
/// does not grow. Reads are served from the header bytes open already holds, so
/// they cost no I/O at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderSlots {
    /// Bytes reserved for entries. Zero means the format declares no region and
    /// is byte-identical to one written before this existed.
    pub capacity: u32,
    /// What checksums the region. See [`HeaderSlotIntegrity`].
    pub integrity: HeaderSlotIntegrity,
    /// The block ids allowed in the region. A write of any other block is
    /// refused, so the declaration is what the file's header can ever contain.
    pub blocks: &'static [u32],
}

impl HeaderSlots {
    /// No region. The default, and byte-identical to a format written before
    /// this option existed.
    pub const NONE: Self = Self {
        capacity: 0,
        integrity: HeaderSlotIntegrity::None,
        blocks: &[],
    };

    /// Whether the format reserves a region at all.
    pub const fn is_declared(&self) -> bool {
        self.capacity > 0
    }

    /// Reserves `capacity` bytes for the given block ids.
    pub const fn new(
        capacity: u32,
        integrity: HeaderSlotIntegrity,
        blocks: &'static [u32],
    ) -> Self {
        Self {
            capacity,
            integrity,
            blocks,
        }
    }

    /// Whether `block_id` may be written into the region.
    pub fn permits(&self, block_id: u32) -> bool {
        let mut index = 0;
        while index < self.blocks.len() {
            if self.blocks[index] == block_id {
                return true;
            }
            index += 1;
        }
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexPolicy {
    pub scan_on_open: bool,
    pub checkpoint_on_flush: bool,
    pub block_offset_chain: bool,
    pub keyed_offset_chain: bool,
    /// Whether a commit point appends an internal *segment* record.
    ///
    /// A segment is varlit's own lookup unit, not a unit the declaration names:
    /// one segment covers exactly the records a single commit point added, and
    /// its record's footer chains back to the previous segment through
    /// `prev_same_block_offset`. Open follows that chain backwards from the
    /// last record footer and never reads a data record.
    ///
    /// Deliberately absent from the layout DSL. Blocks are what a user
    /// declares; segment granularity is varlit's decision, so there is nothing
    /// here for a declaration to choose.
    ///
    /// **A recovery open does not take the chain**, and the cost difference is
    /// the whole file: `open_recover_writer` frames every record where
    /// `open_readonly` and `open_writer` frame one per commit point. Measured
    /// on a 20-per-flush file, 200 records then 800: 10 and 40 framed for the
    /// chained opens, 220 and 880 for the recovering one. That is deliberate --
    /// a recovery open exists to look at every record, and the chain names only
    /// the records a commit point closed, so walking it would skip exactly what
    /// recovery is there to find. See `segment_chain_open_is_allowed`, which
    /// also excludes `IntegrityVerification::AtOpen` for the same reason.
    ///
    /// **Combines with [`Self::open_digest_on_flush`]**, and the two are not
    /// alternatives: the chain builds an index and the digest avoids building
    /// one, so they serve different open entry points rather than competing to
    /// answer the same question. What they do interact over is the end of the
    /// file -- a digest is appended after the segment closing the same commit
    /// point, so the newest segment no longer ends at `file_len`, which the
    /// walk checks. `trailing_open_digest` finds the digest and ends the chain
    /// at it; without that the digest would silently switch the chain off, and
    /// silently is the word, because the result is a slower open rather than a
    /// failed one. Contrast [`Self::header_tails`], which *is* refused
    /// alongside the digest: two summary tables answering one question need a
    /// precedence rule, and no safe one exists.
    pub segment_on_flush: bool,
    /// Whether a commit point appends an internal *open digest* record.
    ///
    /// The three facts an open needs that are not in any one record: where the
    /// committed file ends, what the next append's sequence is, and where each
    /// block's newest record sits. Today they are the by-product of walking
    /// every record, which is the only reason an open that keeps no index still
    /// reads the whole file.
    ///
    /// A digest is those three facts written down. It is **constant size in the
    /// record count** — a header plus twelve bytes per distinct block id —
    /// where a segment writes an entry per record it covers. That is the whole
    /// difference between the two, and it is why this one can be on for a file
    /// that gets a billion records.
    ///
    /// It does not build an index and is not a substitute for one:
    /// [`VarlitFile::open_readonly_lazy`] is the open it serves, and the caller
    /// builds whatever index it needs with `record_map`. An open that wants the
    /// index still scans or walks the segment chain.
    ///
    /// **Combines with [`Self::segment_on_flush`]** for exactly that reason:
    /// the two serve different open entry points, so there is no choice to make
    /// between them and no precedence rule to define. `open_readonly_lazy`
    /// reads this digest, `open_readonly` walks the chain, and
    /// [`crate::LazyOpenSource`] reports which route a lazy open actually took.
    ///
    /// The one interaction is positional, and it is handled rather than
    /// forbidden. A digest is appended *after* the segment closing the same
    /// commit point, so with both on the newest segment no longer ends at
    /// `file_len` -- which is precisely what the chain walk verifies.
    /// `trailing_open_digest` frames the digest and ends the chain there, so
    /// the walk still succeeds and the digest still appears in the index it
    /// produces. Without it, declaring the digest would turn the chain off
    /// without any error, which surfaces as an open that got slower rather than
    /// one that failed.
    ///
    /// Refused alongside [`Self::header_tails`], which is the case that really
    /// is a conflict: both are summary tables answering one question, and their
    /// safety properties differ, so "whichever is newer" would sometimes prefer
    /// the weaker one.
    pub open_digest_on_flush: bool,
    /// Whether the file header carries a fixed region holding each block's
    /// newest record offset.
    ///
    /// The same three facts as [`Self::open_digest_on_flush`], written to a
    /// place that cannot move instead of appended as a record. That is the
    /// whole difference, and it is the difference between a resume that works
    /// and one that works most of the time: a digest record is only usable
    /// while it is the file's *last* record, so anything appended after it —
    /// a partial write, a record from a run that then crashed — hides it and
    /// the open falls back to reading every record. The moment a resume is
    /// needed is the moment that is most likely.
    ///
    /// The region is written at create and rewritten at the end of every commit
    /// point, after the commit marker and inside the durability request that
    /// already follows it — so it costs **no extra `fsync`**, and its length
    /// never changes, so it moves no record.
    ///
    /// **A slot has to earn belief, and it does so in two bounded steps.** It
    /// records the offset of the commit marker it was written for. An open
    /// frames that record and requires it to be a commit marker, then walks
    /// forward — at most a segment and an open digest may follow one — and
    /// requires the walk to land exactly on the end of the file. The second
    /// step is the one that matters: a stale table names an older marker that
    /// is still in the file and still checksums, and adopting it would seed the
    /// next append with a `prev_same_block_offset` that skips every record in
    /// between. "Reaches the end of the file" is exactly "nothing was committed
    /// after this". Every tail is then re-framed and its block id checked.
    ///
    /// **It serves a fresh open, not an open handle.** The header is read once,
    /// at open, so a handle held across many commits keeps the table it opened
    /// with. That is what the option is for — a resume — and it is not a live
    /// view.
    ///
    /// **The handle it opens keeps no resident directory.** That is the point —
    /// the memory a resume costs must not grow with the file — but it is a
    /// smaller handle than a scanning open's: `blocks::<T>()` and
    /// `mmap_payloads` refuse with `NoResidentDirectory`, and `record_map` is
    /// the way to walk records. Keyed lookups work: `key_tail_offsets` rebuilds
    /// from the chains, at a cost bounded by the keyed records rather than by
    /// the file.
    ///
    /// **What the two slots buy, precisely.** They alternate, and only one can
    /// pass the walk — the other names an older marker by construction. The
    /// exception is a commit point that appended nothing: both slots then name
    /// the same marker and both are true of the file, so a torn write to one is
    /// answered by the other, with `generation` breaking the tie. Outside that
    /// case a torn slot means the scan.
    ///
    /// **With [`Self::segment_on_flush`]** the commit point appends a segment
    /// after its marker, and the walk steps through it. That is the only
    /// combination where the walk covers more than the marker itself.
    ///
    /// Refused together with [`Self::open_digest_on_flush`]: they are two
    /// answers to the same question with different safety properties, and no
    /// precedence rule between them is safe in both directions. Also refused
    /// for a format declaring matrix blocks (the matrix region sits at a fixed
    /// offset behind the header, which these bytes would move), without a
    /// crc32 integrity policy (only a per-slot checksum separates a torn
    /// in-place rewrite from a good one), and without a `transaction_marker`
    /// commit policy (with no marker there is nothing for a slot to name, and
    /// the region would stay cold for the life of the file).
    ///
    /// **Changes the bytes on disk.** The region lives in the header, so
    /// `append_log_start` and every record offset move, and the schema hash
    /// changes with it — a file written without this option does not open with
    /// it, and there is no way to add the region to an existing file in place.
    ///
    /// Requires [`Self::block_offset_chain`], for the reason
    /// [`Self::open_digest_on_flush`] does: a tail offset is an entry point to
    /// a chain, and without the chain it names a record with nothing behind it.
    pub header_tails: bool,
}

include!("format/index_policy.rs");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionMarkerMode {
    OnFlush,
    Explicit,
    /// [`Self::OnFlush`] with a single-barrier durable commit; see
    /// [`CommitPolicy::single_barrier`].
    OnFlushSingleBarrier,
    /// [`Self::Explicit`] with a single-barrier durable commit; see
    /// [`CommitPolicy::single_barrier`].
    ExplicitSingleBarrier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitPolicy {
    None,
    RecordFooter,
    TransactionMarker(TransactionMarkerMode),
}

impl CommitPolicy {
    pub const fn requires_record_footer(self) -> bool {
        !matches!(self, Self::None)
    }

    pub const fn marker_on_flush(self) -> bool {
        matches!(
            self,
            Self::TransactionMarker(
                TransactionMarkerMode::OnFlush | TransactionMarkerMode::OnFlushSingleBarrier
            )
        )
    }

    /// Whether `commit_durable` makes a transaction durable with one storage
    /// barrier instead of two.
    ///
    /// The ordered commit syncs the transaction's records, then writes the
    /// marker and syncs again, so a durable marker always follows durable
    /// records. A single-barrier commit writes records and marker and syncs
    /// once; storage may then persist the marker without some of the records
    /// before it. The marker therefore names the records it commits (where
    /// they start, how many, and a chain of their checksums), every open
    /// verifies the newest transaction against it before treating that marker
    /// as a commit point, and a transaction that fails is not committed. The
    /// mode requires `IntegrityPolicy::Crc32WithHeader`, so every byte of
    /// every record is under a checksum that open can check.
    pub const fn single_barrier(self) -> bool {
        matches!(
            self,
            Self::TransactionMarker(
                TransactionMarkerMode::OnFlushSingleBarrier
                    | TransactionMarkerMode::ExplicitSingleBarrier
            )
        )
    }

    pub const fn is_transaction_marker(self) -> bool {
        matches!(self, Self::TransactionMarker(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryPolicy {
    Strict,
    TruncateTail,
    /// Keep a crashed writer's uncommitted tail, marked dead.
    ///
    /// `TruncateTail` answers a crash by deleting every record past the last
    /// commit marker. That is correct and it is cheap, and it is also the only
    /// operation in varlit that destroys data the caller might have wanted: the
    /// records framed, they are structurally complete, and the writer simply
    /// never got to say so.
    ///
    /// This keeps them. On a read-write open of a file whose liveness block
    /// says the previous writer never released, the tail is walked, every
    /// record in it is marked [`crate::RECORD_MUTABLE_FLAG_DEAD`], and a commit marker
    /// is appended so the file ends at a commit point again.
    ///
    /// **The appended marker is not optional.** Without it the file ends past
    /// its last marker, and the *next* marker to be written would make
    /// `committed_prefix_len` treat the whole tail as committed — records the
    /// original writer never committed would become visible, retroactively.
    /// Committing them here, flagged dead, is the honest statement: they
    /// arrived, they were never committed, they are kept, and a defragmenting
    /// rewrite is free to drop them.
    ///
    /// Requires `liveness: footer_flags`, because the mark has nowhere to live
    /// without it.
    ///
    /// The cost is that the file keeps bytes `TruncateTail` would have
    /// reclaimed, until something defragments it.
    MarkTail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestPolicy {
    None,
    Embedded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionAlgorithm {
    Zstd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionLevel {
    Fast,
    Default,
    Best,
    Exact(i32),
}

impl CompressionLevel {
    pub const fn to_zstd_level(self) -> i32 {
        match self {
            Self::Fast => 1,
            Self::Default => 3,
            Self::Best => 19,
            Self::Exact(level) => level,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionHeaderMode {
    RecordExplicit,
    FileExplicit,
    FormatContract,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VariableCompression {
    pub algorithm: CompressionAlgorithm,
    pub level: CompressionLevel,
    pub header_mode: CompressionHeaderMode,
    pub min_uncompressed_len: u64,
    pub only_if_smaller: bool,
    pub max_uncompressed_len: u64,
}

impl VariableCompression {
    pub const DEFAULT_MAX_UNCOMPRESSED_LEN: u64 = 64 * 1024 * 1024;

    pub const fn zstd(header_mode: CompressionHeaderMode) -> Self {
        Self {
            algorithm: CompressionAlgorithm::Zstd,
            level: CompressionLevel::Default,
            header_mode,
            min_uncompressed_len: 0,
            only_if_smaller: true,
            max_uncompressed_len: Self::DEFAULT_MAX_UNCOMPRESSED_LEN,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionPolicy {
    None,
    VariableBlocks(VariableCompression),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockCompressionDescriptor {
    pub block_id: u32,
    pub compression: VariableCompression,
}

/// Declares that a block's records are **not retained in the resident index**.
///
/// The resident index holds one 104-byte entry per record for the life of the
/// handle, which is what makes an open cost memory proportional to record count
/// rather than to bytes. A non-resident block breaks that for its own records:
/// they are written, sequenced, chained and recoverable exactly as before, and
/// they are simply not mirrored in memory.
///
/// **Not part of the computed schema hash.** Residency changes no byte of the
/// file — a file written with `resident = false` on a block is byte-identical
/// to one written without it — so hashing it would turn a memory optimisation
/// into a migration for files that did not change.
///
/// What a non-resident block gives up is everything that reads the resident
/// index: `blocks::<T>()` refuses rather than returning an empty collection,
/// the records are absent from `index_entries()`, and replacement cannot
/// resolve a target. Reach them through the block offset chain, which this
/// option requires for exactly that reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockResidencyDescriptor {
    pub block_id: u32,
    pub resident: bool,
}

/// One matrix dimension that grows past its declared extent, and the row count
/// each chunk covers.
///
/// A matrix's dimensions are otherwise fixed at create, which cannot model a
/// stream whose extent is unknown when the file is made. Declaring this makes
/// dimension 0 unbounded: rows past the declared extent land in new tiles,
/// added as they are written. The dimension's value at create must still equal
/// `rows_per_chunk`. A cell that never received a value keeps its clear commit
/// bit and reads as `MatrixNotCommitted`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrowingMatrixDimension {
    /// The dimension that grows. Must be `dimensions[0]` of every matrix block.
    pub name: &'static str,
    /// The declared value of `name` at create. Rows no longer form chunks.
    pub rows_per_chunk: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldPresence {
    Required,
    Defaulted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldDescriptor {
    pub id: u32,
    pub name: &'static str,
    pub wire_type: WireType,
    pub presence: FieldPresence,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockDescriptor {
    pub id: u32,
    pub name: &'static str,
    pub version: u16,
    pub kind: BlockKind,
    pub fields: &'static [FieldDescriptor],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutPreset {
    VarlitNative,
    None,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentRepeat {
    Once,
    UntilEof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutPartKind {
    FileHeader(FileHeaderDescriptor),
    Segment(SegmentDescriptor),
    LeadIn(LeadInDescriptor),
    Metadata(MetadataDescriptor),
    RawRegion(RawRegionDescriptor),
    Footer(FooterDescriptor),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutPartDescriptor {
    pub name: &'static str,
    pub kind: LayoutPartKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileHeaderDescriptor {
    pub name: &'static str,
    pub fields: &'static [LayoutFieldDescriptor],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentDescriptor {
    pub name: &'static str,
    pub repeat: SegmentRepeat,
    pub lead_in: LeadInDescriptor,
    pub metadata: MetadataDescriptor,
    pub raw_region: RawRegionDescriptor,
    pub footer: Option<FooterDescriptor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeadInDescriptor {
    pub name: &'static str,
    pub fields: &'static [LayoutFieldDescriptor],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetadataDescriptor {
    pub name: &'static str,
    pub source: LayoutBytesSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawRegionDescriptor {
    pub name: &'static str,
    pub source: LayoutBytesSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FooterDescriptor {
    pub name: &'static str,
    pub fields: &'static [LayoutFieldDescriptor],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutBytesSource {
    Caller,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutFieldDescriptor {
    pub name: &'static str,
    pub ty: LayoutFieldType,
    pub source: LayoutFieldSource,
    pub endian: Option<Endian>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutFieldType {
    Bytes { len: u64 },
    U8,
    U16,
    U32,
    U64,
    I64,
}

impl LayoutFieldType {
    pub const fn byte_len(self) -> u64 {
        match self {
            Self::Bytes { len } => len,
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
            Self::U64 | Self::I64 => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutFieldSource {
    LiteralBytes(&'static [u8]),
    LiteralU64(u64),
    LiteralI64(i64),
    Caller,
    Finalize(LayoutFinalize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutFinalize {
    pub target: LayoutAnchor,
    pub relative_to: LayoutAnchor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutAnchor {
    SegmentStart,
    AfterLeadIn,
    MetadataStart,
    RawRegionStart,
    SegmentEnd,
    FooterStart,
    FooterEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutSpec {
    pub preset: LayoutPreset,
    pub parts: &'static [LayoutPartDescriptor],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlan {
    pub preset: LayoutPreset,
    pub parts: Vec<LayoutPlanPartDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlanPartDescriptor {
    pub name: String,
    pub kind: LayoutPlanPartKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutPlanPartKind {
    FileHeader(LayoutPlanFieldGroup),
    Segment(LayoutPlanSegment),
    LeadIn(LayoutPlanFieldGroup),
    Metadata(LayoutPlanRegion),
    RawRegion(LayoutPlanRegion),
    Footer(LayoutPlanFieldGroup),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlanSegment {
    pub name: String,
    pub repeat: SegmentRepeat,
    pub lead_in: LayoutPlanFieldGroup,
    pub metadata: LayoutPlanRegion,
    pub raw_region: LayoutPlanRegion,
    pub footer: Option<LayoutPlanFieldGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlanFieldGroup {
    pub name: String,
    pub fields: Vec<LayoutPlanField>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlanRegion {
    pub name: String,
    pub source: LayoutPlanRegionSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutPlanRegionSource {
    Caller,
    Native(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlanField {
    pub name: String,
    pub ty: LayoutPlanFieldType,
    pub source: LayoutPlanFieldSource,
    pub endian: Option<Endian>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutPlanFieldType {
    Bytes { len: LayoutPlanLen },
    U8,
    U16,
    U32,
    U64,
    I64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutPlanLen {
    Fixed(u64),
    Dynamic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutPlanFieldSource {
    LiteralBytes(Vec<u8>),
    LiteralU64(u64),
    LiteralI64(i64),
    Caller,
    Finalize(LayoutFinalize),
    Native(&'static str),
}

impl LayoutSpec {
    pub const fn varlit_native() -> Self {
        Self {
            preset: LayoutPreset::VarlitNative,
            parts: &[],
        }
    }

    pub const fn none() -> Self {
        Self {
            preset: LayoutPreset::None,
            parts: &[],
        }
    }

    pub const fn custom(parts: &'static [LayoutPartDescriptor]) -> Self {
        Self {
            preset: LayoutPreset::Custom,
            parts,
        }
    }

    pub const fn none_with_parts(parts: &'static [LayoutPartDescriptor]) -> Self {
        Self {
            preset: LayoutPreset::None,
            parts,
        }
    }

    pub const fn is_varlit_native_default(self) -> bool {
        matches!(self.preset, LayoutPreset::VarlitNative) && self.parts.is_empty()
    }

    pub fn first_segment(self) -> Option<SegmentDescriptor> {
        self.parts.iter().find_map(|part| match part.kind {
            LayoutPartKind::Segment(segment) => Some(segment),
            _ => None,
        })
    }

    pub fn file_header(self) -> Option<FileHeaderDescriptor> {
        self.parts.iter().find_map(|part| match part.kind {
            LayoutPartKind::FileHeader(header) => Some(header),
            _ => None,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FormatSpec {
    pub magic: &'static [u8],
    pub version: u16,
    pub endian: Endian,
    /// Declared schema hash compared against the value stored in a file's
    /// native header when the file is opened.
    ///
    /// Schema-hash comparison is opt-in: leaving this at `0` (the default)
    /// records the computed hash when files are created but **disables** the
    /// open-time equality check, so any schema-hash-bearing file of this
    /// format opens without a [`Error::SchemaHashMismatch`]. Pin a non-zero
    /// value (typically [`FormatSpec::computed_schema_hash`], via
    /// `schema_hash: computed;` in `varlit_format!` or
    /// [`FormatSpec::with_computed_schema_hash`]) to make mismatched schemas
    /// fail closed at open.
    pub schema_hash: u64,
    pub extension: Option<&'static str>,
    pub index_policy: IndexPolicy,
    pub commit_policy: CommitPolicy,
    pub integrity_policy: IntegrityPolicy,
    /// Whether a record can be marked dead after it was written.
    pub liveness_policy: LivenessPolicy,
    /// A fixed-size region of the file header that user blocks can be written
    /// into after the file exists. See [`HeaderSlots`].
    pub header_slots: HeaderSlots,
    pub recovery_policy: RecoveryPolicy,
    pub manifest_policy: ManifestPolicy,
    pub compression_policy: CompressionPolicy,
    pub block_compression: &'static [BlockCompressionDescriptor],
    pub block_residency: &'static [BlockResidencyDescriptor],
    pub blocks: &'static [BlockDescriptor],
    pub matrix_dimensions: &'static [MatrixDimensionDescriptor],
    pub matrix_commits: &'static [MatrixCommitDescriptor],
    pub matrix_blocks: &'static [MatrixBlockDescriptor],
    pub matrix_aux: &'static [MatrixAuxDescriptor],
    /// The dimension that grows past its declared extent, if any.
    pub growing_matrix: Option<GrowingMatrixDimension>,
    /// How a chunk's slot regions are compressed, if at all.
    pub chunk_compression: Option<VariableCompression>,
    /// Bytes of the write-ahead ring a new matrix file reserves; `None` for
    /// the default (16 MiB). A reader may fall behind by as many
    /// publications as fit in it before its snapshot expires.
    pub matrix_ring_bytes: Option<u64>,
    /// Bounded keyed storage: keyed blocks are stored once per key in a
    /// record heap inside the file and updated in place, so the file follows
    /// its live data (see `docs/design/bounded-keyed-storage.md`). `None`:
    /// keyed records are appended to the log, as before.
    pub keyed_storage: Option<KeyedStorage>,
    /// Per-block schema identity of the implementations backing
    /// [`FormatSpec::blocks`], as `(block_id, declared endian override,
    /// keyedness, generated schema fingerprint)` tuples.
    ///
    /// `varlit_format!` fills this from the registered block types so
    /// [`FormatSpec::computed_schema_hash`] covers the per-block encoding
    /// inputs that [`BlockDescriptor`] alone does not carry (endian override,
    /// keyedness, and the generated codec identity). Manual schemas use a
    /// static slice such as `&[MyBlock::IDENTITY]`. Every block used through
    /// typed APIs must have an entry; absent identities are rejected before
    /// encoding or decoding. Raw record access needs no Rust type contract.
    /// The fingerprint values themselves stay process-local: they are never
    /// compared against on-disk descriptors, only folded into the computed
    /// hash and checked by the in-process registration gate.
    ///
    /// Where an entry exists for a block id it is **authoritative** for that
    /// id's registration contract, including the endian override :
    /// typed registration rejects any [`crate::VarlitBlock`] implementation
    /// whose `ENDIAN`, resolved through [`FormatSpec::endian`], disagrees with
    /// the entry's — a manual type can no longer request a block written
    /// big-endian through a little-endian implementation and read
    /// byte-swapped values. `None` in the endian position means "no override;
    /// inherit [`FormatSpec::endian`]", which is how it is both hashed
    /// (as an explicit absence marker) and compared (after resolution).
    ///
    pub block_identities: &'static [(u32, Option<Endian>, bool, u64)],
    pub layout: LayoutSpec,
    pub read_limits: ReadLimits,
    /// Whether files this spec creates carry fixed-offset anchors. Not part of
    /// the schema: a file's own header says whether it is anchored, and that is
    /// how it is read and appended to. See [`crate::AnchorPolicy`].
    pub anchors: crate::AnchorPolicy,
}

/// How a format stores its keyed blocks when it declares bounded keyed
/// storage: once per key, in a record heap inside the file, published
/// through the file's write-ahead ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyedStorage {
    /// Free space allowed in the heap before the writer compacts, in
    /// thousandths of live bytes: 10 to 50 (1 to 5 %).
    pub free_space_permille: u16,
    /// Bytes of write-ahead ring a new file reserves; `None` for the default
    /// (2 MiB). A reader may fall behind by as many publications as fit.
    pub ring_bytes: Option<u64>,
    /// Bytes one round of changes may allocate before the writer publishes
    /// it; `None` for the default (64 MiB). A `sync` that changes more is
    /// several publications, each atomic. 1 MiB to 1 TiB.
    pub change_budget: Option<u64>,
}

impl KeyedStorage {
    /// The default: 5 % free space, the default ring and change budget.
    pub const IN_PLACE: Self = Self {
        free_space_permille: 50,
        ring_bytes: None,
        change_budget: None,
    };

    /// Allows `percent` (1 to 5) of free space before compaction.
    pub const fn with_free_space_percent(mut self, percent: u16) -> Self {
        self.free_space_permille = percent.saturating_mul(10);
        self
    }

    /// Reserves `bytes` of write-ahead ring in new files.
    pub const fn with_ring_bytes(mut self, bytes: u64) -> Self {
        self.ring_bytes = Some(bytes);
        self
    }

    /// Lets one round of changes allocate `bytes` (1 MiB to 1 TiB).
    pub const fn with_change_budget(mut self, bytes: u64) -> Self {
        self.change_budget = Some(bytes);
        self
    }

    /// The change budget in bytes.
    pub const fn budget(self) -> u64 {
        match self.change_budget {
            Some(bytes) => bytes,
            None => crate::keyed_heap::DEFAULT_BUDGET,
        }
    }

    pub(crate) const fn validate(self) -> Result<()> {
        if self.free_space_permille < 10 || self.free_space_permille > 50 {
            return Err(Error::InvalidFormatSpec(
                "keyed storage free space must be 1 to 5 %",
            ));
        }
        let budget = self.budget();
        if budget < crate::keyed_heap::MIN_BUDGET || budget > crate::keyed_heap::MAX_BUDGET {
            return Err(Error::InvalidFormatSpec(
                "keyed storage change budget must be 1 MiB to 1 TiB",
            ));
        }
        Ok(())
    }
}

/// What a file's keyed storage holds (see
/// [`VarlitFile::keyed_storage_stats`](crate::VarlitFile::keyed_storage_stats)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyedStorageStats {
    /// The published generation the handle shows (a writer: its last).
    pub generation: u64,
    pub keys: u64,
    /// Bytes of the records stored: leaf entries and the items of records
    /// kept outside the leaves, headers and padding included.
    pub live_bytes: u64,
    /// Bytes of every keyed page: leaves, index pages, item pages, the
    /// freed item log and pooled pages.
    pub heap_bytes: u64,
    /// Bytes of pooled pages waiting for reuse.
    pub free_region_bytes: u64,
    /// Leaf pages, and interior pages: the key index, one boundary per
    /// leaf.
    pub leaf_pages: u64,
    pub index_pages: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct FormatSpecBuilder {
    magic: Option<&'static [u8]>,
    version: u16,
    endian: Endian,
    schema_hash: u64,
    extension: Option<&'static str>,
    index_policy: IndexPolicy,
    commit_policy: CommitPolicy,
    integrity_policy: IntegrityPolicy,
    liveness_policy: LivenessPolicy,
    header_slots: HeaderSlots,
    recovery_policy: RecoveryPolicy,
    manifest_policy: ManifestPolicy,
    compression_policy: CompressionPolicy,
    block_compression: &'static [BlockCompressionDescriptor],
    block_residency: &'static [BlockResidencyDescriptor],
    blocks: &'static [BlockDescriptor],
    matrix_dimensions: &'static [MatrixDimensionDescriptor],
    matrix_commits: &'static [MatrixCommitDescriptor],
    matrix_blocks: &'static [MatrixBlockDescriptor],
    matrix_aux: &'static [MatrixAuxDescriptor],
    growing_matrix: Option<GrowingMatrixDimension>,
    chunk_compression: Option<VariableCompression>,
    block_identities: &'static [(u32, Option<Endian>, bool, u64)],
    layout: LayoutSpec,
    read_limits: ReadLimits,
}

// The implementation is split by concern across these files, included here
// so they share this module (its privacy, and the paths the public API
// inventory records).
include!("format/spec_settings.rs");
include!("format/spec_entrypoints.rs");
include!("format/spec_schema.rs");
include!("format/spec_validation.rs");

struct Fnv1a64 {
    state: u64,
}

impl Fnv1a64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self {
            state: Self::OFFSET,
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(Self::PRIME);
        }
    }

    fn write_u8(&mut self, value: u8) {
        self.write_bytes(&[value]);
    }

    fn write_u16(&mut self, value: u16) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn write_u32(&mut self, value: u32) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn write_str(&mut self, value: &str) {
        self.write_bytes(&(value.len() as u64).to_le_bytes());
        self.write_bytes(value.as_bytes());
    }

    fn finish(self) -> u64 {
        self.state
    }
}

const fn block_kind_hash_byte(kind: BlockKind) -> u8 {
    match kind {
        BlockKind::Fixed => 1,
        BlockKind::Variable => 2,
        BlockKind::Matrix => 3,
        BlockKind::Internal => 4,
    }
}

const fn matrix_commit_kind_hash_byte(kind: MatrixCommitKind) -> u8 {
    match kind {
        MatrixCommitKind::Cell => 1,
        MatrixCommitKind::Single => 2,
        MatrixCommitKind::PerChannel => 3,
    }
}

/// Bytes the `VHSL` block occupies in the file header, framing included, or
/// zero for a format that declares no region.
///
/// **One derivation, three consumers**, and that is deliberate: the encoder in
/// `file.rs` produces the region, the validator above refuses an over-large one,
/// and `native_layout`'s published plan has to report the same number because
/// that plan is where a tool outside this crate learns where the header ends and
/// the first record begins. Deriving it twice is what once let the plan describe
/// a header short by a whole region.
///
/// Saturating rather than fallible, because the plan is infallible and a
/// saturated length is honest where a zero would read as "there is no region";
/// a spec that would saturate is refused by `validate` above.
pub(crate) fn header_slots_region_len(spec: FormatSpec) -> u64 {
    if !spec.header_slots.is_declared() {
        return 0;
    }
    u64::from(spec.header_slots.capacity)
        .saturating_add(HEADER_SLOTS_PREFIX_LEN)
        .saturating_add(HEADER_SLOTS_BLOCK_FRAMING_LEN)
}

/// `version u16 | flags u16 | capacity u32 | used u32 | crc32 u32`.
pub(crate) const HEADER_SLOTS_PREFIX_LEN: u64 = 2 + 2 + 4 + 4 + 4;
/// The `magic[4] | len u32` every extension block after `VCHD` carries.
pub(crate) const HEADER_SLOTS_BLOCK_FRAMING_LEN: u64 = 4 + 4;

const fn header_slot_integrity_hash_byte(integrity: HeaderSlotIntegrity) -> u8 {
    match integrity {
        HeaderSlotIntegrity::None => 0,
        HeaderSlotIntegrity::Rolling => 1,
        HeaderSlotIntegrity::Sealed => 2,
    }
}

const fn index_policy_hash_byte(policy: IndexPolicy) -> u8 {
    // Bit 4 is `segment_on_flush`. It is hashed because it changes the bytes
    // a writer produces - a segment record is a record - so a file written
    // with it is not the file a spec without it describes. A spec that leaves
    // it off hashes to exactly the byte it hashed to before the bit existed.
    //
    // Bit 5 is `header_tails`, and it is hashed for a stronger reason than
    // that: the region lives in the file *header*, so it moves
    // `append_log_start` and with it every record offset. A build that ignored
    // the flag would compute a different append log start and fail somewhere
    // downstream with an unrelated error. `open_digest_on_flush` is
    // deliberately still absent - it adds a record a reader without it indexes
    // as any other internal record, and moves nothing.
    (if policy.scan_on_open { 1 } else { 0 })
        | (if policy.checkpoint_on_flush {
            1 << 1
        } else {
            0
        })
        | (if policy.block_offset_chain { 1 << 2 } else { 0 })
        | (if policy.keyed_offset_chain { 1 << 3 } else { 0 })
        | (if policy.segment_on_flush { 1 << 4 } else { 0 })
        | (if policy.header_tails { 1 << 5 } else { 0 })
}

const fn commit_policy_hash_byte(policy: CommitPolicy) -> u8 {
    match policy {
        CommitPolicy::None => 1,
        CommitPolicy::RecordFooter => 2,
        CommitPolicy::TransactionMarker(TransactionMarkerMode::OnFlush) => 3,
        CommitPolicy::TransactionMarker(TransactionMarkerMode::Explicit) => 4,
        CommitPolicy::TransactionMarker(TransactionMarkerMode::OnFlushSingleBarrier) => 5,
        CommitPolicy::TransactionMarker(TransactionMarkerMode::ExplicitSingleBarrier) => 6,
    }
}

/// A **bit inside the integrity byte**, not a byte of its own.
///
/// It has to be in the schema hash: the policy changes what the record checksum
/// covers, so a build that ignored the flag would read every record as corrupt
/// rather than as incompatible, which is the wrong error and the wrong
/// remedy. But a new `write_u8` would change the hash of every format that
/// leaves the option *off*, and "a user who does not opt in pays nothing" is a
/// standing requirement, not a nicety — a changed hash means existing files
/// stop opening. Setting a previously-zero bit in a byte that is already hashed
/// keeps off byte-identical, which is exactly how `header_tails` was landed.
///
/// The integrity byte rather than any other, because that is what this policy
/// modifies: it decides how much of the record the checksum covers.
const fn liveness_policy_hash_bit(policy: LivenessPolicy) -> u8 {
    match policy {
        LivenessPolicy::None => 0,
        LivenessPolicy::FooterFlags => 0x80,
    }
}

const fn integrity_policy_hash_byte(policy: IntegrityPolicy) -> u8 {
    match policy {
        IntegrityPolicy::None => 1,
        IntegrityPolicy::Crc32 => 2,
        IntegrityPolicy::Crc32WithHeader => 3,
    }
}

const fn recovery_policy_hash_byte(policy: RecoveryPolicy) -> u8 {
    match policy {
        RecoveryPolicy::Strict => 1,
        RecoveryPolicy::TruncateTail => 2,
        RecoveryPolicy::MarkTail => 3,
    }
}

const fn manifest_policy_hash_byte(policy: ManifestPolicy) -> u8 {
    match policy {
        ManifestPolicy::None => 1,
        ManifestPolicy::Embedded => 2,
    }
}

const fn field_presence_hash_byte(presence: FieldPresence) -> u8 {
    match presence {
        FieldPresence::Required => 1,
        FieldPresence::Defaulted => 2,
    }
}

fn hash_compression_policy(hash: &mut Fnv1a64, policy: CompressionPolicy) {
    match policy {
        CompressionPolicy::None => hash.write_u8(0),
        CompressionPolicy::VariableBlocks(compression) => {
            hash.write_u8(1);
            hash_variable_compression(hash, compression);
        }
    }
}

fn hash_variable_compression(hash: &mut Fnv1a64, compression: VariableCompression) {
    hash.write_u8(compression_algorithm_hash_byte(compression.algorithm));
    hash.write_u8(compression_level_hash_byte(compression.level));
    if let CompressionLevel::Exact(level) = compression.level {
        hash.write_bytes(&level.to_le_bytes());
    }
    hash.write_u8(compression_header_mode_hash_byte(compression.header_mode));
    hash.write_bytes(&compression.min_uncompressed_len.to_le_bytes());
    hash.write_u8(u8::from(compression.only_if_smaller));
    hash.write_bytes(&compression.max_uncompressed_len.to_le_bytes());
}

fn hash_layout_spec(hash: &mut Fnv1a64, layout: LayoutSpec) {
    hash.write_u8(layout_preset_hash_byte(layout.preset));
    hash.write_bytes(&(layout.parts.len() as u64).to_le_bytes());
    for part in layout.parts {
        hash.write_str(part.name);
        match part.kind {
            LayoutPartKind::FileHeader(header) => {
                hash.write_u8(1);
                hash.write_str(header.name);
                hash_layout_fields(hash, header.fields);
            }
            LayoutPartKind::Segment(segment) => {
                hash.write_u8(2);
                hash_segment_descriptor(hash, segment);
            }
            LayoutPartKind::LeadIn(lead_in) => {
                hash.write_u8(3);
                hash.write_str(lead_in.name);
                hash_layout_fields(hash, lead_in.fields);
            }
            LayoutPartKind::Metadata(metadata) => {
                hash.write_u8(4);
                hash.write_str(metadata.name);
                hash.write_u8(layout_bytes_source_hash_byte(metadata.source));
            }
            LayoutPartKind::RawRegion(raw) => {
                hash.write_u8(5);
                hash.write_str(raw.name);
                hash.write_u8(layout_bytes_source_hash_byte(raw.source));
            }
            LayoutPartKind::Footer(footer) => {
                hash.write_u8(6);
                hash.write_str(footer.name);
                hash_layout_fields(hash, footer.fields);
            }
        }
    }
}

fn layout_spec_to_plan(layout: LayoutSpec) -> LayoutPlan {
    LayoutPlan {
        preset: layout.preset,
        parts: layout
            .parts
            .iter()
            .map(|part| LayoutPlanPartDescriptor {
                name: part.name.to_string(),
                kind: match part.kind {
                    LayoutPartKind::FileHeader(header) => LayoutPlanPartKind::FileHeader(
                        layout_field_group_to_plan(header.name, header.fields),
                    ),
                    LayoutPartKind::Segment(segment) => {
                        LayoutPlanPartKind::Segment(layout_segment_to_plan(segment))
                    }
                    LayoutPartKind::LeadIn(lead_in) => LayoutPlanPartKind::LeadIn(
                        layout_field_group_to_plan(lead_in.name, lead_in.fields),
                    ),
                    LayoutPartKind::Metadata(metadata) => LayoutPlanPartKind::Metadata(
                        layout_region_to_plan(metadata.name, metadata.source),
                    ),
                    LayoutPartKind::RawRegion(raw) => {
                        LayoutPlanPartKind::RawRegion(layout_region_to_plan(raw.name, raw.source))
                    }
                    LayoutPartKind::Footer(footer) => LayoutPlanPartKind::Footer(
                        layout_field_group_to_plan(footer.name, footer.fields),
                    ),
                },
            })
            .collect(),
    }
}

fn layout_segment_to_plan(segment: SegmentDescriptor) -> LayoutPlanSegment {
    LayoutPlanSegment {
        name: segment.name.to_string(),
        repeat: segment.repeat,
        lead_in: layout_field_group_to_plan(segment.lead_in.name, segment.lead_in.fields),
        metadata: layout_region_to_plan(segment.metadata.name, segment.metadata.source),
        raw_region: layout_region_to_plan(segment.raw_region.name, segment.raw_region.source),
        footer: segment
            .footer
            .map(|footer| layout_field_group_to_plan(footer.name, footer.fields)),
    }
}

fn layout_field_group_to_plan(
    name: &'static str,
    fields: &[LayoutFieldDescriptor],
) -> LayoutPlanFieldGroup {
    LayoutPlanFieldGroup {
        name: name.to_string(),
        fields: fields.iter().copied().map(layout_field_to_plan).collect(),
    }
}

fn layout_region_to_plan(name: &'static str, source: LayoutBytesSource) -> LayoutPlanRegion {
    LayoutPlanRegion {
        name: name.to_string(),
        source: match source {
            LayoutBytesSource::Caller => LayoutPlanRegionSource::Caller,
        },
    }
}

fn layout_field_to_plan(field: LayoutFieldDescriptor) -> LayoutPlanField {
    LayoutPlanField {
        name: field.name.to_string(),
        ty: layout_field_type_to_plan(field.ty),
        source: match field.source {
            LayoutFieldSource::LiteralBytes(bytes) => {
                LayoutPlanFieldSource::LiteralBytes(bytes.to_vec())
            }
            LayoutFieldSource::LiteralU64(value) => LayoutPlanFieldSource::LiteralU64(value),
            LayoutFieldSource::LiteralI64(value) => LayoutPlanFieldSource::LiteralI64(value),
            LayoutFieldSource::Caller => LayoutPlanFieldSource::Caller,
            LayoutFieldSource::Finalize(finalize) => LayoutPlanFieldSource::Finalize(finalize),
        },
        endian: field.endian,
    }
}

fn layout_field_type_to_plan(ty: LayoutFieldType) -> LayoutPlanFieldType {
    match ty {
        LayoutFieldType::Bytes { len } => LayoutPlanFieldType::Bytes {
            len: LayoutPlanLen::Fixed(len),
        },
        LayoutFieldType::U8 => LayoutPlanFieldType::U8,
        LayoutFieldType::U16 => LayoutPlanFieldType::U16,
        LayoutFieldType::U32 => LayoutPlanFieldType::U32,
        LayoutFieldType::U64 => LayoutPlanFieldType::U64,
        LayoutFieldType::I64 => LayoutPlanFieldType::I64,
    }
}

fn native_layout_plan(spec: FormatSpec) -> LayoutPlan {
    let footer = if spec.spec_needs_record_footer() {
        Some(LayoutPlanFieldGroup {
            name: "VarlitRecordFooter".to_string(),
            fields: crate::native_layout::native_record_footer_plan_fields(),
        })
    } else {
        None
    };

    LayoutPlan {
        preset: LayoutPreset::VarlitNative,
        parts: vec![
            LayoutPlanPartDescriptor {
                name: "VarlitFileHeader".to_string(),
                kind: LayoutPlanPartKind::FileHeader(LayoutPlanFieldGroup {
                    name: "VarlitFileHeader".to_string(),
                    fields: crate::native_layout::native_file_header_plan_fields(spec),
                }),
            },
            LayoutPlanPartDescriptor {
                name: "VarlitRecord".to_string(),
                kind: LayoutPlanPartKind::Segment(LayoutPlanSegment {
                    name: "VarlitRecord".to_string(),
                    repeat: SegmentRepeat::UntilEof,
                    lead_in: LayoutPlanFieldGroup {
                        name: "VarlitRecordHeader".to_string(),
                        fields: crate::native_layout::native_record_header_plan_fields(),
                    },
                    metadata: LayoutPlanRegion {
                        name: "NoMetadata".to_string(),
                        source: LayoutPlanRegionSource::Native("none"),
                    },
                    raw_region: LayoutPlanRegion {
                        name: "Payload".to_string(),
                        source: LayoutPlanRegionSource::Native("record_payload"),
                    },
                    footer,
                }),
            },
        ],
    }
}

fn hash_segment_descriptor(hash: &mut Fnv1a64, segment: SegmentDescriptor) {
    hash.write_str(segment.name);
    hash.write_u8(segment_repeat_hash_byte(segment.repeat));
    hash.write_str(segment.lead_in.name);
    hash_layout_fields(hash, segment.lead_in.fields);
    hash.write_str(segment.metadata.name);
    hash.write_u8(layout_bytes_source_hash_byte(segment.metadata.source));
    hash.write_str(segment.raw_region.name);
    hash.write_u8(layout_bytes_source_hash_byte(segment.raw_region.source));
    match segment.footer {
        Some(footer) => {
            hash.write_u8(1);
            hash.write_str(footer.name);
            hash_layout_fields(hash, footer.fields);
        }
        None => hash.write_u8(0),
    }
}

fn hash_layout_fields(hash: &mut Fnv1a64, fields: &[LayoutFieldDescriptor]) {
    hash.write_bytes(&(fields.len() as u64).to_le_bytes());
    for field in fields {
        hash.write_str(field.name);
        hash.write_u8(layout_field_type_hash_byte(field.ty));
        if let LayoutFieldType::Bytes { len } = field.ty {
            hash.write_bytes(&len.to_le_bytes());
        }
        hash.write_u8(match field.endian {
            Some(endian) => endian.to_byte(),
            None => 0,
        });
        hash_layout_field_source(hash, field.source);
    }
}

fn hash_layout_field_source(hash: &mut Fnv1a64, source: LayoutFieldSource) {
    match source {
        LayoutFieldSource::LiteralBytes(bytes) => {
            hash.write_u8(1);
            hash.write_bytes(&(bytes.len() as u64).to_le_bytes());
            hash.write_bytes(bytes);
        }
        LayoutFieldSource::LiteralU64(value) => {
            hash.write_u8(2);
            hash.write_bytes(&value.to_le_bytes());
        }
        LayoutFieldSource::LiteralI64(value) => {
            hash.write_u8(3);
            hash.write_bytes(&value.to_le_bytes());
        }
        LayoutFieldSource::Caller => hash.write_u8(4),
        LayoutFieldSource::Finalize(finalize) => {
            hash.write_u8(5);
            hash.write_u8(layout_anchor_hash_byte(finalize.target));
            hash.write_u8(layout_anchor_hash_byte(finalize.relative_to));
        }
    }
}

const fn layout_preset_hash_byte(preset: LayoutPreset) -> u8 {
    match preset {
        LayoutPreset::VarlitNative => 1,
        LayoutPreset::None => 2,
        LayoutPreset::Custom => 3,
    }
}

const fn segment_repeat_hash_byte(repeat: SegmentRepeat) -> u8 {
    match repeat {
        SegmentRepeat::Once => 1,
        SegmentRepeat::UntilEof => 2,
    }
}

const fn layout_bytes_source_hash_byte(source: LayoutBytesSource) -> u8 {
    match source {
        LayoutBytesSource::Caller => 1,
    }
}

const fn layout_field_type_hash_byte(ty: LayoutFieldType) -> u8 {
    match ty {
        LayoutFieldType::Bytes { .. } => 1,
        LayoutFieldType::U8 => 2,
        LayoutFieldType::U16 => 3,
        LayoutFieldType::U32 => 4,
        LayoutFieldType::U64 => 5,
        LayoutFieldType::I64 => 6,
    }
}

const fn layout_anchor_hash_byte(anchor: LayoutAnchor) -> u8 {
    match anchor {
        LayoutAnchor::SegmentStart => 1,
        LayoutAnchor::AfterLeadIn => 2,
        LayoutAnchor::MetadataStart => 3,
        LayoutAnchor::RawRegionStart => 4,
        LayoutAnchor::SegmentEnd => 5,
        LayoutAnchor::FooterStart => 6,
        LayoutAnchor::FooterEnd => 7,
    }
}

const fn compression_algorithm_hash_byte(algorithm: CompressionAlgorithm) -> u8 {
    match algorithm {
        CompressionAlgorithm::Zstd => 1,
    }
}

const fn compression_level_hash_byte(level: CompressionLevel) -> u8 {
    match level {
        CompressionLevel::Fast => 1,
        CompressionLevel::Default => 2,
        CompressionLevel::Best => 3,
        CompressionLevel::Exact(_) => 4,
    }
}

const fn compression_header_mode_hash_byte(mode: CompressionHeaderMode) -> u8 {
    match mode {
        CompressionHeaderMode::RecordExplicit => 1,
        CompressionHeaderMode::FileExplicit => 2,
        CompressionHeaderMode::FormatContract => 3,
    }
}

include!("format/builder.rs");

impl Default for FormatSpecBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod read_limit_tests {
    use super::*;

    #[test]
    fn scalar_meet_never_promotes_missing_to_trust() {
        use ReadLimit::{Finite, Missing, TrustedUnbounded};

        assert_eq!(Finite(9).meet(Finite(4)), Finite(4));
        assert_eq!(Finite(0).meet(TrustedUnbounded), Finite(0));
        assert_eq!(Missing.meet(Finite(7)), Finite(7));
        assert_eq!(Missing.meet(TrustedUnbounded), Missing);
        assert_eq!(TrustedUnbounded.meet(Missing), Missing);
        assert_eq!(TrustedUnbounded.meet(TrustedUnbounded), TrustedUnbounded);
    }

    #[test]
    fn runtime_limits_only_tighten_finite_declarations() {
        let declared = ReadLimits::finite_all(100)
            .with_max_file_len(50)
            .with_max_records(0);
        let runtime = ReadLimits::trusted_unbounded()
            .with_max_file_len(75)
            .with_max_records(10);
        let effective = declared.tighten(runtime);

        assert_eq!(effective.max_file_len, ReadLimit::Finite(50));
        assert_eq!(effective.max_records, ReadLimit::Finite(0));
        assert!(!effective.is_trusted_api);
    }

    #[test]
    fn missing_fields_resolve_from_standard_policy() {
        let resolved = ReadLimits::missing()
            .with_max_record_payload_len(7)
            .resolve();

        assert_eq!(resolved.max_record_payload_len, ReadLimit::Finite(7));
        assert_eq!(resolved.max_file_len, ReadLimit::Finite(u64::MAX));
        assert_eq!(resolved.max_scan_bytes, ReadLimit::Finite(u64::MAX));
        assert_eq!(resolved.max_records, ReadLimit::Finite(u64::MAX));
        assert_eq!(resolved.max_segments, ReadLimit::Finite(u64::MAX));
        assert_eq!(
            resolved.max_index_bytes,
            ReadLimit::Finite(1024 * 1024 * 1024)
        );
        assert_eq!(
            resolved.max_keyed_tail_bytes,
            ReadLimit::Finite(256 * 1024 * 1024)
        );
        assert_eq!(
            resolved.max_logical_payload_len,
            ReadLimits::STANDARD.max_logical_payload_len
        );
    }

    #[test]
    fn resource_overlay_can_raise_or_lower_format_defaults() {
        let spec = FormatSpec::new(
            b"LIMITS",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            &[],
        )
        .with_resource_defaults(
            ReadLimits::missing()
                .with_max_record_payload_len(10)
                .with_max_materialized_bytes(100),
        );

        let raised =
            spec.with_resource_limits(ReadLimits::missing().with_max_record_payload_len(20));
        assert_eq!(
            raised.read_limits.max_record_payload_len,
            ReadLimit::Finite(20)
        );
        assert_eq!(
            raised.read_limits.max_materialized_bytes,
            ReadLimit::Finite(100)
        );

        let lowered =
            spec.with_resource_limits(ReadLimits::missing().with_max_record_payload_len(5));
        assert_eq!(
            lowered.read_limits.max_record_payload_len,
            ReadLimit::Finite(5)
        );

        let tightened =
            spec.tighten_read_limits(ReadLimits::missing().with_max_record_payload_len(20));
        assert_eq!(
            tightened.read_limits.max_record_payload_len,
            ReadLimit::Finite(10)
        );
    }

    #[test]
    fn ordinary_and_trusted_resolution_are_distinct() {
        let missing = ReadLimits::missing();
        assert!(matches!(
            missing.require(ReadLimitKey::FileLen),
            Err(Error::MissingResourceLimit { .. })
        ));

        let trusted = ReadLimits::trusted_unbounded();
        assert!(matches!(
            trusted.require(ReadLimitKey::FileLen),
            Err(Error::TrustedUnboundedRequiresExplicitApi { .. })
        ));
        assert_eq!(
            trusted
                .authorize_trusted_api()
                .require(ReadLimitKey::FileLen)
                .unwrap(),
            None
        );

        let bounded = trusted.with_max_file_len(3).authorize_trusted_api();
        assert!(bounded.check(ReadLimitKey::FileLen, 3).is_ok());
        assert!(matches!(
            bounded.check(ReadLimitKey::FileLen, 4),
            Err(Error::LimitExceeded { limit: 3, .. })
        ));
    }

    #[test]
    fn untrusted_preset_is_finite_in_every_aggregate_dimension() {
        let limits = ReadLimits::untrusted();
        for (name, limit) in [
            ("file length", limits.max_file_len),
            ("record count", limits.max_records),
            ("index bytes", limits.max_index_bytes),
            ("scan bytes", limits.max_scan_bytes),
            ("segment count", limits.max_segments),
        ] {
            match limit {
                ReadLimit::Finite(value) => {
                    assert!(value < u64::MAX, "{name} must have a real finite bound");
                }
                other => panic!("{name} must be finite, got {other:?}"),
            }
        }
        assert_eq!(
            limits.max_record_payload_len,
            ReadLimits::STANDARD.max_record_payload_len
        );
        assert_eq!(
            limits.max_materialized_bytes,
            ReadLimits::STANDARD.max_materialized_bytes
        );
    }

    #[test]
    fn read_limits_do_not_change_schema_hash() {
        let base = FormatSpec::new(
            b"LIMITS",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            &[],
        );
        let bounded = base.with_read_limits(ReadLimits::finite_all(1));
        assert_eq!(base.computed_schema_hash(), bounded.computed_schema_hash());
    }
}

#[cfg(test)]
mod matrix_tile_tests {
    use super::*;

    #[test]
    fn the_default_tile_fills_about_64_kib_nearly_square() {
        // 1 KiB cells (+4 checksum bytes): 63 fit, so 32 cells as 4 x 8.
        assert_eq!(matrix_default_tile(1024), [4, 8]);
        // 4-byte cells: 8192 fit, 64 x 128.
        assert_eq!(matrix_default_tile(4), [64, 128]);
        // A cell larger than the target still gets a tile of one.
        assert_eq!(matrix_default_tile(1 << 20), [1, 1]);
        for stride in [1u64, 3, 8, 100, 1000, 4096, 65_532, 65_533] {
            let [rows, cols] = matrix_default_tile(stride);
            let cells = rows * cols;
            assert!(cells.is_power_of_two() && rows <= cols, "stride {stride}");
            assert!(cells == 1 || cells * (stride + 4) <= MATRIX_DEFAULT_TILE_BYTES);
            assert!(
                cells * 2 * (stride + 4) > MATRIX_DEFAULT_TILE_BYTES,
                "stride {stride}"
            );
        }
    }

    #[test]
    fn a_declared_tile_is_used_and_zero_means_default() {
        let mut block = MatrixBlockDescriptor {
            block_id: 1,
            dimensions: ["r", "c"],
            category: "data",
            slot_stride: 8,
            tile: [0, 0],
        };
        assert_eq!(block.effective_tile(), matrix_default_tile(8));
        block.tile = [16, 2];
        assert_eq!(block.effective_tile(), [16, 2]);
    }
}
