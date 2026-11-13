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
        validate_mmap_index_entry(entry, file.len()?)?;
        let anchors = self.file.anchor_layout();
        let split = anchors.is_some_and(|layout| {
            crate::file::anchored_range_is_split(&layout, entry.payload_offset, entry.payload_len)
        });
        let (mmap, owned) = if len == 0 {
            (None, None)
        } else if split {
            // An anchor interrupts this payload: hand out a copy instead.
            self.spec
                .read_limits
                .check(ReadLimitKey::MaterializedBytes, entry.payload_len)?;
            let mut bytes = mmap_views::AlignedBytes::zeroed(len)?;
            self.snapshot
                .read_exact_at(entry.payload_offset, bytes.as_bytes_mut())?;
            (None, Some(bytes))
        } else {
            let physical = anchors
                .map_or(Some(entry.payload_offset), |layout| {
                    layout.physical(entry.payload_offset)
                })
                .ok_or(Error::MmapPayloadOutOfBounds {
                    offset: entry.payload_offset,
                    len: entry.payload_len,
                })?;
            // SAFETY: caller promises external immutability; the checked extent
            // belongs to this pinned file and fits the captured generation.
            (
                Some(unsafe {
                    memmap2::MmapOptions::new()
                        .offset(physical)
                        .len(len)
                        .map(file.raw())?
                }),
                None,
            )
        };
        let window = MmapPayloadWindow {
            offset: entry.payload_offset,
            mmap,
            owned,
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
        // The handle's record list when it keeps one, the file walked when it
        // does not: either way every record of the snapshot is mapped.
        let list = self.resident_directory("mmap_payloads")?;
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
        unsafe { self.map_payload_records(&list, 0..list.len(), base, span, mapped_len, mapped_len) }
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
        let list = self.resident_directory("mmap_payload_range")?;
        let total = list.len();
        if records.start > records.end || records.end > total {
            return Err(Error::MmapRecordRangeOutOfBounds {
                start: records.start,
                end: records.end,
                records: total,
            });
        }
        if records.is_empty() {
            // SAFETY: nothing is mapped for an empty range.
            return unsafe { self.map_payload_records(&list, records, 0, 0, 0, 0) };
        }
        let mapped_len = self.snapshot.len();
        let first = list.record_at(records.start)?;
        let last = list.record_at(records.end - 1)?;
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
        unsafe { self.map_payload_records(&list, records, first.record_offset, span, span, end) }
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
        list: &RecordList<'_>,
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
            let entry = list.record_at(at)?;
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
            if captured.max(extent_end) > file.len()? {
                return Err(Error::MmapPayloadOutOfBounds {
                    offset: 0,
                    len: captured,
                });
            }
            Some(file)
        };
        // An anchored file maps the physical extent of the logical one, and
        // keeps an owned copy of each record an anchor interrupts.
        let anchors = self.file.anchor_layout();
        let overflow = || Error::MmapPayloadOutOfBounds {
            offset: base,
            len: span,
        };
        let (physical_base, physical_len) = match anchors {
            None => (base, map_len),
            Some(layout) => {
                let start = layout.physical(base).ok_or_else(overflow)?;
                let end = layout.physical_end(extent_end).ok_or_else(overflow)?;
                (
                    start,
                    usize::try_from(end.saturating_sub(start)).map_err(|_| overflow())?,
                )
            }
        };
        let mut crossing = Vec::new();
        if let Some(layout) = anchors {
            let mut copied = 0u64;
            for entry in &index {
                let end = entry.checked_physical_end()?;
                let len = end - entry.record_offset;
                if !crate::file::anchored_range_is_split(&layout, entry.record_offset, len) {
                    continue;
                }
                copied = copied.saturating_add(len);
                self.spec
                    .read_limits
                    .check(ReadLimitKey::MaterializedBytes, copied)?;
                let mut bytes = mmap_views::AlignedBytes::zeroed(
                    usize::try_from(len).map_err(|_| overflow())?,
                )?;
                self.snapshot
                    .read_exact_at(entry.record_offset, bytes.as_bytes_mut())?;
                crossing.try_reserve(1).map_err(|_| Error::AllocationFailed {
                    resource: "anchored record copies",
                    requested: len,
                })?;
                crossing.push((entry.record_offset, bytes));
            }
        }
        let mmap = if let (false, Some(file)) = (physical_len == 0, file) {
            // SAFETY: the caller guarantees the mapped extent of the cloned
            // backing object stays immutable and valid for the mapping's life.
            Some(unsafe {
                memmap2::MmapOptions::new()
                    .offset(physical_base)
                    .len(physical_len)
                    .map(file.raw())?
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
            anchors,
            physical_base,
            crossing,
        })
    }

    #[cfg(feature = "mmap")]
    /// One committed matrix cell of this handle's generation, checksum
    /// verified, as an owned window (its bytes are copied: a cell's newest
    /// value may still be in the write-ahead ring).
    ///
    /// # Safety
    ///
    /// As for [`Self::mmap_matrix`]; this window itself borrows no mapping.
    pub unsafe fn mmap_matrix_cell<T: VarlitMatrixBlock>(
        &self,
        key: MatrixKey,
    ) -> Result<MmapPayloadWindow> {
        self.spec
            .read_limits
            .check(ReadLimitKey::MmapLen, T::SLOT_STRIDE)?;
        let payload = self.matrix_cell_payload::<T>(key)?;
        let (tiles, source) = self.tiles_read()?;
        let (geom, tile, slot) = tiles
            .slot_of(&source, T::ID, key)?
            .ok_or(Error::MatrixNotCommitted)?;
        let mut owned = AlignedBytes::zeroed(payload.len())?;
        owned.as_bytes_mut().copy_from_slice(&payload);
        Ok(MmapPayloadWindow {
            offset: tile + geom.slot_at(slot),
            mmap: None,
            owned: Some(owned),
        })
    }

    #[cfg(feature = "mmap")]
    /// Maps the file's matrix as a read-only snapshot of this handle's
    /// generation.
    ///
    /// # Safety
    ///
    /// The caller must prevent mutation, truncation, replacement, backing-file
    /// invalidation, and any other modification through every handle, thread,
    /// and process for the full lifetime of the returned mapping. The mapping
    /// remains tied to this already-open backing object even if its path is
    /// renamed or replaced.
    pub unsafe fn mmap_matrix(&self) -> Result<MmapMatrix> {
        let file = self.snapshot.try_clone_file()?;
        let physical_len = file.raw().metadata()?.len();
        // Truncated since this handle captured it: nothing it addresses can
        // be trusted to be there.
        let captured = self
            .file
            .anchor_layout()
            .map_or(Some(self.snapshot.len()), |layout| layout.physical_end(self.snapshot.len()))
            .ok_or(Error::InvalidMatrixLayout)?;
        if captured > physical_len {
            return Err(Error::MmapPayloadOutOfBounds {
                offset: 0,
                len: captured,
            });
        }
        self.spec
            .read_limits
            .check(ReadLimitKey::MmapLen, physical_len)?;
        let (tiles, source) = self.tiles_read()?;
        let entries = tiles.overlay_entries()?;
        let patched: u64 = entries.iter().map(|(_, _, len)| u64::from(*len)).sum();
        self.spec.read_limits.check(
            ReadLimitKey::MaterializedBytes,
            patched.div_ceil(4096).saturating_add(entries.len() as u64) * 4096,
        )?;
        let map_len = usize::try_from(physical_len)
            .map_err(|_| Error::LengthOverflow { value: physical_len })?;
        // SAFETY: the caller guarantees the backing object stays immutable
        // and valid for the mapping's lifetime; the mapping is private, so the
        // patches below never reach the file.
        let mut mapping =
            unsafe { memmap2::MmapOptions::new().len(map_len).map_copy(file.raw())? };
        let layout = self.file.anchor_layout();
        let mut bytes = Vec::new();
        for (target, at, len) in entries {
            bytes.clear();
            bytes.try_reserve_exact(len as usize).map_err(|_| Error::AllocationFailed {
                resource: "matrix mapping patch",
                requested: u64::from(len),
            })?;
            bytes.resize(len as usize, 0);
            crate::matrix_tiles::store::TileSource::read_at(&source, at, &mut bytes)?;
            let mut write = |physical: u64, from: usize, n: usize| -> Result<()> {
                let start = physical as usize;
                mapping
                    .get_mut(start..start + n)
                    .ok_or(Error::MmapPayloadOutOfBounds {
                        offset: physical,
                        len: n as u64,
                    })?
                    .copy_from_slice(&bytes[from..from + n]);
                Ok(())
            };
            match layout {
                None => write(target, 0, len as usize)?,
                Some(layout) => layout.split(target, u64::from(len), |segment| {
                    if let crate::anchor::Segment::Data {
                        physical,
                        logical,
                        len,
                    } = segment
                    {
                        write(physical, (logical - target) as usize, len as usize)?;
                    }
                    Ok(())
                })?,
            }
        }
        let source = MmapSource {
            mmap: mapping.make_read_only()?,
            layout,
        };
        let tiles = crate::matrix_tiles::api::TiledMatrix::open_reader(
            self.spec,
            &source,
            self.matrix_meta_offset()?,
        )?;
        Ok(MmapMatrix {
            spec: self.spec,
            tiles,
            source,
        })
    }
}
