// Writes of a [`TiledMatrix`]: cells and runs staged through the ring, and
// their commit bits.

// Included by `api.rs`, whose module it shares.

impl TiledMatrix {
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
        let tile = self
            .writer()?
            .tile(store, block, coord, true)?
            .ok_or(Error::InvalidMatrixLayout)?;
        self.with_scratch(|this, scratch| {
            let bytes = sized(&mut scratch.slot, geom.slot_len())?;
            bytes[..payload.len()].copy_from_slice(payload);
            let crc = crc32fast::hash(payload);
            bytes[payload.len()..].copy_from_slice(&crc.to_le_bytes());
            this.writer()?
                .engine
                .stage(store, tile + geom.slot_at(slot), bytes)?;
            this.set_bit(store, geom, tile, slot, false, &mut scratch.bitmap)
        })
    }

    /// Writes and commits a run of cells: `payloads` holds consecutive
    /// cells' payloads in storage order (row after row of the keyspace) from
    /// `start`. Equivalent to [`Self::write_cell`] then [`Self::commit_cell`]
    /// on each cell, but each stretch of the run that lies in one tile row is
    /// staged as one range, and each tile's commit bitmap is read and staged
    /// once for the stretches it holds. Slots stream through the handle's
    /// cell buffer in pieces of at most [`RUN_PIECE_BYTES`].
    pub(crate) fn write_run(
        &mut self,
        store: &mut dyn TileStore,
        block_id: u32,
        start: MatrixKey,
        payloads: &[u8],
    ) -> Result<()> {
        let block = self.block_index(block_id)?;
        let geom = self.geom.blocks[block];
        let stride = crate::width::index(geom.stride)?;
        if stride == 0 || !payloads.len().is_multiple_of(stride) {
            return Err(Error::MatrixSizeMismatch {
                expected: geom.stride,
                actual: payloads.len() as u64,
            });
        }
        let cells = (payloads.len() / stride) as u64;
        if cells == 0 {
            return Ok(());
        }
        let width = self.geom.dims[1];
        let first = start
            .scan
            .checked_mul(width)
            .and_then(|at| at.checked_add(start.ch))
            .ok_or(Error::InvalidMatrixLayout)?;
        let last = first + cells - 1;
        self.geom.check_key(start)?;
        self.geom
            .check_key(MatrixKey::new(last / width, last % width))?;
        let slot_len = crate::width::index(geom.slot_len())?;
        let piece_cells = (RUN_PIECE_BYTES / slot_len).max(1);
        self.with_scratch(|this, scratch| {
            // The tile whose bitmap is held in `scratch.bitmap`, changed and
            // not staged yet.
            let mut held: Option<u64> = None;
            let mut done = 0u64;
            while done < cells {
                let at = first + done;
                let key = MatrixKey::new(at / width, at % width);
                let (coord, slot) = geom.locate(key);
                // This stretch: the rest of the cell's tile row, of the
                // keyspace row and of the run.
                let in_tile_row = geom.tile[1] - key.ch % geom.tile[1];
                let in_row = width - key.ch;
                let stretch = in_tile_row.min(in_row).min(cells - done);
                let tile = this
                    .writer()?
                    .tile(store, block, coord, true)?
                    .ok_or(Error::InvalidMatrixLayout)?;
                if held.is_some_and(|held| held != tile) {
                    let previous = held.take().expect("held");
                    this.writer()?.engine.stage(
                        store,
                        previous + geom.bitmap_at(),
                        &scratch.bitmap[..crate::width::index(geom.bitmap_unit_len())?],
                    )?;
                }
                if held.is_none() {
                    this.bitmap_unit(store, geom, tile, &mut scratch.bitmap)?;
                    held = Some(tile);
                }
                let mut put = 0u64;
                while put < stretch {
                    let n = crate::width::index((stretch - put).min(piece_cells as u64))?;
                    let bytes = sized(&mut scratch.slot, (n * slot_len) as u64)?;
                    for (i, slot_bytes) in bytes.chunks_exact_mut(slot_len).enumerate() {
                        let cell = crate::width::index(done + put)? + i;
                        let payload = &payloads[cell * stride..(cell + 1) * stride];
                        slot_bytes[..stride].copy_from_slice(payload);
                        slot_bytes[stride..]
                            .copy_from_slice(&crc32fast::hash(payload).to_le_bytes());
                    }
                    this.writer()?
                        .engine
                        .stage(store, tile + geom.slot_at(slot + put), bytes)?;
                    put += n as u64;
                }
                let unit = &mut scratch.bitmap;
                for i in 0..stretch {
                    let bit_at = slot + i;
                    unit[(bit_at / 8) as usize] |= 1u8 << (bit_at % 8);
                }
                seal_bitmap(&mut unit[..crate::width::index(geom.bitmap_unit_len())?]);
                done += stretch;
            }
            if let Some(tile) = held {
                this.writer()?.engine.stage(
                    store,
                    tile + geom.bitmap_at(),
                    &scratch.bitmap[..crate::width::index(geom.bitmap_unit_len())?],
                )?;
            }
            Ok(())
        })
    }

    fn set_bit(
        &mut self,
        store: &mut dyn TileStore,
        geom: BlockGeom,
        tile: u64,
        slot: u64,
        value: bool,
        unit: &mut Vec<u8>,
    ) -> Result<()> {
        let unit = self.bitmap_unit(store, geom, tile, unit)?;
        if bit(unit, slot) == value {
            return Ok(());
        }
        let byte = (slot / 8) as usize;
        let mask = 1u8 << (slot % 8);
        if value {
            unit[byte] |= mask;
        } else {
            unit[byte] &= !mask;
        }
        seal_bitmap(unit);
        self.writer()?
            .engine
            .stage(store, tile + geom.bitmap_at(), unit)
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
        self.with_scratch(|this, scratch| {
            let bytes = sized(&mut scratch.slot, geom.slot_len())?;
            this.read(store, tile + geom.slot_at(slot), bytes)?;
            let (payload, crc) = bytes.split_at(crate::width::index(geom.stride)?);
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
            this.set_bit(store, geom, tile, slot, true, &mut scratch.bitmap)
        })
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
        self.with_scratch(|this, scratch| {
            this.set_bit(store, geom, tile, slot, false, &mut scratch.bitmap)
        })
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
        self.with_scratch(|this, scratch| {
            let mut cleared = 0u64;
            let mut from = 0u64;
            while let Some((key, tile)) = this.next_tile(store, block, from)? {
                // Unchecked: clearing is also how a damaged bitmap is
                // recovered.
                let unit = this.raw_bitmap_unit(store, geom, tile, &mut scratch.bitmap)?;
                let bits = crate::width::index(geom.bitmap_len())?;
                let set: u64 = unit[..bits]
                    .iter()
                    .map(|byte| u64::from(byte.count_ones()))
                    .sum();
                if set > 0 {
                    cleared += set;
                    unit[..bits].fill(0);
                    seal_bitmap(unit);
                    this.writer()?
                        .engine
                        .stage(store, tile + geom.bitmap_at(), unit)?;
                }
                let Some(next) = key.checked_add(1) else {
                    break;
                };
                from = next;
            }
            Ok(cleared)
        })
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
        self.with_scratch(|this, scratch| {
            let mut committed = 0u64;
            let mut from = 0u64;
            let slot_len = geom.slot_len();
            // Slots are checked a run at a time, through the slot buffer.
            let run = (scratch.slots.len() as u64 / slot_len).max(1);
            while let Some((key, tile)) = this.next_tile(store, block, from)? {
                // Unchecked: the bitmap is rebuilt whole, damaged or not.
                let unit = this.raw_bitmap_unit(store, geom, tile, &mut scratch.bitmap)?;
                let bits = crate::width::index(geom.bitmap_len())?;
                unit[..bits].fill(0);
                let mut first = 0u64;
                while first < geom.cells() {
                    let count = run.min(geom.cells() - first);
                    let slots = sized(&mut scratch.slots, count * slot_len)?;
                    this.read(store, tile + geom.slot_at(first), slots)?;
                    for (index, piece) in slots
                        .chunks_exact(crate::width::index(slot_len)?)
                        .enumerate()
                    {
                        let slot = first + index as u64;
                        let (payload, crc) = piece.split_at(crate::width::index(geom.stride)?);
                        if u32::from_le_bytes(crc.try_into().expect("four"))
                            == crc32fast::hash(payload)
                        {
                            unit[(slot / 8) as usize] |= 1 << (slot % 8);
                            committed += 1;
                        }
                    }
                    first += count;
                }
                seal_bitmap(unit);
                this.writer()?
                    .engine
                    .stage(store, tile + geom.bitmap_at(), unit)?;
                let Some(next) = key.checked_add(1) else {
                    break;
                };
                from = next;
            }
            Ok(committed)
        })
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
            // Counted a page of bits at a time.
            let len = channels.div_ceil(8);
            let mut page = [0u8; 256];
            let mut committed = 0u64;
            let mut done = 0u64;
            while done < len {
                let n = crate::width::bounded(len - done, page.len());
                self.read(source, self.meta + at + done, &mut page[..n])?;
                committed += page[..n]
                    .iter()
                    .map(|byte| u64::from(byte.count_ones()))
                    .sum::<u64>();
                done += n as u64;
                // Bits past the last channel are never set, but a damaged
                // byte must not count them.
                if done == len && channels % 8 != 0 {
                    let last = page[n - 1] >> (channels % 8);
                    committed -= u64::from(last.count_ones());
                }
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
        let mut scratch = self.scratch()?;
        let mut from = 0u64;
        while let Some((key, tile)) = self.next_tile(source, block, from)? {
            let unit = self.bitmap_unit(source, geom, tile, &mut scratch.bitmap)?;
            committed += unit[..crate::width::index(geom.bitmap_len())?]
                .iter()
                .map(|byte| u64::from(byte.count_ones()))
                .sum::<u64>();
            if self.geom.growing {
                let mut header = [0u8; 32];
                self.read(source, tile, &mut header)?;
                let row = u64::from_le_bytes(header[16..24].try_into().expect("eight"));
                // The header is read here without its checksum: a row it
                // names is only ever a larger extent, never an overflow.
                rows = rows.max(row.saturating_add(1).saturating_mul(geom.tile[0]));
            }
            let Some(next) = key.checked_add(1) else {
                break;
            };
            from = next;
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
}
