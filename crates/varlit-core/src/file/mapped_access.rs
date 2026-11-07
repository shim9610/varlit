// Native file mapped access operations.

impl VarlitFile {
    /// Maps only the stored payload of a record from this captured generation.
    /// No resident record directory is required. The mapping limit applies to
    /// this payload, not to the file or the sum of its record lengths. Compressed
    /// records expose their stored encoding, not decompressed bytes.
    ///
    /// # Safety
    /// The caller must prevent mutation, truncation and backing-file invalidation
    /// of this record for the entire returned window's lifetime, through all
    /// handles, threads and processes. The same external immutability contract
    /// as `mmap_payloads` applies; this API adds no writer lock.
    /// Appending strictly after this immutable record is permitted: the window
    /// has a fixed length and excludes both the file header and later records.
    /// The caller must still exclude truncation, replacement, compaction and
    /// every operation that could rewrite this record or invalidate its storage.
    #[cfg(feature = "mmap")]
    pub unsafe fn mmap_payload_window(
        &self,
        entry: &RecordIndexEntry,
    ) -> Result<MmapPayloadWindow> {
        if entry.record_offset < append_log_start_for_file(self)? {
            return Err(Error::MmapEntryNotInSnapshot);
        }
        validate_mmap_index_entry(entry, self.snapshot.len())?;
        if self
            .spec
            .read_limits
            .read_validation
            .is_none_or(|p| p.structure)
        {
            let actual = fault_record_entry(
                &self.snapshot,
                self.spec,
                entry.record_offset,
                entry.committed,
            )?;
            if actual != *entry {
                return Err(Error::MmapEntryNotInSnapshot);
            }
        }
        self.spec
            .read_limits
            .check(ReadLimitKey::MmapLen, entry.payload_len)?;
        let len = usize::try_from(entry.payload_len).map_err(|_| Error::LengthOverflow {
            value: entry.payload_len,
        })?;
        let file = self.snapshot.try_clone_file()?;
        validate_mmap_index_entry(entry, file.metadata()?.len())?;
        let mmap = if len == 0 {
            None
        } else {
            // SAFETY: caller promises external immutability; the checked extent
            // belongs to this pinned file and fits the captured generation.
            Some(unsafe {
                memmap2::MmapOptions::new()
                    .offset(entry.payload_offset)
                    .len(len)
                    .map(&file)?
            })
        };
        let window = MmapPayloadWindow {
            offset: entry.payload_offset,
            mmap,
        };
        entry.verify_snapshot_record(self.spec, &self.snapshot, window.as_bytes())?;
        Ok(window)
    }

    #[cfg(feature = "mmap")]
    /// Maps the file's indexed record payloads as a read-only snapshot.
    ///
    /// # Safety
    ///
    /// The caller must keep the captured mapped record region immutable and
    /// valid through every handle, thread and process for the full lifetime of
    /// the returned mapping. Truncation, replacement of the backing object and
    /// operations that rewrite any captured record must be excluded. The mapping
    /// remains tied to this already-open backing object even if its path is
    /// renamed or replaced.
    /// Appending strictly beyond the captured region and updating unmapped
    /// header bytes are permitted while the captured records remain unchanged.
    ///
    /// Once that condition holds, Varlit keeps safe accessors sound by creating
    /// a read-only mapping from a clone of the existing handle, validating every
    /// indexed extent with checked arithmetic, rejecting entries outside the
    /// copied snapshot, and tying every returned slice to the mapping owner.
    /// Use owned reads instead when external immutability cannot be guaranteed.
    ///
    /// **The file header is not mapped.** The mapping begins at the start of
    /// the append log, so the bytes a commit can rewrite — the
    /// `IndexPolicy::header_tails` region among them — are outside every
    /// reference this type constructs. The condition above is therefore about
    /// the *records* — and a `replace_*` is **not** the only thing that rewrites
    /// one. On a format declaring `with_growing_matrix_dimension`, any
    /// `flush`, `commit`, `sync` or drop that writes an already-backed chunk
    /// goes through `rewrite_chunk_record`, which overwrites that record's
    /// header and payload where they sit — inside this mapping. A writer of
    /// such a format has to be excluded for the mapping's lifetime like any
    /// other; "I call no `replace_*`" is not enough.
    pub unsafe fn mmap_payloads(&self) -> Result<MmapPayloads> {
        // The index this maps is the *resident* one, and a lazily opened handle
        // deliberately has none. Without this the answer was `Ok` with zero
        // entries — `is_empty()` true, every window `None`, no error anywhere.
        // Measured on the fixture that pins it, which holds sixty-five records;
        // the count is not what makes it wrong, but it is what was measured.
        // Refusing by name is what every other directory-needing read does, and
        // it is the difference between "this handle cannot answer that" and
        // "there is nothing here".
        let _ = self.resident_directory("mmap_payloads")?;
        let mapped_len = self.snapshot.len();
        // `MmapLen` alone. `FileLen` was checked here too, against the same
        // value — the mapping's length is the snapshot's length — so it bounded
        // nothing the line below does not, and it made a caller who declared a
        // mapping ceiling also have to declare a file ceiling to use it.
        self.spec
            .read_limits
            .check(ReadLimitKey::MmapLen, mapped_len)?;
        let record_count =
            u64::try_from(self.index.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Records, record_count)?;
        let verify = self.spec.read_validation().data_crc || self.spec.read_validation().structure;
        let mmap_index_bytes = mmap_index_bytes_for_count(self.index.len(), verify)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::IndexBytes, mmap_index_bytes)?;

        let file = self.snapshot.try_clone_file()?;
        let current_len = file.metadata()?.len();
        if mapped_len > current_len {
            return Err(Error::MmapPayloadOutOfBounds {
                offset: 0,
                len: mapped_len,
            });
        }
        // Start the mapping at the append log, not at byte 0: see
        // `MmapPayloads::base`. Every payload this type hands out lives at or
        // after this offset by construction, so nothing is lost.
        let (base, mapped_span) = match mapped_len.checked_sub(append_log_start_for_file(self)?) {
            Some(span) if span > 0 => (append_log_start_for_file(self)?, span),
            // The file holds no records. No accessor can produce a window, so
            // the mapping is never dereferenced and nothing constructs a
            // reference over the header — mapping it is sound, and it avoids
            // asking the OS for a zero-length mapping.
            _ => {
                debug_assert_eq!(self.index.len(), 0);
                (0, mapped_len)
            }
        };
        let map_len = usize::try_from(mapped_span)
            .map_err(|_| Error::LengthOverflow { value: mapped_span })?;
        // SAFETY: The caller guarantees that the cloned backing object remains
        // immutable and valid for the mapping's entire lifetime.
        let mmap = unsafe {
            memmap2::MmapOptions::new()
                .offset(base)
                .len(map_len)
                .map(&file)?
        };
        let mut index = Vec::new();
        index
            .try_reserve_exact(self.index.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "mmap index",
                requested: mmap_index_bytes,
            })?;
        let mut by_block: HashMap<u32, Vec<usize>> = HashMap::new();
        let mut verified = Vec::new();
        if verify {
            verified
                .try_reserve_exact(self.index.len())
                .map_err(|_| Error::AllocationFailed {
                    resource: "mmap integrity cache",
                    requested: self.index.len() as u64,
                })?;
            verified.resize_with(self.index.len(), || {
                std::sync::atomic::AtomicBool::new(false)
            });
        }
        by_block
            .try_reserve(
                self.index
                    .len()
                    .min(self.spec.blocks.len().saturating_add(6)),
            )
            .map_err(|_| Error::AllocationFailed {
                resource: "mmap block lookup",
                requested: mmap_index_bytes,
            })?;
        // **One pass, and it is now the only one.** The bounds validation, the
        // copy, the per-block position lists and the ordering witness were four
        // separate walks of the same entries. The ordering check in particular
        // was a `windows(2)` over the finished copy, which is a whole extra
        // traversal in debug builds to learn something each step already knows.
        //
        // What moved, and what it costs: validation used to run before the
        // mapping was taken. It now runs after, so a file with an out-of-bounds
        // entry pays one `mmap`/`munmap` before its refusal — the mapping is
        // dropped by the `?` and nothing has read through it, since
        // `MmapPayloads` is not constructed until the loop finishes. That is
        // the whole price, and it buys one traversal instead of two on the
        // path that succeeds. `mapped_len <= current_len` is still checked
        // before the map, so the map itself is never oversized.
        //
        // Records occupy contiguous ascending physical extents, so the copied
        // index is strictly offset-ordered; `payload_window` relies on that for
        // its binary-search membership check.
        let mut previous_offset: Option<u64> = None;
        for (position, entry) in self.index.iter().enumerate() {
            let entry = entry?;
            validate_mmap_index_entry(&entry, mapped_len)?;
            debug_assert!(
                previous_offset.is_none_or(|previous| previous < entry.record_offset),
                "resident index must be strictly ordered by record offset",
            );
            previous_offset = Some(entry.record_offset);
            let block_id = entry.block_id;
            index.push(entry);
            let positions = by_block.entry(block_id).or_default();
            positions
                .try_reserve(1)
                .map_err(|_| Error::AllocationFailed {
                    resource: "mmap block positions",
                    requested: mmap_index_bytes,
                })?;
            positions.push(position);
        }
        Ok(MmapPayloads {
            spec: self.spec,
            base,
            index,
            by_block,
            verified,
            mmap,
        })
    }

    #[cfg(feature = "mmap")]
    /// Maps one committed matrix cell from this handle's generation.
    ///
    /// Charges `MmapLen` for the cell, and `MaterializedBytes` for its COW
    /// pages only. Bitmap and optional CRC validation use the same generation
    /// as ordinary cell reads. No complete matrix mapping or copied layout is
    /// needed. COW payload pages are copied into this private mapping; this is
    /// not a zero-copy promise for modified cells.
    ///
    /// # Safety
    ///
    /// The caller must keep the underlying file object immutable and valid
    /// for the entire mapping lifetime, as for [`Self::mmap_matrix`].
    pub unsafe fn mmap_matrix_cell<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MmapPayloadWindow> {
        let layout = self.matrix.as_ref().ok_or(Error::MatrixLayoutMissing)?;
        let pages = self.file.matrix_pages().ok_or(Error::MatrixLayoutMissing)?;
        let (offset, len, cell_crc) = crate::matrix::cell_payload_parts::<T>(
            self.spec,
            layout,
            key,
            crate::matrix::BitmapRead::new(pages),
        )?;
        self.spec.read_limits.check(ReadLimitKey::MmapLen, len)?;
        let changed = pages.mapped_window_pages(offset, len)?;
        self.spec.read_limits.check(
            ReadLimitKey::MaterializedBytes,
            changed
                .checked_mul(4096)
                .ok_or(Error::InvalidMatrixLayout)?,
        )?;
        let file = self.snapshot.try_clone_file()?;
        validate_mmap_range(offset, len, self.snapshot.len())?;
        validate_mmap_range(offset, len, file.metadata()?.len())?;
        let mmap = if len == 0 {
            None
        } else {
            let map_len = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
            // SAFETY: the caller guarantees the base file's immutability and
            // lifetime; all COW patches are applied before exposing any slice.
            let mut mapping = unsafe {
                memmap2::MmapOptions::new()
                    .offset(offset)
                    .len(map_len)
                    .map_copy(&file)?
            };
            pages.patch_mapping_window(offset, &mut mapping)?;
            Some(mapping.make_read_only()?)
        };
        let window = MmapPayloadWindow { offset, mmap };
        if let Some(cell_crc) = cell_crc {
            let mut crc = [0; 4];
            let mut source = pages.read_borrow();
            source.seek(SeekFrom::Start(cell_crc.offset))?;
            source.read_exact(&mut crc)?;
            cell_crc.verify(
                window.as_bytes(),
                u32::from_le_bytes(crc),
                crate::matrix::BitmapRead::new(pages),
            )?;
        }
        Ok(window)
    }

    #[cfg(feature = "mmap")]
    /// Maps the file's matrix regions as a read-only snapshot.
    ///
    /// # Safety
    ///
    /// The caller must prevent mutation, truncation, replacement, backing-file
    /// invalidation, and any other modification through every handle, thread,
    /// and process for the full lifetime of the returned mapping. The mapping
    /// remains tied to this already-open backing object even if its path is
    /// renamed or replaced.
    ///
    /// Once that condition holds, Varlit keeps safe accessors sound by creating
    /// a read-only mapping from a clone of the existing handle, validating the
    /// complete matrix extent, checking every requested slot, and tying every
    /// returned slice to the mapping owner. Use owned reads instead when
    /// external immutability cannot be guaranteed.
    pub unsafe fn mmap_matrix(&self) -> Result<MmapMatrix> {
        let layout = self
            .matrix
            .as_ref()
            .ok_or(Error::MatrixLayoutMissing)?
            .try_clone()?;
        let mapped_len = self.snapshot.len();
        // `MmapLen` alone. `FileLen` was checked here too, against the same
        // value — the mapping's length is the snapshot's length — so it bounded
        // nothing the line below does not, and it made a caller who declared a
        // mapping ceiling also have to declare a file ceiling to use it.
        self.spec
            .read_limits
            .check(ReadLimitKey::MmapLen, mapped_len)?;
        let record_count =
            u64::try_from(self.index.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Records, record_count)?;
        self.spec.read_limits.check(
            ReadLimitKey::IndexBytes,
            index_bytes_for_count(self.index.len())?,
        )?;
        let file = self.snapshot.try_clone_file()?;
        let current_len = file.metadata()?.len();
        if mapped_len > current_len {
            return Err(Error::MmapPayloadOutOfBounds {
                offset: 0,
                len: mapped_len,
            });
        }
        validate_mmap_range(0, layout.append_log_start(), current_len)?;
        let map_len =
            usize::try_from(mapped_len).map_err(|_| Error::LengthOverflow { value: mapped_len })?;
        // SAFETY: The caller guarantees that the cloned backing object remains
        // immutable and valid for the mapping's entire lifetime.
        let pages = self.file.matrix_pages().ok_or(Error::MatrixLayoutMissing)?;
        let changed = pages.mapped_page_count()?;
        self.spec.read_limits.check(
            ReadLimitKey::MaterializedBytes,
            changed
                .checked_mul(4096)
                .ok_or(Error::InvalidMatrixLayout)?,
        )?;
        let mut mapping = unsafe { memmap2::MmapOptions::new().len(map_len).map_copy(&file)? };
        pages.patch_mapping(&mut mapping)?;
        let mmap = mapping.make_read_only()?;
        validate_mmap_range(0, layout.append_log_start(), mapped_len)?;
        Ok(MmapMatrix {
            spec: self.spec,
            layout,
            mmap,
        })
    }
}
