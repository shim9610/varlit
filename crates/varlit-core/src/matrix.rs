//! The public matrix vocabulary: keys, extents, cell status, recovery
//! findings and actions, resume signals, commit events and storage
//! diagnostics. Storage itself is `crate::matrix_tiles`.

use crate::format::ReadLimitKey;
use crate::{BlockKind, Error, FormatSpec, Result, TryClone, VarlitMatrixBlock};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixDimensionValue {
    pub name: String,
    pub value: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixDimensions {
    values: Vec<MatrixDimensionValue>,
}

impl MatrixDimensions {
    pub fn from_pairs<const N: usize>(pairs: [(&'static str, u64); N]) -> Self {
        let values = pairs
            .into_iter()
            .map(|(name, value)| MatrixDimensionValue {
                name: name.to_string(),
                value,
            })
            .collect();
        Self { values }
    }

    pub fn get(&self, name: &str) -> Option<u64> {
        self.values
            .iter()
            .find(|value| value.name == name)
            .map(|value| value.value)
    }

    pub fn values(&self) -> &[MatrixDimensionValue] {
        &self.values
    }
}

impl<const N: usize> From<[(&'static str, u64); N]> for MatrixDimensions {
    fn from(value: [(&'static str, u64); N]) -> Self {
        Self::from_pairs(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MatrixKey {
    pub scan: u64,
    pub ch: u64,
}

impl MatrixKey {
    pub const fn new(scan: u64, ch: u64) -> Self {
        Self { scan, ch }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixCellStatus {
    Committed,
    NotCommitted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedBitmap {
    bit_len: u64,
    bytes: Vec<u8>,
}

/// A zero-length bitmap.
impl crate::VarlitEmpty for PackedBitmap {
    fn empty() -> Self {
        Self {
            bit_len: 0,
            bytes: Vec::new(),
        }
    }
}

/// Byte length of a `PackedBitmap` carrying `bit_len` bits.
///
/// Deliberately separate from [`bit_bytes`]: `PackedBitmap` is an ordinary
/// variable-field codec that happens to live in this module, so its failures
/// must be codec failures. Reporting `InvalidMatrixLayout` for a malformed
/// bitmap in a non-matrix field told the caller to look at a matrix layout that
/// may not even exist.
fn packed_bitmap_bytes(bit_len: u64) -> Result<u64> {
    bit_len
        .checked_add(7)
        .map(|value| value / 8)
        .ok_or(Error::LengthOverflow { value: bit_len })
}

impl PackedBitmap {
    pub fn new(bit_len: u64) -> Result<Self> {
        Ok(Self {
            bit_len,
            bytes: filled_bytes_for(packed_bitmap_bytes(bit_len)?, 0, PACKED_BITMAP_RESOURCE)?,
        })
    }

    pub fn bit_len(&self) -> u64 {
        self.bit_len
    }

    pub fn get(&self, ordinal: u64) -> Result<bool> {
        if ordinal >= self.bit_len {
            return Err(Error::InvalidCanonicalEncoding(
                "packed bitmap ordinal is past its bit length",
            ));
        }
        get_bit(&self.bytes, ordinal)
    }

    pub fn set(&mut self, ordinal: u64, value: bool) -> Result<()> {
        if ordinal >= self.bit_len {
            return Err(Error::InvalidCanonicalEncoding(
                "packed bitmap ordinal is past its bit length",
            ));
        }
        set_bit(&mut self.bytes, ordinal, value)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Structural codec identity of [`PackedBitmap`] .
///
/// Derived the way the built-in container codecs derive theirs: a tag naming
/// this codec folded with the identities of the codecs whose bytes it actually
/// emits, in emission order (`u64` bit length, then `Vec<u8>` payload). The
/// value is a const expression over compile-time constants only, so it is
/// stable across builds, distinct from any other built-in identity, and
/// non-zero — which is what `#[derive(VarlitBlock)]` requires of every field
/// codec, and therefore what makes a `PackedBitmap` *variable* field compile.
///
/// It deliberately does **not** make `PackedBitmap` usable as a fixed-width
/// matrix field: the encoding above has no width that is fixed by the type, so
/// there is no compile-time slot stride for it. See
/// `varlit-macros::matrix_field_is_fixed_width`.
const PACKED_BITMAP_SCHEMA_ID: u64 = crate::codec::container_schema_id_with_arity(
    b"packed_bitmap",
    1,
    &[
        <u64 as crate::VarlitEncode>::SCHEMA_ID,
        <Vec<u8> as crate::VarlitEncode>::SCHEMA_ID,
    ],
);

impl crate::VarlitEncode for PackedBitmap {
    const WIRE_TYPE: crate::WireType = crate::WireType::Bytes;
    const SCHEMA_ID: u64 = PACKED_BITMAP_SCHEMA_ID;

    fn encode_varlit(&self, encoder: &mut crate::Encoder) -> Result<()> {
        crate::VarlitEncode::encode_varlit(&self.bit_len, encoder)?;
        crate::VarlitEncode::encode_varlit(&self.bytes, encoder)
    }
}

impl crate::VarlitDecode for PackedBitmap {
    const WIRE_TYPE: crate::WireType = crate::WireType::Bytes;
    // Symmetric codec: `decode_varlit` accepts exactly what `encode_varlit`
    // emits, so it declares the same identity, as the built-in codecs do.
    const SCHEMA_ID: u64 = PACKED_BITMAP_SCHEMA_ID;

    fn decode_varlit(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
        let bit_len = decoder.decode_value::<u64>()?;
        let bytes = decoder.decode_value::<Vec<u8>>()?;
        // Ordinary malformed codec input, so an ordinary decode error: this
        // type is accepted in plain variable fields and must not report a
        // matrix-layout fault there.
        if bytes.len() as u64 != packed_bitmap_bytes(bit_len)? {
            return Err(Error::InvalidCanonicalEncoding(
                "packed bitmap payload length does not match its bit length",
            ));
        }
        Ok(Self { bit_len, bytes })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixCorruptionSeverity {
    Fatal,
    Recoverable,
    Advisory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixCorruptionKind {
    Header,
    Layout,
    CommitMap,
    Slot,
    Sidecar,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixRecoveryFinding {
    pub kind: MatrixCorruptionKind,
    pub severity: MatrixCorruptionSeverity,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatrixRecoveryAction {
    ClearCell { category: String, key: MatrixKey },
    ClearCategory { category: String },
    RebuildCommitMap { category: Option<String> },
    Resume,
    Restart,
    DiscardSidecar,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixRecoveryReport {
    pub findings: Vec<MatrixRecoveryFinding>,
    pub recommended_actions: Vec<MatrixRecoveryAction>,
}

/// Always-available cost qualification for whole-range zeroing.
///
/// Whole-category clear, whole-map rebuild, and page-index rebuild all ask the
/// filesystem to remove a byte range. Where removal is unavailable the range is
/// streamed as zero bytes, which costs `Theta(bitmap bytes)`. These accessors
/// are deliberately *not* behind a test-only feature: a caller that depends on
/// the cheap path must be able to detect at runtime that it did not get it.
impl MatrixRecoveryReport {
    /// Whether this build targets a platform with a range-removal call at all.
    ///
    /// Necessary but **not** sufficient: the call still fails on filesystems
    /// without sparse-file support, in which case the streaming fallback runs
    /// and [`Self::matrix_last_zero_range_streamed_bytes`] reports it.
    pub const fn matrix_sparse_zeroing_supported() -> bool {
        cfg!(any(windows, target_os = "linux", target_os = "macos"))
    }

    /// Bytes the most recent *single* range-zeroing request on this thread had
    /// to stream because the range could not be removed.
    ///
    /// `0` means that one range was removed without writing it, and any nonzero
    /// value is proof that the caller is on the `Theta(bytes)` fallback path —
    /// but the scope is deliberately narrow in two ways a caller must respect:
    ///
    /// * **One request, not one operation.** A whole-category clear issues
    ///   several range requests (validity bitmap, page indexes, page digests,
    ///   commit map). This reports only the last of them, so a `0` here does
    ///   not prove the whole clear avoided streaming. To qualify an operation,
    ///   read [`Self::matrix_total_zero_range_streamed_bytes`] before and after
    ///   it: the delta is nonzero exactly when some request streamed.
    /// * **This thread only.** Both counters are thread-local. A clear
    ///   performed on a worker thread is invisible to a reader on another
    ///   thread, which would see `0` and wrongly conclude it took the cheap
    ///   path. Sample the counters on the thread that ran the operation.
    pub fn matrix_last_zero_range_streamed_bytes() -> u64 {
        // Tiled storage never zeroes a range: it allocates what it writes.
        0
    }

    /// Bytes streamed by every range-zeroing request **on this thread** since
    /// it started, across all matrices.
    ///
    /// This is the counter to qualify a whole operation with: take it before
    /// and after, on the thread performing the operation, and a nonzero delta
    /// is exact proof that at least one range had to be streamed.
    pub fn matrix_total_zero_range_streamed_bytes() -> u64 {
        0
    }
}

/// Thread-local observability counters for the paged matrix integrity
/// representation (/ scaling contracts).
///
/// These hang off `MatrixRecoveryReport` because it is the matrix diagnostic
/// type already exported from the crate root; they are not a property of any

/// A rectangle of cells: rows `row_start..row_end` and columns
/// `col_start..col_end` (`MatrixKey::scan` and `MatrixKey::ch`), read
/// row-major. A row and a column are regions one cell wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MatrixRegion {
    pub row_start: u64,
    pub row_end: u64,
    pub col_start: u64,
    pub col_end: u64,
}

impl MatrixRegion {
    pub const fn new(rows: std::ops::Range<u64>, cols: std::ops::Range<u64>) -> Self {
        Self {
            row_start: rows.start,
            row_end: rows.end,
            col_start: cols.start,
            col_end: cols.end,
        }
    }

    /// Columns `cols` of one row.
    pub const fn row(row: u64, cols: std::ops::Range<u64>) -> Self {
        Self::new(row..row + 1, cols)
    }

    /// Rows `rows` of one column.
    pub const fn column(col: u64, rows: std::ops::Range<u64>) -> Self {
        Self::new(rows, col..col + 1)
    }

    pub const fn is_empty(&self) -> bool {
        self.row_start >= self.row_end || self.col_start >= self.col_end
    }

    pub const fn contains(&self, key: MatrixKey) -> bool {
        key.scan >= self.row_start
            && key.scan < self.row_end
            && key.ch >= self.col_start
            && key.ch < self.col_end
    }

    /// Cells in the region, when that fits a `u64`.
    pub const fn cells(&self) -> Option<u64> {
        if self.is_empty() {
            return Some(0);
        }
        (self.row_end - self.row_start).checked_mul(self.col_end - self.col_start)
    }

    /// The first cell, row-major.
    pub const fn first(&self) -> MatrixKey {
        MatrixKey::new(self.row_start, self.col_start)
    }
}

/// Where a region read stopped because the caller's buffer was full: pass
/// it to the `_from` read to continue with the next cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MatrixCursor {
    region: MatrixRegion,
    next: MatrixKey,
}

impl MatrixCursor {
    pub(crate) const fn new(region: MatrixRegion, next: MatrixKey) -> Self {
        Self { region, next }
    }
    /// The region being read.
    pub const fn region(&self) -> MatrixRegion {
        self.region
    }
    /// The next cell the continuation reads.
    pub const fn next_key(&self) -> MatrixKey {
        self.next
    }
}

/// One page of a region read: how many cells the caller's buffer received,
/// and where to continue when the region is not finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct MatrixPage {
    pub filled: usize,
    pub next: Option<MatrixCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixResumeSignal {
    Clean,
    Partial { committed: u64, total: u64 },
    ResumeAvailable { committed: u64, total: u64 },
    RestartRecommended,
    DiscardRecommended,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixCommitEvent {
    pub block_id: u32,
    pub category: &'static str,
    pub key: MatrixKey,
    pub slot_offset: u64,
    pub slot_len: u64,
}

/// Validates `T` against the block the spec declares for `T::ID`, and returns
/// **the spec's category for that block** — which is the point, not a
/// by-product.
///
/// `T::CATEGORY` used to be checked against the descriptor here and then
/// trusted by thirteen call sites. That made a caller-declared constant the key
/// that selects which commit quarantine and which fatal-access gate apply to a
/// cell, and the check was the only thing holding it up — so the chunk path,
/// which never called this function, keyed those gates on an unvalidated
/// string. Returning the descriptor's category instead removes the load from
/// `T::CATEGORY` altogether: it is now descriptive, no code path can act on a
/// wrong value, and there is nothing left to check.
///
/// What stays checked is what a reader owes: `T` against the *file*. The spec's
/// descriptor table is tied to the file by the schema hash (`file.rs`
/// `SchemaHashMismatch`), so a `T::VERSION` or `T::DIMENSIONS` that disagrees
/// with it is a type that disagrees with the bytes on disk — which must be
/// refused rather than decoded into confidently wrong values.
pub(crate) fn ensure_matrix_block<T: VarlitMatrixBlock>(spec: FormatSpec) -> Result<&'static str> {
    // run the common registration gate first, exactly like the
    // fixed/variable block paths. It enforces the process-local first-seen
    // schema-fingerprint and keyedness contract, so a manual matrix type
    // with the same shape/stride but different codec/decode semantics is
    // rejected before any cell read, write, or mmap view.
    crate::collections::ensure_registered_block::<T>(spec)?;
    let descriptor = spec.block(T::ID).ok_or(Error::UnregisteredBlock(T::ID))?;
    if descriptor.kind != BlockKind::Matrix || T::KIND != BlockKind::Matrix {
        return Err(Error::BlockKindMismatch {
            expected: BlockKind::Matrix,
            actual: T::KIND,
        });
    }
    if descriptor.version != T::VERSION {
        return Err(Error::BlockVersionMismatch {
            block_id: T::ID,
            expected: descriptor.version,
            actual: T::VERSION,
        });
    }
    let matrix = spec
        .matrix_blocks
        .iter()
        .find(|block| block.block_id == T::ID)
        .ok_or(Error::MatrixBlockMissing(T::ID))?;
    if matrix.dimensions != T::DIMENSIONS
        || matrix.slot_stride != T::SLOT_STRIDE
        || matrix.effective_tile() != T::TILE
    {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(matrix.category)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MatrixStorageDiagnostics {
    /// Some(true): sparse flag/holes observed. Some(false): disabled or known
    /// unsupported filesystem. None: sparseness could not be established.
    pub sparse_file: Option<bool>,
    /// The actual file answered a bounded allocation-map probe.
    pub allocation_map_available: bool,
    /// This build has a native hole-punch API. A filesystem may still refuse
    /// individual requests; the zero-range byte counters report that fallback.
    pub hole_punch_api_available: bool,
    /// Windows refused the create-time sparse-file request.
    pub sparse_mark_failed: bool,
}

impl MatrixStorageDiagnostics {
    /// What a tiled matrix reports: it allocates what it writes and never
    /// relies on holes, allocation maps or hole punching.
    pub(crate) const fn not_applicable() -> Self {
        Self {
            sparse_file: Some(false),
            allocation_map_available: false,
            hole_punch_api_available: false,
            sparse_mark_failed: false,
        }
    }
}

const PACKED_BITMAP_RESOURCE: &str = "packed bitmap bytes";

const MATRIX_SIDECAR_RESOURCE: &str = "matrix sidecar";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MatrixSidecarReadPlan {
    pub(crate) format_magic_offset: u64,
    pub(crate) category_offset: u64,
    pub(crate) payload_offset: u64,
    pub(crate) payload_len: u64,
    pub(crate) total_len: u64,
}

pub(crate) fn check_matrix_sidecar_file_len(spec: FormatSpec, file_len: u64) -> Result<()> {
    spec.read_limits.check(ReadLimitKey::SidecarLen, file_len)
}

pub(crate) fn matrix_sidecar_write_len(
    spec: FormatSpec,
    fixed_header_len: u64,
    format_magic_len: u64,
    category_len: u64,
    payload_len: u64,
) -> Result<u64> {
    spec.read_limits
        .check(ReadLimitKey::MaterializedBytes, payload_len)?;
    let metadata_len = fixed_header_len
        .checked_add(format_magic_len)
        .and_then(|len| len.checked_add(category_len))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: MATRIX_SIDECAR_RESOURCE,
        })?;
    let total_len =
        metadata_len
            .checked_add(payload_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: MATRIX_SIDECAR_RESOURCE,
            })?;
    check_matrix_sidecar_file_len(spec, total_len)?;
    Ok(total_len)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn matrix_sidecar_read_plan(
    spec: FormatSpec,
    file_len: u64,
    fixed_header_len: u64,
    format_magic_len: u64,
    category_len: u64,
    payload_len: u64,
    flags: u16,
    reserved: u16,
    trailing_reserved: u32,
) -> Result<MatrixSidecarReadPlan> {
    check_matrix_sidecar_file_len(spec, file_len)?;
    if flags != 0 || reserved != 0 || trailing_reserved != 0 {
        return Err(Error::InvalidMatrixSidecar);
    }
    let total_len = matrix_sidecar_write_len(
        spec,
        fixed_header_len,
        format_magic_len,
        category_len,
        payload_len,
    )?;
    if total_len != file_len {
        return Err(Error::InvalidMatrixSidecar);
    }
    let category_offset = fixed_header_len.checked_add(format_magic_len).ok_or(
        Error::ResourceArithmeticOverflow {
            resource: MATRIX_SIDECAR_RESOURCE,
        },
    )?;
    let payload_offset =
        category_offset
            .checked_add(category_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: MATRIX_SIDECAR_RESOURCE,
            })?;
    Ok(MatrixSidecarReadPlan {
        format_magic_offset: fixed_header_len,
        category_offset,
        payload_offset,
        payload_len,
        total_len,
    })
}

fn filled_bytes_for(len: u64, value: u8, resource: &'static str) -> Result<Vec<u8>> {
    let len_usize = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len_usize)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested: len,
        })?;
    bytes.resize(len_usize, value);
    Ok(bytes)
}

fn get_bit(bits: &[u8], ordinal: u64) -> Result<bool> {
    let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
    let mask = 1u8 << (ordinal % 8);
    Ok(bits.get(byte).ok_or(Error::InvalidMatrixLayout)? & mask != 0)
}

fn set_bit(bits: &mut [u8], ordinal: u64, value: bool) -> Result<()> {
    let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
    let mask = 1u8 << (ordinal % 8);
    let target = bits.get_mut(byte).ok_or(Error::InvalidMatrixLayout)?;
    if value {
        *target |= mask;
    } else {
        *target &= !mask;
    }
    Ok(())
}

impl TryClone for MatrixDimensionValue {
    fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            name: self.name.try_clone()?,
            value: self.value,
        })
    }
}

impl TryClone for MatrixDimensions {
    fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            values: self.values.try_clone()?,
        })
    }
}

impl TryClone for PackedBitmap {
    fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            bit_len: self.bit_len,
            bytes: self.bytes.try_clone()?,
        })
    }
}

impl TryClone for MatrixRecoveryFinding {
    fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            kind: self.kind,
            severity: self.severity,
            message: self.message.try_clone()?,
        })
    }
}
