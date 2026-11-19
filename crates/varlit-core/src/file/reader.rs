// `VarlitReader`, the read-only handle: a `VarlitFile` opened for reading,
// with private caches.
// Included by `file.rs`, whose module it shares.

/// A reader with private caches: Send + !Sync. Move ownership to a worker
/// or open a separate reader for each thread; ordinary reads take `&self`.
#[derive(Debug)]
pub struct VarlitReader {
    file: VarlitFile,
}

impl VarlitReader {
    pub fn matrix_generation(&self) -> Option<u64> {
        self.file.matrix_generation()
    }
    /// The file holding the matrix: the primary file itself, since tiled
    /// storage keeps no companion. `None` when the format has no matrix blocks.
    pub fn matrix_generation_path(&self) -> Option<&Path> {
        self.file.matrix_generation_path()
    }

    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_readonly(spec, path)?,
        })
    }

    /// See [`VarlitFile::follow`].
    ///
    /// The one `&mut self` method on this type, and deliberately so: every read
    /// takes `&self` and stays available to concurrent callers, and advancing
    /// the handle to a later state is the one operation that is not a read.
    pub fn follow(&mut self) -> Result<u64> {
        self.file.follow()
    }

    /// See [`VarlitFile::is_current`].
    pub fn is_current(&self) -> Result<bool> {
        self.file.is_current()
    }

    /// A fresh reader on whatever the pathname resolves to now. See
    /// [`VarlitFile::reopen_readonly`].
    ///
    /// Takes `&self` and returns a separate reader with its own caches.
    pub fn reopen(&self) -> Result<Self> {
        Ok(Self {
            file: self.file.reopen_readonly()?,
        })
    }

    /// See [`VarlitFile::open_readonly_with_scratch`].
    pub fn open_with_scratch<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        scratch: &mut Vec<RecordIndexEntry>,
    ) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_readonly_with_scratch(spec, path, scratch)?,
        })
    }

    /// See [`VarlitFile::open_readonly_without_directory`].
    pub fn open_without_directory<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        index: &mut Vec<RecordIndexEntry>,
    ) -> Result<Self> {
        Ok(Self {
            file: VarlitFile::open_readonly_without_directory(spec, path, index)?,
        })
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

    /// The open file this reader holds, borrowed rather than consumed.
    pub fn file(&self) -> &VarlitFile {
        &self.file
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
    /// `&self` and no I/O: the region is part of the header this handle already
    /// read at open, which is what makes it usable as a seed for a walk.
    pub fn read_header_block<T: VarlitBlock>(&self, out: &mut T) -> Result<bool> {
        self.file.read_header_block::<T>(out)
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

    pub fn mode(&self) -> OpenMode {
        self.file.mode()
    }

    pub fn metadata(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.file.metadata(key)
    }

    pub fn all_metadata(&self) -> Result<Vec<(String, Vec<u8>)>> {
        self.file.all_metadata()
    }

    /// See [`VarlitFile::all_metadata_into`].
    pub fn all_metadata_into(&self, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
        self.file.all_metadata_into(out)
    }

    pub fn schema_manifest(&self) -> Result<Option<SchemaManifest>> {
        self.file.schema_manifest()
    }

    /// See [`VarlitFile::blocks`].
    pub fn blocks<T: VarlitBlock>(&self, payload: &mut Vec<u8>, out: &mut [T]) -> Result<Page> {
        self.file.blocks::<T>(payload, out)
    }

    /// See [`VarlitFile::blocks_from`].
    pub fn blocks_from<T: VarlitBlock>(
        &self,
        from: PageCursor,
        payload: &mut Vec<u8>,
        out: &mut [T],
    ) -> Result<Page> {
        self.file.blocks_from::<T>(from, payload, out)
    }

    /// See [`VarlitFile::block_entries_into`].
    pub fn block_entries_into<T: VarlitBlock>(
        &self,
        out: &mut Vec<RecordIndexEntry>,
    ) -> Result<()> {
        self.file.block_entries_into::<T>(out)
    }

    /// See [`VarlitFile::decode_blocks_into`] — including why it is not the
    /// `_into` twin of [`blocks`](Self::blocks).
    pub fn decode_blocks_into<T: VarlitBlock>(&self, out: &mut Vec<T>) -> Result<()> {
        self.file.decode_blocks_into::<T>(out)
    }

    pub fn blocks_migrated<From, To, M>(&self) -> Result<Vec<To>>
    where
        From: VarlitBlock,
        To: VarlitBlock,
        M: VarlitMigration<From, To>,
    {
        self.file.blocks_migrated::<From, To, M>()
    }

    /// See [`VarlitFile::blocks_migrated_into`].
    pub fn blocks_migrated_into<From, To, M>(&self, out: &mut Vec<To>) -> Result<()>
    where
        From: VarlitBlock,
        To: VarlitBlock,
        M: VarlitMigration<From, To>,
    {
        self.file.blocks_migrated_into::<From, To, M>(out)
    }

    pub fn keyed_blocks<T>(&self) -> Result<KeyedBlockVec<T::Key, T>>
    where
        T: VarlitKeyedBlock,
    {
        self.file.keyed_blocks::<T>()
    }

    /// See [`VarlitFile::keyed_blocks_into`], including which of the three
    /// collections it does not hand over.
    pub fn keyed_blocks_into<T>(
        &self,
        entries: &mut Vec<RecordIndexEntry>,
        by_key: &mut HashMap<T::Key, RecordIndexEntry>,
    ) -> Result<()>
    where
        T: VarlitKeyedBlock,
    {
        self.file.keyed_blocks_into::<T>(entries, by_key)
    }

    pub fn materialized_keyed_blocks<T>(&self) -> Result<HashMap<T::Key, T>>
    where
        T: VarlitMerge,
        T::Key: Eq + Hash,
    {
        self.file.materialized_keyed_blocks::<T>()
    }

    /// See [`VarlitFile::materialized_keyed_blocks_into`].
    pub fn materialized_keyed_blocks_into<T>(&self, out: &mut HashMap<T::Key, T>) -> Result<()>
    where
        T: VarlitMerge,
        T::Key: Eq + Hash,
    {
        self.file.materialized_keyed_blocks_into::<T>(out)
    }

    pub fn scan(&self) -> impl Iterator<Item = Result<BlockEvent>> + '_ {
        self.file.scan()
    }

    /// Verifies every record's stored checksum. See [`VarlitFile::verify_all`].
    pub fn verify_all(&self) -> Result<usize> {
        self.file.verify_all()
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

    pub fn matrix_cell_status<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        self.file.matrix_cell_status::<T>(key)
    }

    pub fn is_matrix_single_committed(&self, name: &str) -> Result<bool> {
        self.file.is_matrix_single_committed(name)
    }

    pub fn is_matrix_channel_committed(&self, name: &str, channel: u64) -> Result<bool> {
        self.file.is_matrix_channel_committed(name, channel)
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

    #[cfg(feature = "mmap")]
    /// Maps the file's indexed record payloads as a read-only snapshot.
    ///
    /// # Safety
    ///
    /// The caller must prevent mutation, truncation, replacement, backing-file
    /// invalidation, and any other modification through every handle, thread,
    /// and process for the full lifetime of the returned mapping.
    ///
    /// Once that condition holds, Varlit keeps safe accessors sound by cloning
    /// the existing backing handle, creating a read-only mapping, validating
    /// every indexed extent with checked arithmetic, rejecting entries outside
    /// the copied snapshot, and tying every returned slice to the mapping owner.
    /// Use owned reads instead when external immutability cannot be guaranteed.
    ///
    /// **The file header is not mapped.** The mapping begins at the start of
    /// the append log, so the bytes a commit can rewrite — the
    /// `IndexPolicy::header_tails` region among them — are outside every
    /// reference this type constructs. Current growing chunks append immutable
    /// record versions rather than rewriting an existing chunk record. This
    /// does not waive the safety condition: exclusive in-place replacement,
    /// liveness updates, truncation and external writes can still change the
    /// mapped backing object. Use owned reads when those operations cannot be
    /// excluded for the mapping's lifetime.
    #[allow(
        unsafe_code,
        reason = "an `unsafe fn` the caller accepts the mapping or exclusivity contract of"
    )]
    pub unsafe fn mmap_payloads(&self) -> Result<MmapPayloads> {
        // SAFETY: The caller accepts the complete file-backed mapping contract.
        unsafe { self.file.mmap_payloads() }
    }

    /// How many records the resident directory holds; see
    /// [`VarlitFile::record_count`].
    pub fn record_count(&self) -> Result<usize> {
        self.file.record_count()
    }

    #[cfg(feature = "mmap")]
    /// Maps only the records at resident positions `records`; see
    /// [`VarlitFile::mmap_payload_range`].
    ///
    /// # Safety
    ///
    /// The same contract as [`Self::mmap_payloads`], for the mapped records.
    #[allow(
        unsafe_code,
        reason = "an `unsafe fn` the caller accepts the mapping or exclusivity contract of"
    )]
    pub unsafe fn mmap_payload_range(
        &self,
        records: std::ops::Range<usize>,
    ) -> Result<MmapPayloads> {
        // SAFETY: The caller accepts the file-backed mapping contract for the
        // mapped records.
        unsafe { self.file.mmap_payload_range(records) }
    }

    #[cfg(feature = "mmap")]
    /// Maps one committed cell; see [`VarlitFile::mmap_matrix_cell`].
    ///
    /// # Safety
    ///
    /// The backing object must remain immutable and valid for the mapping's lifetime.
    #[allow(
        unsafe_code,
        reason = "an `unsafe fn` the caller accepts the mapping or exclusivity contract of"
    )]
    pub unsafe fn mmap_matrix_cell<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MmapPayloadWindow> {
        // SAFETY: the caller upholds the underlying constructor's contract.
        unsafe { self.file.mmap_matrix_cell::<T>(key) }
    }

    #[cfg(feature = "mmap")]
    /// Maps the file's matrix regions as a read-only snapshot.
    ///
    /// # Safety
    ///
    /// The caller must prevent mutation, truncation, replacement, backing-file
    /// invalidation, and any other modification through every handle, thread,
    /// and process for the full lifetime of the returned mapping.
    ///
    /// Once that condition holds, Varlit keeps safe accessors sound by cloning
    /// the existing backing handle, creating a read-only mapping, validating
    /// the complete matrix extent, checking every requested slot, and tying
    /// every returned slice to the mapping owner. Use owned reads instead when
    /// external immutability cannot be guaranteed.
    ///
    /// Unlike [`Self::mmap_payloads`] this mapping *does* start at byte 0 and so
    /// covers the file header: the matrix region it exists to serve sits between
    /// the header and the append log, so there is no base to move it to. That is
    /// sound because the one option that rewrites header bytes,
    /// `IndexPolicy::header_tails`, is refused for a format declaring matrix
    /// blocks — the two cannot meet.
    #[allow(
        unsafe_code,
        reason = "an `unsafe fn` the caller accepts the mapping or exclusivity contract of"
    )]
    pub unsafe fn mmap_matrix(&self) -> Result<MmapMatrix> {
        // SAFETY: The caller accepts the complete file-backed mapping contract.
        unsafe { self.file.mmap_matrix() }
    }
}
