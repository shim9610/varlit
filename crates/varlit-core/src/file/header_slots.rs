// Native file header slots operations.

impl VarlitFile {
    /// Where the `VBTT` region sits and the cold bytes that retire it, or
    /// `None` for a format that declares no region.
    ///
    /// # Why an in-place replacement has to retire it
    ///
    /// The region stores no high-water mark. `corroborate_header_tails`
    /// *derives* one — the commit marker the slot names, then the segment and
    /// digest that may follow it — and that derivation is sound only because a
    /// record's sequence rises with its offset, which holds for an append log.
    ///
    /// An in-place replacement is the one operation that breaks it: it takes a
    /// *fresh* sequence and writes it at an offset **before** the marker, so
    /// the file's true maximum is above every record the derivation looks at.
    /// A lazy open then seeds its writer with a mark that is already used, and
    /// the next append duplicates a sequence — measured on a `header_tails`
    /// file of 64 records, `replace_fixed` took sequence 65 while the marker
    /// still said 64, and the following scan refused the file with
    /// `InvalidCanonicalEncoding("duplicate native record sequence")`. The file
    /// stops opening, so this is data loss rather than a slow open.
    ///
    /// The offsets in the table stay perfectly valid — an in-place replacement
    /// moves no record, which is exactly why this path was allowed to carry the
    /// table forward where [`header_extensions_for_replacement`] resets it. But
    /// the region answers three facts as a package, and one of them has gone
    /// wrong, so it goes out of use whole.
    ///
    /// # Why this is retirement and not a refusal
    ///
    /// The digest hazard is the same shape and is refused instead
    /// (`ensure_in_place_replacement_allowed`). The difference is that a
    /// digest has no "claims nothing" state — it is a record, present and
    /// trusted or not there at all — while a cold region is a state the format
    /// already defines and every open already handles. Where an escape hatch
    /// exists, the capability survives: the replacement still happens, and the
    /// cost is that the next open scans once and the next commit rewarms the
    /// region.
    fn header_tails_invalidation(&self) -> Result<Option<(u64, Vec<u8>)>> {
        if !self.spec.index_policy.header_tails {
            return Ok(None);
        }
        let header_len = append_log_start_for_file(self)?;
        let Some(extent) =
            mutable_header_block_extent(header_len, &self.header_extensions, HEADER_TAILS_MAGIC)?
        else {
            return Ok(None);
        };
        let framed = encode_cold_header_tails_region(self.spec)?;
        let cold = u8::try_clone_slice(
            framed
                .get(HEADER_TAILS_BLOCK_FRAMING_LEN..)
                .ok_or(Error::InvalidCompressionHeader)?,
        )?;
        if extent.payload_len != cold.len() {
            // `write_header_tails` leaves a region of unknown geometry alone,
            // and for a *write* that is right: it cannot encode a slot it does
            // not understand. Retirement is the opposite case — leaving it
            // alone is precisely the outcome that must not happen, because the
            // stale table would go on answering. Unreachable in practice: open
            // refuses a file whose mutable block is a different length than
            // this build would write (`validate_file_header_extensions`), so
            // reaching here means the header changed under an open handle.
            return Err(Error::InvalidCompressionHeader);
        }
        Ok(Some((extent.payload_offset, cold)))
    }

    /// Retires the region in the live file, for the path that mutates bytes
    /// rather than publishing a new generation.
    ///
    /// Ordered before the record write by its caller, so a crash between the
    /// two leaves a file whose lazy route is already out of use. The reverse
    /// order would leave the window where the table outlives the invariant it
    /// was derived under, which is the whole defect.
    ///
    /// The cached region follows the file for the reason
    /// [`Self::write_header_tails`] states: it is what the next commit reads
    /// its generation from, so a cache left warm over a cold file would write
    /// generation `N + 1` while a reader still prefers the warm slot claiming
    /// `N`.
    fn retire_header_tails_in_place(&mut self) -> Result<()> {
        let Some((offset, cold)) = self.header_tails_invalidation()? else {
            return Ok(());
        };
        self.file.overwrite_header_region(offset, &cold)?;
        self.file.sync_data()?;
        self.header_extensions =
            header_extensions_for_replacement(self.spec, &self.header_extensions)?;
        Ok(())
    }

    /// How many bytes the editable header region holds in total.
    ///
    /// The declared capacity, which never changes for the life of the file:
    /// it is folded into the schema hash, so a file written under one capacity
    /// is not opened under another. Zero when the format declares no region.
    pub fn header_slots_capacity(&self) -> usize {
        self.spec.header_slots.capacity as usize
    }

    /// How many of those bytes the blocks currently written there occupy,
    /// framing included.
    pub fn header_slots_used(&self) -> Result<usize> {
        match self.header_slots_view()? {
            Some((view, _)) => Ok(view.used),
            None => Ok(0),
        }
    }

    /// How many bytes remain. A write needs its payload plus twelve bytes of
    /// entry framing to fit in this, unless it replaces a block already there.
    pub fn header_slots_free_bytes(&self) -> Result<usize> {
        Ok(self
            .header_slots_capacity()
            .saturating_sub(self.header_slots_used()?))
    }

    /// Whether the region has been sealed and accepts no further writes.
    ///
    /// Always `false` under [`HeaderSlotIntegrity::None`] and
    /// [`HeaderSlotIntegrity::Rolling`], which have no seal.
    pub fn header_slots_sealed(&self) -> Result<bool> {
        match self.header_slots_view()? {
            Some((view, _)) => Ok(view.is_sealed()),
            None => Ok(false),
        }
    }

    /// Reads a header-declared block out of the editable region.
    ///
    /// Decodes into `out`, reusing what it owns, and returns `true`. `false`
    /// (with `out` untouched) means no block with this id has been written —
    /// which is what a freshly created file answers for every declared id. Takes `&self` and
    /// performs no I/O: the region is part of the header this handle already
    /// holds, so this is a decode of bytes in memory.
    pub fn read_header_block<T: VarlitBlock>(&self, out: &mut T) -> Result<bool> {
        let Some((view, _)) = self.header_slots_view()? else {
            return Ok(false);
        };
        if !self.spec.header_slots.permits(T::ID) {
            return Err(Error::InvalidFormatSpec(
                "block is not declared in header_slots",
            ));
        }
        let mut found: Option<(u16, &[u8])> = None;
        walk_header_slot_entries(
            &self.spec,
            view.entries,
            view.used,
            |id, version, payload| {
                if id == T::ID {
                    found = Some((version, payload));
                }
                Ok(())
            },
        )?;
        let Some((version, payload)) = found else {
            return Ok(false);
        };
        if version != T::VERSION {
            return Err(Error::BlockVersionMismatch {
                block_id: T::ID,
                expected: T::VERSION,
                actual: version,
            });
        }
        MaterializationBudget::new(self.spec).decode_into::<T>(
            payload,
            T::ENDIAN.unwrap_or(self.spec.endian),
            out,
        )?;
        Ok(true)
    }

    /// Writes a header-declared block into the editable region, replacing any
    /// earlier value for the same block id.
    ///
    /// Replacement rather than append is what lets a fixed region take an
    /// unbounded number of edits: writing the same block a thousand times
    /// costs what writing it once costs. Two different blocks must both fit,
    /// and a write that would not is refused with
    /// [`Error::LimitExceeded`] *before* any byte is written — the region is
    /// never left half-updated by a rejected write.
    ///
    /// The region is rewritten in place at a fixed offset and length, so this
    /// moves nothing: the append log starts where it started, and every record
    /// offset already handed out stays valid.
    pub fn write_header_block<T: VarlitBlock>(&mut self, block: &T) -> Result<()> {
        let payload = encode_logical_payload_limited(
            self.spec,
            block,
            T::ENDIAN.unwrap_or(self.spec.endian),
        )?;
        self.write_header_slot_bytes(T::ID, T::VERSION, Some(&payload))
    }

    /// Removes a header-declared block from the editable region, freeing its
    /// bytes for another.
    ///
    /// `Ok(())` whether or not the block was there; the post-condition is that
    /// it is not.
    pub fn remove_header_block<T: VarlitBlock>(&mut self) -> Result<()> {
        self.write_header_slot_bytes(T::ID, T::VERSION, None)
    }

    /// Fixes the region's checksum and refuses every later write.
    ///
    /// This is the "compute and freeze" point of
    /// [`HeaderSlotIntegrity::Sealed`]: until it is called the region is
    /// editable and its checksum is advisory; after it, reads verify the
    /// checksum and writes fail with [`Error::InvalidFormatSpec`]. Sealing is
    /// one-way — nothing unseals a region, because a caller who could would
    /// have gained nothing over declaring [`HeaderSlotIntegrity::Rolling`].
    ///
    /// Refused on a format declaring any other integrity: `None` has no
    /// checksum to freeze and `Rolling` recomputes on every write, so a seal
    /// would mean something different in each and silently do nothing in one.
    pub fn seal_header_slots(&mut self) -> Result<()> {
        if self.spec.header_slots.integrity != HeaderSlotIntegrity::Sealed {
            return Err(Error::InvalidFormatSpec(
                "header_slots integrity is not `sealed`",
            ));
        }
        self.check_write()?;
        let Some((view, extent)) = self.header_slots_view()? else {
            return Err(Error::InvalidFormatSpec("format declares no header_slots"));
        };
        if view.is_sealed() {
            return Ok(());
        }
        walk_header_slot_entries(&self.spec, view.entries, view.used, |_, _, _| Ok(()))?;
        let used = view.used;
        let payload = encode_header_slots_payload(self.spec, used, view.entries, true)?;
        self.ensure_write()?
            .commit_header_slots_payload(extent, &payload)
    }

    /// The region's decoded view plus where it lives, or `None` when the
    /// format declares none.
    ///
    /// Verification happens here rather than at each entry, so every read path
    /// gets it: `Rolling` always checks, `Sealed` checks once sealed, `None`
    /// never does.
    fn header_slots_view(&self) -> Result<Option<(HeaderSlotsView<'_>, MutableHeaderExtent)>> {
        if !self.spec.header_slots.is_declared() {
            return Ok(None);
        }
        let header_len = append_log_start_for_file(self)?;
        let Some(extent) =
            mutable_header_block_extent(header_len, &self.header_extensions, HEADER_SLOTS_MAGIC)?
        else {
            return Ok(None);
        };
        let payload = self
            .header_extensions
            .get(extent.payload_in_extensions..extent.payload_in_extensions + extent.payload_len)
            .ok_or(Error::InvalidCompressionHeader)?;
        let view = decode_header_slots(self.spec, payload)?;
        let verify = match self.spec.header_slots.integrity {
            HeaderSlotIntegrity::None => false,
            HeaderSlotIntegrity::Rolling => true,
            HeaderSlotIntegrity::Sealed => view.is_sealed(),
        };
        if verify && view.checksum != view.expected_checksum(self.header_slots_capacity()) {
            return Err(Error::ChecksumMismatch {
                offset: extent.payload_offset,
            });
        }
        Ok(Some((view, extent)))
    }

    /// Rebuilds the entry region with `block_id` set to `payload` — or removed
    /// when it is `None` — and writes it back.
    fn write_header_slot_bytes(
        &mut self,
        block_id: u32,
        block_version: u16,
        payload: Option<&[u8]>,
    ) -> Result<()> {
        if !self.spec.header_slots.is_declared() {
            return Err(Error::InvalidFormatSpec("format declares no header_slots"));
        }
        if !self.spec.header_slots.permits(block_id) {
            return Err(Error::InvalidFormatSpec(
                "block is not declared in header_slots",
            ));
        }
        self.check_write()?;
        let capacity = self.header_slots_capacity();
        let Some((view, extent)) = self.header_slots_view()? else {
            return Err(Error::InvalidFormatSpec("format declares no header_slots"));
        };
        if view.is_sealed() {
            return Err(Error::InvalidFormatSpec("header_slots region is sealed"));
        }
        let sealed = view.is_sealed();

        // Measure before allocating/copying; an oversized value cannot make
        // temporary storage exceed the schema-pinned region capacity.
        let mut required = payload.map_or(0u64, |p| {
            p.len() as u64 + HEADER_SLOT_ENTRY_HEADER_LEN as u64
        });
        walk_header_slot_entries(&self.spec, view.entries, view.used, |id, _, existing| {
            if id != block_id {
                required += existing.len() as u64 + HEADER_SLOT_ENTRY_HEADER_LEN as u64;
            }
            Ok(())
        })?;
        if required > capacity as u64 {
            return Err(Error::LimitExceeded {
                resource: "header slots region",
                actual: required,
                limit: capacity as u64,
            });
        }
        let mut rebuilt: Vec<u8> = Vec::new();
        rebuilt
            .try_reserve_exact(capacity)
            .map_err(|_| Error::AllocationFailed {
                resource: "header slots region",
                requested: capacity as u64,
            })?;
        walk_header_slot_entries(
            &self.spec,
            view.entries,
            view.used,
            |id, version, existing| {
                if id == block_id {
                    return Ok(());
                }
                rebuilt.extend_from_slice(&id.to_le_bytes());
                rebuilt.extend_from_slice(&version.to_le_bytes());
                rebuilt.extend_from_slice(&0u16.to_le_bytes());
                rebuilt.extend_from_slice(&(existing.len() as u32).to_le_bytes());
                rebuilt.extend_from_slice(existing);
                Ok(())
            },
        )?;
        if let Some(payload) = payload {
            let len = u32::try_from(payload.len()).map_err(|_| Error::LimitExceeded {
                resource: "header slots block",
                actual: payload.len() as u64,
                limit: capacity as u64,
            })?;
            rebuilt.extend_from_slice(&block_id.to_le_bytes());
            rebuilt.extend_from_slice(&block_version.to_le_bytes());
            rebuilt.extend_from_slice(&0u16.to_le_bytes());
            rebuilt.extend_from_slice(&len.to_le_bytes());
            rebuilt.extend_from_slice(payload);
        }
        if rebuilt.len() > capacity {
            return Err(Error::LimitExceeded {
                resource: "header slots region",
                actual: rebuilt.len() as u64,
                limit: capacity as u64,
            });
        }
        let used = rebuilt.len();
        // Zero rather than leave: the padding is covered by the checksum, and
        // stale entry bytes past `used` would also be readable in a file dump
        // long after the block that wrote them was removed.
        rebuilt.resize(capacity, 0);
        self.ensure_write()?
            .commit_header_slots(extent, used, &rebuilt, sealed)
    }

    fn write_header_tails_if_needed(&mut self) {
        if self.mode != OpenMode::ReadWrite || !self.spec.index_policy.header_tails {
            return;
        }
        let Some(commit_offset) = self.block_tails.tail(COMMIT_BLOCK_ID) else {
            return;
        };
        let _ = self.write_header_tails(commit_offset);
    }

    fn write_header_tails(&mut self, commit_offset: u64) -> Result<()> {
        // The append log starts exactly where the header ends, matrix or not:
        // a matrix lives in records of that log, never between the header
        // and it, so this *is* the header length.
        let header_len = append_log_start_for_file(self)?;
        let Some(extent) =
            mutable_header_block_extent(header_len, &self.header_extensions, HEADER_TAILS_MAGIC)?
        else {
            return Ok(());
        };
        let capacity = header_tails_capacity(self.spec);
        let slot_len = header_tails_slot_len(capacity).ok_or(Error::InvalidFormatSpec(
            "header_tails region length overflows",
        ))?;
        if extent.payload_len != slot_len.saturating_mul(HEADER_TAILS_SLOTS) {
            // A region this build did not write. Leaving it alone is the only
            // safe answer: its geometry is not the one encoded here.
            return Ok(());
        }
        let generation = {
            let payload = self
                .header_extensions
                .get(
                    extent.payload_in_extensions..extent.payload_in_extensions + extent.payload_len,
                )
                .ok_or(Error::InvalidCompressionHeader)?;
            decode_header_tails_slots(payload)?
                .iter()
                .map(|slot| slot.generation)
                .max()
                .unwrap_or(0)
                .saturating_add(1)
        };
        let index = (generation % HEADER_TAILS_SLOTS as u64) as usize;
        let bytes = encode_header_tails_slot(
            capacity,
            generation,
            commit_offset,
            self.block_tails.as_slice(),
        )?;
        let offset = extent
            .payload_offset
            .checked_add((index * slot_len) as u64)
            .ok_or(Error::InvalidCompressionHeader)?;
        self.file.overwrite_header_region(offset, &bytes)?;
        // The cached region is what the *next* write reads its generation from,
        // and what a `replace_*` carries forward, so it has to follow the file.
        // Leaving it at the create-time bytes would make every commit write
        // generation 1 over the top of generation 1.
        let at = extent.payload_in_extensions + index * slot_len;
        self.header_extensions
            .get_mut(at..at + slot_len)
            .ok_or(Error::InvalidCompressionHeader)?
            .copy_from_slice(&bytes);
        Ok(())
    }

    /// Whether this commit point should write an open digest.
    ///
    /// The third condition is the one that took a bug to learn on the segment
    /// path: a digest is itself a record, so "is there anything to describe"
    /// has to mean "anything other than the digest I already wrote", or every
    /// idle flush appends another one and the file grows while nothing happens.
    fn needs_open_digest(&self) -> bool {
        if !self.spec.index_policy.open_digest_on_flush {
            return false;
        }
        if self.spec.commit_policy.is_transaction_marker()
            && self.has_uncommitted_since_last_commit()
        {
            return false;
        }
        // Two terms, and neither may look at the resident index.
        //
        // The first answers for this handle's own appends, resident or not.
        // The second carries the *open* case — a handle that has appended
        // nothing yet still needs a digest when the file it opened does not end
        // with one — and it must not fire when there is no last resident record
        // at all, which is both a freshly created file and a file whose every
        // block is non-resident.
        //
        // The guard this replaces was `self.index.is_empty()`, meaning "nothing
        // has been written yet". Right intent, wrong instrument: the resident
        // index is empty forever when every block is declared non-resident, so
        // such a file never got a first digest — and a non-resident block
        // reaches its records through the chain whose head the digest carries.
        // The option was inert for exactly the file it exists to serve.
        self.appended_since_digest
            || matches!(self.last_resident_block_id, Some(id) if id != OPEN_DIGEST_BLOCK_ID)
    }

    fn write_open_digest_if_needed(&mut self) {
        if self.mode != OpenMode::ReadWrite || !self.needs_open_digest() {
            return;
        }
        let _ = self.write_open_digest_record();
    }

    fn write_open_digest_record(&mut self) -> Result<()> {
        let record_offset = self.file.len()?;
        // The high-water mark as it will stand *once this record is down*: the
        // digest takes the next sequence itself, so reporting the state before
        // it would send the next writer back over a number this record used.
        let sequence_high_water = match self.sequence_state {
            // `Available(next)` means `next` is the number this digest record is
            // about to take, so once it is down the mark is `next` — not
            // `next - 1`, which is the state *before* it and is what this
            // computed for as long as the comment above forbade exactly that.
            // Latent while the digest route builds read-only handles; sequence
            // reuse the day a writer is seeded from one.
            SequenceState::Available(next) => Some(next),
            // Unreachable in practice — an exhausted writer cannot append the
            // digest either, `available()` refuses first — but representable
            // now, which it was not.
            SequenceState::Exhausted => Some(u64::MAX),
        };
        // Its own tail is not in the table it writes — it cannot be, the record
        // does not exist yet. From the second digest onward the table does
        // carry a digest tail: the *previous* one, because `block_tails` is
        // maintained for every block. So a digest open answers
        // `block_tail_offset(OPEN_DIGEST_BLOCK_ID)` with the previous digest
        // where a scan answers with this one. The id is internal and nothing
        // walks from it, so the two answers are equally usable — but they are
        // not the same answer, and the claim that this table omits the digest
        // entirely was true only of the first one. Nothing walks the digest
        // chain: the digest is found at the end of the file, not by following
        // one.
        let payload = encode_digest_payload(
            self.spec,
            sequence_high_water,
            self.block_tails.as_slice(),
            record_offset,
        )?;
        self.check_write()?;
        let info = self.ensure_write()?.write_record_with_prev_key(
            OPEN_DIGEST_BLOCK_ID,
            DIGEST_VERSION,
            RECORD_FLAG_INTERNAL,
            0,
            &payload,
            None,
        )?;
        debug_assert_eq!(info.record_offset, record_offset);
        Ok(())
    }

    fn write_embedded_manifest_if_needed(&mut self) -> Result<()> {
        if self.spec.manifest_policy != ManifestPolicy::Embedded
            || self.block_tails.tail(MANIFEST_BLOCK_ID).is_some()
        {
            return Ok(());
        }
        let payload = encode_schema_manifest(self.spec)?;
        self.write_record(
            MANIFEST_BLOCK_ID,
            MANIFEST_PAYLOAD_VERSION,
            RECORD_FLAG_INTERNAL,
            &payload,
        )?;
        Ok(())
    }

    fn write_commit_marker(&mut self) -> Result<AppendInfo> {
        self.check_write()?;
        if self.spec.commit_policy.single_barrier() {
            // The marker names what it commits. A handle's first record
            // establishes its anchor, so do that before reading the scope.
            let mut permit = self.ensure_write()?;
            if !permit.commit_scope_initialized() {
                permit.initialize_commit_scope()?;
            }
            let payload = permit.commit_scope_marker().encode(crate::Endian::Little);
            return permit.write_record_with_prev_key(
                COMMIT_BLOCK_ID,
                1,
                RECORD_FLAG_INTERNAL,
                0,
                &payload,
                None,
            );
        }
        self.ensure_write()?.write_record_with_prev_key(
            COMMIT_BLOCK_ID,
            1,
            RECORD_FLAG_INTERNAL,
            0,
            COMMIT_PAYLOAD_MAGIC,
            None,
        )
    }

    fn has_uncommitted_since_last_commit(&self) -> bool {
        self.uncommitted_since_commit
    }

}
