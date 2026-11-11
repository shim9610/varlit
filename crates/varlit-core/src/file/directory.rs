// Native file directory operations.

impl VarlitFile {
    /// The newest value written for `key`, or `None`.
    ///
    /// Visits every metadata record — latest-wins needs the whole walk, so
    /// there is no early break — but only *materializes* the ones it returns.
    /// `split_metadata_payload` compares the stored key against the borrowed
    /// payload, and the decode that allocates a `String` and a `Vec<u8>` runs
    /// only for a record that matches and wins.
    ///
    /// `max_materialized_bytes` bounds one record here, not the sum over the
    /// walk: a lookup keeps at most one value alive, and charging the sum
    /// refused a healthy file whose *total* metadata exceeded the ceiling
    /// (`STANDARD` sets it to 1 GiB) even when the requested entry was the
    /// first record in the file.
    pub fn metadata(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.metadata_in(self.resident_directory("metadata")?, key)
    }

    fn metadata_in<D: RecordDirectory + ?Sized>(
        &self,
        dir: &D,
        key: &str,
    ) -> Result<Option<Vec<u8>>> {
        let mut found = None;
        let mut budget = MaterializationBudget::new(self.spec);
        // One payload buffer for the whole walk. It used to be one per record,
        // and the walk is over every record in the file.
        let mut payload = Vec::new();
        for record_ordinal in 0..dir.record_count() {
            if directory_skips(dir, record_ordinal, &[METADATA_BLOCK_ID]) {
                continue;
            }
            let entry = dir.record_at(record_ordinal)?;
            if entry.block_id != METADATA_BLOCK_ID {
                continue;
            }
            budget.reset();
            let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
            budget.consume(logical_len)?;
            entry.read_payload_snapshot_into(self.spec, &self.snapshot, &mut payload)?;
            let (stored_key, _) = split_metadata_payload(&payload, self.spec.endian)?;
            if stored_key != key {
                continue;
            }
            let order = MergeOrder::for_record(0, entry.sequence, record_ordinal);
            let should_replace = found
                .as_ref()
                .is_none_or(|(old_order, _): &(MergeOrder, Vec<u8>)| order >= *old_order);
            if should_replace {
                // Decoded through the accounted decoder exactly as before, so
                // what a returned value costs the budget is unchanged.
                let (_, value): (String, Vec<u8>) = budget.decode(&payload, self.spec.endian)?;
                found = Some((order, value));
            }
        }
        Ok(found.map(|(_, value)| value))
    }

    /// **This allocates.** [`all_metadata_into`](Self::all_metadata_into) takes
    /// the caller's buffer instead.
    pub fn all_metadata(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let mut values = Vec::new();
        self.all_metadata_into(&mut values)?;
        Ok(values)
    }

    /// Fills the caller's buffer with every metadata pair.
    ///
    /// `out` is cleared and then extended. Sufficient outer capacity can be
    /// reused; metadata names and values still allocate separately, and a
    /// larger result may grow the buffer.
    pub fn all_metadata_into(&self, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
        self.all_metadata_into_in(self.resident_directory("all_metadata")?, out)
    }

    fn all_metadata_into_in<D: RecordDirectory + ?Sized>(
        &self,
        dir: &D,
        out: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<()> {
        out.clear();
        let mut budget = MaterializationBudget::new(self.spec);
        let mut payload = Vec::new();
        for position in 0..dir.record_count() {
            if directory_skips(dir, position, &[METADATA_BLOCK_ID]) {
                continue;
            }
            let entry = dir.record_at(position)?;
            if entry.block_id != METADATA_BLOCK_ID {
                continue;
            }
            let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
            budget.consume(logical_len)?;
            entry.read_payload_snapshot_into(self.spec, &self.snapshot, &mut payload)?;
            out.try_reserve(1).map_err(|_| Error::AllocationFailed {
                resource: "metadata entries",
                requested: logical_len,
            })?;
            out.push(budget.decode(&payload, self.spec.endian)?);
        }
        Ok(())
    }

    pub fn schema_manifest(&self) -> Result<Option<SchemaManifest>> {
        self.schema_manifest_in(self.resident_directory("schema_manifest")?)
    }

    fn schema_manifest_in<D: RecordDirectory + ?Sized>(
        &self,
        dir: &D,
    ) -> Result<Option<SchemaManifest>> {
        let mut newest: Option<(MergeOrder, RecordIndexEntry)> = None;
        for record_ordinal in 0..dir.record_count() {
            if directory_skips(dir, record_ordinal, &[MANIFEST_BLOCK_ID]) {
                continue;
            }
            let entry = dir.record_at(record_ordinal)?;
            if entry.block_id != MANIFEST_BLOCK_ID {
                continue;
            }
            let order = MergeOrder::for_record(0, entry.sequence, record_ordinal);
            // `>=`, not `>`: this replaced a `max_by_key`, which returns the
            // *last* of several equal maxima.
            if newest.as_ref().is_none_or(|(best, _)| order >= *best) {
                newest = Some((order, entry));
            }
        }
        let Some((_, entry)) = newest else {
            return Ok(None);
        };
        let mut budget = MaterializationBudget::new(self.spec);
        let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
        budget.consume(logical_len)?;
        let payload = entry.read_payload_snapshot(self.spec, &self.snapshot)?;
        Ok(Some(decode_schema_manifest(&payload, &mut budget)?))
    }

    /// The record offset of the newest record of `block_id`, or `None` if the
    /// file holds none.
    ///
    /// H2. The entry point of the block offset chain, and the only way to reach
    /// a non-resident block: from here each record's footer names its
    /// predecessor. `&self`, answered from the maintained tail table without
    /// touching the resident index.
    pub fn block_tail_offset(&self, block_id: u32) -> Option<u64> {
        self.block_tails.tail(block_id)
    }

    /// Walks a block's records newest-first through the footer chain.
    ///
    /// H2, and the only way to read a non-resident block. Takes `&self`, reads
    /// positionally through the open snapshot, and materialises one entry at a
    /// time — the resident index is never consulted and never grown, so this
    /// costs the working set rather than the record count.
    ///
    /// Refuses without `block_offset_chain`: without it the footer carries no
    /// predecessor, and a walk would silently stop after one record rather than
    /// report that it cannot answer.
    pub fn block_chain(&self, block_id: u32) -> Result<BlockChain<'_>> {
        if !self.spec.index_policy.block_offset_chain {
            return Err(Error::InvalidFormatSpec(
                "block_chain requires block_offset_chain",
            ));
        }
        Ok(BlockChain {
            spec: self.spec,
            snapshot: &self.snapshot,
            block_id,
            next: self.block_tails.tail(block_id),
            visited: 0,
        })
    }

    /// Walks one key's records, newest first, from `from`.
    ///
    /// Seed it with that key's newest offset --
    /// [`key_tail_offsets`](Self::key_tail_offsets) is where those come from --
    /// and the iterator follows
    /// [`prev_same_key_offset`](RecordIndexEntry::prev_same_key_offset) back
    /// through every earlier generation of the key. Turn an entry into a value
    /// with [`read_block_at`](Self::read_block_at) or
    /// [`decode_block_into`](Self::decode_block_into).
    ///
    /// **It crosses block ids, and that is deliberate** -- the one way it
    /// differs from [`Self::block_chain`], which refuses a step that leaves its
    /// block. A delete writes its tombstone under [`TOMBSTONE_BLOCK_ID`], and
    /// that tombstone carries a live predecessor and becomes the key's tail, so
    /// a walk that refused it would stop at the first deleted generation and
    /// look like an answer. Filter on `entry.block_id` yourself; the walk will
    /// not filter for you.
    ///
    /// **Merge ops are not on this chain.** [`Self::push_op`] appends under
    /// [`OP_BLOCK_ID`] with `prev_same_key_offset` set to `None` and does not
    /// move the key's tail, so this walk never surfaces one. A history
    /// reconstructed from it alone omits every op-applied mutation; a caller
    /// using [`VarlitMerge`] has to fold the ops in by another route.
    /// Measured by `an_op_record_is_not_on_the_chain` in
    /// `crates/varlit/tests/keyed_chain_walk.rs`.
    ///
    /// Lazy and `&self`, exactly like `block_chain`: one offset is held, so a
    /// key with more generations than memory is still walkable. Each step is
    /// charged against `max_records`, and the chain must strictly decrease, so a
    /// crafted or damaged file cannot make the walk loop.
    ///
    /// Refuses without `keyed_offset_chain`: without it the writer records no
    /// predecessor at all
    /// ([`prev_same_key_offset`](RecordIndexEntry::prev_same_key_offset) is
    /// `None` on every record), so a walk would stop after one record rather
    /// than report that it cannot answer. Same refusal, for the same reason,
    /// that `block_chain` carries.
    pub fn keyed_chain(&self, from: u64) -> Result<KeyedChain<'_>> {
        if !self.spec.index_policy.keyed_offset_chain {
            return Err(Error::InvalidFormatSpec(
                "keyed_chain requires keyed_offset_chain",
            ));
        }
        Ok(KeyedChain {
            spec: self.spec,
            snapshot: &self.snapshot,
            next: Some(from),
            visited: 0,
        })
    }

    /// Decodes the record at `record_offset` as `T`.
    ///
    /// The companion to [`Self::block_chain`]: the walk yields offsets and
    /// entries, this turns one into a value. Positional and `&self`, so a
    /// non-resident block is read without anything entering the resident index.
    ///
    /// `out` receives the value, reusing what it owns; `scratch` receives the
    /// record's bytes. On error `out` is valid but unspecified.
    pub fn read_block_at<T: VarlitBlock>(
        &self,
        record_offset: u64,
        scratch: &mut Vec<u8>,
        out: &mut T,
    ) -> Result<()> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let entry = read_record_entry_positional(self.spec, &self.snapshot, record_offset)?;
        if entry.block_id != T::ID {
            return Err(Error::UnregisteredBlock(entry.block_id));
        }
        if entry.block_version != T::VERSION {
            return Err(Error::BlockVersionMismatch {
                block_id: T::ID,
                expected: T::VERSION,
                actual: entry.block_version,
            });
        }
        let mut budget = MaterializationBudget::new(self.spec);
        let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
        budget.consume(logical_len)?;
        entry.read_logical_payload_snapshot_into(self.spec, &self.snapshot, scratch)?;
        budget.decode_into(scratch, T::ENDIAN.unwrap_or(self.spec.endian), out)
    }

    /// Frames the record at `record_offset` into its index entry.
    ///
    /// The other half of [`Self::read_block_at`]. That one turns an offset into
    /// a *value* and discards the entry it framed to get there; this returns the
    /// entry, which is what a caller following a chain of their own needs -- the
    /// entry is where the next offset lives
    /// ([`prev_same_key_offset`](RecordIndexEntry::prev_same_key_offset),
    /// [`prev_same_block_offset`](RecordIndexEntry::prev_same_block_offset)).
    /// Without it a hand-written walk reads the value at hop one and has nowhere
    /// to go for hop two.
    ///
    /// It publishes no information [`Self::block_chain`] does not already hand
    /// out; it hands out the same entries by a route the caller steers.
    ///
    /// Positional and `&self`: two fixed-size reads through the snapshot, no
    /// allocation, and nothing enters the resident index -- so a walk over a
    /// file larger than memory costs the hops and not the file.
    ///
    /// **The offset is not trusted.** It is the caller's, so it may name
    /// anything; an extent outside this handle's snapshot is refused, and bytes
    /// that do not frame a record are refused. What it cannot do is reach
    /// outside this file, so the worst a wrong offset buys is an error or some
    /// other record of the same file. Identical in that respect to
    /// [`Self::read_payload_into`], which takes a whole caller-supplied entry.
    pub fn record_entry_at(&self, record_offset: u64) -> Result<RecordIndexEntry> {
        read_record_entry_positional(self.spec, &self.snapshot, record_offset)
    }

    /// Reads the stored bytes of a record the caller already holds an entry
    /// for, into a buffer the caller already owns.
    ///
    /// This is the half of "the host manages the memory" that the `_into`
    /// family alone does not give you. [`index_entries_into`](Self::index_entries_into)
    /// hands a host the index; without this, using one of those entries meant
    /// [`RecordIndexEntry::read_payload`], which reopens the file **by path** —
    /// a different object if the path was replaced — and allocates. So the two
    /// halves of the pattern are: take the index into your buffer once, then
    /// read through it into your buffer as often as you like, both bound to
    /// this open handle.
    ///
    /// `out` ends exactly the record's stored length. Optional frame/CRC checks
    /// follow [`crate::ReadValidation`], identically to internal payload reads.
    /// `TRUSTED` accepts the supplied metadata while retaining extent and
    /// allocation limits.
    ///
    /// The caller supplies the entry. Snapshot extent checks remain mandatory;
    /// checksum mismatches are refused when the read policy verifies data CRCs.
    /// `TRUSTED` skips optional frame and data CRC checks. An entry cannot
    /// reach outside this file — every offset is validated against this
    /// handle's snapshot bounds — so the worst a wrong entry buys is an error
    /// or some other record of the same file.
    pub fn read_payload_into(&self, entry: &RecordIndexEntry, out: &mut Vec<u8>) -> Result<()> {
        entry.read_payload_snapshot_into(self.spec, &self.snapshot, out)
    }

    /// The same, decompressed if the record is compressed.
    ///
    /// For an uncompressed record this is a read straight into `out` with no
    /// copy. For a compressed one the decompressor produces its own buffer and
    /// `out` is replaced — said here because it is the one case where handing
    /// in a buffer does not save the allocation.
    pub fn read_logical_payload_into(
        &self,
        entry: &RecordIndexEntry,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        entry.read_logical_payload_snapshot_into(self.spec, &self.snapshot, out)
    }

    /// Decodes a record the caller already holds an entry for, reading its
    /// bytes through the caller's buffer.
    ///
    /// The value is decoded into `out`, reusing the vectors and strings it
    /// already owns; `scratch` keeps the last record's bytes. With both
    /// buffers reused, a steady walk of same-shaped uncompressed records
    /// allocates nothing; decompression may still allocate. On error `out` is
    /// valid but unspecified.
    ///
    /// Refuses an entry whose block id or version is not `T`'s, exactly as
    /// [`read_block_at`](Self::read_block_at) does. Unlike `read_block_at` it
    /// does not re-read the record header to find that out — the caller's entry
    /// already says it, which is the point of having kept the entry.
    pub fn decode_block_into<T: VarlitBlock>(
        &self,
        entry: &RecordIndexEntry,
        scratch: &mut Vec<u8>,
        out: &mut T,
    ) -> Result<()> {
        let mut budget = MaterializationBudget::new(self.spec);
        self.read_block_payload::<T>(entry, scratch, &mut budget)?;
        budget.decode_into(scratch, T::ENDIAN.unwrap_or(self.spec.endian), out)
    }

    /// [`Self::decode_block_into`] as a fresh value, for crate checks that
    /// discard it.
    pub(crate) fn decode_block_owned<T: VarlitBlock>(
        &self,
        entry: &RecordIndexEntry,
        scratch: &mut Vec<u8>,
    ) -> Result<T> {
        let mut budget = MaterializationBudget::new(self.spec);
        self.read_block_payload::<T>(entry, scratch, &mut budget)?;
        budget.decode(scratch, T::ENDIAN.unwrap_or(self.spec.endian))
    }

    fn read_block_payload<T: VarlitBlock>(
        &self,
        entry: &RecordIndexEntry,
        scratch: &mut Vec<u8>,
        budget: &mut MaterializationBudget,
    ) -> Result<()> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        if entry.block_id != T::ID {
            return Err(Error::UnregisteredBlock(entry.block_id));
        }
        if entry.block_version != T::VERSION {
            return Err(Error::BlockVersionMismatch {
                block_id: T::ID,
                expected: T::VERSION,
                actual: entry.block_version,
            });
        }
        let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
        budget.consume(logical_len)?;
        entry.read_logical_payload_snapshot_into(self.spec, &self.snapshot, scratch)?;
        Ok(())
    }

    /// This handle's own record directory, or the typed refusal that says who
    /// has it instead.
    ///
    /// Every position-based read goes through here rather than touching
    /// `self.index`, so a handle opened without a directory refuses by name
    /// instead of answering as though the file were empty.
    fn resident_directory(&self, operation: &'static str) -> Result<&ResidentIndex> {
        if self.index.is_retained() {
            Ok(&self.index)
        } else {
            Err(Error::NoResidentDirectory { operation })
        }
    }

    /// Reads that resolve positions through a directory **the caller supplies**.
    ///
    /// Every method on the returned view is the same read the handle offers,
    /// answered against `directory` instead of against the handle's own. The
    /// directory is anything implementing [`RecordDirectory`] — in practice the
    /// `Vec<RecordIndexEntry>` that [`index_entries_into`](Self::index_entries_into)
    /// or [`open_readonly_with_scratch`](Self::open_readonly_with_scratch)
    /// filled.
    ///
    /// ```ignore
    /// let mut index = Vec::new();
    /// let file = VarlitFile::open_readonly_with_scratch(spec, path, &mut index)?;
    /// // `index` is the file's record index; the handle has its own copy too.
    /// let blocks = file.with_directory(&index).blocks::<Point>()?;
    /// ```
    ///
    /// **Why this exists.** The handle keeps a 16-byte directory slot per
    /// record, and that is the last thing about a read that still scales with
    /// the file. A handle that stopped keeping it must not lose `blocks`,
    /// `scan` and the rest — it must ask whoever has the directory for it.
    /// This is that ask, and it is available whether or not the handle also
    /// has one, so the two can be compared on the same file.
    pub fn with_directory<'a, D: RecordDirectory + ?Sized>(
        &'a self,
        directory: &'a D,
    ) -> Result<DirectoryRead<'a, D>> {
        self.check_directory_describes_this_file(directory)?;
        Ok(DirectoryRead {
            file: self,
            directory,
        })
    }

    /// Refuses a directory whose entries are not where it says they are.
    ///
    /// A supplied directory is trusted offsets, and trusting an offset is
    /// correct — it is what makes handing over the scan buffer cheaper than
    /// keeping a resident index. What was never bounded is *which file* the
    /// offsets are trusted against. `reopen_readonly` made the unbounded
    /// version reachable in an ordinary sequence rather than by a mistake:
    /// build a directory, let a `replace_*` republish the pathname, reopen, and
    /// read through the directory you already had. Measured on an
    /// `integrity: none` format, eight records read back as zeros.
    ///
    /// # Why it is derived rather than declared
    ///
    /// The obvious fix — a provenance tag on the directory — cannot be applied
    /// to the directory that matters most. `Vec<RecordIndexEntry>` *is* the
    /// scan buffer, it is a foreign type, and no field can be added to it. So
    /// the check asks the file instead: re-frame what the directory claims and
    /// require the record to still be there. Same shape as the header-tail
    /// slot, which re-frames every offset it adopts rather than believing the
    /// table.
    ///
    /// # What it costs, and what it therefore does not check
    ///
    /// Two record headers, at `with_directory` rather than per read, so a walk
    /// of 600,000 records pays it once. Checking every entry would be a second
    /// full pass and would cost exactly what the supplied directory exists to
    /// avoid. The two checked are the first and the last, which is what catches
    /// the case this exists for: a republish moves every record by a uniform
    /// delta, so an entry at either end lands on a different record — a
    /// different sequence above all, since sequences are file-global and a
    /// rewrite preserves them per record rather than per offset.
    ///
    /// Be exact about the residual: this is a **disagreement detector, not an
    /// authenticator**. A directory that is wrong only in the middle passes, and
    /// a caller who fabricates entries that happen to frame correctly is not
    /// stopped. It converts the common accident into a typed refusal; it does
    /// not make a supplied directory a trusted input.
    ///
    /// An empty directory describes nothing and is accepted: there is no claim
    /// to disagree with, and refusing it would break the legitimate "I want no
    /// records" call.
    fn check_directory_describes_this_file<D: RecordDirectory + ?Sized>(
        &self,
        directory: &D,
    ) -> Result<()> {
        let count = directory.record_count();
        let Some(last) = count.checked_sub(1) else {
            return Ok(());
        };
        let limit = self.snapshot.len();
        for position in if last == 0 { vec![0] } else { vec![0, last] } {
            let stated = directory.record_at(position)?;
            let mismatch = || Error::DirectoryDoesNotDescribeThisFile {
                position,
                offset: stated.record_offset,
            };
            // Outside the snapshot is the same disagreement seen from the other
            // end, and it has to be answered before the read rather than by it:
            // a positional read past the logical length is already refused, but
            // as `UnexpectedEof`, which says nothing about the directory.
            if stated.checked_physical_end().map_err(|_| mismatch())? > limit {
                return Err(mismatch());
            }
            // The header only. Building a whole entry through
            // `fault_record_entry` would also read the footer and would charge
            // the record-entry fault counter, which several tests measure as a
            // cost model — this probe is not a fault, it is a check.
            let mut header_bytes = [0; RECORD_HEADER_LEN as usize];
            self.snapshot
                .read_exact_at(stated.record_offset, &mut header_bytes)?;
            let actual =
                read_native_record_header(&mut header_bytes.as_slice(), stated.record_offset)?
                    .fields;
            // `sequence` is the discriminator that carries this: it is
            // file-global and strictly increasing, and a republish preserves it
            // per *record*, so a record that moved leaves a different one at
            // the offset it vacated.
            if !record_header_describes(&actual, &stated) {
                return Err(mismatch());
            }
        }
        Ok(())
    }

    /// A [`RecordMap`] over the caller's buffer: an index built one record at a
    /// time, only as far as the caller asks.
    ///
    /// **Reads nothing.** The walk starts on the first
    /// [`advance`](RecordMap::advance), [`find`](RecordMap::find) or
    /// [`fill_to`](RecordMap::fill_to), so a map that is never asked a question
    /// costs one `Vec::clear`.
    ///
    /// This is the third answer to "who holds the index", next to the handle's
    /// own 16-byte directory and the caller's full
    /// [`index_entries_into`](Self::index_entries_into) buffer: **nobody holds
    /// all of it, because nobody read all of it.** The map borrows the buffer
    /// rather than the handle, so reads through the handle
    /// (`read_payload_into`, `decode_block_into`) stay available while the map
    /// is alive.
    ///
    /// Available on a handle opened without a directory
    /// ([`open_readonly_without_directory`](Self::open_readonly_without_directory)),
    /// which is the pairing it exists for: the handle keeps nothing, and the
    /// caller builds exactly the part of the index its question needed.
    pub fn record_map<'a>(&self, buffer: &'a mut Vec<RecordIndexEntry>) -> Result<RecordMap<'a>> {
        RecordMap::new(self, buffer)
    }

    /// The block's records as a lazy collection: one decoded value at a time.
    ///
    /// **This allocates** one index entry per matching record, which is what
    /// makes `get(i)` positional.
    /// [`block_entries_into`](Self::block_entries_into) fills the caller's
    /// buffer with those same entries instead.
    ///
    /// Deliberately *not* pointing at
    /// [`decode_blocks_into`](Self::decode_blocks_into): that one decodes every
    /// matching record, so it is bounded by the caller's buffer and not by the
    /// file, which is the opposite of what this collection is for.
    pub fn blocks<T: VarlitBlock>(&self) -> Result<BlockVec<T>> {
        self.blocks_in(self.resident_directory("blocks")?)
    }

    fn blocks_in<T: VarlitBlock, D: RecordDirectory + ?Sized>(
        &self,
        dir: &D,
    ) -> Result<BlockVec<T>> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        // A non-resident block has no entries here, and an empty collection
        // would say "nothing was written" rather than "not through this door".
        crate::collections::ensure_resident_block::<T>(self.spec)?;
        // **One traversal, charged per entry.** This used to walk the index
        // once to count and once to copy, so that `IndexBytes` could be charged
        // for the exact total before the first copy. That was nearly free when
        // the index was a `Vec` in memory. It is not free now: each traversal
        // faults every entry off disk, so the second pass was `N` extra
        // positional reads to learn a number the first pass already knew.
        //
        // The charge still precedes the memory — it is now per entry, the same
        // shape `key_tail_offsets` uses for `KeyedTailBytes` — so a file that
        // exceeds the ceiling is still refused during the build rather than
        // after the whole array is resident.
        let mut entries: Vec<RecordIndexEntry> = Vec::new();
        for position in 0..dir.record_count() {
            if directory_skips(dir, position, &[T::ID]) {
                continue;
            }
            let entry = dir.record_at(position)?;
            if entry.block_id != T::ID {
                continue;
            }
            let requested = index_bytes_for_count(entries.len().saturating_add(1))?;
            self.spec
                .read_limits
                .check(ReadLimitKey::IndexBytes, requested)?;
            entries
                .try_reserve(1)
                .map_err(|_| Error::AllocationFailed {
                    resource: "block index",
                    requested,
                })?;
            entries.push(entry);
        }
        Ok(BlockVec::new(self.spec, self.snapshot.clone(), entries))
    }

    /// Fills the caller's buffer with the index entry of every record of type
    /// `T` — the entries [`blocks`](Self::blocks) would have allocated.
    ///
    /// `out` is cleared and then extended. This is the caller-supplied-buffer
    /// form of `blocks()`: same entries, same per-entry `IndexBytes` charge,
    /// same laziness — no payload is read and nothing is decoded.
    pub fn block_entries_into<T: VarlitBlock>(
        &self,
        out: &mut Vec<RecordIndexEntry>,
    ) -> Result<()> {
        self.block_entries_into_in::<T, _>(self.resident_directory("block_entries_into")?, out)
    }

    fn block_entries_into_in<T: VarlitBlock, D: RecordDirectory + ?Sized>(
        &self,
        dir: &D,
        out: &mut Vec<RecordIndexEntry>,
    ) -> Result<()> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        crate::collections::ensure_resident_block::<T>(self.spec)?;
        out.clear();
        for position in 0..dir.record_count() {
            if directory_skips(dir, position, &[T::ID]) {
                continue;
            }
            let entry = dir.record_at(position)?;
            if entry.block_id != T::ID {
                continue;
            }
            let requested = index_bytes_for_count(out.len().saturating_add(1))?;
            self.spec
                .read_limits
                .check(ReadLimitKey::IndexBytes, requested)?;
            out.try_reserve(1).map_err(|_| Error::AllocationFailed {
                resource: "block index",
                requested,
            })?;
            out.push(entry);
        }
        Ok(())
    }

    /// Decodes **every** record of type `T` into the caller's buffer.
    ///
    /// `out` ends holding exactly the decoded records. Elements it already
    /// held are decoded into, so their vectors and strings are reused; it
    /// only grows past its old length. On error it holds the records decoded
    /// before the failing one.
    ///
    /// **Not the `_into` twin of [`blocks`](Self::blocks), and named so it
    /// cannot be mistaken for one.** `blocks()` returns a lazy collection whose
    /// peak is one payload however many records it covers; this materializes
    /// all of them, so what it costs is bounded by the caller's buffer and not
    /// by the file. For a block carrying a `String` or a `Vec` that is
    /// unbounded in the heap as well as in the buffer.
    ///
    /// It shipped for one round as `blocks_into`, with `blocks()`'s
    /// documentation recommending it as the memory-friendly form. That was
    /// backwards and is corrected here: reach for it when you want all of them
    /// anyway, and for [`block_entries_into`](Self::block_entries_into) when
    /// you want what `blocks()` holds.
    ///
    /// The per-record charge is unchanged from `BlockVec::get`: the
    /// materialization ceiling bounds one payload, not the sum over the walk.
    pub fn decode_blocks_into<T: VarlitBlock>(&self, out: &mut Vec<T>) -> Result<()> {
        self.decode_blocks_into_in::<T, _>(self.resident_directory("decode_blocks_into")?, out)
    }

    fn decode_blocks_into_in<T: VarlitBlock, D: RecordDirectory + ?Sized>(
        &self,
        dir: &D,
        out: &mut Vec<T>,
    ) -> Result<()> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        crate::collections::ensure_resident_block::<T>(self.spec)?;
        // Elements already in `out` are decoded into (keeping what they own);
        // only records beyond its length are pushed. Truncated to the filled
        // prefix on every exit.
        let mut filled = 0usize;
        let result = self.decode_blocks_fill(dir, out, &mut filled);
        out.truncate(filled);
        result
    }

    fn decode_blocks_fill<T: VarlitBlock, D: RecordDirectory + ?Sized>(
        &self,
        dir: &D,
        out: &mut Vec<T>,
        filled: &mut usize,
    ) -> Result<()> {
        let mut budget = MaterializationBudget::new(self.spec);
        let mut payload = Vec::new();
        for position in 0..dir.record_count() {
            if directory_skips(dir, position, &[T::ID]) {
                continue;
            }
            let entry = dir.record_at(position)?;
            if entry.block_id != T::ID {
                continue;
            }
            if entry.block_version != T::VERSION {
                return Err(Error::BlockVersionMismatch {
                    block_id: T::ID,
                    expected: T::VERSION,
                    actual: entry.block_version,
                });
            }
            // One record's materialization at a time, exactly as `BlockVec::get`
            // and `BlockCursor::next_into` charge it: the ceiling bounds one
            // payload, not the sum over the walk.
            budget.reset();
            let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
            budget.consume(logical_len)?;
            entry.read_logical_payload_snapshot_into(self.spec, &self.snapshot, &mut payload)?;
            let endian = T::ENDIAN.unwrap_or(self.spec.endian);
            if let Some(slot) = out.get_mut(*filled) {
                budget.decode_into(&payload, endian, slot)?;
            } else {
                out.try_reserve(1).map_err(|_| Error::AllocationFailed {
                    resource: "decoded blocks",
                    requested: logical_len,
                })?;
                out.push(budget.decode(&payload, endian)?);
            }
            *filled += 1;
        }
        Ok(())
    }

    /// **This allocates.** [`blocks_migrated_into`](Self::blocks_migrated_into)
    /// takes the caller's buffer instead.
    pub fn blocks_migrated<From, To, M>(&self) -> Result<Vec<To>>
    where
        From: VarlitBlock,
        To: VarlitBlock,
        M: VarlitMigration<From, To>,
    {
        let mut migrated = Vec::new();
        self.blocks_migrated_into::<From, To, M>(&mut migrated)?;
        Ok(migrated)
    }

    /// Fills the caller's buffer with every migrated block.
    ///
    /// `out` is cleared and then extended. The peak is still one payload at a
    /// time — the decode is per record and the decoded value is moved straight
    /// into `out` — so what the caller now owns is the only thing that grows.
    pub fn blocks_migrated_into<From, To, M>(&self, out: &mut Vec<To>) -> Result<()>
    where
        From: VarlitBlock,
        To: VarlitBlock,
        M: VarlitMigration<From, To>,
    {
        self.blocks_migrated_into_in::<From, To, M, _>(
            self.resident_directory("blocks_migrated")?,
            out,
        )
    }

    fn blocks_migrated_into_in<From, To, M, D: RecordDirectory + ?Sized>(
        &self,
        dir: &D,
        out: &mut Vec<To>,
    ) -> Result<()>
    where
        From: VarlitBlock,
        To: VarlitBlock,
        M: VarlitMigration<From, To>,
    {
        if From::ID != To::ID {
            return Err(Error::MigrationBlockIdMismatch {
                from: From::ID,
                to: To::ID,
            });
        }
        out.clear();
        let mut budget = MaterializationBudget::new(self.spec);
        let mut payload = Vec::new();
        for position in 0..dir.record_count() {
            if directory_skips(dir, position, &[From::ID]) {
                continue;
            }
            let entry = dir.record_at(position)?;
            if entry.block_id != From::ID || entry.block_version != From::VERSION {
                continue;
            }
            let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
            budget.consume(logical_len)?;
            entry.read_logical_payload_snapshot_into(self.spec, &self.snapshot, &mut payload)?;
            let from: From = budget.decode(&payload, From::ENDIAN.unwrap_or(self.spec.endian))?;
            out.try_reserve(1).map_err(|_| Error::AllocationFailed {
                resource: "migrated blocks",
                requested: logical_len,
            })?;
            out.push(M::migrate(from)?);
        }
        Ok(())
    }

    /// **This allocates** two collections that scale with the file: an index
    /// entry per matching record, and a map entry per distinct key.
    /// [`keyed_blocks_into`](Self::keyed_blocks_into) fills the caller's.
    pub fn keyed_blocks<T>(&self) -> Result<KeyedBlockVec<T::Key, T>>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash + Clone,
    {
        // evaluated here as well as in `keyed_blocks_into`. It is not
        // redundant: this is a `const` assertion, and the compile error names
        // the function whose instantiation first forced it. Leaving it only in
        // the callee moved `fail_keyed_contradiction`'s diagnostic off the
        // user's own call and into this file, which is a worse error for a
        // mistake the user made.
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        let mut entries = Vec::new();
        let mut by_key = HashMap::new();
        self.keyed_blocks_into::<T>(&mut entries, &mut by_key)?;
        let blocks = BlockVec::new(self.spec, self.snapshot.clone(), entries);
        Ok(KeyedBlockVec::from_parts(blocks, by_key))
    }

    /// Fills the caller's two collections: the block's index entries in file
    /// order, and the newest entry per key.
    ///
    /// Both are cleared and then filled. They are exactly what
    /// [`keyed_blocks`](Self::keyed_blocks) would have allocated, and
    /// `KeyedBlockVec::from_parts` reassembles a collection from them if that
    /// is what the caller wants.
    ///
    /// **Two of three, and here is the third.** The walk also builds a
    /// transient `HashMap<T::Key, (MergeOrder, Option<RecordIndexEntry>)>`,
    /// which is what decides "newest" when a key repeats. It cannot be the
    /// caller's without putting `MergeOrder` — a private ordering type — into a
    /// public signature. So this removes one map of `distinct keys` and the
    /// entry vector, and leaves that one. Stated rather than implied, because
    /// the saving is two thirds and not all of it.
    pub fn keyed_blocks_into<T>(
        &self,
        entries: &mut Vec<RecordIndexEntry>,
        by_key: &mut HashMap<T::Key, RecordIndexEntry>,
    ) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash + Clone,
    {
        self.keyed_blocks_into_in::<T, _>(self.resident_directory("keyed_blocks")?, entries, by_key)
    }

    fn keyed_blocks_into_in<T, D: RecordDirectory + ?Sized>(
        &self,
        dir: &D,
        entries: &mut Vec<RecordIndexEntry>,
        by_key: &mut HashMap<T::Key, RecordIndexEntry>,
    ) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash + Clone,
    {
        // compile-time keyedness contract (also covers the
        // VarlitReader::keyed_blocks wrapper); registration is the runtime
        // backstop.
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        entries.clear();
        by_key.clear();
        let mut state: HashMap<T::Key, (MergeOrder, Option<RecordIndexEntry>)> = HashMap::new();
        let mut budget = MaterializationBudget::new(self.spec);
        // One payload buffer for the whole walk, not one per record.
        let mut payload = Vec::new();
        for record_ordinal in 0..dir.record_count() {
            if directory_skips(dir, record_ordinal, &[T::ID, TOMBSTONE_BLOCK_ID]) {
                continue;
            }
            let entry = dir.record_at(record_ordinal)?;
            // One record's materialization at a time. Every decoded block is
            // dropped once its key is taken; what survives the loop is index
            // entries and keys, each charged as it is taken through
            // `index_bytes_for_count`. See `MaterializationBudget`.
            budget.reset();
            let order = MergeOrder::for_record(0, entry.sequence, record_ordinal);
            match entry.block_id {
                id if id == T::ID => {
                    if entry.block_version != T::VERSION {
                        return Err(Error::BlockVersionMismatch {
                            block_id: T::ID,
                            expected: T::VERSION,
                            actual: entry.block_version,
                        });
                    }
                    let logical_len =
                        entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
                    budget.consume(logical_len)?;
                    entry.read_logical_payload_snapshot_into(
                        self.spec,
                        &self.snapshot,
                        &mut payload,
                    )?;
                    let block: T =
                        budget.decode(&payload, T::ENDIAN.unwrap_or(self.spec.endian))?;
                    let key = block.key();
                    if should_apply(state.get(&key), order) {
                        let requested = index_bytes_for_count(state.len().saturating_add(1))?;
                        state.try_reserve(1).map_err(|_| Error::AllocationFailed {
                            resource: "keyed index",
                            requested,
                        })?;
                        state.insert(key, (order, Some(entry.clone())));
                    }
                    let requested = index_bytes_for_count(entries.len().saturating_add(1))?;
                    entries
                        .try_reserve(1)
                        .map_err(|_| Error::AllocationFailed {
                            resource: "block index",
                            requested,
                        })?;
                    entries.push(entry.clone());
                }
                TOMBSTONE_BLOCK_ID => {
                    let logical_len =
                        entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
                    budget.consume(logical_len)?;
                    entry.read_payload_snapshot_into(self.spec, &self.snapshot, &mut payload)?;
                    if let Some(key) =
                        decode_internal_key_payload::<T>(self.spec.endian, &payload, &mut budget)?
                        && should_apply(state.get(&key), order)
                    {
                        let requested = index_bytes_for_count(state.len().saturating_add(1))?;
                        state.try_reserve(1).map_err(|_| Error::AllocationFailed {
                            resource: "keyed index",
                            requested,
                        })?;
                        state.insert(key, (order, None));
                    }
                }
                _ => {}
            }
        }
        let requested = index_bytes_for_count(state.len())?;
        by_key
            .try_reserve(state.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "keyed index",
                requested,
            })?;
        for (key, (_, entry)) in state {
            if let Some(entry) = entry {
                by_key.insert(key, entry);
            }
        }
        Ok(())
    }

    /// **This allocates.**
    /// [`materialized_keyed_blocks_into`](Self::materialized_keyed_blocks_into)
    /// takes the caller's map instead.
    pub fn materialized_keyed_blocks<T>(&self) -> Result<HashMap<T::Key, T>>
    where
        T: VarlitMerge,
        T::Key: Eq + Hash,
    {
        let mut values = HashMap::new();
        self.materialized_keyed_blocks_into::<T>(&mut values)?;
        Ok(values)
    }

    /// Fills the caller's map with the merged live value per key.
    ///
    /// `out` is cleared and then filled. The transient merge state is still
    /// this crate's — it holds a `MergeOrder` beside each value and cannot be
    /// the caller's map without leaking that ordering into the signature — so
    /// what this saves is the second map, not the first. Said plainly rather
    /// than implied: reusing `out` across calls removes one allocation of
    /// `distinct keys`, not both.
    pub fn materialized_keyed_blocks_into<T>(&self, out: &mut HashMap<T::Key, T>) -> Result<()>
    where
        T: VarlitMerge,
        T::Key: Eq + Hash,
    {
        self.materialized_keyed_blocks_into_in::<T, _>(
            self.resident_directory("materialized_keyed_blocks")?,
            out,
        )
    }

    fn materialized_keyed_blocks_into_in<T, D: RecordDirectory + ?Sized>(
        &self,
        dir: &D,
        out: &mut HashMap<T::Key, T>,
    ) -> Result<()>
    where
        T: VarlitMerge,
        T::Key: Eq + Hash,
    {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let mut state: HashMap<T::Key, (MergeOrder, Option<T>)> = HashMap::new();
        let mut budget = MaterializationBudget::new(self.spec);
        apply_merge_entries::<T, _>(
            self.spec,
            &self.snapshot,
            dir,
            MergeShard::single_file(),
            &mut state,
            &mut budget,
            u64::MAX,
        )?;
        out.clear();
        let requested = allocation_bytes::<(T::Key, T)>(state.len(), "materialized keyed map")?;
        out.try_reserve(state.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "materialized keyed map",
                requested,
            })?;
        for (key, (_, value)) in state {
            if let Some(value) = value {
                out.insert(key, value);
            }
        }
        Ok(())
    }

    /// Every record's block identity and extent, in file order.
    ///
    /// **Each item is fallible, and the iterator is lazy.** Producing an event
    /// means producing the index entry behind it, which is a read on a store
    /// that does not keep them all in memory; the two ways to hide that are to
    /// materialize the whole scan into a `Vec` first — the eager load this
    /// exists not to do — or to swallow a read error as an early `None`, which
    /// would report a truncated file as a short one. So the `Result` is here,
    /// on each item, and nothing is read until the item is asked for.
    /// **One item, and it is the refusal**, when this handle keeps no directory.
    /// `scan` returns an iterator rather than a `Result`, so there is nowhere
    /// else to put it — and an empty iterator would say "no records", which is
    /// the answer a directory-less handle must never give.
    pub fn scan(&self) -> impl Iterator<Item = Result<BlockEvent>> + '_ {
        let refusal = self.resident_directory("scan").err().map(Err).into_iter();
        let events = self
            .index
            .is_retained()
            .then(|| Self::scan_in(&self.index))
            .into_iter()
            .flatten();
        refusal.chain(events)
    }

    fn scan_in<D: RecordDirectory + ?Sized>(
        dir: &D,
    ) -> impl Iterator<Item = Result<BlockEvent>> + '_ {
        directory_records(dir).map(|entry| entry.map(|entry| BlockEvent::from(&entry)))
    }

    /// Verifies every record's stored checksum against its bytes.
    ///
    /// Available on demand so that choosing
    /// [`IntegrityVerification::OnDemand`] defers the announcement
    /// [`IntegrityVerification::AtOpen`] makes while opening, rather than
    /// losing it. Reads every payload, so it costs a full pass over the file;
    /// that cost is the reason it is not the default at open.
    ///
    /// **What "every record" covers.** Two passes, because two things hold the
    /// records: the resident index, and — for a block declared
    /// `resident: false` — that block's footer chain, which is the only way
    /// back to records the index deliberately does not mirror
    /// ([`Self::block_chain`]). The chain pass used to be missing, so a
    /// non-resident block's records were silently omitted; a file whose only
    /// appends were non-resident reported `Ok(0)`, indistinguishable from an
    /// empty file. Both passes call the same `read_payload_snapshot` the read
    /// path calls, so there is still exactly one verification in the crate.
    ///
    /// This is *not* bit-for-bit what `AtOpen` does. `AtOpen` verifies every
    /// physical record as scan frames it, including records after the commit
    /// boundary that this handle excludes; this pass answers for the records
    /// the handle can see.
    ///
    /// **Bounded by the snapshot.** The chain pass reads through the same
    /// snapshot as every other read on this handle, and a read-only handle
    /// sizes that snapshot from the last *resident* record. Under a commit
    /// policy that leaves no resident record after the block's newest one —
    /// [`CommitPolicy::None`], which is what a format declaring no commit
    /// policy gets — the walk therefore reports
    /// [`Error::SnapshotRangeOutOfBounds`] rather than a clean verdict over
    /// bytes it never read. That is the refusal [`Self::block_chain`] already
    /// gives on the same handle: such a block is unreachable read-only in that
    /// configuration, and only `verify_all`'s `Ok` said otherwise. A read-write
    /// handle sizes the snapshot from the file and is unaffected, as is any
    /// policy that ends the file with a resident record
    /// (`transaction_marker`). Measured, 12 non-resident records under
    /// `CommitPolicy::None`: read-write `Ok(12)`, read-only
    /// `Err(SnapshotRangeOutOfBounds { offset: 1347, len: 32, snapshot_len: 27 })`.
    ///
    /// Takes `&self` and reads positionally within the owning thread.
    /// One entry and one payload are materialised
    /// at a time; nothing enters the resident index.
    ///
    /// Returns the number of records verified. A mismatch is
    /// [`Error::ChecksumMismatch`] naming the offending record's offset.
    pub fn verify_all(&self) -> Result<usize> {
        self.verify_all_in(self.resident_directory("verify_all")?)
    }

    fn verify_all_in<D: RecordDirectory + ?Sized>(&self, dir: &D) -> Result<usize> {
        if self.spec.integrity_policy == IntegrityPolicy::None {
            return Ok(0);
        }
        let mut verified = 0usize;
        // The bytes are read to check their checksum and then dropped, so one
        // buffer serves the whole pass. This was an allocation per record.
        let mut payload = Vec::new();
        for entry in directory_records(dir) {
            let entry = entry?;
            // `read_payload_snapshot` is the same check the read path performs,
            // which is exactly the point: there is one verification in the
            // crate, and this method chooses when it runs rather than adding a
            // second copy of it.
            entry.read_payload_snapshot_into(
                self.spec.with_strict_read_validation(),
                &self.snapshot,
                &mut payload,
            )?;
            verified += 1;
        }
        // A non-resident block's records are never in `self.index` (a
        // descriptor may only name a declared user block, and only ids below
        // `RESERVED_BLOCK_ID_START` can be declared), so this adds to the pass
        // above and cannot double-count it.
        for descriptor in self.spec.block_residency {
            if descriptor.resident {
                continue;
            }
            // `FormatSpec::validate` refuses a non-resident block without
            // `block_offset_chain`, so this cannot be the refusal `block_chain`
            // raises for a format that has no chain; it is propagated rather
            // than asserted away.
            for entry in self.block_chain(descriptor.block_id)? {
                let entry = entry?;
                entry.read_payload_snapshot_into(
                    self.spec.with_strict_read_validation(),
                    &self.snapshot,
                    &mut payload,
                )?;
                verified += 1;
            }
        }
        Ok(verified)
    }

    /// A snapshot of every index entry, owned by the caller.
    ///
    /// **Owned rather than borrowed, and that is the point.** Returning
    /// `&[RecordIndexEntry]` required the entries to live contiguously inside
    /// this handle for as long as any caller held the borrow — which is exactly
    /// the requirement that keeps the whole index resident and makes a
    /// demand-filled store impossible. An owned snapshot only has to exist
    /// while its holder does.
    ///
    /// `IndexEntries` derefs to `[RecordIndexEntry]`, so `len()`, indexing,
    /// `iter()` and passing it where a slice is expected all read the same as
    /// before. What changed is who owns the bytes.
    ///
    /// **This allocates.** [`index_entries_into`](Self::index_entries_into)
    /// takes the caller's buffer instead and is the form to use in a loop: the
    /// storage is the caller's business, and a library that decides it for them
    /// allocates once per call for no reason.
    /// How many records the resident directory holds — the positions
    /// [`Self::mmap_payload_range`](VarlitFile::mmap_payload_range) and the
    /// index views address. Copies nothing.
    ///
    /// A lazily opened handle keeps no directory and is refused by name.
    pub fn record_count(&self) -> Result<usize> {
        Ok(self.resident_directory("record_count")?.len())
    }

    pub fn index_entries(&self) -> IndexEntries {
        let mut entries = Vec::new();
        // The infallible surface is kept because 100-odd call sites read it as
        // a slice; a caller that wants the refusal calls `_into`.
        let _ = self.index_entries_into(&mut entries);
        IndexEntries { entries }
    }

    /// Fills the caller's buffer with the record index.
    ///
    /// `out` is cleared and then extended. Sufficient capacity is reused;
    /// a larger index may grow the buffer. This is not a fixed-capacity API.
    ///
    /// The copy is charged against `ReadLimitKey::IndexBytes`, like the index
    /// it copies.
    pub fn index_entries_into(&self, out: &mut Vec<RecordIndexEntry>) -> Result<()> {
        self.index_entries_into_in(self.resident_directory("index_entries")?, out)
    }

    /// Visits the retained directory without copying it into an owned result.
    pub(crate) fn record_entries(
        &self,
    ) -> Result<impl Iterator<Item = Result<RecordIndexEntry>> + '_> {
        Ok(self.resident_directory("record_entries")?.iter())
    }

    fn index_entries_into_in<D: RecordDirectory + ?Sized>(
        &self,
        dir: &D,
        out: &mut Vec<RecordIndexEntry>,
    ) -> Result<()> {
        out.clear();
        let count = dir.record_count();
        let requested = index_bytes_for_count(count)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::IndexBytes, requested)?;
        out.try_reserve(count)
            .map_err(|_| Error::AllocationFailed {
                resource: "index entry snapshot",
                requested,
            })?;
        for entry in directory_records(dir) {
            out.push(entry?);
        }
        Ok(())
    }

    /// Builds the per-key tail offsets for `T` from the resident index.
    ///
    /// every entry this admits is charged to
    /// [`crate::ReadLimits::max_keyed_tail_bytes`] *before* the memory is taken, so
    /// the configured ceiling bounds the **peak** of the build and not merely
    /// what is retained afterwards.
    ///
    /// This is the entry point `VarlitFile::keyed_tail_map` uses to seed the
    /// resident keyed-tail cache - the cache the generic keyed API maintains
    /// and the one the generated keyed writers prime at construction through
    /// [`VarlitFile::prime_keyed_tails`] . Before
    /// this charge existed, both were guarded by `try_reserve` alone: a file
    /// with `N` distinct keys forced an `N`-entry resident map whatever the
    /// configured ceiling said, and `max_keyed_tail_bytes` - including
    /// `UNTRUSTED`'s 256 MiB - only refused *further* growth within the
    /// session.
    ///
    /// The charged value is the structural peak of this function, which is
    /// larger than the map it returns: the build keeps a transient
    /// `(MergeOrder, u64)` per key so it can resolve the winning record, and
    /// that map is still alive when the returned map is reserved. The charge
    /// is therefore `n * size_of::<(T::Key, (MergeOrder, u64))>()` while the
    /// transient map grows to `n`, and the sum of both maps' inline storage at
    /// the final reservation. Opening a file consequently charges more than
    /// the steady-state map costs; the charge is a conservative model of what
    /// is actually allocated, not of what survives.
    ///
    /// This build can only account for inline storage, because the map it
    /// returns holds `T::Key` rather than a canonical byte payload; heap owned
    /// by a `T::Key` is not charged here. The *retained* cache this seeds is
    /// byte-keyed, and its charge does include the key payload heap it owns
    /// (see `KeyedTailMap::charge_for`).
    /// **This allocates.**
    /// [`key_tail_offsets_into`](Self::key_tail_offsets_into) takes the
    /// caller's map instead.
    pub fn key_tail_offsets<T>(&self) -> Result<HashMap<T::Key, u64>>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        let mut offsets = HashMap::new();
        self.key_tail_offsets_into::<T>(&mut offsets)?;
        Ok(offsets)
    }

    /// Fills the caller's map with the newest record offset per key.
    ///
    /// `out` is cleared and then filled. As with
    /// [`materialized_keyed_blocks_into`](Self::materialized_keyed_blocks_into),
    /// the transient ordering map stays this crate's — it carries a
    /// `MergeOrder` the caller has no use for — so this removes the second map
    /// of `distinct keys`, not the first. The `KeyedTailBytes` charge below
    /// still accounts for both being alive at once, unchanged.
    /// # Without a resident directory
    ///
    /// A lazy handle keeps none, so this used to be one of the entry points it
    /// simply could not serve — and because a generated writer primes one keyed
    /// tail map per keyed block at construction, that refusal arrived before
    /// the caller had done anything at all.
    ///
    /// The chains hold the answer. `prev_same_block_offset` links each record
    /// to the previous record of its block, so walking one block's chain
    /// newest-first reaches every record of that block, reading nothing else in
    /// the file. `keyed_chain_directory` collects those records and the
    /// tombstones into file order and hands them to the same builder the
    /// resident path uses, so the two produce the same map by construction
    /// rather than by two implementations agreeing.
    pub fn key_tail_offsets_into<T>(&self, out: &mut HashMap<T::Key, u64>) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        if let Some(dir) = self.index.is_retained().then_some(&self.index) {
            return self.key_tail_offsets_into_in::<T, _>(dir, out);
        }
        let chained = self.keyed_chain_directory(T::ID, "key_tail_offsets")?;
        self.key_tail_offsets_into_in::<T, _>(&chained, out)
    }

    /// The records that can move `block_id`'s keyed tails, in file order.
    ///
    /// Two chains, because two block ids move a keyed tail: the block itself,
    /// and [`TOMBSTONE_BLOCK_ID`], whose records may name any keyed block's
    /// key. The tombstone chain is shared by every keyed block, so it is not
    /// filtered here — the builder decodes each one against `T` exactly as it
    /// does on the resident path.
    ///
    /// # Why file order matters
    ///
    /// The builder breaks sequence ties with the record's *position in the
    /// directory* (`MergeOrder::for_record`). A chain yields records
    /// newest-first with no position at all, so the sort is what makes the two
    /// paths agree: records are appended at strictly increasing offsets and
    /// directory position is append order, so sorting by offset reconstructs
    /// exactly the ordinals the resident path would have used.
    ///
    /// # What this is bounded by
    ///
    /// The records of one keyed block plus the tombstones — not the file. That
    /// is the whole point, and it is why this may be materialised at all: the
    /// keyed blocks a format declares are the small ones by design, and
    /// [`BlockChain`] already charges its walk against `ReadLimitKey::Records`.
    fn keyed_chain_directory(
        &self,
        block_id: u32,
        operation: &'static str,
    ) -> Result<Vec<RecordIndexEntry>> {
        if !self.spec.index_policy.block_offset_chain {
            // Without the block chain there is no second route to these
            // records, so the refusal the caller would have had stands.
            return Err(Error::NoResidentDirectory { operation });
        }
        let mut records = Vec::new();
        for id in [block_id, TOMBSTONE_BLOCK_ID] {
            for entry in self.block_chain(id)? {
                let entry = entry?;
                let requested = index_bytes_for_count(records.len().saturating_add(1))?;
                self.spec
                    .read_limits
                    .check(ReadLimitKey::IndexBytes, requested)?;
                records
                    .try_reserve(1)
                    .map_err(|_| Error::AllocationFailed {
                        resource: "keyed chain directory",
                        requested,
                    })?;
                records.push(entry);
            }
        }
        records.sort_unstable_by_key(|entry| entry.record_offset);
        Ok(records)
    }

    fn key_tail_offsets_into_in<T, D: RecordDirectory + ?Sized>(
        &self,
        dir: &D,
        out: &mut HashMap<T::Key, u64>,
    ) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        // compile-time keyedness contract (also covers the
        // VarlitReader/VarlitWriter key_tail_offsets wrappers); registration is
        // the runtime backstop.
        let () = crate::traits::KeyedBlockContract::<T>::OK;
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let mut tails: HashMap<T::Key, (MergeOrder, u64)> = HashMap::new();
        let mut budget = MaterializationBudget::new(self.spec);
        // One payload buffer for the whole walk, not one per record.
        let mut payload = Vec::new();
        for record_ordinal in 0..dir.record_count() {
            // Only two block ids can move `T`'s tail: `T` itself, and a
            // tombstone, which may name one of `T`'s keys. When the directory
            // can say which a record is without building the entry, every
            // other record is skipped here — before `record_at`, which for the
            // resident index is a positional read of that record's header.
            //
            // This is what makes a generated writer's construction affordable.
            // It primes one map per keyed block, so a format with four of them
            // read every record header four times to keep four small maps. The
            // walk is still O(records); what it no longer does is read them.
            //
            // `record_ordinal` stays the position in the directory rather than
            // a count of what survived the filter, because `MergeOrder` uses it
            // to break ties between records that share a sequence.
            if directory_skips(dir, record_ordinal, &[T::ID, TOMBSTONE_BLOCK_ID]) {
                continue;
            }
            let entry = dir.record_at(record_ordinal)?;
            // One record's materialization at a time (see
            // `MaterializationBudget`). Every decoded block is dropped once its
            // key is taken, so the peak here is one payload however many
            // records the block has; what the build retains is keys and
            // offsets, and those are charged to `max_keyed_tail_bytes` below.
            //
            // This is on the *write* path — `push_keyed` seeds the tail cache
            // from here — so a budget that drained across the walk could refuse
            // the first append to an undamaged file whose records for `T` total
            // more than `max_materialized_bytes` (`STANDARD`: 1 GiB).
            budget.reset();
            let order = MergeOrder::for_record(0, entry.sequence, record_ordinal);
            match entry.block_id {
                id if id == T::ID => {
                    let logical_len =
                        entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
                    budget.consume(logical_len)?;
                    entry.read_logical_payload_snapshot_into(
                        self.spec,
                        &self.snapshot,
                        &mut payload,
                    )?;
                    let block: T =
                        budget.decode(&payload, T::ENDIAN.unwrap_or(self.spec.endian))?;
                    let key = block.key();
                    if tails.get(&key).is_none_or(|(old, _)| order >= *old) {
                        let requested = allocation_bytes::<(T::Key, (MergeOrder, u64))>(
                            tails.len().saturating_add(1),
                            "key tail offsets",
                        )?;
                        // charge the growth before it is taken, so a
                        // file whose distinct key count alone exceeds the
                        // configured ceiling is refused during the build
                        // rather than after the whole map is resident.
                        self.spec
                            .read_limits
                            .check(ReadLimitKey::KeyedTailBytes, requested)?;
                        tails.try_reserve(1).map_err(|_| Error::AllocationFailed {
                            resource: "key tail offsets",
                            requested,
                        })?;
                        tails.insert(key, (order, entry.record_offset));
                    }
                }
                TOMBSTONE_BLOCK_ID => {
                    let logical_len =
                        entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
                    budget.consume(logical_len)?;
                    entry.read_payload_snapshot_into(self.spec, &self.snapshot, &mut payload)?;
                    if let Some(key) =
                        decode_internal_key_payload::<T>(self.spec.endian, &payload, &mut budget)?
                        && tails.get(&key).is_none_or(|(old, _)| order >= *old)
                    {
                        let requested = allocation_bytes::<(T::Key, (MergeOrder, u64))>(
                            tails.len().saturating_add(1),
                            "key tail offsets",
                        )?;
                        // charge the growth before it is taken, so a
                        // file whose distinct key count alone exceeds the
                        // configured ceiling is refused during the build
                        // rather than after the whole map is resident.
                        self.spec
                            .read_limits
                            .check(ReadLimitKey::KeyedTailBytes, requested)?;
                        tails.try_reserve(1).map_err(|_| Error::AllocationFailed {
                            resource: "key tail offsets",
                            requested,
                        })?;
                        tails.insert(key, (order, entry.record_offset));
                    }
                }
                _ => {}
            }
        }
        let requested = allocation_bytes::<(T::Key, u64)>(tails.len(), "key tail offsets")?;
        // the transient ordering map is still alive while the
        // returned map is reserved, so the peak charged here is the sum of
        // both.
        let peak =
            allocation_bytes::<(T::Key, (MergeOrder, u64))>(tails.len(), "key tail offsets")?
                .checked_add(requested)
                .ok_or(Error::AllocationFailed {
                    resource: "key tail offsets",
                    requested: u64::MAX,
                })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::KeyedTailBytes, peak)?;
        out.clear();
        out.try_reserve(tails.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "key tail offsets",
                requested,
            })?;
        out.extend(tails.into_iter().map(|(key, (_, offset))| (key, offset)));
        Ok(())
    }
}
