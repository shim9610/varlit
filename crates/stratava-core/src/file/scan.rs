//! scan implementation, kept within the original parent privacy boundary.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ScanIntent {
    ReadOnly,
    Writer,
    Recover,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RecoverableTail {
    pub(super) offset: u64,
    pub(super) truncate_to: u64,
}

#[derive(Debug)]
pub(super) enum RecordRead {
    Entry(RecordIndexEntry),
    RecoverableTail(RecoverableTail),
}

std::thread_local! {
    /// Bytes this thread has advanced over while scanning records, all opens
    /// summed. The single charge point for `ReadLimitKey::ScanBytes` is
    /// `ScanAccounting::advance`, so this is every record byte an open walks —
    /// and the number that separates an open that follows the on-disk index
    /// from one that walks the file.
    static OPEN_SCAN_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[inline]
pub(super) fn count_open_scan_bytes(bytes: u64) {
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    OPEN_SCAN_BYTES.with(|total| total.set(total.get().saturating_add(bytes)));
    #[cfg(not(any(test, feature = "scalable-fault-injection")))]
    let _ = bytes;
}

/// Reads and clears this thread's scanned-record byte total.
#[cfg(any(test, feature = "scalable-fault-injection"))]
pub(crate) fn take_open_scan_bytes() -> u64 {
    OPEN_SCAN_BYTES.with(|total| total.replace(0))
}

#[cfg(any(test, feature = "scalable-fault-injection"))]
thread_local! {
    static RECORD_ENTRY_FAULTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one entry rebuilt from its record header. Inert outside tests and
/// without the `scalable-fault-injection` feature.
#[inline]
pub(super) fn count_record_entry_fault() {
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    RECORD_ENTRY_FAULTS.with(|count| count.set(count.get().saturating_add(1)));
}

/// Reads and clears this thread's count of entries faulted in from the file.
#[cfg(any(test, feature = "scalable-fault-injection"))]
pub(crate) fn take_record_entry_faults() -> u64 {
    RECORD_ENTRY_FAULTS.with(|count| count.replace(0))
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ScanAccounting {
    pub(super) advanced: u64,
}

impl ScanAccounting {
    pub(super) fn advance(&mut self, spec: FormatSpec, bytes: u64) -> Result<()> {
        let advanced =
            self.advanced
                .checked_add(bytes)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "scan bytes",
                })?;
        spec.read_limits.check(ReadLimitKey::ScanBytes, advanced)?;
        count_open_scan_bytes(bytes);
        self.advanced = advanced;
        Ok(())
    }
}

/// What a single record read is allowed to tolerate, and what it must check.
///
/// One value rather than three parameters because the three are one decision:
/// how this particular walk treats a record it cannot fully validate. Splitting
/// them let a caller pass a recovery boundary while silently skipping the check
/// that produces the evidence for it.
#[derive(Clone, Copy, Debug)]
pub(super) struct ScanChecks {
    /// Offset a torn record may be truncated back to, if tolerated.
    pub(super) partial_boundary: Option<u64>,
    /// Offset a checksum mismatch may be truncated back to, if tolerated.
    pub(super) checksum_boundary: Option<u64>,
    /// Whether the stored checksum is recomputed from the payload here.
    ///
    /// False only on the open path under `IntegrityVerification::OnDemand`,
    /// where the identical check runs on the read that returns the record.
    pub(super) verify_checksums: bool,
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    pub(super) static RECORDS_FRAMED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    pub(super) static CHUNK_BYTES_READ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts bytes read to answer a chunked cell read on this thread. Inert
/// without the `scalable-fault-injection` feature.
#[inline]
pub(super) fn note_chunk_bytes_read(bytes: u64) {
    #[cfg(feature = "scalable-fault-injection")]
    CHUNK_BYTES_READ.with(|read| read.set(read.get().saturating_add(bytes)));
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = bytes;
}

/// Counts records framed while building an index on this thread. Inert without
/// the `scalable-fault-injection` feature.
#[inline]
pub(super) fn note_record_framed() {
    #[cfg(feature = "scalable-fault-injection")]
    RECORDS_FRAMED.with(|framed| framed.set(framed.get().saturating_add(1)));
}

#[cfg(any(test, feature = "scalable-fault-injection"))]
std::thread_local! {
    /// Entry comparisons this thread has made looking up a rewritten record's
    /// chain predecessor (`find_rewritten_by_offset`).
    ///
    /// Fault-testing hook only. This is the unit the front-to-back scan made
    /// quadratic: a `replace_block` over N records probed ~N^2/2 times and now
    /// probes at most `N * ceil(log2 N)`. A regression test asserts the count,
    /// not the wall clock.
    pub(super) static REPLACEMENT_PREDECESSOR_PROBES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one predecessor-lookup comparison on this thread. Inert outside tests
/// and without the `scalable-fault-injection` feature.
#[inline]
pub(super) fn note_replacement_predecessor_probe() {
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    REPLACEMENT_PREDECESSOR_PROBES.with(|probes| probes.set(probes.get().saturating_add(1)));
}

pub(super) fn read_record_entry_at(
    spec: FormatSpec,
    file: &mut (impl Read + Seek),
    file_len: u64,
    offset: u64,
    checks: ScanChecks,
    accounting: &mut ScanAccounting,
) -> Result<RecordRead> {
    note_record_framed();
    let ScanChecks {
        partial_boundary,
        checksum_boundary,
        verify_checksums,
    } = checks;
    let remaining = file_len
        .checked_sub(offset)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "scan offset",
        })?;
    if remaining < RECORD_HEADER_LEN {
        accounting.advance(spec, remaining)?;
        return Ok(RecordRead::RecoverableTail(RecoverableTail {
            offset,
            truncate_to: offset,
        }));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut entry = read_record_header(file, spec, offset)?;
    validate_record_entry(spec, &entry)?;
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, entry.payload_len)?;
    if !entry.is_compressed() {
        spec.read_limits
            .check(ReadLimitKey::LogicalPayloadLen, entry.payload_len)?;
    }
    let payload_end = entry.payload_offset.checked_add(entry.payload_len).ok_or(
        Error::ResourceArithmeticOverflow {
            resource: "record extent",
        },
    )?;
    let record_end = payload_end.checked_add(record_footer_len(spec)).ok_or(
        Error::ResourceArithmeticOverflow {
            resource: "record extent",
        },
    )?;
    let record_len = record_end
        .checked_sub(offset)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "record extent",
        })?;
    accounting.advance(spec, record_len)?;
    if record_end > file_len {
        let Some(truncate_to) = partial_boundary else {
            return Err(Error::CorruptTail { offset });
        };
        return Ok(RecordRead::RecoverableTail(RecoverableTail {
            offset,
            truncate_to,
        }));
    }

    let logical_len = entry.logical_payload_len_before_allocation(spec, file)?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;

    let footer = if spec.spec_needs_record_footer() {
        let footer_offset = payload_end;
        let footer_bytes = read_record_footer_bytes(file, footer_offset)?;
        let footer = decode_record_footer(spec, &footer_bytes, footer_offset, entry.record_offset)?;
        entry.footer_offset = Some(footer_offset);
        entry.prev_same_block_offset = footer.prev_same_block_offset;
        entry.prev_same_key_offset = footer.prev_same_key_offset;
        entry.mutable_flags = footer.mutable_flags;
        Some(footer_bytes)
    } else {
        None
    };

    // SCAN-01: this is the single line that made open read the whole file. The
    // check itself is not removed - it is *relocated* to the read that returns
    // the record, where `read_payload_snapshot` already performs the identical
    // one on every typed read. See `IntegrityVerification` for why that is a
    // choice about when damage is announced rather than whether bytes are
    // checked, and for the one caller that must never take this branch off:
    // recovery, whose truncation evidence *is* the mismatch.
    if verify_checksums
        && matches!(
            spec.integrity_policy,
            IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader
        )
    {
        let expected = checksum_record_file(
            spec,
            file,
            &entry,
            footer.as_ref().map(|bytes| &bytes[..]).unwrap_or(&[]),
        )?;
        if expected != entry.checksum {
            if let Some(truncate_to) = checksum_boundary {
                return Ok(RecordRead::RecoverableTail(RecoverableTail {
                    offset,
                    truncate_to,
                }));
            }
            return Err(Error::ChecksumMismatch { offset });
        }
    }

    if entry.block_id == COMMIT_BLOCK_ID {
        if entry.payload_len != COMMIT_PAYLOAD_MAGIC.len() as u64 {
            return Err(Error::InvalidCommitMarker { offset });
        }
        let mut payload = [0; COMMIT_PAYLOAD_MAGIC.len()];
        file.seek(SeekFrom::Start(entry.payload_offset))?;
        file.read_exact(&mut payload)?;
        if &payload != COMMIT_PAYLOAD_MAGIC {
            return Err(Error::InvalidCommitMarker { offset });
        }
    }

    Ok(RecordRead::Entry(entry))
}

#[derive(Debug)]
pub(crate) struct NativeStreamScanner {
    pub(super) spec: FormatSpec,
    pub(super) file: crate::snapshot::SnapshotReader,
    pub(super) snapshot: SnapshotFile,
    pub(super) offset: u64,
    pub(super) accounting: ScanAccounting,
    pub(super) records: u64,
    pub(super) previous_sequence: Option<u64>,
}

#[cfg(test)]
thread_local! {
    static STREAM_SCANNER_CONSTRUCTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static STREAM_SCANNER_ENTRIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static STREAM_POINT_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_stream_io_counters() {
    STREAM_SCANNER_CONSTRUCTIONS.set(0);
    STREAM_SCANNER_ENTRIES.set(0);
    STREAM_POINT_READS.set(0);
}

#[cfg(test)]
pub(crate) fn stream_io_counters() -> (u64, u64, u64) {
    (
        STREAM_SCANNER_CONSTRUCTIONS.get(),
        STREAM_SCANNER_ENTRIES.get(),
        STREAM_POINT_READS.get(),
    )
}

impl NativeStreamScanner {
    pub(crate) fn from_snapshot(spec: FormatSpec, snapshot: SnapshotFile) -> Result<Self> {
        let mut file = snapshot.reader();
        let header_len = read_file_header(spec, &mut file)?;
        let mut accounting = ScanAccounting::default();
        accounting.advance(spec, header_len)?;
        #[cfg(test)]
        STREAM_SCANNER_CONSTRUCTIONS.set(STREAM_SCANNER_CONSTRUCTIONS.get() + 1);
        Ok(Self {
            spec,
            file,
            snapshot,
            offset: header_len,
            accounting,
            records: 0,
            previous_sequence: None,
        })
    }

    pub(crate) fn extend_snapshot(&mut self, snapshot: SnapshotFile) {
        debug_assert!(self.snapshot.same_file_object(&snapshot));
        debug_assert!(self.snapshot.len() <= snapshot.len());
        self.file.extend_snapshot(snapshot.clone());
        self.snapshot = snapshot;
    }

    pub(crate) fn next_entry(&mut self) -> Result<Option<RecordIndexEntry>> {
        if self.offset == self.snapshot.len() {
            return Ok(None);
        }
        #[cfg(test)]
        STREAM_SCANNER_ENTRIES.set(STREAM_SCANNER_ENTRIES.get() + 1);
        let partial_boundary = self.spec.spec_needs_record_footer().then_some(self.offset);
        let entry = match read_record_entry_at(
            self.spec,
            &mut self.file,
            self.snapshot.len(),
            self.offset,
            // An explicit scan is a caller asking to walk records; it is not
            // the open path this policy exists to shorten.
            ScanChecks {
                partial_boundary,
                checksum_boundary: None,
                verify_checksums: true,
            },
            &mut self.accounting,
        )? {
            RecordRead::Entry(entry) => entry,
            RecordRead::RecoverableTail(tail) => {
                self.offset = tail.truncate_to;
                return Ok(None);
            }
        };
        if matches!(
            entry.block_id,
            OP_BLOCK_ID | INDEX_BLOCK_ID | COMMIT_BLOCK_ID | SEGMENT_BLOCK_ID
        ) {
            return Err(Error::StreamingUnsupported);
        }
        if self
            .previous_sequence
            .is_some_and(|previous| entry.sequence <= previous)
        {
            return Err(Error::StreamingUnsupported);
        }
        self.records = self
            .records
            .checked_add(1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?;
        self.spec
            .read_limits
            .check(ReadLimitKey::Records, self.records)?;
        self.previous_sequence = Some(entry.sequence);
        self.offset = entry.checked_physical_end()?;
        Ok(Some(entry))
    }

    pub(crate) const fn logical_eof(&self) -> u64 {
        self.offset
    }

    pub(crate) fn snapshot(&self) -> &SnapshotFile {
        &self.snapshot
    }

    pub(crate) const fn spec(&self) -> FormatSpec {
        self.spec
    }
}

pub(crate) fn read_stream_entry_at(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    offset: u64,
    physical_len: u64,
) -> Result<RecordIndexEntry> {
    let validated = snapshot.validate(
        crate::scalable_extent::UntrustedRecordPointer::new(offset, physical_len),
        spec.spec_needs_record_footer(),
    )?;
    let span = validated.span();
    #[cfg(test)]
    STREAM_POINT_READS.set(STREAM_POINT_READS.get() + 1);
    let mut file = snapshot.reader();
    let entry = read_stream_entry_at_file(spec, &mut file, snapshot, offset)?;
    if entry.record_offset != span.record_offset().get()
        || entry.payload_offset != span.payload_offset().get()
        || entry.payload_len != span.payload_len().get()
        || entry.checked_physical_end()? != span.end().get()
    {
        return Err(Error::InvalidIndexCheckpoint);
    }
    Ok(entry)
}

pub(crate) fn read_stream_entry_at_file(
    spec: FormatSpec,
    file: &mut (impl Read + Seek),
    snapshot: &SnapshotFile,
    offset: u64,
) -> Result<RecordIndexEntry> {
    let mut accounting = ScanAccounting::default();
    let partial_boundary = spec.spec_needs_record_footer().then_some(offset);
    match read_record_entry_at(
        spec,
        file,
        snapshot.len(),
        offset,
        // A single record read by offset: the caller named this record, so the
        // check belongs here and costs one payload, not a file.
        ScanChecks {
            partial_boundary,
            checksum_boundary: None,
            verify_checksums: true,
        },
        &mut accounting,
    )? {
        RecordRead::Entry(entry) => Ok(entry),
        RecordRead::RecoverableTail(_) => Err(Error::CorruptTail { offset }),
    }
}

#[derive(Debug)]
pub(crate) struct PreparedStreamRecord {
    pub(crate) bytes: Vec<u8>,
    pub(crate) info: AppendInfo,
    pub(crate) block_id: u32,
    pub(crate) block_version: u16,
    pub(crate) flags: u16,
    pub(crate) checksum: u32,
}

/// What [`prepare_stream_record_into`] knows about the record it just appended
/// to the caller's buffer.
///
/// The same fields as [`PreparedStreamRecord`] with the buffer taken out, plus
/// `len` — because a caller staging several records into one buffer needs to
/// know where this one ended, and `bytes.len()` is the answer it no longer has.
pub(crate) struct PreparedRecordParts {
    pub(crate) len: usize,
    pub(crate) info: AppendInfo,
    pub(crate) block_id: u32,
    pub(crate) block_version: u16,
    pub(crate) flags: u16,
    pub(crate) checksum: u32,
}

impl PreparedStreamRecord {
    /// The same record described as [`PreparedRecordParts`], for the callers
    /// that still take the owning form.
    pub(crate) fn parts(&self) -> PreparedRecordParts {
        PreparedRecordParts {
            len: self.bytes.len(),
            info: self.info,
            block_id: self.block_id,
            block_version: self.block_version,
            flags: self.flags,
            checksum: self.checksum,
        }
    }
}

/// The owning form, for the callers that want a record and nothing else: the
/// creation nonce, the manifest, a tombstone, the probe. None of them is a hot
/// path, and each allocates exactly the buffer it hands back.
#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_stream_record(
    spec: FormatSpec,
    block_id: u32,
    block_version: u16,
    flags: u16,
    uncompressed_len_hint: u32,
    payload: &[u8],
    sequence: u64,
    record_offset: u64,
    prev_same_block_offset: Option<u64>,
    prev_same_key_offset: Option<u64>,
) -> Result<PreparedStreamRecord> {
    let mut bytes = Vec::new();
    let parts = prepare_stream_record_into(
        spec,
        block_id,
        block_version,
        flags,
        uncompressed_len_hint,
        payload,
        sequence,
        record_offset,
        prev_same_block_offset,
        prev_same_key_offset,
        &mut bytes,
    )?;
    debug_assert_eq!(bytes.len(), parts.len);
    Ok(PreparedStreamRecord {
        bytes,
        info: parts.info,
        block_id: parts.block_id,
        block_version: parts.block_version,
        flags: parts.flags,
        checksum: parts.checksum,
    })
}

/// [`prepare_stream_user_record`], staging into the caller's buffer.
///
/// The single-record push path holds one buffer for the life of the writer and
/// clears it per record; the batch path appends record after record into the
/// chunk it is about to write. Both used to take a fresh `Vec` per record from
/// the owning form below, and the batch path then copied it into the chunk
/// buffer and dropped it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_stream_user_record_into<T: StratavaBlock>(
    spec: FormatSpec,
    value: &T,
    sequence: u64,
    record_offset: u64,
    prev_same_block_offset: Option<u64>,
    prev_same_key_offset: Option<u64>,
    out: &mut Vec<u8>,
) -> Result<PreparedRecordParts> {
    let endian = T::ENDIAN.unwrap_or(spec.endian);
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    let logical = encode_to_vec_limited(
        value,
        endian,
        logical_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )?;
    let stored = prepare_user_record_payload(spec, T::ID, T::KIND, &logical)?;
    prepare_stream_record_into(
        spec,
        T::ID,
        T::VERSION,
        stored.flags,
        stored.uncompressed_len_hint,
        &stored.bytes,
        sequence,
        record_offset,
        prev_same_block_offset,
        prev_same_key_offset,
        out,
    )
}

/// The owning form. Only the in-crate probe and the unit tests below want a
/// record they can hold; every production path stages through
/// [`prepare_stream_user_record_into`].
#[cfg(test)]
pub(crate) fn prepare_stream_user_record<T: StratavaBlock>(
    spec: FormatSpec,
    value: &T,
    sequence: u64,
    record_offset: u64,
    prev_same_block_offset: Option<u64>,
    prev_same_key_offset: Option<u64>,
) -> Result<PreparedStreamRecord> {
    let endian = T::ENDIAN.unwrap_or(spec.endian);
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    let logical = encode_to_vec_limited(
        value,
        endian,
        logical_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )?;
    let stored = prepare_user_record_payload(spec, T::ID, T::KIND, &logical)?;
    prepare_stream_record(
        spec,
        T::ID,
        T::VERSION,
        stored.flags,
        stored.uncompressed_len_hint,
        &stored.bytes,
        sequence,
        record_offset,
        prev_same_block_offset,
        prev_same_key_offset,
    )
}

pub(crate) fn prepare_stream_tombstone_record<T: StratavaKeyedBlock>(
    spec: FormatSpec,
    key: &T::Key,
    sequence: u64,
    record_offset: u64,
    prev_same_block_offset: Option<u64>,
    prev_same_key_offset: Option<u64>,
) -> Result<PreparedStreamRecord> {
    let payload = encode_internal_key_payload::<T>(spec, key)?;
    prepare_stream_record(
        spec,
        TOMBSTONE_BLOCK_ID,
        1,
        RECORD_FLAG_INTERNAL,
        0,
        &payload,
        sequence,
        record_offset,
        prev_same_block_offset,
        prev_same_key_offset,
    )
}

pub(crate) fn prepare_stream_manifest_record(
    spec: FormatSpec,
    sequence: u64,
    record_offset: u64,
) -> Result<PreparedStreamRecord> {
    let payload = encode_schema_manifest(spec)?;
    prepare_stream_record(
        spec,
        MANIFEST_BLOCK_ID,
        1,
        RECORD_FLAG_INTERNAL,
        0,
        &payload,
        sequence,
        record_offset,
        None,
        None,
    )
}

/// Produces a fresh 128-bit creation nonce for a stream/indexed primary.
///
/// Same uniqueness-not-secrecy contract as the matrix creation nonce: it only
/// has to differ between two `create` calls, including two that reuse the same
/// OS file object.
pub(crate) fn fresh_stream_creation_nonce() -> [u8; MATRIX_CREATION_NONCE_LEN] {
    fresh_matrix_creation_nonce()
}

/// Frames the creation-nonce record (STO-01).
///
/// The nonce lives *inside the primary* because that is the only place an
/// equal-length in-place rewrite cannot preserve: a nonce in the sidecar, the
/// writer-lock file or any companion file survives the rewrite and detects
/// nothing. It is written exactly once, at create, as the first record of the
/// log, so it costs nothing per append.
///
/// The payload is the bare 16-byte nonce under a dedicated reserved block id
/// rather than a keyed metadata envelope. That keeps the record at the smallest
/// possible size, so it fits inside any payload limit that admits a 16-byte
/// user record and needs no exemption from the caller's read limits.
pub(crate) fn prepare_stream_creation_nonce_record(
    spec: FormatSpec,
    nonce: [u8; MATRIX_CREATION_NONCE_LEN],
    sequence: u64,
    record_offset: u64,
) -> Result<PreparedStreamRecord> {
    prepare_stream_record(
        spec,
        CREATION_NONCE_BLOCK_ID,
        1,
        RECORD_FLAG_INTERNAL,
        0,
        &nonce,
        sequence,
        record_offset,
        None,
        None,
    )
}

/// Recovers the creation nonce of a stream/indexed primary, or `None` when the
/// primary carries none (a legacy primary, or one bootstrapped from a resident
/// `StratavaFile`).
///
/// Cost is one bounded point read of the first record, performed only at open
/// and at create ??never on an append or lookup path.
///
/// Any failure to read or decode the leading record is reported as "no nonce"
/// rather than as an error. That is fail-closed, not permissive: a primary that
/// *was* created with a nonce recorded a fingerprint that folds it in, so
/// answering `None` can only ever produce a *different* fingerprint and refuse
/// the sidecar. It must never turn a damaged leading record into an open-time
/// error on a path whose job is to compute an identity.
pub(crate) fn read_stream_creation_nonce(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    header_len: u64,
) -> Option<[u8; MATRIX_CREATION_NONCE_LEN]> {
    if header_len >= snapshot.len() {
        return None;
    }
    let mut file = snapshot.reader();
    let entry = read_stream_entry_at_file(spec, &mut file, snapshot, header_len).ok()?;
    if entry.block_id != CREATION_NONCE_BLOCK_ID
        || entry.flags & RECORD_FLAG_INTERNAL == 0
        || entry.block_version != 1
        || entry.payload_len != MATRIX_CREATION_NONCE_LEN as u64
    {
        return None;
    }
    let payload = entry.read_payload_snapshot(spec, snapshot).ok()?;
    <[u8; MATRIX_CREATION_NONCE_LEN]>::try_from(payload.as_slice()).ok()
}

/// Appends one encoded record to `out` and returns what the writer needs to
/// know about it.
///
/// **Appends rather than replaces**, which is what lets the batch path stage a
/// whole chunk through one buffer: each record lands directly where it will be
/// written from, instead of into a per-record `Vec` that is copied into the
/// chunk and dropped.
///
/// This used to build that per-record `Vec` itself — `try_reserve_exact` plus
/// three `extend_from_slice` — so continuous append allocated once per record,
/// on the path the project's design policy names as sacred.
#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_stream_record_into(
    spec: FormatSpec,
    block_id: u32,
    block_version: u16,
    flags: u16,
    uncompressed_len_hint: u32,
    payload: &[u8],
    sequence: u64,
    record_offset: u64,
    prev_same_block_offset: Option<u64>,
    prev_same_key_offset: Option<u64>,
    out: &mut Vec<u8>,
) -> Result<PreparedRecordParts> {
    let prev_same_block_offset = spec
        .index_policy
        .block_offset_chain
        .then_some(prev_same_block_offset)
        .flatten();
    let prev_same_key_offset = spec
        .index_policy
        .keyed_offset_chain
        .then_some(prev_same_key_offset)
        .flatten();
    let payload_len =
        u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    let logical_len =
        record_logical_len_from_parts(spec, block_id, flags, uncompressed_len_hint, payload)?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;
    let payload_offset =
        record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "file length",
            })?;
    let payload_end =
        payload_offset
            .checked_add(payload_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "file length",
            })?;
    let encoded_footer = if spec.spec_needs_record_footer() {
        Some(encode_record_footer(RecordFooterFields {
            prev_same_block_offset,
            prev_same_key_offset,
            mutable_flags: 0,
        })?)
    } else {
        None
    };
    let footer = encoded_footer.as_ref().map_or(&[][..], |bytes| &bytes[..]);
    let _record_end =
        payload_end
            .checked_add(footer.len() as u64)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "file length",
            })?;
    let header = RecordHeaderFields {
        block_id,
        block_version,
        flags,
        sequence,
        payload_len,
        checksum: 0,
        uncompressed_len_hint,
    };
    let checksum = checksum_record_fields(spec, record_offset, header, payload, footer)?;
    let header = encode_native_record_header(
        RecordHeaderFields { checksum, ..header },
        record_offset,
        record_footer_len(spec),
    )?;
    let total_len = header
        .len()
        .checked_add(payload.len())
        .and_then(|len| len.checked_add(footer.len()))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "record buffer length",
        })?;
    // `try_reserve` and not `try_reserve_exact`: `out` is the caller's buffer
    // and outlives this record, so exact-fitting it to one record is what would
    // make the next record reallocate. A fresh `Vec` reserves exactly `total_len`
    // on its first record either way, because `try_reserve` on an empty `Vec`
    // asks the allocator for what was requested.
    out.try_reserve(total_len)
        .map_err(|_| Error::AllocationFailed {
            resource: "record buffer",
            requested: total_len as u64,
        })?;
    out.extend_from_slice(&header);
    out.extend_from_slice(payload);
    out.extend_from_slice(footer);
    Ok(PreparedRecordParts {
        len: total_len,
        block_id,
        block_version,
        flags,
        checksum,
        info: AppendInfo {
            sequence,
            record_offset,
            payload_offset,
            payload_len,
            footer_offset: (!footer.is_empty()).then_some(payload_end),
            prev_same_block_offset,
            prev_same_key_offset,
            committed: true,
        },
    })
}

/// Whether this open may take the segment chain instead of the record scan.
///
/// Two exclusions, and both are about what the scan does that the walk does
/// not. A **recovery** scan verifies every record and truncates on the
/// mismatch; that evidence is the point of the open, and a walk that reads no
/// data record cannot produce it. [`IntegrityVerification::AtOpen`] asks for
/// the same verification of every record for the same reason. Under either,
/// the walk would be answering a cheaper question than the one asked.
/// What an open recovers from the file, beyond the resident index itself.
///
/// The index is filtered — a non-resident block's records are on disk and not
/// in it — so two things that used to be derived *from* the index cannot be any
/// more, and are carried out of the walk instead.
#[derive(Debug)]
pub(super) struct ScannedIndex {
    /// The highest sequence any record in the file carries, resident or not.
    ///
    /// `SequenceState::from_index` took the maximum in the resident index. With
    /// a non-resident block that maximum is stale, and the writer would re-issue
    /// sequence numbers already on disk — silently, and for exactly the block
    /// the option exists to serve. Sequences are file-global and gapless, so
    /// the high-water mark has to come from every record.
    pub(super) sequence_high_water: Option<u64>,
    /// The high-water mark as of the newest commit marker.
    ///
    /// A writer open discards everything after that marker, so those sequences
    /// go with it and may be re-issued — which is what the pre-H1 behaviour did
    /// by taking the maximum of the already-truncated index. Every non-resident
    /// survivor sits at or before the marker, so this bounds them all; the one
    /// record that survives *past* the marker is the trailing segment, and it
    /// is resident, so the final entry list covers it.
    pub(super) sequence_high_water_at_commit: Option<u64>,
    /// The tails as of the newest commit marker, for the same reason.
    ///
    /// A tail left pointing into the truncated region makes the next append
    /// write a footer naming a record past the end of the file, and the reopen
    /// after that fails with `InvalidRecordFooter`.
    pub(super) newest_at_commit: HashMap<u32, u64>,
    /// Newest record offset per block id, resident or not.
    ///
    /// `BlockTails::from_index` cannot answer this any more: it would drop the
    /// tail of a non-resident block, and that tail is the only way back to its
    /// records. Collected in a map and ordered exactly once at the end, because
    /// a per-record `note_appended` reintroduces the `Theta(B^2)` sorted-vector
    /// insertion term PERF3-03 removed.
    pub(super) newest: HashMap<u32, u64>,
    /// Where the last record the walk accepted physically ends, resident or
    /// not.
    ///
    /// The fourth thing derived from a filtered list, and the one that did not
    /// get this treatment until now. `open_readonly` used to bind its snapshot
    /// to the last *resident* entry's end, so under a markerless policy that
    /// ends on a non-resident block the snapshot stopped before every record of
    /// that block — not merely before the tail. `block_chain` is the published
    /// way to reach a non-resident block and it walks through the snapshot, so
    /// the whole chain became unreachable through a read-only handle.
    ///
    /// It is deliberately NOT `file.metadata()?.len()`: a torn trailing record
    /// must still leave the snapshot at the end of the last *complete* record,
    /// which is a fact only the scan has.
    pub(super) physical_end: Option<u64>,
    /// The same, as of the newest commit marker.
    ///
    /// A marker format discards everything after that marker, and a read-only
    /// handle must not expose records a writer open would delete. Uncommitted
    /// non-resident records past the marker are dropped with it, which is what
    /// stops this from becoming "the physical end of the file".
    pub(super) physical_end_at_commit: Option<u64>,
}

impl ScannedIndex {
    /// Records one record's contribution to the parts that outlive filtering.
    ///
    /// `physical_end` is where this record ends, which both call sites already
    /// hold: the segment walk computes it one statement earlier as the offset
    /// the next record must begin at, and the scan assigns it to `offset` five
    /// lines earlier for the same reason. Neither pays a new computation.
    pub(super) fn note(&mut self, entry: &RecordIndexEntry, physical_end: u64) {
        self.sequence_high_water = Some(match self.sequence_high_water {
            Some(seen) => seen.max(entry.sequence),
            None => entry.sequence,
        });
        self.newest.insert(entry.block_id, entry.record_offset);
        self.physical_end = Some(physical_end);
        if entry.block_id == COMMIT_BLOCK_ID {
            self.sequence_high_water_at_commit = self.sequence_high_water;
            self.physical_end_at_commit = self.physical_end;
            // O(B) and allocation-free after the first marker: the keys are
            // already there, only the values move.
            self.newest_at_commit.clear();
            self.newest_at_commit
                .extend(self.newest.iter().map(|(id, offset)| (*id, *offset)));
        }
    }

    /// Drops the recovered state to what survives a commit-boundary truncation.
    ///
    /// `entries` is the surviving resident list. Everything non-resident that
    /// survives sits at or before the newest commit marker, so the snapshots
    /// taken there bound it; the one record that survives *past* the marker is
    /// the trailing segment, which is resident and therefore in `entries`.
    pub(super) fn commit_boundary_applied(&mut self, entries: &[RecordIndexEntry]) -> Result<()> {
        let surviving = entries.iter().map(|entry| entry.sequence).max();
        self.sequence_high_water = match (self.sequence_high_water_at_commit, surviving) {
            (Some(at_commit), Some(resident)) => Some(at_commit.max(resident)),
            (at_commit, resident) => at_commit.or(resident),
        };
        // `max`, not "take the marker's end": `committed_prefix_len`
        // deliberately keeps one trailing SEGMENT_BLOCK_ID record past the
        // marker, and that record's bytes are inside the boundary too.
        let surviving_end = entries
            .last()
            .map(RecordIndexEntry::checked_physical_end)
            .transpose()?;
        self.physical_end = match (self.physical_end_at_commit, surviving_end) {
            (Some(at_commit), Some(resident)) => Some(at_commit.max(resident)),
            (at_commit, resident) => at_commit.or(resident),
        };
        self.newest.clear();
        self.newest.extend(
            self.newest_at_commit
                .iter()
                .map(|(id, offset)| (*id, *offset)),
        );
        for entry in entries {
            self.newest.insert(entry.block_id, entry.record_offset);
        }
        Ok(())
    }

    /// Orders the collected tails exactly once (PERF3-03).
    pub(super) fn block_tails(&self) -> BlockTails {
        BlockTails::from_newest(&self.newest)
    }

    /// The same tails, for a caller holding a `BlockTails` to merge them into
    /// rather than one to replace.
    ///
    /// [`StratavaFile::follow`] is that caller: it frames a *range*, so a block
    /// with no record in the range has no entry here and must keep the tail it
    /// already had. Replacing wholesale would report every such block as having
    /// no records at all.
    pub(super) fn newest_pairs(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        self.newest.iter().map(|(id, offset)| (*id, *offset))
    }

    pub(super) fn sequence_state(&self) -> SequenceState {
        match self.sequence_high_water {
            None => SequenceState::Available(0),
            Some(u64::MAX) => SequenceState::Exhausted,
            Some(sequence) => SequenceState::Available(sequence + 1),
        }
    }

    /// The byte range a handle opened on this scan may read.
    ///
    /// `append_start` is the answer for a file whose append log the walk found
    /// empty — the header is all there is.
    pub(super) fn physical_end(&self, append_start: u64) -> u64 {
        self.physical_end.unwrap_or(append_start)
    }
}

/// Whether this open may try the segment chain instead of the record scan.
///
/// **`segment_on_flush` is deliberately not consulted.** It is the *writer's*
/// policy — whether flushing appends a segment — and asking it here made the
/// reader consult its own configuration instead of the file in front of it: a
/// file written with a chain, opened by a spec that does not declare one, had
/// its index ignored and was walked header-to-EOF. The chain is a property of
/// the bytes, so the bytes decide. `load_index_from_segments` already answers
/// `Ok(None)` for a file whose chain does not describe it, and
/// `read_segment_tip_offset` costs one seek and a 40-byte read to find that
/// out, so a file without a chain pays that and falls back to the scan.
///
/// The other two conditions are about what the scan does that the walk does
/// not, and they stay: a recovery open exists to look at every record, and
/// `AtOpen` verification is a promise to checksum every record at open.
pub(super) fn segment_chain_open_is_allowed(spec: FormatSpec, intent: ScanIntent) -> bool {
    let _ = spec.index_policy.segment_on_flush;
    intent != ScanIntent::Recover
        && spec
            .read_limits
            .resolve()
            .effective_integrity_verification()
            != IntegrityVerification::AtOpen
}

pub(super) fn load_generation_index<B: ScanBuffer>(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    intent: ScanIntent,
    out: &mut B,
    pages: Option<&crate::matrix_generation::MatrixFile>,
) -> Result<ScannedIndex> {
    let end = pages.map_or(file.metadata()?.len(), |p| p.head().native_eof);
    out.reset();
    if segment_chain_open_is_allowed(spec, intent)
        && let Some(scanned) = out.segments(spec, file, header_len, end)?
    {
        return Ok(scanned);
    }
    out.reset();
    scan_records_range_at_len(spec, file, header_len, header_len, None, intent, out, end)
}

/// Finds the newest segment record from the end of the file.
///
/// The last 32 bytes are a record footer if this file ends in a record at all,
/// and the eight before them are that record's payload trailer if that record
/// is a segment. Neither alone is proof — the footer carries no block id and
/// the trailer is only eight bytes of payload — so the offset this returns is a
/// candidate that the caller confirms by reading the header there.
pub(super) fn read_segment_tip_offset(
    file: &mut File,
    append_start: u64,
    chain_end: u64,
) -> Result<u64> {
    let probe_len = SEGMENT_TRAILER_LEN + RECORD_FOOTER_LEN;
    let minimum = RECORD_HEADER_LEN + SEGMENT_PREFIX_LEN + probe_len;
    if chain_end
        .checked_sub(append_start)
        .is_none_or(|available| available < minimum)
    {
        return Err(Error::InvalidIndexSegment);
    }
    let probe_offset = chain_end - probe_len;
    let mut probe = [0u8; (SEGMENT_TRAILER_LEN + RECORD_FOOTER_LEN) as usize];
    file.seek(SeekFrom::Start(probe_offset))?;
    file.read_exact(&mut probe)?;
    let trailer_len = SEGMENT_TRAILER_LEN as usize;
    if &probe[trailer_len..trailer_len + 4] != RECORD_FOOTER_MAGIC {
        return Err(Error::InvalidIndexSegment);
    }
    let mut version = [0; 2];
    version.copy_from_slice(&probe[trailer_len + 4..trailer_len + 6]);
    if u16::from_le_bytes(version) != RECORD_FOOTER_VERSION {
        return Err(Error::InvalidIndexSegment);
    }
    let mut trailer = [0; SEGMENT_TRAILER_LEN as usize];
    trailer.copy_from_slice(&probe[..trailer_len]);
    let record_offset = u64::from_le_bytes(trailer);
    if record_offset < append_start || record_offset > probe_offset - SEGMENT_PREFIX_LEN {
        return Err(Error::InvalidIndexSegment);
    }
    Ok(record_offset)
}

/// Reads the open digest sitting at the end of the file, or reports that this
/// file's last record is not one.
///
/// The probe is the segment tip's, for the reason that one exists: open arrives
/// at EOF holding a record footer, and the header that would say where the
/// record starts is `payload_len` bytes further back. The trailer closes it.
///
/// Every claim is checked against the file rather than trusted. The record at
/// the trailer's offset must *be* a digest record — right block id, right
/// version, internal flag — its extent must end exactly at the end of the file,
/// and its checksum must verify, which is what stops a digest whose bytes rotted
/// from misreporting a block tail that nothing else would contradict. Any
/// failure is `Err`, and the caller answers it with the scan.
pub(super) fn frame_open_digest(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    file_len: u64,
) -> Result<RecordIndexEntry> {
    let probe_len = DIGEST_TRAILER_LEN + RECORD_FOOTER_LEN;
    let minimum = RECORD_HEADER_LEN + DIGEST_PREFIX_LEN + probe_len;
    if file_len
        .checked_sub(append_start)
        .is_none_or(|available| available < minimum)
    {
        return Err(Error::InvalidIndexSegment);
    }
    let probe_offset = file_len - probe_len;
    let mut probe = [0u8; (DIGEST_TRAILER_LEN + RECORD_FOOTER_LEN) as usize];
    file.seek(SeekFrom::Start(probe_offset))?;
    file.read_exact(&mut probe)?;
    let trailer_len = DIGEST_TRAILER_LEN as usize;
    if &probe[trailer_len..trailer_len + 4] != RECORD_FOOTER_MAGIC {
        return Err(Error::InvalidIndexSegment);
    }
    let mut version = [0; 2];
    version.copy_from_slice(&probe[trailer_len + 4..trailer_len + 6]);
    if u16::from_le_bytes(version) != RECORD_FOOTER_VERSION {
        return Err(Error::InvalidIndexSegment);
    }
    let mut trailer = [0; DIGEST_TRAILER_LEN as usize];
    trailer.copy_from_slice(&probe[..trailer_len]);
    let record_offset = u64::from_le_bytes(trailer);
    if record_offset < append_start || record_offset > probe_offset - DIGEST_PREFIX_LEN {
        return Err(Error::InvalidIndexSegment);
    }
    let mut accounting = ScanAccounting::default();
    let entry = match read_record_entry_at(
        spec,
        file,
        file_len,
        record_offset,
        // A digest is either wholly there or it is not a digest. There is no
        // recoverable tail here and no boundary to truncate back to: the scan
        // owns that decision, and this defers to it by failing.
        ScanChecks {
            partial_boundary: None,
            checksum_boundary: None,
            verify_checksums: true,
        },
        &mut accounting,
    )? {
        RecordRead::Entry(entry) => entry,
        RecordRead::RecoverableTail(_) => return Err(Error::InvalidIndexSegment),
    };
    if entry.block_id != OPEN_DIGEST_BLOCK_ID
        || entry.block_version != DIGEST_VERSION
        || entry.flags != RECORD_FLAG_INTERNAL
        || entry.checked_physical_end()? != file_len
    {
        return Err(Error::InvalidIndexSegment);
    }
    Ok(entry)
}

/// The digest record sitting past the end of the segment chain, if there is one.
///
/// A digest is appended *after* the segment that closes the same commit point,
/// so with both options on the newest segment does not end at `file_len` and
/// the chain walk — which checks exactly that — rejects a chain that is
/// perfectly good. Enabling the digest would silently turn the segment chain
/// off, which is the sort of interaction that shows up as a performance
/// mystery rather than as a failure.
///
/// Costs one 40-byte probe and one record frame, and only for a format that
/// declared the digest. A file that has no digest answers `file_len`, which is
/// what the walk assumed before this existed.
pub(super) fn trailing_open_digest(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    file_len: u64,
) -> Option<RecordIndexEntry> {
    if !spec.index_policy.open_digest_on_flush {
        return None;
    }
    frame_open_digest(spec, file, append_start, file_len).ok()
}

/// Reads the digest at the end of the file: the record, then what it says.
/// A header tail slot as the file carries it, after its checksum verified.
pub(super) struct HeaderTailsSlot {
    pub(super) generation: u64,
    /// The commit marker record this table was written for. Everything that
    /// makes the table checkable hangs off this one field.
    pub(super) commit_offset: u64,
    pub(super) tails: Vec<(u32, u64)>,
}

/// Every slot whose checksum and framing verify and which claims a table,
/// newest generation first.
///
/// A slot that fails anything here is skipped rather than refused: the region
/// is overwritten in place, so a half-written slot is an expected state and the
/// answer to it is the other slot, or the scan.
pub(super) fn decode_header_tails_slots(payload: &[u8]) -> Vec<HeaderTailsSlot> {
    let mut slots: Vec<HeaderTailsSlot> = Vec::new();
    if payload.is_empty() || !payload.len().is_multiple_of(HEADER_TAILS_SLOTS) {
        return slots;
    }
    let slot_len = payload.len() / HEADER_TAILS_SLOTS;
    if slot_len < HEADER_TAILS_SLOT_HEADER_LEN + HEADER_TAILS_SLOT_TRAILER_LEN {
        return slots;
    }
    for index in 0..HEADER_TAILS_SLOTS {
        let slot = &payload[index * slot_len..(index + 1) * slot_len];
        let body = &slot[..slot_len - HEADER_TAILS_SLOT_TRAILER_LEN];
        let mut recorded = [0u8; 4];
        recorded.copy_from_slice(&slot[slot_len - HEADER_TAILS_SLOT_TRAILER_LEN..]);
        // The one check that separates a torn write from a good one. The slot's
        // length never changes, so without this a half-written slot frames
        // perfectly and names records that are not where it says.
        let Ok(computed) = crc32_bytes(body) else {
            continue;
        };
        if computed != u32::from_le_bytes(recorded) {
            continue;
        }
        let read_u16 = |at: usize| u16::from_le_bytes([body[at], body[at + 1]]);
        let read_u32 =
            |at: usize| u32::from_le_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
        let read_u64 = |at: usize| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&body[at..at + 8]);
            u64::from_le_bytes(bytes)
        };
        if read_u16(0) != HEADER_TAILS_VERSION || read_u16(2) & !HEADER_TAILS_KNOWN_FLAGS != 0 {
            continue;
        }
        let capacity = read_u32(4) as usize;
        let count = read_u32(8) as usize;
        // The capacity the slot declares must be the geometry the region
        // actually has, so a corrupt count cannot walk past the slot.
        if header_tails_slot_len(capacity) != Some(slot_len) || count > capacity {
            continue;
        }
        // A cold or overflow slot claims nothing and is never adopted (see
        // `corroborate_header_tails`), but it is kept here because its
        // `generation` is what the next write has to advance past.
        let mut tails: Vec<(u32, u64)> = Vec::new();
        if tails.try_reserve_exact(count).is_err() {
            continue;
        }
        let mut previous: Option<u32> = None;
        let mut ordered = true;
        for entry in 0..count {
            let at = HEADER_TAILS_SLOT_HEADER_LEN + entry * HEADER_TAILS_ENTRY_LEN;
            let block_id = read_u32(at);
            let offset = read_u64(at + 4);
            // Strictly ascending, for the reason the digest requires it: every
            // lookup in `BlockTails` is a binary search, and an unsorted table
            // answers wrong rather than slowly.
            if previous.is_some_and(|last| block_id <= last) {
                ordered = false;
                break;
            }
            previous = Some(block_id);
            tails.push((block_id, offset));
        }
        if !ordered {
            continue;
        }
        slots.push(HeaderTailsSlot {
            generation: read_u64(12),
            commit_offset: read_u64(20),
            tails,
        });
    }
    slots.sort_unstable_by_key(|slot| std::cmp::Reverse(slot.generation));
    slots
}

/// Adopt the newest header tail table the *file* corroborates, or fail into the
/// scan.
///
/// The table lives at a fixed offset, which is the whole point — nothing
/// appended can move it or bury it — and is also the whole difficulty: a
/// structure at a fixed offset is always present and always "last", so its
/// position proves nothing about which state of the file it describes. The open
/// digest gets that for free by being a record whose extent must end exactly at
/// EOF (`frame_open_digest`); this has to earn it, in two bounded steps.
///
/// **(a) Is `commit_offset` a commit marker?** One positional framing with the
/// record checksum verified. This proves the table names *a* commit point.
///
/// **(b) Is it the *newest* one?** Walk forward from the marker's end; it must
/// reach the end of the file. Only a segment and an open digest may follow a
/// marker, and only in that order — `committed_prefix_len` accepts exactly the
/// shapes `[SEGMENT]`, `[DIGEST]` and `[SEGMENT, DIGEST]` — so the walk frames
/// at most two records and is not a scan.
///
/// **(b) is the one that matters.** A stale table names an older marker, and
/// that marker is still in the file and still checksums, so (a) passes. Without
/// (b) the reader would resume at an older commit boundary, and the next append
/// would write a `prev_same_block_offset` that skips every record in between —
/// a chain that is silently short, with every link's checksum intact. "Reaches
/// the end of the file" is exactly "no commit happened after this one".
///
/// Each tail is then re-framed and its block id checked, so a table that
/// survives both steps and still lies is detected rather than trusted.
pub(super) fn read_header_tails(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    file_len: u64,
    header_len: u64,
    extensions: &[u8],
) -> Result<OpenDigest> {
    let Some(extent) = mutable_header_block_extent(header_len, extensions, HEADER_TAILS_MAGIC)?
    else {
        return Err(Error::InvalidIndexSegment);
    };
    let payload = extensions
        .get(extent.payload_in_extensions..extent.payload_in_extensions + extent.payload_len)
        .ok_or(Error::InvalidIndexSegment)?;

    for slot in decode_header_tails_slots(payload) {
        if let Some(digest) = corroborate_header_tails(spec, file, append_start, file_len, &slot)
            .ok()
            .flatten()
        {
            return Ok(digest);
        }
    }
    Err(Error::InvalidIndexSegment)
}

pub(super) fn corroborate_header_tails(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    file_len: u64,
    slot: &HeaderTailsSlot,
) -> Result<Option<OpenDigest>> {
    let checks = ScanChecks {
        // A table is either wholly corroborated or it is not adopted. There is
        // no recoverable tail to negotiate here, and the scan owns that
        // decision, exactly as `frame_open_digest` defers to it.
        partial_boundary: None,
        checksum_boundary: None,
        verify_checksums: true,
    };
    let mut accounting = ScanAccounting::default();
    // A cold slot, or one whose table overflowed its capacity. Both say "I know
    // nothing", and adopting one would assert that no block has a tail.
    if slot.tails.is_empty() {
        return Ok(None);
    }
    if slot.commit_offset < append_start || slot.commit_offset >= file_len {
        return Ok(None);
    }
    // (a)
    let RecordRead::Entry(marker) = read_record_entry_at(
        spec,
        file,
        file_len,
        slot.commit_offset,
        checks,
        &mut accounting,
    )?
    else {
        return Ok(None);
    };
    if marker.block_id != COMMIT_BLOCK_ID || marker.flags & RECORD_FLAG_INTERNAL == 0 {
        return Ok(None);
    }
    // (b) — at most a segment then a digest, and it must land exactly on EOF.
    let mut cursor = marker.checked_physical_end()?;
    let mut sequence_high_water = marker.sequence;
    for expected in [SEGMENT_BLOCK_ID, OPEN_DIGEST_BLOCK_ID] {
        if cursor == file_len {
            break;
        }
        let RecordRead::Entry(entry) =
            read_record_entry_at(spec, file, file_len, cursor, checks, &mut accounting)?
        else {
            return Ok(None);
        };
        if entry.block_id != expected {
            return Ok(None);
        }
        sequence_high_water = sequence_high_water.max(entry.sequence);
        cursor = entry.checked_physical_end()?;
    }
    if cursor != file_len {
        return Ok(None);
    }
    // Every offset the table hands out is re-framed against the file. A table
    // that survives (a) and (b) and still lies is detected here, not trusted.
    for (block_id, offset) in &slot.tails {
        if *offset < append_start || *offset >= file_len {
            return Ok(None);
        }
        let RecordRead::Entry(entry) =
            read_record_entry_at(spec, file, file_len, *offset, checks, &mut accounting)?
        else {
            return Ok(None);
        };
        if entry.block_id != *block_id {
            return Ok(None);
        }
    }
    Ok(Some(OpenDigest {
        physical_end: file_len,
        sequence_high_water: Some(sequence_high_water),
        block_tails: BlockTails::from_sorted(slot.tails.clone()),
    }))
}

/// Picks which of the two lazy routes reads the block tails.
///
/// Both answer the same three facts and both fail into the scan; they differ in
/// where the answer is stored and therefore in what can hide it. Kept as one
/// dispatch so the two open sites stay identical either way.
pub(super) fn read_lazy_block_tails(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    file_len: u64,
    header_len: u64,
    extensions: &[u8],
    source: LazyOpenSource,
) -> Result<OpenDigest> {
    match source {
        LazyOpenSource::HeaderTails => {
            read_header_tails(spec, file, append_start, file_len, header_len, extensions)
        }
        LazyOpenSource::Digest => read_open_digest(spec, file, append_start, file_len),
        // The scan is not a route this reaches: it is the fallback the callers
        // take when both of the above fail.
        LazyOpenSource::FullScan => Err(Error::InvalidIndexSegment),
    }
}

pub(super) fn read_open_digest(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    file_len: u64,
) -> Result<OpenDigest> {
    let entry = frame_open_digest(spec, file, append_start, file_len)?;
    let payload = entry.read_payload_file_with_len(file, file_len)?;
    decode_digest_payload(&payload, append_start, entry.record_offset, file_len)
}

/// Walks the segment chain backwards from the end of the file and materializes
/// the index it describes.
///
/// Every link is checked against the file it claims to describe rather than
/// trusted: the record at each offset must be a segment record, its extent must
/// end exactly where the following link said its coverage began, and the
/// entries it carries must tile its coverage with no gap and no overlap. The
/// oldest link must reach the start of the append log. A chain that fails any
/// of these describes some other file, and the caller falls back to the scan.
///
/// Termination is structural: each link's predecessor offset must be strictly
/// smaller and no smaller than `append_start`, so the walk is bounded by the
/// segments in the file whatever the bytes say.
pub(super) fn walk_segment_chain(
    spec: FormatSpec,
    file: &mut File,
    append_start: u64,
    entries: &mut Vec<RecordIndexEntry>,
    file_len: u64,
) -> Result<ScannedIndex> {
    let mut accounting = ScanAccounting::default();
    accounting.advance(spec, append_start)?;

    // **Pass one carries the links, not the segments they describe.** This used
    // to collect `(RecordIndexEntry, DecodedSegment)` per link, and a
    // `DecodedSegment` owns the entries it describes — so the whole file's index
    // was materialised here and then materialised again into `scanned.entries`
    // below, two live copies at peak.
    //
    // A `RecordIndexEntry` per link is bounded by the segment count, which is
    // what `ReadLimitKey::Segments` already bounds; the entry arrays are what
    // followed the record count. So the links stay and the segments go: pass one
    // reads each link's fixed `SEGMENT_PREFIX_LEN` prefix for the one number it
    // needs to check that the links join up, and pass two reads the entry array
    // one segment at a time.
    //
    // Keeping the entry also keeps the accounting honest: each segment record's
    // header is read and charged exactly once, as before.
    let mut links: Vec<RecordIndexEntry> = Vec::new();
    // Not `file_len`: with the digest option on, the newest segment is followed
    // by the digest record that closes the same commit point. The digest is
    // kept, not just measured — it is a resident record, so the index this walk
    // produces has to contain it or it is not the index the scan produces.
    let digest = trailing_open_digest(spec, file, append_start, file_len);
    let chain_end = digest
        .as_ref()
        .map_or(file_len, |entry| entry.record_offset);
    let mut next = Some(read_segment_tip_offset(file, append_start, chain_end)?);
    // What the link being read must end at: the end of the chain for the tip,
    // and the coverage start of its successor for every link behind it.
    let mut expected_end = chain_end;
    while let Some(record_offset) = next {
        let entry = match read_record_entry_at(
            spec,
            file,
            file_len,
            record_offset,
            // A chain link is either wholly there or it is not a link. There is
            // no recoverable tail here and no boundary to truncate back to: the
            // scan owns that decision, and this walk defers to it by failing.
            ScanChecks {
                partial_boundary: None,
                checksum_boundary: None,
                verify_checksums: true,
            },
            &mut accounting,
        )? {
            RecordRead::Entry(entry) => entry,
            RecordRead::RecoverableTail(_) => return Err(Error::InvalidIndexSegment),
        };
        if entry.block_id != SEGMENT_BLOCK_ID
            || entry.block_version != SEGMENT_VERSION
            || entry.flags != RECORD_FLAG_INTERNAL
            || entry.checked_physical_end()? != expected_end
        {
            return Err(Error::InvalidIndexSegment);
        }
        let prefix = entry.read_payload_prefix_file_with_len(file, file_len, SEGMENT_PREFIX_LEN)?;
        let prefix = decode_segment_prefix(&prefix)?;
        if prefix.covered_start < append_start || prefix.covered_start > record_offset {
            return Err(Error::InvalidIndexSegment);
        }
        expected_end = prefix.covered_start;
        next = match entry.prev_same_block_offset {
            Some(previous) if previous >= record_offset || previous < append_start => {
                return Err(Error::InvalidIndexSegment);
            }
            previous => previous,
        };
        let count = u64::try_from(links.len())
            .ok()
            .and_then(|links| links.checked_add(1))
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "segment count",
            })?;
        spec.read_limits.check(ReadLimitKey::Segments, count)?;
        // Link staging also consumes the scan buffer. Otherwise a long chain
        // could grow without bound before the entry-array limit triggers the
        // ordinary open's streaming fallback.
        let link_count =
            usize::try_from(count).map_err(|_| Error::LengthOverflow { value: count })?;
        spec.read_limits
            .check(ReadLimitKey::IndexBytes, index_bytes_for_count(link_count)?)?;
        links.try_reserve(1).map_err(|_| Error::AllocationFailed {
            resource: "segment chain",
            requested: count,
        })?;
        links.push(entry);
    }
    // The oldest link must reach the append log, or records written before the
    // chain began are outside every segment and would be lost.
    if expected_end != append_start {
        return Err(Error::InvalidIndexSegment);
    }

    let mut scanned = ScannedIndex {
        sequence_high_water: None,
        sequence_high_water_at_commit: None,
        newest: HashMap::new(),
        newest_at_commit: HashMap::new(),
        physical_end: None,
        physical_end_at_commit: None,
    };
    // **Pass two, oldest link first, one segment live at a time.** Each link is
    // re-read and decoded here and its entries are moved straight into
    // `scanned`, so the peak is one segment plus the index being built rather
    // than the index twice.
    let mut running = append_start;
    for link in links.into_iter().rev() {
        let payload = link.read_payload_file_with_len(file, file_len)?;
        let segment = decode_segment_payload(spec, &payload, link.record_offset, file_len)?;
        drop(payload);
        // `preceding_records` is the writer's *resident* index position, and
        // the filter below drops exactly what the writer never installed, so
        // the two counts stay comparable with a non-resident block in play.
        let count =
            u64::try_from(entries.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?;
        if segment.covered_start != running || segment.preceding_records != count {
            return Err(Error::InvalidIndexSegment);
        }
        for covered in segment.entries {
            running = push_scanned_entry(spec, &mut scanned, entries, covered, running)?;
        }
        // The segment record closes its own coverage, so it sits immediately
        // after the last record it describes.
        if link.record_offset != running {
            return Err(Error::InvalidIndexSegment);
        }
        running = push_scanned_entry(spec, &mut scanned, entries, link, running)?;
    }
    // The digest closes the file the way the tip segment closes the chain, and
    // it is indexed here for the same reason every link is: a scan of these
    // bytes reports it, so a walk that dropped it would hand back a different
    // index for the same file.
    if let Some(digest) = digest {
        if digest.record_offset != running {
            return Err(Error::InvalidIndexSegment);
        }
        running = push_scanned_entry(spec, &mut scanned, entries, digest, running)?;
    }
    if running != file_len {
        return Err(Error::InvalidIndexSegment);
    }
    // Same single witness the scan carries, on the same shape of list
    // (PERF2-07): a chain is written by this crate but read from a file
    // anyone can hand over.
    validate_unique_sequences(entries)?;
    // The scan ends by cutting its list to the committed prefix, and the walk
    // must not hand back a list the scan would have cut. A chain that tiles the
    // whole append log without a commit marker in it is structurally perfect
    // and still a lie: `truncate_uncommitted_tail_if_needed` runs next, cuts
    // the file to the header, and would leave this handle holding an index of
    // records that are no longer on disk — after which the next append lands on
    // an offset the index already claims and the file is unreadable by either
    // path. Refusing routes it to the scan, which already knows how to truncate
    // and to report the empty index that goes with it.
    if spec.commit_policy.is_transaction_marker()
        && committed_prefix_len(entries) != Some(entries.len())
    {
        return Err(Error::InvalidIndexSegment);
    }
    Ok(scanned)
}

/// Appends one entry to an index under construction, charging the same limits
/// the record scan charges, and reports where the record after it must begin.
pub(super) fn push_scanned_entry(
    spec: FormatSpec,
    scanned: &mut ScannedIndex,
    entries: &mut Vec<RecordIndexEntry>,
    entry: RecordIndexEntry,
    expected_offset: u64,
) -> Result<u64> {
    if entry.record_offset != expected_offset
        || entry.payload_offset
            != expected_offset
                .checked_add(RECORD_HEADER_LEN)
                .ok_or(Error::InvalidIndexSegment)?
    {
        return Err(Error::InvalidIndexSegment);
    }
    let end = entry.checked_physical_end()?;
    // Every record contributes its sequence, its block tail and the physical
    // end of the walk; only a resident one is materialised. The chain carries
    // every record precisely so this filter can happen here rather than on
    // disk.
    scanned.note(&entry, end);
    if record_is_resident(spec, entry.block_id) {
        reserve_scanned_entry(spec, entries)?;
        entries.push(entry);
    }
    Ok(end)
}

/// Whether a record of `block_id` is mirrored in the resident index.
///
/// stratava's own bookkeeping is always resident: the manifest, commit markers,
/// checkpoints and segment records are what the open paths navigate by.
/// The block tails of a generation being rewritten, as of the records already
/// written into it.
///
/// Rebuilds an open digest for a republished generation.
///
/// # The high-water mark counts this record
///
/// `written` holds only the records emitted *before* this one — a republish
/// pushes each entry after writing it — so a maximum over that alone reports
/// the state *before* the digest, which is `next - 1`. A lazy open seeds its
/// writer with `sequence_high_water + 1`, so that number sends the next writer
/// back over the sequence this very record used, and the append after it
/// duplicates a sequence. The file then fails to open at all.
///
/// `own_sequence` closes that. A republish carries every entry's sequence
/// forward unchanged (`updated = source_entry.clone()`), so the digest's own
/// sequence is the source entry's.
///
/// This is the same correction [`StratavaFile::write_open_digest_record`] makes
/// on the append path, and it is a function rather than a line at each site
/// because it was a line at each site: the append path was corrected and the
/// three republish copies were not, all three still carrying the comment that
/// a digest "never describes itself". Measured on a `crc32 +
/// open_digest_on_flush` file of 64 records: the digest sat at sequence 64 and
/// recorded 63, a lazy writer resumed at 64, and the next scan refused the
/// file with `InvalidCanonicalEncoding("duplicate native record sequence")`.
pub(super) fn rebuilt_open_digest_payload(
    spec: FormatSpec,
    written: &[RecordIndexEntry],
    own_sequence: u64,
    record_offset: u64,
) -> Result<Vec<u8>> {
    let high_water = written
        .iter()
        .map(|entry| entry.sequence)
        .chain(core::iter::once(own_sequence))
        .max();
    encode_digest_payload(
        spec,
        high_water,
        rewritten_block_tails(written).as_slice(),
        record_offset,
    )
}

/// One forward pass into a map and one ordering — the shape
/// [`BlockTails::from_newest`] exists for, and the same shape the open scan
/// uses. It runs once per digest record in the source, so a rewrite of a file
/// with `S` commit points pays `O(S * N)`; that is the order the rewrite's
/// segment re-encoding already pays, on the path that is already the expensive
/// one.
pub(super) fn rewritten_block_tails(entries: &[RecordIndexEntry]) -> BlockTails {
    let mut newest: HashMap<u32, u64> = HashMap::new();
    for entry in entries {
        newest.insert(entry.block_id, entry.record_offset);
    }
    BlockTails::from_newest(&newest)
}

pub(super) fn record_is_resident(spec: FormatSpec, block_id: u32) -> bool {
    block_id >= RESERVED_BLOCK_ID_START || spec.block_is_resident(block_id)
}

/// Charges the record and index-byte ceilings for one more index entry and
/// reserves room for it.
pub(super) fn reserve_scanned_entry(
    spec: FormatSpec,
    entries: &mut Vec<RecordIndexEntry>,
) -> Result<()> {
    let record_count = u64::try_from(entries.len())
        .map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "record count",
        })?
        .checked_add(1)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "record count",
        })?;
    spec.read_limits
        .check(ReadLimitKey::Records, record_count)?;
    let index_bytes = index_bytes_for_count(usize::try_from(record_count).map_err(|_| {
        Error::LengthOverflow {
            value: record_count,
        }
    })?)?;
    spec.read_limits
        .check(ReadLimitKey::IndexBytes, index_bytes)?;
    entries.try_reserve(1).map_err(|_| Error::AllocationFailed {
        resource: "record index",
        requested: index_bytes,
    })
}

/// How many leading entries a transaction-marker format treats as committed.
///
/// The commit marker is the boundary, and everything after it is a writer's
/// unfinished work — with exactly one exception, bounded at one entry. A
/// segment record is appended *after* the marker whose transaction it closes,
/// because open finds the chain at the end of the file and a record behind the
/// segment would hide it. That segment describes only records the marker
/// already committed, so it is inside the boundary rather than past it.
///
/// Returns `None` when the file holds no commit marker, which means nothing in
/// it is committed.
pub(super) fn committed_prefix_len(entries: &[RecordIndexEntry]) -> Option<usize> {
    let position = entries
        .iter()
        .rposition(|entry| entry.block_id == COMMIT_BLOCK_ID)?;
    let kept = position + 1;
    // `entries.len() == kept + 1` was too strict: it kept the segment only when
    // it was the very last entry, so one uncommitted record appended after a
    // flush took the segment down with it. The writer then restarted coverage
    // from the older surviving segment, and every later segment re-covered
    // everything since — an O(N) payload and an O(N) allocation on every flush,
    // until `segment_payload_len` passed `max_record_payload_len` and segments
    // stopped being written at all, silently. The segment describes only
    // records the marker already committed, so keeping it is exactly as safe
    // whatever follows it.
    // The same argument extends to the digest, which is written after the
    // segment for the same reason the segment is written after the marker: both
    // describe only records the marker already committed, so keeping them is
    // exactly as safe whatever follows. The shapes the writer can produce are
    // `[SEGMENT]`, `[DIGEST]` and `[SEGMENT, DIGEST]`, and only those are
    // accepted — a longer run is a file this did not write, and truncating back
    // to the marker is the safe answer for one.
    let mut kept = kept;
    if entries.len() > kept && entries[kept].block_id == SEGMENT_BLOCK_ID {
        kept += 1;
    }
    if entries.len() > kept && entries[kept].block_id == OPEN_DIGEST_BLOCK_ID {
        kept += 1;
    }
    Some(kept)
}

/// Deals with whatever sits past the last commit marker.
///
/// Two answers, and which one applies is the difference between losing a
/// crashed writer's records and keeping them.
///
/// `TruncateTail` (and `Strict`) delete it, which is what stratava has always
/// done. `RecoveryPolicy::MarkTail` keeps it — but only when the file's
/// liveness block says the previous writer never released, because an
/// uncommitted tail left by an *orderly* writer (one that appended and then
/// chose not to commit) is that writer's business and truncating it is the
/// contract it was written under.
///
/// Returns `Some(committed_end)` when it left a tail in place for the caller to
/// mark. The caller must then mark and re-commit it: a file that ends past its
/// last marker is one the *next* marker would retroactively commit, and that is
/// the whole reason this cannot simply skip the truncate and stop.
pub(super) fn truncate_uncommitted_tail_if_needed<B: ScanBuffer>(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    entries: &B,
    previous_writer_crashed: bool,
) -> Result<Option<u64>> {
    if !spec.commit_policy.is_transaction_marker() {
        return Ok(None);
    }
    let committed_end = entries
        .last_entry()?
        .map(|entry| entry.checked_physical_end())
        .transpose()?
        .unwrap_or(header_len);
    if file.metadata()?.len() <= committed_end {
        return Ok(None);
    }
    if spec.recovery_policy == RecoveryPolicy::MarkTail && previous_writer_crashed {
        return Ok(Some(committed_end));
    }
    file.set_len(committed_end)?;
    Ok(None)
}

#[derive(Debug)]
pub(super) struct IndexCheckpoint {
    pub(super) covered_offset: u64,
    pub(super) entries: Vec<RecordIndexEntry>,
}

pub(super) fn decode_index_checkpoint(
    spec: FormatSpec,
    payload: &[u8],
    file_len: u64,
) -> Result<IndexCheckpoint> {
    const PREFIX_LEN: usize = 4 + 2 + 8 + 8;
    const ENTRY_LEN_V1: usize = 4 + 2 + 2 + 8 + 8 + 8 + 8 + 4;
    const ENTRY_LEN_V2: usize = ENTRY_LEN_V1 + 4;
    const ENTRY_LEN_V3: usize = ENTRY_LEN_V2 + 8 + 8 + 8 + 1;
    if payload.len() < PREFIX_LEN || &payload[..4] != INDEX_CHECKPOINT_MAGIC {
        return Err(Error::InvalidIndexCheckpoint);
    }
    let mut version = [0; 2];
    version.copy_from_slice(&payload[4..6]);
    let checkpoint_version = u16::from_le_bytes(version);
    if checkpoint_version == 0 || checkpoint_version > INDEX_CHECKPOINT_VERSION {
        return Err(Error::InvalidIndexCheckpoint);
    }
    let entry_len = match checkpoint_version {
        1 => ENTRY_LEN_V1,
        2 => ENTRY_LEN_V2,
        3 => ENTRY_LEN_V3,
        _ => return Err(Error::InvalidIndexCheckpoint),
    };
    let mut covered_offset = [0; 8];
    covered_offset.copy_from_slice(&payload[6..14]);
    let covered_offset = u64::from_le_bytes(covered_offset);
    let mut count = [0; 8];
    count.copy_from_slice(&payload[14..22]);
    let count = u64::from_le_bytes(count);
    spec.read_limits.check(ReadLimitKey::Records, count)?;
    let count_usize = usize::try_from(count).map_err(|_| Error::LengthOverflow { value: count })?;
    let resident_bytes = index_bytes_for_count(count_usize)?;
    spec.read_limits
        .check(ReadLimitKey::IndexBytes, resident_bytes)?;
    let expected_len = PREFIX_LEN
        .checked_add(
            count_usize
                .checked_mul(entry_len)
                .ok_or(Error::InvalidIndexCheckpoint)?,
        )
        .ok_or(Error::InvalidIndexCheckpoint)?;
    if payload.len() != expected_len {
        return Err(Error::InvalidIndexCheckpoint);
    }

    let mut position = PREFIX_LEN;
    for _ in 0..count_usize {
        let entry =
            read_index_entry_payload(&payload[position..position + entry_len], checkpoint_version);
        if entry.checked_physical_end()? > file_len {
            return Err(Error::InvalidIndexCheckpoint);
        }
        position += entry_len;
    }

    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count_usize)
        .map_err(|_| Error::AllocationFailed {
            resource: "checkpoint index",
            requested: resident_bytes,
        })?;
    let mut position = PREFIX_LEN;
    for _ in 0..count_usize {
        entries.push(read_index_entry_payload(
            &payload[position..position + entry_len],
            checkpoint_version,
        ));
        position += entry_len;
    }
    Ok(IndexCheckpoint {
        covered_offset,
        entries,
    })
}

/// Bytes of an index checkpoint payload that precede its entry array.
pub(super) const INDEX_CHECKPOINT_PREFIX_LEN: u64 = 4 + 2 + 8 + 8;

/// Runs the two ceilings an index checkpoint declares, from its prefix alone.
///
/// This replaces reading the whole payload and decoding it. `inspect_index_
/// checkpoint` did exactly that on every checkpoint record an open walked past
/// — and then dropped the result on the floor (`let _ = validate_index_
/// checkpoint(...)`), so the entire read, allocation and decode existed to
/// raise `LimitExceeded` from two fields. A checkpoint payload IS the index as
/// of its own position, and checkpoints are geometrically spaced, so that was
/// O(N) entries read and discarded per open of a `checkpoint_on_flush` format.
///
/// `count` sits at bytes 14..22, so the ceilings are decided by 22 bytes. The
/// errors raised are the same ones from the same fields.
pub(super) fn check_index_checkpoint_limits(spec: FormatSpec, prefix: &[u8]) -> Result<()> {
    check_index_checkpoint_limits_for(spec, prefix, true)
}
pub(super) fn check_index_checkpoint_limits_for(
    spec: FormatSpec,
    prefix: &[u8],
    materialize: bool,
) -> Result<()> {
    let prefix_len = INDEX_CHECKPOINT_PREFIX_LEN as usize;
    if prefix.len() < prefix_len || &prefix[..4] != INDEX_CHECKPOINT_MAGIC {
        // Not a checkpoint this build understands. `decode_index_checkpoint`
        // answered the same shape with a non-limit error, which the caller
        // swallowed, so there is nothing to raise here either.
        return Ok(());
    }
    let mut version = [0; 2];
    version.copy_from_slice(&prefix[4..6]);
    if !(1..=INDEX_CHECKPOINT_VERSION).contains(&u16::from_le_bytes(version)) {
        return Ok(());
    }
    let mut count = [0; 8];
    count.copy_from_slice(&prefix[14..22]);
    let count = u64::from_le_bytes(count);
    spec.read_limits.check(ReadLimitKey::Records, count)?;
    if !materialize {
        return Ok(());
    }
    let count_usize = usize::try_from(count).map_err(|_| Error::LengthOverflow { value: count })?;
    spec.read_limits.check(
        ReadLimitKey::IndexBytes,
        index_bytes_for_count(count_usize)?,
    )
}

pub(super) fn inspect_index_checkpoint<'a>(
    spec: FormatSpec,
    payload: &[u8],
    header_len: u64,
    file_len: u64,
    checkpoint_record: &RecordIndexEntry,
    observed_prefix_len: usize,
    observed_prefix: impl Iterator<Item = &'a RecordIndexEntry>,
) -> Result<()> {
    match decode_index_checkpoint(spec, payload, file_len) {
        Ok(checkpoint) => {
            let _ = validate_index_checkpoint(
                header_len,
                file_len,
                checkpoint_record,
                &checkpoint,
                observed_prefix_len,
                observed_prefix,
            );
            Ok(())
        }
        Err(error @ Error::LimitExceeded { .. })
        | Err(error @ Error::MissingResourceLimit { .. })
        | Err(error @ Error::TrustedUnboundedRequiresExplicitApi { .. })
        | Err(error @ Error::ResourceArithmeticOverflow { .. })
        | Err(error @ Error::LengthOverflow { .. })
        | Err(error @ Error::AllocationFailed { .. }) => Err(error),
        Err(_) => Ok(()),
    }
}

pub(super) fn read_index_entry_payload(
    payload: &[u8],
    checkpoint_version: u16,
) -> RecordIndexEntry {
    let mut u32_buf = [0; 4];
    let mut u16_buf = [0; 2];
    let mut u64_buf = [0; 8];

    u32_buf.copy_from_slice(&payload[0..4]);
    let block_id = u32::from_le_bytes(u32_buf);
    u16_buf.copy_from_slice(&payload[4..6]);
    let block_version = u16::from_le_bytes(u16_buf);
    u16_buf.copy_from_slice(&payload[6..8]);
    let flags = u16::from_le_bytes(u16_buf);
    u64_buf.copy_from_slice(&payload[8..16]);
    let sequence = u64::from_le_bytes(u64_buf);
    u64_buf.copy_from_slice(&payload[16..24]);
    let record_offset = u64::from_le_bytes(u64_buf);
    u64_buf.copy_from_slice(&payload[24..32]);
    let payload_offset = u64::from_le_bytes(u64_buf);
    u64_buf.copy_from_slice(&payload[32..40]);
    let payload_len = u64::from_le_bytes(u64_buf);
    u32_buf.copy_from_slice(&payload[40..44]);
    let checksum = u32::from_le_bytes(u32_buf);
    let uncompressed_len_hint = if checkpoint_version >= 2 {
        u32_buf.copy_from_slice(&payload[44..48]);
        u32::from_le_bytes(u32_buf)
    } else {
        0
    };
    let (footer_offset, prev_same_block_offset, prev_same_key_offset, committed) =
        if checkpoint_version >= 3 {
            u64_buf.copy_from_slice(&payload[48..56]);
            let footer_offset = nonzero_offset(u64::from_le_bytes(u64_buf));
            u64_buf.copy_from_slice(&payload[56..64]);
            let prev_same_block_offset = nonzero_offset(u64::from_le_bytes(u64_buf));
            u64_buf.copy_from_slice(&payload[64..72]);
            let prev_same_key_offset = nonzero_offset(u64::from_le_bytes(u64_buf));
            let committed = payload[72] != 0;
            (
                footer_offset,
                prev_same_block_offset,
                prev_same_key_offset,
                committed,
            )
        } else {
            (None, None, None, true)
        };

    RecordIndexEntry {
        block_id,
        block_version,
        flags,
        sequence,
        record_offset,
        payload_offset,
        payload_len,
        checksum,
        uncompressed_len_hint,
        footer_offset,
        prev_same_block_offset,
        prev_same_key_offset,
        // Index-checkpoint entries do not carry the word. That is deliberate
        // rather than an omission: the checkpoint is written once, and the
        // whole point of this field is that it changes afterwards, so a copy
        // stored there could only ever go stale. The record's own footer is the
        // authority, and `fault_record_entry` reads it from there.
        mutable_flags: 0,
        committed,
    }
}

pub(super) fn validate_index_checkpoint<'a>(
    header_len: u64,
    file_len: u64,
    checkpoint_record: &RecordIndexEntry,
    checkpoint: &IndexCheckpoint,
    observed_prefix_len: usize,
    observed_prefix: impl Iterator<Item = &'a RecordIndexEntry>,
) -> Result<()> {
    if checkpoint.covered_offset != checkpoint_record.record_offset
        || checkpoint.covered_offset < header_len
        || checkpoint.covered_offset > file_len
    {
        return Err(Error::InvalidIndexCheckpoint);
    }
    if checkpoint.entries.len() != observed_prefix_len
        || !checkpoint.entries.iter().zip(observed_prefix).all(
            |(checkpoint_entry, observed_entry)| {
                record_headers_match(checkpoint_entry, observed_entry)
            },
        )
    {
        return Err(Error::InvalidIndexCheckpoint);
    }

    // The records a checkpoint covers are contiguous, because the append log
    // is. `<` accepted a *gap* between one record's end and the next record's
    // start, so a one-entry checkpoint could claim `covered_offset` far past
    // the record it listed and validate: every record in between is on disk,
    // absent from the checkpoint, and therefore absent from `index_entries()`
    // and from every typed read. `!=` is the check that was meant, and the
    // terminal equality below closes the same hole at the far end - without it
    // the gap simply moves to after the last entry.
    let mut previous_offset = header_len;
    for entry in &checkpoint.entries {
        let expected_payload_offset = entry
            .record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::InvalidIndexCheckpoint)?;
        if entry.record_offset < header_len
            || entry.record_offset != previous_offset
            || entry.payload_offset != expected_payload_offset
            || entry.record_offset == checkpoint_record.record_offset
        {
            return Err(Error::InvalidIndexCheckpoint);
        }
        let physical_end = entry
            .checked_physical_end()
            .map_err(|_| Error::InvalidIndexCheckpoint)?;
        if physical_end > checkpoint.covered_offset || physical_end > file_len {
            return Err(Error::InvalidIndexCheckpoint);
        }
        previous_offset = physical_end;
    }
    if previous_offset != checkpoint.covered_offset {
        return Err(Error::InvalidIndexCheckpoint);
    }
    Ok(())
}

pub(super) fn record_headers_match(left: &RecordIndexEntry, right: &RecordIndexEntry) -> bool {
    left.block_id == right.block_id
        && left.block_version == right.block_version
        && left.flags == right.flags
        && left.sequence == right.sequence
        && left.record_offset == right.record_offset
        && left.payload_offset == right.payload_offset
        && left.payload_len == right.payload_len
        && left.checksum == right.checksum
        && left.uncompressed_len_hint == right.uncompressed_len_hint
        && left.footer_offset == right.footer_offset
        && left.prev_same_block_offset == right.prev_same_block_offset
        && left.prev_same_key_offset == right.prev_same_key_offset
        && left.committed == right.committed
}

/// Frames records from `from` to the end of the file.
///
/// `from` is normally the start of the append log, which is also the byte count
/// this charges against `ScanBytes` before framing anything — an open walks the
/// header to reach the log and the ceiling counts those bytes.
///
/// [`StratavaFile::follow`] passes the end of the range it has already framed
/// instead, and charges nothing for the prefix: the bytes before `from` were
/// charged by the open that framed them, and charging them again would make a
/// handle that follows a growing file refuse against a ceiling it never
/// approached. `from` doubles as the initial commit boundary, which is the
/// other half of resuming: a read-only handle's snapshot ends at a commit
/// point, so the tail past it stands or falls on a marker of its own exactly
/// as it would in a scan of the whole file.
#[allow(clippy::too_many_arguments)]
pub(super) fn scan_records_range_at_len<B: ScanBuffer>(
    spec: FormatSpec,
    file: &mut File,
    from: u64,
    prefix_charge: u64,
    resume_commit_end: Option<u64>,
    intent: ScanIntent,
    entries: &mut B,
    file_len: u64,
) -> Result<ScannedIndex> {
    let header_len = from;
    let mut offset = header_len;
    entries.reset();
    let mut scanned = ScannedIndex {
        sequence_high_water: None,
        sequence_high_water_at_commit: None,
        newest: HashMap::new(),
        newest_at_commit: HashMap::new(),
        physical_end: None,
        physical_end_at_commit: None,
    };
    // A commit boundary the *caller* already knows about, because it is where
    // this range begins.
    //
    // It decides `checksum_boundary`, and that decides whether a record whose
    // checksum fails is a recoverable tail the walk stops at or a hard
    // `ChecksumMismatch`. A scan of the whole file has seen the last marker by
    // the time it reaches anything after it, so it stops cleanly; a range that
    // *starts* after that marker had not, so it errored where the open it is
    // supposed to agree with returned `Ok`. Measured under
    // `IntegrityVerification::AtOpen`, on a file whose first uncommitted record
    // was byte-flipped: open `Ok`, follow `Err(ChecksumMismatch { offset: 818 })`.
    //
    // `None` for a scan from the start of the append log, where no commit has
    // happened yet and a failing checksum really is unrecoverable.
    let mut latest_commit_end = resume_commit_end;
    let mut accounting = ScanAccounting::default();
    accounting.advance(spec, prefix_charge)?;
    while offset < file_len {
        let partial_boundary = match spec.commit_policy {
            CommitPolicy::None
                if intent == ScanIntent::Recover
                    && spec.recovery_policy == RecoveryPolicy::TruncateTail =>
            {
                Some(offset)
            }
            CommitPolicy::None => None,
            CommitPolicy::RecordFooter => Some(offset),
            CommitPolicy::TransactionMarker(_) => Some(latest_commit_end.unwrap_or(header_len)),
        };
        let checksum_boundary = if spec.commit_policy.is_transaction_marker() {
            latest_commit_end
        } else {
            None
        };
        // A recovery scan verifies whatever the policy says, because a checksum
        // mismatch is the evidence `RecoveryPolicy::TruncateTail` truncates on:
        // `checksum_boundary` above is only reachable through this check.
        // Dropping it here would turn a recoverable tail into a silently
        // accepted one, which is the failure this whole policy must not create.
        let verify_checksums = intent == ScanIntent::Recover
            || spec
                .read_limits
                .resolve()
                .effective_integrity_verification()
                == IntegrityVerification::AtOpen;
        let entry = match read_record_entry_at(
            spec,
            file,
            file_len,
            offset,
            ScanChecks {
                partial_boundary,
                checksum_boundary,
                verify_checksums,
            },
            &mut accounting,
        )? {
            RecordRead::Entry(entry) => entry,
            RecordRead::RecoverableTail(tail) => match intent {
                ScanIntent::ReadOnly => break,
                ScanIntent::Recover if spec.recovery_policy == RecoveryPolicy::TruncateTail => {
                    file.set_len(tail.truncate_to)?;
                    break;
                }
                ScanIntent::Writer if spec.commit_policy.is_transaction_marker() => break,
                ScanIntent::Writer | ScanIntent::Recover => {
                    return Err(Error::CorruptTail {
                        offset: tail.offset,
                    });
                }
            },
        };
        offset = entry
            .checked_physical_end()
            .map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "record extent",
            })?;
        // Every record contributes its sequence, its block tail and the
        // physical end of the walk even when it is not materialised: the
        // high-water mark is what the next append continues from, the tail is
        // the only way back to a non-resident record, and the end is the byte
        // range a read-only handle may look at. `offset` was just advanced to
        // this record's end.
        scanned.note(&entry, offset);
        let resident = record_is_resident(spec, entry.block_id);
        if resident {
            entries.reserve_entry(spec)?;
        }
        if entry.block_id == INDEX_BLOCK_ID
            && (1..=INDEX_CHECKPOINT_VERSION).contains(&entry.block_version)
        {
            let prefix = entry.read_payload_prefix_file_with_len(
                file,
                file_len,
                INDEX_CHECKPOINT_PREFIX_LEN,
            )?;
            entries.checkpoint_limits(spec, &prefix)?;
        }
        if entry.block_id == COMMIT_BLOCK_ID {
            latest_commit_end = Some(offset);
        }
        if resident {
            entries.push_entry(entry)?;
        }
    }
    entries.finish(spec, &mut scanned)?;
    Ok(scanned)
}

impl RecordIndexEntry {
    pub(super) fn read_payload_file_with_len(
        &self,
        file: &mut File,
        file_len: u64,
    ) -> Result<Vec<u8>> {
        self.validate_payload_extent(file_len)?;
        self.read_payload_file_validated(file)
    }

    /// Reads the first `len` bytes of this record's payload.
    ///
    /// For a segment link, everything the chain walk's first pass needs —
    /// `covered_start`, the entry count and `preceding_records` — sits in the
    /// fixed prefix, so the pass does not have to pull the whole entry array
    /// off disk and throw it away.
    pub(super) fn read_payload_prefix_file_with_len(
        &self,
        file: &mut File,
        file_len: u64,
        len: u64,
    ) -> Result<Vec<u8>> {
        self.validate_payload_extent(file_len)?;
        if self.payload_len < len {
            return Err(Error::InvalidIndexSegment);
        }
        file.seek(SeekFrom::Start(self.payload_offset))?;
        let mut prefix = try_alloc_bytes(len, PHYSICAL_PAYLOAD_RESOURCE)?;
        file.read_exact(&mut prefix)?;
        Ok(prefix)
    }

    pub(super) fn read_payload_file_validated(&self, file: &mut File) -> Result<Vec<u8>> {
        file.seek(SeekFrom::Start(self.payload_offset))?;
        let mut payload = try_alloc_bytes(self.payload_len, PHYSICAL_PAYLOAD_RESOURCE)?;
        file.read_exact(&mut payload)?;
        Ok(payload)
    }

    pub(super) fn validate_payload_extent(&self, file_len: u64) -> Result<()> {
        let payload_end =
            self.payload_offset
                .checked_add(self.payload_len)
                .ok_or(Error::LengthOverflow {
                    value: self.payload_len,
                })?;
        if payload_end > file_len {
            return Err(Error::UnexpectedEof);
        }
        if let Some(footer_offset) = self.footer_offset {
            if footer_offset != payload_end {
                return Err(Error::CorruptTail {
                    offset: self.record_offset,
                });
            }
            if self.checked_physical_end()? > file_len {
                return Err(Error::UnexpectedEof);
            }
        }
        Ok(())
    }

    pub(super) fn logical_payload_len_before_allocation(
        &self,
        spec: FormatSpec,
        file: &mut (impl Read + Seek),
    ) -> Result<u64> {
        if !self.is_compressed() {
            return Ok(self.payload_len);
        }
        validate_record_entry(spec, self)?;
        let compression = variable_compression_for_block(spec, self.block_id)
            .ok_or(Error::InvalidCompressionHeader)?;
        let expected_len = match compression.header_mode {
            CompressionHeaderMode::RecordExplicit => {
                if self.payload_len < 20 {
                    return Err(Error::InvalidCompressionHeader);
                }
                let mut prefix = [0; 20];
                file.seek(SeekFrom::Start(self.payload_offset))?;
                file.read_exact(&mut prefix)?;
                let (_, expected_len, _) = decode_compression_envelope(&prefix)?;
                expected_len
            }
            CompressionHeaderMode::FileExplicit | CompressionHeaderMode::FormatContract => {
                if self.uncompressed_len_hint == 0 {
                    return Err(Error::InvalidCompressionHeader);
                }
                u64::from(self.uncompressed_len_hint)
            }
        };
        if expected_len > compression.max_uncompressed_len {
            return Err(Error::DecompressedLengthLimitExceeded {
                actual: expected_len,
                limit: compression.max_uncompressed_len,
            });
        }
        Ok(expected_len)
    }

    pub(super) fn logical_payload_len_from_snapshot(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
    ) -> Result<u64> {
        validate_record_entry(spec, self)?;
        let compression = variable_compression_for_block(spec, self.block_id)
            .ok_or(Error::InvalidCompressionHeader)?;
        let expected_len = match compression.header_mode {
            CompressionHeaderMode::RecordExplicit => {
                if self.payload_len < 20 {
                    return Err(Error::InvalidCompressionHeader);
                }
                let mut prefix = [0; 20];
                snapshot.read_exact_at(self.payload_offset, &mut prefix)?;
                let (_, expected_len, _) = decode_compression_envelope(&prefix)?;
                expected_len
            }
            CompressionHeaderMode::FileExplicit | CompressionHeaderMode::FormatContract => {
                if self.uncompressed_len_hint == 0 {
                    return Err(Error::InvalidCompressionHeader);
                }
                u64::from(self.uncompressed_len_hint)
            }
        };
        if expected_len > compression.max_uncompressed_len {
            return Err(Error::DecompressedLengthLimitExceeded {
                actual: expected_len,
                limit: compression.max_uncompressed_len,
            });
        }
        Ok(expected_len)
    }

    pub(super) fn verify_snapshot_record(
        &self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
        payload: &[u8],
    ) -> Result<()> {
        if spec.integrity_policy == IntegrityPolicy::None {
            return Ok(());
        }

        let mut header_bytes = [0; RECORD_HEADER_LEN as usize];
        snapshot.read_exact_at(self.record_offset, &mut header_bytes)?;
        let mut header_reader = header_bytes.as_slice();
        let actual = read_native_record_header(&mut header_reader, self.record_offset)?.fields;
        if !record_header_describes(&actual, self) {
            return Err(Error::ChecksumMismatch {
                offset: self.record_offset,
            });
        }

        let mut footer_bytes = [0; RECORD_FOOTER_LEN as usize];
        let footer = if let Some(footer_offset) = self.footer_offset {
            snapshot.read_exact_at(footer_offset, &mut footer_bytes)?;
            let decoded =
                decode_record_footer(spec, &footer_bytes, footer_offset, self.record_offset)?;
            if decoded.prev_same_block_offset != self.prev_same_block_offset
                || decoded.prev_same_key_offset != self.prev_same_key_offset
            {
                return Err(Error::ChecksumMismatch {
                    offset: self.record_offset,
                });
            }
            &footer_bytes[..]
        } else {
            &[]
        };
        let header = RecordHeaderFields {
            block_id: self.block_id,
            block_version: self.block_version,
            flags: self.flags,
            sequence: self.sequence,
            payload_len: self.payload_len,
            checksum: 0,
            uncompressed_len_hint: self.uncompressed_len_hint,
        };
        let actual_checksum =
            checksum_record_fields(spec, self.record_offset, header, payload, footer)?;
        if actual_checksum != self.checksum {
            return Err(Error::ChecksumMismatch {
                offset: self.record_offset,
            });
        }
        Ok(())
    }
}
