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
        // Refusing by name is what every other directory-needing read does, and
        // it is the difference between "this handle cannot answer that" and
        // "there is nothing here".
        let _ = self.resident_directory("mmap_payloads")?;
        let mapped_len = self.snapshot.len();
        // Start the mapping at the append log, not at byte 0: see
        // `MmapPayloads::base`. Every payload this type hands out lives at or
        // after this offset by construction, so nothing is lost.
        let base = append_log_start_for_file(self)?;
        let span = mapped_len.saturating_sub(base);
        // SAFETY: forwarded verbatim; the caller upholds this function's
        // contract for the whole captured region.
        // The whole-file form charges and requires the captured snapshot's
        // length, as it always has: a truncated file is refused even when it
        // holds no record to map.
        unsafe { self.map_payload_records(0..self.index.len(), base, span, mapped_len, mapped_len) }
    }

    #[cfg(feature = "mmap")]
    /// Maps only the records at resident index positions `records`, from the
    /// first one's header to the last one's end.
    ///
    /// [`Self::mmap_payloads`] maps every record of the generation and copies
    /// the whole resident index; this maps and copies only the range asked
    /// for. A reader following a growing file maps
    /// `old_count..file.record_count()?`.
    ///
    /// The result is an ordinary [`MmapPayloads`]: every accessor behaves as
    /// on a whole-file mapping restricted to these records. Positions are
    /// relative to the range: `index_entries()[0]` is record `records.start`,
    /// and `block_payload_window::<T>(i)` is the `i`-th record of `T` *within
    /// the range*. An entry outside the range is refused with
    /// [`Error::MmapEntryNotInSnapshot`]. `MmapLen` is charged for the
    /// mapped span, `Records` and `IndexBytes` for the copied entries. An
    /// empty range maps nothing and is not an error.
    ///
    /// # Safety
    ///
    /// The same contract as [`Self::mmap_payloads`], for the mapped records:
    /// the caller must keep them immutable and their backing object valid
    /// through every handle, thread and process for the full lifetime of the
    /// returned mapping. Appending after the range is permitted; records
    /// outside it are not mapped and not covered by this condition.
    pub unsafe fn mmap_payload_range(
        &self,
        records: std::ops::Range<usize>,
    ) -> Result<MmapPayloads> {
        let _ = self.resident_directory("mmap_payload_range")?;
        let total = self.index.len();
        if records.start > records.end || records.end > total {
            return Err(Error::MmapRecordRangeOutOfBounds {
                start: records.start,
                end: records.end,
                records: total,
            });
        }
        if records.is_empty() {
            // SAFETY: nothing is mapped for an empty range.
            return unsafe { self.map_payload_records(records, 0, 0, 0, 0) };
        }
        let mapped_len = self.snapshot.len();
        let first = self.index.entry_at(records.start)?;
        let last = self.index.entry_at(records.end - 1)?;
        validate_mmap_index_entry(&first, mapped_len)?;
        validate_mmap_index_entry(&last, mapped_len)?;
        let span = last
            .checked_physical_end()?
            .checked_sub(first.record_offset)
            .ok_or(Error::MmapPayloadOutOfBounds {
                offset: first.record_offset,
                len: u64::MAX,
            })?;
        // SAFETY: forwarded verbatim; the caller upholds this function's
        // contract for the mapped records.
        let end = first.record_offset.saturating_add(span);
        unsafe { self.map_payload_records(records, first.record_offset, span, span, end) }
    }

    /// Validates and copies the resident entries at `records`, then maps
    /// `span` bytes from file offset `base`, which must cover every one of
    /// them. `MmapLen` is charged for `charged` — the snapshot length for the
    /// whole-file form, the mapped span for a range — and the backing file
    /// must still be at least `captured` long (0: nothing to check, nothing
    /// mapped). Every limit is checked and every entry validated before the OS
    /// is asked for the mapping.
    ///
    /// # Safety
    ///
    /// The caller's mapping contract covers `base..base + span`.
    #[cfg(feature = "mmap")]
    unsafe fn map_payload_records(
        &self,
        records: std::ops::Range<usize>,
        base: u64,
        span: u64,
        charged: u64,
        captured: u64,
    ) -> Result<MmapPayloads> {
        let count = records.len();
        // `MmapLen` is charged for the captured snapshot (`mmap_payloads`) or
        // the range's extent (`mmap_payload_range`).
        self.spec.read_limits.check(ReadLimitKey::MmapLen, charged)?;
        let record_count = u64::try_from(count).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "record count",
        })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Records, record_count)?;
        let verify = self.spec.read_validation().data_crc || self.spec.read_validation().structure;
        let mmap_index_bytes = mmap_index_bytes_for_count(count, verify)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::IndexBytes, mmap_index_bytes)?;
        let extent_end = base.checked_add(span).ok_or(Error::MmapPayloadOutOfBounds {
            offset: base,
            len: span,
        })?;
        if extent_end > self.snapshot.len() {
            return Err(Error::MmapPayloadOutOfBounds {
                offset: base,
                len: span,
            });
        }
        let map_len =
            usize::try_from(span).map_err(|_| Error::LengthOverflow { value: span })?;

        let mut index = Vec::new();
        index
            .try_reserve_exact(count)
            .map_err(|_| Error::AllocationFailed {
                resource: "mmap index",
                requested: mmap_index_bytes,
            })?;
        let mut by_block: HashMap<u32, Vec<usize>> = HashMap::new();
        let mut verified = Vec::new();
        if verify {
            verified
                .try_reserve_exact(count)
                .map_err(|_| Error::AllocationFailed {
                    resource: "mmap integrity cache",
                    requested: record_count,
                })?;
            verified.resize_with(count, || std::sync::atomic::AtomicBool::new(false));
        }
        by_block
            .try_reserve(count.min(self.spec.blocks.len().saturating_add(6)))
            .map_err(|_| Error::AllocationFailed {
                resource: "mmap block lookup",
                requested: mmap_index_bytes,
            })?;
        // One pass validates, copies and groups the entries. Records occupy
        // contiguous ascending physical extents, so the copy is strictly
        // offset-ordered; `payload_window` relies on that for its
        // binary-search membership check.
        let mut previous_offset: Option<u64> = None;
        for (position, at) in records.enumerate() {
            let entry = self.index.entry_at(at)?;
            if entry.record_offset < base {
                return Err(Error::MmapPayloadOutOfBounds {
                    offset: entry.record_offset,
                    len: RECORD_HEADER_LEN,
                });
            }
            validate_mmap_index_entry(&entry, extent_end)?;
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

        let file = if captured == 0 {
            None
        } else {
            let file = self.snapshot.try_clone_file()?;
            if captured.max(extent_end) > file.metadata()?.len() {
                return Err(Error::MmapPayloadOutOfBounds {
                    offset: 0,
                    len: captured,
                });
            }
            Some(file)
        };
        let mmap = if let (false, Some(file)) = (map_len == 0, file) {
            // SAFETY: the caller guarantees the mapped extent of the cloned
            // backing object stays immutable and valid for the mapping's life.
            Some(unsafe {
                memmap2::MmapOptions::new()
                    .offset(base)
                    .len(map_len)
                    .map(&file)?
            })
        } else {
            // No record lies in an empty extent, so no accessor can produce a
            // window; nothing is mapped.
            None
        };
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
