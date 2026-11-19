// `VarlitWriter`, the writable handle over a `VarlitFile`.
// Included by `file.rs`, whose module it shares.

#[derive(Debug)]
pub struct VarlitWriter {
    file: VarlitFile,
}

impl VarlitWriter {
    pub fn matrix_generation(&self) -> Option<u64> {
        self.file.matrix_generation()
    }
    /// The file holding the matrix: the primary file itself, since tiled
    /// storage keeps no companion. `None` when the format has no matrix blocks.
    pub fn matrix_generation_path(&self) -> Option<&Path> {
        self.file.matrix_generation_path()
    }

    pub fn create<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::create(spec, path)?,
        })
    }

    /// Creates a new native file, failing if `path` already exists.
    ///
    /// See [`VarlitFile::create_new`].
    pub fn create_new<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::create_new(spec, path)?,
        })
    }

    pub fn create_with_dims<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::create_with_dims(spec, path, dims)?,
        })
    }

    /// Creates a new native matrix file, failing if `path` already exists.
    ///
    /// See [`VarlitFile::create_new_with_dims`].
    pub fn create_new_with_dims<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::create_new_with_dims(spec, path, dims)?,
        })
    }

    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open(spec, path)?,
        })
    }

    /// See [`VarlitFile::open_lazy`].
    pub fn open_lazy<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_lazy(spec, path)?,
        })
    }

    /// See [`VarlitFile::open_scan`].
    pub fn open_scan<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_scan(spec, path)?,
        })
    }

    /// See [`VarlitFile::open_lazy_with_report`].
    pub fn open_lazy_with_report<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
    ) -> Result<(Self, LazyOpenSource)> {
        let (file, source) = VarlitFile::open_lazy_with_report(spec, path)?;
        Ok((Self { file }, source))
    }

    /// See [`VarlitFile::open_lazy_or_scan`].
    pub fn open_lazy_or_scan<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_lazy_or_scan(spec, path)?,
        })
    }

    /// See [`VarlitFile::open_lazy_or_scan_with_report`].
    pub fn open_lazy_or_scan_with_report<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
    ) -> Result<(Self, LazyOpenSource)> {
        let (file, source) = VarlitFile::open_lazy_or_scan_with_report(spec, path)?;
        Ok((Self { file }, source))
    }

    /// See [`VarlitFile::open_with_scratch`].
    pub fn open_with_scratch<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        scratch: &mut Vec<RecordIndexEntry>,
    ) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_with_scratch(spec, path, scratch)?,
        })
    }

    pub fn open_with_lock_policy<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        policy: WriterLockBreakPolicy,
    ) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_with_lock_policy(spec, path, policy)?,
        })
    }

    pub fn open_recover<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_recover(spec, path)?,
        })
    }

    pub fn open_recover_with_report<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
    ) -> Result<(Self, RecoveryReport)> {
        let (file, report) = VarlitFile::open_recover_with_report(spec, path)?;
        Ok((Self { file }, report))
    }

    pub fn into_inner(self) -> VarlitFile {
        self.file
    }

    pub fn spec(&self) -> FormatSpec {
        self.file.spec()
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn mode(&self) -> OpenMode {
        self.file.mode()
    }

    /// Verifies every record's stored checksum. See [`VarlitFile::verify_all`].
    pub fn verify_all(&self) -> Result<usize> {
        self.file.verify_all()
    }

    /// The open file this writer holds, borrowed rather than consumed.
    ///
    /// `into_inner` is the only other route to it and it takes `self`, which
    /// makes every `&mut self` entry point on [`VarlitFile`] unreachable from a
    /// writer that is still being written through. Reaching one is what a
    /// caller wants when the capability has no forwarding method here yet;
    /// the header-slot family below is forwarded precisely so that this is not
    /// needed for it.
    pub fn file_mut(&mut self) -> &mut VarlitFile {
        &mut self.file
    }

    /// The open file this writer holds, borrowed for reading.
    pub fn file(&self) -> &VarlitFile {
        &self.file
    }

    /// See [`VarlitFile::put_keyed`].
    pub fn put_keyed<T: VarlitKeyedBlock>(&mut self, block: &T) -> Result<()> {
        self.file.put_keyed(block)
    }

    /// See [`VarlitFile::remove_keyed`].
    pub fn remove_keyed<T: VarlitKeyedBlock>(&mut self, key: &T::Key) -> Result<bool> {
        self.file.remove_keyed::<T>(key)
    }

    /// See [`VarlitFile::get_keyed`].
    pub fn get_keyed<T: VarlitKeyedBlock>(
        &self,
        key: &T::Key,
        scratch: &mut Vec<u8>,
        out: &mut T,
    ) -> Result<bool> {
        self.file.get_keyed(key, scratch, out)
    }

    /// See [`VarlitFile::range_keyed`].
    pub fn range_keyed<T, R>(
        &self,
        range: R,
        scratch: &mut Vec<u8>,
        out: &mut T,
        visit: &mut dyn FnMut(&T) -> Result<bool>,
    ) -> Result<u64>
    where
        T: VarlitKeyedBlock,
        R: core::ops::RangeBounds<T::Key>,
    {
        self.file.range_keyed(range, scratch, out, visit)
    }

    /// Reads a block out of the editable header region.
    /// See [`VarlitFile::read_header_block`].
    ///
    /// Forwarded because the writer is the handle that knows what to record
    /// there: an append returns its own `record_offset`, and writing that
    /// offset into the region is what lets a later reader seed a walk without
    /// rebuilding an index. Reaching `write_header_block` used to mean
    /// `into_inner`, which ends the writer.
    pub fn read_header_block<T: VarlitBlock>(&self, out: &mut T) -> Result<bool> {
        self.file.read_header_block::<T>(out)
    }

    /// Writes a block into the editable header region, replacing any earlier
    /// value for the same block id. See [`VarlitFile::write_header_block`].
    pub fn write_header_block<T: VarlitBlock>(&mut self, block: &T) -> Result<()> {
        self.file.write_header_block(block)
    }

    /// Removes a block from the editable header region, freeing its bytes.
    /// See [`VarlitFile::remove_header_block`].
    pub fn remove_header_block<T: VarlitBlock>(&mut self) -> Result<()> {
        self.file.remove_header_block::<T>()
    }

    /// Fixes the region's checksum and refuses every later write.
    /// See [`VarlitFile::seal_header_slots`].
    pub fn seal_header_slots(&mut self) -> Result<()> {
        self.file.seal_header_slots()
    }

    /// The declared size of the editable header region; `0` when undeclared.
    pub fn header_slots_capacity(&self) -> usize {
        self.file.header_slots_capacity()
    }

    /// How much of the region the current blocks occupy.
    pub fn header_slots_used(&self) -> Result<usize> {
        self.file.header_slots_used()
    }

    /// How much of the region is left.
    pub fn header_slots_free_bytes(&self) -> Result<usize> {
        self.file.header_slots_free_bytes()
    }

    /// Whether the region's seal is down.
    pub fn header_slots_sealed(&self) -> Result<bool> {
        self.file.header_slots_sealed()
    }

    /// See [`VarlitFile::index_entries`].
    pub fn index_entries(&self, out: &mut [RecordIndexEntry]) -> Result<Page> {
        self.file.index_entries(out)
    }

    /// See [`VarlitFile::index_entries_from`].
    pub fn index_entries_from(
        &self,
        from: PageCursor,
        out: &mut [RecordIndexEntry],
    ) -> Result<Page> {
        self.file.index_entries_from(from, out)
    }

    /// Fills the caller's buffer with the record index; see
    /// [`VarlitFile::index_entries_into`].
    pub fn index_entries_into(&self, out: &mut Vec<RecordIndexEntry>) -> Result<()> {
        self.file.index_entries_into(out)
    }

    /// See [`VarlitFile::with_directory`].
    pub fn with_directory<'a, D: RecordDirectory + ?Sized>(
        &'a self,
        directory: &'a D,
    ) -> Result<DirectoryRead<'a, D>> {
        self.file.with_directory(directory)
    }

    /// See [`VarlitFile::record_map`].
    pub fn record_map<'a>(&self, buffer: &'a mut Vec<RecordIndexEntry>) -> Result<RecordMap<'a>> {
        self.file.record_map(buffer)
    }

    /// See [`VarlitFile::record_map_from`].
    pub fn record_map_from<'a>(
        &self,
        buffer: &'a mut Vec<RecordIndexEntry>,
        offset: u64,
    ) -> Result<RecordMap<'a>> {
        self.file.record_map_from(buffer, offset)
    }

    /// See [`VarlitFile::anchor_layout`].
    pub fn anchor_layout(&self) -> Option<crate::AnchorLayout> {
        self.file.anchor_layout()
    }

    /// See [`VarlitFile::anchor`].
    pub fn anchor(&self, k: u64) -> Result<Option<crate::Anchor>> {
        self.file.anchor(k)
    }

    /// See [`VarlitFile::seek_sequence`].
    pub fn seek_sequence(&self, sequence: u64) -> Result<u64> {
        self.file.seek_sequence(sequence)
    }

    /// See [`VarlitFile::read_payload_into`].
    pub fn read_payload_into(&self, entry: &RecordIndexEntry, out: &mut Vec<u8>) -> Result<()> {
        self.file.read_payload_into(entry, out)
    }

    /// See [`VarlitFile::read_logical_payload_into`].
    pub fn read_logical_payload_into(
        &self,
        entry: &RecordIndexEntry,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        self.file.read_logical_payload_into(entry, out)
    }

    /// See [`VarlitFile::read_record_into`].
    pub fn read_record_into(&self, entry: &RecordIndexEntry, out: &mut [u8]) -> Result<RecordPage> {
        self.file.read_record_into(entry, out)
    }

    /// See [`VarlitFile::read_record_from`].
    pub fn read_record_from(&self, cursor: RecordCursor, out: &mut [u8]) -> Result<RecordPage> {
        self.file.read_record_from(cursor, out)
    }

    /// See [`VarlitFile::view_block`].
    pub fn view_block<T: VarlitView>(&self, entry: &RecordIndexEntry) -> Result<T::View> {
        self.file.view_block::<T>(entry)
    }

    /// See [`VarlitFile::read_span_into`].
    pub fn read_span_into(
        &self,
        entry: &RecordIndexEntry,
        span: FieldSpan,
        out: &mut [u8],
    ) -> Result<RecordPage> {
        self.file.read_span_into(entry, span, out)
    }

    /// See [`VarlitFile::decode_field`].
    pub fn decode_field<F: crate::VarlitDecode>(
        &self,
        entry: &RecordIndexEntry,
        span: FieldSpan,
    ) -> Result<F> {
        self.file.decode_field(entry, span)
    }

    /// See [`VarlitFile::decode_block_into`].
    pub fn decode_block_into<T: VarlitBlock>(
        &self,
        entry: &RecordIndexEntry,
        scratch: &mut Vec<u8>,
        out: &mut T,
    ) -> Result<()> {
        self.file.decode_block_into::<T>(entry, scratch, out)
    }

    pub fn key_tail_offsets<T>(&self) -> Result<HashMap<T::Key, u64>>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.key_tail_offsets::<T>()
    }

    /// See [`VarlitFile::key_tail_offsets_into`].
    pub fn key_tail_offsets_into<T>(&self, out: &mut HashMap<T::Key, u64>) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.key_tail_offsets_into::<T>(out)
    }

    // `VarlitWriter::reserve_keyed_tail_slot` has been
    // removed.
    //
    // It existed so a generated keyed writer could reserve a slot in its own
    // `HashMap<T::Key, u64>` before the append that would fill it. That
    // reserved the map's inline table, but it could not stop the *later*
    // `HashMap::insert` from invoking the caller's `Hash` or `Eq` after the
    // record was already authoritative - a stateful or panicking
    // implementation could therefore fail after publication and leave the
    // writer holding a stale predecessor, which makes the next same-key
    // mutation link *around* the committed event.
    //
    // The generated writers no longer own a typed tail map at all. They route
    // through `VarlitWriter::push_keyed_info` and `VarlitWriter::delete_info`,
    // which maintain the same byte-keyed resident cache the generic keyed API
    // uses (`VarlitFile::push_keyed_info`): its keys are the canonical
    // internal key payload, so the only step after the authoritative append is
    // a `HashMap<Vec<u8>, u64>` insert into a slot reserved beforehand. No
    // user-defined trait method can run there.
    //
    pub fn push<T: VarlitBlock>(&mut self, block: &T) -> Result<u64> {
        self.file.push(block)
    }

    pub fn push_info<T: VarlitBlock>(&mut self, block: &T) -> Result<AppendInfo> {
        self.file.push_info(block)
    }

    /// Appends a keyed block through the generic API while maintaining the
    /// keyed predecessor chain. See [`VarlitFile::push_keyed`].
    pub fn push_keyed<T>(&mut self, block: &T) -> Result<u64>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.push_keyed(block)
    }

    /// Appends a keyed block through the generic API while maintaining the
    /// keyed predecessor chain. See [`VarlitFile::push_keyed_info`].
    pub fn push_keyed_info<T>(&mut self, block: &T) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.push_keyed_info(block)
    }

    pub fn push_with_prev_key_info<T: VarlitBlock>(
        &mut self,
        block: &T,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo> {
        self.file
            .push_with_prev_key_info(block, prev_same_key_offset)
    }

    pub fn delete<T>(&mut self, key: &T::Key) -> Result<u64>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.delete::<T>(key)
    }

    /// Appends a tombstone through the maintained keyed path. See
    /// [`VarlitFile::delete_info`]. This is the entry point the generated
    /// `delete_<block>` writer methods use.
    pub fn delete_info<T>(&mut self, key: &T::Key) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.delete_info::<T>(key)
    }

    /// Builds `T`'s keyed tail map now. See [`VarlitFile::prime_keyed_tails`].
    /// The generated keyed writers call this once per keyed block at
    /// construction.
    #[doc(hidden)]
    pub fn prime_keyed_tails<T>(&mut self) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.prime_keyed_tails::<T>()
    }

    pub fn delete_with_prev_key_info<T>(
        &mut self,
        key: &T::Key,
        prev_same_key_offset: Option<u64>,
    ) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
    {
        self.file
            .delete_with_prev_key_info::<T>(key, prev_same_key_offset)
    }

    pub fn push_op<T>(&mut self, key: &T::Key, op: &T::Op) -> Result<u64>
    where
        T: VarlitMerge,
    {
        self.file.push_op::<T>(key, op)
    }

    pub fn write_metadata(&mut self, key: &str, value: &[u8]) -> Result<u64> {
        self.file.write_metadata(key, value)
    }

    /// Replaces a native user record while preserving its sequence number.
    ///
    /// [`Error::PublishedButRebindFailed`] means publication already succeeded.
    /// Do not retry blindly; discard this poisoned writer and reopen the path.
    ///
    /// [`Error::PublishedButParentSyncPending`] also means publication already
    /// succeeded; the writer was rebound to the published generation and stays
    /// usable, but the rename is not yet guaranteed durable against power loss
    /// until the parent directory is synced (for example by a later successful
    /// publication or an explicit directory sync).
    pub fn replace_block<T: VarlitReplaceBlock>(
        &mut self,
        index: usize,
        block: &T,
    ) -> Result<ReplacementInfo> {
        self.file.replace_block(index, block)
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
    /// See [`VarlitFile::defragment`].
    pub fn defragment(&mut self) -> Result<DefragmentReport> {
        self.file.defragment()
    }

    /// See [`VarlitFile::mark_record_dead`].
    pub fn mark_record_dead(&mut self, record_offset: u64) -> Result<()> {
        self.file.mark_record_dead(record_offset)
    }

    /// See [`VarlitFile::record_is_dead`].
    pub fn record_is_dead(&self, record_offset: u64) -> Result<bool> {
        self.file.record_is_dead(record_offset)
    }

    /// See [`VarlitFile::opened_after_crash`].
    pub const fn opened_after_crash(&self) -> bool {
        self.file.opened_after_crash()
    }

    pub fn replace_fixed<T: VarlitBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        self.file.replace_fixed(index, block)
    }

    /// Replaces a fixed record by mutating the backing object directly.
    ///
    /// # Errors
    ///
    /// See [`VarlitFile::replace_fixed_in_place_exclusive`]: a stored record
    /// whose version differs from `T::VERSION` is refused with
    /// [`Error::BlockVersionMismatch`] .
    ///
    /// # Safety
    ///
    /// See [`VarlitFile::replace_fixed_in_place_exclusive`] for the full
    /// contract, including the requirement that a keyed record's key must not
    /// change.
    ///
    /// The caller must exclude every reader, writer, mapping, raw reference,
    /// handle, thread, and process for this operation and for the lifetime of
    /// every view that could observe the affected object.
    #[allow(
        unsafe_code,
        reason = "an `unsafe fn` the caller accepts the mapping or exclusivity contract of"
    )]
    pub unsafe fn replace_fixed_in_place_exclusive<T: VarlitBlock>(
        &mut self,
        index: usize,
        block: &T,
    ) -> Result<u64> {
        // SAFETY: The caller accepts the exclusivity contract documented above.
        unsafe { self.file.replace_fixed_in_place_exclusive(index, block) }
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
        self.file.replace_rewrite(index, block)
    }

    /// See [`VarlitFile::replace`].
    pub fn replace<T: VarlitReplaceBlock>(&mut self, index: usize, block: &T) -> Result<u64> {
        self.file.replace(index, block)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()
    }

    pub fn commit(&mut self) -> Result<AppendInfo> {
        self.file.commit()
    }

    pub fn commit_durable(&mut self) -> Result<AppendInfo> {
        self.file.commit_durable()
    }

    pub fn sync(&mut self) -> Result<()> {
        self.file.sync()
    }

    pub fn immediate(&mut self) -> Result<()> {
        self.file.immediate()
    }

    pub fn set_immediate_policy(&mut self, policy: crate::ImmediatePolicy) -> Result<()> {
        self.file.set_immediate_policy(policy)
    }

    pub fn write_matrix_cell<T: VarlitMatrixBlock>(
        &mut self,
        key: MatrixKey,
        value: &T,
    ) -> Result<()> {
        self.file.write_matrix_cell(key, value)
    }

    /// Copies one cell's bytes out of `source`.
    ///
    /// `source` is a **shared** borrow: the read side of a byte copy needs no
    /// exclusive handle, so a copy may run while other threads are reading the
    /// same source handle. Callers that were passing `&mut VarlitReader` still
    /// compile — `&mut T` coerces to `&T`.
    pub fn copy_matrix_cell_bytes_from<From, To>(
        &mut self,
        source: &VarlitReader,
        key: MatrixKey,
    ) -> Result<()>
    where
        From: VarlitMatrixBlock,
        To: VarlitMatrixBlock,
    {
        ensure_matrix_byte_copy_compatible::<From, To>()?;
        let payload = source.matrix_cell_payload::<From>(key)?;
        self.file.write_matrix_cell_payload::<To>(key, &payload)?;
        self.file.commit_matrix_cell::<To>(key)
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
        self.file.write_matrix_cell_durable(key, value, hook)
    }

    /// [`Self::write_matrix_cell_durable`] with a caller-supplied durability
    /// barrier; the same post-commit contract applies, including
    /// [`Error::MatrixCommittedButHookFailed`] and
    /// [`Error::MatrixCommittedButDurabilityUnproven`]. Both barrier callbacks
    /// run before publication and receive the primary file.
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
        self.file
            .write_matrix_cell_durable_with_barrier(key, value, barrier, hook)
    }

    pub fn read_matrix_cell<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
        out: &mut T,
    ) -> Result<()> {
        self.file.read_matrix_cell(key, out)
    }

    /// See [`VarlitFile::read_matrix_run_into`].
    pub fn read_matrix_run_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [T],
    ) -> Result<()> {
        self.file.read_matrix_run_into(start, out)
    }

    /// See [`VarlitFile::read_matrix_run_bytes_into`].
    pub fn read_matrix_run_bytes_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        self.file.read_matrix_run_bytes_into::<T>(start, out)
    }

    /// See [`VarlitFile::read_matrix_region_into`].
    pub fn read_matrix_region_into<T: VarlitMatrixBlock>(
        &self,
        region: MatrixRegion,
        out: &mut [T],
    ) -> Result<MatrixPage> {
        self.file.read_matrix_region_into::<T>(region, out)
    }

    /// See [`VarlitFile::read_matrix_region_from`].
    pub fn read_matrix_region_from<T: VarlitMatrixBlock>(
        &self,
        cursor: MatrixCursor,
        out: &mut [T],
    ) -> Result<MatrixPage> {
        self.file.read_matrix_region_from::<T>(cursor, out)
    }

    /// See [`VarlitFile::read_matrix_region_bytes_into`].
    pub fn read_matrix_region_bytes_into<T: VarlitMatrixBlock>(
        &self,
        region: MatrixRegion,
        out: &mut [u8],
    ) -> Result<MatrixPage> {
        self.file.read_matrix_region_bytes_into::<T>(region, out)
    }

    /// See [`VarlitFile::read_matrix_region_bytes_from`].
    pub fn read_matrix_region_bytes_from<T: VarlitMatrixBlock>(
        &self,
        cursor: MatrixCursor,
        out: &mut [u8],
    ) -> Result<MatrixPage> {
        self.file.read_matrix_region_bytes_from::<T>(cursor, out)
    }

    /// See [`VarlitFile::read_matrix_cells_into`].
    pub fn read_matrix_cells_into<T: VarlitMatrixBlock>(
        &self,
        keys: &[MatrixKey],
        out: &mut [T],
    ) -> Result<()> {
        self.file.read_matrix_cells_into::<T>(keys, out)
    }

    /// See [`VarlitFile::read_matrix_cells_bytes_into`].
    pub fn read_matrix_cells_bytes_into<T: VarlitMatrixBlock>(
        &self,
        keys: &[MatrixKey],
        out: &mut [u8],
    ) -> Result<()> {
        self.file.read_matrix_cells_bytes_into::<T>(keys, out)
    }

    /// See [`VarlitFile::read_matrix_column_into`].
    pub fn read_matrix_column_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [T],
    ) -> Result<()> {
        self.file.read_matrix_column_into(start, out)
    }

    /// See [`VarlitFile::read_matrix_column_bytes_into`].
    pub fn read_matrix_column_bytes_into<T: VarlitMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [u8],
    ) -> Result<()> {
        self.file.read_matrix_column_bytes_into::<T>(start, out)
    }

    /// See [`VarlitFile::read_matrix_row_into`].
    pub fn read_matrix_row_into<T: VarlitMatrixBlock>(
        &self,
        row: u64,
        out: &mut [T],
    ) -> Result<()> {
        self.file.read_matrix_row_into(row, out)
    }

    pub fn matrix_cell_payload<T: VarlitMatrixBlock>(&self, key: MatrixKey) -> Result<Vec<u8>> {
        self.file.matrix_cell_payload::<T>(key)
    }

    pub fn matrix_aux_len(&self, name: &str) -> Result<u64> {
        self.file.matrix_aux_len(name)
    }

    pub fn read_matrix_aux(&self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        self.file.read_matrix_aux(name, offset, len)
    }

    pub fn write_matrix_aux(&mut self, name: &str, offset: u64, payload: &[u8]) -> Result<()> {
        self.file.write_matrix_aux(name, offset, payload)
    }

    pub fn matrix_cell_status<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        self.file.matrix_cell_status::<T>(key)
    }

    /// See [`VarlitFile::write_matrix_run_bytes`].
    pub fn write_matrix_run_bytes<T: VarlitMatrixBlock>(
        &mut self,
        start: MatrixKey,
        payloads: &[u8],
    ) -> Result<()> {
        self.file.write_matrix_run_bytes::<T>(start, payloads)
    }

    pub fn commit_matrix_cell<T: VarlitMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        self.file.commit_matrix_cell::<T>(key)
    }

    pub fn clear_matrix_cell<T: VarlitMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        self.file.clear_matrix_cell::<T>(key)
    }

    pub fn clear_matrix_cell_by_category(&mut self, category: &str, key: MatrixKey) -> Result<()> {
        self.file.clear_matrix_cell_by_category(category, key)
    }

    pub fn clear_matrix_category(&mut self, category: &str) -> Result<u64> {
        self.file.clear_matrix_category(category)
    }

    pub fn apply_matrix_recovery_action(&mut self, action: &MatrixRecoveryAction) -> Result<()> {
        self.file.apply_matrix_recovery_action(action)
    }

    pub fn rebuild_matrix_commit_from_crc<T: VarlitMatrixBlock>(&mut self) -> Result<u64> {
        self.file.rebuild_matrix_commit_from_crc::<T>()
    }

    pub fn is_matrix_single_committed(&self, name: &str) -> Result<bool> {
        self.file.is_matrix_single_committed(name)
    }

    pub fn set_matrix_single_committed(&mut self, name: &str, value: bool) -> Result<()> {
        self.file.set_matrix_single_committed(name, value)
    }

    pub fn is_matrix_channel_committed(&self, name: &str, channel: u64) -> Result<bool> {
        self.file.is_matrix_channel_committed(name, channel)
    }

    pub fn set_matrix_channel_committed(
        &mut self,
        name: &str,
        channel: u64,
        value: bool,
    ) -> Result<()> {
        self.file.set_matrix_channel_committed(name, channel, value)
    }

    pub fn matrix_resume_signal(&self, category: &str) -> Result<MatrixResumeSignal> {
        self.file.matrix_resume_signal(category)
    }

    pub fn matrix_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_sidecar_resume_signal(category, sidecar_path)
    }

    pub fn matrix_verified_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_verified_sidecar_resume_signal(category, sidecar_path)
    }

    pub fn matrix_verified_sidecar_resume_signal_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<MatrixResumeSignal> {
        self.file
            .matrix_verified_sidecar_resume_signal_with_generation(
                category,
                sidecar_path,
                expected_generation,
            )
    }

    pub fn write_matrix_sidecar<P: AsRef<Path>>(
        &mut self,
        category: &str,
        sidecar_path: P,
        generation: u64,
        payload: &[u8],
    ) -> Result<MatrixSidecarManifest> {
        self.file
            .write_matrix_sidecar(category, sidecar_path, generation, payload)
    }

    pub fn read_matrix_sidecar<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        self.file.read_matrix_sidecar(category, sidecar_path)
    }

    pub fn read_matrix_sidecar_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        self.file
            .read_matrix_sidecar_with_generation(category, sidecar_path, expected_generation)
    }

    /// Verifies this matrix's commit metadata now, and reports what was found.
    ///
    /// See [`VarlitFile::verify_matrix_metadata`]: checks every tile header
    /// and commit bitmap. It reports; it does not change what reads do.
    pub fn verify_matrix_metadata(&self) -> Result<MatrixRecoveryReport> {
        self.file.verify_matrix_metadata()
    }
}
