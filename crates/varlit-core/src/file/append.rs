// Native file append operations.

impl VarlitFile {
    pub fn spec(&self) -> FormatSpec {
        self.spec.ordinary_read()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn snapshot(&self) -> &SnapshotFile {
        &self.snapshot
    }

    pub fn mode(&self) -> OpenMode {
        self.mode
    }

    pub fn push<T: VarlitBlock>(&mut self, block: &T) -> Result<u64> {
        Ok(self.push_info(block)?.sequence)
    }

    /// Appends a block without a keyed predecessor link.
    ///
    /// a keyed block cannot go through this entry point when the
    /// format enables `keyed_offset_chain`. `T: VarlitBlock` exposes no key, so
    /// this path can only write `prev_same_key_offset = None`, which silently
    /// truncates the physical keyed chain as soon as a second record shares a
    /// key. Such calls are rejected with
    /// [`Error::KeyedChainRequiresKeyedApi`]; use the generated keyed writer
    /// method or [`VarlitFile::push_keyed_info`], both of which maintain the
    /// chain. The bounded streaming and indexed writers already reject the
    /// same combination.
    pub fn push_info<T: VarlitBlock>(&mut self, block: &T) -> Result<AppendInfo> {
        if self.spec.index_policy.keyed_offset_chain && T::IS_KEYED {
            return Err(Error::KeyedChainRequiresKeyedApi { block_id: T::ID });
        }
        self.push_with_prev_key_info(block, None)
    }

    /// Appends a keyed block through the generic API, linking it to the
    /// previous record with the same key.
    ///
    /// This is the generic equivalent of the generated `push_<block>` writer
    /// method. The predecessor comes from a per-block-id tail map that is
    /// built once from the resident index on first use for a given block type
    /// (`O(N)` decode of that block's records, exactly what the generated
    /// writer pays at open) and then maintained in O(1) per append. Formats
    /// without `keyed_offset_chain` skip the map entirely.
    ///
    /// Interleaving this with [`VarlitFile::push_with_prev_key_info`] for the
    /// same block id is supported but drops the cached map: a caller-supplied
    /// link carries no key the file can observe, so the next call here rebuilds.
    pub fn push_keyed<T>(&mut self, block: &T) -> Result<u64>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        Ok(self.push_keyed_info(block)?.sequence)
    }

    /// Appends a keyed block through the generic API, linking it to the
    /// previous record with the same key. See [`VarlitFile::push_keyed`].
    pub fn push_keyed_info<T>(&mut self, block: &T) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        // keyed generic entry points evaluate the compile-time
        // keyedness contract post-monomorphization.
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        if !self.spec.index_policy.keyed_offset_chain {
            return self.push_with_prev_key_info_unlinked(block, None);
        }
        let _permit = self.ensure_write()?;
        let key = encode_internal_key_payload::<T>(self.spec, &block.key())?;
        // every fallible part of the tail-cache update happens here,
        // strictly before the append that the update describes. See
        // [`VarlitFile::reserve_keyed_tail_slot`].
        let previous = self.reserve_keyed_tail_slot::<T>(&key)?;
        let info = self.push_with_prev_key_info_unlinked(block, previous)?;
        self.commit_keyed_tail(T::ID, key, info.record_offset);
        Ok(info)
    }

    /// Returns the maintained keyed tails for `T`, building them from the
    /// resident index the first time this block type is used.
    fn keyed_tail_map<T>(&mut self) -> Result<&mut KeyedTailMap>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        if !self.keyed_tails.tails.contains_key(&T::ID) {
            let spec = self.spec;
            crate::collections::ensure_registered_block::<T>(spec)?;
            let mut built = KeyedTailMap::default();
            let mut payload = Vec::new();
            let mut consume = |entry: RecordIndexEntry| -> Result<()> {
                let mut budget = MaterializationBudget::new(spec);
                let key = if entry.block_id == T::ID {
                    if entry.block_version != T::VERSION {
                        return Err(Error::BlockVersionMismatch {
                            block_id: T::ID,
                            expected: T::VERSION,
                            actual: entry.block_version,
                        });
                    }
                    budget.consume(entry.logical_payload_len_snapshot(spec, &self.snapshot)?)?;
                    entry.read_logical_payload_snapshot_into(spec, &self.snapshot, &mut payload)?;
                    let block: T = budget.decode(&payload, T::ENDIAN.unwrap_or(spec.endian))?;
                    Some(block.key())
                } else if entry.block_id == TOMBSTONE_BLOCK_ID {
                    budget.consume(entry.logical_payload_len_snapshot(spec, &self.snapshot)?)?;
                    entry.read_payload_snapshot_into(spec, &self.snapshot, &mut payload)?;
                    decode_internal_key_payload::<T>(spec.endian, &payload, &mut budget)?
                } else {
                    None
                };
                if let Some(key) = key {
                    let key = encode_internal_key_payload::<T>(spec, &key)?;
                    if let Some(offset) = built.get(&key)? {
                        let previous = fault_record_entry(&self.snapshot, spec, offset, true)?;
                        if (previous.sequence, previous.record_offset)
                            > (entry.sequence, entry.record_offset)
                        {
                            return Ok(());
                        }
                    }
                    built.reserve(spec, &key)?;
                    built.insert(key, entry.record_offset);
                }
                Ok(())
            };
            if self.index.is_retained() {
                for ordinal in 0..self.index.len() {
                    if !directory_skips(&self.index, ordinal, &[T::ID, TOMBSTONE_BLOCK_ID]) {
                        consume(self.index.record_at(ordinal)?)?;
                    }
                }
            } else {
                // Walk chains directly; no temporary directory proportional to
                // the records of a block is required to build the tail index.
                for id in [T::ID, TOMBSTONE_BLOCK_ID] {
                    for entry in self.block_chain(id)? {
                        consume(entry?)?;
                    }
                }
            }
            self.keyed_tails
                .tails
                .try_reserve(1)
                .map_err(|_| Error::AllocationFailed {
                    resource: "keyed tail cache",
                    requested: std::mem::size_of::<KeyedTailMap>() as u64,
                })?;
            self.keyed_tails.tails.insert(T::ID, built);
        }
        Ok(self.keyed_tails.tails.get_mut(&T::ID).unwrap())
    }

    /// Resolves `key`'s current predecessor *and* reserves the tail-cache slot
    /// the append that follows will occupy.
    ///
    /// The generic keyed mutation paths used to append first and only then
    /// grow the resident tail cache. That ordering could return `Err` from a
    /// cache reservation after the record or tombstone was already written,
    /// indexed and published in writer state: the caller read "nothing
    /// happened" while the record existed, and - worse - the stale cached
    /// predecessor survived, so the next generic keyed mutation on the same
    /// writer linked *around* the record that had in fact succeeded, silently
    /// truncating the physical keyed chain.
    ///
    /// Every fallible step - building the map from the resident index,
    /// the allocation arithmetic, and the `try_reserve` - therefore happens
    /// here, before the append. Only the infallible
    /// [`VarlitFile::commit_keyed_tail`] runs afterwards, so a generic keyed
    /// mutation can no longer fail after it has become authoritative.
    ///
    /// A key already present needs no capacity: its slot is overwritten in
    /// place.
    ///
    /// the growth is additionally *charged*, before it is taken,
    /// against [`ReadLimits::max_keyed_tail_bytes`]. `try_reserve` alone only
    /// reports an allocation the allocator refuses; the charge is what makes a
    /// large-but-satisfiable cache refusable by configured policy.
    ///
    /// The charged value is the map's inline storage,
    /// `(len + 1) * size_of::<(Vec<u8>, u64)>()`, plus the key payload bytes
    /// the map owns - here the payloads already resident plus the one about to
    /// be inserted. It is checked per keyed block id, not summed across block
    /// ids, and it excludes `HashMap` control bytes and load-factor slack.
    fn reserve_keyed_tail_slot<T>(&mut self, key: &[u8]) -> Result<Option<u64>>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        let spec = self.spec;
        let tails = self.keyed_tail_map::<T>()?;
        let previous = tails.get(key)?;
        if tails.offsets.contains_key(key) {
            return Ok(previous);
        }
        let requested = tails.charge_for(1, key.len() as u64)?;
        take_injected_keyed_tail_reservation_failure(requested)?;
        tails.reserve(spec, key)?;
        Ok(previous)
    }

    /// Records `key`'s new tail after the append became authoritative
    /// Infallible by construction.
    ///
    /// [`VarlitFile::reserve_keyed_tail_slot`] already reserved the slot, so the
    /// insert cannot allocate. If the reservation is nevertheless not
    /// observable - the map was dropped in between, or capacity is somehow
    /// gone - the cached map for this block id is *discarded* rather than left
    /// holding the superseded predecessor. Dropping it is allocation-free and
    /// always safe: the next generic keyed mutation rebuilds it from the
    /// resident index, which already contains this record. The one outcome
    /// this must never produce is a surviving stale predecessor.
    fn commit_keyed_tail(&mut self, block_id: u32, key: Vec<u8>, record_offset: u64) {
        let reservation_lost = take_injected_keyed_tail_commit_loss();
        match self.keyed_tails.tails.get_mut(&block_id) {
            Some(tails)
                if !reservation_lost
                    && (tails.offsets.contains_key(&key)
                        || tails.offsets.capacity() > tails.offsets.len()) =>
            {
                let payload_len = key.len() as u64;
                if tails.offsets.insert(key, record_offset).is_none() {
                    tails.key_payload_bytes = tails.key_payload_bytes.saturating_add(payload_len);
                }
            }
            _ => self.keyed_tails.invalidate(block_id),
        }
    }

    pub fn push_with_prev_key_info<T: VarlitBlock>(
        &mut self,
        block: &T,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        // the caller owns this block's keyed chain, so any tail map
        // this file cached for the block id can no longer be trusted - the
        // record's key is not observable here.
        self.keyed_tails.invalidate(T::ID);
        self.push_with_prev_key_info_unlinked(block, prev_same_key_offset)
    }

    fn push_with_prev_key_info_unlinked<T: VarlitBlock>(
        &mut self,
        block: &T,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        self.check_write()?;
        let mandatory = T::IMMEDIATE || block.immediate_if();
        self.ensure_user_block::<T>()?;
        if T::KIND == BlockKind::Matrix {
            return Err(Error::BlockKindMismatch {
                expected: BlockKind::Fixed,
                actual: T::KIND,
            });
        }
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        // Taken out and put back on every path, including the error one: the
        // buffer belongs to the writer, and a failed encode must not cost it.
        // Same shape as `StreamWriter::push_info`.
        let mut buffer = std::mem::take(&mut self.record_buffer);
        let result =
            match encode_logical_payload_limited_into(self.spec, block, endian, &mut buffer) {
                Ok(()) => self.ensure_write()?.write_user_record(
                    T::ID,
                    T::VERSION,
                    T::KIND,
                    &buffer,
                    prev_same_key_offset,
                ),
                Err(error) => Err(error),
            };
        self.record_buffer = buffer;
        let info = result?;
        self.apply_immediate::<T>(info, crate::ImmediateOperation::Append, mandatory)?;
        Ok(info)
    }

    /// Appends a tombstone for `key`, linking it to the previous record with
    /// the same key.
    ///
    /// this used to write `prev_same_key_offset = None` and truncate
    /// the physical keyed chain. It now resolves and maintains the predecessor
    /// through the same tail map as [`VarlitFile::push_keyed_info`].
    pub fn delete<T>(&mut self, key: &T::Key) -> Result<u64>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        Ok(self.delete_info::<T>(key)?.sequence)
    }

    /// Appends a tombstone for `key` and returns the full [`AppendInfo`].
    ///
    /// This is [`VarlitFile::delete`] with the append description the generated
    /// keyed writers return, and it is the single implementation both routes
    /// use. Its ordering is the one property that matters: every fallible step
    /// (the tail-map build, the key encoding, the slot charge and reservation)
    /// happens strictly before the tombstone is appended, and the only step
    /// after the authoritative mutation is the infallible, allocation-free,
    /// user-code-free `VarlitFile::commit_keyed_tail` (invariant 3).
    pub fn delete_info<T>(&mut self, key: &T::Key) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        if !self.spec.index_policy.keyed_offset_chain {
            return self.delete_with_prev_key_info_unlinked::<T>(key, None);
        }
        let _permit = self.ensure_write()?;
        // The tombstone payload *is* the canonical internal key payload, and
        // it is also the tail map's key. Encoding it once here and handing the
        // same bytes to both consumers keeps the delete path at one key
        // encoding per record; it used to encode twice.
        let key_payload = encode_internal_key_payload::<T>(self.spec, key)?;
        // reserve before the tombstone becomes authoritative; see
        // [`VarlitFile::reserve_keyed_tail_slot`].
        let previous = self.reserve_keyed_tail_slot::<T>(&key_payload)?;
        let info = self.write_tombstone_payload::<T>(&key_payload, previous)?;
        self.commit_keyed_tail(T::ID, key_payload, info.record_offset);
        Ok(info)
    }

    /// Builds the keyed tail map for `T` now rather than on first keyed
    /// mutation.
    ///
    /// Generated keyed writers call this at construction. The cache spills to
    /// a private temporary index when `max_keyed_tail_bytes` is reached; key
    /// cardinality does not refuse the open. Formats without `keyed_offset_chain`
    /// maintain no tail map, so this is a no-op for them.
    pub fn prime_keyed_tails<T>(&mut self) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        if !self.spec.index_policy.keyed_offset_chain {
            return Ok(());
        }
        self.keyed_tail_map::<T>()?;
        Ok(())
    }

    pub fn delete_with_prev_key_info<T>(
        &mut self,
        key: &T::Key,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
    {
        // the caller owns this block's keyed chain; drop the cached
        // tail map so a later maintained append rebuilds it.
        self.keyed_tails.invalidate(T::ID);
        self.delete_with_prev_key_info_unlinked::<T>(key, prev_same_key_offset)
    }

    fn delete_with_prev_key_info_unlinked<T>(
        &mut self,
        key: &T::Key,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
    {
        // keyed generic entry points evaluate the compile-time
        // keyedness contract post-monomorphization; the registration below
        // stays as the runtime backstop. This terminal site also covers the
        // VarlitFile::delete and VarlitReader/VarlitWriter delete wrappers.
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        let _permit = self.ensure_write()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let payload = encode_internal_key_payload::<T>(self.spec, key)?;
        self.write_tombstone_payload::<T>(&payload, prev_same_key_offset)
    }

    /// Appends a tombstone whose canonical internal key payload is already
    /// encoded.
    ///
    /// Split out of `delete_with_prev_key_info_unlinked` so the maintained
    /// delete path can encode the key exactly once and use the same bytes for
    /// the tail map and for the record.
    fn write_tombstone_payload<T>(
        &mut self,
        payload: &[u8],
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
    {
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        self.check_write()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let info = self.ensure_write()?.write_record_with_prev_key(
            TOMBSTONE_BLOCK_ID,
            1,
            RECORD_FLAG_INTERNAL,
            0,
            payload,
            prev_same_key_offset,
        )?;
        self.apply_immediate::<T>(info, crate::ImmediateOperation::Delete, T::IMMEDIATE)?;
        Ok(info)
    }

    pub fn push_op<T>(&mut self, key: &T::Key, op: &T::Op) -> Result<u64>
    where
        T: VarlitMerge,
    {
        let _permit = self.ensure_write()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let payload = encode_internal_op_payload::<T>(self.spec, key, op)?;
        self.write_record(OP_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, &payload)
    }

    pub fn write_metadata(&mut self, key: &str, value: &[u8]) -> Result<u64> {
        // Encoded as (String, Vec<u8>): an 8-byte length prefix per part.
        const METADATA_ENVELOPE_LEN: u64 = 16;
        let _permit = self.ensure_write()?;
        // reject an oversized entry with the typed limit error before
        // the caller's key and value are cloned or encoded.
        let entry_len = (key.len() as u64)
            .checked_add(value.len() as u64)
            .and_then(|len| len.checked_add(METADATA_ENVELOPE_LEN))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "metadata entry length",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::LogicalPayloadLen, entry_len)?;
        let payload = encode_logical_payload_limited(
            self.spec,
            &(key.to_string(), value.to_vec()),
            self.spec.endian,
        )?;
        self.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, &payload)
    }

    pub fn flush(&mut self) -> Result<()> {
        let _permit = self.ensure_not_poisoned()?;
        self.write_embedded_manifest_if_needed()?;
        // First, before the checkpoint and before the marker. A chunk is an
        // ordinary record: writing the chunk after the marker would leave it outside the
        // committed prefix, and writing it after the checkpoint would leave it out
        // of the index that checkpoint serialises.
        if self.mode == OpenMode::ReadWrite {
            self.write_open_chunk_record()?;
        }
        if self.mode == OpenMode::ReadWrite
            && self.spec.index_policy.checkpoint_on_flush
            && self.needs_index_checkpoint()
        {
            self.write_index_checkpoint()?;
        }
        if self.mode == OpenMode::ReadWrite
            && self.spec.commit_policy.marker_on_flush()
            && self.has_uncommitted_since_last_commit()
        {
            self.write_commit_marker()?;
        }
        // Last, and after the marker: the chain is found from the end of the
        // file, so anything appended behind this record hides it.
        self.close_commit_point();
        self.file.flush()?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<AppendInfo> {
        let _permit = self.ensure_write()?;
        if !self.spec.commit_policy.is_transaction_marker() {
            return Err(Error::InvalidFormatSpec(
                "commit markers require transaction_marker commit policy",
            ));
        }
        self.write_embedded_manifest_if_needed()?;
        self.write_open_chunk_record()?;
        if self.spec.index_policy.checkpoint_on_flush && self.needs_index_checkpoint() {
            self.write_index_checkpoint()?;
        }
        if !self.has_uncommitted_since_last_commit()
            && let Some(entry) = self.index.last_entry_of_block(COMMIT_BLOCK_ID)?
        {
            let info = AppendInfo::from(&entry);
            self.close_commit_point();
            return Ok(info);
        }
        let info = self.write_commit_marker()?;
        self.close_commit_point();
        Ok(info)
    }

    pub fn commit_durable(&mut self) -> Result<AppendInfo> {
        let _permit = self.ensure_write()?;
        if !self.spec.commit_policy.is_transaction_marker() {
            return Err(Error::InvalidFormatSpec(
                "commit markers require transaction_marker commit policy",
            ));
        }
        self.write_embedded_manifest_if_needed()?;
        self.write_open_chunk_record()?;
        if self.spec.index_policy.checkpoint_on_flush && self.needs_index_checkpoint() {
            self.write_index_checkpoint()?;
        }
        if !self.has_uncommitted_since_last_commit()
            && let Some(entry) = self.index.last_entry_of_block(COMMIT_BLOCK_ID)?
        {
            let info = AppendInfo::from(&entry);
            self.close_commit_point();
            self.file.flush()?;
            self.file.sync_all()?;
            // a durable commit on a file this handle created must
            // also establish the pathname; this happens once, not per commit.
            let publication = self.publish_matrix_generation();
            self.poison_after_started_matrix_error(publication)?;
            self.sync_created_pathname_once()?;
            self.immediate_state.reset();
            return Ok(info);
        }
        self.file.flush()?;
        self.file.sync_data()?;
        let info = self.write_commit_marker()?;
        // The segment closes the commit point, so it is appended after the
        // marker and before the durability request that makes both durable.
        // `close_commit_point` cannot report a failure for the same
        // reason the two steps below do not report theirs as a bare `Err`: the
        // commit has happened.
        self.close_commit_point();
        // INVARIANT 3: `write_commit_marker` is the authoritative commit. From
        // here the marker record is in the file and a reader that opens it
        // after a clean process exit sees the transaction as committed, so a
        // failure of the durability request that follows must not be reported
        // as a bare `Err` — that would entitle the caller to believe nothing
        // happened and re-run the transaction, appending a second marker for
        // work already recorded. Both remaining steps therefore report typed
        // published outcomes: content durability as
        // `CommittedButDurabilityUnproven`, and the created pathname's
        // directory entry as `PublishedButParentSyncPending`, which
        // `sync_created_pathname_once` already produces and which leaves the
        // request pending so a later `sync` retries it.
        if let Err(source) = self.flush_and_sync_all() {
            return Err(crate::fallible::boxed_error(
                source,
                Error::ErrorContextAllocationFailed {
                    context: crate::AllocationFailureContext::Committed {
                        sequence: info.sequence,
                    },
                },
                |source| Error::CommittedButDurabilityUnproven {
                    sequence: info.sequence,
                    source,
                },
            ));
        }
        let publication = self.publish_matrix_generation();
        self.poison_after_started_matrix_error(publication)?;
        self.sync_created_pathname_once()?;
        self.immediate_state.reset();
        Ok(info)
    }

    /// Flushes the buffered writer and syncs the file contents.
    ///
    /// Split out so the post-commit-marker durability request in
    /// [`Self::commit_durable`] has exactly one failure point to classify.
    fn flush_and_sync_all(&mut self) -> Result<()> {
        take_injected_commit_durability_failure()?;
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(())
    }

    fn publish_matrix_generation(&mut self) -> Result<()> {
        let Some(pages) = self.file.matrix_pages() else {
            return Ok(());
        };
        let previous_end = pages.head().native_eof;
        let end = if self.spec.commit_policy.is_transaction_marker()
            && self.has_uncommitted_since_last_commit()
        {
            // A matrix-only sync must not publish an append tail that writer
            // recovery would later truncate. Keep at least the old confirmed
            // EOF, including its segment/digest, and any newly written marker.
            match self.block_tails.tail(COMMIT_BLOCK_ID) {
                Some(offset) if offset >= previous_end => {
                    fault_record_entry(&self.snapshot, self.spec, offset, false)?
                        .checked_physical_end()?
                }
                _ => previous_end,
            }
        } else {
            self.file.metadata()?.len()
        };
        self.file.publish_matrix(end)
    }

    /// Requests durable persistence of everything written so far.
    ///
    /// This syncs the file's contents and, for a pathname this handle created,
    /// its parent directory entry exactly once. Without the second
    /// step a power loss could leave a fully synced object with no name on
    /// platforms that require an explicit directory sync, which would not be
    /// "durably persisted" in any useful sense; the atomic replacement path
    /// already syncs the parent and reports a pending parent sync, so creation
    /// now makes the same guarantee through the same machinery.
    ///
    /// The parent sync costs one directory fsync per created file, never
    /// recurs, and is not on the append path. A failed parent sync is reported
    /// as [`Error::PublishedButParentSyncPending`] - the contents are durable
    /// and the pathname is visible, only its directory entry's durability is
    /// unconfirmed - and leaves the request pending, so a later `sync` retries
    /// it.
    pub fn sync(&mut self) -> Result<()> {
        let _permit = self.ensure_not_poisoned()?;
        // A committed chunked cell lives in memory until its chunk is written,
        // so syncing the file without writing the chunk made `sync()` return `Ok` having
        // made nothing durable — while the same call on a region row did.
        if self.mode == OpenMode::ReadWrite {
            self.write_open_chunk_record()?;
            // The write is an ordinary record append, so it lands *past* any
            // digest or segment this file already ends with — and a digest that
            // is not the last record is one no lazy open can use. `flush` writes out
            // before it closes the commit point for exactly this reason; `sync`
            // written and stopped, silently demoting every later
            // `open_readonly_lazy` on the file to a full scan.
            //
            // Re-closing is cheap but no longer free, and the count matters:
            // the segment and the digest are each predicated on there being
            // something new to describe, so an idle close appends nothing. The
            // header tail region is not — it has no such predicate, because its
            // write is an overwrite of a fixed extent rather than an append, so
            // an idle close costs one seek and one slot-sized write — 176
            // bytes on a two-block format, 12 more per declared block; the 360
            // the author guide quotes is the whole two-slot region — and advances
            // the slot generation. It never grows the file and it is never on
            // the append path; `the_region_write_is_one_seek_per_commit_point`
            // is what holds that to a count.
            self.close_commit_point();
        }
        self.file.sync_all()?;
        if self.mode == OpenMode::ReadWrite
            && let Err(error) = self.publish_matrix_generation()
        {
            self.poison.poison();
            return Err(error);
        }
        self.sync_created_pathname_once()?;
        self.immediate_state.reset();
        Ok(())
    }

    /// Persist all preceding writes. Does not append a transaction commit marker.
    pub fn immediate(&mut self) -> Result<()> {
        self.sync()
    }

    /// Install OR-combined conditions for native user appends and deletions.
    pub fn set_immediate_policy(&mut self, policy: crate::ImmediatePolicy) -> Result<()> {
        let _permit = self.ensure_write()?;
        self.immediate_state.policy = policy.validate()?;
        Ok(())
    }

    fn apply_immediate<T: VarlitBlock>(
        &mut self,
        info: AppendInfo,
        operation: crate::ImmediateOperation,
        mandatory: bool,
    ) -> Result<()> {
        let bytes = self.snapshot.len().saturating_sub(info.record_offset);
        self.immediate_state.advance(1, bytes);
        let event = self
            .immediate_state
            .event(T::ID, operation, info, bytes, 0, 0);
        if (mandatory || self.immediate_state.policy.matches(event))
            && let Err(error) = self.immediate()
        {
            // Keyed wrappers have not updated their tail caches yet.
            self.poison.poison();
            return Err(crate::immediate::appended_immediate_error(
                info.sequence,
                error,
            ));
        }
        Ok(())
    }

    /// Makes a pathname created by this handle durable, at most once.
    fn sync_created_pathname_once(&mut self) -> Result<()> {
        if !self.pending_pathname_parent_sync {
            return Ok(());
        }
        match sync_parent_directory(&self.path) {
            Ok(()) => {
                self.pending_pathname_parent_sync = false;
                Ok(())
            }
            Err(error) => Err(crate::fallible::boxed_path_error(
                error,
                &self.path,
                Error::ErrorContextAllocationFailed {
                    context: crate::AllocationFailureContext::ParentSyncPending,
                },
                |path, source| Error::PublishedButParentSyncPending { path, source },
            )),
        }
    }

    pub fn inspect_writer_lock<P: AsRef<Path>>(path: P) -> Result<Option<WriterLockInfo>> {
        read_writer_lock_info(path.as_ref())
    }

    /// OS-object identity bytes of this writer's native file.
    ///
    /// Diagnostic scaffolding captures this at creation time so cleanup can
    /// refuse to delete a file that another process has since swapped in at
    /// the same pathname.
    pub(crate) fn native_object_identity(&self) -> Result<Vec<u8>> {
        self.file.object_identity()
    }

    /// Arms `count` injected post-publication parent-directory sync failures.
    ///
    /// Fault-testing hook only: each armed failure makes the next
    /// `replace.parent_sync` boundary return an error after a publication has
    /// already succeeded, which is otherwise hard to reproduce on demand.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_parent_sync_failures(count: u64) {
        INJECTED_PARENT_SYNC_FAILURES.store(count, std::sync::atomic::Ordering::Release);
    }

    /// Arms `count` injected indeterminate replacement failures.
    ///
    /// Fault-testing hook only: each armed failure makes the next atomic
    /// pathname publication fail with
    /// [`Error::ReplacePublicationIndeterminate`] before touching the target,
    /// which models an unreconciled Windows `ReplaceFileW` 1176/1177 outcome
    /// that cannot be produced on demand by a real filesystem.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_replace_indeterminate_failures(count: u64) {
        INJECTED_REPLACE_INDETERMINATE_FAILURES.store(count, std::sync::atomic::Ordering::Release);
    }

    /// Arms `count` injected rewrite-temp preparation failures (STO4-P2).
    ///
    /// Fault-testing hook only: each armed failure makes the next internal
    /// merge/compact publication fail immediately after creating its rewrite
    /// temp and before the scope that used to be responsible for deleting it.
    /// The observable contract is that no `.rewrite.` artifact survives.
    /// Thread-local.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_rewrite_temp_preparation_failures(count: u64) {
        INJECTED_REWRITE_TEMP_PREPARATION_FAILURES.with(|armed| armed.set(count));
    }

    /// Arms `count` injected generic keyed-tail reservation failures.
    ///
    /// Fault-testing hook only: each armed failure makes the next generic
    /// keyed push/delete fail its pre-append tail-cache reservation with
    /// [`Error::AllocationFailed`]. The observable contract under this fault is
    /// that the append did **not** happen, which is exactly what the previous
    /// append-then-reserve ordering could not guarantee. Thread-local.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_keyed_tail_reservation_failures(count: u64) {
        INJECTED_KEYED_TAIL_RESERVATION_FAILURES.with(|armed| armed.set(count));
    }

    /// Arms `count` injected post-append keyed-tail reservation losses
    ///
    ///
    /// Fault-testing hook only: each armed loss makes the next generic keyed
    /// push/delete behave as if the slot reserved before the append were no
    /// longer usable afterwards. The mutation still succeeds; the cached tail
    /// map for that block id must be discarded rather than left holding the
    /// superseded predecessor, so the following mutation still links to the
    /// record that succeeded. Thread-local.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_keyed_tail_commit_losses(count: u64) {
        INJECTED_KEYED_TAIL_COMMIT_LOSSES.with(|armed| armed.set(count));
    }

    /// Arms `count` injected post-commit-marker durability failures.
    ///
    /// Fault-testing hook only: each armed failure makes the next
    /// [`Self::commit_durable`] fail its `flush`/`sync_all` *after* the commit
    /// marker has been appended. The observable contract under this fault is
    /// that the caller receives [`Error::CommittedButDurabilityUnproven`]
    /// rather than a bare `Err`, because the marker bytes are in the file and
    /// re-running the transaction would append a second marker for work that
    /// is already recorded. Thread-local.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_commit_durability_failures(count: u64) {
        INJECTED_COMMIT_DURABILITY_FAILURES.with(|armed| armed.set(count));
    }

    /// Returns the cumulative number of resident index entries this thread's
    /// checkpoint flush-cadence machinery has examined.
    ///
    /// Fault-testing hook only: regression tests delta-measure this around a
    /// flush-per-record workload to pin the cadence work at O(1) per flush
    /// (near-linear cumulative touches) instead of the historical O(N) rescan
    /// per flush. The counter is thread-local so concurrently running tests
    /// cannot pollute each other's measurements.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn checkpoint_cadence_index_touches() -> u64 {
        CHECKPOINT_CADENCE_INDEX_TOUCHES.with(|touches| touches.get())
    }

    /// Returns the cumulative number of resident index entries this thread's
    /// block-offset-chain predecessor machinery has examined.
    ///
    /// Fault-testing hook only. The append path resolves predecessors from the
    /// maintained tail table and must never advance this counter, so a
    /// regression test can delta-measure an append window and assert zero for
    /// any record count; only wholesale index loads (open, recovery, rebind,
    /// rollback) pay one pass.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn block_tail_index_touches() -> u64 {
        BLOCK_TAIL_INDEX_TOUCHES.with(|touches| touches.get())
    }

    /// Returns the cumulative number of block-tail tuples this thread has
    /// displaced by inserting into the sorted tail vector.
    ///
    /// Fault-testing hook only. Wholesale tail construction must not advance
    /// this counter at all: it orders the distinct block ids once instead of
    /// inserting each first-seen id, so a reverse-ordered index no longer pays
    /// the `Theta(B^2)` movement the index-visit counter above cannot see.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn block_tail_entries_moved() -> u64 {
        BLOCK_TAIL_ENTRIES_MOVED.with(|moved| moved.get())
    }

    /// Returns the cumulative number of records this thread has framed - read a
    /// header, an extent and a footer for - while building an index.
    ///
    /// Fault-testing hook only. This is the unit `IndexPolicy::segment_on_flush`
    /// exists to reduce: a scan frames every record in the file, and a segment
    /// chain frames one record per commit point and no data record at all, so a
    /// regression test can delta-measure an open and tell the two apart by the
    /// count rather than by the wall clock.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn records_framed() -> u64 {
        RECORDS_FRAMED.with(|framed| framed.get())
    }

    /// Reads and clears the number of index-entry comparisons this thread has
    /// made resolving a rewritten record's chain predecessor.
    ///
    /// Fault-testing hook only. `replace_block` validates every rewritten
    /// record's predecessor against the prefix it has already written; a
    /// front-to-back scan of that growing prefix costs `Theta(N^2)` and the
    /// sorted lookup that replaced it costs `O(N log N)`, which a regression
    /// test tells apart by this count rather than by the clock.
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    #[doc(hidden)]
    pub fn take_replacement_predecessor_probes() -> u64 {
        REPLACEMENT_PREDECESSOR_PROBES.with(|probes| probes.replace(0))
    }

    /// Reads and clears the number of `fstat`s this thread has issued to bind
    /// a snapshot's length (`snapshot.rs`'s `checked_snapshot_bounds`).
    ///
    /// Fault-testing hook only. The append path rebinds through
    /// `SnapshotFile::with_written_len`, which proves `len <= physical_len`
    /// from the write that just returned, so an append window must leave this
    /// at zero for any record count. Open still pays one per snapshot bind,
    /// which is what tells a real fix apart from a neutered counter.
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    #[doc(hidden)]
    pub fn take_snapshot_bounds_fstats() -> u64 {
        crate::snapshot::take_snapshot_bounds_fstats()
    }

    /// Record bytes this thread has walked while scanning, read and cleared.
    ///
    /// Fault-testing hook only. An open that follows the on-disk segment chain
    /// walks the chain; an open that scans walks the file. This is the number
    /// that tells them apart.
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    #[doc(hidden)]
    pub fn take_open_scan_bytes() -> u64 {
        take_open_scan_bytes()
    }

    /// Reads and clears the number of `fstat`s this thread has issued through
    /// the writer's own record handle (`RecordFile::metadata`).
    ///
    /// Fault-testing hook only, and a sibling of the counter above rather than
    /// the same one: this is the `AppendSnapshot` end-of-file probe, which the
    /// writer now answers from the snapshot it maintains.
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    #[doc(hidden)]
    pub fn take_record_file_metadata_calls() -> u64 {
        record_file::take_record_file_metadata_calls()
    }

    /// Reads and clears this thread's count of `lseek`s `RecordFile` issued on
    /// its own handle.
    ///
    /// The sibling the metadata counter did not have. The append path's
    /// `AppendSnapshot` took its rollback cursor with a per-record
    /// `stream_position`, which is an `lseek`, which
    /// `an_append_window_issues_no_metadata_syscall` could not see because it
    /// counts `metadata` only.
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    #[doc(hidden)]
    pub fn take_record_file_seeks() -> u64 {
        record_file::take_record_file_seeks()
    }

    /// Reads and clears this thread's count of resident-index entries rebuilt
    /// from their record headers.
    ///
    /// The resident index stores an offset and a commit bit per record, not the
    /// entry — every read of an entry is one positional read of that record's
    /// header (`fault_record_entry`). So this counts what walking the directory
    /// actually costs, which no byte counter does: `take_open_scan_bytes`
    /// measures the open scan, and a directory walk after open is invisible to
    /// it.
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    #[doc(hidden)]
    pub fn take_record_entry_faults() -> u64 {
        take_record_entry_faults()
    }

    /// Cumulative bytes this thread has read to answer chunked cell reads.
    ///
    /// Fault-testing hook only. This is the unit a growing matrix's read path
    /// exists to keep small: a chunk is one record, so "read the cell" and
    /// "read the chunk" differ by orders of magnitude and a regression test can
    /// tell them apart by an absolute byte count rather than a ratio.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn chunk_bytes_read() -> u64 {
        CHUNK_BYTES_READ.with(|read| read.get())
    }

    /// The write-side guard: poison check plus open-mode check, returning the
    /// witness the guarded operations demand. See [`Self::ensure_not_poisoned`].
    fn check_write(&self) -> Result<()> {
        self.check_mutation(WRITER_POISON_CONTEXT)?;
        match self.mode {
            OpenMode::ReadWrite => Ok(()),
            OpenMode::ReadOnly => Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "file was opened read-only",
            ))),
        }
    }

    fn ensure_write(&mut self) -> Result<FileMutationPermit<'_>> {
        self.check_write()?;
        self.writer_permit(WRITER_POISON_CONTEXT)
    }

    /// The one poison check for this writer, and the only source of the
    /// witness its guarded operations demand.
    ///
    /// Shape B, mechanical enforcement. `PoisonFlag` owns the state
    /// and lives in `crate::writer_permit`, so nothing in this ~12000-line file
    /// can read or assign it. Every function that puts bytes into an already
    /// published generation — the append core, and the one in-place record
    /// rewrite — takes the [`MutationPermit`] this returns, by reference, so a
    /// mutating method written later cannot reach the disk without having asked
    /// for one. See `crate::writer_permit` for what that makes impossible.
    fn ensure_not_poisoned(&mut self) -> Result<FileMutationPermit<'_>> {
        self.writer_permit(WRITER_POISON_CONTEXT)
    }

    /// Handles a failed atomic publication for a copy-on-write writer.
    ///
    /// Pre-publication failures leave the target untouched, so the temp file
    /// is deleted and the writer stays usable. An indeterminate outcome
    /// ([`Error::ReplacePublicationIndeterminate`]) means the pathname state
    /// is unknown: the temp file is preserved for out-of-band reconciliation
    /// and the writer is poisoned so a blind retry cannot publish over an
    /// unknown generation.
    fn fail_publication(&mut self, temp_path: &Path, error: Error) -> Error {
        if matches!(error, Error::ReplacePublicationIndeterminate { .. }) {
            self.poison.poison();
        } else {
            let _ = remove_file(temp_path);
        }
        error
    }

    /// Classifies the outcome of a matrix mutation and poisons the writer when
    /// the failure could have left disk and memory disagreeing.
    ///
    /// INVARIANT 3. The narrow [`Error::Io`] predicate is **deliberate**, and
    /// round 10 re-derived it rather than inheriting it. The review's
    /// offered two corrections — prepare the allocation before the disk write,
    /// or poison on every error raised after on-disk mutation begins — and the
    /// first was taken. That choice is what makes this predicate correct: every
    /// disk write in `crate::matrix` is now paired with an *infallible*
    /// in-memory install (`commit_byte_write`, `commit_current_write_bit`, the
    /// page-index mirror resynchronisation), so a matrix mutation is a sequence
    /// of individually atomic sub-steps. A typed refusal raised between two of
    /// them — `LimitExceeded` from a later page charge, `AllocationFailed` from
    /// a later first-touch page, `InvalidMatrixLayout` from later geometry —
    /// leaves the file and memory agreeing on everything the earlier sub-steps
    /// did, so the writer stays usable and the caller can retry. That contract
    /// is asserted by
    /// `matrix_integrity_scaling.rs::a_failed_page_allocation_cannot_leave_a_bit_on_disk`,
    /// which injects exactly such a failure at both first-touch boundaries and
    /// requires the same writer to keep working.
    ///
    /// Poisoning on *any* post-write error was implemented and rejected on that
    /// evidence: it converts those clean refusals into an unusable writer.
    /// `Error::Io` remains the right trigger because a failed write is the one
    /// case where what reached the file is unknown.
    ///
    /// The obligation this leaves on future work is therefore not "widen the
    /// predicate" but "keep the pairing": any new fallible step placed *between*
    /// a matrix disk write and its in-memory twin reopens , and must be
    /// moved ahead of the write instead.
    fn finish_matrix_mutation<T>(&mut self, result: Result<T>) -> Result<T> {
        if matches!(&result, Err(Error::Io(_))) {
            self.poison.poison();
        }
        result
    }

    /// A failed durability operation leaves the writer unfit to continue.
    /// Barrier callbacks precede generation publication and propagate their
    /// original errors. Publication/hook outcomes are classified by the caller.
    fn poison_after_started_matrix_error<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.poison.poison();
        }
        result
    }

    fn ensure_user_block<T: VarlitBlock>(&self) -> Result<()> {
        if T::ID >= RESERVED_BLOCK_ID_START {
            return Err(Error::ReservedBlockId(T::ID));
        }
        Ok(())
    }

    fn publish_sequence(&mut self, sequence: u64) {
        debug_assert_eq!(self.sequence_state, SequenceState::Available(sequence));
        self.sequence_state = SequenceState::after_publishing(sequence);
    }

    #[cfg(test)]
    fn push_info_for_test(&mut self, block_id: u32, payload: &[u8]) -> Result<AppendInfo> {
        self.check_write()?;
        self.ensure_write()?
            .write_record_with_prev_key(block_id, 1, 0, 0, payload, None)
    }

    pub(crate) fn write_record(
        &mut self,
        block_id: u32,
        block_version: u16,
        flags: u16,
        payload: &[u8],
    ) -> Result<u64> {
        self.check_write()?;
        Ok(self
            .ensure_write()?
            .write_record_with_prev_key(block_id, block_version, flags, 0, payload, None)?
            .sequence)
    }

    fn rollback_append(&mut self, snapshot: AppendSnapshot, operation_error: Error) -> Error {
        // The tail states are advanced only after the record is fully written,
        // so this normally truncates nothing; rebuilding when it does keeps
        // the `BlockTails::from_index` invariant unconditional.
        let truncated = self.index.len() > snapshot.index_len;
        self.index.truncate(snapshot.index_len);
        // Restore the one tail this append moved rather than rebuilding the
        // table from the resident index.
        //
        // **Unreachable today, and kept anyway.** Both rollback points are
        // above `block_tails.note_appended`, so a rollback always finds the
        // tail where it started and this restores it to itself — exactly as
        // the rebuild it replaces never actually ran, for the same reason.
        // What changed is which one is *correct if the ordering ever moves*:
        // rebuilding from the resident index drops a non-resident block's tail
        // entirely, and that tail is the only way back to its records. This is
        // also `O(1)` and allocation-free, which the rebuild was not
        //
        self.block_tails
            .restore(snapshot.block_id, snapshot.previous_block_tail);
        self.uncommitted_since_commit = snapshot.uncommitted_since_commit;
        self.appended_since_digest = snapshot.appended_since_digest;
        self.last_resident_block_id = snapshot.last_resident_block_id;
        if truncated {
            self.keyed_tails.invalidate_all();
        }
        self.sequence_state = snapshot.sequence_state;
        self.checkpoint_cadence = snapshot.checkpoint_cadence;
        self.segment_cursor = snapshot.segment_cursor;

        #[cfg(test)]
        let truncate_result =
            fail_rollback_if_requested().and_then(|()| self.file.set_len(snapshot.eof));
        #[cfg(not(test))]
        let truncate_result = self.file.set_len(snapshot.eof);
        // Back to the end of file this append started from — the offset the
        // truncate above just restored, and the one the next append's
        // `SEEK_END` would land on anyway. See `AppendSnapshot::eof`.
        let cursor_result = self.file.seek_to(snapshot.eof).map(|_| ());
        if let Some(source) = truncate_result.err().or_else(|| cursor_result.err()) {
            self.poison.poison();
            Error::WriteRollbackFailed {
                operation: "append record",
                source,
            }
        } else {
            operation_error
        }
    }

    /// The payload length a full checkpoint of the current index would need.
    fn index_checkpoint_payload_len(&self) -> Result<u64> {
        const CHECKPOINT_PREFIX_LEN: u64 = 4 + 2 + 8 + 8;
        let entry_count =
            u64::try_from(self.index.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "checkpoint entry count",
            })?;
        entry_count
            .checked_mul(SEGMENT_ENTRY_LEN)
            .and_then(|bytes| bytes.checked_add(CHECKPOINT_PREFIX_LEN))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "checkpoint payload length",
            })
    }

    /// Whether a full checkpoint of the current index would fit the format's
    /// own record ceilings.
    ///
    /// The index only grows, so once this is false it stays false and the
    /// answer costs one multiply per flush. Every way it can be false is a
    /// reason `write_index_checkpoint` would refuse, which is why the decision
    /// belongs here: a checkpoint is derived, and declining to write one costs
    /// the next open a scan it already knows how to do.
    fn index_checkpoint_fits(&self) -> bool {
        let Ok(payload_len) = self.index_checkpoint_payload_len() else {
            return false;
        };
        let limits = &self.spec.read_limits;
        u64::try_from(self.index.len())
            .is_ok_and(|count| limits.check(ReadLimitKey::Records, count).is_ok())
            && limits
                .check(ReadLimitKey::RecordPayloadLen, payload_len)
                .is_ok()
            && limits
                .check(ReadLimitKey::LogicalPayloadLen, payload_len)
                .is_ok()
    }

    fn write_index_checkpoint(&mut self) -> Result<u64> {
        const CHECKPOINT_PREFIX_LEN: u64 = 4 + 2 + 8 + 8;
        const CHECKPOINT_ENTRY_LEN: u64 = 4 + 2 + 2 + 8 + 8 + 8 + 8 + 4 + 4 + 8 + 8 + 8 + 1;
        let covered_offset = self.file.metadata()?.len();
        let entry_count =
            u64::try_from(self.index.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "checkpoint entry count",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Records, entry_count)?;
        let payload_len = entry_count
            .checked_mul(CHECKPOINT_ENTRY_LEN)
            .and_then(|bytes| bytes.checked_add(CHECKPOINT_PREFIX_LEN))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "checkpoint payload length",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::LogicalPayloadLen, payload_len)?;
        let mut payload = Vec::new();
        let payload_capacity = usize::try_from(payload_len)
            .map_err(|_| Error::LengthOverflow { value: payload_len })?;
        payload
            .try_reserve_exact(payload_capacity)
            .map_err(|_| Error::AllocationFailed {
                resource: "checkpoint payload",
                requested: payload_len,
            })?;
        payload.extend_from_slice(INDEX_CHECKPOINT_MAGIC);
        payload.extend_from_slice(&INDEX_CHECKPOINT_VERSION.to_le_bytes());
        payload.extend_from_slice(&covered_offset.to_le_bytes());
        payload.extend_from_slice(&entry_count.to_le_bytes());
        for entry in self.index.iter() {
            let entry = entry?;
            push_index_entry_bytes(&mut payload, &entry, entry.committed);
        }
        self.write_record(INDEX_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, &payload)
    }

    /// Appends the segment record that closes this commit point.
    ///
    /// It must be the last record the commit point writes. Open finds the
    /// chain by reading the last record footer in the file, so any record
    /// appended after this one hides it — which costs the next open a full
    /// scan, never correctness.
    fn write_segment_record(&mut self) -> Result<()> {
        let start = self.segment_cursor.next_position;
        debug_assert!(start <= self.index.len());
        let covered_start = match self.segment_cursor.covered_start {
            Some(offset) => offset,
            None => append_log_start_for_file(self)?,
        };
        let preceding_records =
            u64::try_from(start).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "segment entry count",
            })?;
        // The record lands at the current end of file, and the trailer has to
        // name that offset before the header that would state it exists. The
        // append core budgets the same value from the same source and refuses
        // to write anywhere else, so the two cannot disagree.
        let record_offset = self.file.metadata()?.len();
        let payload = encode_segment_payload(
            self.spec,
            self.index.iter().skip(start),
            covered_start,
            preceding_records,
            record_offset,
        )?;
        self.check_write()?;
        let info = self.ensure_write()?.write_record_with_prev_key(
            SEGMENT_BLOCK_ID,
            SEGMENT_VERSION,
            RECORD_FLAG_INTERNAL,
            0,
            &payload,
            None,
        )?;
        debug_assert_eq!(info.record_offset, record_offset);
        Ok(())
    }

    /// Whether this commit point should close a segment.
    ///
    /// Three conditions, and the third is the one that matters: a segment is
    /// only written once everything it would cover is committed. Under a
    /// transaction-marker policy that means the commit marker is already down.
    /// A segment that covered uncommitted records would outlive them — the next
    /// writer open truncates them — and would then describe a file that no
    /// longer exists.
    fn needs_index_segment(&self) -> bool {
        // The digest, when both options are on, is appended *after* the segment
        // and is itself a resident record — so it advances `index.len()` past
        // the cursor and would make the next idle flush think a commit point
        // was left uncovered. It would then write a segment, which makes the
        // digest stale, which writes another digest, and the file grows forever
        // on a handle nothing is appending to. Only a trailing digest can be in
        // this position, so subtracting it is the whole correction.
        let trailing_digest =
            usize::from(self.last_resident_block_id == Some(OPEN_DIGEST_BLOCK_ID));
        self.spec.index_policy.segment_on_flush
            && self.segment_cursor.next_position < self.index.len() - trailing_digest
            && !(self.spec.commit_policy.is_transaction_marker()
                && self.has_uncommitted_since_last_commit())
    }

    /// Writes the segment record if one is due, and reports nothing if it
    /// could not be written.
    ///
    /// INVARIANT 3. Every caller runs this *after* the record its operation is
    /// accountable for is in the file, and for a commit point that record is
    /// the commit marker. The segment is derived from records that are already
    /// durable, so its failure cannot make the operation not have happened; the
    /// next open answers a missing or torn segment with the full scan. Turning
    /// it into an error here would tell a caller to re-run a transaction that
    /// is already committed.
    ///
    /// A failed append rolls itself back, and a failed *rollback* poisons the
    /// writer, so a failure this swallows is still visible: the next operation
    /// on this handle refuses.
    fn write_index_segment_if_needed(&mut self) {
        if self.mode != OpenMode::ReadWrite || !self.needs_index_segment() {
            return;
        }
        let _ = self.write_segment_record();
    }

    /// Appends whatever this commit point owes the *next open*, in the order
    /// the next open looks for it.
    ///
    /// Both are found from the end of the file, so both are last and their
    /// order is the reverse of the order they are read: the digest goes after
    /// the segment because the digest is the one probed at `file_len`. A
    /// segment appended behind it would hide it, which is the same reason both
    /// come after the commit marker.
    ///
    /// INVARIANT 3, inherited from `write_index_segment_if_needed`: every
    /// caller runs this after the record its operation is accountable for is in
    /// the file, and neither of these can make that record not have happened.
    /// A missing or torn one is answered by the next open with the full scan.
    fn close_commit_point(&mut self) {
        self.write_index_segment_if_needed();
        self.write_open_digest_if_needed();
        // Last, and deliberately after both, so the slot's tails include the
        // records this commit point itself appended.
        //
        // Not for the reader's sake: the slot names a commit *offset*, not a
        // file length, so a reader corroborating it walks forward from the same
        // marker either way and still lands on the end of the file — that is
        // what the segment arm of the walk is for. What the order buys is the
        // slot's own `SEGMENT` entry. Written first, it would name the previous
        // commit point's segment, and corroboration cannot catch that: a stale
        // segment offset still frames as a segment record. It is only wrong as
        // a chain link, which is the same silent breakage a stale table causes
        // anywhere else.
        // `the_slot_names_the_segment_from_its_own_commit_point` in
        // `header_tails_segment_combo.rs` is the assertion that holds this.
        self.write_header_tails_if_needed();
    }

    /// Rewrites the header tail slot for the commit point just closed.
    ///
    /// Failure is swallowed for the same reason the segment's and the digest's
    /// are: the commit marker is already in the file, so the transaction has
    /// happened and reporting an error here would entitle the caller to re-run
    /// it. A slot that was not written is a slot a later open does not adopt —
    /// it names an older commit marker, the forward walk does not reach the end
    /// of the file, and the open falls back to the scan. Slow, never wrong.
    /// Writes the liveness block's flags in place.
    ///
    /// Two callers and two values: a writer open sets `DIRTY`, and a clean
    /// release clears it. Everything between is unchanged, which is the point —
    /// this is not a heartbeat, it is a bracket.
    fn write_liveness_flags(&mut self, flags: u16) -> Result<()> {
        if self.spec.liveness_policy != LivenessPolicy::FooterFlags {
            return Ok(());
        }
        let header_len = append_log_start_for_file(self)?;
        let Some(extent) =
            mutable_header_block_extent(header_len, &self.header_extensions, LIVENESS_MAGIC)?
        else {
            return Ok(());
        };
        let block = encode_liveness_block(flags);
        // The payload only: the framing this seeks past is the magic and the
        // declared length, which must not move.
        self.file
            .overwrite_header_region(extent.payload_offset, &block[8..])?;
        // Kept in step so a later read of this handle's own cached region does
        // not disagree with the file it just wrote.
        let start = extent.payload_in_extensions;
        if let Some(window) = self
            .header_extensions
            .get_mut(start..start + LIVENESS_PAYLOAD_LEN)
        {
            window.copy_from_slice(&block[8..]);
        }
        Ok(())
    }

    /// Takes the object: records whether the *previous* writer released it
    /// cleanly, then marks it held by this one.
    ///
    /// Order matters and is the whole content of the mechanism — the read has
    /// to happen before the write, or every open reports itself as the crash it
    /// is looking for. Called only from a read-write open, because the question
    /// "was this abandoned" is only answerable by somebody who could take the
    /// object: a reader that finds the bit set cannot tell an abandoned file
    /// from one a healthy writer is appending to right now.
    fn mark_writer_open(&mut self) -> Result<()> {
        if self.spec.liveness_policy != LivenessPolicy::FooterFlags {
            return Ok(());
        }
        let header_len = append_log_start_for_file(self)?;
        self.opened_after_crash = decode_liveness_flags(header_len, &self.header_extensions)?
            .is_some_and(|flags| flags & LIVENESS_FLAG_DIRTY != 0);
        self.write_liveness_flags(LIVENESS_FLAG_DIRTY)
    }

    /// Marks a crashed writer's uncommitted tail dead, then commits it.
    ///
    /// Called once, at the open that discovers the crash, and only under
    /// `RecoveryPolicy::MarkTail`. `from` is the end of the committed prefix —
    /// the point `TruncateTail` would have cut at.
    ///
    /// **The commit marker at the end is the load-bearing part.** Marking alone
    /// would leave the file ending past its last marker, and the next marker
    /// written would make `committed_prefix_len` treat this whole tail as
    /// committed — records the original writer never committed, becoming
    /// visible retroactively. Committing them *here*, flagged dead, is the
    /// honest statement: they arrived, they were never committed by their
    /// writer, they are kept, and a defragmenting rewrite may drop them.
    ///
    /// Costs two framing passes over the tail, four bytes per user record and
    /// one appended record. No prefix rescans or record-count-sized target list.
    fn recover_marked_tail(&mut self, from: u64) -> Result<usize> {
        // Walked directly rather than through `scan_records_range_at_len`, and that is
        // not a shortcut. Under a marker policy the scan applies
        // `committed_prefix_len` to whatever it framed and **clears the list**
        // when no marker is in range — which is exactly this range, by
        // definition. That behaviour is right for `follow`, whose question is
        // "what is committed"; here the question is the opposite one.
        let end = self.snapshot.len();
        let mut offset = from;
        let mut marked = 0usize;
        let mut framed_end = from;
        let mut highest: Option<u64> = None;
        while offset < end {
            let Ok(entry) = fault_record_entry(&self.snapshot, self.spec, offset, false) else {
                // A half-written record is the expected find in a crashed
                // writer's tail, not a surprise. Everything before it framed and
                // is kept; this and anything after it never became a record and
                // is cut below.
                break;
            };
            let Ok(next) = entry.checked_physical_end() else {
                break;
            };
            if next > end || next <= offset {
                break;
            }
            // The kept tail's sequences are *higher* than anything the handle
            // knows about: its high-water mark came from the committed prefix,
            // which the scan truncated at the marker. Without this the marker
            // appended below reuses a sequence a kept record already holds, and
            // the next open refuses the file with "duplicate native record
            // sequence" — measured, before this line existed.
            highest = highest.max(Some(entry.sequence));
            framed_end = next;
            offset = next;
        }
        if let Some(highest) = highest {
            let next = highest
                .checked_add(1)
                .map_or(SequenceState::Exhausted, SequenceState::Available);
            if let (SequenceState::Available(now), SequenceState::Available(want)) =
                (self.sequence_state, next)
                && want > now
            {
                self.sequence_state = next;
            } else if matches!(next, SequenceState::Exhausted) {
                self.sequence_state = next;
            }
        }
        // Cut whatever did not frame. Leaving it would put unframeable bytes
        // *before* the marker appended below, which every later scan would walk
        // into — worse than the truncation this policy exists to avoid, because
        // it would take the framed records with it.
        if framed_end < end {
            self.file.set_len(framed_end)?;
            self.snapshot = self.snapshot.with_len(framed_end)?;
            let snapshot = self.snapshot.clone();
            self.index.rebind(snapshot);
        }
        // The first walk established this exact contiguous extent. Reuse its
        // boundary witness in a second streaming pass, without an input-sized
        // target vector or one prefix scan per marked record.
        offset = from;
        while offset < framed_end {
            let entry =
                fault_record_entry_prefixed::<128>(&self.snapshot, self.spec, offset, false)?;
            let next = entry.checked_physical_end()?;
            if next <= offset || next > framed_end {
                return Err(Error::CorruptTail { offset });
            }
            validate_record_entry(&self.spec, &entry)?;
            if entry.block_id < RESERVED_BLOCK_ID_START {
                self.write_framed_record_flags(&entry, RECORD_MUTABLE_FLAG_DEAD)?;
                marked += 1;
            }
            offset = next;
        }
        // `write_commit_marker` directly, not `commit()`. `commit()` reuses the
        // existing marker when nothing has been appended since the last one —
        // and nothing has been, because marking rewrites footers rather than
        // appending. That short-circuit is right for an ordinary commit and
        // exactly wrong here: the file ends past its last marker precisely
        // because this handle appended nothing, and leaving it that way is the
        // retroactive-commit hazard the marker exists to close.
        //
        // Unconditional, including when zero records were marked: an empty tail
        // that framed nothing still moved the end past the marker.
        self.write_commit_marker()?;
        self.close_commit_point();
        Ok(marked)
    }

    /// Whether the writer that held this file before this handle died without
    /// releasing it.
    ///
    /// The durable half of a crash verdict, and the half varlit could not answer
    /// before:
    /// the other signal — "does the file end exactly at a commit point" — is
    /// positional, describes only a suffix, and is truncated away by this very
    /// open before anyone can ask.
    ///
    /// `false` for a read-only handle by construction: answering needs the
    /// object lock this handle does not hold, and a set bit on a file a healthy
    /// writer is appending to is not a crash.
    pub const fn opened_after_crash(&self) -> bool {
        self.opened_after_crash
    }

    /// Marks a clean release, and makes it durable before returning.
    ///
    /// The sync matters here and nowhere else in this pair: a cleared bit that
    /// never reaches the platter reads as a crash, which costs a recovery pass
    /// that had nothing to do. The *set* bit needs no such care, because the
    /// direction it errs in is the safe one.
    fn mark_writer_closed(&mut self) -> Result<()> {
        if self.spec.liveness_policy != LivenessPolicy::FooterFlags {
            return Ok(());
        }
        self.write_liveness_flags(0)?;
        self.file.sync_data()?;
        Ok(())
    }
}
