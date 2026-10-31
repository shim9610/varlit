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
/// requirement that the backing object remains immutable and valid for this
/// value's full lifetime.
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
    // of every entry (PERF2-07).
    pub(super) index: Vec<RecordIndexEntry>,
    pub(super) by_block: HashMap<u32, Vec<usize>>,
    // One byte per record only when integrity is enabled. The mapping is
    // immutable by contract, so a successful check remains valid. Relaxed
    // atomics retain Send + Sync without locks or publishing mutable data;
    // racing first readers may both verify the same immutable bytes.
    pub(super) verified: Vec<AtomicBool>,
    pub(super) mmap: memmap2::Mmap,
}

/// One record payload or matrix cell mapped independently of the file's total size.
/// Owns its mapping; dropping another window never invalidates this one.
#[cfg(feature = "mmap")]
#[derive(Debug)]
pub struct MmapPayloadWindow {
    pub(super) offset: u64,
    pub(super) mmap: Option<memmap2::Mmap>,
}
#[cfg(feature = "mmap")]
impl MmapPayloadWindow {
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn as_bytes(&self) -> &[u8] {
        self.mmap.as_deref().unwrap_or(&[])
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
        // the former per-entry hash-set copy provided (PERF2-07).
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
        // File offsets are absolute; the mapping starts at `base`.
        let start = offset
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
            .get(start..end)
            .ok_or(Error::MmapPayloadOutOfBounds {
                offset,
                len: len as u64,
            })
    }
}

#[cfg(feature = "mmap")]
/// A read-only mapping paired with an immutable snapshot of a validated matrix
/// layout.
///
/// Varlit validates the complete matrix extent against the mapped length before
/// constructing this type. Safe accessors use checked slot offsets and verify
/// configured CRC evidence. The mapping owns its OS mapping, so returned slices
/// cannot outlive it.
///
/// These guarantees rely on the caller having upheld the unsafe constructor's
/// requirement that the backing object remains immutable and valid for this
/// value's full lifetime.
#[derive(Debug)]
pub struct MmapMatrix {
    pub(super) spec: FormatSpec,
    pub(super) layout: crate::matrix::MatrixLayout,
    pub(super) mmap: memmap2::Mmap,
}

#[cfg(feature = "mmap")]
impl MmapMatrix {
    pub fn cell_payload_window<T: VarlitMatrixBlock>(&self, key: MatrixKey) -> Result<&[u8]> {
        let (offset, len, crc_offset) = crate::matrix::cell_payload_parts::<T>(
            self.spec,
            &self.layout,
            key,
            crate::matrix::BitmapRead::Mapped(&self.mmap),
        )?;
        let payload = self.window(offset, len)?;
        if let Some(crc_offset) = crc_offset {
            let expected = self.read_crc(crc_offset)?;
            crate::matrix::verify_payload_crc_bytes(offset, payload, expected)?;
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

    pub(super) fn window(&self, offset: u64, len: u64) -> Result<&[u8]> {
        let start =
            usize::try_from(offset).map_err(|_| Error::MmapPayloadOutOfBounds { offset, len })?;
        let len_usize =
            usize::try_from(len).map_err(|_| Error::MmapPayloadOutOfBounds { offset, len })?;
        let end = start
            .checked_add(len_usize)
            .ok_or(Error::MmapPayloadOutOfBounds { offset, len })?;
        self.mmap
            .get(start..end)
            .ok_or(Error::MmapPayloadOutOfBounds { offset, len })
    }

    pub(super) fn read_crc(&self, offset: u64) -> Result<u32> {
        let bytes = self.window(offset, 4)?;
        let mut crc = [0; 4];
        crc.copy_from_slice(bytes);
        Ok(u32::from_le_bytes(crc))
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
