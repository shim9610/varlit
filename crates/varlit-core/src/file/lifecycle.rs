// Native file lifecycle operations.

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
        let mut file = options.open(&path)?;
        // bind the single-writer object lock before any destructive
        // initialization. Opening with `.truncate(true)` would clear the new
        // object inside the pre-bind window where a concurrent creator that
        // lost the race could still hold an unbound handle to it.
        lock.bind_native(&file, &path)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let header_extensions = write_file_header(spec, &mut file)?;
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        let mut file = Self {
            spec,
            path,
            file: RecordFile::new(file),
            index: ResidentIndex::empty(snapshot.clone(), spec),
            snapshot,
            mode: OpenMode::ReadWrite,
            follow_scan: None,
            header_extensions,
            matrix: None,
            matrix_creation_nonce: None,
            sequence_state: SequenceState::Available(0),
            checkpoint_cadence: CheckpointCadence::new_empty(),
            block_tails: BlockTails::new_empty(),
            segment_cursor: SegmentCursor::new_empty(),
            uncommitted_since_commit: false,
            record_buffer: Vec::new(),
            immediate_state: Default::default(),
            appended_since_digest: false,
            last_resident_block_id: None,
            keyed_tails: KeyedTails::new_empty(),
            // this handle established the pathname, so its
            // directory entry is not durable until the first durability
            // request syncs the parent directory.
            open_chunk: None,
            chunk_directory: std::cell::OnceCell::new(),
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
            // A chunk is buffered against `MatrixSlotRegionLen` and written
            // against `RecordPayloadLen`, and nothing reconciled them: a spec
            // could pass `validate`, accept writes, and then die at the first
            // write with the data already in RAM and no way to get it out.
            // Refused here, before a byte is accepted.
            let written = chunk_payload_len_for(spec, &dims)?;
            spec.read_limits
                .check(ReadLimitKey::RecordPayloadLen, written)?;
            spec.read_limits
                .check(ReadLimitKey::MatrixSlotRegionLen, written)?;
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
        let mut file = options.open(&path)?;
        // bind the single-writer object lock before any destructive
        // initialization; see `create_impl`.
        lock.bind_native(&file, &path)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let header_extensions = write_file_header(spec, &mut file)?;
        // stamp a fresh creation nonce so this logical matrix is
        // distinguishable from any earlier matrix that lived in the same OS
        // file object at the same layout offsets.
        let creation_nonce = fresh_matrix_creation_nonce();
        write_matrix_creation_nonce_region(&mut file, creation_nonce)?;
        let header_len = file.stream_position()?;
        let matrix = crate::matrix::create_layout(spec, &mut file, header_len, &dims)?;
        let mut matrix_pages = crate::matrix_generation::MatrixFile::create(
            &path,
            &file,
            matrix.append_log_start(),
            creation_nonce,
            matrix_pending_limit(spec),
        )?;
        matrix_pages.set_cache_bytes(spec.read_limits.matrix_generation_cache_budget());
        matrix_pages.set_traversal_bytes(limit_or_max(
            spec.read_limits.require(ReadLimitKey::ScanBytes)?,
        ));
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        let mut file = Self {
            spec,
            path,
            file: RecordFile::new(file).with_matrix(Some(matrix_pages)),
            index: ResidentIndex::empty(snapshot.clone(), spec),
            snapshot,
            mode: OpenMode::ReadWrite,
            follow_scan: None,
            header_extensions,
            matrix: Some(matrix),
            matrix_creation_nonce: Some(creation_nonce),
            sequence_state: SequenceState::Available(0),
            checkpoint_cadence: CheckpointCadence::new_empty(),
            block_tails: BlockTails::new_empty(),
            segment_cursor: SegmentCursor::new_empty(),
            uncommitted_since_commit: false,
            record_buffer: Vec::new(),
            immediate_state: Default::default(),
            appended_since_digest: false,
            last_resident_block_id: None,
            keyed_tails: KeyedTails::new_empty(),
            // this handle established the pathname, so its
            // directory entry is not durable until the first durability
            // request syncs the parent directory.
            open_chunk: None,
            chunk_directory: std::cell::OnceCell::new(),
            pending_pathname_parent_sync: true,
            opened_after_crash: false,
            poison: PoisonFlag::healthy(),
            _lock: None,
        };
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
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        lock.bind_native(&file, &path)?;
        let captured_len = check_open_file_len(spec, &file)?;
        let (header_len, header_extensions) = read_file_header_parts(spec, &mut file)?;
        let (matrix, matrix_creation_nonce, matrix_pages) = split_matrix_state(
            read_matrix_layout_if_needed(&path, true, spec, &mut file, header_len, captured_len)?,
        );
        let append_start = append_log_start(header_len, matrix.as_ref());
        let mut index = load_generation_index(
            spec,
            &mut file,
            append_start,
            ScanIntent::Writer,
            scratch,
            matrix_pages.as_ref(),
        )?;
        // The sequence high-water mark and the block tails come from the scan
        // rather than from the index, because the index is filtered: a
        // non-resident block's records are on disk and not in it.
        let sequence_state = index.sequence_state();
        let block_tails = index.block_tails()?;
        let frames_verified = index.frames_verified;
        let boundaries = index.boundaries.take();
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
        let checkpoint_cadence = derived.checkpoint_cadence;
        let segment_cursor = derived.segment_cursor;
        let snapshot = SnapshotFile::from_file_with_len(
            file.try_clone()?,
            matrix_pages
                .as_ref()
                .map_or(file.metadata()?.len(), |p| p.head().native_eof),
        )?;
        let mut opened = Self {
            spec,
            path,
            file: RecordFile::new(file).with_matrix(matrix_pages),
            index: ResidentIndex::adopt_generation(
                index,
                snapshot.clone(),
                spec,
                frames_verified,
                boundaries,
            )?,
            snapshot,
            mode: OpenMode::ReadWrite,
            follow_scan: None,
            header_extensions,
            matrix,
            matrix_creation_nonce,
            sequence_state,
            checkpoint_cadence,
            block_tails,
            open_chunk: None,
            chunk_directory: std::cell::OnceCell::new(),
            segment_cursor,
            uncommitted_since_commit: false,
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
        let file = OpenOptions::new().read(true).open(&path)?;
        Self::open_readonly_file(spec, path, file, scratch, retention)
    }

    // Accepts an already-open, independently positioned read handle. Admission
    // can check object identity before parsing any caller-controlled file bytes.
    fn open_readonly_file<B: ScanBuffer>(
        spec: FormatSpec,
        path: PathBuf,
        mut file: File,
        scratch: &mut B,
        retention: DirectoryRetention,
    ) -> Result<Self> {
        let captured_len = check_open_file_len(spec, &file)?;
        let (header_len, header_extensions) = read_file_header_parts(spec, &mut file)?;
        let (matrix, matrix_creation_nonce, matrix_pages) = split_matrix_state(
            read_matrix_layout_if_needed(&path, false, spec, &mut file, header_len, captured_len)?,
        );
        let append_start = append_log_start(header_len, matrix.as_ref());
        let mut scanned = load_generation_index(
            spec,
            &mut file,
            append_start,
            ScanIntent::ReadOnly,
            scratch,
            matrix_pages.as_ref(),
        )?;
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
        let index = &*scratch;
        let derived = derive_index_state(index.entries(), false)?;
        let checkpoint_cadence = derived.checkpoint_cadence;
        let segment_cursor = derived.segment_cursor;
        let snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, logical_len)?;
        Ok(Self {
            spec,
            path,
            file: RecordFile::new(file).with_matrix(matrix_pages),
            index: match retention {
                // Not "built and then dropped": a caller that asked for no
                // directory never has one allocated on its behalf. `N` slots is
                // the largest thing an open would have taken, and the point of
                // the option is that varlit does not take it.
                DirectoryRetention::None => ResidentIndex::none_retained(snapshot.clone(), spec),
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
            matrix,
            matrix_creation_nonce,
            sequence_state,
            checkpoint_cadence,
            block_tails,
            open_chunk: None,
            chunk_directory: std::cell::OnceCell::new(),
            segment_cursor,
            uncommitted_since_commit: false,
            record_buffer: Vec::new(),
            immediate_state: Default::default(),
            appended_since_digest: false,
            last_resident_block_id: index.last_entry()?.map(|entry| entry.block_id),
            keyed_tails: KeyedTails::new_empty(),
            pending_pathname_parent_sync: false,
            opened_after_crash: false,
            poison: PoisonFlag::healthy(),
            _lock: None,
        })
    }

    /// Opens read-only **without reading the records at all**, using the open
    /// digest at the end of the file.
    ///
    /// The three facts an open needs — where the committed file ends, the next
    /// append's sequence, each block's newest record — are the by-product of
    /// walking every record, and that walk is the only reason an open that keeps
    /// no index still reads the whole file. `IndexPolicy::open_digest_on_flush`
    /// writes them down at each commit point; this reads them back.
    ///
    /// Measured, 50,000 records:
    ///
    /// | open | read syscalls | bytes read | file overhead |
    /// | --- | --- | --- | --- |
    /// | `open_readonly` (scan) | 100,316 | 3,207,102 | — |
    /// | `open_readonly` (segment chain) | 516 | 7,332,522 | +3.67 MB |
    /// | this, from a digest | **20** | **516** | **+12.8 KB** |
    ///
    /// The last column is the one that decides which of the two tail records a
    /// format wants. A segment writes an entry per record it covers, so its
    /// overhead grows with the file; a digest writes twelve bytes per distinct
    /// block id, so the 12.8 KB above is a hundred flushes' worth and would be
    /// the same at a billion records. What the segment buys for its size is an
    /// open that *builds the index*; a digest deliberately does not.
    ///
    /// The handle keeps **no directory**, because there is none to keep — no
    /// record was framed. Position-based reads (`blocks`, `scan`,
    /// `keyed_blocks`) refuse with `NoResidentDirectory` naming
    /// [`with_directory`](Self::with_directory), and the index the caller
    /// actually needs is built with [`record_map`](Self::record_map), which
    /// walks only as far as the question. That pairing is the point: an open
    /// that reads nothing, and an index that reads only what was asked for.
    ///
    /// What still works with no directory at all, because none of it goes
    /// through positions: [`block_chain`](Self::block_chain) and
    /// [`block_tail_offset`](Self::block_tail_offset) — the digest carries the
    /// tails, which is why it carries them — [`read_block_at`](Self::read_block_at),
    /// and every `_into` read that takes an entry.
    ///
    /// **Falls back to the full scan** when the file has no usable digest: one
    /// written before the option was enabled, one whose writer appended past
    /// its last commit, one truncated or rotted. That is not an error — the
    /// file is readable and this opens it — but it is the difference between
    /// eleven syscalls and a hundred thousand, so
    /// [`open_readonly_lazy_with_report`](Self::open_readonly_lazy_with_report)
    /// says which happened.
    pub fn open_readonly_lazy<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self::open_readonly_lazy_with_report(spec, path)?.0)
    }

    /// [`open_readonly_lazy`](Self::open_readonly_lazy), and which of the two
    /// routes it took.
    pub fn open_readonly_lazy_with_report<P: AsRef<Path>>(
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

    /// Opens a **read-write** handle from the digest, without scanning.
    ///
    /// The writer twin of [`open_readonly_lazy`](Self::open_readonly_lazy), and
    /// the half that was missing. Every other writer open calls `load_index`
    /// with `ScanIntent::Writer`, so it is `O(records)` in time and in retained
    /// memory at any file size — which put the standing "TB-scale files must
    /// work, memory bounded by the working set" requirement on the read side
    /// only, while the workload that requirement names as primary is
    /// continuous append, and a continuous appender reopens its writer on every
    /// restart.
    ///
    /// **Falls back to the scanning open** on any file whose digest is not
    /// usable, exactly as the read-only route does, and
    /// [`open_lazy_with_report`](Self::open_lazy_with_report) says which
    /// happened.
    ///
    /// # What this handle does not have
    ///
    /// No resident directory, the same as the read-only digest open:
    /// `blocks::<T>()` refuses with `NoResidentDirectory` and
    /// [`record_map`](Self::record_map) is the way to walk records. Appending
    /// is unaffected — the append path maintains `block_tails` and the sequence
    /// itself, and the digest supplied both.
    ///
    /// # Why `checkpoint_on_flush` and `segment_on_flush` are refused
    ///
    /// Not because their state is hard to recover — that was the reading of
    /// `open_readonly_from_digest`'s comment about the walk, and it is the
    /// wrong one. `CheckpointCadence` is two `usize`s and `SegmentCursor`'s
    /// `next_position` is *a position in the resident index*. Both records
    /// serialize that index: a checkpoint writes the whole of it, a segment one
    /// entry per record. A handle that keeps no index has nothing to write, so
    /// the cadence is not approximate here, it has no referent. Refusing the
    /// combination at open is honest where writing nothing would leave files
    /// that open slower than their spec claims.
    pub fn open_lazy<P: AsRef<Path>>(spec: FormatSpec, path: P) -> Result<Self> {
        Ok(Self::open_lazy_with_report(spec, path)?.0)
    }

    /// [`open_lazy`](Self::open_lazy), and which of the two routes it took.
    pub fn open_lazy_with_report<P: AsRef<Path>>(
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
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        lock.bind_native(&file, path)?;
        let captured_len = check_open_file_len(spec, &file)?;
        let (header_len, header_extensions) = read_file_header_parts(spec, &mut file)?;
        let (matrix, matrix_creation_nonce, matrix_pages) = split_matrix_state(
            read_matrix_layout_if_needed(path, true, spec, &mut file, header_len, captured_len)?,
        );
        let append_start = append_log_start(header_len, matrix.as_ref());
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
        let captured_len = matrix_pages
            .as_ref()
            .map_or(captured_len, |pages| pages.head().native_eof);
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
        Ok(Self {
            spec,
            path: path.to_path_buf(),
            file: RecordFile::new(file).with_matrix(matrix_pages),
            index: ResidentIndex::none_retained(snapshot.clone(), spec),
            snapshot,
            mode: OpenMode::ReadWrite,
            follow_scan: None,
            header_extensions,
            matrix,
            matrix_creation_nonce,
            sequence_state: match digest.sequence_high_water {
                None => SequenceState::Available(0),
                Some(u64::MAX) => SequenceState::Exhausted,
                Some(sequence) => SequenceState::Available(sequence + 1),
            },
            // Both are refused above rather than approximated: see
            // `open_lazy`'s doc. Empty is what "never used" looks like.
            checkpoint_cadence: CheckpointCadence::new_empty(),
            segment_cursor: SegmentCursor::new_empty(),
            block_tails: digest.block_tails,
            open_chunk: None,
            chunk_directory: std::cell::OnceCell::new(),
            uncommitted_since_commit: false,
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
        })
    }

    fn open_readonly_from_digest(
        spec: FormatSpec,
        path: &Path,
        source: LazyOpenSource,
    ) -> Result<Self> {
        let mut file = OpenOptions::new().read(true).open(path)?;
        let captured_len = check_open_file_len(spec, &file)?;
        let (header_len, header_extensions) = read_file_header_parts(spec, &mut file)?;
        let (matrix, matrix_creation_nonce, matrix_pages) = split_matrix_state(
            read_matrix_layout_if_needed(path, false, spec, &mut file, header_len, captured_len)?,
        );
        let append_start = append_log_start(header_len, matrix.as_ref());
        let captured_len = matrix_pages
            .as_ref()
            .map_or(captured_len, |pages| pages.head().native_eof);
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
        Ok(Self {
            spec,
            path: path.to_path_buf(),
            file: RecordFile::new(file).with_matrix(matrix_pages),
            index: ResidentIndex::none_retained(snapshot.clone(), spec),
            snapshot,
            mode: OpenMode::ReadOnly,
            follow_scan: None,
            header_extensions,
            matrix,
            matrix_creation_nonce,
            sequence_state: match digest.sequence_high_water {
                None => SequenceState::Available(0),
                Some(u64::MAX) => SequenceState::Exhausted,
                Some(sequence) => SequenceState::Available(sequence + 1),
            },
            // Both are writer state — when the next checkpoint is due, which
            // records the next segment covers — and this handle is read-only.
            // Recovering them is what would have required the walk.
            checkpoint_cadence: CheckpointCadence::new_empty(),
            segment_cursor: SegmentCursor::new_empty(),
            block_tails: digest.block_tails,
            open_chunk: None,
            chunk_directory: std::cell::OnceCell::new(),
            uncommitted_since_commit: false,
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
        })
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
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        lock.bind_native(&file, &path)?;
        let original_len = file.metadata()?.len();
        let (header_len, header_extensions) = read_file_header_parts(spec, &mut file)?;
        let (matrix, matrix_creation_nonce, matrix_pages) = split_matrix_state(
            read_matrix_layout_if_needed(&path, true, spec, &mut file, header_len, original_len)?,
        );
        let append_start = append_log_start(header_len, matrix.as_ref());
        let mut index = load_generation_index(
            spec,
            &mut file,
            append_start,
            ScanIntent::Recover,
            scratch,
            matrix_pages.as_ref(),
        )?;
        let sequence_state = index.sequence_state();
        let block_tails = index.block_tails()?;
        let frames_verified = index.frames_verified;
        let boundaries = index.boundaries.take();
        let index = &*scratch;
        // No mark here: this route reaches a file that already ended at a
        // commit point, so there is no tail. See the digest's frame check.
        truncate_uncommitted_tail_if_needed(spec, &mut file, append_start, index, false)?;
        let recovered_len = file.metadata()?.len();
        let derived = derive_index_state(index.entries(), false)?;
        let checkpoint_cadence = derived.checkpoint_cadence;
        let segment_cursor = derived.segment_cursor;
        let records_preserved = index.record_count();
        let snapshot = SnapshotFile::from_file_with_len(
            file.try_clone()?,
            matrix_pages
                .as_ref()
                .map_or(file.metadata()?.len(), |p| p.head().native_eof),
        )?;
        Ok((
            Self {
                spec,
                path,
                file: RecordFile::new(file).with_matrix(matrix_pages),
                index: ResidentIndex::adopt_generation(
                    index,
                    snapshot.clone(),
                    spec,
                    frames_verified,
                    boundaries,
                )?,
                snapshot,
                mode: OpenMode::ReadWrite,
                follow_scan: None,
                header_extensions,
                matrix,
                matrix_creation_nonce,
                sequence_state,
                checkpoint_cadence,
                block_tails,
                open_chunk: None,
                chunk_directory: std::cell::OnceCell::new(),
                segment_cursor,
                uncommitted_since_commit: false,
                record_buffer: Vec::new(),
                immediate_state: Default::default(),
                appended_since_digest: false,
                last_resident_block_id: index.last_entry()?.map(|entry| entry.block_id),
                keyed_tails: KeyedTails::new_empty(),
                pending_pathname_parent_sync: false,
                opened_after_crash: false,
                poison: PoisonFlag::healthy(),
                _lock: None,
            },
            RecoveryReport {
                original_len,
                recovered_len,
                records_preserved,
            },
        ))
    }
}
