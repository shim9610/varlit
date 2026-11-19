// Reads of a [`TiledMatrix`]: cells, key batches and regions, against a
// writer's own state or a reader's validated snapshot.

// Included by `api.rs`, whose module it shares.

impl TiledMatrix {
    fn read(&self, source: &dyn TileSource, at: u64, buf: &mut [u8]) -> Result<()> {
        match &self.engine {
            Engine::Writer(writer) => writer.engine.read(source, at, buf),
            Engine::Reader(reader) => reader
                .lock()
                .map_err(|_| Error::InvalidMatrixLayout)?
                .reader
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

    /// A tile's checked commit bitmap unit, read into `unit`.
    fn bitmap_unit<'u>(
        &self,
        source: &dyn TileSource,
        geom: BlockGeom,
        tile: u64,
        unit: &'u mut Vec<u8>,
    ) -> Result<&'u mut [u8]> {
        let unit = self.raw_bitmap_unit(source, geom, tile, unit)?;
        checked_bitmap(unit)?;
        Ok(unit)
    }

    /// A tile's commit bitmap unit as stored, checksum not checked, read
    /// into `unit`.
    fn raw_bitmap_unit<'u>(
        &self,
        source: &dyn TileSource,
        geom: BlockGeom,
        tile: u64,
        unit: &'u mut Vec<u8>,
    ) -> Result<&'u mut [u8]> {
        let unit = sized(unit, geom.bitmap_unit_len())?;
        self.read(source, tile + geom.bitmap_at(), unit)?;
        Ok(unit)
    }

    /// Runs `body` with this handle's buffers taken out of it, so the body
    /// can use the handle mutably; they are put back after.
    fn with_scratch<R>(
        &mut self,
        body: impl FnOnce(&mut Self, &mut MatrixScratch) -> Result<R>,
    ) -> Result<R> {
        let held = self
            .scratch
            .get_mut()
            .map_err(|_| Error::InvalidMatrixLayout)?;
        let mut scratch = std::mem::replace(held, MatrixScratch::empty());
        let result = body(self, &mut scratch);
        if let Ok(held) = self.scratch.get_mut() {
            *held = scratch;
        }
        result
    }

    /// The first tile of `block` at Morton key `from` or later, as this
    /// handle sees the directory.
    fn next_tile(
        &self,
        source: &dyn TileSource,
        block: usize,
        from: u64,
    ) -> Result<Option<(u64, u64)>> {
        let root_at = self.meta + self.geom.root_at(block);
        super::view::next_tile(root_at, from, &mut |at, buf| self.read(source, at, buf))
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
        let mut scratch = self.scratch()?;
        self.with_access(source, block, &mut |access| {
            let Some(tile) = access.tile(coord)? else {
                return Ok(MatrixCellStatus::NotCommitted);
            };
            let bitmap = sized(&mut scratch.bitmap, geom.bitmap_unit_len())?;
            access.read(tile + geom.bitmap_at(), bitmap)?;
            checked_bitmap(bitmap)?;
            Ok(if bit(bitmap, slot) {
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
        let mut scratch = self.scratch()?;
        let scratch = &mut *scratch;
        self.with_access(source, block, &mut |access| {
            let tile = access.tile(coord)?.ok_or(Error::MatrixNotCommitted)?;
            let bitmap = sized(&mut scratch.bitmap, geom.bitmap_unit_len())?;
            let piece = sized(&mut scratch.slots, geom.slot_len())?;
            access.read(tile + geom.bitmap_at(), bitmap)?;
            checked_bitmap(bitmap)?;
            if !bit(bitmap, slot) {
                return Err(Error::MatrixNotCommitted);
            }
            let at = tile + geom.slot_at(slot);
            access.read(at, piece)?;
            self.check_slot(piece, geom, at)?;
            out.copy_from_slice(&piece[..crate::width::index(geom.stride)?]);
            Ok(())
        })
    }

    /// Checks a slot's payload against its CRC (when cells are verified).
    fn check_slot(&self, piece: &[u8], geom: BlockGeom, at: u64) -> Result<()> {
        let (payload, crc) = piece.split_at(crate::width::index(geom.stride)?);
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
        let stride = crate::width::index(geom.stride)?;
        let count = (out.len() / stride) as u64;
        if count == 0 {
            return Ok(());
        }
        self.geom.check_key(key)?;
        self.geom
            .check_key(MatrixKey::new(key.scan, key.ch + count - 1))?;
        let mut scratch = self.scratch()?;
        let scratch = &mut *scratch;
        let slot_bytes = sized(
            &mut scratch.slots,
            geom.slot_len() * geom.tile[1].min(count),
        )?;
        let bitmap = sized(&mut scratch.bitmap, geom.bitmap_unit_len())?;
        self.with_access(source, block, &mut |access| {
            let mut done = 0u64;
            while done < count {
                let at = MatrixKey::new(key.scan, key.ch + done);
                let (coord, slot) = geom.locate(at);
                let in_tile = geom.row_run(at).min(count - done);
                let tile = access.tile(coord)?.ok_or(Error::MatrixNotCommitted)?;
                access.read(tile + geom.bitmap_at(), bitmap)?;
                checked_bitmap(bitmap)?;
                if (0..in_tile).any(|i| !bit(bitmap, slot + i)) {
                    return Err(Error::MatrixNotCommitted);
                }
                let bytes = &mut slot_bytes[..crate::width::index(in_tile * geom.slot_len())?];
                let first = tile + geom.slot_at(slot);
                access.read(first, bytes)?;
                for (i, piece) in bytes
                    .chunks_exact(crate::width::index(geom.slot_len())?)
                    .enumerate()
                {
                    self.check_slot(piece, geom, first + i as u64 * geom.slot_len())?;
                    let to = (crate::width::index(done)? + i) * stride;
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
        let mut scratch = self.scratch()?;
        let scratch = &mut *scratch;
        let slots = sized(&mut scratch.slots, geom.tile[1] * geom.slot_len())?;
        let bitmap = sized(&mut scratch.bitmap, geom.bitmap_unit_len())?;
        self.with_access(source, block, &mut |access| {
            let buffers = PassBuffers {
                slots: &mut *slots,
                bitmap: &mut *bitmap,
            };
            self.region_pass(access, geom, region, start, out, buffers)
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
                    if reader.reader.settle(source, oldest)? {
                        return result;
                    }
                }
                Err(Error::MatrixSnapshotExpired {
                    snapshot: reader.reader.snapshot(),
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
        let mut scratch = self.scratch()?;
        let scratch = &mut *scratch;
        for first in (0..keys.len()).step_by(ORDER_KEYS) {
            let last = (first + ORDER_KEYS).min(keys.len());
            self.read_cells_part(source, block, keys, first..last, out, scratch)?;
        }
        Ok(())
    }

    /// [`Self::read_cells`] for the keys in `part`, ordered by tile in the
    /// order buffer, which holds them all.
    fn read_cells_part(
        &self,
        source: &dyn TileSource,
        block: usize,
        keys: &[MatrixKey],
        part: std::ops::Range<usize>,
        out: &mut [u8],
        scratch: &mut MatrixScratch,
    ) -> Result<()> {
        let geom = self.geom.blocks[block];
        let order = &mut scratch.order;
        order.clear();
        if order.capacity() < part.len() {
            return Err(Error::InvalidMatrixLayout);
        }
        for index in part {
            let key = keys[index];
            self.geom.check_key(key)?;
            let (coord, slot) = geom.locate(key);
            order.push((coord, slot, index));
        }
        order.sort_unstable_by_key(|(coord, slot, index)| (coord.row, coord.col, *slot, *index));
        let span_cap = (CELLS_SPAN_BYTES / geom.slot_len()).clamp(1, geom.cells());
        let gap = (CELLS_GAP_BYTES / geom.slot_len()).max(1);
        // Scratch for the widest span these keys make, not the cap.
        let mut widest = 1u64;
        let mut at = 0usize;
        while at < order.len() {
            let end = span_end(order, at, gap, span_cap);
            widest = widest.max(order[end - 1].1 - order[at].1 + 1);
            at = end;
        }
        let slots = sized(&mut scratch.slots, widest * geom.slot_len())?;
        let bitmap = sized(&mut scratch.bitmap, geom.bitmap_unit_len())?;
        self.with_access(source, block, &mut |access| {
            let buffers = PassBuffers {
                slots: &mut *slots,
                bitmap: &mut *bitmap,
            };
            self.cells_pass(access, geom, order, out, buffers)
        })
    }

    fn cells_pass(
        &self,
        access: &mut dyn RegionAccess,
        geom: BlockGeom,
        order: &[(TileCoord, u64, usize)],
        out: &mut [u8],
        buffers: PassBuffers<'_>,
    ) -> Result<()> {
        let PassBuffers {
            slots: scratch,
            bitmap,
        } = buffers;
        let stride = crate::width::index(geom.stride)?;
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
                let bytes = &mut scratch[..crate::width::index(span * slot_len)?];
                access.read(first, bytes)?;
                for (_, slot, index) in &order[at..end] {
                    let from = crate::width::index((slot - first_slot) * slot_len)?;
                    let piece = &bytes[from..from + crate::width::index(slot_len)?];
                    self.check_slot(piece, geom, first + from as u64)?;
                    out[index * stride..(index + 1) * stride].copy_from_slice(&piece[..stride]);
                }
                at = end;
            }
        }
        Ok(())
    }

    fn region_pass(
        &self,
        access: &mut dyn RegionAccess,
        geom: BlockGeom,
        region: crate::MatrixRegion,
        start: MatrixKey,
        out: &mut [u8],
        buffers: PassBuffers<'_>,
    ) -> Result<(usize, Option<MatrixKey>)> {
        let PassBuffers {
            slots: scratch,
            bitmap,
        } = buffers;
        let stride = crate::width::index(geom.stride)?;
        let capacity = out.len() / stride;
        let mut key = start;
        let mut filled = 0usize;
        let mut bitmap_of: Option<u64> = None;
        while filled < capacity {
            let (coord, slot) = geom.locate(key);
            let run = geom
                .row_run(key)
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
            let bytes = &mut scratch[..crate::width::index(run * geom.slot_len())?];
            access.read(first, bytes)?;
            for i in 0..crate::width::index(run)? {
                let piece = &bytes[i * crate::width::index(geom.slot_len())?
                    ..(i + 1) * crate::width::index(geom.slot_len())?];
                self.check_slot(piece, geom, first + i as u64 * geom.slot_len())?;
                out[(filled + i) * stride..(filled + i + 1) * stride]
                    .copy_from_slice(&piece[..stride]);
            }
            filled += crate::width::index(run)?;
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
}
