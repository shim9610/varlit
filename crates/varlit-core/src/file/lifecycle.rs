// Native file lifecycle operations.

fn lazy_metadata_error(error: Error, source: LazyOpenSource) -> Error {
    match error {
        Error::InvalidIndexSegment => Error::LazyOpenMetadataUnavailable { metadata: source },
        other => other,
    }
}

fn ensure_lazy_verification_policy(spec: FormatSpec) -> Result<()> {
    if spec.read_limits.read_validation != Some(crate::ReadValidation::TRUSTED)
        && spec
            .read_limits
            .resolve()
            .effective_integrity_verification()
            == IntegrityVerification::AtOpen
    {
        return Err(Error::LazyOpenVerificationRequiresScan);
    }
    Ok(())
}

fn ensure_lazy_writer_policy(spec: FormatSpec) -> Result<()> {
    for (enabled, policy) in [
        (spec.index_policy.checkpoint_on_flush, "checkpoint_on_flush"),
        (spec.index_policy.segment_on_flush, "segment_on_flush"),
    ] {
        if enabled {
            return Err(Error::LazyWriterIndexPolicy { policy });
        }
    }
    Ok(())
}

impl VarlitFile {
    pub fn create<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Self::create_impl(spec, path.as_ref(), false)
    }

    /// Creates a new native file, failing if `path` already exists.
    ///
    /// Unlike [`VarlitFile::create`], this never truncates or reuses an
    /// existing file, so callers that must prove they created and own the
    /// file (for example diagnostic scaffolding) cannot destroy caller data.
    pub fn create_new<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Self::create_impl(spec, path.as_ref(), true)
    }

    fn create_impl(spec: FormatSpec, path: &Path, exclusive: bool) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_write_limits(spec)?;
        if spec.has_matrix_blocks() {
            return Err(Error::MatrixDimensionsRequired);
        }
        let path = path.to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        // Everything past the acquisition runs inside `with_writer_lock`, so a
        // failure gives the claim back before the error propagates instead of at
        // scope exit; see `release_writer_lock_after_failure`.
        with_writer_lock(lock, |lock| {
            Self::create_locked(spec, path, exclusive, lock)
        })
    }

    fn create_locked(
        spec: FormatSpec,
        path: PathBuf,
        exclusive: bool,
        lock: &mut WriterLock,
    ) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        if exclusive {
            options.create_new(true);
        } else {
            options.create(true);
        }
        let mut file = PrimaryFile::new(options.open(&path)?, None);
        // bind the single-writer object lock before any destructive
        // initialization. Opening with `.truncate(true)` would clear the new
        // object inside the pre-bind window where a concurrent creator that
        // lost the race could still hold an unbound handle to it.
        lock.bind_native(file.raw(), &path)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let header_extensions = write_file_header(spec, &mut file)?;
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        let mut file = Self {
            spec,
            path,
            file: RecordFile::new(file),
            index: ResidentIndex::none_retained_created(snapshot.clone(), spec),
            snapshot,
            mode: OpenMode::ReadWrite,
            follow_scan: None,
            header_extensions,
            sequence_state: SequenceState::Available(0),
            block_tails: BlockTails::new_empty(),
            segment_cursor: SegmentCursor::new_empty(),
            uncommitted_since_commit: false,
            commit_scope: CommitScope::FRESH,
            record_buffer: Vec::new(),
            immediate_state: Default::default(),
            appended_since_digest: false,
            last_resident_block_id: None,
            keyed_tails: KeyedTails::new_empty(),
            // this handle established the pathname, so its
            // directory entry is not durable until the first durability
            // request syncs the parent directory.
            tiles: std::cell::OnceCell::new(),
            record_cache: std::cell::RefCell::new(ChunkCache::for_spec(spec)?),
            pending_pathname_parent_sync: true,
            opened_after_crash: false,
            poison: PoisonFlag::healthy(),
            _lock: None,
        };
        file.write_embedded_manifest_if_needed()?;
        file.mark_writer_open()?;
        Ok(file)
    }

    pub fn create_with_dims<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<Self> {
        Self::create_with_dims_impl(spec, path.as_ref(), dims, false)
    }

    /// Creates a new native matrix file, failing if `path` already exists.
    ///
    /// Unlike [`VarlitFile::create_with_dims`], this never reuses or truncates
    /// an existing file object: the exclusive create *is* the ownership claim,
    /// and the same handle is kept locked from that claim through the complete
    /// matrix initialization. Diagnostic scaffolding (the format self-test)
    /// must use this instead of claiming the pathname with a separate handle
    /// and then re-opening it by name.
    pub fn create_new_with_dims<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<Self> {
        Self::create_with_dims_impl(spec, path.as_ref(), dims, true)
    }

    fn create_with_dims_impl(
        spec: FormatSpec,
        path: &Path,
        dims: MatrixDimensions,
        exclusive: bool,
    ) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_write_limits(spec)?;
        if !spec.has_matrix_blocks() {
            return Self::create_impl(spec, path, exclusive);
        }
        // Chunk 0 *is* the matrix region, and `chunk_for_row` divides by
        // `rows_per_chunk` to find it. If the declared extent were any other
        // value the two would disagree: with an extent of 100 and chunks of 4,
        // row 50 routes to chunk 12 while the region already holds it, and the
        // region's rows 4..100 become unreachable. Refused at create, where the
        // dimension value first exists.
        if let Some(growing) = spec.growing_matrix {
            let declared = dims
                .get(growing.name)
                .ok_or_else(|| Error::MatrixDimensionMissing(growing.name.to_string()))?;
            if declared != growing.rows_per_chunk {
                return Err(Error::MatrixSizeMismatch {
                    expected: growing.rows_per_chunk,
                    actual: declared,
                });
            }
        }
        let path = path.to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        // As in `create_impl`: a failure past the acquisition gives the claim
        // back before the error propagates.
        with_writer_lock(lock, |lock| {
            Self::create_with_dims_locked(spec, path, dims, exclusive, lock)
        })
    }

    fn create_with_dims_locked(
        spec: FormatSpec,
        path: PathBuf,
        dims: MatrixDimensions,
        exclusive: bool,
        lock: &mut WriterLock,
    ) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        if exclusive {
            options.create_new(true);
        } else {
            options.create(true);
        }
        let mut file = PrimaryFile::new(options.open(&path)?, None);
        // bind the single-writer object lock before any destructive
        // initialization; see `create_impl`.
        lock.bind_native(file.raw(), &path)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let header_extensions = write_file_header(spec, &mut file)?;
        // stamp a fresh creation nonce so this logical matrix is
        // distinguishable from any earlier matrix that lived in the same OS
        // file object at the same layout offsets.
        // Distinguishes this matrix from any earlier one that lived in the same
        // file object; kept in the matrix's metadata record.
        let creation_nonce = fresh_matrix_creation_nonce();
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        let mut file = Self {
            spec,
            path,
            file: RecordFile::new(file),
            index: ResidentIndex::none_retained_created(snapshot.clone(), spec),
            snapshot,
            mode: OpenMode::ReadWrite,
            follow_scan: None,
            header_extensions,
            sequence_state: SequenceState::Available(0),
            block_tails: BlockTails::new_empty(),
            segment_cursor: SegmentCursor::new_empty(),
            uncommitted_since_commit: false,
            commit_scope: CommitScope::FRESH,
            record_buffer: Vec::new(),
            immediate_state: Default::default(),
            appended_since_digest: false,
            last_resident_block_id: None,
            keyed_tails: KeyedTails::new_empty(),
            // this handle established the pathname, so its
            // directory entry is not durable until the first durability
            // request syncs the parent directory.
            tiles: std::cell::OnceCell::new(),
            record_cache: std::cell::RefCell::new(ChunkCache::for_spec(spec)?),
            pending_pathname_parent_sync: true,
            opened_after_crash: false,
            poison: PoisonFlag::healthy(),
            _lock: None,
        };
        file.create_tiles(&dims, creation_nonce)?;
        file.write_embedded_manifest_if_needed()?;
        file.mark_writer_open()?;
        Ok(file)
    }

    /// Opens a read-only handle that keeps **no** record directory, and hands
    /// the directory to the caller.
    ///
    /// The last thing about an open that still scales with the file is the
    /// 16-byte directory slot per record. This is the option that removes it
    /// from varlit's side. It is not a reduction in capability: `blocks`,
    /// `scan`, `keyed_blocks`, `metadata`, `verify_all` and the rest all still
    /// work — through [`with_directory`](Self::with_directory), which is where
    /// the directory now is.
    ///
    /// ```ignore
    /// let mut index = Vec::new();
    /// let file = VarlitFile::open_readonly_without_directory(spec, path, &mut index)?;
    /// let points = file.with_directory(&index)?.blocks::<Point>()?;
    /// // and `file.blocks::<Point>()` refuses with `NoResidentDirectory`,
    /// // naming `with_directory`, rather than answering as if empty.
    /// ```
    ///
    /// **`index` is output, not scratch** — the one place in this crate where a
    /// caller-supplied buffer is kept rather than reused. That is deliberate
    /// and it is why the parameter is not optional: a handle that keeps no
    /// directory cannot produce one afterwards, so a caller who did not take it
    /// here would have no way to read the file positionally at all. Taking the
    /// buffer is what makes the option impossible to half-use.
    ///
    /// Read-only. A writer keeps its directory unconditionally: the checkpoint
    /// cadence, the segment cursor and the append rollback all resolve
    /// positions through it, so there is nothing to hand over.
    pub fn open_readonly_without_directory<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        index: &mut Vec<RecordIndexEntry>,
    ) -> Result<Self> {
        Self::open_readonly_inner(spec, path, index, DirectoryRetention::None)
    }

    /// Opens a read-write handle.
    ///
    /// **This allocates the scan's entry buffer.**
    /// [`open_with_scratch`](Self::open_with_scratch) takes the caller's
    /// instead; see [`open_readonly_with_scratch`](Self::open_readonly_with_scratch)
    /// for what that buffer is and why the caller owns it.
    pub fn open<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Self::open_with_lock_policy(spec, path, WriterLockBreakPolicy::Refuse)
    }

    /// Explicitly reconstructs writer state using the normal scanning open.
    /// Like [`Self::open`], this can recover/truncate an uncommitted tail.
    pub fn open_scan<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Self::open(spec, path)
    }

    /// Opens a read-write handle, scanning into the caller's buffer.
    ///
    /// See [`open_readonly_with_scratch`](Self::open_readonly_with_scratch):
    /// `scratch` is scratch, not output, and is cleared on entry.
    pub fn open_with_scratch<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        scratch: &mut Vec<RecordIndexEntry>,
    ) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        // Every way this open can fail - a rejected header, a version or schema
        // mismatch, a read-limit refusal, a corrupt tail - happens after the
        // claim was taken, so all of them release it here. See
        // `with_writer_lock`.
        with_writer_lock(lock, |lock| {
            Self::open_locked_with_scratch(spec, path, lock, scratch)
        })
    }

    fn open_locked(spec: FormatSpec, path: PathBuf, lock: &mut WriterLock) -> Result<Self> {
        let mut scratch = BufferedScan::new(spec);
        Self::open_locked_with_scratch(spec, path, lock, &mut scratch)
    }

    fn open_locked_with_scratch<B: ScanBuffer>(
        spec: FormatSpec,
        path: PathBuf,
        lock: &mut WriterLock,
        scratch: &mut B,
    ) -> Result<Self> {
        let mut file =
            PrimaryFile::new(OpenOptions::new().read(true).write(true).open(&path)?, None);
        lock.bind_native(file.raw(), &path)?;
        let (header_len, header_extensions) = read_primary_header(spec, &mut file)?;
        // Logical: measured once the header has installed the anchor layout.
        check_open_file_len(spec, &file)?;
        let append_start = header_len;
        let mut index =
            load_generation_index(spec, &mut file, append_start, ScanIntent::Writer, scratch)?;
        // The sequence high-water mark and the block tails come from the scan
        // rather than from the index, because the index is filtered: a
        // non-resident block's records are on disk and not in it.
        let sequence_state = index.sequence_state();
        let block_tails = index.block_tails()?;
        let frames_verified = index.frames_verified;
        let boundaries = index.boundaries.take();
        let base_records = index.base_records;
        let base_segment = index.base_segment;
        let index = &*scratch;
        // The crash verdict has to be read *before* the handle exists, because
        // truncation happens here and would destroy the evidence it decides
        // about. `mark_writer_open` re-reads it for the handle's own field; the
        // two agree because nothing has written the block in between.
        let previous_writer_crashed = decode_liveness_flags(header_len, &header_extensions)?
            .is_some_and(|flags| flags & LIVENESS_FLAG_DIRTY != 0);
        let pending_mark = truncate_uncommitted_tail_if_needed(
            spec,
            &mut file,
            append_start,
            index,
            previous_writer_crashed,
        )?;
        let derived = derive_index_state(index.entries(), false)?;
        let segment_cursor =
            SegmentCursor::rebased(derived.segment_cursor, base_records, base_segment);
        let snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, file.len()?)?;
        let mut opened = Self {
            spec,
            path,
            file: RecordFile::new(file),
            index: ResidentIndex::none_retained_opened(
                snapshot.clone(),
                spec,
                base_records + index.record_count(),
                frames_verified,
                boundaries,
            ),
            snapshot,
            mode: OpenMode::ReadWrite,
            follow_scan: None,
            header_extensions,
            sequence_state,
            block_tails,
            tiles: std::cell::OnceCell::new(),
            record_cache: std::cell::RefCell::new(ChunkCache::for_spec(spec)?),
            segment_cursor,
            uncommitted_since_commit: false,
            commit_scope: CommitScope::FRESH,
            record_buffer: Vec::new(),
            immediate_state: Default::default(),
            appended_since_digest: false,
            last_resident_block_id: index.last_entry()?.map(|entry| entry.block_id),
            keyed_tails: KeyedTails::new_empty(),
            pending_pathname_parent_sync: false,
            opened_after_crash: false,
            poison: PoisonFlag::healthy(),
            _lock: None,
        };
        opened.mark_writer_open()?;
        if let Some(from) = pending_mark {
            opened.recover_marked_tail(from)?;
        }
        Ok(opened)
    }

    pub fn open_with_lock_policy<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        policy: WriterLockBreakPolicy,
    ) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        let lock = WriterLock::acquire_with_policy(&path, policy)?;
        // A broken stale claim that then fails to open must not become a new
        // stale claim of this process's own making.
        with_writer_lock(lock, |lock| Self::open_locked(spec, path, lock))
    }

    /// Opens a read-only handle.
    ///
    /// Uses bounded scan/directory buffers and private temporary spill files.
    /// [`open_readonly_with_scratch`](Self::open_readonly_with_scratch) instead
    /// fills a reusable caller-owned entry array, which retains its result-size limit.
    pub fn open_readonly<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        let mut scratch = BufferedScan::new(spec);
        Self::open_readonly_inner(spec, path, &mut scratch, DirectoryRetention::Keep)
    }

    /// Explicitly reconstructs read-only state by scanning, keeping no directory.
    /// Returns scan/I/O/resource errors; never changes the input file.
    pub fn open_readonly_scan<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        let mut scratch = BufferedScan::new(spec);
        Self::open_readonly_inner(spec, path, &mut scratch, DirectoryRetention::None)
    }

    /// Opens a read-only handle, scanning into the caller's buffer.
    ///
    /// `scratch` is where the open path builds the entries it reads out of the
    /// file. It is **scratch, not output**: the handle keeps a 16-byte
    /// directory slot per record and rebuilds each entry from the record when
    /// asked, so nothing in `scratch` is retained past this call and the caller
    /// may clear it, reuse it for the next open, or drop it.
    ///
    /// Taking it here is the point. The entry buffer is the largest allocation
    /// an open makes — it scales with record count — and a library that sizes
    /// and owns it decides the caller's memory for them. Opening a thousand
    /// files through one buffer allocates once, not a thousand times.
    ///
    /// The buffer is cleared on entry, so its previous contents never leak into
    /// this file's index.
    pub fn open_readonly_with_scratch<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
        scratch: &mut Vec<RecordIndexEntry>,
    ) -> Result<Self> {
        Self::open_readonly_inner(spec, path, scratch, DirectoryRetention::Keep)
    }

    fn open_readonly_inner<P: AsRef<Path>, B: ScanBuffer>(
        spec: FormatSpec,
        path: P,
        scratch: &mut B,
        retention: DirectoryRetention,
    ) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        let mut file = PrimaryFile::new(OpenOptions::new().read(true).open(&path)?, None);
        let (header_len, header_extensions) = read_primary_header(spec, &mut file)?;
        // Logical: measured once the header has installed the anchor layout.
        check_open_file_len(spec, &file)?;
        let append_start = header_len;
        let mut scanned =
            load_generation_index(spec, &mut file, append_start, ScanIntent::ReadOnly, scratch)?;

        let sequence_state = scanned.sequence_state();
        let block_tails = scanned.block_tails()?;
        let frames_verified = scanned.frames_verified;
        let boundaries = scanned.boundaries.take();
        // The snapshot comes from the scan, not from the resident list. The
        // list is filtered — a non-resident block's records are on disk and not
        // in it, exactly as `open_locked`'s comment says — so binding the
        // snapshot to its last entry stopped a read-only handle at the last
        // *resident* record. Under a markerless policy that ends on a
        // non-resident block, that is before every record of that block, and
        // `block_chain`, the published way to reach one, walks through the
        // snapshot.
        //
        // It is still not the file's physical length: a torn trailing record
        // must leave the snapshot at the end of the last complete record, and
        // under a marker policy the uncommitted tail a writer open would delete
        // must not be visible here either. Both facts belong to the scan.
        let logical_len = scanned.physical_end(append_start);
        // An open that resumed at an anchor holds only the records after it,
        // so it cannot keep a whole-file list; it keeps the count instead.
        let base_records = scanned.base_records;
        let base_segment = scanned.base_segment;
        let retention = if base_records > 0 {
            DirectoryRetention::None
        } else {
            retention
        };
        let index = &*scratch;
        let derived = derive_index_state(index.entries(), false)?;
        let segment_cursor =
            SegmentCursor::rebased(derived.segment_cursor, base_records, base_segment);
        let snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, logical_len)?;
        let mut opened = Self {
            spec,
            path,
            file: RecordFile::new(file),
            index: match retention {
                // Not "built and then dropped": a caller that asked for no
                // directory never has one allocated on its behalf. `N` slots is
                // the largest thing an open would have taken, and the point of
                // the option is that varlit does not take it.
                // The scan counted the records; keeping that one number is what
                // spares every later read a counting walk.
                DirectoryRetention::None => ResidentIndex::none_retained_opened(
                    snapshot.clone(),
                    spec,
                    base_records + index.record_count(),
                    frames_verified,
                    boundaries,
                ),
                DirectoryRetention::Keep => ResidentIndex::adopt_generation(
                    index,
                    snapshot.clone(),
                    spec,
                    frames_verified,
                    boundaries,
                )?,
            },
            snapshot,
            mode: OpenMode::ReadOnly,
            follow_scan: None,
            header_extensions,
            sequence_state,
            block_tails,
            tiles: std::cell::OnceCell::new(),
            record_cache: std::cell::RefCell::new(ChunkCache::for_spec(spec)?),
            segment_cursor,
            uncommitted_since_commit: false,
            commit_scope: CommitScope::FRESH,
            record_buffer: Vec::new(),
            immediate_state: Default::default(),
            appended_since_digest: false,
            last_resident_block_id: index.last_entry()?.map(|entry| entry.block_id),
            keyed_tails: KeyedTails::new_empty(),
            pending_pathname_parent_sync: false,
            opened_after_crash: false,
            poison: PoisonFlag::healthy(),
            _lock: None,
        };
        opened.open_tiles_now()?;
        Ok(opened)
    }

    /// Opens read-only using only the selected summary metadata, without a full scan.
    ///
    /// Uses the header-tail table when declared, otherwise the trailing digest.
    /// Returns errors directly; never substitutes a full scan or another summary
    /// source after an error. `LazyOpenMetadataUnavailable` means the selected
    /// summary is missing, stale or structurally invalid. I/O, allocation,
    /// checksum and resource errors retain their existing variants.
    ///
    /// The handle keeps no directory. Use `block_chain`, `block_tail_offset`,
    /// `read_block_at`, entry-taking `_into` reads, or `record_map` as appropriate.
    /// Position-based operations require a caller-provided directory.
    ///
    /// After an error, callers may explicitly choose [`Self::open_readonly_scan`]
    /// or [`Self::open_readonly_lazy_or_scan`]. AtOpen full-payload verification
    /// returns `LazyOpenVerificationRequiresScan` without starting a scan.
    pub fn open_readonly_lazy<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self::open_readonly_lazy_with_report(spec, path)?.0)
    }

    /// [`open_readonly_lazy`](Self::open_readonly_lazy), and which of the two
    /// metadata sources it used; this API never reports `FullScan`.
    pub fn open_readonly_lazy_with_report<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
    ) -> Result<(Self, LazyOpenSource)> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        ensure_lazy_verification_policy(spec)?;
        let source = if spec.index_policy.header_tails {
            LazyOpenSource::HeaderTails
        } else {
            LazyOpenSource::Digest
        };
        let file = Self::open_readonly_from_digest(spec, path.as_ref(), source)
            .map_err(|error| lazy_metadata_error(error, source))?;
        Ok((file, source))
    }

    /// Opens from metadata, falling back to a full scan on failure.
    /// This explicitly preserves the former `open_readonly_lazy` behavior.
    pub fn open_readonly_lazy_or_scan<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self::open_readonly_lazy_or_scan_with_report(spec, path)?.0)
    }

    /// Like [`Self::open_readonly_lazy_or_scan`], reporting the route taken.
    /// Metadata errors other than missing-limit/authorization errors can cause
    /// a scan; its result is returned. Use strict lazy open to retain that error.
    pub fn open_readonly_lazy_or_scan_with_report<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
    ) -> Result<(Self, LazyOpenSource)> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        // The header table first, because nothing appended can hide it; the
        // digest second, because a file written before the option existed has
        // only that. Each failure is a fall-through, not an error, exactly as
        // the digest already falls through to the scan.
        if spec.index_policy.header_tails
            && let Ok(file) =
                Self::open_readonly_from_digest(spec, &path, LazyOpenSource::HeaderTails)
        {
            return Ok((file, LazyOpenSource::HeaderTails));
        }
        match Self::open_readonly_from_digest(spec, &path, LazyOpenSource::Digest) {
            Ok(file) => Ok((file, LazyOpenSource::Digest)),
            // Only a *spec-level* refusal propagates, exactly as it does for
            // the segment chain: those describe the caller's configuration and
            // the scan would give the same answer.
            Err(error @ Error::MissingResourceLimit { .. })
            | Err(error @ Error::TrustedUnboundedRequiresExplicitApi { .. }) => Err(error),
            Err(_) => {
                // The scan needs somewhere to put the entries it frames. This
                // buffer is transient — the handle keeps no directory either
                // way — but it is `N` entries while it lives, which is the cost
                // of the fallback and is why the report exists.
                let mut scratch = BufferedScan::new(spec);
                let file =
                    Self::open_readonly_inner(spec, path, &mut scratch, DirectoryRetention::None)?;
                Ok((file, LazyOpenSource::FullScan))
            }
        }
    }

    /// Opens a writer using only the selected summary metadata, without a full scan.
    ///
    /// Like [`Self::open_readonly_lazy`], returns metadata/I/O/allocation errors
    /// directly. A failed metadata open does not truncate the native tail; its
    /// writer lock is released. No resident directory is built.
    ///
    /// Checkpoint/segment-on-flush policies are refused because they serialize
    /// a directory this handle does not retain. AtOpen verification requires an
    /// explicit scan. Choose [`Self::open_scan`] to reconstruct writer state, or
    /// [`Self::open_lazy_or_scan`] to retain automatic fallback behavior.
    pub fn open_lazy<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self::open_lazy_with_report(spec, path)?.0)
    }

    /// [`open_lazy`](Self::open_lazy), and the selected summary source.
    pub fn open_lazy_with_report<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
    ) -> Result<(Self, LazyOpenSource)> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        ensure_lazy_verification_policy(spec)?;
        ensure_lazy_writer_policy(spec)?;
        let source = if spec.index_policy.header_tails {
            LazyOpenSource::HeaderTails
        } else {
            LazyOpenSource::Digest
        };
        let path = path.as_ref();
        let lock = WriterLock::acquire(path)?;
        let (file, lock) = with_writer_lock_value(lock, |lock| {
            Self::open_writer_from_digest(spec, path, lock, source)
                .map_err(|error| lazy_metadata_error(error, source))
        })?;
        let mut file = file;
        file._lock = Some(lock);
        Ok((file, source))
    }

    /// Opens a writer from metadata, falling back to a scanning open on failure.
    /// This explicitly preserves the former `open_lazy` behavior, including
    /// normal scanning-writer recovery of the uncommitted tail.
    pub fn open_lazy_or_scan<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self::open_lazy_or_scan_with_report(spec, path)?.0)
    }

    /// Like [`Self::open_lazy_or_scan`], reporting the route taken.
    pub fn open_lazy_or_scan_with_report<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
    ) -> Result<(Self, LazyOpenSource)> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        if spec.index_policy.checkpoint_on_flush {
            return Err(Error::LazyWriterIndexPolicy {
                policy: "checkpoint_on_flush",
            });
        }
        if spec.index_policy.segment_on_flush {
            return Err(Error::LazyWriterIndexPolicy {
                policy: "segment_on_flush",
            });
        }
        let path = path.as_ref().to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        // The claim is taken before either route runs and released by
        // `with_writer_lock_value` on every failure, the same as the scanning
        // open — a lazy attempt that falls back must not hold two claims, and a
        // refused one must not leave a stale claim behind.
        let ((mut file, source), lock) = with_writer_lock_value(lock, |lock| {
            if spec.index_policy.header_tails
                && let Ok(file) =
                    Self::open_writer_from_digest(spec, &path, lock, LazyOpenSource::HeaderTails)
            {
                return Ok((file, LazyOpenSource::HeaderTails));
            }
            match Self::open_writer_from_digest(spec, &path, lock, LazyOpenSource::Digest) {
                Ok(file) => Ok((file, LazyOpenSource::Digest)),
                // Only a spec-level refusal propagates, exactly as it does for
                // the read-only route: those describe the caller's
                // configuration and the scan would answer the same.
                Err(error @ Error::MissingResourceLimit { .. })
                | Err(error @ Error::TrustedUnboundedRequiresExplicitApi { .. }) => Err(error),
                Err(_) => {
                    let mut scratch = BufferedScan::new(spec);
                    let file = Self::open_locked_with_scratch(
                        spec,
                        path.try_clone()?,
                        lock,
                        &mut scratch,
                    )?;
                    Ok((file, LazyOpenSource::FullScan))
                }
            }
        })?;
        // Installed here rather than inside the body, the same as
        // `with_writer_lock` does it: the body runs twice on the fallback path
        // and must not claim the lock either time.
        file._lock = Some(lock);
        Ok((file, source))
    }

    /// The digest route for a writer: no scan, no directory, and the writer
    /// lock already bound by the caller.
    fn open_writer_from_digest(
        spec: FormatSpec,
        path: &Path,
        lock: &mut WriterLock,
        source: LazyOpenSource,
    ) -> Result<Self> {
        let mut file =
            PrimaryFile::new(OpenOptions::new().read(true).write(true).open(path)?, None);
        lock.bind_native(file.raw(), path)?;
        let (header_len, header_extensions) = read_primary_header(spec, &mut file)?;
        // Logical: measured once the header has installed the anchor layout.
        let captured_len = check_open_file_len(spec, &file)?;
        let append_start = header_len;
        // No `truncate_uncommitted_tail_if_needed`, and both routes earn that
        // the same way — by requiring the thing they read to account for the
        // file all the way to its last byte.
        //
        // The digest does it by position: `frame_open_digest` refuses a digest
        // whose extent does not end exactly at the file's length, so a digest
        // that was found is the last record and was written at a commit point.
        //
        // The header table cannot do it by position — it is at a fixed offset
        // and is always "last" — so it does it by walking:
        // `corroborate_header_tails` frames the commit marker the slot names
        // and steps forward through at most a segment and a digest, and refuses
        // unless that lands exactly on the file's length. A file with records
        // past its last marker overshoots and this route fails into the scan,
        // which is the open that does truncate.
        //
        // Either way a file that reaches the line below has no uncommitted tail
        // to cut. `a_file_with_an_uncommitted_tail_is_truncated_by_the_scan`
        // is what holds that for the header route.
        let digest = read_lazy_block_tails(
            spec,
            &mut file,
            append_start,
            captured_len,
            header_len,
            &header_extensions,
            source,
        )?;
        let snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, digest.physical_end)?;
        let mut opened = Self {
            spec,
            path: path.to_path_buf(),
            file: RecordFile::new(file),
            index: ResidentIndex::none_retained(snapshot.clone(), spec),
            snapshot,
            mode: OpenMode::ReadWrite,
            follow_scan: None,
            header_extensions,
            sequence_state: match digest.sequence_high_water {
                None => SequenceState::Available(0),
                Some(u64::MAX) => SequenceState::Exhausted,
                Some(sequence) => SequenceState::Available(sequence + 1),
            },
            // Both are refused above rather than approximated: see
            // `open_lazy`'s doc. Empty is what "never used" looks like.
            segment_cursor: SegmentCursor::new_empty(),
            block_tails: digest.block_tails,
            tiles: std::cell::OnceCell::new(),
            record_cache: std::cell::RefCell::new(ChunkCache::for_spec(spec)?),
            uncommitted_since_commit: false,
            commit_scope: CommitScope::FRESH,
            record_buffer: Vec::new(),
            immediate_state: Default::default(),
            // The file ends with a digest describing everything before it, so
            // there is nothing to describe until something is appended. Both
            // terms of `needs_open_digest` are false here, which is right.
            appended_since_digest: false,
            last_resident_block_id: None,
            keyed_tails: KeyedTails::new_empty(),
            pending_pathname_parent_sync: false,
            opened_after_crash: false,
            poison: PoisonFlag::healthy(),
            _lock: None,
        };
        opened.open_tiles_now()?;
        Ok(opened)
    }

    fn open_readonly_from_digest(
        spec: FormatSpec,
        path: &Path,
        source: LazyOpenSource,
    ) -> Result<Self> {
        let mut file = PrimaryFile::new(OpenOptions::new().read(true).open(path)?, None);
        let (header_len, header_extensions) = read_primary_header(spec, &mut file)?;
        // Logical: measured once the header has installed the anchor layout.
        let captured_len = check_open_file_len(spec, &file)?;
        let append_start = header_len;
        let digest = read_lazy_block_tails(
            spec,
            &mut file,
            append_start,
            captured_len,
            header_len,
            &header_extensions,
            source,
        )?;
        let snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, digest.physical_end)?;
        let mut opened = Self {
            spec,
            path: path.to_path_buf(),
            file: RecordFile::new(file),
            index: ResidentIndex::none_retained(snapshot.clone(), spec),
            snapshot,
            mode: OpenMode::ReadOnly,
            follow_scan: None,
            header_extensions,
            sequence_state: match digest.sequence_high_water {
                None => SequenceState::Available(0),
                Some(u64::MAX) => SequenceState::Exhausted,
                Some(sequence) => SequenceState::Available(sequence + 1),
            },
            // Both are writer state — when the next checkpoint is due, which
            // records the next segment covers — and this handle is read-only.
            // Recovering them is what would have required the walk.
            segment_cursor: SegmentCursor::new_empty(),
            block_tails: digest.block_tails,
            tiles: std::cell::OnceCell::new(),
            record_cache: std::cell::RefCell::new(ChunkCache::for_spec(spec)?),
            uncommitted_since_commit: false,
            commit_scope: CommitScope::FRESH,
            record_buffer: Vec::new(),
            immediate_state: Default::default(),
            appended_since_digest: false,
            // No record was framed, so there is nothing to seed this from — and
            // nothing to seed it for: it is append-path state and this handle is
            // read-only.
            last_resident_block_id: None,
            keyed_tails: KeyedTails::new_empty(),
            pending_pathname_parent_sync: false,
            opened_after_crash: false,
            poison: PoisonFlag::healthy(),
            _lock: None,
        };
        opened.open_tiles_now()?;
        Ok(opened)
    }

    pub fn open_recover<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self::open_recover_with_report(spec, path)?.0)
    }

    pub fn open_recover_with_report<P: AsRef<Path>>(
        spec: FormatSpec,
        path: P,
    ) -> Result<(Self, RecoveryReport)> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        let path = path.as_ref().to_path_buf();
        let lock = WriterLock::acquire(&path)?;
        // This shape is a pair, not a bare `VarlitFile`, so it uses
        // `with_writer_lock_value` and installs the claim itself.
        let ((mut file, report), lock) =
            with_writer_lock_value(lock, |lock| Self::open_recover_locked(spec, path, lock))?;
        file._lock = Some(lock);
        Ok((file, report))
    }

    fn open_recover_locked(
        spec: FormatSpec,
        path: PathBuf,
        lock: &mut WriterLock,
    ) -> Result<(Self, RecoveryReport)> {
        let mut scratch = BufferedScan::new(spec);
        Self::open_recover_locked_with_scratch(spec, path, lock, &mut scratch)
    }

    fn open_recover_locked_with_scratch<B: ScanBuffer>(
        spec: FormatSpec,
        path: PathBuf,
        lock: &mut WriterLock,
        scratch: &mut B,
    ) -> Result<(Self, RecoveryReport)> {
        let spec = spec.with_strict_read_validation();
        let mut file =
            PrimaryFile::new(OpenOptions::new().read(true).write(true).open(&path)?, None);
        lock.bind_native(file.raw(), &path)?;
        let original_len = file.len()?;
        let (header_len, header_extensions) = read_primary_header(spec, &mut file)?;
        let append_start = header_len;
        let mut index =
            load_generation_index(spec, &mut file, append_start, ScanIntent::Recover, scratch)?;
        let sequence_state = index.sequence_state();
        let block_tails = index.block_tails()?;
        let frames_verified = index.frames_verified;
        let boundaries = index.boundaries.take();
        let base_records = index.base_records;
        let base_segment = index.base_segment;
        let index = &*scratch;
        // No mark here: this route reaches a file that already ended at a
        // commit point, so there is no tail. See the digest's frame check.
        truncate_uncommitted_tail_if_needed(spec, &mut file, append_start, index, false)?;
        let recovered_len = file.len()?;
        let derived = derive_index_state(index.entries(), false)?;
        let segment_cursor =
            SegmentCursor::rebased(derived.segment_cursor, base_records, base_segment);
        let records_preserved = index.record_count();
        let snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, file.len()?)?;
        let report = RecoveryReport {
            original_len,
            recovered_len,
            records_preserved,
        };
        let mut opened = Self {
            spec,
            path,
            file: RecordFile::new(file),
            index: ResidentIndex::none_retained_opened(
                snapshot.clone(),
                spec,
                base_records + index.record_count(),
                frames_verified,
                boundaries,
            ),
            snapshot,
            mode: OpenMode::ReadWrite,
            follow_scan: None,
            header_extensions,
            sequence_state,
            block_tails,
            tiles: std::cell::OnceCell::new(),
            record_cache: std::cell::RefCell::new(ChunkCache::for_spec(spec)?),
            segment_cursor,
            uncommitted_since_commit: false,
            commit_scope: CommitScope::FRESH,
            record_buffer: Vec::new(),
            immediate_state: Default::default(),
            appended_since_digest: false,
            last_resident_block_id: index.last_entry()?.map(|entry| entry.block_id),
            keyed_tails: KeyedTails::new_empty(),
            pending_pathname_parent_sync: false,
            opened_after_crash: false,
            poison: PoisonFlag::healthy(),
            _lock: None,
        };
        opened.open_tiles_now()?;
        Ok((opened, report))
    }
}
