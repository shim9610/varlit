// Generation validation and record rewriting: replacement generations,
// streamed record copies and offset chains.
// Included by `file.rs`, whose module it shares.

/// Checks a live file against the record directory that indexes it.
///
/// **What this can still prove, and what it no longer can.** The directory
/// holds each record's offset and nothing else; the entry it produces is
/// rebuilt from the record's own header. So `physical_record_headers_match`
/// below compares a header against itself here and cannot fail — the header
/// fields of an indexed record are no longer a fact the index could disagree
/// with the file about, because the index does not store them.
///
/// What the walk still establishes is everything that was ever load-bearing on
/// this path: the directory's offsets are contiguous from `append_start`, they
/// cover the file exactly (`offset != file_len` is the tail refusal), every
/// record's stored checksum verifies against its bytes, every checkpoint's
/// declared ceilings hold, and no sequence is reused. A file replaced or
/// truncated under the writer fails the first or the second of those.
fn validate_generation_index(
    spec: FormatSpec,
    file: &mut PrimaryFile,
    append_start: u64,
    index: resident_index::Kept<'_>,
) -> Result<()> {
    validate_generation_file_inner(spec, file, append_start, &index, false)
}

/// The same walk against entries the caller is still holding — the rewrite
/// paths, which validate the generation they have just built before it is
/// published and so have no directory for it yet.
fn validate_generation_file(
    spec: FormatSpec,
    file: &mut PrimaryFile,
    append_start: u64,
    expected_entries: &dyn RecordDirectory,
) -> Result<()> {
    validate_generation_file_inner(spec, file, append_start, expected_entries, false)
}

fn validate_replacement_generation_file(
    spec: FormatSpec,
    file: &mut PrimaryFile,
    append_start: u64,
    expected_entries: &dyn RecordDirectory,
) -> Result<()> {
    validate_generation_file_inner(spec, file, append_start, expected_entries, true)
}

/// Strict replacement validation compares checkpoint rows incrementally with
/// the already validated prefix. Other callers screen only the fixed header.
fn validate_generation_file_inner(
    spec: FormatSpec,
    file: &mut PrimaryFile,
    append_start: u64,
    expected_entries: &dyn RecordDirectory,
    strict_prefix: bool,
) -> Result<()> {
    let file_len = check_open_file_len(spec, file)?;
    let header_len = read_file_header(spec, file)?;
    if append_start < header_len || append_start > file_len {
        return Err(Error::InvalidCanonicalEncoding(
            "invalid native append-log boundary",
        ));
    }
    let mut accounting = ScanAccounting::default();
    accounting.advance(spec, append_start)?;
    let mut offset = append_start;
    // Increasing runs with disjoint ranges prove uniqueness without scratch.
    // Overlapping ranges take the bounded external-sort fallback.
    let mut sequences = native_rewrite::SequenceRanges::default();
    let mut walked = 0usize;
    // The walk is sequential, so it reads through a window rather than three
    // small reads per record; checkpoint rows read positionally underneath.
    let mut window = scan::WindowedReader::new(file)?;
    for (position, expected) in directory_records(expected_entries).enumerate() {
        let expected = expected?;
        let expected = &expected;
        walked += 1;
        if expected.record_offset != offset {
            return Err(Error::InvalidCanonicalEncoding(
                "native record offsets are not contiguous",
            ));
        }
        let actual = match read_record_entry_at(
            spec,
            &mut window,
            file_len,
            offset,
            // Generation validation proves a just-published file matches what
            // was indexed; verifying is the point of this walk, so it does not
            // consult the open-time policy.
            ScanChecks {
                partial_boundary: None,
                checksum_boundary: None,
                verify_checksums: true,
            },
            &mut accounting,
        )? {
            RecordRead::Entry(entry) => entry,
            RecordRead::RecoverableTail(tail) => {
                return Err(Error::CorruptTail {
                    offset: tail.offset,
                });
            }
        };
        if !physical_record_headers_match(expected, &actual) {
            return Err(Error::InvalidCanonicalEncoding(
                "native record changed after it was indexed",
            ));
        }
        if actual.block_id == INDEX_BLOCK_ID
            && (1..=INDEX_CHECKPOINT_VERSION).contains(&actual.block_version)
        {
            if strict_prefix {
                native_rewrite::validate_checkpoint(
                    spec,
                    window.inner(),
                    &actual,
                    expected_entries,
                    position,
                    append_start,
                )?;
            } else {
                let prefix = actual.read_payload_prefix_file_with_len(
                    window.inner(),
                    file_len,
                    INDEX_CHECKPOINT_PREFIX_LEN,
                )?;
                check_index_checkpoint_limits_for(spec, &prefix, false)?;
            }
        }
        sequences.note(actual.sequence);
        offset = actual.checked_physical_end()?;
    }
    debug_assert_eq!(
        walked,
        expected_entries.record_count(),
        "declared entry count did not hold"
    );
    if offset != file_len {
        return Err(Error::CorruptTail { offset });
    }
    if sequences.proves_unique() {
        return Ok(());
    }
    native_rewrite::validate_sequences(expected_entries, resident_index::buffer_bytes(spec) / 2)
}

fn physical_record_headers_match(left: &RecordIndexEntry, right: &RecordIndexEntry) -> bool {
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
}

#[derive(Clone, Copy)]
enum RewritePayload<'a> {
    Bytes(&'a [u8]),
    Snapshot { offset: u64, len: u64 },
    Index(IndexPayload<'a>),
}

fn rewrite_record_streaming(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    source: &mut SourceWindow,
    summary: &RewriteSummary,
    output: &mut PrimaryFile,
    mut entry: RecordIndexEntry,
    payload: RewritePayload<'_>,
) -> Result<RecordIndexEntry> {
    let payload_len = match payload {
        RewritePayload::Bytes(bytes) => {
            u64::try_from(bytes.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?
        }
        RewritePayload::Snapshot { len, .. } => len,
        RewritePayload::Index(index) => index.len(spec)?,
    };
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    let logical_len = match payload {
        RewritePayload::Bytes(bytes) => record_logical_len_from_parts(
            spec,
            entry.block_id,
            entry.flags,
            entry.uncompressed_len_hint,
            bytes,
        )?,
        RewritePayload::Snapshot { .. } => entry.logical_payload_len_snapshot(spec, snapshot)?,
        RewritePayload::Index(_) => payload_len,
    };
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;

    let record_offset = output.stream_position()?;
    let payload_offset =
        record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "rewrite file length",
            })?;
    let record_end =
        payload_offset
            .checked_add(payload_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "rewrite file length",
            })?;
    let header = RecordHeaderFields {
        block_id: entry.block_id,
        block_version: entry.block_version,
        flags: entry.flags,
        sequence: entry.sequence,
        payload_len,
        checksum: 0,
        uncompressed_len_hint: entry.uncompressed_len_hint,
    };
    output.begin_record_summarized(record_offset, entry.sequence, record_end, summary);
    entry.record_offset = record_offset;
    entry.payload_offset = payload_offset;
    entry.payload_len = payload_len;
    entry.footer_offset = None;
    entry.prev_same_block_offset = None;
    entry.prev_same_key_offset = None;
    if let Some(checksum) = write_staged_record(
        spec,
        snapshot,
        source,
        output,
        record_offset,
        header,
        &payload,
        &[],
        None,
    )? {
        entry.checksum = checksum;
        return Ok(entry);
    }
    write_record_header(output, spec, record_offset, header)?;
    match payload {
        RewritePayload::Bytes(bytes) => output.write_all(bytes)?,
        RewritePayload::Index(index) => index.write(output)?,
        RewritePayload::Snapshot { offset, len } => {
            snapshot.copy_range_to(offset, len, output)?;
        }
    }

    entry.checksum = match spec.integrity_policy {
        IntegrityPolicy::None => 0,
        IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader => {
            checksum_record_file(spec, output, &entry, &[])?
        }
    };
    output.seek(SeekFrom::Start(record_offset))?;
    write_record_header(
        output,
        spec,
        record_offset,
        RecordHeaderFields {
            checksum: entry.checksum,
            ..header
        },
    )?;
    output.seek(SeekFrom::Start(record_end))?;
    Ok(entry)
}

/// Read-ahead over the source snapshot for one rewrite pass.
///
/// A rewrite copies records in file order, so the payloads it stages are
/// adjacent; reading them through one window turns one read per record into
/// one read per window. Reads the window cannot hold go straight through.
pub(crate) struct SourceWindow {
    buffer: Option<Box<[u8; SOURCE_WINDOW]>>,
    start: u64,
    len: usize,
}

const SOURCE_WINDOW: usize = 16 * 1024;

impl SourceWindow {
    pub(crate) const fn new() -> Self {
        Self {
            buffer: None,
            start: 0,
            len: 0,
        }
    }

    fn read(&mut self, snapshot: &SnapshotFile, offset: u64, out: &mut [u8]) -> Result<()> {
        let want = out.len() as u64;
        let end = offset
            .checked_add(want)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "rewrite source window",
            })?;
        if out.len() > SOURCE_WINDOW / 2 || end > snapshot.len() {
            return snapshot.read_exact_at(offset, out);
        }
        let held = self.start + self.len as u64;
        if self.buffer.is_none() || offset < self.start || end > held {
            if self.buffer.is_none() {
                self.buffer = Some(crate::fallible::try_box([0u8; SOURCE_WINDOW])?);
            }
            let buffer = self.buffer.as_mut().expect("allocated above");
            let len = crate::width::index((snapshot.len() - offset).min(SOURCE_WINDOW as u64))?;
            snapshot.read_exact_at(offset, &mut buffer[..len])?;
            self.start = offset;
            self.len = len;
        }
        let buffer = self.buffer.as_ref().expect("filled above");
        let at = crate::width::index(offset - self.start)?;
        out.copy_from_slice(&buffer[at..at + out.len()]);
        Ok(())
    }
}

/// The largest record a rewrite assembles in memory before writing it.
///
/// Header, payload and footer of a record this small are staged in one stack
/// buffer, checksummed there and written with one call. The streamed route
/// writes the header, the payload and the footer separately, reads the payload
/// back to checksum it, and seeks back to rewrite the header: four writes and
/// at least one read per record, which dominated the cost of a whole-file
/// rewrite of small records.
const STAGED_RECORD_LEN: usize = 4096;

/// Writes a small rewritten record in one call and returns its checksum, or
/// `None` when the record must take the streamed route (too large, an index
/// payload, or a checksum this build cannot compute).
///
/// `verify` names the source record a snapshot copy must still match; its
/// checksum is recomputed from the staged bytes before anything is written,
/// exactly as the streamed copy does.
#[allow(clippy::too_many_arguments)]
fn write_staged_record(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    source: &mut SourceWindow,
    output: &mut PrimaryFile,
    record_offset: u64,
    header: RecordHeaderFields,
    payload: &RewritePayload<'_>,
    footer: &[u8],
    verify: Option<&RecordIndexEntry>,
) -> Result<Option<u32>> {
    if cfg!(not(feature = "integrity")) && spec.integrity_policy != IntegrityPolicy::None {
        return Ok(None);
    }
    let header_len = crate::width::small(RECORD_HEADER_LEN);
    let payload_len = match payload {
        RewritePayload::Bytes(bytes) => bytes.len(),
        RewritePayload::Snapshot { len, .. } => match usize::try_from(*len) {
            Ok(len) => len,
            Err(_) => return Ok(None),
        },
        RewritePayload::Index(_) => return Ok(None),
    };
    let Some(total) = header_len
        .checked_add(payload_len)
        .and_then(|len| len.checked_add(footer.len()))
        .filter(|&total| total <= STAGED_RECORD_LEN)
    else {
        return Ok(None);
    };
    let mut buffer = [0u8; STAGED_RECORD_LEN];
    let payload_end = header_len + payload_len;
    match payload {
        RewritePayload::Bytes(bytes) => buffer[header_len..payload_end].copy_from_slice(bytes),
        RewritePayload::Snapshot { offset, .. } => {
            source.read(snapshot, *offset, &mut buffer[header_len..payload_end])?;
        }
        RewritePayload::Index(_) => unreachable!("index payloads stream"),
    }
    buffer[payload_end..total].copy_from_slice(footer);

    let footer_len = record_footer_len(spec);
    let crc_of = |fields: RecordHeaderFields, at: u64, footer: &[u8]| -> Result<u32> {
        let mut hash = crc32fast::Hasher::new();
        if spec.integrity_policy == IntegrityPolicy::Crc32WithHeader {
            hash.update(&encode_native_record_header(fields, at, footer_len)?);
        }
        hash.update(&buffer[header_len..payload_end]);
        hash.update(checksummed_footer(spec, footer));
        Ok(hash.finalize())
    };
    if let Some(source) = verify
        && spec.integrity_policy != IntegrityPolicy::None
    {
        let source_footer = if spec.spec_needs_record_footer() {
            Some(encode_record_footer(RecordFooterFields {
                prev_same_block_offset: source.prev_same_block_offset,
                prev_same_key_offset: source.prev_same_key_offset,
                mutable_flags: source.mutable_flags,
            })?)
        } else {
            None
        };
        let actual = crc_of(
            RecordHeaderFields {
                block_id: source.block_id,
                block_version: source.block_version,
                flags: source.flags,
                sequence: source.sequence,
                payload_len: source.payload_len,
                checksum: 0,
                uncompressed_len_hint: source.uncompressed_len_hint,
            },
            source.record_offset,
            source_footer.as_ref().map_or(&[][..], |bytes| &bytes[..]),
        )?;
        if actual != source.checksum {
            return Err(Error::ChecksumMismatch {
                offset: source.record_offset,
            });
        }
    }
    let checksum = match spec.integrity_policy {
        IntegrityPolicy::None => 0,
        IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader => {
            crc_of(header, record_offset, footer)?
        }
    };
    buffer[..header_len].copy_from_slice(&encode_native_record_header(
        RecordHeaderFields { checksum, ..header },
        record_offset,
        footer_len,
    )?);
    output.write_all(&buffer[..total])?;
    Ok(Some(checksum))
}

/// Which backward chain a link belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chain {
    Block,
    Key,
}

/// How a whole-file rewrite maps an old record offset to a new one.
///
/// The two rewrites in this crate move records for different reasons, and the
/// arithmetic that is correct for one is silently wrong for the other.
///
/// `replace_block` resizes **exactly one** record and removes none, so every
/// record after the target moves by the same amount and every record before it
/// does not move at all. That is `Uniform`, and it is why
/// [`ReplacementInfo::translate_record_offset`] can be one subtraction.
///
/// A defragmenting rewrite drops records, so each survivor moves by a different
/// amount and some chain predecessors are not in the new file at all. A uniform
/// delta there points a link at a byte offset that is not where that record
/// went.
///
/// **How badly that fails was measured rather than assumed.** Substituting
/// `Uniform` into the defragmenting rewrite does not publish a corrupt file: it
/// is refused at write time with `InvalidRecordFooter { offset: 616 }`, because
/// `validate_replacement_predecessors` resolves every link against the prefix
/// already written and requires it to name an *earlier record of the same
/// block*. Two of the four defragment tests fail that way, and nothing reaches
/// the pathname. So the honest claim is that this type is required for the
/// operation to work at all, not that it is the last thing between a caller and
/// silent corruption — that guard is the predecessor check, and it is older
/// than this.
///
/// `Tables` stores both chain resolutions in one sorted, spillable row per
/// source record. Dropped records inherit their predecessor in each chain;
/// zero means that no predecessor in that chain survived.
enum ChainRemap<'a> {
    Uniform(ReplacementInfo),
    Tables(&'a OffsetRemap),
}

impl ChainRemap<'_> {
    fn translate(&self, chain: Chain, old: u64) -> Result<Option<u64>> {
        match self {
            Self::Uniform(info) => info.translate_record_offset(old).map(Some),
            Self::Tables(remap) => remap.get(old, chain),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn rewrite_replacement_record_streaming(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    source: &mut SourceWindow,
    summary: &RewriteSummary,
    output: &mut PrimaryFile,
    mut entry: RecordIndexEntry,
    payload: RewritePayload<'_>,
    replacement: ChainRemap<'_>,
    rewritten_prefix: &RewriteDirectory,
) -> Result<RecordIndexEntry> {
    let source_entry = matches!(payload, RewritePayload::Snapshot { .. }).then(|| entry.clone());
    let payload_len = match payload {
        RewritePayload::Bytes(bytes) => {
            u64::try_from(bytes.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?
        }
        RewritePayload::Snapshot { len, .. } => len,
        RewritePayload::Index(index) => index.len(spec)?,
    };
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    let logical_len = match payload {
        RewritePayload::Bytes(bytes) => record_logical_len_from_parts(
            spec,
            entry.block_id,
            entry.flags,
            entry.uncompressed_len_hint,
            bytes,
        )?,
        RewritePayload::Snapshot { .. } => entry.logical_payload_len_snapshot(spec, snapshot)?,
        RewritePayload::Index(_) => payload_len,
    };
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, logical_len)?;

    let record_offset = output.stream_position()?;
    let payload_offset =
        record_offset
            .checked_add(RECORD_HEADER_LEN)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "replacement file length",
            })?;
    let payload_end =
        payload_offset
            .checked_add(payload_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "replacement file length",
            })?;
    let record_end = payload_end.checked_add(record_footer_len(spec)).ok_or(
        Error::ResourceArithmeticOverflow {
            resource: "replacement file length",
        },
    )?;

    entry.record_offset = record_offset;
    entry.payload_offset = payload_offset;
    entry.payload_len = payload_len;
    entry.footer_offset = spec.spec_needs_record_footer().then_some(payload_end);
    entry.prev_same_block_offset = entry
        .prev_same_block_offset
        .map(|offset| replacement.translate(Chain::Block, offset))
        .transpose()?
        .flatten();
    entry.prev_same_key_offset = entry
        .prev_same_key_offset
        .map(|offset| replacement.translate(Chain::Key, offset))
        .transpose()?
        .flatten();

    let header = RecordHeaderFields {
        block_id: entry.block_id,
        block_version: entry.block_version,
        flags: entry.flags,
        sequence: entry.sequence,
        payload_len,
        checksum: 0,
        uncompressed_len_hint: entry.uncompressed_len_hint,
    };
    output.begin_record_summarized(record_offset, entry.sequence, record_end, summary);
    let staged_footer = if spec.spec_needs_record_footer() {
        Some(encode_record_footer(RecordFooterFields {
            prev_same_block_offset: entry.prev_same_block_offset,
            prev_same_key_offset: entry.prev_same_key_offset,
            // Carried, not reset: a republish moves a record, it does not bring
            // it back to life.
            mutable_flags: entry.mutable_flags,
        })?)
    } else {
        None
    };
    if let Some(checksum) = write_staged_record(
        spec,
        snapshot,
        source,
        output,
        record_offset,
        header,
        &payload,
        staged_footer.as_ref().map_or(&[][..], |bytes| &bytes[..]),
        source_entry.as_ref(),
    )? {
        // The predecessor check reads the record back, so it runs after the
        // write; a refusal discards the whole temporary file.
        validate_replacement_predecessors(output, &entry, rewritten_prefix)?;
        output.seek(SeekFrom::Start(record_end))?;
        entry.checksum = checksum;
        return Ok(entry);
    }
    write_record_header(output, spec, record_offset, header)?;
    let mut copied_crc = None;
    match payload {
        RewritePayload::Bytes(bytes) => output.write_all(bytes)?,
        RewritePayload::Index(index) => index.write(output)?,
        RewritePayload::Snapshot { offset, len } => {
            if spec.integrity_policy == IntegrityPolicy::None {
                snapshot.copy_range_to(offset, len, output)?;
            } else {
                copied_crc = Some(copy_verified_snapshot_record(
                    spec,
                    snapshot,
                    output,
                    source_entry.as_ref().unwrap(),
                    offset,
                    len,
                )?);
            }
        }
    }
    let encoded_footer = staged_footer;
    if let Some(footer) = &encoded_footer {
        output.write_all(footer)?;
    }
    let footer = encoded_footer.as_ref().map_or(&[][..], |bytes| &bytes[..]);
    validate_replacement_predecessors(output, &entry, rewritten_prefix)?;

    entry.checksum = match spec.integrity_policy {
        IntegrityPolicy::None => 0,
        IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader => {
            if let Some(mut hash) = copied_crc {
                hash.update(checksummed_footer(spec, footer));
                hash.finalize()
            } else {
                checksum_record_file(spec, output, &entry, footer)?
            }
        }
    };
    output.seek(SeekFrom::Start(record_offset))?;
    write_record_header(
        output,
        spec,
        record_offset,
        RecordHeaderFields {
            checksum: entry.checksum,
            ..header
        },
    )?;
    output.seek(SeekFrom::Start(record_end))?;
    Ok(entry)
}

/// Validate the copied source and checksum the replacement in one payload pass.
/// Snapshot copies keep the header fields intact. Forking the CRC state before
/// the changed footer avoids a second read or hash of the payload.
#[cfg(feature = "integrity")]
fn copy_verified_snapshot_record(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    output: &mut PrimaryFile,
    source: &RecordIndexEntry,
    offset: u64,
    len: u64,
) -> Result<crc32fast::Hasher> {
    if offset != source.payload_offset || len != source.payload_len {
        return Err(Error::InvalidCanonicalEncoding(
            "rewrite source payload changed",
        ));
    }
    // Snapshot copies preserve all header fields covered by the CRC. Only
    // physical offsets (not stored in the header) and footer links change.
    let mut payload = crc32fast::Hasher::new();
    if spec.integrity_policy == IntegrityPolicy::Crc32WithHeader {
        payload.update(&encode_native_record_header(
            RecordHeaderFields {
                block_id: source.block_id,
                block_version: source.block_version,
                flags: source.flags,
                sequence: source.sequence,
                payload_len: source.payload_len,
                checksum: 0,
                uncompressed_len_hint: source.uncompressed_len_hint,
            },
            source.record_offset,
            record_footer_len(spec),
        )?);
    }
    if len <= 1024 {
        copy_snapshot_payload_crc::<1024>(&mut payload, snapshot, output, offset, len)?;
    } else {
        copy_snapshot_payload_crc::<STREAM_BUFFER_LEN>(
            &mut payload,
            snapshot,
            output,
            offset,
            len,
        )?;
    }
    let checksum = |footer: &[u8]| {
        let mut hash = payload.clone();
        hash.update(checksummed_footer(spec, footer));
        hash.finalize()
    };
    let source_footer = if spec.spec_needs_record_footer() {
        Some(encode_record_footer(RecordFooterFields {
            prev_same_block_offset: source.prev_same_block_offset,
            prev_same_key_offset: source.prev_same_key_offset,
            mutable_flags: source.mutable_flags,
        })?)
    } else {
        None
    };
    let actual = checksum(source_footer.as_ref().map_or(&[][..], |bytes| &bytes[..]));
    if actual != source.checksum {
        return Err(Error::ChecksumMismatch {
            offset: source.record_offset,
        });
    }
    Ok(payload)
}

// Keeping each sized array in its own call frame avoids reserving the wide
// frame when a small record is copied. Source CRC and footer validation remain
// in the caller and still precede publication.
#[cfg(feature = "integrity")]
#[inline(never)]
fn copy_snapshot_payload_crc<const BUFFER: usize>(
    payload: &mut crc32fast::Hasher,
    snapshot: &SnapshotFile,
    output: &mut PrimaryFile,
    offset: u64,
    len: u64,
) -> Result<()> {
    const {
        assert!(BUFFER > 0);
    }
    let mut remaining = len;
    let mut at = offset;
    let mut buffer = [0; BUFFER];
    while remaining != 0 {
        let count = crate::width::index(remaining.min(buffer.len() as u64))?;
        snapshot.read_exact_at(at, &mut buffer[..count])?;
        output.write_all(&buffer[..count])?;
        payload.update(&buffer[..count]);
        remaining -= count as u64;
        at = at
            .checked_add(count as u64)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "rewrite payload offset",
            })?;
    }
    Ok(())
}

#[cfg(not(feature = "integrity"))]
fn copy_verified_snapshot_record(
    _spec: FormatSpec,
    _snapshot: &SnapshotFile,
    _output: &mut PrimaryFile,
    _source: &RecordIndexEntry,
    _offset: u64,
    _len: u64,
) -> Result<crc32fast::Hasher> {
    Err(Error::IntegrityFeatureDisabled)
}

fn validate_replacement_predecessors(
    output: &mut PrimaryFile,
    entry: &RecordIndexEntry,
    rewritten_prefix: &RewriteDirectory,
) -> Result<()> {
    if let Some(offset) = entry.prev_same_block_offset {
        let Some(previous) = rewritten_prefix.find_offset(offset)? else {
            return Err(Error::InvalidRecordFooter { offset });
        };
        if offset >= entry.record_offset || previous.block_id != entry.block_id {
            return Err(Error::InvalidRecordFooter { offset });
        }
    }
    if let Some(offset) = entry.prev_same_key_offset {
        let Some(previous) = rewritten_prefix.find_offset(offset)? else {
            return Err(Error::InvalidRecordFooter { offset });
        };
        if offset >= entry.record_offset
            || replacement_chain_block_id(output, &previous)?
                != replacement_chain_block_id(output, entry)?
        {
            return Err(Error::InvalidRecordFooter { offset });
        }
    }
    Ok(())
}

fn replacement_chain_block_id(file: &mut PrimaryFile, entry: &RecordIndexEntry) -> Result<u32> {
    if !matches!(entry.block_id, TOMBSTONE_BLOCK_ID | OP_BLOCK_ID) {
        return Ok(entry.block_id);
    }
    if entry.payload_len < 4 {
        return Err(Error::InvalidRecordFooter {
            offset: entry.record_offset,
        });
    }
    file.seek(SeekFrom::Start(entry.payload_offset))?;
    let mut block_id = [0; 4];
    file.read_exact(&mut block_id)?;
    Ok(u32::from_le_bytes(block_id))
}
