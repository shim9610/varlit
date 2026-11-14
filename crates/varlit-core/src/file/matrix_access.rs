// Native file matrix access operations, over the tiled matrix storage
// (`crate::matrix_tiles`).

impl VarlitFile {
    /// Runs a matrix mutation: refused on a read-only or poisoned handle, and
    /// an I/O failure poisons the writer.
    fn matrix_mutation<R>(
        &mut self,
        op: impl FnOnce(
            &mut crate::matrix_tiles::api::TiledMatrix,
            &mut matrix_tiles_io::FileTiles<'_>,
        ) -> Result<R>,
    ) -> Result<R> {
        self.check_write()?;
        let result = self.with_tiles(op);
        self.finish_matrix_mutation(result)
    }

    /// Encodes `value` into exactly `T::SLOT_STRIDE` bytes.
    fn encode_matrix_slot<T: VarlitMatrixBlock>(&mut self, value: &T) -> Result<Vec<u8>> {
        let mut encoded = std::mem::take(&mut self.record_buffer);
        let result = match crate::codec::encode_into_limited(
            value,
            T::ENDIAN.unwrap_or(self.spec.endian),
            T::SLOT_STRIDE.saturating_add(1),
            "matrix slot payload",
            &mut encoded,
        ) {
            Ok(()) if encoded.len() as u64 == T::SLOT_STRIDE => {
                let mut out = Vec::new();
                out.try_reserve_exact(encoded.len())
                    .map_err(|_| Error::AllocationFailed {
                        resource: "matrix slot payload",
                        requested: encoded.len() as u64,
                    })?;
                out.extend_from_slice(&encoded);
                Ok(out)
            }
            Ok(()) => Err(Error::MatrixSizeMismatch {
                expected: T::SLOT_STRIDE,
                actual: encoded.len() as u64,
            }),
            Err(Error::LimitExceeded { actual, .. }) => Err(Error::MatrixSizeMismatch {
                expected: T::SLOT_STRIDE,
                actual,
            }),
            Err(error) => Err(error),
        };
        self.record_buffer = encoded;
        result
    }

    pub fn write_matrix_cell<T: VarlitMatrixBlock>(
        &mut self,
        key: MatrixKey,
        value: &T,
    ) -> Result<()> {
        crate::matrix::ensure_matrix_block::<T>(self.spec)?;
        self.check_write()?;
        let payload = self.encode_matrix_slot(value)?;
        self.matrix_mutation(|tiles, store| tiles.write_cell(store, T::ID, key, &payload))
    }

    pub fn write_matrix_cell_payload<T: VarlitMatrixBlock>(
        &mut self,
        key: MatrixKey,
        payload: &[u8],
    ) -> Result<()> {
        crate::matrix::ensure_matrix_block::<T>(self.spec)?;
        self.matrix_mutation(|tiles, store| tiles.write_cell(store, T::ID, key, payload))
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
    /// the confirmed generation before retrying. A published generation whose
    /// final durability request failed yields
    /// [`Error::MatrixCommittedButDurabilityUnproven`]. Both skip the hook.
    /// After successful durable publication, hook failure yields
    /// [`Error::MatrixCommittedButHookFailed`]; retry only the notification.
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
    /// barrier; the same post-commit contract applies. Both barrier callbacks
    /// precede publication and receive the primary file.
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
        // Allocate existing public error payloads before any mutation, so a
        // late hook/durability error needs no allocation and Error stays small.
        let mut error_slot = crate::fallible::try_box(Error::InvalidImmediatePolicy)?;
        self.write_matrix_cell(key, value)?;
        let event = {
            let (tiles, source) = self.tiles_read()?;
            let category = tiles.category_of(T::ID)?;
            let (geom, tile, slot) = tiles
                .slot_of(&source, T::ID, key)?
                .ok_or(Error::InvalidMatrixLayout)?;
            MatrixCommitEvent {
                block_id: T::ID,
                category,
                key,
                slot_offset: tile + geom.slot_at(slot),
                slot_len: geom.stride,
            }
        };
        let event = crate::fallible::try_box(event)?;
        // Pre-publication: nothing is visible yet, so a failure here is a
        // plain refusal.
        let sync_data = barrier.sync_matrix_data(self.file.barrier_file());
        self.poison_after_started_matrix_error(sync_data)?;
        self.commit_matrix_cell::<T>(key)?;
        let sync_commit = barrier.sync_matrix_commit(self.file.barrier_file());
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
        hook((*event).clone()).map_err(|source| {
            *error_slot = source;
            Error::MatrixCommittedButHookFailed {
                event,
                source: error_slot,
            }
        })
    }

    /// The committed payload of `key` (exactly `T::SLOT_STRIDE` bytes),
    /// charged against `MaterializedBytes`.
    fn matrix_payload_into<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        crate::matrix::ensure_matrix_block::<T>(self.spec)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::MaterializedBytes, T::SLOT_STRIDE)?;
        let stride = T::SLOT_STRIDE as usize;
        if out.capacity() < stride {
            out.clear();
            out.try_reserve_exact(stride)
                .map_err(|_| Error::AllocationFailed {
                    resource: "matrix cell payload",
                    requested: T::SLOT_STRIDE,
                })?;
        }
        out.resize(stride, 0);
        let (tiles, source) = self.tiles_read()?;
        tiles.read_cell(&source, T::ID, key, out)
    }

    /// Decodes one committed cell into `out`, reusing what it owns. On error
    /// `out` is valid but unspecified.
    pub fn read_matrix_cell<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
        out: &mut T,
    ) -> Result<()> {
        // The slot is staged in the handle's scratch, not a buffer made
        // for this call.
        let mut payload = lend_scratch(&self.record_cache);
        let decoded = (|| {
            self.matrix_payload_into::<T>(key, &mut payload)?;
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
            crate::codec::Decoder::decode_from_slice_limited_into(
                &payload,
                T::ENDIAN.unwrap_or(self.spec.endian),
                decode_limit,
                out,
            )
        })();
        return_scratch(&self.record_cache, payload);
        decoded
    }

    pub fn matrix_cell_payload<T: VarlitMatrixBlock>(&self, key: MatrixKey) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        self.matrix_payload_into::<T>(key, &mut payload)?;
        Ok(payload)
    }

    pub fn matrix_aux_len(&self, name: &str) -> Result<u64> {
        self.tiles_read()?.0.aux_len(name)
    }

    pub fn read_matrix_aux(&self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        self.spec
            .read_limits
            .check(ReadLimitKey::MaterializedBytes, len)?;
        let (tiles, source) = self.tiles_read()?;
        // Range-checked before anything is allocated for it.
        tiles.aux_len(name)?;
        let mut out = Vec::new();
        out.try_reserve_exact(len as usize)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix aux read",
                requested: len,
            })?;
        out.resize(len as usize, 0);
        tiles.read_aux(&source, name, offset, &mut out)?;
        Ok(out)
    }

    pub fn write_matrix_aux(&mut self, name: &str, offset: u64, payload: &[u8]) -> Result<()> {
        self.matrix_mutation(|tiles, store| tiles.write_aux(store, name, offset, payload))
    }

    pub fn matrix_cell_status<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MatrixCellStatus> {
        crate::matrix::ensure_matrix_block::<T>(self.spec)?;
        let (tiles, source) = self.tiles_read()?;
        tiles.cell_status(&source, T::ID, key)
    }

    pub fn commit_matrix_cell<T: VarlitMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        crate::matrix::ensure_matrix_block::<T>(self.spec)?;
        self.matrix_mutation(|tiles, store| tiles.commit_cell(store, T::ID, key))
    }

    pub fn clear_matrix_cell<T: VarlitMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        crate::matrix::ensure_matrix_block::<T>(self.spec)?;
        self.matrix_mutation(|tiles, store| tiles.clear_cell(store, T::ID, key))
    }

    pub fn clear_matrix_cell_by_category(&mut self, category: &str, key: MatrixKey) -> Result<()> {
        self.matrix_mutation(|tiles, store| tiles.clear_cell_by_category(store, category, key))
    }

    /// Clears every commit bit of a category. Published readers keep their
    /// snapshot until they follow.
    pub fn clear_matrix_category(&mut self, category: &str) -> Result<u64> {
        self.matrix_mutation(|tiles, store| tiles.clear_category(store, category))
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

    /// Rebuilds a block's commit bits from its per-cell checksums: every
    /// written cell whose checksum holds is committed, every other one is
    /// not. Visits the tiles that exist. Returns the committed count.
    pub fn rebuild_matrix_commit_from_crc<T: VarlitMatrixBlock>(&mut self) -> Result<u64> {
        crate::matrix::ensure_matrix_block::<T>(self.spec)?;
        self.matrix_mutation(|tiles, store| tiles.rebuild_commit_from_crc(store, T::ID))
    }

    pub fn is_matrix_single_committed(&self, name: &str) -> Result<bool> {
        let (tiles, source) = self.tiles_read()?;
        tiles.is_single_committed(&source, name)
    }

    pub fn set_matrix_single_committed(&mut self, name: &str, value: bool) -> Result<()> {
        self.matrix_mutation(|tiles, store| tiles.set_single_committed(store, name, value))
    }

    pub fn is_matrix_channel_committed(&self, name: &str, channel: u64) -> Result<bool> {
        let (tiles, source) = self.tiles_read()?;
        tiles.is_channel_committed(&source, name, channel)
    }

    pub fn set_matrix_channel_committed(
        &mut self,
        name: &str,
        channel: u64,
        value: bool,
    ) -> Result<()> {
        self.matrix_mutation(|tiles, store| {
            tiles.set_channel_committed(store, name, channel, value)
        })
    }

    /// What a resumed writer should do with this category: `Clean` when none
    /// or all of its cells are committed, `Partial` otherwise.
    pub fn matrix_resume_signal(&self, category: &str) -> Result<MatrixResumeSignal> {
        let (tiles, source) = self.tiles_read()?;
        tiles.resume_signal(&source, category)
    }

    fn matrix_sidecar_signal(
        &self,
        category: &str,
        sidecar_exists: bool,
    ) -> Result<MatrixResumeSignal> {
        let (tiles, source) = self.tiles_read()?;
        let (committed, total) = tiles.progress(&source, category)?;
        let clean = committed == 0 || committed == total;
        Ok(match (clean, sidecar_exists) {
            (true, true) => MatrixResumeSignal::DiscardRecommended,
            (true, false) => MatrixResumeSignal::Clean,
            (false, true) => MatrixResumeSignal::ResumeAvailable { committed, total },
            (false, false) => MatrixResumeSignal::RestartRecommended,
        })
    }

    pub fn matrix_sidecar_resume_signal<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
    ) -> Result<MatrixResumeSignal> {
        self.matrix_sidecar_signal(category, sidecar_path.as_ref().exists())
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
        if !path.exists() {
            return self.matrix_sidecar_signal(category, false);
        }
        let identity = self.matrix_native_identity()?;
        match read_matrix_sidecar_file(self.spec, category, path, expected_generation, identity) {
            Ok(_) => self.matrix_sidecar_signal(category, true),
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
    /// already be durable (native -> sidecar). Publication itself is a
    /// same-directory temp write + fsync + atomic replace + parent sync, so a
    /// crash mid-publish leaves the previous sidecar intact.
    pub fn write_matrix_sidecar<P: AsRef<Path>>(
        &mut self,
        category: &str,
        sidecar_path: P,
        generation: u64,
        payload: &[u8],
    ) -> Result<MatrixSidecarManifest> {
        self.check_write()?;
        // The category must exist.
        self.tiles_read()?
            .0
            .progress(&self.tiles_read()?.1, category)?;
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
        self.tiles_read()?
            .0
            .progress(&self.tiles_read()?.1, category)?;
        let identity = self.matrix_native_identity()?;
        read_matrix_sidecar_file(self.spec, category, sidecar_path, None, identity)
    }

    pub fn read_matrix_sidecar_with_generation<P: AsRef<Path>>(
        &self,
        category: &str,
        sidecar_path: P,
        expected_generation: u64,
    ) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
        self.tiles_read()?
            .0
            .progress(&self.tiles_read()?.1, category)?;
        let identity = self.matrix_native_identity()?;
        read_matrix_sidecar_file(
            self.spec,
            category,
            sidecar_path,
            Some(expected_generation),
            identity,
        )
    }

    /// The native-file identity a matrix sidecar is bound to: the OS object
    /// and schema hash, the matrix's metadata record, its creation nonce.
    fn matrix_native_identity(&self) -> Result<MatrixNativeIdentity> {
        let creation_nonce = self.matrix_nonce()?;
        let file = self.snapshot.try_clone_file()?;
        let fingerprint = native_object_fingerprint(self.spec, file.raw())?;
        Ok(MatrixNativeIdentity {
            fingerprint,
            layout_generation: self.matrix_meta_offset()?,
            creation_nonce,
        })
    }

    /// Per-file storage observations. Tiled storage allocates what it writes
    /// and never depends on sparse files, so there is nothing to report
    /// beyond that.
    pub fn matrix_storage_diagnostics(&self) -> Option<crate::MatrixStorageDiagnostics> {
        self.spec
            .has_matrix_blocks()
            .then(crate::MatrixStorageDiagnostics::not_applicable)
    }

    /// Findings of the last verification. Tiled storage checks each tile's
    /// header and commit bitmap when it reads them and reports damage as an
    /// error of that read, so there is nothing held back to report.
    pub fn matrix_recovery_report(&self) -> MatrixRecoveryReport {
        MatrixRecoveryReport {
            findings: Vec::new(),
            recommended_actions: Vec::new(),
        }
    }

    /// Verifies every tile's header and commit bitmap now. A damaged one is a
    /// `Recoverable` finding recommending its category be cleared.
    pub fn verify_matrix_metadata(&self) -> Result<MatrixRecoveryReport> {
        let (tiles, source) = self.tiles_read()?;
        tiles.verify(&source)
    }
}
