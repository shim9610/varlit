// Native file matrix access operations.

impl VarlitFile {
    /// The two gates every region path passes before addressing a cell.
    ///
    /// `ensure_fatal_access_allowed` is the layout-wide stop, and
    /// `ensure_commit_publishable` is the per-category quarantine. The chunk
    /// path consulted neither, so a file whose matrix had been fenced off was
    /// still readable and writable through any chunked row.
    fn ensure_chunk_access_allowed(&self, category: &str) -> Result<()> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::ensure_chunk_access_allowed(matrix, category)
    }

    /// The whole of what a region path does before it addresses a cell, for a
    /// typed chunk path: validate `T` against the file's schema, then gate on
    /// **the spec's** category for `T::ID`.
    ///
    /// Both halves were missing on the read side, and the fix for the write
    /// side was itself incomplete: it gated on `T::CATEGORY`, a constant the
    /// caller declares, so the type under inspection chose which quarantine
    /// applied to it. `ensure_matrix_block` returns the descriptor's category,
    /// which is the one the file's schema hash covers.
    fn ensure_chunk_block_access<T: VarlitMatrixBlock>(&self) -> Result<&'static str> {
        let category = crate::matrix::ensure_matrix_block::<T>(self.spec)?;
        self.ensure_chunk_access_allowed(category)?;
        Ok(category)
    }

    /// The same gate for the paths that address a block by id rather than by
    /// type — `clear_matrix_cell_by_category` reaches the chunk that way, and
    /// went through no gate at all.
    fn ensure_chunk_access_by_block_id(&self, block_id: u32) -> Result<()> {
        let category = self
            .spec
            .matrix_blocks
            .iter()
            .find(|block| block.block_id == block_id)
            .ok_or(Error::MatrixBlockMissing(block_id))?
            .category;
        self.ensure_chunk_access_allowed(category)
    }

    /// Which chunk a row belongs to, and its row inside that chunk.
    ///
    /// `None` when no dimension grows, or when the row is inside the declared
    /// extent — chunk 0 is the matrix region, so those rows take the path they
    /// always took and this feature is invisible to them.
    fn chunk_for_row(&self, row: u64) -> Option<(u64, u64)> {
        let rows = self.spec.growing_rows_per_chunk()?;
        let chunk = row / rows;
        if chunk == 0 {
            return None;
        }
        Some((chunk, row % rows))
    }

    /// Cells per row for one matrix block, i.e. the extent of dimension 1.
    fn chunk_row_width(&self, block: &crate::format::MatrixBlockDescriptor) -> Result<u64> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        matrix
            .dimension(block.dimensions[1])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[1].to_string()))
    }

    /// Opens chunk `index`, writing the chunk whatever chunk was open before it.
    ///
    /// Refuses a chunk older than the open one. That refusal is the whole
    /// contract of writing the chunk: a value that arrives late is reported, not dropped.
    /// Makes chunk `index` the open one, loading it back from its record when
    /// one exists.
    ///
    /// **Both halves of this used to be a refusal.** Writing a row that belonged
    /// to an already-written chunk — or to any chunk older than the open one —
    /// returned `MatrixChunkClosed`, which made a written chunk permanently
    /// unmodifiable and, worse, made the rows of a chunk that was merely
    /// *skipped* permanently unwritable. A reader-writer that can edit a written
    /// region row could not edit a written chunk row, for no reason in the
    /// format: a chunk record is an ordinary record and `RecordFile` has had a
    /// route for rewriting one of those in place since round 12.
    ///
    /// The cost of the reload, stated: the chunk record's commit map, checksum
    /// table and slot region are read into the buffer — one chunk's worth of
    /// bytes, the same ceiling `rows_per_chunk` already sets on holding one
    /// open, so the memory bound is unchanged. It is a cold path (a chunk
    /// transition, not a record), and a format that never revisits an old row
    /// never takes it.
    fn open_chunk_at(&mut self, index: u64) -> Result<()> {
        match &self.open_chunk {
            Some(open) if open.index == index => return Ok(()),
            Some(_) => self.write_open_chunk_record()?,
            None => {}
        }
        if let Some(locator) = self.chunk_directory()?.find(index) {
            return self.reopen_written_chunk(locator);
        }
        let rows = self
            .spec
            .growing_rows_per_chunk()
            .ok_or(Error::MatrixLayoutMissing)?;
        let first_row = index
            .checked_mul(rows)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "matrix chunk first row",
            })?;
        let cell_crc = !matches!(self.spec.integrity_policy, IntegrityPolicy::None);
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(self.spec.matrix_blocks.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix chunk blocks",
                requested: self.spec.matrix_blocks.len() as u64,
            })?;
        for descriptor in self.spec.matrix_blocks {
            let width = self.chunk_row_width(descriptor)?;
            let cells = rows
                .checked_mul(width)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk cells",
                })?;
            self.spec
                .read_limits
                .check(ReadLimitKey::MatrixCells, cells)?;
            let slot_len = cells.checked_mul(descriptor.slot_stride).ok_or(
                Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk slot region",
                },
            )?;
            self.spec
                .read_limits
                .check(ReadLimitKey::MatrixSlotRegionLen, slot_len)?;
            let commit_len = cells.div_ceil(8);
            blocks.push(OpenChunkBlock {
                block_id: descriptor.block_id,
                stride: descriptor.slot_stride,
                cells,
                written: try_zeroed_vec(commit_len, "matrix chunk write map")?,
                commit: try_zeroed_vec(commit_len, "matrix chunk commit map")?,
                crc: if cell_crc {
                    try_zeroed_vec(
                        cells.checked_mul(MATRIX_CHUNK_CRC_LEN).ok_or(
                            Error::ResourceArithmeticOverflow {
                                resource: "matrix chunk checksum table",
                            },
                        )?,
                        "matrix chunk checksum table",
                    )?
                } else {
                    Vec::new()
                },
                slots: try_zeroed_vec(slot_len, "matrix chunk slot region")?,
            });
        }
        self.open_chunk = Some(OpenChunk {
            index,
            first_row,
            rows,
            compression: self.spec.chunk_compression,
            blocks,
            dirty: false,
            backing: None,
        });
        Ok(())
    }

    /// Loads a written chunk's record back into the buffer as the open chunk.
    ///
    /// Every geometry field was already checked against the spec by
    /// `build_chunk_directory` (rows, first row, block count, checksum policy,
    /// compression policy) and by `chunk_block_location` (per-block stride and
    /// cell count), so this reads bytes rather than re-deriving trust.
    ///
    /// **`written` is not on disk, and this is what that costs.** The record
    /// carries the commit map, the checksums and the slot bytes; the
    /// write-this-session bitmap is buffer state, so it cannot be restored. The
    /// buffer is seeded the way the region answers the same question — a cell
    /// counts as written if it is committed, or if its slot bytes are not all
    /// zero (`matrix::commit_cell`). The one case that differs from an
    /// uninterrupted session: a cell written with an all-zero value and never
    /// committed reads as unwritten after a reload, so committing it needs the
    /// value written again. That is exactly the region's behaviour for the same
    /// cell, and the alternative — persisting a second bitmap — would grow every
    /// chunk record for a distinction that only a discarded write can observe.
    fn reopen_written_chunk(&mut self, locator: ChunkLocator) -> Result<()> {
        if locator.compressed_slots {
            return Err(Error::MatrixChunkNotReopenable {
                chunk: locator.index,
                reason: "chunk compression",
            });
        }
        if self.spec.index_policy.segment_on_flush {
            return Err(Error::MatrixChunkNotReopenable {
                chunk: locator.index,
                reason: "segment_on_flush",
            });
        }
        let rows = self
            .spec
            .growing_rows_per_chunk()
            .ok_or(Error::MatrixLayoutMissing)?;
        let first_row =
            locator
                .index
                .checked_mul(rows)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "matrix chunk first row",
                })?;
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(self.spec.matrix_blocks.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix chunk blocks",
                requested: self.spec.matrix_blocks.len() as u64,
            })?;
        for descriptor in self.spec.matrix_blocks {
            let location = self
                .chunk_block_location(
                    locator.record_offset,
                    locator.payload_len,
                    locator.block_count,
                    locator.cell_crc,
                    false,
                    descriptor.block_id,
                )?
                // A block this spec declares and the record does not: the
                // record was written by a different layout, and every cell
                // address derived from it would name another block's bytes.
                .ok_or(Error::InvalidMatrixChunk)?;
            let cells = location.cells;
            self.spec
                .read_limits
                .check(ReadLimitKey::MatrixCells, cells)?;
            let slot_len =
                cells
                    .checked_mul(location.stride)
                    .ok_or(Error::ResourceArithmeticOverflow {
                        resource: "matrix chunk slot region",
                    })?;
            self.spec
                .read_limits
                .check(ReadLimitKey::MatrixSlotRegionLen, slot_len)?;
            // The stored commit map, at its stored length. Re-encoding it at
            // the canonical `cells.div_ceil(8)` would be a different number of
            // bytes for a record whose map is longer than that — which
            // `chunk_block_location` permits, checking only that it covers the
            // cells — and the rewrite must reproduce the payload length exactly.
            let commit_len = location
                .crc_offset
                .unwrap_or(location.slots_offset)
                .checked_sub(location.commit_offset)
                .ok_or(Error::InvalidMatrixChunk)?;
            let mut commit = try_zeroed_vec(commit_len, "matrix chunk commit map")?;
            self.snapshot
                .read_exact_at(location.commit_offset, &mut commit)?;
            note_chunk_bytes_read(commit_len);
            let crc = match location.crc_offset {
                Some(crc_offset) => {
                    let crc_len = cells.checked_mul(MATRIX_CHUNK_CRC_LEN).ok_or(
                        Error::ResourceArithmeticOverflow {
                            resource: "matrix chunk checksum table",
                        },
                    )?;
                    let mut crc = try_zeroed_vec(crc_len, "matrix chunk checksum table")?;
                    self.snapshot.read_exact_at(crc_offset, &mut crc)?;
                    note_chunk_bytes_read(crc_len);
                    crc
                }
                None => Vec::new(),
            };
            let mut slots = try_zeroed_vec(slot_len, "matrix chunk slot region")?;
            self.snapshot
                .read_exact_at(location.slots_offset, &mut slots)?;
            note_chunk_bytes_read(slot_len);
            let written = seed_written_map(&commit, &slots, cells, location.stride)?;
            blocks.push(OpenChunkBlock {
                block_id: descriptor.block_id,
                stride: location.stride,
                cells,
                written,
                commit,
                crc,
                slots,
            });
        }
        self.open_chunk = Some(OpenChunk {
            index: locator.index,
            first_row,
            rows,
            compression: None,
            blocks,
            // The buffer equals the record it came from. Writing it out now
            // would rewrite a record with its own bytes, and an idle flush
            // between a reopen and the first edit must stay a no-op.
            dirty: false,
            backing: Some(locator.ordinal),
        });
        Ok(())
    }

    /// Writes the open chunk as one ordinary internal record.
    ///
    /// **The chunk is released only after the record is on disk.** The first
    /// version `take()`d it up front and then ran three fallible steps — the
    /// encode, the payload ceiling, the append — so any failure destroyed the
    /// data *and* left `open_chunk` empty, which made a retried `flush()`
    /// return `Ok(())` for a chunk that no longer existed anywhere. Silent loss
    /// reported as success.
    ///
    /// **A chunk with no committed cell stays open rather than being dropped.**
    /// It was dropped before, so `write` → `flush` → `commit` lost the write on
    /// a chunked row while working on a region row, and the row could become
    /// permanently unwritable once a later chunk written past it. There is
    /// nothing to publish — a reader cannot see an uncommitted cell — so
    /// holding it costs a commit point nothing, and an idle flush still writes
    /// no record, which is the property this guard was for.
    fn write_open_chunk_record(&mut self) -> Result<()> {
        let (payload, index, block_count, cell_crc, compressed_slots) = {
            let Some(chunk) = self.open_chunk.as_ref() else {
                return Ok(());
            };
            if !chunk.dirty {
                return Ok(());
            }
            (
                chunk.encode()?,
                chunk.index,
                u32::try_from(chunk.blocks.len()).map_err(|_| {
                    Error::ResourceArithmeticOverflow {
                        resource: "matrix chunk block count",
                    }
                })?,
                chunk.blocks.iter().any(|block| !block.crc.is_empty()),
                chunk.compression.is_some(),
            )
        };
        let payload_len = payload.len() as u64;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
        // Taken from the append itself, not from a second `metadata()` call:
        // the two would disagree if anything landed between them.
        self.check_write()?;
        // Append a new immutable version; confirmed EOF controls its visibility.
        let info = self.ensure_write()?.write_record_with_prev_key(
            MATRIX_CHUNK_BLOCK_ID,
            MATRIX_CHUNK_VERSION,
            RECORD_FLAG_INTERNAL,
            0,
            &payload,
            None,
        )?;
        let record_offset = info.record_offset;
        // Extend the directory rather than invalidate it: a writer writing the chunk its
        // millionth chunk must not pay a rebuild, and the newest entry is what
        // answers "which chunks are written" on the next write.
        //
        // `get_mut`, never `chunk_directory()`. Asking for the directory here
        // *builds* it, and building it needs a growing-matrix spec — so a
        // format that declares none, which can still reach this through a
        // hand-built chunk, got `InvalidMatrixChunk` out of a write that had
        // already succeeded. The ordinal is only needed to back the buffer
        // below, and a handle that has no directory yet has not looked one up:
        // it will build it on the next chunk access and find this record there.
        let mut ordinal = None;
        if let Some(directory) = self.chunk_directory.get_mut() {
            let at = directory.next_ordinal();
            ordinal = Some(at);
            directory.note_written(ChunkLocator {
                index,
                record_offset,
                payload_len,
                block_count,
                cell_crc,
                compressed_slots,
                ordinal: at,
            });
        }
        // **On disk, and the buffer stays.** Releasing it here lost data, and
        // the loss was invisible: the write bitmap is buffer state that no
        // record carries, so a chunk released at a flush and reloaded at the
        // next commit came back with `written` rebuilt from "is the slot
        // non-zero" — and a cell holding a legitimate all-zero value therefore
        // read as never written. `write` → `flush` → `commit` on such a cell
        // returned `MatrixCellNotWritten` on a chunked row while succeeding on
        // the identical region row (measured 2026-08-09), which is the exact
        // defect a zero-valued cell was already fixed for once this round.
        //
        // Keeping it is also cheaper — no reload on the next edit — and costs
        // nothing in memory, because one chunk buffer is the ceiling this design
        // already sets. It is released on a transition to a different chunk
        // index, which is the only point where a second buffer would be needed.
        //
        // The backing ordinal addresses the newest appended chunk version.
        // Keep session-only write evidence with the buffer across flushes.
        // Normal chunk access builds the directory before opening the buffer.
        match ordinal {
            Some(at) => {
                if let Some(chunk) = self.open_chunk.as_mut() {
                    chunk.dirty = false;
                    chunk.backing = Some(at);
                }
            }
            None => self.open_chunk = None,
        }
        Ok(())
    }

    /// Whether the open chunk holds anything the file does not.
    ///
    /// The question `open_chunk.is_some()` used to stand in for, and stopped
    /// answering when a written chunk began staying open: a clean backed chunk
    /// is byte-for-byte its record, so a handle that dropped now would lose
    /// nothing.
    fn open_chunk_is_unwritten(&self) -> bool {
        self.open_chunk
            .as_ref()
            .is_some_and(|chunk| chunk.dirty || chunk.backing.is_none())
    }

    fn chunk_block_slice<T: VarlitMatrixBlock>(
        &mut self,
        chunk_index: u64,
        local_row: u64,
        key: MatrixKey,
    ) -> Result<(usize, u64)> {
        // Every region path goes through these before touching a cell and the
        // chunk path went through none of them: a quarantined category stayed
        // writable through a chunked row, a layout whose fatal-access gate had
        // fired was still addressable, and `T` was never checked against the
        // file's schema at all.
        self.ensure_chunk_block_access::<T>()?;
        self.open_chunk_at(chunk_index)?;
        let width = self
            .spec
            .matrix_blocks
            .iter()
            .find(|block| block.block_id == T::ID)
            .ok_or(Error::MatrixBlockMissing(T::ID))
            .and_then(|block| self.chunk_row_width(block))?;
        if key.ch >= width {
            return Err(Error::MatrixKeyOutOfBounds {
                scan: key.scan,
                ch: key.ch,
            });
        }
        let ordinal = local_row
            .checked_mul(width)
            .and_then(|base| base.checked_add(key.ch))
            .ok_or(Error::InvalidMatrixLayout)?;
        let chunk = self.open_chunk.as_ref().ok_or(Error::InvalidMatrixChunk)?;
        let position = chunk
            .block_position(T::ID)
            .ok_or(Error::MatrixBlockMissing(T::ID))?;
        Ok((position, ordinal))
    }

    /// Writes one already-encoded cell into the open chunk.
    ///
    /// The shared tail of both write entry points: `write_matrix_cell` encodes
    /// and calls this, `write_matrix_cell_payload` calls it directly. Keeping
    /// one body is the point — the payload entry point spent its first version
    /// unrouted, refusing a chunked row with `MatrixKeyOutOfBounds` while
    /// `write_matrix_cell` accepted the same key.
    fn write_chunk_cell_payload<T: VarlitMatrixBlock>(
        &mut self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
        payload: &[u8],
    ) -> Result<()> {
        let (position, ordinal) = self.chunk_block_slice::<T>(chunk_index, local_row, key)?;
        let chunk = self.open_chunk.as_mut().ok_or(Error::InvalidMatrixChunk)?;
        let block = &mut chunk.blocks[position];
        if payload.len() as u64 != block.stride {
            return Err(Error::MatrixSizeMismatch {
                expected: block.stride,
                actual: payload.len() as u64,
            });
        }
        let start = usize::try_from(
            ordinal
                .checked_mul(block.stride)
                .ok_or(Error::InvalidMatrixChunk)?,
        )
        .map_err(|_| Error::InvalidMatrixLayout)?;
        block.slots[start..start + payload.len()].copy_from_slice(payload);
        if !block.crc.is_empty() {
            let at = usize::try_from(
                ordinal
                    .checked_mul(MATRIX_CHUNK_CRC_LEN)
                    .ok_or(Error::InvalidMatrixChunk)?,
            )
            .map_err(|_| Error::InvalidMatrixLayout)?;
            let checksum = crc32_bytes(payload)?;
            block.crc[at..at + 4].copy_from_slice(&checksum.to_le_bytes());
        }
        let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
        block.written[byte] |= 1u8 << (ordinal % 8);
        // A-7. A write, not just a commit, now makes the buffer differ from the
        // file, and a chunk that is not `dirty` is dropped on the next chunk
        // transition — so without this a write into a reloaded chunk vanished
        // silently, which is strictly worse than the refusal it replaced.
        //
        // Widening `dirty` to "has content" was tried before reopening existed
        // and had to be reverted: it made an uncommitted write force the chunk
        // out, and a chunk that had gone out could never be opened again, so a
        // `write` → `flush` sequence *locked the rest of its rows forever*. That
        // is no longer the case — a written chunk comes back — which is what
        // makes this the honest meaning rather than a trap.
        chunk.dirty = true;
        Ok(())
    }

    pub fn write_matrix_cell<T: VarlitMatrixBlock>(
        &mut self,
        key: MatrixKey,
        value: &T,
    ) -> Result<()> {
        let _permit = self.ensure_write()?;
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            let (position, ordinal) = self.chunk_block_slice::<T>(chunk_index, local_row, key)?;
            let stride = self
                .open_chunk
                .as_ref()
                .ok_or(Error::InvalidMatrixChunk)?
                .blocks[position]
                .stride;
            // Bounded exactly as the region path bounds it : one byte
            // past the stride is enough to report the true length, and no
            // further buffering is possible whatever the encode does.
            let encoded = match crate::codec::encode_to_vec_limited(
                value,
                T::ENDIAN.unwrap_or(self.spec.endian),
                stride.saturating_add(1),
                "matrix chunk slot payload",
            ) {
                Ok(encoded) => encoded,
                Err(Error::LimitExceeded { actual, .. }) => {
                    return Err(Error::MatrixSizeMismatch {
                        expected: stride,
                        actual,
                    });
                }
                Err(error) => return Err(error),
            };
            // One body, shared with `write_matrix_cell_payload`. This branch
            // used to keep its own copy of the store, and when the write
            // bitmap arrived only the shared one learned to set it — so the
            // typed write silently stopped marking cells written.
            let _ = (position, ordinal);
            return self.write_chunk_cell_payload::<T>(key, chunk_index, local_row, &encoded);
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::write_cell(self.spec, matrix, self.file.matrix_region(), key, value)
        };
        self.finish_matrix_mutation(result)
    }

    pub fn write_matrix_cell_payload<T: VarlitMatrixBlock>(
        &mut self,
        key: MatrixKey,
        payload: &[u8],
    ) -> Result<()> {
        let _permit = self.ensure_write()?;
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            return self.write_chunk_cell_payload::<T>(key, chunk_index, local_row, payload);
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::write_cell_payload::<T>(
                self.spec,
                matrix,
                self.file.matrix_region(),
                key,
                payload,
            )
        };
        self.finish_matrix_mutation(result)
    }

    /// Copies one cell's bytes out of `source`; see the note on
    /// [`VarlitWriter::copy_matrix_cell_bytes_from`] for why the source borrow is
    /// shared.
    pub fn copy_matrix_cell_bytes_from<From, To>(
        &mut self,
        source: &VarlitFile,
        key: MatrixKey,
    ) -> Result<()>
    where
        From: VarlitMatrixBlock,
        To: VarlitMatrixBlock,
    {
        ensure_matrix_byte_copy_compatible::<From, To>()?;
        let payload = source.matrix_cell_payload::<From>(key)?;
        self.write_matrix_cell_payload::<To>(key, &payload)?;
        self.commit_matrix_cell::<To>(key)
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
        let mut barrier = FileMatrixDurabilityBarrier;
        self.write_matrix_cell_durable_with_barrier(key, value, &mut barrier, hook)
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
        // A chunked row has no durability point of its own: the cell lives in
        // memory until its chunk is written, and the write is what a barrier
        // could make durable. Refused by name rather than left to fail as
        // `MatrixKeyOutOfBounds`, which said nothing about why.
        if self.chunk_for_row(key.scan).is_some() {
            return Err(Error::InvalidFormatSpec(
                "a per-cell durability barrier does not apply to a chunked row; \
                 a chunk becomes durable when it is written",
            ));
        }
        // INVARIANT 3 . The commit event is pure layout geometry - block
        // index, ordinal, slot offset and stride - so it is derived *before*
        // the authoritative commit rather than after it. Previously
        // `commit_event` ran after the commit was durable and could return a
        // bare `Err` (fatal-forensics gating, block lookup, ordinal
        // arithmetic) for a cell that was already committed and synced.
        let event = {
            let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::commit_event::<T>(self.spec, matrix, key)?
        };
        // Allocate existing public error payloads before any mutation, so a
        // late hook/durability error needs no allocation and Error stays small.
        let event = crate::fallible::try_box(event)?;
        let mut error_slot = crate::fallible::try_box(Error::InvalidImmediatePolicy)?;
        self.write_matrix_cell(key, value)?;
        // Pre-publication. Nothing is committed yet, so a failure here is a
        // plain refusal: the slot bytes may be on disk but the commit bit is
        // not, and an uncommitted slot is not visible to any reader.
        let sync_data = barrier.sync_matrix_data(self.file.matrix_barrier_file()?);
        self.poison_after_started_matrix_error(sync_data)?;
        // Commit bits are still private working pages. Both caller barriers
        // run before the generation head is published; failure keeps readers
        // on the preceding generation and poisons this writer.
        self.commit_matrix_cell::<T>(key)?;
        let sync_commit = barrier.sync_matrix_commit(self.file.matrix_barrier_file()?);
        self.poison_after_started_matrix_error(sync_commit)?;
        let previous_generation = self.matrix_generation();
        if let Err(source) = self.sync() {
            self.poison.poison();
            *error_slot = source;
            return Err(if self.matrix_generation() != previous_generation {
                Error::MatrixCommittedButDurabilityUnproven {
                    event,
                    source: error_slot,
                }
            } else {
                Error::MatrixPublicationUncertain {
                    event,
                    source: error_slot,
                }
            });
        }
        // Step 2 of 2. The cell is committed and durable from here on. The hook is the
        // documented post-publication notification step, so its failure is a
        // *published* outcome, not a rollback: it is reported as
        // `Error::MatrixCommittedButHookFailed` carrying the committed event,
        // so a caller can retry the notification alone instead of repeating
        // the durable write and duplicating the hook's external work.
        hook((*event).clone()).map_err(|source| {
            *error_slot = source;
            Error::MatrixCommittedButHookFailed {
                event,
                source: error_slot,
            }
        })
    }

    /// Every written chunk as `(record offset, payload length)`, oldest first.
    ///
    /// The list is **sorted here**, by chunk index, which is what makes the
    /// binary search in `find` legal. It used to be left in file order and that
    /// order demanded to be ascending, on the reasoning that only the newest
    /// chunk is ever open — no longer true: a chunk that committed nothing is
    /// never written, so a later write to one of its rows appends its record
    /// after higher-indexed ones. File order is still what `ordinal` records,
    /// because that is what addresses a record for a rewrite.
    ///
    /// **`payload_len` is the point, not a convenience.** Every offset the chunk
    /// decode computes comes from bytes in the file, and without the record's
    /// own extent to bound them a crafted chunk reads whatever it names. The
    /// framing already established this length and already charged it against
    /// `RecordPayloadLen`; the decode below treats it as the wall.
    fn build_chunk_directory(&self) -> Result<ChunkDirectory> {
        let mut chunks = Vec::new();
        // **Through the retention gate, not straight at the field.** A handle
        // opened by `open_lazy` / `open_readonly_lazy` keeps no resident
        // directory: `self.index` is *empty*, not merely unbuilt. Reading it
        // directly made every written chunk invisible, which is not a read that
        // returns less — it is a writer that believes chunk N has no record and
        // appends a **second** one for it. That file no longer opens: the
        // duplicate refusal below fires, every chunked read answers
        // `InvalidMatrixChunk`, and the cells that were committed are gone.
        //
        // `NoResidentDirectory` is the same answer `blocks::<T>()` gives on such
        // a handle, and for the same reason: an empty answer here is
        // indistinguishable from "this file has no chunks", and that
        // indistinguishability is what destroys data rather than inconveniencing
        // a caller.
        let index = self.resident_directory("matrix chunk directory")?;
        for entry in index.iter() {
            let entry = entry?;
            if entry.block_id != MATRIX_CHUNK_BLOCK_ID {
                continue;
            }
            let prefix = self.read_chunk_prefix(entry.record_offset, entry.payload_len)?;
            // `rows` and `first_row` were decoded and thrown away by every
            // caller, and the row width was taken from the record's `cells`
            // divided by *this spec's* `rows_per_chunk` — so a file written
            // under a different `rows_per_chunk` was not refused, it was
            // reinterpreted, and a read returned another row's bytes.
            let rows_per_chunk = self
                .spec
                .growing_rows_per_chunk()
                .ok_or(Error::InvalidMatrixChunk)?;
            if prefix.rows != rows_per_chunk
                || prefix.first_row
                    != prefix
                        .index
                        .checked_mul(rows_per_chunk)
                        .ok_or(Error::InvalidMatrixChunk)?
                || u64::from(prefix.block_count) != self.spec.matrix_blocks.len() as u64
            {
                return Err(Error::InvalidMatrixChunk);
            }
            let cell_crc = !matches!(self.spec.integrity_policy, IntegrityPolicy::None);
            if prefix.cell_crc != cell_crc
                || prefix.compressed_slots != self.spec.chunk_compression.is_some()
            {
                return Err(Error::InvalidMatrixChunk);
            }
            chunks.try_reserve(1).map_err(|_| Error::AllocationFailed {
                resource: "matrix chunk directory",
                requested: chunks.len() as u64 + 1,
            })?;
            chunks.push(ChunkLocator {
                index: prefix.index,
                record_offset: entry.record_offset,
                payload_len: entry.payload_len,
                block_count: prefix.block_count,
                cell_crc: prefix.cell_crc,
                compressed_slots: prefix.compressed_slots,
                // File order, which is the order this loop walks and the order
                // `ReplacementTarget` counts in. Captured here because the sort
                // below is about to destroy it.
                ordinal: chunks.len(),
            });
        }
        // Chunk records are immutable versions. Within this handle's confirmed
        // EOF the record with the greatest offset is the current version.
        // Keep the original record count for the next append's ordinal.
        let next_ordinal = chunks.len();
        chunks.sort_unstable_by_key(|chunk| (chunk.index, std::cmp::Reverse(chunk.record_offset)));
        chunks.dedup_by_key(|chunk| chunk.index);
        Ok(ChunkDirectory {
            chunks,
            next_ordinal,
        })
    }

    /// The written-chunk directory, built at most once per handle.
    ///
    /// On failure nothing is cached, so a later call retries rather than
    /// caching a half-built answer.
    fn chunk_directory(&self) -> Result<&ChunkDirectory> {
        if let Some(directory) = self.chunk_directory.get() {
            return Ok(directory);
        }
        let built = self.build_chunk_directory()?;
        Ok(self.chunk_directory.get_or_init(|| built))
    }

    /// Reads one chunk record's prefix: its index, first row and row count.
    ///
    /// `MATRIX_CHUNK_PREFIX_LEN` bytes whatever the chunk holds. This is the
    /// read that keeps a cell lookup off the chunk's own size.
    fn read_chunk_prefix(&self, record_offset: u64, payload_len: u64) -> Result<ChunkPrefix> {
        if payload_len < MATRIX_CHUNK_PREFIX_LEN {
            return Err(Error::InvalidMatrixChunk);
        }
        let payload_offset = record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::InvalidMatrixChunk)?;
        let mut prefix = [0u8; MATRIX_CHUNK_PREFIX_LEN as usize];
        self.snapshot.read_exact_at(payload_offset, &mut prefix)?;
        note_chunk_bytes_read(MATRIX_CHUNK_PREFIX_LEN);
        if &prefix[..4] != MATRIX_CHUNK_MAGIC {
            return Err(Error::InvalidMatrixChunk);
        }
        if u16::from_le_bytes([prefix[4], prefix[5]]) != MATRIX_CHUNK_VERSION {
            return Err(Error::InvalidMatrixChunk);
        }
        let flags = u16::from_le_bytes([prefix[6], prefix[7]]);
        if flags & !MATRIX_CHUNK_KNOWN_FLAGS != 0 {
            return Err(Error::InvalidMatrixChunk);
        }
        let index = u64::from_le_bytes(prefix[8..16].try_into().expect("slice"));
        let first_row = u64::from_le_bytes(prefix[16..24].try_into().expect("slice"));
        let rows = u64::from_le_bytes(prefix[24..32].try_into().expect("slice"));
        let block_count = u32::from_le_bytes(prefix[32..36].try_into().expect("slice"));
        // The descriptor table has to fit in this record. Without this the field
        // is a bare `u32` from disk: `0xFFFF_FFFF` asked for a 137,438,953,440
        // byte reservation out of a 1 KB file, which `try_reserve` refused on
        // this host and a host with overcommit would have accepted and then
        // faulted in. No `ReadLimits` ceiling was charged, including under
        // `STANDARD`, which is the setting for untrusted input.
        let descriptors_len = MATRIX_CHUNK_BLOCK_DESC_LEN
            .checked_mul(u64::from(block_count))
            .ok_or(Error::InvalidMatrixChunk)?;
        if descriptors_len > payload_len - MATRIX_CHUNK_PREFIX_LEN {
            return Err(Error::InvalidMatrixChunk);
        }
        Ok(ChunkPrefix {
            index,
            first_row,
            rows,
            block_count,
            cell_crc: flags & MATRIX_CHUNK_FLAG_CELL_CRC != 0,
            compressed_slots: flags & MATRIX_CHUNK_FLAG_COMPRESSED_SLOTS != 0,
        })
    }

    /// Finds the written chunk with this index, by binary search over the chunk
    /// records — `O(log chunks)` prefix reads, and no state built at open.
    fn find_chunk_record(&self, chunk_index: u64) -> Result<Option<ChunkLocator>> {
        Ok(self.chunk_directory()?.find(chunk_index))
    }

    /// Where one block's commit map and slot region sit inside a chunk record.
    ///
    /// **Every offset here is derived from bytes in the file, so every one of
    /// them is bounded by `payload_len` — the record's own extent, which the
    /// framing established and already charged.** Without that wall a crafted
    /// `commit_len` moved a block's slot region onto a *neighbouring record* and
    /// a cell read returned that record's bytes decoded as a value: no error, no
    /// refusal, and `IntegrityPolicy::Crc32` did not catch it, because a
    /// positional cell read never verifies a footer under the default
    /// `IntegrityVerification::OnDemand`.
    fn chunk_block_location(
        &self,
        record_offset: u64,
        payload_len: u64,
        block_count: u32,
        cell_crc: bool,
        compressed_slots: bool,
        block_id: u32,
    ) -> Result<Option<ChunkBlockLocation>> {
        let payload_offset = record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::InvalidMatrixChunk)?;
        let payload_end = payload_offset
            .checked_add(payload_len)
            .ok_or(Error::InvalidMatrixChunk)?;
        // `read_chunk_prefix` already proved this fits; recomputed rather than
        // threaded so the bound and its use cannot drift apart.
        let descriptors_len = MATRIX_CHUNK_BLOCK_DESC_LEN
            .checked_mul(u64::from(block_count))
            .filter(|len| *len <= payload_len - MATRIX_CHUNK_PREFIX_LEN)
            .ok_or(Error::InvalidMatrixChunk)?;
        let descriptors_len_usize =
            usize::try_from(descriptors_len).map_err(|_| Error::InvalidMatrixChunk)?;
        // A stack buffer for every format anyone declares, so a cell read
        // allocates nothing. `MATRIX_CHUNK_INLINE_DESCRIPTORS` blocks is 18 at
        // 28 bytes each; past that the read is rare enough to take a heap
        // buffer, and a hostile `block_count` cannot force one because
        // `descriptors_len` is bounded by this record's own payload above.
        let mut inline = [0u8; MATRIX_CHUNK_INLINE_DESCRIPTOR_BYTES];
        let mut spilled;
        let descriptors: &mut [u8] = if descriptors_len_usize <= inline.len() {
            &mut inline[..descriptors_len_usize]
        } else {
            spilled = try_zeroed_vec(descriptors_len, "matrix chunk descriptors")?;
            &mut spilled[..descriptors_len_usize]
        };
        self.snapshot.read_exact_at(
            payload_offset
                .checked_add(MATRIX_CHUNK_PREFIX_LEN)
                .ok_or(Error::InvalidMatrixChunk)?,
            descriptors,
        )?;
        note_chunk_bytes_read(descriptors_len);
        let mut cursor = payload_offset
            .checked_add(MATRIX_CHUNK_PREFIX_LEN)
            .and_then(|offset| offset.checked_add(descriptors_len))
            .ok_or(Error::InvalidMatrixChunk)?;
        let mut found = None;
        for position in 0..block_count as usize {
            let base = position * MATRIX_CHUNK_BLOCK_DESC_LEN as usize;
            let id = u32::from_le_bytes(descriptors[base..base + 4].try_into().expect("slice"));
            let stride =
                u64::from_le_bytes(descriptors[base + 8..base + 16].try_into().expect("slice"));
            let cells =
                u64::from_le_bytes(descriptors[base + 16..base + 24].try_into().expect("slice"));
            let commit_len =
                u64::from_le_bytes(descriptors[base + 24..base + 32].try_into().expect("slice"));
            // The commit map, then the slots, then the next block — each must
            // land at or before the end of this record's payload.
            let crc_offset = cursor
                .checked_add(commit_len)
                .filter(|offset| *offset <= payload_end)
                .ok_or(Error::InvalidMatrixChunk)?;
            // A compressed block packs each checksum beside its cell inside the
            // sub-block, so there is no separate table to skip.
            let crc_len = if cell_crc && !compressed_slots {
                cells
                    .checked_mul(MATRIX_CHUNK_CRC_LEN)
                    .ok_or(Error::InvalidMatrixChunk)?
            } else {
                0
            };
            let slots_offset = crc_offset
                .checked_add(crc_len)
                .filter(|offset| *offset <= payload_end)
                .ok_or(Error::InvalidMatrixChunk)?;
            // A plain slot region's length is arithmetic. A compressed one's
            // is whatever its own index terminator says, which is a value from
            // the file and so is bounded by `payload_end` like every other.
            let slots_len = if compressed_slots {
                let count = sub_block_count(cells, compressed_cell_width(stride, cell_crc));
                let index_len = count
                    .checked_add(1)
                    .and_then(|entries| entries.checked_mul(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN))
                    .filter(|len| {
                        slots_offset
                            .checked_add(*len)
                            .is_some_and(|end| end <= payload_end)
                    })
                    .ok_or(Error::InvalidMatrixChunk)?;
                let terminator = slots_offset
                    .checked_add(index_len)
                    .and_then(|end| end.checked_sub(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN))
                    .ok_or(Error::InvalidMatrixChunk)?;
                let mut bytes = [0u8; MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN as usize];
                self.snapshot.read_exact_at(terminator, &mut bytes)?;
                note_chunk_bytes_read(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN);
                index_len
                    .checked_add(u64::from(u32::from_le_bytes(bytes)))
                    .ok_or(Error::InvalidMatrixChunk)?
            } else {
                cells.checked_mul(stride).ok_or(Error::InvalidMatrixChunk)?
            };
            let next = slots_offset
                .checked_add(slots_len)
                .filter(|offset| *offset <= payload_end)
                .ok_or(Error::InvalidMatrixChunk)?;
            // Every descriptor must be the one this spec would have written.
            // `stride` and `cells` were previously taken from the record and
            // never compared, so a crafted or foreign `cells` widened the row
            // pitch and every key resolved to a different cell.
            let descriptor = self
                .spec
                .matrix_blocks
                .iter()
                .find(|block| block.block_id == id)
                .ok_or(Error::InvalidMatrixChunk)?;
            let expected_cells = self
                .spec
                .growing_rows_per_chunk()
                .and_then(|rows| rows.checked_mul(self.chunk_row_width(descriptor).ok()?))
                .ok_or(Error::InvalidMatrixChunk)?;
            if stride != descriptor.slot_stride || cells != expected_cells {
                return Err(Error::InvalidMatrixChunk);
            }
            if id == block_id {
                // The commit map must cover the cells it claims, or the bit read
                // for a high ordinal lands in the slot region.
                if commit_len < cells.div_ceil(8) {
                    return Err(Error::InvalidMatrixChunk);
                }
                found = Some(ChunkBlockLocation {
                    commit_offset: cursor,
                    crc_offset: (cell_crc && !compressed_slots).then_some(crc_offset),
                    slots_offset,
                    slots_end: next,
                    compressed_slots,
                    cell_crc,
                    stride,
                    cells,
                });
            }
            cursor = next;
        }
        Ok(found)
    }

    /// The chunk-local ordinal of `key`, and where its block's bytes are.
    ///
    /// `Ok(None)` means no chunk record holds that row — which is not an error:
    /// a chunk nothing committed is never written, so its rows read exactly as
    /// a written chunk's uncommitted rows do.
    fn locate_chunk_cell<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<Option<(ChunkBlockLocation, u64)>> {
        let Some(locator) = self.find_chunk_record(chunk_index)? else {
            return Ok(None);
        };
        // `block_count` comes from the directory, so finding the chunk and
        // locating a block inside it cost one prefix read between them rather
        // than one each.
        let Some(location) = self.chunk_block_location(
            locator.record_offset,
            locator.payload_len,
            locator.block_count,
            locator.cell_crc,
            locator.compressed_slots,
            T::ID,
        )?
        else {
            return Err(Error::MatrixBlockMissing(T::ID));
        };
        // The spec's `rows_per_chunk` is legitimate as the divisor only because
        // the directory build refused any chunk whose own `rows` disagreed with
        // it. Before that check existed this line was the defect.
        let width = location
            .cells
            .checked_div(
                self.spec
                    .growing_rows_per_chunk()
                    .ok_or(Error::InvalidMatrixChunk)?,
            )
            .ok_or(Error::InvalidMatrixChunk)?;
        if key.ch >= width {
            return Err(Error::MatrixKeyOutOfBounds {
                scan: key.scan,
                ch: key.ch,
            });
        }
        let ordinal = local_row
            .checked_mul(width)
            .and_then(|base| base.checked_add(key.ch))
            .filter(|ordinal| *ordinal < location.cells)
            .ok_or(Error::InvalidMatrixChunk)?;
        Ok(Some((location, ordinal)))
    }

    fn chunk_cell_is_committed(&self, location: ChunkBlockLocation, ordinal: u64) -> Result<bool> {
        let mut byte = [0u8; 1];
        self.snapshot.read_exact_at(
            location
                .commit_offset
                .checked_add(ordinal / 8)
                .ok_or(Error::InvalidMatrixChunk)?,
            &mut byte,
        )?;
        note_chunk_bytes_read(1);
        Ok(byte[0] & (1u8 << (ordinal % 8)) != 0)
    }

    /// One cell out of the chunk this handle is still filling, if that is where
    /// the row lives.
    ///
    /// **The open chunk is not on disk.** Without this, a writer could not read
    /// back a cell it had just written and committed — `read_matrix_cell` said
    /// `MatrixNotCommitted` until the next write, which is a write-then-read
    /// inconsistency and not a property anyone would want. Found by sweeping
    /// every matrix entry point against a chunked row; the tests that existed
    /// all read through a fresh reader after a flush and could not see it.
    /// The open chunk's block and cell ordinal for a key, or `None` when the row
    /// is not in the open chunk.
    ///
    /// Shared by the payload read and the status read so that answering "is it
    /// committed" costs no allocation.
    fn open_chunk_cell<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<Option<(&OpenChunkBlock, u64)>> {
        let Some(chunk) = self.open_chunk.as_ref() else {
            return Ok(None);
        };
        if chunk.index != chunk_index {
            return Ok(None);
        }
        let Some(position) = chunk.block_position(T::ID) else {
            return Err(Error::MatrixBlockMissing(T::ID));
        };
        let block = &chunk.blocks[position];
        let width = block
            .cells
            .checked_div(chunk.rows)
            .ok_or(Error::InvalidMatrixChunk)?;
        if key.ch >= width {
            return Err(Error::MatrixKeyOutOfBounds {
                scan: key.scan,
                ch: key.ch,
            });
        }
        let ordinal = local_row
            .checked_mul(width)
            .and_then(|base| base.checked_add(key.ch))
            .filter(|ordinal| *ordinal < block.cells)
            .ok_or(Error::InvalidMatrixChunk)?;
        Ok(Some((block, ordinal)))
    }

    fn read_open_chunk_cell_payload<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<Option<Vec<u8>>> {
        let Some((block, ordinal)) = self.open_chunk_cell::<T>(key, chunk_index, local_row)? else {
            return Ok(None);
        };
        let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
        if block.commit[byte] & (1u8 << (ordinal % 8)) == 0 {
            return Err(Error::MatrixNotCommitted);
        }
        let start = usize::try_from(
            ordinal
                .checked_mul(block.stride)
                .ok_or(Error::InvalidMatrixChunk)?,
        )
        .map_err(|_| Error::InvalidMatrixLayout)?;
        let len = usize::try_from(block.stride).map_err(|_| Error::InvalidMatrixLayout)?;
        Ok(Some(u8::try_clone_slice(&block.slots[start..start + len])?))
    }

    /// Whether the open chunk holds a committed value for this cell.
    ///
    /// `Ok(None)` means the row is not in the open chunk, so the written records
    /// own the answer.
    fn open_chunk_cell_status<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<Option<MatrixCellStatus>> {
        // Reads the commit bit and stops. The first version answered this
        // boolean by calling `read_open_chunk_cell_payload`, which copies the
        // slot into a fresh `Vec` — a malloc, a memcpy and a free per status
        // call, for data sitting in memory two fields away.
        let Some((block, ordinal)) = self.open_chunk_cell::<T>(key, chunk_index, local_row)? else {
            return Ok(None);
        };
        let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
        Ok(Some(if block.commit[byte] & (1u8 << (ordinal % 8)) == 0 {
            MatrixCellStatus::NotCommitted
        } else {
            MatrixCellStatus::Committed
        }))
    }

    /// One cell out of a written chunk, read positionally.
    ///
    /// Two small reads and one `stride`-byte read. The chunk itself is never
    /// materialised, whatever it holds.
    fn read_chunk_cell_payload<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<Vec<u8>> {
        self.ensure_chunk_block_access::<T>()?;
        if let Some(payload) =
            self.read_open_chunk_cell_payload::<T>(key, chunk_index, local_row)?
        {
            return Ok(payload);
        }
        let Some((location, ordinal)) = self.locate_chunk_cell::<T>(key, chunk_index, local_row)?
        else {
            return Err(Error::MatrixNotCommitted);
        };
        if !self.chunk_cell_is_committed(location, ordinal)? {
            return Err(Error::MatrixNotCommitted);
        }
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, location.stride)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::MaterializedBytes, location.stride)?;
        let payload = if location.compressed_slots {
            let (payload, stored) = self.read_compressed_chunk_cell(location, ordinal)?;
            // The checksum came out of the same decode as the cell, so a
            // damaged sub-block cannot produce a matching pair.
            if self.spec.read_validation().data_crc
                && let Some(expected) = stored
            {
                let actual = crc32_bytes(&payload)?;
                if actual != expected {
                    return Err(Error::MatrixChecksumMismatch {
                        offset: location.slots_offset,
                        expected,
                        actual,
                    });
                }
            }
            return Ok(payload);
        } else {
            let mut payload = try_zeroed_vec(location.stride, "matrix chunk slot payload")?;
            self.snapshot.read_exact_at(
                location
                    .slots_offset
                    .checked_add(
                        ordinal
                            .checked_mul(location.stride)
                            .ok_or(Error::InvalidMatrixChunk)?,
                    )
                    .ok_or(Error::InvalidMatrixChunk)?,
                &mut payload,
            )?;
            note_chunk_bytes_read(location.stride);
            payload
        };
        self.verify_chunk_cell(location, ordinal, &payload)?;
        Ok(payload)
    }

    /// One cell out of a compressed slot region.
    ///
    /// The sub-block is arithmetic — `ordinal / cells_per_sub_block(stride)` —
    /// so finding it costs two four-byte reads of the offset index and no
    /// search. What it costs over a plain chunk is exactly one sub-block
    /// decode, and that is the whole price of `with_chunk_compression`.
    ///
    /// The per-cell checksum still guards the result: it is computed over the
    /// *uncompressed* cell bytes and stored plain, so a corrupted sub-block
    /// decodes to something whose checksum does not match, and the caller sees
    /// the mismatch rather than the bytes.
    fn read_compressed_chunk_sub_block(
        &self,
        location: ChunkBlockLocation,
        ordinal: u64,
    ) -> Result<Vec<u8>> {
        let cell_crc = location.cell_crc;
        let width = compressed_cell_width(location.stride, cell_crc);
        let per = cells_per_sub_block(width);
        let count = sub_block_count(location.cells, width);
        let index = ordinal / per;
        if index >= count {
            return Err(Error::InvalidMatrixChunk);
        }
        let index_len = count
            .checked_add(1)
            .and_then(|entries| entries.checked_mul(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN))
            .ok_or(Error::InvalidMatrixChunk)?;
        let entry_at = location
            .slots_offset
            .checked_add(
                index
                    .checked_mul(MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN)
                    .ok_or(Error::InvalidMatrixChunk)?,
            )
            .ok_or(Error::InvalidMatrixChunk)?;
        let mut pair = [0u8; 2 * MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN as usize];
        self.snapshot.read_exact_at(entry_at, &mut pair)?;
        note_chunk_bytes_read(2 * MATRIX_CHUNK_SUB_BLOCK_OFFSET_LEN);
        let start = u64::from(u32::from_le_bytes(pair[..4].try_into().expect("slice")));
        let end = u64::from(u32::from_le_bytes(pair[4..].try_into().expect("slice")));
        if end < start {
            return Err(Error::InvalidMatrixChunk);
        }
        let stored_len = end - start;
        let data_at = location
            .slots_offset
            .checked_add(index_len)
            .and_then(|base| base.checked_add(start))
            .filter(|at| {
                at.checked_add(stored_len)
                    .is_some_and(|end| end <= location.slots_end)
            })
            .ok_or(Error::InvalidMatrixChunk)?;

        // The uncompressed extent of this sub-block: a full one everywhere but
        // possibly the last.
        let cells_here = per.min(location.cells.saturating_sub(index * per));
        let raw_len = cells_here
            .checked_mul(width)
            .ok_or(Error::InvalidMatrixChunk)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::MaterializedBytes, raw_len)?;

        // Writers store raw bytes when compression would not shrink them.
        // Refuse an oversized extent before allocating attacker-controlled bytes.
        if stored_len > raw_len {
            return Err(Error::InvalidMatrixChunk);
        }
        let mut stored = try_zeroed_vec(stored_len, "matrix chunk sub-block")?;
        self.snapshot.read_exact_at(data_at, &mut stored)?;
        note_chunk_bytes_read(stored_len);

        // Equal lengths mean the writer stored it raw, which it does whenever
        // compressing did not shrink it. The writer never emits a compressed
        // sub-block of exactly the uncompressed length, so this is unambiguous.
        let raw = if stored_len == raw_len {
            stored
        } else {
            let compression = self
                .spec
                .chunk_compression
                .ok_or(Error::InvalidMatrixChunk)?;
            let decoded = decompress_with_algorithm(compression.algorithm, &stored, raw_len)?;
            if decoded.len() as u64 != raw_len {
                return Err(Error::DecompressedLengthMismatch {
                    expected: raw_len,
                    actual: decoded.len() as u64,
                });
            }
            decoded
        };

        Ok(raw)
    }

    fn read_compressed_chunk_cell(
        &self,
        location: ChunkBlockLocation,
        ordinal: u64,
    ) -> Result<(Vec<u8>, Option<u32>)> {
        let cell_crc = location.cell_crc;
        let width = compressed_cell_width(location.stride, cell_crc);
        let per = cells_per_sub_block(width);
        let raw = self.read_compressed_chunk_sub_block(location, ordinal)?;
        let at = usize::try_from(
            (ordinal % per)
                .checked_mul(width)
                .ok_or(Error::InvalidMatrixChunk)?,
        )
        .map_err(|_| Error::InvalidMatrixChunk)?;
        let stride = usize::try_from(location.stride).map_err(|_| Error::InvalidMatrixChunk)?;
        let payload = raw
            .get(at..at + stride)
            .map(<[u8]>::to_vec)
            .ok_or(Error::InvalidMatrixChunk)?;
        let stored = if cell_crc {
            let bytes = raw
                .get(at + stride..at + stride + 4)
                .ok_or(Error::InvalidMatrixChunk)?;
            Some(u32::from_le_bytes(bytes.try_into().expect("slice")))
        } else {
            None
        };
        Ok((payload, stored))
    }

    /// Checks one chunked cell against the checksum stored beside it.
    ///
    /// **The record footer's crc does not cover this read.** It covers the
    /// record, and it is verified on a *record* read; a positional cell read is
    /// not one, so under the default `IntegrityVerification::OnDemand` a single
    /// flipped bit in a written chunk came back as data with no error, while the
    /// identical flip one row earlier — in the matrix region, which keeps
    /// per-cell checksums — was refused. A chunk keeps them now, for the same
    /// reason and in the same shape.
    fn verify_chunk_cell(
        &self,
        location: ChunkBlockLocation,
        ordinal: u64,
        payload: &[u8],
    ) -> Result<()> {
        if !self.spec.read_validation().data_crc {
            return Ok(());
        }
        let Some(crc_offset) = location.crc_offset else {
            return Ok(());
        };
        let at = crc_offset
            .checked_add(
                ordinal
                    .checked_mul(MATRIX_CHUNK_CRC_LEN)
                    .ok_or(Error::InvalidMatrixChunk)?,
            )
            .ok_or(Error::InvalidMatrixChunk)?;
        let mut stored = [0u8; MATRIX_CHUNK_CRC_LEN as usize];
        self.snapshot.read_exact_at(at, &mut stored)?;
        note_chunk_bytes_read(MATRIX_CHUNK_CRC_LEN);
        let expected = u32::from_le_bytes(stored);
        let actual = crc32_bytes(payload)?;
        if actual != expected {
            return Err(Error::MatrixChecksumMismatch {
                offset: location
                    .slots_offset
                    .checked_add(
                        ordinal
                            .checked_mul(location.stride)
                            .ok_or(Error::InvalidMatrixChunk)?,
                    )
                    .ok_or(Error::InvalidMatrixChunk)?,
                expected,
                actual,
            });
        }
        Ok(())
    }

    pub fn read_matrix_cell<T: VarlitMatrixBlock>(&self, key: MatrixKey) -> Result<T> {
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            let payload = self.read_chunk_cell_payload::<T>(key, chunk_index, local_row)?;
            // Bounded exactly as the region twin bounds it (`matrix::read_cell`):
            // the slot bytes are already charged against `MaterializedBytes`,
            // so what remains of that budget is what the decode may materialize
            // on top of them. Unbounded here, a decoder that expands past its
            // stride was capped on a region row and uncapped on a chunked one.
            let materialized_limit = self
                .spec
                .read_limits
                .require(ReadLimitKey::MaterializedBytes)?
                .unwrap_or(u64::MAX);
            let decode_limit = materialized_limit.checked_sub(payload.len() as u64).ok_or(
                Error::ResourceArithmeticOverflow {
                    resource: "materialized bytes",
                },
            )?;
            return crate::codec::Decoder::decode_from_slice_limited(
                &payload,
                T::ENDIAN.unwrap_or(self.spec.endian),
                decode_limit,
            );
        }
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::read_cell(self.spec, matrix, self.file.matrix_region_reader(), key)
    }

    /// Whether a chunked cell has a committed value.
    ///
    /// Answers for a chunked row the same way the region answers for a region
    /// row: a value that never arrived is `NotCommitted`, permanently and by
    /// design. Sealing asks no question about completeness.
    fn chunked_cell_status<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<MatrixCellStatus> {
        self.ensure_chunk_block_access::<T>()?;
        if let Some(status) = self.open_chunk_cell_status::<T>(key, chunk_index, local_row)? {
            return Ok(status);
        }
        match self.locate_chunk_cell::<T>(key, chunk_index, local_row)? {
            Some((location, ordinal)) if self.chunk_cell_is_committed(location, ordinal)? => {
                Ok(MatrixCellStatus::Committed)
            }
            _ => Ok(MatrixCellStatus::NotCommitted),
        }
    }

    pub fn matrix_cell_payload<T: VarlitMatrixBlock>(&self, key: MatrixKey) -> Result<Vec<u8>> {
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            return self.read_chunk_cell_payload::<T>(key, chunk_index, local_row);
        }
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::read_cell_payload::<T>(
            self.spec,
            matrix,
            self.file.matrix_region_reader(),
            key,
        )
    }

    pub fn matrix_aux_len(&self, name: &str) -> Result<u64> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::aux_len(matrix, name)
    }

    pub fn read_matrix_aux(&self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::read_aux_at_len(
            matrix,
            self.file.matrix_region_reader(),
            self.snapshot.len(),
            name,
            offset,
            len,
        )
    }

    pub fn write_matrix_aux(&mut self, name: &str, offset: u64, payload: &[u8]) -> Result<()> {
        let _permit = self.ensure_write()?;
        let result = {
            let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::write_aux_at_len(
                matrix,
                self.file.matrix_region(),
                self.snapshot.len(),
                name,
                offset,
                payload,
            )
        };
        self.finish_matrix_mutation(result)
    }

    pub fn matrix_cell_status<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            return self.chunked_cell_status::<T>(key, chunk_index, local_row);
        }
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::cell_status::<T>(
            self.spec,
            matrix,
            key,
            self.file.matrix_region_reader().bitmap_read(),
        )
    }

    pub fn commit_matrix_cell<T: VarlitMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        let _permit = self.ensure_write()?;
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            let (position, ordinal) = self.chunk_block_slice::<T>(chunk_index, local_row, key)?;
            let chunk = self.open_chunk.as_mut().ok_or(Error::InvalidMatrixChunk)?;
            let block = &mut chunk.blocks[position];
            let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
            let bit = 1u8 << (ordinal % 8);
            // The region path refuses a commit of a cell that was never
            // written, and answers from its write bitmap. This asked the slot
            // bytes instead — "are they all zero" — which is not the same
            // question: a cell holding a legitimate zero could never be
            // committed.
            if block.commit[byte] & bit == 0 && block.written[byte] & bit == 0 {
                return Err(Error::MatrixCellNotWritten);
            }
            block.commit[byte] |= bit;
            chunk.dirty = true;
            return Ok(());
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::commit_cell::<T>(self.spec, matrix, self.file.matrix_region(), key)
        };
        self.finish_matrix_mutation(result)
    }

    /// Clears one chunked cell: its commit bit and its slot bytes.
    ///
    /// Opens the cell's chunk first, loading it back from its record when it has
    /// one. This used to refuse anything but the currently open chunk, which
    /// left the two halves of the same capability disagreeing: after
    /// [`Self::open_chunk_at`] learned to reload, a written cell could be
    /// *overwritten* but not *cleared*. A caller cannot act on that distinction
    /// — clearing is what `write` is for a cell that should go back to having no
    /// value — so it was a refusal with nothing behind it.
    ///
    /// The refusals that remain are the ones `open_chunk_at` reports, and they
    /// are about the record's shape rather than about the cell: a compressed
    /// chunk cannot be rewritten at its stored length. See
    /// [`Error::MatrixChunkNotReopenable`].
    fn clear_chunk_cell<T: VarlitMatrixBlock>(
        &mut self,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<()> {
        self.open_chunk_at(chunk_index)?;
        let (position, ordinal) = self.chunk_block_slice::<T>(chunk_index, local_row, key)?;
        let chunk = self.open_chunk.as_mut().ok_or(Error::InvalidMatrixChunk)?;
        let block = &mut chunk.blocks[position];
        let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
        block.commit[byte] &= !(1u8 << (ordinal % 8));
        block.written[byte] &= !(1u8 << (ordinal % 8));
        let start = usize::try_from(
            ordinal
                .checked_mul(block.stride)
                .ok_or(Error::InvalidMatrixChunk)?,
        )
        .map_err(|_| Error::InvalidMatrixLayout)?;
        let len = usize::try_from(block.stride).map_err(|_| Error::InvalidMatrixLayout)?;
        block.slots[start..start + len].fill(0);
        // A chunk with a record on disk must be written out again, or the clear
        // lives in the buffer while the file keeps serving the old value —
        // silent loss of a mutation, reported as success. A chunk with no record
        // has nothing to contradict, so clearing it leaves an idle flush writing
        // nothing, which is what `dirty` is for.
        if chunk.backing.is_some() {
            chunk.dirty = true;
        }
        Ok(())
    }

    /// [`Self::clear_chunk_cell`] addressed by block id rather than by type, for
    /// [`Self::clear_matrix_cell_by_category`].
    ///
    /// **One cell, exactly as its typed twin clears one cell.** Only the way the
    /// block is named differs — a category string instead of a Rust type — and
    /// the two must therefore refuse and accept the same things. They briefly
    /// did not: `clear_chunk_cell` learned to reopen a written chunk and this
    /// did not, so the same cell cleared under one spelling and returned
    /// `MatrixChunkClosed` under the other. The comment justifying that claimed
    /// this entry point walks a whole category; it does not, and
    /// [`Self::clear_matrix_category`] is the one that does.
    fn clear_chunk_cell_by_id(
        &mut self,
        block_id: u32,
        key: MatrixKey,
        chunk_index: u64,
        local_row: u64,
    ) -> Result<()> {
        // Its typed twin gates through `chunk_block_slice`; this one addresses
        // the block by id and so went through nothing, leaving a quarantined
        // category clearable through any chunked row.
        self.ensure_chunk_access_by_block_id(block_id)?;
        self.open_chunk_at(chunk_index)?;
        let chunk = self.open_chunk.as_mut().ok_or(Error::InvalidMatrixChunk)?;
        let position = chunk
            .block_position(block_id)
            .ok_or(Error::MatrixBlockMissing(block_id))?;
        let rows = chunk.rows;
        let block = &mut chunk.blocks[position];
        let width = block
            .cells
            .checked_div(rows)
            .ok_or(Error::InvalidMatrixChunk)?;
        if key.ch >= width {
            return Err(Error::MatrixKeyOutOfBounds {
                scan: key.scan,
                ch: key.ch,
            });
        }
        let ordinal = local_row
            .checked_mul(width)
            .and_then(|base| base.checked_add(key.ch))
            .filter(|ordinal| *ordinal < block.cells)
            .ok_or(Error::InvalidMatrixChunk)?;
        let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
        block.commit[byte] &= !(1u8 << (ordinal % 8));
        block.written[byte] &= !(1u8 << (ordinal % 8));
        let start = usize::try_from(
            ordinal
                .checked_mul(block.stride)
                .ok_or(Error::InvalidMatrixChunk)?,
        )
        .map_err(|_| Error::InvalidMatrixLayout)?;
        let len = usize::try_from(block.stride).map_err(|_| Error::InvalidMatrixLayout)?;
        block.slots[start..start + len].fill(0);
        // Same reason as in the typed twin: a chunk with a record on disk must
        // go out again, or the file keeps serving the value this just cleared.
        if chunk.backing.is_some() {
            chunk.dirty = true;
        }
        Ok(())
    }

    pub fn clear_matrix_cell<T: VarlitMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        let _permit = self.ensure_write()?;
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            return self.clear_chunk_cell::<T>(key, chunk_index, local_row);
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::clear_cell::<T>(self.spec, matrix, self.file.matrix_region(), key)
        };
        self.finish_matrix_mutation(result)
    }

    pub fn clear_matrix_cell_by_category(&mut self, category: &str, key: MatrixKey) -> Result<()> {
        let _permit = self.ensure_write()?;
        if let Some((chunk_index, local_row)) = self.chunk_for_row(key.scan) {
            let block_id = self
                .spec
                .matrix_blocks
                .iter()
                .find(|block| block.category == category)
                .map(|block| block.block_id)
                .ok_or_else(|| Error::MatrixCommitMissing(category.to_string()))?;
            return self.clear_chunk_cell_by_id(block_id, key, chunk_index, local_row);
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::clear_cell_by_category(
                self.spec,
                matrix,
                self.file.matrix_region(),
                category,
                key,
            )
        };
        self.finish_matrix_mutation(result)
    }

    /// Clears a category in the working generation, including stored chunks.
    /// Each affected chunk is loaded and appended as a new immutable version;
    /// memory holds one chunk at a time. Published readers remain unchanged
    /// until sync. A partial failure can leave partial working changes; reopen
    /// to discard them instead of publishing an incomplete operation.
    pub fn clear_matrix_category(&mut self, category: &str) -> Result<u64> {
        let _permit = self.ensure_write()?;
        let mut cleared = 0u64;
        if self.spec.growing_matrix.is_some() {
            // Refused before anything is touched. `clear_category` reaches its
            // own `ensure_fatal_access_allowed` only after the open chunk had
            // already been zeroed, so a refusal returned an error to a caller
            // entitled to believe nothing had happened.
            self.ensure_chunk_access_allowed(category)?;
            // The same discipline for the reopen refusals. A format whose
            // chunks cannot be rewritten at their stored length would fail on
            // the first written chunk — after the region had been cleared and
            // possibly after several chunks had been — so it is asked here,
            // where the answer is a property of the spec and nothing has moved.
            if let Some(oldest) = self.chunk_directory()?.chunks.first().map(|it| it.index) {
                if self.spec.chunk_compression.is_some() {
                    return Err(Error::MatrixChunkNotReopenable {
                        chunk: oldest,
                        reason: "chunk compression",
                    });
                }
                if self.spec.index_policy.segment_on_flush {
                    return Err(Error::MatrixChunkNotReopenable {
                        chunk: oldest,
                        reason: "segment_on_flush",
                    });
                }
            }
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::clear_category(self.spec, matrix, self.file.matrix_region(), category)
        };
        // The region first, as before: it is one write that either happens or
        // does not, so a failure there leaves the chunks untouched rather than
        // half-cleared behind an error.
        let region_cleared = self.finish_matrix_mutation(result)?;
        if self.spec.growing_matrix.is_some() {
            // The open chunk, whether or not it has a record yet. Doing it here
            // rather than inside the walk keeps the common case — a growing
            // matrix that has never written a chunk — exactly what it was: a
            // memset over a buffer, no I/O at all.
            if let Some(position) = self.open_chunk_block_for(category)? {
                cleared = cleared.saturating_add(self.clear_open_chunk_block(position));
            }
            cleared = cleared.saturating_add(self.clear_written_chunks_of(category)?);
        }
        Ok(cleared + region_cleared)
    }

    /// Clears the category out of every written chunk, one at a time.
    ///
    /// Walks the directory by position rather than over a collected list of
    /// indexes: the list would be one `u64` per chunk held for the length of
    /// the walk, which is the shape this file removes everywhere else, and the
    /// walk rewrites records in place so the directory neither grows nor
    /// reorders under it. The chunk already open is skipped — the caller
    /// cleared it in its buffer — and is written out by the first reload, which
    /// is what carries that clear to disk.
    fn clear_written_chunks_of(&mut self, category: &str) -> Result<u64> {
        let mut cleared = 0u64;
        let mut at = 0usize;
        loop {
            let Some(index) = self.chunk_directory()?.chunks.get(at).map(|it| it.index) else {
                return Ok(cleared);
            };
            at += 1;
            if self
                .open_chunk
                .as_ref()
                .is_some_and(|open| open.index == index)
            {
                continue;
            }
            self.open_chunk_at(index)?;
            if let Some(position) = self.open_chunk_block_for(category)? {
                cleared = cleared.saturating_add(self.clear_open_chunk_block(position));
            }
        }
    }

    /// Which block of the open chunk a category names, if any.
    ///
    /// The fallible half of clearing a chunk category, split out so the caller
    /// can resolve before it destroys: every `None` here is "nothing to clear",
    /// and the apply step that follows cannot fail at all.
    fn open_chunk_block_for(&self, category: &str) -> Result<Option<usize>> {
        let Some(block_id) = self
            .spec
            .matrix_blocks
            .iter()
            .find(|block| block.category == category)
            .map(|block| block.block_id)
        else {
            return Ok(None);
        };
        let Some(chunk) = self.open_chunk.as_ref() else {
            return Ok(None);
        };
        Ok(chunk.block_position(block_id))
    }

    /// Clears one already-resolved block of the open chunk, returning how many
    /// cells were committed in it. Infallible by construction — a memset over a
    /// buffer this handle owns.
    fn clear_open_chunk_block(&mut self, position: usize) -> u64 {
        let Some(chunk) = self.open_chunk.as_mut() else {
            return 0;
        };
        let block = &mut chunk.blocks[position];
        let cleared = block
            .commit
            .iter()
            .map(|byte| u64::from(byte.count_ones()))
            .sum();
        block.commit.fill(0);
        block.written.fill(0);
        block.slots.fill(0);
        block.crc.fill(0);
        // `dirty` is what makes the next flush write this chunk out, and the
        // answer differs by whether the chunk already has a record.
        //
        // **No record.** Leaving `dirty` set after clearing every bit wrote an
        // all-uncommitted record — a full-size record for rows that hold
        // nothing, which (before chunks could be reopened) then refused every
        // later write to those rows, permanently. So it is recomputed from
        // what is left, not cleared outright: another block of the same chunk
        // may still hold committed cells.
        //
        // **Has a record.** The opposite, and getting this wrong loses the
        // clear silently: the record on disk still holds the cells this just
        // zeroed, so a chunk that goes non-dirty is never written and the file
        // keeps serving them. It was unreachable while `clear_matrix_category`
        // refused a written chunk, and became reachable the moment that
        // refusal was lifted.
        chunk.dirty = chunk.backing.is_some()
            || chunk
                .blocks
                .iter()
                .any(|block| block.commit.iter().any(|byte| *byte != 0));
        cleared
    }

    pub fn apply_matrix_recovery_action(&mut self, action: &MatrixRecoveryAction) -> Result<()> {
        match action {
            MatrixRecoveryAction::ClearCell { category, key } => {
                self.clear_matrix_cell_by_category(category, *key)
            }
            MatrixRecoveryAction::ClearCategory { category } => {
                self.clear_matrix_category(category).map(|_| ())
            }
            MatrixRecoveryAction::RebuildCommitMap { .. } => Err(Error::InvalidFormatSpec(
                "typed rebuild requires rebuild_matrix_commit_from_crc",
            )),
            MatrixRecoveryAction::Resume
            | MatrixRecoveryAction::Restart
            | MatrixRecoveryAction::DiscardSidecar => Ok(()),
        }
    }

    /// Rebuilds a commit map from the per-cell checksum table.
    ///
    /// **Refused for a format with a growing dimension.** The table it reads is
    /// the matrix region's; a chunk carries no per-cell checksums of its own —
    /// its record footer crc covers the whole payload instead. Without this
    /// refusal the call returned `Ok(0)` on a file full of chunks, which reads
    /// as "the commit map was already right" rather than "this answered for the
    /// region and nothing else".
    pub fn rebuild_matrix_commit_from_crc<T: VarlitMatrixBlock>(&mut self) -> Result<u64> {
        let _permit = self.ensure_write()?;
        if self.spec.growing_matrix.is_some() {
            return Err(Error::InvalidFormatSpec(
                "rebuild_matrix_commit_from_crc covers the matrix region only; \
                 a growing matrix's chunks carry no per-cell checksum table",
            ));
        }
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::rebuild_commit_map_from_crc::<T>(
                self.spec,
                matrix,
                self.file.matrix_region(),
            )
        };
        self.finish_matrix_mutation(result)
    }

    pub fn is_matrix_single_committed(&self, name: &str) -> Result<bool> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::is_single_committed(
            matrix,
            name,
            self.file.matrix_region_reader().bitmap_read(),
        )
    }

    pub fn set_matrix_single_committed(&mut self, name: &str, value: bool) -> Result<()> {
        let _permit = self.ensure_write()?;
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::set_single_committed(matrix, self.file.matrix_region(), name, value)
        };
        self.finish_matrix_mutation(result)
    }

    pub fn is_matrix_channel_committed(&self, name: &str, channel: u64) -> Result<bool> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::is_channel_committed(
            matrix,
            name,
            channel,
            self.file.matrix_region_reader().bitmap_read(),
        )
    }

    pub fn set_matrix_channel_committed(
        &mut self,
        name: &str,
        channel: u64,
        value: bool,
    ) -> Result<()> {
        let _permit = self.ensure_write()?;
        let result = {
            let matrix = self.matrix.as_mut().ok_or(Error::MatrixLayoutMissing)?;
            crate::matrix::set_channel_committed(
                matrix,
                self.file.matrix_region(),
                name,
                channel,
                value,
            )
        };
        self.finish_matrix_mutation(result)
    }

    /// What a resumed writer should do with this category.
    ///
    /// A growing matrix holding a **dirty** chunk is never `Clean`: that chunk
    /// is live state this handle has and the next handle will not, so reporting
    /// a clean category over it told a caller the acquisition had finished when
    /// it had not.
    ///
    /// **Dirty, not merely open.** The test is `is_unwritten`, and it used to be
    /// `is_some`. A chunk stays open after its record is written now — that is
    /// what preserves the write bitmap across a flush, which is not on disk —
    /// so "a chunk is open" no longer implies "something is only in memory". A
    /// clean chunk's every byte is in its record; the next handle reads exactly
    /// what this one would.
    pub fn matrix_resume_signal(&self, category: &str) -> Result<MatrixResumeSignal> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        let signal = crate::matrix::resume_signal(
            matrix,
            category,
            self.file.matrix_region_reader().bitmap_read(),
        )?;
        if self.open_chunk_is_unwritten() && matches!(signal, MatrixResumeSignal::Clean) {
            return Ok(MatrixResumeSignal::Partial {
                committed: 0,
                total: 0,
            });
        }
        Ok(signal)
    }

    pub fn matrix_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::sidecar_resume_signal(
            matrix,
            category,
            sidecar_path.as_ref().exists(),
            self.file.matrix_region_reader().bitmap_read(),
        )
    }

    pub fn matrix_verified_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.matrix_verified_sidecar_resume_signal_inner(category, sidecar_path.as_ref(), None)
    }

    pub fn matrix_verified_sidecar_resume_signal_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<MatrixResumeSignal> {
        self.matrix_verified_sidecar_resume_signal_inner(
            category,
            sidecar_path.as_ref(),
            Some(expected_generation),
        )
    }

    fn matrix_verified_sidecar_resume_signal_inner(
        &self,
        category: &str,
        path: &Path,
        expected_generation: Option<u64>,
    ) -> Result<MatrixResumeSignal> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        if !path.exists() {
            return crate::matrix::sidecar_resume_signal(
                matrix,
                category,
                false,
                self.file.matrix_region_reader().bitmap_read(),
            );
        }
        let identity = self.matrix_native_identity()?;
        match read_matrix_sidecar_file(self.spec, category, path, expected_generation, identity) {
            Ok(_) => crate::matrix::sidecar_resume_signal(
                matrix,
                category,
                true,
                self.file.matrix_region_reader().bitmap_read(),
            ),
            Err(Error::IntegrityFeatureDisabled) => Err(Error::IntegrityFeatureDisabled),
            Err(error @ Error::LimitExceeded { .. })
            | Err(error @ Error::MissingResourceLimit { .. })
            | Err(error @ Error::TrustedUnboundedRequiresExplicitApi { .. })
            | Err(error @ Error::ResourceArithmeticOverflow { .. })
            | Err(error @ Error::LengthOverflow { .. })
            | Err(error @ Error::AllocationFailed { .. }) => Err(error),
            Err(_) => Ok(MatrixResumeSignal::DiscardRecommended),
        }
    }

    /// Publishes a matrix resume sidecar bound to this native file.
    ///
    /// Ordering contract: the native matrix data this sidecar summarizes must
    /// already be durable (native -> sidecar). Callers write and sync native
    /// cells/commits first, then call this, so a sidecar can never advertise
    /// progress that is not yet present in the native file. Publication itself
    /// is a same-directory temp write + fsync + atomic replace + parent sync,
    /// so a crash mid-publish leaves the previous sidecar intact.
    pub fn write_matrix_sidecar<P: AsRef<Path>>(
        &mut self,
        category: &str,
        sidecar_path: P,
        generation: u64,
        payload: &[u8],
    ) -> Result<MatrixSidecarManifest> {
        let _permit = self.ensure_write()?;
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::resume_signal(
            matrix,
            category,
            self.file.matrix_region_reader().bitmap_read(),
        )?;
        let identity = self.matrix_native_identity()?;
        write_matrix_sidecar_file(
            self.spec,
            category,
            sidecar_path,
            generation,
            payload,
            identity,
        )
    }

    pub fn read_matrix_sidecar<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::resume_signal(
            matrix,
            category,
            self.file.matrix_region_reader().bitmap_read(),
        )?;
        let identity = self.matrix_native_identity()?;
        read_matrix_sidecar_file(self.spec, category, sidecar_path, None, identity)
    }

    pub fn read_matrix_sidecar_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        crate::matrix::resume_signal(
            matrix,
            category,
            self.file.matrix_region_reader().bitmap_read(),
        )?;
        let identity = self.matrix_native_identity()?;
        read_matrix_sidecar_file(
            self.spec,
            category,
            sidecar_path,
            Some(expected_generation),
            identity,
        )
    }

    /// Computes the native-file identity a matrix sidecar is bound to.
    ///
    /// Reuses the same OS-object identity bytes as the scalable sidecar identity
    /// machinery (volume + file ID on Windows, device + inode on Unix) and folds
    /// in the schema hash, then pairs it with the matrix layout generation.
    fn matrix_native_identity(&self) -> Result<MatrixNativeIdentity> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        let creation_nonce = self
            .matrix_creation_nonce
            .ok_or(Error::MatrixLayoutMissing)?;
        let file = self.snapshot.try_clone_file()?;
        let fingerprint = native_object_fingerprint(self.spec, &file)?;
        Ok(MatrixNativeIdentity {
            fingerprint,
            layout_generation: matrix.append_log_start(),
            creation_nonce,
        })
    }

    /// Per-file storage observations, without probing or changing any live cursor.
    pub fn matrix_storage_diagnostics(&self) -> Option<crate::MatrixStorageDiagnostics> {
        self.matrix
            .as_ref()
            .map(|layout| layout.storage_diagnostics())
    }

    pub fn matrix_recovery_report(&self) -> MatrixRecoveryReport {
        match self.matrix.as_ref() {
            Some(matrix) => crate::matrix::recovery_report(
                matrix,
                self.file.matrix_region_reader().bitmap_read(),
            ),
            None => MatrixRecoveryReport {
                findings: Vec::new(),
                recommended_actions: Vec::new(),
            },
        }
    }

    /// Verifies this matrix's commit metadata now, and reports what was found.
    ///
    /// The on-demand half of `MatrixMetadataVerification`. A matrix opened under
    /// `MatrixMetadataVerification::AtOpen` — the default — has already had this
    /// pass run, and its findings are in `matrix_recovery_report`; one opened
    /// under `OnDemand` has not, and this is how a caller asks. Running it twice
    /// is allowed and answers the same thing.
    ///
    /// What it does: reads every page of every commit map named by the persisted
    /// page index **unioned with** the platform allocation map — the second term
    /// is what sees stray bytes in a page nothing ever published — authenticates
    /// each against its stored digest, and reports the `Recoverable` commit-map
    /// findings and the `RebuildCommitMap` / `ClearCategory` recommendations that
    /// follow. Cost: `O(live pages + allocated pages)` bytes read, one 4096-byte
    /// buffer retained, no matter the matrix's size.
    ///
    /// What it does **not** do: arm the quarantine
    /// (`Error::MatrixCommitQuarantined`) or gate a writer. That gate is derived
    /// once, at open, from the findings the layout is assembled with; a caller who
    /// needs a damaged category failed closed reopens with `AtOpen`.
    ///
    /// `Err(Error::MatrixLayoutMissing)` when this file has no matrix.
    pub fn verify_matrix_metadata(&self) -> Result<MatrixRecoveryReport> {
        let matrix = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        // A private cursor borrows the current view, including pending writer
        // pages. Positional reads preserve `&self` without cloning the working
        // directory or sharing its mutation capability.
        let mut file = self
            .file
            .matrix_pages()
            .ok_or(Error::MatrixLayoutMissing)?
            .read_borrow();
        crate::matrix::verify_matrix_metadata(matrix, &mut file)
    }
}
