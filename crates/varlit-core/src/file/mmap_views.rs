//! Memory-mapped owned views and bounds checks.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "mmap")]
/// A read-only mapping paired with an immutable snapshot of the validated
/// append-log index.
///
/// Varlit constructs this type only after mapping the already-open backing
/// object, validating every record extent against the mapped length, and
/// copying the index into the owner. Safe window methods accept only entries in
/// that copied snapshot and use checked slice bounds. The mapping owns its OS
/// mapping, so returned slices cannot outlive it.
///
/// These guarantees rely on the caller having upheld the unsafe constructor's
/// requirement that the captured mapped bytes stay immutable and the backing
/// object stays valid for this value's full lifetime. Later appends outside the
/// captured region do not enlarge or alter this snapshot.
#[derive(Debug)]
pub struct MmapPayloads {
    pub(super) spec: FormatSpec,
    /// The file offset the mapping starts at, which is the start of the append
    /// log rather than byte 0.
    ///
    /// **The file header is deliberately outside the mapping.** Every accessor
    /// slices through `Deref`, which materialises a `&[u8]` over the *whole*
    /// mapping before taking a window, so a mapping that began at byte 0 would
    /// mean a live shared reference covering the header for as long as any
    /// window is held — and `IndexPolicy::header_tails` rewrites part of that
    /// header at every commit point. Moving the base is what removes those
    /// bytes from every reference Rust constructs here; the OS still maps the
    /// page they sit in, and that is not the part that matters.
    pub(super) base: u64,
    // Sorted by `record_offset` (append-log order), so snapshot membership is
    // a binary search on this one copy instead of a second full HashSet copy
    // of every entry.
    pub(super) index: Vec<RecordIndexEntry>,
    pub(super) by_block: HashMap<u32, Vec<usize>>,
    // One byte per record only when an optional read check is selected. The mapping is
    // immutable by contract, so a successful check remains valid. Relaxed
    // atomics retain Send + Sync without locks or publishing mutable data;
    // racing first readers may both verify the same immutable bytes.
    pub(super) verified: Vec<AtomicBool>,
    /// `None` when no record is mapped (an empty file or an empty range).
    pub(super) mmap: Option<memmap2::Mmap>,
    /// The file's anchors, and the physical offset `base` maps to. Offsets
    /// stay logical everywhere above `window`.
    pub(super) anchors: Option<crate::anchor::AnchorLayout>,
    pub(super) physical_base: u64,
    /// Owned copies of the records whose bytes an anchor interrupts, sorted
    /// by record offset: `(record offset, bytes, len)`. A window inside such a
    /// record is served from here instead of the mapping.
    pub(super) crossing: Vec<(u64, AlignedBytes)>,
}

/// Bytes held 16-byte aligned, so a raw view of an owned copy is as aligned
/// as one of the mapping it stands in for.
#[cfg(feature = "mmap")]
#[derive(Debug)]
pub(crate) struct AlignedBytes {
    words: Box<[u128]>,
    len: usize,
}

#[cfg(feature = "mmap")]
impl AlignedBytes {
    pub(crate) fn zeroed(len: usize) -> Result<Self> {
        let words = len.div_ceil(16);
        let mut storage = Vec::new();
        storage
            .try_reserve_exact(words)
            .map_err(|_| Error::AllocationFailed {
                resource: "anchored record copy",
                requested: len as u64,
            })?;
        storage.resize(words, 0u128);
        Ok(Self {
            words: storage.into_boxed_slice(),
            len,
        })
    }
    pub(crate) fn as_bytes(&self) -> &[u8] {
        // SAFETY: `u128` has no padding; the slice covers `len <= 16 * words`
        // initialized bytes of the boxed storage.
        unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), self.len) }
    }
    pub(crate) fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, uniquely borrowed.
        unsafe { std::slice::from_raw_parts_mut(self.words.as_mut_ptr().cast::<u8>(), self.len) }
    }
}

/// One record payload or matrix cell mapped independently of the file's total size.
/// Owns its mapping; dropping another window never invalidates this one.
#[cfg(feature = "mmap")]
#[derive(Debug)]
pub struct MmapPayloadWindow {
    pub(super) offset: u64,
    pub(super) mmap: Option<memmap2::Mmap>,
    /// The payload as an owned copy when an anchor interrupts it.
    pub(super) owned: Option<AlignedBytes>,
}
#[cfg(feature = "mmap")]
impl MmapPayloadWindow {
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn as_bytes(&self) -> &[u8] {
        match &self.owned {
            Some(owned) => owned.as_bytes(),
            None => self.mmap.as_deref().unwrap_or(&[]),
        }
    }
}

#[cfg(feature = "mmap")]
impl MmapPayloads {
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn index_entries(&self) -> &[RecordIndexEntry] {
        &self.index
    }

    pub fn payload_window(&self, entry: &RecordIndexEntry) -> Result<&[u8]> {
        // Entries occupy disjoint physical extents, so `record_offset` is
        // unique within the snapshot: binary search finds the only candidate
        // and the full equality compare keeps the exact-match contract that
        // the former per-entry hash-set copy provided.
        let position = self
            .index
            .binary_search_by_key(&entry.record_offset, |candidate| candidate.record_offset)
            .map_err(|_| Error::MmapEntryNotInSnapshot)?;
        if self.index[position] != *entry {
            return Err(Error::MmapEntryNotInSnapshot);
        }
        self.payload_window_at(position)
    }

    pub fn block_payload_window<T: VarlitBlock>(
        &self,
        block_index: usize,
    ) -> Result<Option<&[u8]>> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        let Some(position) = self
            .by_block
            .get(&T::ID)
            .and_then(|positions| positions.get(block_index))
            .copied()
        else {
            return Ok(None);
        };
        let entry = &self.index[position];
        if entry.block_version != T::VERSION {
            return Err(Error::BlockVersionMismatch {
                block_id: T::ID,
                expected: T::VERSION,
                actual: entry.block_version,
            });
        }
        Ok(Some(self.payload_window_at(position)?))
    }

    /// Decodes a record of `T` into `out` straight from the mapping.
    ///
    /// The mapped-snapshot counterpart of [`VarlitFile::decode_block_into`]:
    /// the same block id/version refusal, the same payload checks
    /// ([`Self::payload_window`]) and the same per-record materialization
    /// charge, but an uncompressed payload is decoded where it lies — there is
    /// no copy into a buffer. `scratch` is used only for a compressed record,
    /// whose mapped bytes are decompressed into it. On error `out` is valid but
    /// unspecified.
    pub fn decode_block_into<T: VarlitBlock>(
        &self,
        entry: &RecordIndexEntry,
        scratch: &mut Vec<u8>,
        out: &mut T,
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
        self.spec
            .read_limits
            .check(ReadLimitKey::RecordPayloadLen, entry.payload_len)?;
        let payload = self.payload_window(entry)?;
        let endian = T::ENDIAN.unwrap_or(self.spec.endian);
        let mut budget = crate::collections::MaterializationBudget::new(self.spec);
        if !entry.is_compressed() {
            self.spec
                .read_limits
                .check(ReadLimitKey::LogicalPayloadLen, entry.payload_len)?;
            budget.consume(entry.payload_len)?;
            return budget.decode_into(payload, endian, out);
        }
        // Admit the declared decompressed length before allocating for it,
        // exactly as the copying read does.
        let declared = compressed_logical_len(self.spec, entry, payload)?;
        self.spec
            .read_limits
            .check(ReadLimitKey::LogicalPayloadLen, declared)?;
        budget.consume(declared)?;
        let mut physical = std::mem::take(scratch);
        physical.clear();
        physical
            .try_reserve_exact(payload.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "mapped compressed payload",
                requested: entry.payload_len,
            })?;
        physical.extend_from_slice(payload);
        // `decode_record_payload` refuses output that differs from `declared`.
        let logical = decode_record_payload(self.spec, entry, physical)?;
        let result = budget.decode_into(&logical, endian, out);
        *scratch = logical;
        result
    }

    #[inline]
    fn payload_window_at(&self, position: usize) -> Result<&[u8]> {
        let entry = &self.index[position];
        let payload = self.window(entry.payload_offset, entry.payload_len)?;
        if let Some(verified) = self.verified.get(position)
            && !verified.load(Ordering::Relaxed)
        {
            self.verify_payload(entry, payload)?;
            verified.store(true, Ordering::Relaxed);
        }
        Ok(payload)
    }

    // Keep framing decode and CRC scratch off the already-verified read path.
    #[cold]
    fn verify_payload(&self, entry: &RecordIndexEntry, payload: &[u8]) -> Result<()> {
        if !self.spec.read_validation().structure {
            return entry.verify_captured_payload(self.spec, payload);
        }
        let header = self.window(entry.record_offset, RECORD_HEADER_LEN)?;
        let footer = match entry.footer_offset {
            Some(offset) => self.window(offset, RECORD_FOOTER_LEN)?,
            None => &[],
        };
        entry.verify_record_bytes(self.spec, header, payload, footer)
    }

    #[cfg(feature = "zero-copy")]
    /// Returns a raw fixed block reference directly backed by the mmap snapshot.
    ///
    /// # Safety
    ///
    /// The caller must ensure the mapped file bytes are not mutated for the
    /// lifetime of the returned reference, including by other handles,
    /// threads, or processes. The block's `VarlitRawFixedBlock` implementation
    /// must also match the actual payload layout.
    pub unsafe fn raw_fixed<T: crate::VarlitRawFixedBlock>(
        &self,
        block_index: usize,
    ) -> Result<Option<&T>> {
        crate::collections::ensure_registered_block::<T>(self.spec)?;
        if T::KIND != BlockKind::Fixed {
            return Err(Error::ZeroCopyBlockKindMismatch { actual: T::KIND });
        }
        let expected_endian = T::ENDIAN.unwrap_or(self.spec.endian);
        if T::RAW_ENDIAN != expected_endian {
            return Err(Error::ZeroCopyEndianMismatch {
                expected: expected_endian,
                actual: T::RAW_ENDIAN,
            });
        }

        let Some(bytes) = self.block_payload_window::<T>(block_index)? else {
            return Ok(None);
        };
        let expected_len = std::mem::size_of::<T>();
        if bytes.len() != expected_len {
            return Err(Error::ZeroCopyPayloadSizeMismatch {
                expected: expected_len,
                actual: bytes.len() as u64,
            });
        }
        let required = std::mem::align_of::<T>();
        let address = bytes.as_ptr() as usize;
        if required > 1 && !address.is_multiple_of(required) {
            return Err(Error::ZeroCopyAlignmentMismatch { required, address });
        }
        T::ref_from_bytes(bytes)
            .map(Some)
            .map_err(|_| Error::ZeroCopyPayloadSizeMismatch {
                expected: expected_len,
                actual: bytes.len() as u64,
            })
    }

    #[inline]
    fn window(&self, offset: u64, len: u64) -> Result<&[u8]> {
        let mapped_offset = match self.anchors {
            None => offset,
            Some(layout) => {
                if crate::file::anchored_range_is_split(&layout, offset, len) {
                    return self.crossing_window(offset, len);
                }
                // The mapping starts at `base`'s physical offset; map this
                // offset to the same frame.
                layout
                    .physical(offset)
                    .and_then(|physical| physical.checked_sub(self.physical_base))
                    .and_then(|delta| delta.checked_add(self.base))
                    .ok_or(Error::MmapPayloadOutOfBounds { offset, len })?
            }
        };
        // File offsets are absolute; the mapping starts at `base`.
        let start = mapped_offset
            .checked_sub(self.base)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or(Error::MmapPayloadOutOfBounds { offset, len })?;
        let len =
            usize::try_from(len).map_err(|_| Error::MmapPayloadOutOfBounds { offset, len })?;
        let end = start
            .checked_add(len)
            .ok_or(Error::MmapPayloadOutOfBounds {
                offset,
                len: len as u64,
            })?;
        self.mmap
            .as_deref()
            .unwrap_or(&[])
            .get(start..end)
            .ok_or(Error::MmapPayloadOutOfBounds {
                offset,
                len: len as u64,
            })
    }
}

#[cfg(feature = "mmap")]
impl MmapPayloads {
    /// A window inside a record an anchor interrupts, from its owned copy.
    #[cold]
    fn crossing_window(&self, offset: u64, len: u64) -> Result<&[u8]> {
        let out = || Error::MmapPayloadOutOfBounds { offset, len };
        let at = self
            .crossing
            .partition_point(|(start, _)| *start <= offset)
            .checked_sub(1)
            .ok_or_else(out)?;
        let (start, bytes) = &self.crossing[at];
        let from = usize::try_from(offset - start).map_err(|_| out())?;
        let to = from
            .checked_add(usize::try_from(len).map_err(|_| out())?)
            .ok_or_else(out)?;
        bytes.as_bytes().get(from..to).ok_or_else(out)
    }
}

#[cfg(feature = "mmap")]
/// A private copy-on-write mapping of the primary file, with the logical
/// offsets the tile engine uses translated around anchors.
#[derive(Debug)]
pub(crate) struct MmapSource {
    pub(super) mmap: memmap2::Mmap,
    pub(super) layout: Option<crate::anchor::AnchorLayout>,
}

#[cfg(feature = "mmap")]
impl MmapSource {
    /// The physical range of logical `[offset, offset + len)` when no anchor
    /// interrupts it.
    pub(super) fn contiguous(&self, offset: u64, len: u64) -> Result<&[u8]> {
        let physical = match self.layout {
            None => offset,
            Some(layout) => {
                let start = layout.physical(offset).ok_or(Error::InvalidMatrixLayout)?;
                if len != 0 {
                    let last = layout
                        .physical(offset + len - 1)
                        .ok_or(Error::InvalidMatrixLayout)?;
                    if last != start + len - 1 {
                        return Err(Error::MmapPayloadOutOfBounds { offset, len });
                    }
                }
                start
            }
        };
        let start =
            usize::try_from(physical).map_err(|_| Error::MmapPayloadOutOfBounds { offset, len })?;
        let end = start
            .checked_add(
                usize::try_from(len).map_err(|_| Error::MmapPayloadOutOfBounds { offset, len })?,
            )
            .ok_or(Error::MmapPayloadOutOfBounds { offset, len })?;
        self.mmap
            .get(start..end)
            .ok_or(Error::MmapPayloadOutOfBounds { offset, len })
    }
}

#[cfg(feature = "mmap")]
impl crate::matrix_tiles::store::TileSource for MmapSource {
    fn len(&self) -> Result<u64> {
        let physical = self.mmap.len() as u64;
        Ok(self
            .layout
            .map_or(physical, |layout| layout.logical_len(physical)))
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let Some(layout) = self.layout else {
            buf.copy_from_slice(self.contiguous(offset, buf.len() as u64)?);
            return Ok(());
        };
        layout.split(offset, buf.len() as u64, |segment| {
            if let crate::anchor::Segment::Data {
                physical,
                logical,
                len,
            } = segment
            {
                let from = physical as usize;
                let to = from + len as usize;
                let bytes = self
                    .mmap
                    .get(from..to)
                    .ok_or(Error::MmapPayloadOutOfBounds { offset, len })?;
                let at = (logical - offset) as usize;
                buf[at..at + len as usize].copy_from_slice(bytes);
            }
            Ok(())
        })
    }
}

#[cfg(feature = "mmap")]
/// A read-only snapshot of a file's matrix, mapped.
///
/// The mapping is private: every change the snapshot's write-ahead ring
/// holds is copied into its place in the mapping when it is made, so a
/// cell's bytes are where its tile says, and returned slices borrow the
/// mapping. Safe accessors check the commit bit and the cell's checksum.
///
/// These guarantees rely on the caller having upheld the unsafe constructor's
/// requirement that the backing object remains immutable and valid for this
/// value's full lifetime.
#[derive(Debug)]
pub struct MmapMatrix {
    pub(super) spec: FormatSpec,
    pub(super) tiles: crate::matrix_tiles::api::TiledMatrix,
    pub(super) source: MmapSource,
}

#[cfg(feature = "mmap")]
impl MmapMatrix {
    pub fn cell_payload_window<T: VarlitMatrixBlock>(&self, key: MatrixKey) -> Result<&[u8]> {
        crate::matrix::ensure_matrix_block::<T>(self.spec)?;
        if self.tiles.cell_status(&self.source, T::ID, key)? != MatrixCellStatus::Committed {
            return Err(Error::MatrixNotCommitted);
        }
        let (geom, tile, slot) = self
            .tiles
            .slot_of(&self.source, T::ID, key)?
            .ok_or(Error::MatrixNotCommitted)?;
        let at = tile + geom.slot_at(slot);
        let bytes = self.source.contiguous(at, geom.slot_len())?;
        let (payload, crc) = bytes.split_at(geom.stride as usize);
        let expected = u32::from_le_bytes(crc.try_into().expect("four"));
        let actual = crc32fast::hash(payload);
        if expected != actual {
            return Err(Error::MatrixChecksumMismatch {
                offset: at,
                expected,
                actual,
            });
        }
        Ok(payload)
    }

    pub fn cell_numeric<T, N>(&self, key: MatrixKey) -> Result<N>
    where
        T: VarlitMatrixBlock,
        N: MatrixNumeric,
    {
        self.cell_numeric_at::<T, N>(key, 0)
    }

    pub fn cell_numeric_at<T, N>(&self, key: MatrixKey, byte_offset: u64) -> Result<N>
    where
        T: VarlitMatrixBlock,
        N: MatrixNumeric,
    {
        let payload = self.cell_payload_window::<T>(key)?;
        let start = usize::try_from(byte_offset).map_err(|_| Error::MatrixNumericOutOfBounds {
            offset: byte_offset,
            len: N::BYTE_LEN as u64,
            payload_len: payload.len() as u64,
        })?;
        let end = start
            .checked_add(N::BYTE_LEN)
            .ok_or(Error::MatrixNumericOutOfBounds {
                offset: byte_offset,
                len: N::BYTE_LEN as u64,
                payload_len: payload.len() as u64,
            })?;
        let bytes = payload
            .get(start..end)
            .ok_or(Error::MatrixNumericOutOfBounds {
                offset: byte_offset,
                len: N::BYTE_LEN as u64,
                payload_len: payload.len() as u64,
            })?;
        Ok(N::read_matrix_numeric(
            bytes,
            T::ENDIAN.unwrap_or(self.spec.endian),
        ))
    }

    #[cfg(feature = "zero-copy")]
    /// Returns a raw matrix cell reference directly backed by the mmap snapshot.
    ///
    /// # Safety
    ///
    /// The caller must ensure the mapped file bytes are not mutated for the
    /// lifetime of the returned reference, including by other handles,
    /// threads, or processes. The block's `VarlitRawMatrixBlock` implementation
    /// must also match the actual slot layout.
    pub unsafe fn raw_cell<T: crate::VarlitRawMatrixBlock>(&self, key: MatrixKey) -> Result<&T> {
        if T::KIND != BlockKind::Matrix {
            return Err(Error::ZeroCopyBlockKindMismatch { actual: T::KIND });
        }
        let expected_endian = T::ENDIAN.unwrap_or(self.spec.endian);
        if T::RAW_ENDIAN != expected_endian {
            return Err(Error::ZeroCopyEndianMismatch {
                expected: expected_endian,
                actual: T::RAW_ENDIAN,
            });
        }
        let bytes = self.cell_payload_window::<T>(key)?;
        let expected_len = std::mem::size_of::<T>();
        if bytes.len() != expected_len {
            return Err(Error::ZeroCopyPayloadSizeMismatch {
                expected: expected_len,
                actual: bytes.len() as u64,
            });
        }
        let required = std::mem::align_of::<T>();
        let address = bytes.as_ptr() as usize;
        if required > 1 && !address.is_multiple_of(required) {
            return Err(Error::ZeroCopyAlignmentMismatch { required, address });
        }
        T::ref_from_bytes(bytes).map_err(|_| Error::ZeroCopyPayloadSizeMismatch {
            expected: expected_len,
            actual: bytes.len() as u64,
        })
    }
}

#[cfg(feature = "mmap")]
pub(super) fn validate_mmap_range(offset: u64, len: u64, mapped_len: u64) -> Result<()> {
    let end = offset
        .checked_add(len)
        .ok_or(Error::MmapPayloadOutOfBounds { offset, len })?;
    if end > mapped_len {
        return Err(Error::MmapPayloadOutOfBounds { offset, len });
    }
    Ok(())
}

#[cfg(feature = "mmap")]
pub(super) fn validate_mmap_index_entry(entry: &RecordIndexEntry, mapped_len: u64) -> Result<()> {
    let expected_payload_offset = entry.record_offset.checked_add(RECORD_HEADER_LEN).ok_or(
        Error::MmapPayloadOutOfBounds {
            offset: entry.record_offset,
            len: RECORD_HEADER_LEN,
        },
    )?;
    if entry.payload_offset != expected_payload_offset {
        return Err(Error::MmapPayloadOutOfBounds {
            offset: entry.record_offset,
            len: RECORD_HEADER_LEN,
        });
    }
    validate_mmap_range(entry.payload_offset, entry.payload_len, mapped_len)?;
    let payload_end = entry.payload_offset.checked_add(entry.payload_len).ok_or(
        Error::MmapPayloadOutOfBounds {
            offset: entry.payload_offset,
            len: entry.payload_len,
        },
    )?;
    if let Some(footer_offset) = entry.footer_offset {
        if footer_offset != payload_end {
            return Err(Error::MmapPayloadOutOfBounds {
                offset: footer_offset,
                len: RECORD_FOOTER_LEN,
            });
        }
        validate_mmap_range(footer_offset, RECORD_FOOTER_LEN, mapped_len)?;
    }
    let physical_end = entry
        .checked_physical_end()
        .map_err(|_| Error::MmapPayloadOutOfBounds {
            offset: entry.record_offset,
            len: u64::MAX,
        })?;
    let record_len =
        physical_end
            .checked_sub(entry.record_offset)
            .ok_or(Error::MmapPayloadOutOfBounds {
                offset: entry.record_offset,
                len: u64::MAX,
            })?;
    validate_mmap_range(entry.record_offset, record_len, mapped_len)
}
