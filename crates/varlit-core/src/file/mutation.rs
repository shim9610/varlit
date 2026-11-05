// Native file mutation operations.

impl VarlitFile {
    /// Replaces a native user record while preserving its sequence number.
    ///
    /// The new generation is validated and synced before atomic publication.
    /// Existing readers remain attached to the old generation.
    /// Rewrites the file without the records marked dead.
    ///
    /// Publishes by the same route `replace_*` uses — a temp file in the same
    /// directory, synced, then renamed over the pathname — so a reader open
    /// across it keeps its own generation whole and `is_current` /
    /// `reopen_readonly` are how it moves. The cost of that route is peak disk:
    /// both generations exist at once.
    ///
    /// Every internal record is kept. Commit markers stay where they are, and
    /// the derived ones — index checkpoints, segments, open digests — are
    /// rebuilt from the records already written rather than copied, because
    /// their payloads *are* record offsets and this moves them.
    ///
    /// # The part that is not a delta
    ///
    /// `replace_block` resizes one record and removes none, so a single
    /// subtraction translates every chain link. Here each survivor moves by a
    /// different amount and some predecessors are gone, so the links are
    /// remapped through a table per chain. Substituting a uniform delta does
    /// not publish a corrupt file: it is refused at write time with
    /// `InvalidRecordFooter`, because every link is resolved against the prefix
    /// already written and must name an earlier record of the same block.
    /// Measured, rather than assumed.
    ///
    /// A link whose whole chain was dropped is **cleared**, not pointed
    /// somewhere plausible.
    ///
    /// # Cost
    ///
    /// One rewritten generation and private page-compressed metadata. The
    /// directory and chain remapping share the existing index buffer budget;
    /// their temporary disk storage scales with record count. Publication
    /// adopts the completed directory without writing a second copy. It needs
    /// the resident directory, so a lazily opened handle refuses by name.
    pub fn defragment(&mut self) -> Result<DefragmentReport> {
        let _permit = self.ensure_write()?;
        if self.spec.liveness_policy != LivenessPolicy::FooterFlags {
            return Err(Error::InvalidFormatSpec(
                "defragment requires liveness: footer_flags — without it no record is ever dead",
            ));
        }
        if !self.spec.layout.is_varlit_native_default() {
            return Err(Error::InvalidFormatSpec(
                "defragment is not supported for custom physical layouts",
            ));
        }
        if self.matrix.is_some() {
            return Err(Error::InvalidFormatSpec(
                "defragment is not supported for matrix storage",
            ));
        }
        let _ = self.resident_directory("defragment")?;
        self.ensure_generation_rewrite_allowed()?;

        let bytes_before = self.file.metadata()?.len();
        let mut new_index = RewriteDirectory::new(self.spec);
        // One entry per record, per chain: a survivor maps its old offset to
        // its new one, and a dropped record inherits whatever its own
        // predecessor in that chain resolved to. Absent means "nothing in this
        // chain survived".
        let mut remap = OffsetRemap::new(self.spec);

        let rewrite_append_start = append_log_start_for_file(self)?;
        let mut segment_start = 0usize;
        let mut segment_covered_start: Option<u64> = None;
        let mut dropped = 0usize;
        let (temp_path, mut temp_file) = create_rewrite_temp_file(&self.path)?;
        let prepare_result = (|| -> Result<()> {
            if let Ok(metadata) = self.file.metadata() {
                temp_file.set_permissions(metadata.permissions())?;
            }
            let replacement_extensions =
                header_extensions_for_replacement(self.spec, &self.header_extensions)?;
            write_native_file_header(&mut temp_file, self.spec, &replacement_extensions)?;
            let mut source_end = rewrite_append_start;
            for source_entry in self.index.iter() {
                let source_entry = source_entry?;
                let old_offset = source_entry.record_offset;
                // Prove directory coverage while copying, including records
                // dropped below. Serialized rows cannot skip bytes or name an
                // embedded header. This reuses headers already read here.
                if old_offset != source_end {
                    return Err(Error::InvalidCanonicalEncoding(
                        "defragment source directory is not contiguous",
                    ));
                }
                source_end = source_entry.checked_physical_end()?;
                // Internal records carry commit and navigation state, never
                // user liveness. Refuse a damaged mark before publishing a
                // generation that could erase its commit boundary.
                if source_entry.block_id >= RESERVED_BLOCK_ID_START && source_entry.is_dead() {
                    return Err(Error::InvalidRecordFooter {
                        offset: source_entry.footer_offset.unwrap_or(old_offset),
                    });
                }
                if source_entry.is_dead() {
                    // Dropped. A link that pointed here has to reach past it to
                    // whatever this record itself pointed at, in the same
                    // chain — which is already resolved, because predecessors
                    // are written before their successors.
                    let block = source_entry
                        .prev_same_block_offset
                        .map(|offset| remap.get(offset, Chain::Block))
                        .transpose()?
                        .flatten();
                    let key = source_entry
                        .prev_same_key_offset
                        .map(|offset| remap.get(offset, Chain::Key))
                        .transpose()?
                        .flatten();
                    remap.push(old_offset, block, key)?;
                    dropped += 1;
                    continue;
                }
                let mut updated = source_entry.clone();
                let checkpoint_payload;
                let payload = if source_entry.block_id == INDEX_BLOCK_ID {
                    RewritePayload::Index(IndexPayload {
                        entries: &new_index,
                        start: 0,
                        covered_start: None,
                        record_offset: temp_file.stream_position()?,
                    })
                } else if source_entry.block_id == SEGMENT_BLOCK_ID {
                    let record_offset = temp_file.stream_position()?;
                    RewritePayload::Index(IndexPayload {
                        entries: &new_index,
                        start: segment_start,
                        covered_start: Some(segment_covered_start.unwrap_or(rewrite_append_start)),
                        record_offset,
                    })
                } else if source_entry.block_id == OPEN_DIGEST_BLOCK_ID {
                    let record_offset = temp_file.stream_position()?;
                    checkpoint_payload = rebuilt_open_digest_payload(
                        self.spec,
                        &new_index,
                        source_entry.sequence,
                        record_offset,
                    )?;
                    RewritePayload::Bytes(&checkpoint_payload)
                } else {
                    RewritePayload::Snapshot {
                        offset: source_entry.payload_offset,
                        len: source_entry.payload_len,
                    }
                };
                updated = rewrite_replacement_record_streaming(
                    self.spec,
                    &self.snapshot,
                    &mut temp_file,
                    updated,
                    payload,
                    ChainRemap::Tables(&remap),
                    &new_index,
                )?;
                remap.push(
                    old_offset,
                    Some(updated.record_offset),
                    Some(updated.record_offset),
                )?;
                if updated.block_id == SEGMENT_BLOCK_ID {
                    segment_covered_start = Some(updated.checked_physical_end()?);
                    segment_start = new_index.len() + 1;
                }
                debug_assert!(
                    new_index
                        .last()
                        .is_none_or(|last| last.record_offset < updated.record_offset)
                );
                new_index.push(updated)?;
            }
            if source_end != self.snapshot.len() {
                return Err(Error::CorruptTail { offset: source_end });
            }
            temp_file.flush()?;
            temp_file.sync_all()?;
            validate_replacement_generation_file(
                self.spec,
                &mut temp_file,
                rewrite_append_start,
                &new_index,
            )?;
            Ok(())
        })();
        if let Err(error) = prepare_result {
            drop(temp_file);
            let _ = remove_file(&temp_path);
            return Err(error);
        }
        drop(temp_file);
        // All chain links and replacement coverage have been validated. The
        // remap is no longer needed; release its heap and spill file before
        // adopting (and optionally compacting) the new private directory.
        drop(remap);

        // Only the two fields the rebind actually reads: it returns this value
        // (which this operation discards in favour of its own report) and names
        // `sequence` in the poisoned-publication error. `record_offset` and the
        // payload lengths would describe a single replaced record, which this
        // operation does not have — so they are zero rather than plausible, and
        // the value never leaves this function.
        let info = ReplacementInfo {
            sequence: new_index.maximum_sequence(),
            record_offset: 0,
            old_payload_len: 0,
            new_payload_len: 0,
            old_physical_len: bytes_before,
            new_physical_len: 0,
        };
        match replace_path_atomically(&temp_path, &self.path) {
            Ok(ReplaceDurability::Durable) => {
                self.rebind_replacement_generation(info, new_index)?;
            }
            Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
                return match self.rebind_replacement_generation(info, new_index) {
                    Ok(_) => Err(crate::fallible::boxed_path_error(
                        sync_error,
                        &self.path,
                        Error::ErrorContextAllocationFailed {
                            context: crate::AllocationFailureContext::ParentSyncPending,
                        },
                        |path, source| Error::PublishedButParentSyncPending { path, source },
                    )),
                    Err(rebind_error) => Err(rebind_error.with_pending_parent_sync(sync_error)),
                };
            }
            Err(error) => return Err(self.fail_publication(&temp_path, error)),
        }
        Ok(DefragmentReport {
            records_dropped: dropped,
            bytes_before,
            bytes_after: self.file.metadata()?.len(),
        })
    }

    pub fn replace_block<T: VarlitReplaceBlock>(
        &mut self,
        index: usize,
        block: &T,
    ) -> Result<ReplacementInfo> {
        let _permit = self.ensure_write()?;
        if !self.spec.layout.is_varlit_native_default() {
            return Err(Error::InvalidFormatSpec(
                "replacement is not supported for custom physical layouts",
            ));
        }
        if self.matrix.is_some() || T::KIND == BlockKind::Matrix {
            return Err(Error::InvalidFormatSpec(
                "replacement is not supported for matrix storage",
            ));
        }
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        // resolution and the target-version refusal are one step, and
        // there is no other way to address the target. See `ReplacementTarget`.
        let target_position = ReplacementTarget::resolve::<T>(&self.index, index)?.position();
        let target = self.index.entry_at(target_position)?;
        let mut materialization = MaterializationBudget::new(self.spec);
        let old_logical_len = target.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
        materialization.consume(old_logical_len)?;
        let old_payload = target.read_logical_payload_snapshot(self.spec, &self.snapshot)?;
        let old: T = materialization.decode(&old_payload, T::ENDIAN.unwrap_or(self.spec.endian))?;
        T::validate_replacement(&old, block)?;

        let encoded = encode_logical_payload_limited(
            self.spec,
            block,
            T::ENDIAN.unwrap_or(self.spec.endian),
        )?;
        let replacement = prepare_user_record_payload(self.spec, T::ID, T::KIND, &encoded)?;
        let replacement_len = u64::try_from(replacement.bytes.len())
            .map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, replacement_len)?;
        let old_physical_len = target
            .checked_physical_end()?
            .checked_sub(target.record_offset)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "replacement physical length",
            })?;
        let new_physical_len = RECORD_HEADER_LEN
            .checked_add(replacement_len)
            .and_then(|len| len.checked_add(record_footer_len(self.spec)))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "replacement physical length",
            })?;
        let info = ReplacementInfo {
            sequence: target.sequence,
            record_offset: target.record_offset,
            old_payload_len: target.payload_len,
            new_payload_len: replacement_len,
            old_physical_len,
            new_physical_len,
        };

        self.ensure_generation_rewrite_allowed()?;
        self.validate_source_generation()?;
        let mut new_index = RewriteDirectory::new(self.spec);

        let rewrite_append_start = append_log_start_for_file(self)?;
        // Segment coverage in the new generation, tracked exactly as the
        // writer's `SegmentCursor` tracks it in the old one.
        let mut segment_start = 0usize;
        let mut segment_covered_start: Option<u64> = None;
        let (temp_path, mut temp_file) = create_rewrite_temp_file(&self.path)?;
        let prepare_result = (|| -> Result<()> {
            if let Ok(metadata) = self.file.metadata() {
                temp_file.set_permissions(metadata.permissions())?;
            }
            // Write back the region this file carries, not the one `spec`
            // would generate: regenerating drops any block this build does not
            // know and moves the append log out from under the index. The one
            // exception is a mutable block, which is reset to cold rather than
            // carried forward describing offsets this file will not have.
            let replacement_extensions =
                header_extensions_for_replacement(self.spec, &self.header_extensions)?;
            write_native_file_header(&mut temp_file, self.spec, &replacement_extensions)?;
            for (position, source_entry) in self.index.iter().enumerate() {
                let source_entry = source_entry?;
                let mut updated = source_entry.clone();
                let checkpoint_payload;
                let payload = if position == target_position {
                    updated.flags = replacement.flags;
                    updated.uncompressed_len_hint = replacement.uncompressed_len_hint;
                    RewritePayload::Bytes(&replacement.bytes)
                } else if source_entry.block_id == INDEX_BLOCK_ID {
                    RewritePayload::Index(IndexPayload {
                        entries: &new_index,
                        start: 0,
                        covered_start: None,
                        record_offset: temp_file.stream_position()?,
                    })
                } else if source_entry.block_id == SEGMENT_BLOCK_ID {
                    // A segment payload is record offsets, and this rewrite
                    // moves them. Copying the bytes would publish a generation
                    // whose chain describes the file it replaced.
                    let record_offset = temp_file.stream_position()?;
                    RewritePayload::Index(IndexPayload {
                        entries: &new_index,
                        start: segment_start,
                        covered_start: Some(segment_covered_start.unwrap_or(rewrite_append_start)),
                        record_offset,
                    })
                } else if source_entry.block_id == OPEN_DIGEST_BLOCK_ID {
                    // A digest payload is block-tail offsets and its own
                    // start offset, and a rewrite moves both, so copying the
                    // bytes would publish a generation whose digest describes
                    // the file it replaced.
                    //
                    // It is rebuilt including this record's own sequence. The
                    // claim that stood here — that a digest "closes a commit
                    // point and never describes itself" — is what made the
                    // high-water mark one too low; see
                    // `rebuilt_open_digest_payload`. The trailer check does
                    // not catch it: the lazy open accepts the digest and
                    // resumes on a sequence that is already taken.
                    let record_offset = temp_file.stream_position()?;
                    checkpoint_payload = rebuilt_open_digest_payload(
                        self.spec,
                        &new_index,
                        source_entry.sequence,
                        record_offset,
                    )?;
                    RewritePayload::Bytes(&checkpoint_payload)
                } else {
                    RewritePayload::Snapshot {
                        offset: source_entry.payload_offset,
                        len: source_entry.payload_len,
                    }
                };
                updated = rewrite_replacement_record_streaming(
                    self.spec,
                    &self.snapshot,
                    &mut temp_file,
                    updated,
                    payload,
                    ChainRemap::Uniform(info),
                    &new_index,
                )?;
                if updated.block_id == SEGMENT_BLOCK_ID {
                    segment_covered_start = Some(updated.checked_physical_end()?);
                    segment_start = new_index.len() + 1;
                }
                // What makes the rewrite directory's binary search legal:
                // each record is written at `output.stream_position()` and
                // pushed in that order, so the prefix is strictly increasing in
                // `record_offset`. Asserted here, where the invariant is made,
                // because that is O(1) — asserting sortedness inside the lookup
                // would reintroduce the O(N) it exists to remove.
                debug_assert!(
                    new_index
                        .last()
                        .is_none_or(|last| last.record_offset < updated.record_offset)
                );
                new_index.push(updated)?;
            }
            temp_file.flush()?;
            temp_file.sync_all()?;
            validate_replacement_generation_file(
                self.spec,
                &mut temp_file,
                append_log_start_for_file(self)?,
                &new_index,
            )?;
            Ok(())
        })();
        if let Err(error) = prepare_result {
            drop(temp_file);
            let _ = remove_file(&temp_path);
            return Err(error);
        }
        drop(temp_file);

        match replace_path_atomically(&temp_path, &self.path) {
            Ok(ReplaceDurability::Durable) => self.rebind_replacement_generation(info, new_index),
            Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
                // Publication already happened: the pathname resolves to the
                // new generation, so the writer must move there regardless.
                //
                // `?` here would drop `sync_error` if the rebind failed
                // too, and `PublishedButRebindFailed` alone cannot say that
                // the published pathname is not yet durable. Both facts are
                // preserved instead.
                match self.rebind_replacement_generation(info, new_index) {
                    Ok(_) => Err(crate::fallible::boxed_path_error(
                        sync_error,
                        &self.path,
                        Error::ErrorContextAllocationFailed {
                            context: crate::AllocationFailureContext::ParentSyncPending,
                        },
                        |path, source| Error::PublishedButParentSyncPending { path, source },
                    )),
                    Err(rebind_error) => Err(rebind_error.with_pending_parent_sync(sync_error)),
                }
            }
            Err(error) => Err(self.fail_publication(&temp_path, error)),
        }
    }

    /// Publishes a same-size fixed replacement through a copy-on-write file
    /// generation.
    ///
    /// [`Error::PublishedButRebindFailed`] means publication already succeeded.
    /// Do not retry blindly; discard this poisoned writer and reopen the path.
    ///
    /// [`Error::PublishedButParentSyncPending`] also means publication already
    /// succeeded; the writer was rebound to the published generation and stays
    /// usable, but the rename is not yet guaranteed durable against power loss
    /// until the parent directory is synced (for example by a later successful
    /// publication or an explicit directory sync).
    /// Refuses an in-place record mutation on a segment-chained format.
    ///
    /// A segment payload records the header fields of every record it covers,
    /// and an in-place replacement restamps the sequence and checksum of a
    /// record that is already covered. Nothing rewrites the segment behind it,
    /// so the chain would keep describing the record as it was - and open would
    /// have no way to notice, because it reads no data record.
    ///
    /// Patching the covering segment instead was considered and is the shape
    /// §4A.5 rejected for the index anchor: an in-place rewrite of a derived
    /// structure, with a tear window a lock-free `open_readonly` can observe.
    /// So this is a capability trade rather than a repair: in-place fixed
    /// replacement and the segment chain are alternatives, and
    /// `replace_block` - which publishes a whole new generation, and re-encodes
    /// every segment payload against the new offsets - is available under both.
    /// Refuses a whole-generation rewrite for a format with a non-resident
    /// block.
    ///
    /// Both rewrite paths rebuild the new generation by iterating the resident
    /// index, which no longer mirrors every record: a non-resident block's
    /// records would be absent from the copy and silently dropped from the
    /// published file. This is a format-level refusal rather than a per-block
    /// one, because the loss is of blocks the caller did not name.
    fn ensure_generation_rewrite_allowed(&self) -> Result<()> {
        if self.matrix.is_some() {
            return Err(Error::InvalidFormatSpec(
                "record generation replacement is not supported for matrix storage",
            ));
        }
        if self.spec.has_non_resident_blocks() {
            return Err(Error::InvalidFormatSpec(
                "a generation rewrite is not supported for a format with a non-resident block",
            ));
        }
        Ok(())
    }

    fn ensure_in_place_replacement_allowed(&self) -> Result<()> {
        if self.spec.index_policy.segment_on_flush {
            return Err(Error::InvalidFormatSpec(
                "in-place replacement is not supported for segment_on_flush formats",
            ));
        }
        // The digest is the same shape of hazard as the segment above, and it
        // is worse in what it costs. `replace_fixed` publishes by copying the
        // generation byte for byte and patching one record, so it rewrites no
        // derived record at all - while assigning the replacement a *fresh*
        // sequence, which raises the file's true maximum above the mark the
        // copied digest still reports.
        //
        // A scan recounts and is unaffected, which is why `checkpoint_on_flush`
        // needs no refusal here and was measured not to. The digest is the one
        // derived record a lazy open *trusts instead of recounting*: it seeds
        // its writer with `sequence_high_water + 1`, so the next append lands
        // on the sequence the replacement took and the file stops opening with
        // `InvalidCanonicalEncoding("duplicate native record sequence")`.
        //
        // Measured before this refusal, on `crc32 + open_digest_on_flush`, 64
        // records: `replace_fixed` gave the replacement sequence 65, the digest
        // went on reporting 64, and the next lazy append also took 65.
        //
        // A capability trade, not a loss: `open_digest_on_flush` forces
        // `block_offset_chain` on, so `replace` routes such a format to
        // `replace_block`, which republishes and rebuilds the digest. Only a
        // caller naming this method directly sees the refusal.
        if self.spec.index_policy.open_digest_on_flush {
            return Err(Error::InvalidFormatSpec(
                "in-place replacement is not supported for open_digest_on_flush formats",
            ));
        }
        Ok(())
    }

    /// Marks the record at `record_offset` dead.
    ///
    /// The one mutation varlit makes to a record that is not a rewrite of it.
    /// It sets a bit in the footer's trailing word — four bytes, outside the
    /// record checksum — and touches nothing else: the payload is not read, no
    /// checksum is recomputed, no record moves, and the file does not grow.
    ///
    /// # What a dead record is, and is not
    ///
    /// It is **advisory**. The record is still framed, still read back by
    /// `blocks`, `scan` and `record_map`, and still part of every chain that
    /// points at it. Nothing is hidden, because hiding it would silently change
    /// what a chain walk means and would make the mark a data-loss operation
    /// rather than a note. What consumes it is a defragmenting rewrite, which
    /// is the operation that can actually drop a record and rebuild the chains
    /// that named it.
    ///
    /// Read it back with [`RecordIndexEntry::is_dead`].
    ///
    /// # Refusals
    ///
    /// - `InvalidFormatSpec` unless the format declares
    ///   `liveness: footer_flags`; without it the word is a reserved field that
    ///   every reader requires to be zero.
    /// - `ReservedBlockId` for an internal record. Marking a commit marker or a
    ///   segment dead would make a defragmenter drop the very records that say
    ///   where the commit boundary is.
    /// - Whatever framing the record at `record_offset` fails with, if it is
    ///   not a record.
    ///
    /// Membership is checked in the private offset directory. Unproven lazy,
    /// serialized-directory and non-resident offsets use an incremental boundary
    /// directory. Each frame is discovered once per snapshot lineage; reverse,
    /// random and repeated requests reuse exact membership. The first request
    /// can scan a prefix. Boundary rows borrow the existing index-buffer budget
    /// and spill eight bytes per discovered record to temporary storage as needed.
    /// A small stack window amortizes reads of adjacent small records; large
    /// payloads are skipped. No payload-sized allocation is made.
    ///
    /// # Durability
    ///
    /// Not synced here. The mark rides the next `flush`/`commit`/`sync` like
    /// any other write; a crash before one leaves the record alive, which is
    /// the safe direction — a defragmenter that has not been told keeps data it
    /// could have dropped.
    pub fn mark_record_dead(&mut self, record_offset: u64) -> Result<()> {
        self.set_record_mutable_flags(record_offset, RECORD_MUTABLE_FLAG_DEAD)
    }

    fn set_record_mutable_flags(&mut self, record_offset: u64, set: u32) -> Result<()> {
        let _permit = self.ensure_write()?;
        if self.spec.liveness_policy != LivenessPolicy::FooterFlags {
            return Err(Error::InvalidFormatSpec(
                "marking a record requires liveness: footer_flags",
            ));
        }
        let use_sequence_hint =
            self.index.has_sequence_hint() && !self.index.has_uniform_boundary(record_offset);
        if !use_sequence_hint {
            self.require_native_record_boundary(record_offset)?;
        }
        let parsed = fault_record_entry_for_mark_with(
            self.snapshot.len(),
            self.index.len(),
            self.spec,
            record_offset,
            |at, bytes| self.snapshot.read_exact_at(at, bytes),
        );
        let entry = match parsed {
            Ok(entry) => entry,
            Err(error) => {
                // Preserve the original boundary-error priority on malformed
                // or interior offsets even when parsing was attempted first.
                if use_sequence_hint {
                    self.require_native_record_boundary(record_offset)?;
                }
                return Err(error);
            }
        };
        if use_sequence_hint
            && !self
                .index
                .matches_sequence_hint(record_offset, entry.sequence)?
        {
            self.require_native_record_boundary(record_offset)?;
            // A genuine row with a sequence gap does not fit this hint. Keep
            // later marks on the established lookup path for this generation.
            self.index.disable_sequence_hint();
        }
        self.write_framed_record_flags(&entry, set)
    }

    #[inline]
    fn require_native_record_boundary(&mut self, record_offset: u64) -> Result<()> {
        if self.index.has_verified_boundary(record_offset)? {
            Ok(())
        } else {
            self.require_uncached_native_boundary(record_offset)
        }
    }

    #[cold]
    #[inline(never)]
    fn require_uncached_native_boundary(&mut self, record_offset: u64) -> Result<()> {
        let append_start = append_log_start_for_file(self)?;
        self.index.verify_boundary(append_start, record_offset)
    }

    #[inline]
    fn write_framed_record_flags(&mut self, entry: &RecordIndexEntry, set: u32) -> Result<()> {
        validate_record_entry(&self.spec, entry)?;
        if entry.block_id >= RESERVED_BLOCK_ID_START {
            return Err(Error::ReservedBlockId(entry.block_id));
        }
        let Some(footer_offset) = entry.footer_offset else {
            return Err(Error::InvalidFormatSpec(
                "marking a record requires a record footer",
            ));
        };
        let Some(word_offset) =
            footer_offset.checked_add(RECORD_FOOTER_LEN - RECORD_FOOTER_MUTABLE_LEN)
        else {
            return Err(Error::ResourceArithmeticOverflow {
                resource: "record footer offset",
            });
        };
        let updated = entry.mutable_flags | set;
        if updated == entry.mutable_flags {
            // Already marked. Skipping the write is not an optimisation — it
            // keeps a repeated mark from being a repeated dirtying of a page
            // that readers may be faulting.
            return Ok(());
        }
        self.file
            .overwrite_record_mutable_flags(word_offset, updated.to_le_bytes())?;
        Ok(())
    }

    /// Whether the record at `record_offset` is marked dead.
    ///
    /// Frames the record and reads its footer, so this costs one record's
    /// worth of positional reads. Reading a run of entries through
    /// [`record_map`](Self::record_map) and asking each
    /// [`RecordIndexEntry::is_dead`] costs the same framing once.
    pub fn record_is_dead(&self, record_offset: u64) -> Result<bool> {
        let entry = fault_record_entry(&self.snapshot, self.spec, record_offset, false)?;
        Ok(entry.is_dead())
    }

    pub fn replace_fixed<T: VarlitBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        let _permit = self.ensure_write()?;
        self.ensure_in_place_replacement_allowed()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        if T::KIND != BlockKind::Fixed {
            return Err(Error::BlockKindMismatch {
                expected: BlockKind::Fixed,
                actual: T::KIND,
            });
        }
        // resolution and the target-version refusal are one step, and
        // there is no other way to address the target. See `ReplacementTarget`.
        let target_position = ReplacementTarget::resolve::<T>(&self.index, index)?.position();

        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        let payload = encode_logical_payload_limited(self.spec, block, endian)?;
        let payload_len =
            u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::LogicalPayloadLen, payload_len)?;
        let old_len = self.index.entry_at(target_position)?.payload_len;
        if old_len != payload_len {
            return Err(Error::ReplaceSizeMismatch {
                old: old_len,
                new: payload_len,
            });
        }
        self.ensure_generation_rewrite_allowed()?;
        self.validate_source_generation()?;

        let sequence = self.sequence_state.available()?;
        let entry = self.index.entry_at(target_position)?;
        let mut footer_bytes = [0; RECORD_FOOTER_LEN as usize];
        let footer = if let Some(footer_offset) = entry.footer_offset {
            self.snapshot
                .read_exact_at(footer_offset, &mut footer_bytes)?;
            &footer_bytes[..]
        } else {
            &[]
        };
        let header = RecordHeaderFields {
            block_id: entry.block_id,
            block_version: entry.block_version,
            flags: entry.flags,
            sequence,
            payload_len: entry.payload_len,
            checksum: 0,
            uncompressed_len_hint: entry.uncompressed_len_hint,
        };
        let checksum =
            checksum_record_fields(self.spec, entry.record_offset, header, &payload, footer)?;
        let header_bytes = encode_native_record_header(
            RecordHeaderFields { checksum, ..header },
            entry.record_offset,
            record_footer_len(self.spec),
        )?;
        let record_offset = entry.record_offset;
        let payload_offset = entry.payload_offset;

        let mut new_index = RewriteDirectory::new(self.spec);
        for (position, source) in self.index.iter().enumerate() {
            let mut updated = source?;
            if position == target_position {
                updated.sequence = sequence;
                updated.checksum = checksum;
            }
            new_index.push(updated)?;
        }

        // The copy below reproduces the header verbatim, warm region included,
        // and that table would describe a file whose true sequence maximum is
        // no longer the one it can be derived from. See
        // `header_tails_invalidation`.
        let cold_tails = self.header_tails_invalidation()?;

        let (temp_path, mut temp_file) = create_rewrite_temp_file(&self.path)?;
        let prepare_result = (|| -> Result<()> {
            if let Ok(metadata) = self.file.metadata() {
                temp_file.set_permissions(metadata.permissions())?;
            }
            self.snapshot
                .copy_range_to(0, self.snapshot.len(), &mut temp_file)?;
            temp_file.seek(SeekFrom::Start(record_offset))?;
            temp_file.write_all(&header_bytes)?;
            temp_file.seek(SeekFrom::Start(payload_offset))?;
            temp_file.write_all(&payload)?;
            if let Some((offset, cold)) = cold_tails.as_ref() {
                temp_file.seek(SeekFrom::Start(*offset))?;
                temp_file.write_all(cold)?;
            }
            temp_file.flush()?;
            temp_file.sync_all()?;
            validate_generation_file(
                self.spec,
                &mut temp_file,
                append_log_start_for_file(self)?,
                &new_index,
            )?;
            Ok(())
        })();
        match prepare_result {
            Ok(()) => {}
            Err(error) => {
                drop(temp_file);
                let _ = remove_file(&temp_path);
                return Err(error);
            }
        }
        drop(temp_file);

        match replace_path_atomically(&temp_path, &self.path) {
            Ok(ReplaceDurability::Durable) => self.rebind_published_generation(sequence, new_index),
            Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
                // Publication already happened: the pathname resolves to the
                // new generation, so the writer must move there regardless.
                //
                // `?` here would drop `sync_error` if the rebind failed
                // too, and `PublishedButRebindFailed` alone cannot say that
                // the published pathname is not yet durable. Both facts are
                // preserved instead.
                match self.rebind_published_generation(sequence, new_index) {
                    Ok(_) => Err(crate::fallible::boxed_path_error(
                        sync_error,
                        &self.path,
                        Error::ErrorContextAllocationFailed {
                            context: crate::AllocationFailureContext::ParentSyncPending,
                        },
                        |path, source| Error::PublishedButParentSyncPending { path, source },
                    )),
                    Err(rebind_error) => Err(rebind_error.with_pending_parent_sync(sync_error)),
                }
            }
            Err(error) => Err(self.fail_publication(&temp_path, error)),
        }
    }

    /// Replaces a fixed record by mutating this file object directly.
    ///
    /// # Errors
    ///
    /// the target is selected by block id, so a version mismatch between
    /// `T` and the stored record is refused with
    /// [`Error::BlockVersionMismatch`], exactly as [`VarlitFile::replace_fixed`]
    /// does. Otherwise `T`'s payload would be written under the stored
    /// record's older version header. The refusal is not a step this function
    /// performs; it is a precondition of resolving the target at all. See
    /// `ReplacementTarget`.
    ///
    /// # Safety
    ///
    /// The caller must exclude every reader, writer, mapping, raw reference,
    /// handle, thread, and process for this operation and for the lifetime of
    /// every view that could observe the affected object.
    ///
    /// For a keyed block in a format with `keyed_offset_chain`, the caller
    /// must additionally **not change the record's key** . This entry
    /// point takes `T: VarlitBlock`, which exposes no key, so the requirement
    /// cannot be checked here, and unlike the copy-on-write paths there is no
    /// new generation in which the chain could be rebuilt: records appended
    /// after this one already carry `prev_same_key_offset` pointers into it,
    /// and rewriting its key in place would leave them linking a record that
    /// now claims a different key. Ordinary keyed replacement enforces the
    /// same-key contract through
    /// [`crate::VarlitReplaceBlock::validate_replacement`]; here it is part of
    /// the unsafe contract.
    ///
    /// What this function *does* guarantee, whether or not the contract is
    /// honoured, is that no resident keyed-tail cache survives the mutation:
    /// the affected block's tail map is dropped **before** the first byte is
    /// written, so a stale predecessor can never be read afterwards, not even
    /// if the write fails part-way or the contract above is violated.
    pub unsafe fn replace_fixed_in_place_exclusive<T: VarlitBlock>(
        &mut self,
        index: usize,
        block: &T,
    ) -> Result<u64> {
        self.check_write()?;
        self.ensure_in_place_replacement_allowed()?;
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        if T::KIND != BlockKind::Fixed {
            return Err(Error::BlockKindMismatch {
                expected: BlockKind::Fixed,
                actual: T::KIND,
            });
        }
        // resolution and the target-version refusal are one step, and
        // there is no other way to address the target. See `ReplacementTarget`.
        let target = ReplacementTarget::resolve::<T>(&self.index, index)?;
        let target_position = target.position();
        let payload = encode_logical_payload_limited(
            self.spec,
            block,
            T::ENDIAN.unwrap_or(self.spec.endian),
        )?;
        let payload_len =
            u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::LogicalPayloadLen, payload_len)?;
        let entry = self.index.entry_at(target_position)?;
        if entry.payload_len != payload_len {
            return Err(Error::ReplaceSizeMismatch {
                old: entry.payload_len,
                new: payload_len,
            });
        }
        self.ensure_generation_rewrite_allowed()?;
        self.validate_source_generation()?;
        let sequence = self.sequence_state.available()?;
        let entry = self.index.entry_at(target_position)?;
        let mut footer_bytes = [0; RECORD_FOOTER_LEN as usize];
        let footer = if let Some(footer_offset) = entry.footer_offset {
            self.snapshot
                .read_exact_at(footer_offset, &mut footer_bytes)?;
            &footer_bytes[..]
        } else {
            &[]
        };
        let header = RecordHeaderFields {
            block_id: entry.block_id,
            block_version: entry.block_version,
            flags: entry.flags,
            sequence,
            payload_len,
            checksum: 0,
            uncompressed_len_hint: entry.uncompressed_len_hint,
        };
        let checksum =
            checksum_record_fields(self.spec, entry.record_offset, header, &payload, footer)?;
        let header_bytes = encode_native_record_header(
            RecordHeaderFields { checksum, ..header },
            entry.record_offset,
            record_footer_len(self.spec),
        )?;
        // Before the record changes, not after: this mutates the live file, so
        // the ordering is the only thing standing between a crash here and a
        // file whose header table outlives the invariant it was derived under.
        // See `header_tails_invalidation`.
        self.retire_header_tails_in_place()?;
        let write_result =
            self.ensure_write()?
                .overwrite_record_bytes_in_place(target, &header_bytes, &payload);
        if let Err(error) = write_result {
            self.poison.poison();
            return Err(error);
        }
        // No index update. `overwrite_record_bytes_in_place` above wrote and
        // flushed a header carrying this sequence and this checksum, and the
        // entry for this position is rebuilt from that header — so the restamp
        // this used to perform would copy the file's own bytes onto themselves.
        self.publish_sequence(sequence);
        Ok(sequence)
    }

    /// Publishes a replacement by rewriting the complete file generation.
    ///
    /// # Not for a format that writes a record footer
    ///
    /// Refused with `InvalidFormatSpec("replace is not supported for
    /// record-footer formats")`, which is every format declaring an offset
    /// chain, `segment_on_flush`, or any commit policy other than
    /// [`CommitPolicy::None`]. Use [`replace_block`](Self::replace_block)
    /// there: it does the same whole-file republish and does maintain the
    /// footer, translating both chain offsets and checking each predecessor
    /// against the prefix it has already written.
    ///
    /// The refusal is a statement about *this* routine rather than about the
    /// operation. Its record writer emits header and payload and stops — no
    /// footer, and both chain fields cleared — so a file it produced for such a
    /// format would frame perfectly and carry no chain at all.
    ///
    /// [`Error::PublishedButRebindFailed`] means publication already succeeded.
    /// Do not retry blindly; discard this poisoned writer and reopen the path.
    ///
    /// [`Error::PublishedButParentSyncPending`] also means publication already
    /// succeeded; the writer was rebound to the published generation and stays
    /// usable, but the rename is not yet guaranteed durable against power loss
    /// until the parent directory is synced (for example by a later successful
    /// publication or an explicit directory sync).
    pub fn replace_rewrite<T: VarlitBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        let _permit = self.ensure_write()?;
        if self.spec.spec_needs_record_footer() {
            return Err(Error::InvalidFormatSpec(
                "replace is not supported for record-footer formats",
            ));
        }
        if self.matrix.is_some() {
            return Err(Error::InvalidFormatSpec(
                "native rewrite is not supported for matrix files",
            ));
        }
        self.ensure_user_block::<T>()?;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        // resolution and the target-version refusal are one step, and
        // there is no other way to address the target. See `ReplacementTarget`.
        let target_position = ReplacementTarget::resolve::<T>(&self.index, index)?.position();
        let sequence = self.sequence_state.available()?;
        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        let encoded = encode_logical_payload_limited(self.spec, block, endian)?;
        let replacement = prepare_user_record_payload(self.spec, T::ID, T::KIND, &encoded)?;
        let replacement_len = u64::try_from(replacement.bytes.len())
            .map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, replacement_len)?;
        self.ensure_generation_rewrite_allowed()?;
        self.validate_source_generation()?;

        let mut new_index = RewriteDirectory::new(self.spec);

        let (temp_path, mut temp_file) = create_rewrite_temp_file(&self.path)?;
        let prepare_result = (|| -> Result<()> {
            if let Ok(metadata) = self.file.metadata() {
                temp_file.set_permissions(metadata.permissions())?;
            }
            // Write back the region this file carries, not the one `spec`
            // would generate: regenerating drops any block this build does not
            // know and moves the append log out from under the index. The one
            // exception is a mutable block, which is reset to cold rather than
            // carried forward describing offsets this file will not have.
            let replacement_extensions =
                header_extensions_for_replacement(self.spec, &self.header_extensions)?;
            write_native_file_header(&mut temp_file, self.spec, &replacement_extensions)?;
            for (position, source_entry) in self.index.iter().enumerate() {
                let source_entry = source_entry?;
                let mut updated = source_entry.clone();
                let checkpoint_payload;
                let payload = if position == target_position {
                    updated.sequence = sequence;
                    updated.flags = replacement.flags;
                    updated.uncompressed_len_hint = replacement.uncompressed_len_hint;
                    RewritePayload::Bytes(&replacement.bytes)
                } else if source_entry.block_id == INDEX_BLOCK_ID {
                    RewritePayload::Index(IndexPayload {
                        entries: &new_index,
                        start: 0,
                        covered_start: None,
                        record_offset: temp_file.stream_position()?,
                    })
                } else if source_entry.block_id == OPEN_DIGEST_BLOCK_ID {
                    // A digest payload is block-tail offsets and its own
                    // start offset, and a rewrite moves both, so copying the
                    // bytes would publish a generation whose digest describes
                    // the file it replaced.
                    //
                    // It is rebuilt including this record's own sequence. The
                    // claim that stood here — that a digest "closes a commit
                    // point and never describes itself" — is what made the
                    // high-water mark one too low; see
                    // `rebuilt_open_digest_payload`. The trailer check does
                    // not catch it: the lazy open accepts the digest and
                    // resumes on a sequence that is already taken.
                    let record_offset = temp_file.stream_position()?;
                    checkpoint_payload = rebuilt_open_digest_payload(
                        self.spec,
                        &new_index,
                        source_entry.sequence,
                        record_offset,
                    )?;
                    RewritePayload::Bytes(&checkpoint_payload)
                } else {
                    RewritePayload::Snapshot {
                        offset: source_entry.payload_offset,
                        len: source_entry.payload_len,
                    }
                };
                updated = rewrite_record_streaming(
                    self.spec,
                    &self.snapshot,
                    &mut temp_file,
                    updated,
                    payload,
                )?;
                new_index.push(updated)?;
            }
            temp_file.flush()?;
            temp_file.sync_all()?;
            validate_generation_file(
                self.spec,
                &mut temp_file,
                append_log_start_for_file(self)?,
                &new_index,
            )?;
            Ok(())
        })();
        match prepare_result {
            Ok(()) => {}
            Err(error) => {
                drop(temp_file);
                let _ = remove_file(&temp_path);
                return Err(error);
            }
        }
        drop(temp_file);

        match replace_path_atomically(&temp_path, &self.path) {
            Ok(ReplaceDurability::Durable) => self.rebind_published_generation(sequence, new_index),
            Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
                // Publication already happened: the pathname resolves to the
                // new generation, so the writer must move there regardless.
                //
                // `?` here would drop `sync_error` if the rebind failed
                // too, and `PublishedButRebindFailed` alone cannot say that
                // the published pathname is not yet durable. Both facts are
                // preserved instead.
                match self.rebind_published_generation(sequence, new_index) {
                    Ok(_) => Err(crate::fallible::boxed_path_error(
                        sync_error,
                        &self.path,
                        Error::ErrorContextAllocationFailed {
                            context: crate::AllocationFailureContext::ParentSyncPending,
                        },
                        |path, source| Error::PublishedButParentSyncPending { path, source },
                    )),
                    Err(rebind_error) => Err(rebind_error.with_pending_parent_sync(sync_error)),
                }
            }
            Err(error) => Err(self.fail_publication(&temp_path, error)),
        }
    }

    /// Replaces the record at `index`, by whichever route the format supports.
    ///
    /// **The route is not a choice the caller can make.** It used to be — this
    /// took a `ReplaceStrategy` — and that was wrong in a way measurable rather
    /// than stylistic: on a format declaring an offset chain or a commit
    /// policy, *both* of the two strategies refused, and the implementation
    /// that works was reachable only by naming
    /// [`replace_block`](Self::replace_block) directly. What decides is the
    /// declaration, so the declaration decides here:
    ///
    /// | format | route | why |
    /// | --- | --- | --- |
    /// | writes a record footer | [`replace_block`](Self::replace_block) | it is the only route that maintains the offset chains a footer carries |
    /// | no footer, fixed block | [`replace_fixed`](Self::replace_fixed) | cheapest — copies the generation and patches one same-length record |
    /// | no footer, otherwise | [`replace_rewrite`](Self::replace_rewrite) | a whole-file republish, with no footer to maintain |
    ///
    /// All three stay public for a caller who wants to name one and be refused
    /// loudly if the format cannot serve it. This is the door for a caller who
    /// wants the replacement rather than a particular mechanism.
    ///
    /// # Bound
    ///
    /// `VarlitReplaceBlock` rather than `VarlitBlock`, which the footer route
    /// needs for its key-preservation check. That costs derived blocks nothing:
    /// the macro emits the impl for every one — trivially `Ok` where there is
    /// no key, and a `ReplacementKeyMismatch` check where there is. Only a
    /// hand-written block needs the one-line impl.
    ///
    /// Returns the replacement record's sequence.
    pub fn replace<T: VarlitReplaceBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        if self.spec.spec_needs_record_footer() {
            return self.replace_block(index, block).map(|info| info.sequence);
        }
        if T::KIND == BlockKind::Fixed {
            return self.replace_fixed(index, block);
        }
        self.replace_rewrite(index, block)
    }

    fn rebind_published_generation(
        &mut self,
        sequence: u64,
        new_index: RewriteDirectory,
    ) -> Result<u64> {
        let rebind = (|| -> Result<(File, SnapshotFile)> {
            #[cfg(test)]
            fail_rebind_after_publish_if_requested()?;
            let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
            // The published generation is a new file object; the single-writer
            // object lock must move with the writer.
            if let Some(lock) = self._lock.as_mut() {
                lock.bind_native(&file, &self.path)?;
            }
            let snapshot = SnapshotFile::new(file.try_clone()?)?;
            Ok((file, snapshot))
        })();

        match rebind {
            Ok((file, snapshot)) => {
                self.file = RecordFile::new(file);
                self.snapshot = snapshot;
                let last_resident_block_id = new_index.last().map(|entry| entry.block_id);
                // Rebinding succeeded and the complete new directory is ready.
                // Release the old private rows before optional compaction; an
                // assignment alone would keep them alive throughout its RHS.
                self.index = ResidentIndex::empty(self.snapshot.clone(), self.spec);
                self.index = new_index.into_resident(self.snapshot.clone(), self.spec);
                // The resident index was replaced wholesale; recover the O(1)
                // flush-cadence state once for the new generation
                // and the O(1) block tails with it . Record offsets
                // moved, so every cached generic keyed tail is stale .
                let derived = derive_index_state(self.index.iter(), true)?;
                self.checkpoint_cadence = derived.checkpoint_cadence;
                self.block_tails = derived.block_tails.expect("tails were requested");
                self.segment_cursor = derived.segment_cursor;
                // The same wholesale replacement: the newest entry is a
                // different record now, so the append-path shortcut that names
                // its block id is stale too.
                self.last_resident_block_id = last_resident_block_id;
                self.keyed_tails.invalidate_all();
                // Both callers publish a generation whose mutable header blocks
                // are cold — `replace_rewrite` because it writes a fresh header
                // through `header_extensions_for_replacement`, `replace_fixed`
                // because it writes the cold region over its byte copy — so the
                // cached region has to say so too, exactly as
                // `rebind_replacement_generation` does. A cache left warm over
                // a cold file makes the next commit write generation `N + 1`
                // while a reader still prefers a warm slot claiming `N`.
                self.header_extensions =
                    header_extensions_for_replacement(self.spec, &self.header_extensions)?;
                self.publish_sequence(sequence);
                Ok(sequence)
            }
            Err(source) => {
                self.poison.poison();
                // the caller folds in a parent-directory sync failure it
                // already observed, via `Error::with_pending_parent_sync`.
                Err(crate::fallible::boxed_error(
                    source,
                    Error::ErrorContextAllocationFailed {
                        context: crate::AllocationFailureContext::PublishedRebind {
                            sequence,
                            parent_sync_pending: false,
                        },
                    },
                    |source| Error::PublishedButRebindFailed {
                        sequence,
                        source,
                        parent_sync: None,
                    },
                ))
            }
        }
    }

    fn rebind_replacement_generation(
        &mut self,
        info: ReplacementInfo,
        new_index: RewriteDirectory,
    ) -> Result<ReplacementInfo> {
        let rebind = (|| -> Result<(File, SnapshotFile)> {
            #[cfg(test)]
            fail_rebind_after_publish_if_requested()?;
            let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
            // The published generation is a new file object; the single-writer
            // object lock must move with the writer.
            if let Some(lock) = self._lock.as_mut() {
                lock.bind_native(&file, &self.path)?;
            }
            let snapshot = SnapshotFile::new(file.try_clone()?)?;
            Ok((file, snapshot))
        })();

        match rebind {
            Ok((file, snapshot)) => {
                self.file = RecordFile::new(file);
                self.snapshot = snapshot;
                let last_resident_block_id = new_index.last().map(|entry| entry.block_id);
                // Rebinding succeeded and the complete new directory is ready.
                // Release the old private rows before optional compaction; an
                // assignment alone would keep them alive throughout its RHS.
                self.index = ResidentIndex::empty(self.snapshot.clone(), self.spec);
                self.index = new_index.into_resident(self.snapshot.clone(), self.spec);
                // The resident index was replaced wholesale; recover the O(1)
                // flush-cadence state once for the new generation
                // and the O(1) block tails with it . Record offsets
                // moved, so every cached generic keyed tail is stale .
                let derived = derive_index_state(self.index.iter(), true)?;
                self.checkpoint_cadence = derived.checkpoint_cadence;
                self.block_tails = derived.block_tails.expect("tails were requested");
                self.segment_cursor = derived.segment_cursor;
                // The same wholesale replacement: the newest entry is a
                // different record now, so the append-path shortcut that names
                // its block id is stale too.
                self.last_resident_block_id = last_resident_block_id;
                self.keyed_tails.invalidate_all();
                // The published file carries a *cold* mutable header block —
                // the rewrite could not carry the old offsets forward — so the
                // cached region has to say so too. Same length either way, which
                // is why nothing derived from `header_extensions.len()` moves.
                self.header_extensions =
                    header_extensions_for_replacement(self.spec, &self.header_extensions)?;
                // The published header's mutable blocks were reset to cold, and
                // one of them is the liveness bit. This handle still holds the
                // object, so leaving it clear would tell the *next* open that
                // the last writer released cleanly — and if this one then dies,
                // `mark_tail` would not run and `truncate_tail` would cut a tail
                // nobody was told about. Re-asserted rather than left to the
                // next open to notice, because by then it is the wrong answer.
                //
                // Not `mark_writer_open`: that re-reads the block for the
                // handle's own crash verdict, which would clobber the verdict
                // this handle already established at its open.
                self.write_liveness_flags(LIVENESS_FLAG_DIRTY)?;
                Ok(info)
            }
            Err(source) => {
                self.poison.poison();
                // the caller folds in a parent-directory sync failure it
                // already observed, via `Error::with_pending_parent_sync`.
                Err(crate::fallible::boxed_error(
                    source,
                    Error::ErrorContextAllocationFailed {
                        context: crate::AllocationFailureContext::PublishedRebind {
                            sequence: info.sequence,
                            parent_sync_pending: false,
                        },
                    },
                    |source| Error::PublishedButRebindFailed {
                        sequence: info.sequence,
                        source,
                        parent_sync: None,
                    },
                ))
            }
        }
    }

    fn validate_source_generation(&self) -> Result<()> {
        let current_len = self.file.metadata()?.len();
        if current_len != self.snapshot.len() {
            return Err(Error::InvalidCanonicalEncoding(
                "native file changed after it was indexed",
            ));
        }
        let mut file = self.snapshot.try_clone_file()?;
        validate_generation_index(
            self.spec,
            &mut file,
            append_log_start_for_file(self)?,
            &self.index,
        )
    }
}
