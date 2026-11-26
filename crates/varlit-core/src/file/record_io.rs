// File and record header I/O: reading and writing the file header,
// record headers and footers, their checks and the native resource limits.
// Included by `file.rs`, whose module it shares.

/// Write the header this spec describes, and hand back the extension region it
/// put there so the caller can record what this file actually carries.
pub(crate) fn write_file_header(
    spec: FormatSpec,
    file: &mut crate::primary_file::PrimaryFile,
) -> Result<Vec<u8>> {
    let extensions = file_header_extensions(spec)?;
    // The header precedes every anchor; the layout is installed after it.
    file.set_layout(None);
    let header_len = write_native_file_header(file, spec, &extensions)?;
    file.set_layout(spec.effective_anchors().layout(header_len));
    Ok(extensions)
}

/// [`write_file_header`] for a stream writer's new file, which also carries
/// the published end (see `stream_file_header_extensions`).
pub(crate) fn write_stream_file_header(
    spec: FormatSpec,
    file: &mut crate::primary_file::PrimaryFile,
) -> Result<Vec<u8>> {
    let extensions = stream_file_header_extensions(spec)?;
    file.set_layout(None);
    let header_len = write_native_file_header(file, spec, &extensions)?;
    file.set_layout(spec.effective_anchors().layout(header_len));
    Ok(extensions)
}

pub(crate) fn read_file_header(spec: FormatSpec, file: &mut (impl Read + Seek)) -> Result<u64> {
    Ok(read_file_header_parts(spec, file)?.0)
}

/// The header length *and* the extension region the file carries.
///
/// The two are not interchangeable with what `spec` would produce: once an
/// unknown block is skippable rather than refused, a file's region can be
/// longer than this build would write, and every offset derived from the header
/// has to come from the file rather than from the spec.
pub(crate) fn read_file_header_parts(
    spec: FormatSpec,
    file: &mut (impl Read + Seek),
) -> Result<(u64, Vec<u8>)> {
    let (header_len, extensions, _) = read_file_header_anchored(spec, file)?;
    Ok((header_len, extensions))
}

/// [`read_file_header_parts`] and the anchor policy the file was written with.
pub(crate) fn read_file_header_anchored(
    spec: FormatSpec,
    file: &mut (impl Read + Seek),
) -> Result<(u64, Vec<u8>, crate::anchor::AnchorPolicy)> {
    file.seek(SeekFrom::Start(0))?;
    let header = read_native_file_header(file, spec)?;
    let hash = header.schema_hash;
    if spec.schema_hash != 0 && hash != spec.schema_hash {
        return Err(Error::SchemaHashMismatch {
            expected: spec.schema_hash,
            actual: hash,
        });
    }
    if uses_format_contract_compression(spec) {
        let computed = spec.computed_schema_hash();
        if hash != computed {
            return Err(Error::SchemaHashMismatch {
                expected: computed,
                actual: hash,
            });
        }
    }
    if header.has_extension_len {
        validate_file_header_extensions(spec, &header.extensions)?;
        Ok((header.header_len, header.extensions, header.anchors))
    } else {
        if uses_file_explicit_compression(spec) {
            return Err(Error::InvalidCompressionHeader);
        }
        Ok((native_file_header_len(spec, 0), Vec::new(), header.anchors))
    }
}

/// Opens the primary at `path` read-only, with the anchors its header records.
fn open_primary_by_path(spec: FormatSpec, path: &Path) -> Result<PrimaryFile> {
    let mut file = PrimaryFile::new(
        crate::primary_file::open_data_file(std::fs::OpenOptions::new().read(true), path)?,
        None,
    );
    read_primary_header(spec, &mut file)?;
    Ok(file)
}

/// Reads the header of a primary handle and installs the anchor layout it
/// records, so every later offset on the handle is logical. Anchors count
/// from the end of the header: a matrix format, whose append log starts after
/// its matrix region, is never anchored (`FormatSpec::effective_anchors`).
pub(crate) fn read_primary_header(
    spec: FormatSpec,
    file: &mut crate::primary_file::PrimaryFile,
) -> Result<(u64, Vec<u8>)> {
    file.set_layout(None);
    let (header_len, extensions, anchors) = read_file_header_anchored(spec, file)?;
    file.set_layout(anchors.layout(header_len));
    Ok((header_len, extensions))
}

/// Produces a fresh 128-bit matrix creation nonce.
///
/// This is a uniqueness value, not a cryptographic secret: it only has to
/// differ between two `create_with_dims` calls that reuse the same OS file
/// object. Wall-clock nanoseconds, the process id and a per-process counter
/// are mixed through FNV-1a into two independent 64-bit lanes.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the clock is split into its 64-bit halves and mixed; the low bits are the point"
)]
fn fresh_matrix_creation_nonce() -> [u8; MATRIX_CREATION_NONCE_LEN] {
    use std::sync::atomic::{AtomicU64, Ordering};

    static CREATION_COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = CREATION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let pid = u64::from(std::process::id());

    const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
    const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;
    let mut nonce = [0u8; MATRIX_CREATION_NONCE_LEN];
    for lane in 0usize..2 {
        let mut hash = FNV_OFFSET_BASIS;
        for word in [
            lane as u64,
            nanos as u64,
            (nanos >> 64) as u64,
            pid,
            counter,
        ] {
            for byte in word.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(FNV_PRIME);
            }
        }
        nonce[lane * 8..(lane + 1) * 8].copy_from_slice(&hash.to_le_bytes());
    }
    // The raw entropy words survive the mix: XOR them back in so even a
    // degenerate clock (nanos == 0 on every call) cannot collapse two creates
    // in the same process to the same nonce (the counter still differs).
    for (index, byte) in nanos
        .to_le_bytes()
        .iter()
        .chain(counter.to_le_bytes().iter())
        .take(MATRIX_CREATION_NONCE_LEN)
        .enumerate()
    {
        nonce[index] ^= byte.rotate_left((index % 7) as u32);
    }
    nonce
}

/// The limits a native format must declare before it may be written or opened.
///
/// **`FileLen` is not among them, and that is the point.** Requiring a
/// declaration for a ceiling that nothing enforces is ceremony: the twenty-one
/// enforcement sites were removed in this round, and a required declaration
/// that no check consults refuses a format for a number that could not have
/// changed any outcome. `max_file_len` remains on `FormatSpec` and the DSL
/// still accepts `limits { file_len: .. }`, so no format definition breaks —
/// the value is simply inert.
///
/// The four that remain each bound a real allocation, and each has a check that
/// consults it.
pub(crate) fn ensure_native_write_limits(spec: FormatSpec) -> Result<()> {
    for key in [
        ReadLimitKey::Records,
        ReadLimitKey::IndexBytes,
        ReadLimitKey::RecordPayloadLen,
        ReadLimitKey::LogicalPayloadLen,
    ] {
        spec.read_limits.require(key)?;
    }
    Ok(())
}

pub(crate) fn ensure_native_open_limits(spec: FormatSpec) -> Result<()> {
    ensure_native_write_limits(spec)?;
    spec.read_limits.require(ReadLimitKey::ScanBytes)?;
    Ok(())
}

/// Whether an anchor interrupts the logical range `[offset, offset + len)`.
#[cfg(feature = "mmap")]
pub(crate) fn anchored_range_is_split(
    layout: &crate::anchor::AnchorLayout,
    offset: u64,
    len: u64,
) -> bool {
    if len < 2 {
        return false;
    }
    match (layout.physical(offset), layout.physical(offset + len - 1)) {
        (Some(first), Some(last)) => last - first != len - 1,
        _ => true,
    }
}

pub(crate) fn check_open_file_len(_spec: FormatSpec, file: &PrimaryFile) -> Result<u64> {
    Ok(file.len()?)
}

fn ensure_matrix_byte_copy_compatible<From, To>() -> Result<()>
where
    From: VarlitMatrixBlock,
    To: VarlitMatrixBlock,
{
    if From::DIMENSIONS != To::DIMENSIONS || From::SLOT_STRIDE != To::SLOT_STRIDE {
        return Err(Error::InvalidFormatSpec(
            "matrix byte-copy requires matching dimensions and slot_stride",
        ));
    }
    Ok(())
}

fn record_footer_len(spec: FormatSpec) -> u64 {
    if spec.spec_needs_record_footer() {
        native_record_footer_len()
    } else {
        0
    }
}

fn nonzero_offset(offset: u64) -> Option<u64> {
    if offset == 0 { None } else { Some(offset) }
}

const fn limit_or_max(limit: Option<u64>) -> u64 {
    match limit {
        Some(limit) => limit,
        None => u64::MAX,
    }
}

fn ensure_payload_limit(resource: &'static str, actual: u64, limit: u64) -> Result<()> {
    if actual > limit {
        Err(Error::LimitExceeded {
            resource,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

fn index_bytes_for_count(count: usize) -> Result<u64> {
    let count = u64::try_from(count).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "index bytes",
    })?;
    let entry_size = u64::try_from(size_of::<RecordIndexEntry>()).map_err(|_| {
        Error::ResourceArithmeticOverflow {
            resource: "index bytes",
        }
    })?;
    count
        .checked_mul(entry_size)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "index bytes",
        })
}

fn allocation_bytes<T>(count: usize, resource: &'static str) -> Result<u64> {
    u64::try_from(count)
        .map_err(|_| Error::ResourceArithmeticOverflow { resource })?
        .checked_mul(size_of::<T>() as u64)
        .ok_or(Error::ResourceArithmeticOverflow { resource })
}

#[cfg(feature = "mmap")]
fn mmap_index_bytes_for_count(count: usize, verify: bool) -> Result<u64> {
    let count = u64::try_from(count).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "mmap index bytes",
    })?;
    // One index entry copy plus one `by_block` position per record; the
    // former second full entry copy for the membership hash set is gone
    //
    let entry_bytes = u64::try_from(size_of::<RecordIndexEntry>())
        .map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "mmap index bytes",
        })?
        .checked_add(size_of::<usize>() as u64)
        .and_then(|bytes| bytes.checked_add(u64::from(verify)))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "mmap index bytes",
        })?;
    count
        .checked_mul(entry_bytes)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "mmap index bytes",
        })
}

/// Bytes the sequence-uniqueness witness allocates for `records` entries.
///
/// This is the transient every resident input open can pay on top of its
/// resident index, so [`KeyedMergeEstimate`] has to charge it.
fn sequence_uniqueness_transient_bytes(records: usize) -> Result<u64> {
    u64::try_from(records)
        .map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "sequence uniqueness index",
        })?
        .checked_mul(size_of::<u64>() as u64)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "sequence uniqueness index",
        })
}

/// Rejects a native file that reuses a record sequence.
///
/// Fast path : the writer hands out sequences from a monotonically
/// increasing counter, so an ordinary append-only file scanned in offset order
/// is strictly increasing. Verifying that costs one allocation-free pass and
/// proves uniqueness outright. Only an input whose records are *not* in
/// ascending sequence order - a rewritten or externally reordered generation -
/// falls back to the sorted copy, which costs `O(N log N)` time and an explicit
/// `8N` temporary. The fallback is still the published worst case, so the
/// resident merge estimate charges it unconditionally rather than assuming the
/// fast path.
fn validate_unique_sequences(entries: &[RecordIndexEntry]) -> Result<()> {
    if entries
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence)
    {
        return Ok(());
    }
    let requested = sequence_uniqueness_transient_bytes(entries.len())?;
    let mut sequences = Vec::new();
    sequences
        .try_reserve_exact(entries.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "sequence uniqueness index",
            requested,
        })?;
    sequences.extend(entries.iter().map(|entry| entry.sequence));
    sequences.sort_unstable();
    if sequences.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(Error::InvalidCanonicalEncoding(
            "duplicate native record sequence",
        ));
    }
    Ok(())
}

/// The three per-handle structures every open and every generation rebind
/// derives from the resident index.
struct DerivedIndexState {
    segment_cursor: SegmentCursor,
    /// `None` when the caller did not ask for tails, which an open does not:
    /// it already has them from the scan or the chain walk, and building the
    /// newest-per-block map here would be work thrown away.
    block_tails: Option<BlockTails>,
}

/// Derives all three in ONE forward pass.
///
/// They used to be three separate walks of the same entries — one forward
/// (`BlockTails`) and reverse ones (`SegmentCursor`) — run
/// back to back at every open and at both generation rebinds. The two reverse
/// walks stop at the newest checkpoint and the newest segment respectively, so
/// on a file that carries neither they each walk the whole index; the forward
/// one has no early exit at all. Three traversals, and a single forward pass
/// answers all of them, because "the last entry with this block id" is just the
/// last one a forward pass saw.
///
/// Each field is still computed by exactly the rule its own `from_index`
/// states, so the invariants those doc comments assert are unchanged. The unit
/// tests below compare this against all three.
fn derive_index_state(
    entries: impl Iterator<Item = Result<RecordIndexEntry>>,
    with_tails: bool,
) -> Result<DerivedIndexState> {
    let mut touched: u64 = 0;
    let mut newest: HashMap<u32, u64> = HashMap::new();
    // Position of, and entry at, the newest record of each kind the derived
    // structures key on.
    let mut newest_segment: Option<(usize, u64)> = None;
    for (position, entry) in entries.enumerate() {
        let entry = entry?;
        touched = touched.saturating_add(1);
        if with_tails {
            if newest.len() == newest.capacity() && !newest.contains_key(&entry.block_id) {
                newest.try_reserve(1).map_err(|_| Error::AllocationFailed {
                    resource: "derived block tails",
                    requested: (newest.len() as u64 + 1).saturating_mul(32),
                })?;
            }
            newest.insert(entry.block_id, entry.record_offset);
        }
        if entry.block_id == SEGMENT_BLOCK_ID {
            newest_segment = Some((position, entry.physical_end()));
        }
    }
    let state = DerivedIndexState {
        segment_cursor: match newest_segment {
            Some((position, physical_end)) => SegmentCursor {
                next_position: position + 1,
                covered_start: Some(physical_end),
            },
            None => SegmentCursor::new_empty(),
        },
        block_tails: with_tails
            .then(|| BlockTails::from_newest(&newest))
            .transpose()?,
    };
    if with_tails {
        note_block_tail_index_touches(touched);
    }
    Ok(state)
}

/// Borrowed entries as the fallible stream [`derive_index_state`] consumes.
///
/// The open paths still hold a plain `Vec` at the point they derive from it —
/// they built it moments earlier — so their entries cannot fail to be produced.
/// This says that in one place instead of at each call site.
#[cfg(test)]
fn infallible_entries(
    entries: &[RecordIndexEntry],
) -> impl DoubleEndedIterator<Item = Result<RecordIndexEntry>> + ExactSizeIterator + '_ {
    entries.iter().cloned().map(Ok)
}

fn read_record_header(
    file: &mut (impl Read + Seek),
    _spec: FormatSpec,
    offset: u64,
) -> Result<RecordIndexEntry> {
    let decoded = read_native_record_header(file, offset)?;
    debug_assert_eq!(decoded.lead_in_len, RECORD_HEADER_LEN);
    let header = decoded.fields;
    Ok(RecordIndexEntry {
        block_id: header.block_id,
        block_version: header.block_version,
        flags: header.flags,
        sequence: header.sequence,
        record_offset: offset,
        payload_offset: offset
            .checked_add(decoded.lead_in_len)
            .ok_or(Error::CorruptTail { offset })?,
        payload_len: header.payload_len,
        checksum: header.checksum,
        uncompressed_len_hint: header.uncompressed_len_hint,
        footer_offset: None,
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        mutable_flags: 0,
        committed: true,
    })
}

fn write_record_header(
    file: &mut PrimaryFile,
    spec: FormatSpec,
    record_offset: u64,
    header: RecordHeaderFields,
) -> Result<()> {
    let footer_len = record_footer_len(spec);
    debug_assert_eq!(native_record_header_len(), RECORD_HEADER_LEN);
    write_native_record_header(file, header, record_offset, footer_len)
}

fn encode_record_footer(
    footer: RecordFooterFields,
) -> Result<[u8; crate::width::small(RECORD_FOOTER_LEN)]> {
    encode_native_record_footer(footer)
}

/// Rebuilds a record's index entry from the record itself, positionally.
///
/// **`&self` throughout, which is the whole point.** `read_record_entry_at`
/// takes `&mut PrimaryFile` and seeks, so it cannot serve an `&self` read path.
/// A demand-filled index uses that path: every read of an entry the index no
/// longer holds has to come
/// from the file, under `&self`. This reads the two fixed-size regions through
/// `SnapshotFile::read_exact_at` and hands them to the same decoders the
/// scanning reader uses, so there is one decode of a record header in the
/// crate and not two.
///
/// `committed` is the caller's, because it is writer state with no
/// representation in the record — proved, not assumed, by
/// `every_index_entry_can_be_rebuilt_from_its_own_record`.
///
/// This is what `ResidentIndex` calls for every entry it produces.
/// `every_index_entry_can_be_rebuilt_from_its_own_record` is the proof that
/// what comes back matches what the scanning reader would have built.
/// Whether the record header actually at an offset is the record an entry
/// claims is there.
///
/// Two callers ask exactly this, under opposite conditions, which is why the
/// seven fields are listed once rather than twice.
/// [`RecordIndexEntry::verify_snapshot_record`] asks it as part of integrity
/// verification and returns early when no checksum is declared;
/// [`VarlitFile::check_directory_describes_this_file`] asks it of a
/// caller-supplied directory and exists *above all* for the no-checksum case,
/// where nothing else would notice. A field added to the record header has to
/// arrive here or both of them silently stop covering it.
fn record_header_describes(actual: &RecordHeaderFields, stated: &RecordIndexEntry) -> bool {
    actual.block_id == stated.block_id
        && actual.block_version == stated.block_version
        && actual.flags == stated.flags
        && actual.sequence == stated.sequence
        && actual.payload_len == stated.payload_len
        && actual.checksum == stated.checksum
        && actual.uncompressed_len_hint == stated.uncompressed_len_hint
}

pub(crate) fn fault_record_entry(
    snapshot: &SnapshotFile,
    spec: FormatSpec,
    record_offset: u64,
    committed: bool,
) -> Result<RecordIndexEntry> {
    fault_record_entry_prefixed::<{ crate::width::small(RECORD_HEADER_LEN) }>(
        snapshot,
        spec,
        record_offset,
        committed,
    )
}

/// A mutation's small fixed prefix can frame the header and footer with one
/// positional read. Ordinary directory reads retain their header-only I/O;
/// large records use the existing separate footer read. No heap buffer or
/// persistent cache is added, and every prefix stays inside the snapshot.
fn fault_record_entry_prefixed<const PREFIX: usize>(
    snapshot: &SnapshotFile,
    spec: FormatSpec,
    record_offset: u64,
    committed: bool,
) -> Result<RecordIndexEntry> {
    const {
        assert!(PREFIX >= crate::width::small(RECORD_HEADER_LEN));
    }
    fault_record_entry_with::<PREFIX>(
        snapshot.len(),
        spec,
        record_offset,
        committed,
        |at, bytes| snapshot.read_exact_at(at, bytes),
    )
}

#[inline]
fn mark_prefers_header_only(snapshot_len: u64, records: usize) -> bool {
    // An I/O hint, never a boundary witness. Avoid copying an unused payload
    // prefix when observed file geometry is dominated by larger records.
    // Small directories keep the prefix because container metadata can dominate
    // their average. Saturation only selects the existing wider read route.
    records >= 32 && snapshot_len > (records as u64).saturating_mul(512)
}

fn fault_record_entry_for_mark_with(
    snapshot_len: u64,
    records: usize,
    spec: FormatSpec,
    record_offset: u64,
    read: impl FnMut(u64, &mut [u8]) -> Result<()>,
) -> Result<RecordIndexEntry> {
    if mark_prefers_header_only(snapshot_len, records) {
        fault_record_entry_with::<{ crate::width::small(RECORD_HEADER_LEN) }>(
            snapshot_len,
            spec,
            record_offset,
            false,
            read,
        )
    } else {
        fault_record_entry_with::<512>(snapshot_len, spec, record_offset, false, read)
    }
}

fn fault_record_entry_with<const PREFIX: usize>(
    snapshot_len: u64,
    spec: FormatSpec,
    record_offset: u64,
    committed: bool,
    mut read: impl FnMut(u64, &mut [u8]) -> Result<()>,
) -> Result<RecordIndexEntry> {
    count_record_entry_fault();
    let mut header_bytes = [0; PREFIX];
    let prefix_len = if PREFIX == crate::width::small(RECORD_HEADER_LEN) {
        crate::width::small(RECORD_HEADER_LEN)
    } else {
        crate::width::index(
            snapshot_len
                .saturating_sub(record_offset)
                .min(PREFIX as u64)
                .max(RECORD_HEADER_LEN),
        )?
    };
    read(record_offset, &mut header_bytes[..prefix_len])?;
    let decoded = read_native_record_header(
        &mut &header_bytes[..crate::width::small(RECORD_HEADER_LEN)],
        record_offset,
    )?;
    debug_assert_eq!(decoded.lead_in_len, RECORD_HEADER_LEN);
    let header = decoded.fields;
    let Some(payload_offset) = record_offset.checked_add(RECORD_HEADER_LEN) else {
        return Err(Error::ResourceArithmeticOverflow {
            resource: "record payload offset",
        });
    };
    let mut entry = RecordIndexEntry {
        block_id: header.block_id,
        block_version: header.block_version,
        flags: header.flags,
        sequence: header.sequence,
        record_offset,
        payload_offset,
        payload_len: header.payload_len,
        checksum: header.checksum,
        uncompressed_len_hint: header.uncompressed_len_hint,
        footer_offset: None,
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        mutable_flags: 0,
        committed,
    };
    if spec.spec_needs_record_footer() {
        let Some(footer_offset) = payload_offset.checked_add(entry.payload_len) else {
            return Err(Error::ResourceArithmeticOverflow {
                resource: "record footer offset",
            });
        };
        let mut footer_bytes = [0; crate::width::small(RECORD_FOOTER_LEN)];
        let within_prefix = if PREFIX > crate::width::small(RECORD_HEADER_LEN) {
            usize::try_from(footer_offset - record_offset)
                .ok()
                .and_then(|start| {
                    start
                        .checked_add(crate::width::small(RECORD_FOOTER_LEN))
                        .filter(|&end| end <= prefix_len)
                        .map(|end| (start, end))
                })
        } else {
            None
        };
        let bytes = match within_prefix {
            Some((start, end)) => &header_bytes[start..end],
            None => {
                read(footer_offset, &mut footer_bytes)?;
                &footer_bytes[..]
            }
        };
        let footer = decode_record_footer(spec, bytes, footer_offset, record_offset)?;
        entry.footer_offset = Some(footer_offset);
        entry.prev_same_block_offset = footer.prev_same_block_offset;
        entry.prev_same_key_offset = footer.prev_same_key_offset;
        entry.mutable_flags = footer.mutable_flags;
    }
    Ok(entry)
}

fn read_record_footer_bytes(
    file: &mut (impl Read + Seek),
    offset: u64,
) -> Result<[u8; crate::width::small(RECORD_FOOTER_LEN)]> {
    file.seek(SeekFrom::Start(offset))?;
    let mut footer = [0; crate::width::small(RECORD_FOOTER_LEN)];
    file.read_exact(&mut footer)?;
    Ok(footer)
}

/// Decodes a footer and holds its trailing word to the spec's policy.
///
/// The native decoder hands the word back rather than judging it, because
/// whether a non-zero value is legitimate is a property of the *spec* and not
/// of the bytes. Under [`LivenessPolicy::None`] it is the reserved field it has
/// always been and a non-zero value is a corrupt footer — refusing it here is
/// what keeps a format that did not opt in exactly as strict as it was before
/// the policy existed, which matters because these four bytes are the ones the
/// checksum does not cover under the other policy.
fn decode_record_footer(
    spec: FormatSpec,
    bytes: &[u8],
    footer_offset: u64,
    record_offset: u64,
) -> Result<RecordFooterFields> {
    let footer = decode_native_record_footer(bytes, footer_offset, record_offset)?;
    if spec.liveness_policy == LivenessPolicy::None && footer.mutable_flags != 0 {
        return Err(Error::InvalidRecordFooter {
            offset: footer_offset,
        });
    }
    Ok(footer)
}

fn checksum_record_fields(
    spec: FormatSpec,
    record_offset: u64,
    mut header: RecordHeaderFields,
    payload: &[u8],
    footer: &[u8],
) -> Result<u32> {
    header.checksum = 0;
    let payload = if header.flags & RECORD_FLAG_IN_PLACE != 0 {
        &[][..]
    } else {
        payload
    };
    let header_bytes = if spec.integrity_policy == IntegrityPolicy::Crc32WithHeader {
        Some(encode_native_record_header(
            header,
            record_offset,
            record_footer_len(spec),
        )?)
    } else {
        None
    };
    checksum_record_bytes(
        spec,
        header_bytes.as_ref().map(|bytes| &bytes[..]).unwrap_or(&[]),
        payload,
        footer,
    )
}

fn try_alloc_bytes(len: u64, resource: &'static str) -> Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested: u64::try_from(len).unwrap_or(u64::MAX),
        })?;
    bytes.resize(len, 0);
    Ok(bytes)
}
