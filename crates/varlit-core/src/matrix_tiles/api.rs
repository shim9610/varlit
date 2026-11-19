//! The matrix API's meaning over the tile engine: blocks and their commit
//! categories, per-cell commit bits and checksums, single and per-channel
//! commit flags, and aux regions.
//!
//! - A cell's slot is its bytes and their CRC32. A slot that was never
//!   written is all zero, which no written slot is: the
//!   CRC32 of any bytes, zeros included, is stored next to them and the CRC32
//!   of an all-zero slot is never zero. So "written" needs no session state.
//! - A tile's commit bitmap is one unit with its own CRC32, rewritten whole.
//! - Singles are one 8-byte unit each; per-channel bitmaps and aux regions
//!   are laid out in [`EXTRA_PAGE`]-byte pages, each rewritten whole, so two
//!   writes of overlapping ranges never leave overlapping units in the ring.

use super::reader_lock::ReaderLock;

use super::geometry::{BlockGeom, Geometry, TileCoord, after_pages, checked_bitmap, seal_bitmap};
use super::store::{TileSource, TileStore};
use super::view::TileReader;
use super::writer::TileWriter;
use crate::{
    Error, FormatSpec, MatrixCellStatus, MatrixCommitKind, MatrixDimensions, MatrixKey,
    MatrixResumeSignal, Result,
};

/// The unit of per-channel bitmaps and aux regions.
pub(crate) const EXTRA_PAGE: u64 = 256;
/// The largest ring a new matrix gets unless the spec says otherwise.
pub(crate) const DEFAULT_RING_BYTES: u64 = 16 << 20;
/// The smallest default ring.
pub(crate) const MIN_DEFAULT_RING_BYTES: u64 = 256 << 10;

/// The working buffers of one read pass: a tile row's slots, and a tile's
/// commit bitmap.
struct PassBuffers<'a> {
    slots: &'a mut [u8],
    bitmap: &'a mut [u8],
}

/// Where the spec's named things live.
#[derive(Clone, Debug)]
pub(crate) struct Names {
    /// `(block id, category)` per block, in geometry order.
    blocks: Vec<(u32, &'static str)>,
    /// `(name, offset in the metadata record)`.
    singles: Vec<(&'static str, u64)>,
    /// `(name, offset, channels)`.
    per_channel: Vec<(&'static str, u64, u64)>,
    /// `(name, offset, byte_len)`.
    aux: Vec<(&'static str, u64, u64)>,
}

/// The two ways a handle holds the engine.
// One per handle; the reader stays inline so its lock is reached directly.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Engine {
    Writer(Box<TileWriter>),
    /// Readers cache tile lookups and the last ring header they validated
    /// against; the lock makes `&self` reads possible across threads.
    Reader(ReaderLock<TileReader>),
}

pub(crate) struct TiledMatrix {
    pub(crate) geom: Geometry,
    names: Names,
    pub(crate) engine: Engine,
    pub(crate) meta: u64,
    /// Distinguishes this matrix from any earlier one at the same path.
    pub(crate) creation_id: [u8; 16],
    /// Whether reads check each cell's checksum: the read-validation
    /// policy's `data_crc`. A commit bitmap's checksum is always checked
    /// before its bits are trusted; tile headers are checked only by
    /// `verify_matrix_metadata`.
    verify_cells: bool,
    /// The buffers cell reads and writes stage through, sized for this
    /// matrix when it is opened and reused by every call.
    scratch: ReaderLock<MatrixScratch>,
    /// Where a typed bulk read stages the encoded cells it decodes: one
    /// batch, sized when the matrix is opened.
    payloads: ReaderLock<Vec<u8>>,
}

/// The cells a typed bulk read stages at a time, and their byte bound.
pub(crate) const RUN_CELLS: usize = 1024;
pub(crate) const RUN_BYTES: usize = 64 * 1024;

/// How many cells of `stride` bytes one staged batch holds.
pub(crate) fn batch_cells(stride: usize, wanted: usize) -> usize {
    RUN_CELLS
        .min((RUN_BYTES / stride).max(1))
        .min(wanted.max(1))
}

/// Staging for one call: a tile's commit bitmap unit, a run of slots (each a
/// payload followed by its checksum) and the tile order of a key batch.
pub(crate) struct MatrixScratch {
    bitmap: Vec<u8>,
    slots: Vec<u8>,
    /// One slot, payload and checksum, for a write or a commit.
    slot: Vec<u8>,
    /// The tile order of up to [`ORDER_KEYS`] keys of a key batch.
    order: Vec<(TileCoord, u64, usize)>,
}

/// Keys of a batch read ordered by tile at a time: a larger batch goes in
/// several passes over the same buffer.
const ORDER_KEYS: usize = 4096;

impl MatrixScratch {
    /// Exactly what the largest block of `geom` needs: one bitmap unit, and
    /// the longest slot run a read stages (a tile row, or a key-batch span).
    fn for_geometry(geom: &Geometry) -> Result<Self> {
        let mut bitmap = 0;
        let mut slots = 0;
        for block in &geom.blocks {
            let span = (CELLS_SPAN_BYTES / block.slot_len()).clamp(1, block.cells());
            bitmap = bitmap.max(block.bitmap_unit_len());
            slots = slots.max(block.tile[1].max(span) * block.slot_len());
        }
        let slot = geom
            .blocks
            .iter()
            .map(BlockGeom::slot_len)
            .max()
            .unwrap_or(0);
        let mut order = Vec::new();
        order
            .try_reserve_exact(ORDER_KEYS)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix key batch order",
                requested: (ORDER_KEYS * std::mem::size_of::<(TileCoord, u64, usize)>()) as u64,
            })?;
        Ok(Self {
            bitmap: zeroed(bitmap)?,
            slots: zeroed(slots)?,
            slot: zeroed(slot)?,
            order,
        })
    }

    /// Buffers that hold nothing, standing in while a call has taken the
    /// real ones; making them allocates nothing.
    const fn empty() -> Self {
        Self {
            bitmap: Vec::new(),
            slots: Vec::new(),
            slot: Vec::new(),
            order: Vec::new(),
        }
    }

    /// The largest batch of encoded cells a typed read of `geom` stages.
    fn payload_batch(geom: &Geometry) -> Result<Vec<u8>> {
        let mut bytes = 0;
        for block in &geom.blocks {
            let stride = crate::width::index(block.stride)?;
            bytes = bytes.max(batch_cells(stride.max(1), usize::MAX) * stride);
        }
        zeroed(bytes as u64)
    }
}

/// The first `len` bytes of a scratch buffer. It was sized for the matrix,
/// so this grows it only if an earlier call lost it to an error.
/// The most slot bytes [`TiledMatrix::write_run`] stages at once.
pub(crate) const RUN_PIECE_BYTES: usize = 64 * 1024;

fn sized(buffer: &mut Vec<u8>, len: u64) -> Result<&mut [u8]> {
    let len = crate::width::index(len)?;
    if buffer.len() < len {
        buffer
            .try_reserve_exact(len - buffer.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix cell buffer",
                requested: len as u64,
            })?;
        buffer.resize(len, 0);
    }
    Ok(&mut buffer[..len])
}

impl std::fmt::Debug for TiledMatrix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TiledMatrix")
            .field("dims", &self.geom.dims)
            .field("meta", &self.meta)
            .field("generation", &self.generation())
            .finish()
    }
}

/// The dimension per-channel flags count over: one named like a channel,
/// otherwise the second.
fn channel_dimension(spec: FormatSpec) -> usize {
    spec.matrix_dimensions
        .iter()
        .position(|dimension| {
            matches!(dimension.name, "ch" | "channel" | "channels" | "n_channels")
        })
        .unwrap_or(1)
}

/// The storage shape of `spec` with extents `dims` (in block dimension
/// order: rows, columns).
pub(crate) fn plan(
    spec: FormatSpec,
    dims: [u64; 2],
    all_dims: &[u64],
) -> Result<(Geometry, Names)> {
    let mut blocks = Vec::new();
    let mut names = Names {
        blocks: Vec::new(),
        singles: Vec::new(),
        per_channel: Vec::new(),
        aux: Vec::new(),
    };
    for block in spec.matrix_blocks {
        push_fallible(
            &mut blocks,
            BlockGeom {
                block_id: block.block_id,
                stride: block.slot_stride,
                tile: block.effective_tile(),
            },
            "matrix plan",
        )?;
        push_fallible(
            &mut names.blocks,
            (block.block_id, block.category),
            "matrix plan",
        )?;
    }
    let mut geom = Geometry {
        dims,
        growing: spec.growing_rows_per_chunk().is_some(),
        blocks,
        extra_len: 0,
    };
    let mut at = geom.extra_at();
    for commit in spec.matrix_commits {
        if commit.kind == MatrixCommitKind::Single {
            push_fallible(&mut names.singles, (commit.name, at), "matrix plan")?;
            at = at.checked_add(8).ok_or(Error::InvalidMatrixLayout)?;
        }
    }
    at = at
        .checked_next_multiple_of(EXTRA_PAGE)
        .ok_or(Error::InvalidMatrixLayout)?;
    let channels = all_dims
        .get(channel_dimension(spec))
        .copied()
        .unwrap_or(dims[1]);
    for commit in spec.matrix_commits {
        if commit.kind == MatrixCommitKind::PerChannel {
            push_fallible(
                &mut names.per_channel,
                (commit.name, at, channels),
                "matrix plan",
            )?;
            at = after_pages(at, channels.div_ceil(8), EXTRA_PAGE)
                .ok_or(Error::InvalidMatrixLayout)?;
        }
    }
    for aux in spec.matrix_aux {
        push_fallible(&mut names.aux, (aux.name, at, aux.byte_len), "matrix plan")?;
        at = after_pages(at, aux.byte_len, EXTRA_PAGE).ok_or(Error::InvalidMatrixLayout)?;
    }
    geom.extra_len = at - geom.extra_at();
    Ok((geom, names))
}

/// The extents a spec's dimensions take, in declaration order, from create.
pub(crate) fn dims_at_create(
    spec: FormatSpec,
    dims: &MatrixDimensions,
) -> Result<(Vec<u64>, [u64; 2])> {
    let mut values = Vec::new();
    for dimension in spec.matrix_dimensions {
        let value = dims
            .get(dimension.name)
            .ok_or_else(|| Error::MatrixDimensionMissing(dimension.name.to_string()))?;
        check_declared(spec, crate::format::ReadLimitKey::MatrixDimension, value)?;
        push_fallible(&mut values, value, "matrix dimensions")?;
    }
    let block = spec
        .matrix_blocks
        .first()
        .ok_or(Error::MatrixLayoutMissing)?;
    let extent = |name: &str| -> Result<u64> {
        spec.matrix_dimensions
            .iter()
            .position(|dimension| dimension.name == name)
            .map(|index| values[index])
            .ok_or_else(|| Error::MatrixDimensionMissing(name.to_string()))
    };
    let pair = [extent(block.dimensions[0])?, extent(block.dimensions[1])?];
    let cells = pair[0]
        .checked_mul(pair[1])
        .ok_or(Error::InvalidMatrixLayout)?;
    check_declared(spec, crate::format::ReadLimitKey::MatrixCells, cells)?;
    Ok((values, pair))
}

/// Checks `value` against a matrix limit the spec declares; a limit it does
/// not declare is no refusal. Every matrix size checked this way follows from
/// the block declarations (tile shape, stride) and format constants, so it
/// is bounded without one.
fn check_declared(spec: FormatSpec, key: crate::format::ReadLimitKey, value: u64) -> Result<()> {
    match spec.read_limits.check(key, value) {
        Err(Error::MissingResourceLimit { .. }) => Ok(()),
        other => other,
    }
}

impl TiledMatrix {
    /// Lays out a new matrix in `store`.
    pub(crate) fn create(
        spec: FormatSpec,
        dims: &MatrixDimensions,
        creation_id: [u8; 16],
        store: &mut dyn TileStore,
    ) -> Result<Self> {
        let (all, pair) = dims_at_create(spec, dims)?;
        let (geom, names) = plan(spec, pair, &all)?;
        // The ring and every tile are records, so the record payload limit
        // bounds them: a declared ring is checked against it, the default
        // shrinks to fit it.
        let payload_limit = match spec
            .read_limits
            .require(crate::format::ReadLimitKey::RecordPayloadLen)
        {
            Ok(limit) => limit.unwrap_or(u64::MAX),
            Err(Error::MissingResourceLimit { .. }) => u64::MAX,
            Err(error) => return Err(error),
        }
        .min(super::wal::MAX_RING_BYTES);
        // By default, room for two rewrites of every cell the declared
        // extents hold, within [256 KiB, 16 MiB]: a small matrix is not given
        // a ring many times its own size.
        let cell_bytes = geom.blocks.iter().fold(0u64, |total, block| {
            total.saturating_add(
                pair[0]
                    .saturating_mul(pair[1])
                    .saturating_mul(block.slot_len()),
            )
        });
        let ring = match spec.matrix_ring_bytes() {
            Some(declared) => declared,
            None => cell_bytes
                .saturating_mul(2)
                .clamp(MIN_DEFAULT_RING_BYTES, DEFAULT_RING_BYTES)
                .min(payload_limit),
        };
        if ring > super::wal::MAX_RING_BYTES {
            return Err(Error::LimitExceeded {
                resource: "matrix ring",
                actual: ring,
                limit: super::wal::MAX_RING_BYTES,
            });
        }
        check_declared(spec, crate::format::ReadLimitKey::RecordPayloadLen, ring)?;
        // The matrix limits, in what they bound for tiles: one tile's slots,
        // its commit bitmap and its checksums, and the metadata record.
        use crate::format::ReadLimitKey;
        for block in &geom.blocks {
            check_declared(spec, ReadLimitKey::RecordPayloadLen, block.tile_bytes())?;
            check_declared(
                spec,
                ReadLimitKey::MatrixSlotRegionLen,
                block.cells() * block.stride,
            )?;
            check_declared(spec, ReadLimitKey::MatrixBitmapBytes, block.bitmap_len())?;
            check_declared(spec, ReadLimitKey::MatrixCrcBytes, block.cells() * 4)?;
        }
        check_declared(spec, ReadLimitKey::MatrixMetadataBytes, geom.meta_len())?;
        let writer = TileWriter::create(store, geom.try_clone()?, ring, creation_id)?;
        // The block pair's extents are in the metadata record, so an open
        // needs no dims.
        let meta = writer.meta;
        let scratch = MatrixScratch::for_geometry(&geom)?;
        let payloads = MatrixScratch::payload_batch(&geom)?;
        Ok(Self {
            geom,
            names,
            engine: Engine::Writer(Box::new(writer)),
            meta,
            creation_id,
            verify_cells: spec.read_validation().data_crc,
            scratch: ReaderLock::new(scratch),
            payloads: ReaderLock::new(payloads),
        })
    }

    /// The metadata record of an existing matrix, read through `source`.
    fn opened_shape(
        spec: FormatSpec,
        source: &dyn TileSource,
        meta: u64,
    ) -> Result<(Geometry, Names, [u8; 16])> {
        let mut fixed = [0u8; crate::width::small(super::geometry::META_FIXED_LEN)];
        source.read_at(meta, &mut fixed)?;
        let pair = Geometry::decode_dims(&fixed);
        // Per-channel extents come from the channel dimension, which for the
        // block pair is one of the two stored.
        let mut all = Vec::new();
        let block = spec
            .matrix_blocks
            .first()
            .ok_or(Error::MatrixLayoutMissing)?;
        for dimension in spec.matrix_dimensions {
            push_fallible(
                &mut all,
                if dimension.name == block.dimensions[0] {
                    pair[0]
                } else {
                    pair[1]
                },
                "matrix dimensions",
            )?;
        }
        let (geom, names) = plan(spec, pair, &all)?;
        geom.decode_meta(&fixed)?;
        Ok((geom, names, Geometry::decode_creation_id(&fixed)))
    }

    pub(crate) fn open_writer(
        spec: FormatSpec,
        store: &mut dyn TileStore,
        meta: u64,
    ) -> Result<Self> {
        let (geom, names, creation_id) = Self::opened_shape(spec, store, meta)?;
        let writer = TileWriter::open(store, geom.try_clone()?, meta)?;
        let scratch = MatrixScratch::for_geometry(&geom)?;
        let payloads = MatrixScratch::payload_batch(&geom)?;
        Ok(Self {
            geom,
            names,
            engine: Engine::Writer(Box::new(writer)),
            meta,
            creation_id,
            verify_cells: spec.read_validation().data_crc,
            scratch: ReaderLock::new(scratch),
            payloads: ReaderLock::new(payloads),
        })
    }

    pub(crate) fn open_reader(
        spec: FormatSpec,
        source: &dyn TileSource,
        meta: u64,
    ) -> Result<Self> {
        let (geom, names, creation_id) = Self::opened_shape(spec, source, meta)?;
        let reader = TileReader::open(source, geom.try_clone()?, meta)?;
        let scratch = MatrixScratch::for_geometry(&geom)?;
        let payloads = MatrixScratch::payload_batch(&geom)?;
        Ok(Self {
            geom,
            names,
            engine: Engine::Reader(ReaderLock::new(reader)),
            meta,
            creation_id,
            verify_cells: spec.read_validation().data_crc,
            scratch: ReaderLock::new(scratch),
            payloads: ReaderLock::new(payloads),
        })
    }

    /// This handle's staging buffers, for the length of one call.
    fn scratch(&self) -> Result<super::reader_lock::ReaderGuard<'_, MatrixScratch>> {
        self.scratch.lock().map_err(|_| Error::InvalidMatrixLayout)
    }

    /// Runs `body` with `len` bytes to stage encoded cells in: this handle's
    /// batch buffer, or one made for the call when that is in use (a read
    /// re-entered from a codec) or was sized for less.
    pub(crate) fn with_payloads<R>(
        &self,
        len: usize,
        body: impl FnOnce(&mut [u8]) -> Result<R>,
    ) -> Result<R> {
        if let Some(mut kept) = self.payloads.try_lock() {
            return body(sized(&mut kept, len as u64)?);
        }
        body(&mut zeroed(len as u64)?)
    }

    /// The published generation this handle shows.
    pub(crate) fn generation(&self) -> u64 {
        match &self.engine {
            Engine::Writer(writer) => writer.engine.published(),
            Engine::Reader(reader) => reader.lock().map_or(0, |reader| reader.reader.snapshot()),
        }
    }

    pub(crate) fn is_writer(&self) -> bool {
        matches!(self.engine, Engine::Writer(_))
    }

    fn writer(&mut self) -> Result<&mut TileWriter> {
        match &mut self.engine {
            Engine::Writer(writer) => Ok(writer),
            Engine::Reader(_) => Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "file was opened read-only",
            ))),
        }
    }

    pub(crate) fn is_dirty(&self) -> bool {
        matches!(&self.engine, Engine::Writer(writer) if writer.engine.is_dirty())
    }

    // ---- publication ----

    pub(crate) fn publish(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        self.writer()?.engine.publish(store)
    }

    /// Moves a reader to the newest publication. `true` when it changed.
    pub(crate) fn follow(&mut self, source: &dyn TileSource) -> Result<bool> {
        match &mut self.engine {
            Engine::Writer(_) => Ok(false),
            Engine::Reader(reader) => reader
                .get_mut()
                .map_err(|_| Error::InvalidMatrixLayout)?
                .reader
                .follow(source),
        }
    }
}

/// Pushes onto a vector whose growth is reserved first: a refusal is an
/// error, never an abort.
fn push_fallible<T>(into: &mut Vec<T>, value: T, resource: &'static str) -> Result<()> {
    into.try_reserve(1).map_err(|_| Error::AllocationFailed {
        resource,
        requested: ((into.len() + 1) * std::mem::size_of::<T>()) as u64,
    })?;
    into.push(value);
    Ok(())
}

/// What a region pass reads through: a writer's own view, or a reader's
/// snapshot with validation deferred to the end of the pass.
trait RegionAccess {
    fn tile(&mut self, coord: TileCoord) -> Result<Option<u64>>;
    fn read(&mut self, at: u64, buf: &mut [u8]) -> Result<()>;
}

struct WriterAccess<'a> {
    writer: &'a TileWriter,
    source: &'a dyn TileSource,
    block: usize,
}

impl RegionAccess for WriterAccess<'_> {
    fn tile(&mut self, coord: TileCoord) -> Result<Option<u64>> {
        self.writer.find(self.source, self.block, coord)
    }
    fn read(&mut self, at: u64, buf: &mut [u8]) -> Result<()> {
        self.writer.engine.read(self.source, at, buf)
    }
}

struct ReaderAccess<'a> {
    reader: &'a mut TileReader,
    source: &'a dyn TileSource,
    block: usize,
    oldest: u64,
}

impl RegionAccess for ReaderAccess<'_> {
    fn tile(&mut self, coord: TileCoord) -> Result<Option<u64>> {
        if let Some(found) = self.reader.cached_tile(self.block, coord)? {
            return Ok(Some(found));
        }
        self.reader.tile(self.source, self.block, coord)
    }
    fn read(&mut self, at: u64, buf: &mut [u8]) -> Result<()> {
        let laid = self.reader.reader.read_unsettled(self.source, at, buf)?;
        self.oldest = self.oldest.min(laid);
        Ok(())
    }
}

/// Largest span of slots a batch key read fetches with one read.
const CELLS_SPAN_BYTES: u64 = 64 * 1024;
/// Unrequested slot bytes a batch key read reads through rather than
/// issuing another read.
const CELLS_GAP_BYTES: u64 = 4096;

/// End (exclusive) of the span of sorted keys starting at `at`: keys of
/// the same tile whose slots are at most `gap` apart, covering fewer than
/// `cap` slots.
fn span_end(order: &[(TileCoord, u64, usize)], at: usize, gap: u64, cap: u64) -> usize {
    let (coord, first, _) = order[at];
    let mut end = at + 1;
    while end < order.len()
        && order[end].0 == coord
        && order[end].1 - order[end - 1].1 <= gap
        && order[end].1 - first < cap
    {
        end += 1;
    }
    end
}

fn bit(unit: &[u8], slot: u64) -> bool {
    unit[(slot / 8) as usize] & (1 << (slot % 8)) != 0
}

fn zeroed(len: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(crate::width::index(len)?)
        .map_err(|_| Error::AllocationFailed {
            resource: "matrix cell buffer",
            requested: len,
        })?;
    bytes.resize(crate::width::index(len)?, 0);
    Ok(bytes)
}

// The implementation is split by concern across these files, included here
// so they share this module.
include!("api/flags.rs");
include!("api/read.rs");
include!("api/write.rs");
