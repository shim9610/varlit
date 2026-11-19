// What a `RecordIndexEntry` knows about its record: extents, checks and
// conversions.
// Included by `file.rs`, whose module it shares.

impl RecordIndexEntry {
    /// Reads stored payload bytes by reopening the object currently named by
    /// `path`.
    ///
    /// This is a low-level, non-snapshot convenience API. It does not prove
    /// that the path still names the object from which this entry was indexed,
    /// does not establish index membership or checksum validity, and applies
    /// the standard physical payload ceiling. Prefer a typed
    /// reader/collection, which remains bound to its opened snapshot. For an
    /// explicitly path-based untrusted-input tool, use
    /// `MyFormat::read_payload_limited` after establishing the entry's provenance.
    ///
    /// Generated format types expose this as `MyFormat::read_payload(&entry,
    /// path)`, so the format is fixed when the binary is compiled; the header
    /// read with it places the entry's logical offsets in an anchored file.
    #[doc(hidden)]
    pub fn __read_payload(&self, spec: FormatSpec, path: &Path) -> Result<Vec<u8>> {
        // The ceiling is checked before any I/O, as it always was.
        crate::ReadLimits::STANDARD.check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        let mut file = open_primary_by_path(spec, path)?;
        let file_len = file.len()?;
        self.validate_payload_extent(file_len)?;
        self.read_payload_file_validated(&mut file)
    }

    /// Reads stored payload bytes from the object currently named by `path`
    /// after applying an allocation ceiling.
    ///
    /// This remains path-based and non-snapshot: pathname replacement can make
    /// it read a different object with a compatible extent. The limit does not
    /// establish index membership or checksum validity. High-level readers do
    /// not use this method. Generated format types expose it as
    /// `MyFormat::read_payload_limited(&entry, path, limit)`.
    #[doc(hidden)]
    pub fn __read_payload_limited(
        &self,
        spec: FormatSpec,
        path: &Path,
        limit: u64,
    ) -> Result<Vec<u8>> {
        ensure_payload_limit(PHYSICAL_PAYLOAD_RESOURCE, self.payload_len, limit)?;
        let mut file = open_primary_by_path(spec, path)?;
        let file_len = file.len()?;
        self.validate_payload_extent(file_len)?;
        self.read_payload_file_validated(&mut file)
    }

    /// Whether the stored payload is an encoding of the logical one
    /// (compressed, or chunked with per-chunk checksums) rather than the
    /// logical bytes themselves.
    pub fn is_compressed(&self) -> bool {
        self.flags & (RECORD_FLAG_COMPRESSED | RECORD_FLAG_CHUNKED) != 0
    }

    /// Whether the payload carries a CRC per 64 KiB chunk.
    pub fn is_chunked(&self) -> bool {
        self.flags & RECORD_FLAG_CHUNKED != 0
    }

    /// Reads and decodes through the current pathname with separate stored and
    /// logical allocation ceilings.
    ///
    /// Generated format types expose this as `MyFormat::read_logical_payload`,
    /// `MyFormat::read_logical_payload_limited` and
    /// `MyFormat::read_logical_payload_with_resource_limits`, so the format is
    /// fixed when the binary is compiled. Non-snapshot: the limits bound
    /// allocation but do not establish index membership or record integrity.
    #[doc(hidden)]
    pub fn __read_logical_payload_limited(
        &self,
        spec: FormatSpec,
        path: &Path,
        physical_limit: u64,
        logical_limit: u64,
    ) -> Result<Vec<u8>> {
        let spec = spec.resolve_entrypoint();
        let mut file = open_primary_by_path(spec, path)?;
        let file_len = file.len()?;
        self.validate_payload_extent(file_len)?;
        spec.read_limits
            .check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        ensure_payload_limit(PHYSICAL_PAYLOAD_RESOURCE, self.payload_len, physical_limit)?;
        let logical_len = self.logical_payload_len_before_allocation(spec, &mut file)?;
        spec.read_limits
            .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;
        ensure_payload_limit(LOGICAL_PAYLOAD_RESOURCE, logical_len, logical_limit)?;
        let payload = self.read_payload_file_validated(&mut file)?;
        decode_record_payload(spec, self, payload)
    }

    pub(crate) fn logical_payload_len_snapshot(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
    ) -> Result<u64> {
        spec.read_limits
            .check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        self.validate_payload_extent(snapshot.len())?;
        let logical_len = if !self.is_compressed() {
            self.payload_len
        } else {
            self.logical_payload_len_from_snapshot(spec, snapshot)?
        };
        spec.read_limits
            .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;
        Ok(logical_len)
    }

    /// **This allocates.**
    /// [`read_payload_snapshot_into`](Self::read_payload_snapshot_into) takes
    /// the caller's buffer, which is what the per-record loops use.
    pub(crate) fn read_payload_snapshot(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
    ) -> Result<Vec<u8>> {
        let mut payload = Vec::new();
        self.read_payload_snapshot_into(spec, snapshot, &mut payload)?;
        Ok(payload)
    }

    /// Reads stored bytes into the caller's buffer, applying selected read checks.
    ///
    /// `out` ends exactly `payload_len` bytes long; see
    /// [`SnapshotFile::read_into_at`] for why that matters to the checksum.
    /// The record's whole extent — header through footer — when its framing
    /// is self-consistent: header before payload, footer (if any) right after
    /// it. `None` sends a caller to the part-by-part path, which reports the
    /// inconsistency itself.
    fn whole_record_len(&self) -> Option<usize> {
        let header_len = self.payload_offset.checked_sub(self.record_offset)?;
        let payload_end = self.payload_offset.checked_add(self.payload_len)?;
        let end = match self.footer_offset {
            Some(at) if at == payload_end => at.checked_add(RECORD_FOOTER_LEN)?,
            Some(_) => return None,
            None => payload_end,
        };
        if header_len < RECORD_HEADER_LEN {
            return None;
        }
        usize::try_from(end - self.record_offset).ok()
    }

    /// This record's payload out of `whole`, its bytes already read (header,
    /// payload and footer), after exactly the checks
    /// [`read_payload_snapshot_into`](Self::read_payload_snapshot_into) makes
    /// when it reads a small record whole. `None` when that path would not
    /// read this record whole: it is compressed or chunked, structure
    /// validation is off, or `whole` is not exactly the record.
    pub(crate) fn payload_from_whole<'b>(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
        whole: &'b [u8],
    ) -> Result<Option<&'b [u8]>> {
        if self.is_compressed()
            || self.is_chunked()
            || !spec.read_validation().structure
            || self.whole_record_len() != Some(whole.len())
        {
            return Ok(None);
        }
        spec.read_limits
            .check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        self.validate_payload_extent(snapshot.len())?;
        let header_len = crate::width::index(self.payload_offset - self.record_offset)?;
        let payload_end = header_len + crate::width::index(self.payload_len)?;
        let footer = match self.footer_offset {
            Some(at) => {
                let at = crate::width::index(at - self.record_offset)?;
                &whole[at..at + crate::width::small(RECORD_FOOTER_LEN)]
            }
            None => &[][..],
        };
        self.verify_record_bytes(
            spec,
            &whole[..header_len],
            &whole[header_len..payload_end],
            footer,
        )?;
        Ok(Some(&whole[header_len..payload_end]))
    }

    pub(crate) fn read_payload_snapshot_into(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        spec.read_limits
            .check(ReadLimitKey::RecordPayloadLen, self.payload_len)?;
        self.validate_payload_extent(snapshot.len())?;
        // A record that fits one page is read whole — header, payload and
        // footer in one positional read onto the stack — and verified from
        // those bytes. A larger one reads its payload straight into `out` and
        // its framing separately, where copying the payload would cost more
        // than the two reads it saves.
        const WHOLE: usize = 4096;
        if spec.read_validation().structure
            && let Some(whole) = self.whole_record_len().filter(|len| *len <= WHOLE)
        {
            let mut bytes = [0u8; WHOLE];
            snapshot.read_exact_at(self.record_offset, &mut bytes[..whole])?;
            let header_len = crate::width::index(self.payload_offset - self.record_offset)?;
            let payload_end = header_len + crate::width::index(self.payload_len)?;
            let footer = match self.footer_offset {
                Some(at) => {
                    let at = crate::width::index(at - self.record_offset)?;
                    &bytes[at..at + crate::width::small(RECORD_FOOTER_LEN)]
                }
                None => &[][..],
            };
            self.verify_record_bytes(
                spec,
                &bytes[..header_len],
                &bytes[header_len..payload_end],
                footer,
            )?;
            out.clear();
            out.try_reserve(crate::width::index(self.payload_len)?)
                .map_err(|_| Error::AllocationFailed {
                    resource: ReadLimitKey::RecordPayloadLen.resource(),
                    requested: self.payload_len,
                })?;
            out.extend_from_slice(&bytes[header_len..payload_end]);
            return Ok(());
        }
        snapshot.read_into_at(
            self.payload_offset,
            self.payload_len,
            limit_or_max(spec.read_limits.require(ReadLimitKey::RecordPayloadLen)?),
            ReadLimitKey::RecordPayloadLen.resource(),
            out,
        )?;
        self.verify_snapshot_record(spec, snapshot, out)
    }

    /// **This allocates.**
    /// [`read_logical_payload_snapshot_into`](Self::read_logical_payload_snapshot_into)
    /// takes the caller's buffer.
    pub(crate) fn read_logical_payload_snapshot(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
    ) -> Result<Vec<u8>> {
        let mut decoded = Vec::new();
        self.read_logical_payload_snapshot_into(spec, snapshot, &mut decoded)?;
        Ok(decoded)
    }

    /// Reads this record's logical bytes into the caller's buffer.
    ///
    /// **Uncompressed records cost no copy.** That is the case worth designing
    /// for — it is every record of a format that declares no compression — and
    /// the obvious `_into` gets it wrong: reading into a scratch and copying
    /// into `out` adds a full `memcpy` per record to a path that today moves
    /// the buffer straight out. So the uncompressed arm reads directly into
    /// `out` and stops there.
    ///
    /// **Compressed records are unchanged, and no better.** The decompressor
    /// produces its own buffer, so `out`'s allocation is replaced rather than
    /// reused. That is what happens today too; this does not regress it, and it
    /// does not improve it either. Making it reuse `out` needs a decompression
    /// entry point that writes into a caller buffer, which is a change to
    /// `decompress_with_algorithm` and is not made here.
    pub(crate) fn read_logical_payload_snapshot_into(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let logical_len = self.logical_payload_len_snapshot(spec, snapshot)?;
        if self.is_chunked() {
            // Chunks are read (and decompressed) straight into `out`, each
            // checked where it lands: the stored payload is never held.
            let cache = std::cell::RefCell::new(ChunkCache::default());
            return self.read_chunked_snapshot_into(spec, snapshot, logical_len, out, &cache);
        }
        if self.is_compressed() {
            let physical = self.read_payload_snapshot(spec, snapshot)?;
            *out = decode_record_payload(spec, self, physical)?;
        } else {
            self.read_payload_snapshot_into(spec, snapshot, out)?;
        }
        let actual =
            u64::try_from(out.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        if actual != logical_len {
            return Err(Error::DecompressedLengthMismatch {
                expected: logical_len,
                actual,
            });
        }
        Ok(())
    }

    /// [`Self::read_logical_payload_snapshot_into`] through a handle's
    /// buffers: a compressed record's stored bytes and decompression context
    /// are the handle's, so a settled read allocates nothing.
    pub(crate) fn read_logical_payload_cached(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
        out: &mut Vec<u8>,
        cache: &std::cell::RefCell<ChunkCache>,
    ) -> Result<()> {
        if !self.is_compressed() {
            return self.read_logical_payload_snapshot_into(spec, snapshot, out);
        }
        let logical_len = self.logical_payload_len_snapshot(spec, snapshot)?;
        if self.is_chunked() {
            return self.read_chunked_snapshot_into(spec, snapshot, logical_len, out, cache);
        }
        let Ok(mut cache) = cache.try_borrow_mut() else {
            return self.read_logical_payload_snapshot_into(spec, snapshot, out);
        };
        cache.decode_compressed_into(spec, snapshot, self, out)?;
        if out.len() as u64 != logical_len {
            return Err(Error::DecompressedLengthMismatch {
                expected: logical_len,
                actual: out.len() as u64,
            });
        }
        Ok(())
    }

    fn read_chunked_snapshot_into(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
        logical_len: u64,
        out: &mut Vec<u8>,
        cache: &std::cell::RefCell<ChunkCache>,
    ) -> Result<()> {
        self.verify_snapshot_framing(spec, snapshot)?;
        let len = usize::try_from(logical_len)
            .map_err(|_| Error::LengthOverflow { value: logical_len })?;
        if out.len() < len {
            out.try_reserve_exact(len - out.len())
                .map_err(|_| Error::AllocationFailed {
                    resource: ReadLimitKey::LogicalPayloadLen.resource(),
                    requested: logical_len,
                })?;
        }
        out.resize(len, 0);
        let mut filled = 0;
        while filled < len {
            let page = fill_record(
                spec,
                snapshot,
                self,
                filled as u64,
                &mut out[filled..],
                cache,
            )?;
            if page.end != logical_len {
                return Err(Error::DecompressedLengthMismatch {
                    expected: logical_len,
                    actual: page.end,
                });
            }
            if page.filled == 0 {
                return Err(Error::UnexpectedEof);
            }
            filled += page.filled;
        }
        Ok(())
    }

    /// Whether the writer marked this record dead.
    ///
    /// Always `false` for a format without `liveness: footer_flags`, where the
    /// word is a reserved field every reader requires to be zero.
    ///
    /// **Advisory.** The record is still here, still framed, and still read
    /// back by every collection; a defragmenting rewrite is what acts on this.
    /// Unknown bits in the word are ignored rather than refused — the word is
    /// outside the checksum, so there is nothing to tell a newer build's flag
    /// from a flipped bit, and refusing would turn a forward-compatible mark
    /// into an unreadable file.
    pub const fn is_dead(&self) -> bool {
        self.mutable_flags & RECORD_MUTABLE_FLAG_DEAD != 0
    }

    pub fn checked_physical_end(&self) -> Result<u64> {
        self.payload_offset
            .checked_add(self.payload_len)
            .and_then(|end| {
                end.checked_add(u64::from(self.footer_offset.is_some()) * RECORD_FOOTER_LEN)
            })
            .ok_or(Error::LengthOverflow {
                value: self.payload_len,
            })
    }

    pub fn physical_end(&self) -> u64 {
        self.payload_offset
            .saturating_add(self.payload_len)
            .saturating_add(u64::from(self.footer_offset.is_some()) * RECORD_FOOTER_LEN)
    }
}

impl From<&RecordIndexEntry> for AppendInfo {
    fn from(entry: &RecordIndexEntry) -> Self {
        Self {
            sequence: entry.sequence,
            record_offset: entry.record_offset,
            payload_offset: entry.payload_offset,
            payload_len: entry.payload_len,
            footer_offset: entry.footer_offset,
            prev_same_block_offset: entry.prev_same_block_offset,
            prev_same_key_offset: entry.prev_same_key_offset,
            committed: entry.committed,
        }
    }
}
