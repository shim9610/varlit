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

use super::geometry::{BlockGeom, Geometry, TileCoord, checked_bitmap, seal_bitmap};
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
    /// policy's `data_crc`. Commit bitmaps and tile headers are always
    /// checked: they decide which bytes are read at all.
    verify_cells: bool,
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
            at += 8;
        }
    }
    at = at.next_multiple_of(EXTRA_PAGE);
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
            at += channels.div_ceil(8).next_multiple_of(EXTRA_PAGE);
        }
    }
    for aux in spec.matrix_aux {
        push_fallible(&mut names.aux, (aux.name, at, aux.byte_len), "matrix plan")?;
        at += aux.byte_len.next_multiple_of(EXTRA_PAGE);
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
        Ok(Self {
            geom,
            names,
            engine: Engine::Writer(Box::new(writer)),
            meta,
            creation_id,
            verify_cells: spec.read_validation().data_crc,
        })
    }

    /// The metadata record of an existing matrix, read through `source`.
    fn opened_shape(
        spec: FormatSpec,
        source: &dyn TileSource,
        meta: u64,
    ) -> Result<(Geometry, Names, [u8; 16])> {
        let mut fixed = [0u8; super::geometry::META_FIXED_LEN as usize];
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
        Ok(Self {
            geom,
            names,
            engine: Engine::Writer(Box::new(writer)),
            meta,
            creation_id,
            verify_cells: spec.read_validation().data_crc,
        })
    }

    pub(crate) fn open_reader(
        spec: FormatSpec,
        source: &dyn TileSource,
        meta: u64,
    ) -> Result<Self> {
        let (geom, names, creation_id) = Self::opened_shape(spec, source, meta)?;
        let reader = TileReader::open(source, geom.try_clone()?, meta)?;
        Ok(Self {
            geom,
            names,
            engine: Engine::Reader(ReaderLock::new(reader)),
            meta,
            creation_id,
            verify_cells: spec.read_validation().data_crc,
        })
    }

    /// The published generation this handle shows.
    pub(crate) fn generation(&self) -> u64 {
        match &self.engine {
            Engine::Writer(writer) => writer.published(),
            Engine::Reader(reader) => reader.lock().map_or(0, |reader| reader.snapshot()),
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
        matches!(&self.engine, Engine::Writer(writer) if writer.is_dirty())
    }

    // ---- reading ----

    fn read(&self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        match &self.engine {
            Engine::Writer(writer) => writer.read(source, at, buf),
            Engine::Reader(reader) => reader
                .lock()
                .map_err(|_| Error::InvalidMatrixLayout)?
                .read(source, at, buf),
        }
    }

    fn find(&self, source: &dyn TileSource, block: usize, coord: TileCoord) -> Result<Option<u64>> {
        match &self.engine {
            Engine::Writer(writer) => writer.find(source, block, coord),
            Engine::Reader(reader) => reader
                .lock()
                .map_err(|_| Error::InvalidMatrixLayout)?
                .tile(source, block, coord),
        }
    }

    pub(crate) fn block_index(&self, block_id: u32) -> Result<usize> {
        self.names
            .blocks
            .iter()
            .position(|(id, _)| *id == block_id)
            .ok_or(Error::MatrixBlockMissing(block_id))
    }

    fn category_block(&self, category: &str) -> Result<usize> {
        self.names
            .blocks
            .iter()
            .position(|(_, name)| *name == category)
            .ok_or_else(|| Error::MatrixCommitMissing(category.to_string()))
    }

    pub(crate) fn category_of(&self, block_id: u32) -> Result<&'static str> {
        Ok(self.names.blocks[self.block_index(block_id)?].1)
    }

    /// A tile's checked commit bitmap unit.
    fn bitmap_unit(&self, source: &dyn TileSource, geom: BlockGeom, tile: u64) -> Result<Vec<u8>> {
        let mut unit = zeroed(geom.bitmap_unit_len())?;
        self.read(source, tile + geom.bitmap_at(), &mut unit)?;
        checked_bitmap(&unit)?;
        Ok(unit)
    }

    /// A tile's commit bitmap unit as stored, checksum not checked.
    fn raw_bitmap_unit(
        &self,
        source: &dyn TileSource,
        geom: BlockGeom,
        tile: u64,
    ) -> Result<Vec<u8>> {
        let mut unit = zeroed(geom.bitmap_unit_len())?;
        self.read(source, tile + geom.bitmap_at(), &mut unit)?;
        Ok(unit)
    }

    /// Where `key`'s slot is, if its tile exists.
    pub(crate) fn slot_of(
        &self,
        source: &dyn TileSource,
        block_id: u32,
        key: MatrixKey,
    ) -> Result<Option<(BlockGeom, u64, u64)>> {
        self.geom.check_key(key)?;
        let block = self.block_index(block_id)?;
        let geom = self.geom.blocks[block];
        let (coord, slot) = geom.locate(key);
        Ok(self
            .find(source, block, coord)?
            .map(|tile| (geom, tile, slot)))
    }

    pub(crate) fn cell_status(
        &self,
        source: &dyn TileSource,
        block_id: u32,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        self.geom.check_key(key)?;
        let block = self.block_index(block_id)?;
        let geom = self.geom.blocks[block];
        let (coord, slot) = geom.locate(key);
        self.with_access(source, block, &mut |access| {
            let Some(tile) = access.tile(coord)? else {
                return Ok(MatrixCellStatus::NotCommitted);
            };
            // Allocated after the lookup: a refused tile-cache entry is
            // tolerated, so a refusal here must come later to be reported.
            let mut bitmap = zeroed(geom.bitmap_unit_len())?;
            access.read(tile + geom.bitmap_at(), &mut bitmap)?;
            checked_bitmap(&bitmap)?;
            Ok(if bit(&bitmap, slot) {
                MatrixCellStatus::Committed
            } else {
                MatrixCellStatus::NotCommitted
            })
        })
    }

    /// The committed payload of `key` into `out` (exactly the stride). A
    /// reader validates its snapshot once for the call.
    pub(crate) fn read_cell(
        &self,
        source: &dyn TileSource,
        block_id: u32,
        key: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        self.geom.check_key(key)?;
        let block = self.block_index(block_id)?;
        let geom = self.geom.blocks[block];
        if out.len() as u64 != geom.stride {
            return Err(Error::MatrixSizeMismatch {
                expected: geom.stride,
                actual: out.len() as u64,
            });
        }
        let (coord, slot) = geom.locate(key);
        self.with_access(source, block, &mut |access| {
            let tile = access.tile(coord)?.ok_or(Error::MatrixNotCommitted)?;
            let mut bitmap = zeroed(geom.bitmap_unit_len())?;
            let mut piece = zeroed(geom.slot_len())?;
            access.read(tile + geom.bitmap_at(), &mut bitmap)?;
            checked_bitmap(&bitmap)?;
            if !bit(&bitmap, slot) {
                return Err(Error::MatrixNotCommitted);
            }
            let at = tile + geom.slot_at(slot);
            access.read(at, &mut piece)?;
            self.check_slot(&piece, geom, at)?;
            out.copy_from_slice(&piece[..geom.stride as usize]);
            Ok(())
        })
    }

    /// Checks a slot's payload against its CRC (when cells are verified).
    fn check_slot(&self, piece: &[u8], geom: BlockGeom, at: u64) -> Result<()> {
        let (payload, crc) = piece.split_at(geom.stride as usize);
        let expected = u32::from_le_bytes(crc.try_into().expect("four"));
        let actual = crc32fast::hash(payload);
        if self.verify_cells && expected != actual {
            return Err(Error::MatrixChecksumMismatch {
                offset: at,
                expected,
                actual,
            });
        }
        Ok(())
    }

    /// Committed payloads of `count` cells along row `key.scan` from
    /// `key.ch`, into `out` (`count * stride` bytes). Every cell must be
    /// committed. Reads one tile row segment at a time; a reader validates
    /// its snapshot once for the call.
    pub(crate) fn read_row_run(
        &self,
        source: &dyn TileSource,
        block_id: u32,
        key: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        let block = self.block_index(block_id)?;
        let geom = self.geom.blocks[block];
        let stride = geom.stride as usize;
        let count = (out.len() / stride) as u64;
        if count == 0 {
            return Ok(());
        }
        self.geom.check_key(key)?;
        self.geom
            .check_key(MatrixKey::new(key.scan, key.ch + count - 1))?;
        let mut slot_bytes = zeroed(geom.slot_len() * geom.tile[1].min(count))?;
        let mut bitmap = zeroed(geom.bitmap_unit_len())?;
        self.with_access(source, block, &mut |access| {
            let mut done = 0u64;
            while done < count {
                let at = MatrixKey::new(key.scan, key.ch + done);
                let (coord, slot) = geom.locate(at);
                let in_tile = (geom.tile[1] - at.ch % geom.tile[1]).min(count - done);
                let tile = access.tile(coord)?.ok_or(Error::MatrixNotCommitted)?;
                access.read(tile + geom.bitmap_at(), &mut bitmap)?;
                checked_bitmap(&bitmap)?;
                if (0..in_tile).any(|i| !bit(&bitmap, slot + i)) {
                    return Err(Error::MatrixNotCommitted);
                }
                let bytes = &mut slot_bytes[..(in_tile * geom.slot_len()) as usize];
                let first = tile + geom.slot_at(slot);
                access.read(first, bytes)?;
                for (i, piece) in bytes.chunks_exact(geom.slot_len() as usize).enumerate() {
                    self.check_slot(piece, geom, first + i as u64 * geom.slot_len())?;
                    let to = (done as usize + i) * stride;
                    out[to..to + stride].copy_from_slice(&piece[..stride]);
                }
                done += in_tile;
            }
            Ok(())
        })
    }

    /// Reads committed cells of `block_id` in `region`, row-major from
    /// `start`, into `out` (whole strides only). Returns how many cells were
    /// filled and, when the region goes on, the key to continue from.
    ///
    /// A reader validates its snapshot once for the whole call, not once
    /// per cell, and reads each tile row segment (its bitmap, then its slots)
    /// with one read each.
    pub(crate) fn read_region(
        &self,
        source: &dyn TileSource,
        block_id: u32,
        region: crate::MatrixRegion,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<(usize, Option<MatrixKey>)> {
        let block = self.block_index(block_id)?;
        let geom = self.geom.blocks[block];
        if region.is_empty() || !region.contains(start) {
            return Err(Error::MatrixKeyOutOfBounds {
                scan: start.scan,
                ch: start.ch,
            });
        }
        self.geom.check_key(start)?;
        self.geom
            .check_key(MatrixKey::new(region.row_end - 1, region.col_end - 1))?;
        let mut scratch = zeroed(geom.tile[1] * geom.slot_len())?;
        let mut bitmap = zeroed(geom.bitmap_unit_len())?;
        self.with_access(source, block, &mut |access| {
            self.region_pass(access, geom, region, start, out, &mut scratch, &mut bitmap)
        })
    }

    /// Runs `pass` against this engine's view of `block`. A writer reads
    /// its own state; a reader runs the pass against its snapshot and
    /// validates the snapshot once at the end, running the pass again (up
    /// to eight times) when the ring moved under it.
    fn with_access<R>(
        &self,
        source: &dyn TileSource,
        block: usize,
        pass: &mut dyn FnMut(&mut dyn RegionAccess) -> Result<R>,
    ) -> Result<R> {
        match &self.engine {
            Engine::Writer(writer) => pass(&mut WriterAccess {
                writer,
                source,
                block,
            }),
            Engine::Reader(reader) => {
                let mut reader = reader.lock().map_err(|_| Error::InvalidMatrixLayout)?;
                for _ in 0..8 {
                    let mut access = ReaderAccess {
                        reader: &mut reader,
                        source,
                        block,
                        oldest: u64::MAX,
                    };
                    let result = pass(&mut access);
                    let oldest = access.oldest;
                    // Settle even after an error: a damaged-looking cell read
                    // while the ring moved is read again, not reported.
                    if reader.settle(source, oldest)? {
                        return result;
                    }
                }
                Err(Error::MatrixSnapshotExpired {
                    snapshot: reader.snapshot(),
                    checkpointed: 0,
                })
            }
        }
    }

    /// Reads the committed payloads of `keys` (any order, repeats allowed)
    /// into `out`, key `i` at `out[i * stride..]`. Every key must be in
    /// bounds and committed.
    ///
    /// The keys are visited grouped by tile: each tile's bitmap is read
    /// once, and slots close together in a tile are read with one read
    /// (gaps up to [`CELLS_GAP_BYTES`] are read through rather than split).
    /// A reader validates its snapshot once for the whole call. The sort
    /// order costs 32 bytes per key, allocated fallibly.
    pub(crate) fn read_cells(
        &self,
        source: &dyn TileSource,
        block_id: u32,
        keys: &[MatrixKey],
        out: &mut [u8],
    ) -> Result<()> {
        let block = self.block_index(block_id)?;
        let geom = self.geom.blocks[block];
        let expected = (keys.len() as u64)
            .checked_mul(geom.stride)
            .ok_or(Error::InvalidMatrixLayout)?;
        if out.len() as u64 != expected {
            return Err(Error::MatrixSizeMismatch {
                expected,
                actual: out.len() as u64,
            });
        }
        if keys.is_empty() {
            return Ok(());
        }
        let mut order: Vec<(TileCoord, u64, usize)> = Vec::new();
        order
            .try_reserve_exact(keys.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix key batch order",
                requested: (keys.len() * std::mem::size_of::<(TileCoord, u64, usize)>()) as u64,
            })?;
        for (index, key) in keys.iter().enumerate() {
            self.geom.check_key(*key)?;
            let (coord, slot) = geom.locate(*key);
            order.push((coord, slot, index));
        }
        order.sort_unstable_by_key(|(coord, slot, index)| (coord.row, coord.col, *slot, *index));
        let span_cap = (CELLS_SPAN_BYTES / geom.slot_len()).clamp(1, geom.cells());
        let gap = (CELLS_GAP_BYTES / geom.slot_len()).max(1);
        // Scratch for the widest span these keys make, not the cap.
        let mut widest = 1u64;
        let mut at = 0usize;
        while at < order.len() {
            let end = span_end(&order, at, gap, span_cap);
            widest = widest.max(order[end - 1].1 - order[at].1 + 1);
            at = end;
        }
        let mut scratch = zeroed(widest * geom.slot_len())?;
        let mut bitmap = zeroed(geom.bitmap_unit_len())?;
        self.with_access(source, block, &mut |access| {
            self.cells_pass(access, geom, &order, out, &mut scratch, &mut bitmap)
        })
    }

    fn cells_pass(
        &self,
        access: &mut dyn RegionAccess,
        geom: BlockGeom,
        order: &[(TileCoord, u64, usize)],
        out: &mut [u8],
        scratch: &mut [u8],
        bitmap: &mut [u8],
    ) -> Result<()> {
        let stride = geom.stride as usize;
        let slot_len = geom.slot_len();
        let span_cap = (CELLS_SPAN_BYTES / slot_len).clamp(1, geom.cells());
        let gap = (CELLS_GAP_BYTES / slot_len).max(1);
        let mut at = 0usize;
        while at < order.len() {
            let coord = order[at].0;
            let tile = access.tile(coord)?.ok_or(Error::MatrixNotCommitted)?;
            access.read(tile + geom.bitmap_at(), bitmap)?;
            checked_bitmap(bitmap)?;
            // Every key of this tile, as spans of nearby slots.
            while at < order.len() && order[at].0 == coord {
                let first_slot = order[at].1;
                let end = span_end(order, at, gap, span_cap);
                let span = order[end - 1].1 - first_slot + 1;
                if order[at..end]
                    .iter()
                    .any(|(_, slot, _)| !bit(bitmap, *slot))
                {
                    return Err(Error::MatrixNotCommitted);
                }
                let first = tile + geom.slot_at(first_slot);
                let bytes = &mut scratch[..(span * slot_len) as usize];
                access.read(first, bytes)?;
                for (_, slot, index) in &order[at..end] {
                    let from = ((slot - first_slot) * slot_len) as usize;
                    let piece = &bytes[from..from + slot_len as usize];
                    self.check_slot(piece, geom, first + from as u64)?;
                    out[index * stride..(index + 1) * stride].copy_from_slice(&piece[..stride]);
                }
                at = end;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn region_pass(
        &self,
        access: &mut dyn RegionAccess,
        geom: BlockGeom,
        region: crate::MatrixRegion,
        start: MatrixKey,
        out: &mut [u8],
        scratch: &mut [u8],
        bitmap: &mut [u8],
    ) -> Result<(usize, Option<MatrixKey>)> {
        let stride = geom.stride as usize;
        let capacity = out.len() / stride;
        let mut key = start;
        let mut filled = 0usize;
        let mut bitmap_of: Option<u64> = None;
        while filled < capacity {
            let (coord, slot) = geom.locate(key);
            let run = (geom.tile[1] - key.ch % geom.tile[1])
                .min(region.col_end - key.ch)
                .min((capacity - filled) as u64);
            let tile = access.tile(coord)?.ok_or(Error::MatrixNotCommitted)?;
            if bitmap_of != Some(tile) {
                access.read(tile + geom.bitmap_at(), bitmap)?;
                checked_bitmap(bitmap)?;
                bitmap_of = Some(tile);
            }
            if (0..run).any(|i| !bit(bitmap, slot + i)) {
                return Err(Error::MatrixNotCommitted);
            }
            let first = tile + geom.slot_at(slot);
            let bytes = &mut scratch[..(run * geom.slot_len()) as usize];
            access.read(first, bytes)?;
            for i in 0..run as usize {
                let piece =
                    &bytes[i * geom.slot_len() as usize..(i + 1) * geom.slot_len() as usize];
                self.check_slot(piece, geom, first + i as u64 * geom.slot_len())?;
                out[(filled + i) * stride..(filled + i + 1) * stride]
                    .copy_from_slice(&piece[..stride]);
            }
            filled += run as usize;
            key.ch += run;
            if key.ch == region.col_end {
                key = MatrixKey::new(key.scan + 1, region.col_start);
                if key.scan == region.row_end {
                    return Ok((filled, None));
                }
            }
        }
        Ok((filled, Some(key)))
    }

    // ---- writing ----

    /// Stores `payload` as `key`'s slot and clears its commit bit: a
    /// rewritten cell is invisible until committed again.
    pub(crate) fn write_cell(
        &mut self,
        store: &mut dyn TileStore,
        block_id: u32,
        key: MatrixKey,
        payload: &[u8],
    ) -> Result<()> {
        self.geom.check_key(key)?;
        let block = self.block_index(block_id)?;
        let geom = self.geom.blocks[block];
        if payload.len() as u64 != geom.stride {
            return Err(Error::MatrixSizeMismatch {
                expected: geom.stride,
                actual: payload.len() as u64,
            });
        }
        let (coord, slot) = geom.locate(key);
        let writer = self.writer()?;
        let tile = writer
            .tile(store, block, coord, true)?
            .ok_or(Error::InvalidMatrixLayout)?;
        let mut bytes = zeroed(geom.slot_len())?;
        bytes[..payload.len()].copy_from_slice(payload);
        let crc = crc32fast::hash(payload);
        bytes[payload.len()..].copy_from_slice(&crc.to_le_bytes());
        writer.stage(store, tile + geom.slot_at(slot), &bytes)?;
        self.set_bit(store, geom, tile, slot, false)
    }

    fn set_bit(
        &mut self,
        store: &mut dyn TileStore,
        geom: BlockGeom,
        tile: u64,
        slot: u64,
        value: bool,
    ) -> Result<()> {
        let mut unit = self.bitmap_unit(store, geom, tile)?;
        if bit(&unit, slot) == value {
            return Ok(());
        }
        let byte = (slot / 8) as usize;
        let mask = 1u8 << (slot % 8);
        if value {
            unit[byte] |= mask;
        } else {
            unit[byte] &= !mask;
        }
        seal_bitmap(&mut unit);
        self.writer()?.stage(store, tile + geom.bitmap_at(), &unit)
    }

    /// Sets `key`'s commit bit. The cell must hold a written slot.
    pub(crate) fn commit_cell(
        &mut self,
        store: &mut dyn TileStore,
        block_id: u32,
        key: MatrixKey,
    ) -> Result<()> {
        let Some((geom, tile, slot)) = self.slot_of(store, block_id, key)? else {
            return Err(Error::MatrixCellNotWritten);
        };
        let mut bytes = zeroed(geom.slot_len())?;
        self.read(store, tile + geom.slot_at(slot), &mut bytes)?;
        let (payload, crc) = bytes.split_at(geom.stride as usize);
        let stored = u32::from_le_bytes(crc.try_into().expect("four"));
        if stored != crc32fast::hash(payload) {
            // All zero: never written, or cleared. Anything else: damaged.
            return Err(if bytes.iter().all(|byte| *byte == 0) {
                Error::MatrixCellNotWritten
            } else {
                Error::MatrixChecksumMismatch {
                    offset: tile + geom.slot_at(slot),
                    expected: stored,
                    actual: crc32fast::hash(payload),
                }
            });
        }
        self.set_bit(store, geom, tile, slot, true)
    }

    /// Clears `key`'s commit bit. Its bytes stay, so committing it again
    /// without a new write publishes the value it held.
    pub(crate) fn clear_cell(
        &mut self,
        store: &mut dyn TileStore,
        block_id: u32,
        key: MatrixKey,
    ) -> Result<()> {
        let Some((geom, tile, slot)) = self.slot_of(store, block_id, key)? else {
            return Ok(());
        };
        self.set_bit(store, geom, tile, slot, false)
    }

    pub(crate) fn clear_cell_by_category(
        &mut self,
        store: &mut dyn TileStore,
        category: &str,
        key: MatrixKey,
    ) -> Result<()> {
        let block_id = self.names.blocks[self.category_block(category)?].0;
        self.clear_cell(store, block_id, key)
    }

    /// Clears every commit bit of `category`. Returns how many were set.
    pub(crate) fn clear_category(
        &mut self,
        store: &mut dyn TileStore,
        category: &str,
    ) -> Result<u64> {
        let block = self.category_block(category)?;
        let geom = self.geom.blocks[block];
        let tiles = self.tiles_of(store, block)?;
        let mut cleared = 0u64;
        for tile in tiles {
            // Unchecked: clearing is also how a damaged bitmap is recovered.
            let mut unit = self.raw_bitmap_unit(store, geom, tile)?;
            let set: u64 = unit[..geom.bitmap_len() as usize]
                .iter()
                .map(|byte| u64::from(byte.count_ones()))
                .sum();
            if set == 0 {
                continue;
            }
            cleared += set;
            let bits = geom.bitmap_len() as usize;
            unit[..bits].fill(0);
            seal_bitmap(&mut unit);
            self.writer()?
                .stage(store, tile + geom.bitmap_at(), &unit)?;
        }
        Ok(cleared)
    }

    /// Sets the commit bit of every written cell of `block_id` whose
    /// checksum holds. Returns how many cells are committed afterwards.
    pub(crate) fn rebuild_commit_from_crc(
        &mut self,
        store: &mut dyn TileStore,
        block_id: u32,
    ) -> Result<u64> {
        let block = self.block_index(block_id)?;
        let geom = self.geom.blocks[block];
        let tiles = self.tiles_of(store, block)?;
        let mut committed = 0u64;
        let mut slots = zeroed(geom.cells() * geom.slot_len())?;
        for tile in tiles {
            self.read(store, tile + geom.slots_at(), &mut slots)?;
            // Unchecked: the bitmap is rebuilt whole, damaged or not.
            let mut unit = self.raw_bitmap_unit(store, geom, tile)?;
            let bits = geom.bitmap_len() as usize;
            unit[..bits].fill(0);
            for slot in 0..geom.cells() {
                let piece = &slots
                    [(slot * geom.slot_len()) as usize..((slot + 1) * geom.slot_len()) as usize];
                let (payload, crc) = piece.split_at(geom.stride as usize);
                if u32::from_le_bytes(crc.try_into().expect("four")) == crc32fast::hash(payload) {
                    unit[(slot / 8) as usize] |= 1 << (slot % 8);
                    committed += 1;
                }
            }
            seal_bitmap(&mut unit);
            self.writer()?
                .stage(store, tile + geom.bitmap_at(), &unit)?;
        }
        Ok(committed)
    }

    fn tiles_of(&mut self, store: &mut dyn TileStore, block: usize) -> Result<Vec<u64>> {
        let mut tiles = Vec::new();
        let mut push = |tile: u64| -> Result<()> {
            tiles.try_reserve(1).map_err(|_| Error::AllocationFailed {
                resource: "matrix tile list",
                requested: (tiles.len() as u64 + 1) * 8,
            })?;
            tiles.push(tile);
            Ok(())
        };
        self.writer()?.for_each_tile(&*store, block, &mut push)?;
        Ok(tiles)
    }

    /// Committed and total of any commit category, for resume signals: the
    /// cells of a cell category, the channels of a per-channel one, the one
    /// flag of a single one.
    pub(crate) fn progress(&self, source: &dyn TileSource, category: &str) -> Result<(u64, u64)> {
        if self.names.singles.iter().any(|(name, _)| *name == category) {
            return Ok((u64::from(self.is_single_committed(source, category)?), 1));
        }
        if let Some((_, at, channels)) = self
            .names
            .per_channel
            .iter()
            .find(|(name, ..)| *name == category)
        {
            let mut bits = zeroed(channels.div_ceil(8))?;
            self.read(source, self.meta + at, &mut bits)?;
            let mut committed: u64 = bits.iter().map(|byte| u64::from(byte.count_ones())).sum();
            // Bits past the last channel are never set, but a damaged byte
            // must not count them.
            if channels % 8 != 0 {
                let last = bits[bits.len() - 1] >> (channels % 8);
                committed -= u64::from(last.count_ones());
            }
            return Ok((committed, *channels));
        }
        self.cell_progress(source, category)
    }

    fn cell_progress(&self, source: &dyn TileSource, category: &str) -> Result<(u64, u64)> {
        let block = self.category_block(category)?;
        let geom = self.geom.blocks[block];
        let mut committed = 0u64;
        let mut rows = self.geom.dims[0];
        let mut visit = |tile: u64| -> Result<()> {
            let unit = self.bitmap_unit(source, geom, tile)?;
            committed += unit[..geom.bitmap_len() as usize]
                .iter()
                .map(|byte| u64::from(byte.count_ones()))
                .sum::<u64>();
            if self.geom.growing {
                let mut header = [0u8; 32];
                self.read(source, tile, &mut header)?;
                let row = u64::from_le_bytes(header[16..24].try_into().expect("eight"));
                rows = rows.max((row + 1) * geom.tile[0]);
            }
            Ok(())
        };
        match &self.engine {
            Engine::Writer(writer) => {
                writer.for_each_tile_ref(source, block, &mut visit)?;
            }
            Engine::Reader(reader) => {
                let mut tiles = Vec::new();
                reader
                    .lock()
                    .map_err(|_| Error::InvalidMatrixLayout)?
                    .for_each_tile(source, block, &mut |tile| {
                        push_fallible(&mut tiles, tile, "matrix tile list")
                    })?;
                for tile in tiles {
                    visit(tile)?;
                }
            }
        }
        let total = rows
            .checked_mul(self.geom.dims[1])
            .ok_or(Error::InvalidMatrixLayout)?;
        Ok((committed, total))
    }

    pub(crate) fn resume_signal(
        &self,
        source: &dyn TileSource,
        category: &str,
    ) -> Result<MatrixResumeSignal> {
        let (committed, total) = self.progress(source, category)?;
        Ok(if committed == 0 || committed == total {
            MatrixResumeSignal::Clean
        } else {
            MatrixResumeSignal::Partial { committed, total }
        })
    }

    // ---- flags and aux ----

    fn single_at(&self, name: &str) -> Result<u64> {
        self.names
            .singles
            .iter()
            .find(|(single, _)| *single == name)
            .map(|(_, at)| self.meta + at)
            .ok_or_else(|| Error::MatrixCommitMissing(name.to_string()))
    }

    pub(crate) fn is_single_committed(&self, source: &dyn TileSource, name: &str) -> Result<bool> {
        let mut unit = [0u8; 8];
        self.read(source, self.single_at(name)?, &mut unit)?;
        Ok(unit[0] != 0)
    }

    pub(crate) fn set_single_committed(
        &mut self,
        store: &mut dyn TileStore,
        name: &str,
        value: bool,
    ) -> Result<()> {
        let at = self.single_at(name)?;
        let mut unit = [0u8; 8];
        unit[0] = u8::from(value);
        self.writer()?.stage(store, at, &unit)
    }

    fn channel_at(&self, name: &str, channel: u64) -> Result<(u64, u8)> {
        let (_, at, channels) = self
            .names
            .per_channel
            .iter()
            .find(|(commit, ..)| *commit == name)
            .ok_or_else(|| Error::MatrixCommitMissing(name.to_string()))?;
        if channel >= *channels {
            return Err(Error::MatrixCommitMissing(name.to_string()));
        }
        Ok((self.meta + at + channel / 8, 1 << (channel % 8)))
    }

    pub(crate) fn is_channel_committed(
        &self,
        source: &dyn TileSource,
        name: &str,
        channel: u64,
    ) -> Result<bool> {
        let (at, mask) = self.channel_at(name, channel)?;
        let mut byte = [0u8; 1];
        self.read(source, at, &mut byte)?;
        Ok(byte[0] & mask != 0)
    }

    pub(crate) fn set_channel_committed(
        &mut self,
        store: &mut dyn TileStore,
        name: &str,
        channel: u64,
        value: bool,
    ) -> Result<()> {
        let (at, mask) = self.channel_at(name, channel)?;
        self.rewrite_extra(store, at, 1, &mut |bytes| {
            if value {
                bytes[0] |= mask;
            } else {
                bytes[0] &= !mask;
            }
        })
    }

    fn aux(&self, name: &str) -> Result<(u64, u64)> {
        self.names
            .aux
            .iter()
            .find(|(aux, ..)| *aux == name)
            .map(|(_, at, len)| (self.meta + at, *len))
            .ok_or_else(|| Error::MatrixAuxMissing(name.to_string()))
    }

    pub(crate) fn aux_len(&self, name: &str) -> Result<u64> {
        Ok(self.aux(name)?.1)
    }

    fn aux_range(&self, name: &str, offset: u64, len: u64) -> Result<u64> {
        let (at, aux_len) = self.aux(name)?;
        if offset.checked_add(len).is_none_or(|end| end > aux_len) {
            return Err(Error::MatrixAuxOutOfBounds {
                name: name.to_string(),
                offset,
                len,
                byte_len: aux_len,
            });
        }
        Ok(at + offset)
    }

    pub(crate) fn read_aux(
        &self,
        source: &dyn TileSource,
        name: &str,
        offset: u64,
        out: &mut [u8],
    ) -> Result<()> {
        let at = self.aux_range(name, offset, out.len() as u64)?;
        self.read(source, at, out)
    }

    pub(crate) fn write_aux(
        &mut self,
        store: &mut dyn TileStore,
        name: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<()> {
        let at = self.aux_range(name, offset, bytes.len() as u64)?;
        self.rewrite_extra(store, at, bytes.len() as u64, &mut |region| {
            region.copy_from_slice(bytes);
        })
    }

    /// Rewrites `[at, at + len)` of the metadata area page by page: each
    /// touched [`EXTRA_PAGE`] is read, changed through `edit` (given exactly
    /// the requested range once all pages are gathered), and staged whole.
    fn rewrite_extra(
        &mut self,
        store: &mut dyn TileStore,
        at: u64,
        len: u64,
        edit: &mut dyn FnMut(&mut [u8]),
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let base = self.meta + self.geom.extra_at();
        let first = base + (at - base) / EXTRA_PAGE * EXTRA_PAGE;
        let end = base + (at + len - base).div_ceil(EXTRA_PAGE) * EXTRA_PAGE;
        let mut pages = zeroed(end - first)?;
        self.read(store, first, &mut pages)?;
        edit(&mut pages[(at - first) as usize..(at - first + len) as usize]);
        let writer = self.writer()?;
        for (index, page) in pages.chunks(EXTRA_PAGE as usize).enumerate() {
            writer.stage(store, first + index as u64 * EXTRA_PAGE, page)?;
        }
        Ok(())
    }

    /// Checks every tile's header and commit bitmap. A damaged one is a
    /// `Recoverable` finding recommending its category be cleared.
    pub(crate) fn verify(&self, source: &dyn TileSource) -> Result<crate::MatrixRecoveryReport> {
        let mut report = crate::MatrixRecoveryReport {
            findings: Vec::new(),
            recommended_actions: Vec::new(),
        };
        for (block, (_, category)) in self.names.blocks.iter().enumerate() {
            let geom = self.geom.blocks[block];
            let mut tiles = Vec::new();
            match &self.engine {
                Engine::Writer(writer) => writer.for_each_tile_ref(source, block, &mut |tile| {
                    push_fallible(&mut tiles, tile, "matrix tile list")
                })?,
                Engine::Reader(reader) => reader
                    .lock()
                    .map_err(|_| Error::InvalidMatrixLayout)?
                    .for_each_tile(source, block, &mut |tile| {
                        push_fallible(&mut tiles, tile, "matrix tile list")
                    })?,
            }
            let mut damaged = false;
            for tile in tiles {
                let mut header = [0u8; super::geometry::TILE_HEADER_LEN as usize];
                self.read(source, tile, &mut header)?;
                let row = u64::from_le_bytes(header[16..24].try_into().expect("eight"));
                let col = u64::from_le_bytes(header[24..32].try_into().expect("eight"));
                let ok = geom
                    .check_tile_header(TileCoord { row, col }, &header)
                    .is_ok()
                    && self.bitmap_unit(source, geom, tile).is_ok();
                if !ok {
                    damaged = true;
                    report
                        .findings
                        .try_reserve(1)
                        .map_err(|_| Error::AllocationFailed {
                            resource: "matrix recovery report",
                            requested: 64,
                        })?;
                    report.findings.push(crate::MatrixRecoveryFinding {
                        kind: crate::MatrixCorruptionKind::CommitMap,
                        severity: crate::MatrixCorruptionSeverity::Recoverable,
                        message: format!("tile at offset {tile} of category {category} is damaged"),
                    });
                }
            }
            if damaged {
                report
                    .recommended_actions
                    .try_reserve(1)
                    .map_err(|_| Error::AllocationFailed {
                        resource: "matrix recovery report",
                        requested: 64,
                    })?;
                report
                    .recommended_actions
                    .push(crate::MatrixRecoveryAction::ClearCategory {
                        category: (*category).to_string(),
                    });
            }
        }
        Ok(report)
    }

    /// Every range this handle's snapshot has in the ring instead of in
    /// place: `(target, ring offset, len)`.
    #[cfg(feature = "mmap")]
    pub(crate) fn overlay_entries(&self) -> Result<Vec<(u64, u64, u32)>> {
        let mut out = Vec::new();
        let mut push = |target: u64, entry: super::view::OverlayEntry| -> Result<()> {
            push_fallible(
                &mut out,
                (target, entry.at, entry.len),
                "matrix overlay list",
            )
        };
        match &self.engine {
            Engine::Writer(writer) => writer.for_each_overlay(&mut push)?,
            Engine::Reader(reader) => reader
                .lock()
                .map_err(|_| Error::InvalidMatrixLayout)?
                .for_each_overlay(&mut push)?,
        }
        Ok(out)
    }

    // ---- publication ----

    pub(crate) fn publish(&mut self, store: &mut dyn TileStore) -> Result<u64> {
        self.writer()?.publish(store)
    }

    /// Moves a reader to the newest publication. `true` when it changed.
    pub(crate) fn follow(&mut self, source: &dyn TileSource) -> Result<bool> {
        match &mut self.engine {
            Engine::Writer(_) => Ok(false),
            Engine::Reader(reader) => reader
                .get_mut()
                .map_err(|_| Error::InvalidMatrixLayout)?
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
        self.writer.read(self.source, at, buf)
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
        let laid = self.reader.read_unsettled(self.source, at, buf)?;
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
        .try_reserve_exact(len as usize)
        .map_err(|_| Error::AllocationFailed {
            resource: "matrix cell buffer",
            requested: len,
        })?;
    bytes.resize(len as usize, 0);
    Ok(bytes)
}
