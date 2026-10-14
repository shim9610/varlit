//! header codec implementation, kept within the original parent privacy boundary.
use super::*;

pub(super) fn encode_index_checkpoint_payload(
    spec: FormatSpec,
    entries: &[RecordIndexEntry],
    covered_offset: u64,
) -> Result<Vec<u8>> {
    const PREFIX_LEN: u64 = 4 + 2 + 8 + 8;
    const ENTRY_LEN: u64 = 4 + 2 + 2 + 8 + 8 + 8 + 8 + 4 + 4 + 8 + 8 + 8 + 1;
    let count = u64::try_from(entries.len()).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "checkpoint entry count",
    })?;
    let payload_len = count
        .checked_mul(ENTRY_LEN)
        .and_then(|bytes| bytes.checked_add(PREFIX_LEN))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "checkpoint payload length",
        })?;
    spec.read_limits.check(ReadLimitKey::Records, count)?;
    spec.read_limits.check(
        ReadLimitKey::IndexBytes,
        index_bytes_for_count(entries.len())?,
    )?;
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, payload_len)?;
    let capacity =
        usize::try_from(payload_len).map_err(|_| Error::LengthOverflow { value: payload_len })?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource: "checkpoint payload",
            requested: payload_len,
        })?;
    payload.extend_from_slice(INDEX_CHECKPOINT_MAGIC);
    payload.extend_from_slice(&INDEX_CHECKPOINT_VERSION.to_le_bytes());
    payload.extend_from_slice(&covered_offset.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    for entry in entries {
        push_index_entry_bytes(&mut payload, entry, entry.committed);
    }
    Ok(payload)
}

/// Serializes one index entry in the layout [`read_index_entry_payload`]
/// decodes at checkpoint version 3.
///
/// `committed` is a parameter rather than a field read because the two writers
/// answer it differently: a checkpoint records the writer's live view, while a
/// segment is only ever written once its whole coverage is committed, so it
/// records what a scan of the same bytes would produce.
pub(super) fn push_index_entry_bytes(
    payload: &mut Vec<u8>,
    entry: &RecordIndexEntry,
    committed: bool,
) {
    payload.extend_from_slice(&entry.block_id.to_le_bytes());
    payload.extend_from_slice(&entry.block_version.to_le_bytes());
    payload.extend_from_slice(&entry.flags.to_le_bytes());
    payload.extend_from_slice(&entry.sequence.to_le_bytes());
    payload.extend_from_slice(&entry.record_offset.to_le_bytes());
    payload.extend_from_slice(&entry.payload_offset.to_le_bytes());
    payload.extend_from_slice(&entry.payload_len.to_le_bytes());
    payload.extend_from_slice(&entry.checksum.to_le_bytes());
    payload.extend_from_slice(&entry.uncompressed_len_hint.to_le_bytes());
    payload.extend_from_slice(&entry.footer_offset.unwrap_or(0).to_le_bytes());
    payload.extend_from_slice(&entry.prev_same_block_offset.unwrap_or(0).to_le_bytes());
    payload.extend_from_slice(&entry.prev_same_key_offset.unwrap_or(0).to_le_bytes());
    payload.push(u8::from(committed));
}

/// The payload length a segment record covering `count` entries occupies.
pub(super) fn segment_payload_len(count: u64) -> Result<u64> {
    count
        .checked_mul(SEGMENT_ENTRY_LEN)
        .and_then(|bytes| bytes.checked_add(SEGMENT_PREFIX_LEN))
        .and_then(|bytes| bytes.checked_add(SEGMENT_TRAILER_LEN))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "segment payload length",
        })
}

/// Serializes one segment: the records a single commit point added, and the
/// segment record's own start offset as the closing trailer.
/// Encodes a segment's entry array from a stream.
///
/// `ExactSizeIterator` because the payload length is charged and reserved
/// before the first entry is written — the count has to be known, and a
/// `skip(start)` over the index knows it without the index being a slice.
pub(super) fn encode_segment_payload(
    spec: FormatSpec,
    entries: impl ExactSizeIterator<Item = Result<RecordIndexEntry>>,
    covered_start: u64,
    preceding_records: u64,
    record_offset: u64,
) -> Result<Vec<u8>> {
    let count = u64::try_from(entries.len()).map_err(|_| Error::ResourceArithmeticOverflow {
        resource: "segment entry count",
    })?;
    let payload_len = segment_payload_len(count)?;
    spec.read_limits.check(ReadLimitKey::Records, count)?;
    spec.read_limits
        .check(ReadLimitKey::RecordPayloadLen, payload_len)?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, payload_len)?;
    let capacity =
        usize::try_from(payload_len).map_err(|_| Error::LengthOverflow { value: payload_len })?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource: "segment payload",
            requested: payload_len,
        })?;
    payload.extend_from_slice(SEGMENT_MAGIC);
    payload.extend_from_slice(&SEGMENT_VERSION.to_le_bytes());
    payload.extend_from_slice(&0u16.to_le_bytes());
    payload.extend_from_slice(&covered_start.to_le_bytes());
    payload.extend_from_slice(&count.to_le_bytes());
    payload.extend_from_slice(&preceding_records.to_le_bytes());
    for entry in entries {
        // A segment is written only after every record it covers is committed,
        // so this is not the writer's live `committed` bit - which stays false
        // for a data record under a marker policy - but what a scan of these
        // same bytes reports. The two open paths must agree entry for entry.
        push_index_entry_bytes(&mut payload, &entry?, true);
    }
    payload.extend_from_slice(&record_offset.to_le_bytes());
    Ok(payload)
}

/// One segment, as read back from its record.
#[derive(Debug)]
pub(super) struct DecodedSegment {
    /// File offset of the first record this segment covers.
    pub(super) covered_start: u64,
    /// Records in the file before `covered_start`.
    pub(super) preceding_records: u64,
    pub(super) entries: Vec<RecordIndexEntry>,
}

/// What the chain walk's first pass reads out of a segment's fixed prefix.
///
/// Only `covered_start`: the pass exists to check that the links join up, and
/// `preceding_records` is checked in pass two against the index actually built,
/// where it means something. Reading it here would be a number compared to
/// nothing.
#[derive(Clone, Copy, Debug)]
pub(super) struct SegmentPrefix {
    pub(super) covered_start: u64,
}

/// Decodes only the fixed prefix, which is everything the chain walk's first
/// pass needs to check that the links join up.
pub(super) fn decode_segment_prefix(prefix: &[u8]) -> Result<SegmentPrefix> {
    let prefix_len = SEGMENT_PREFIX_LEN as usize;
    if prefix.len() < prefix_len || &prefix[..4] != SEGMENT_MAGIC {
        return Err(Error::InvalidIndexSegment);
    }
    let mut u16_buf = [0; 2];
    u16_buf.copy_from_slice(&prefix[4..6]);
    if u16::from_le_bytes(u16_buf) != SEGMENT_VERSION {
        return Err(Error::InvalidIndexSegment);
    }
    u16_buf.copy_from_slice(&prefix[6..8]);
    if u16::from_le_bytes(u16_buf) != 0 {
        return Err(Error::InvalidIndexSegment);
    }
    let mut u64_buf = [0; 8];
    u64_buf.copy_from_slice(&prefix[8..16]);
    Ok(SegmentPrefix {
        covered_start: u64::from_le_bytes(u64_buf),
    })
}

pub(super) fn decode_segment_payload(
    spec: FormatSpec,
    payload: &[u8],
    record_offset: u64,
    file_len: u64,
) -> Result<DecodedSegment> {
    let prefix_len = SEGMENT_PREFIX_LEN as usize;
    let trailer_len = SEGMENT_TRAILER_LEN as usize;
    let entry_len = SEGMENT_ENTRY_LEN as usize;
    if payload.len() < prefix_len + trailer_len || &payload[..4] != SEGMENT_MAGIC {
        return Err(Error::InvalidIndexSegment);
    }
    let mut u16_buf = [0; 2];
    u16_buf.copy_from_slice(&payload[4..6]);
    if u16::from_le_bytes(u16_buf) != SEGMENT_VERSION {
        return Err(Error::InvalidIndexSegment);
    }
    u16_buf.copy_from_slice(&payload[6..8]);
    if u16::from_le_bytes(u16_buf) != 0 {
        return Err(Error::InvalidIndexSegment);
    }
    let mut u64_buf = [0; 8];
    u64_buf.copy_from_slice(&payload[8..16]);
    let covered_start = u64::from_le_bytes(u64_buf);
    u64_buf.copy_from_slice(&payload[16..24]);
    let count = u64::from_le_bytes(u64_buf);
    u64_buf.copy_from_slice(&payload[24..32]);
    let preceding_records = u64::from_le_bytes(u64_buf);
    // The trailer is what let this record be found from EOF at all; if it does
    // not name this record, the bytes at EOF were not this record's.
    u64_buf.copy_from_slice(&payload[payload.len() - trailer_len..]);
    if u64::from_le_bytes(u64_buf) != record_offset {
        return Err(Error::InvalidIndexSegment);
    }

    spec.read_limits.check(ReadLimitKey::Records, count)?;
    let count_usize = usize::try_from(count).map_err(|_| Error::LengthOverflow { value: count })?;
    let resident_bytes = index_bytes_for_count(count_usize)?;
    spec.read_limits
        .check(ReadLimitKey::IndexBytes, resident_bytes)?;
    let expected_len = usize::try_from(segment_payload_len(count)?)
        .map_err(|_| Error::LengthOverflow { value: count })?;
    if payload.len() != expected_len {
        return Err(Error::InvalidIndexSegment);
    }

    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count_usize)
        .map_err(|_| Error::AllocationFailed {
            resource: "segment index",
            requested: resident_bytes,
        })?;
    let mut position = prefix_len;
    for _ in 0..count_usize {
        let entry = read_index_entry_payload(
            &payload[position..position + entry_len],
            INDEX_CHECKPOINT_VERSION,
        );
        if entry.checked_physical_end()? > file_len {
            return Err(Error::InvalidIndexSegment);
        }
        // A segment never covers another segment: they are written last, so
        // the record after one is the first record of the next segment. A
        // chain that claimed otherwise could hide records between two links.
        if entry.block_id == SEGMENT_BLOCK_ID {
            return Err(Error::InvalidIndexSegment);
        }
        entries.push(entry);
        position += entry_len;
    }
    Ok(DecodedSegment {
        covered_start,
        preceding_records,
        entries,
    })
}

pub(super) fn encode_schema_manifest(spec: FormatSpec) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&MANIFEST_PAYLOAD_VERSION.to_le_bytes());
    payload.extend_from_slice(&spec.version.to_le_bytes());
    payload.push(spec.endian.to_byte());
    payload.extend_from_slice(&spec.schema_hash.to_le_bytes());
    payload.push(index_policy_byte(spec.index_policy));
    payload.push(integrity_policy_byte(spec.integrity_policy));
    payload.push(recovery_policy_byte(spec.recovery_policy));
    payload.push(manifest_policy_byte(spec.manifest_policy));
    payload.push(commit_policy_byte(spec.commit_policy));
    match spec.extension {
        Some(extension) => {
            payload.push(1);
            let extension = extension.as_bytes();
            let extension_len =
                u16::try_from(extension.len()).map_err(|_| Error::InvalidSchemaManifest)?;
            payload.extend_from_slice(&extension_len.to_le_bytes());
            payload.extend_from_slice(extension);
        }
        None => payload.push(0),
    }
    encode_manifest_compression_policy(&mut payload, spec.compression_policy);
    payload.extend_from_slice(&(spec.blocks.len() as u32).to_le_bytes());
    for block in spec.blocks {
        payload.extend_from_slice(&block.id.to_le_bytes());
        payload.extend_from_slice(&block.version.to_le_bytes());
        payload.push(block_kind_byte(block.kind));
        let name = block.name.as_bytes();
        let name_len = u16::try_from(name.len()).map_err(|_| Error::InvalidSchemaManifest)?;
        payload.extend_from_slice(&name_len.to_le_bytes());
        payload.extend_from_slice(name);
        payload.extend_from_slice(&(block.fields.len() as u32).to_le_bytes());
        for field in block.fields {
            payload.extend_from_slice(&field.id.to_le_bytes());
            payload.push(wire_type_byte(field.wire_type));
            payload.push(field_presence_byte(field.presence));
            let name = field.name.as_bytes();
            let name_len = u16::try_from(name.len()).map_err(|_| Error::InvalidSchemaManifest)?;
            payload.extend_from_slice(&name_len.to_le_bytes());
            payload.extend_from_slice(name);
        }
    }
    Ok(payload)
}

pub(super) fn decode_schema_manifest(
    payload: &[u8],
    budget: &mut MaterializationBudget,
) -> Result<SchemaManifest> {
    let mut cursor = ManifestCursor { payload, offset: 0 };
    let payload_version = cursor.read_u16()?;
    if !(1..=MANIFEST_PAYLOAD_VERSION).contains(&payload_version) {
        return Err(Error::InvalidSchemaManifest);
    }
    let format_version = cursor.read_u16()?;
    let endian = Endian::from_byte(cursor.read_u8()?).ok_or(Error::InvalidSchemaManifest)?;
    let schema_hash = cursor.read_u64()?;
    let index_policy = index_policy_from_byte(cursor.read_u8()?)?;
    let integrity_policy = integrity_policy_from_byte(cursor.read_u8()?)?;
    let recovery_policy = recovery_policy_from_byte(cursor.read_u8()?)?;
    let manifest_policy = manifest_policy_from_byte(cursor.read_u8()?)?;
    let commit_policy = if payload_version >= 4 {
        commit_policy_from_byte(cursor.read_u8()?)?
    } else {
        CommitPolicy::None
    };
    let (extension, compression_policy) = if payload_version >= 3 {
        let extension = match cursor.read_u8()? {
            0 => None,
            1 => {
                let len = cursor.read_u16()? as usize;
                Some(cursor.read_string(len, budget)?)
            }
            _ => return Err(Error::InvalidSchemaManifest),
        };
        let compression_policy = decode_manifest_compression_policy(&mut cursor)?;
        (extension, compression_policy)
    } else {
        (None, CompressionPolicy::None)
    };
    let block_count = cursor.read_u32()? as usize;
    let minimum_block_len = if payload_version >= 2 { 13 } else { 9 };
    preflight_manifest_count(block_count, minimum_block_len, cursor.remaining())?;
    consume_manifest_items::<SchemaBlockDescriptor>(budget, block_count)?;
    let mut blocks = Vec::new();
    blocks
        .try_reserve_exact(block_count)
        .map_err(|_| manifest_allocation_failed::<SchemaBlockDescriptor>(block_count))?;
    for _ in 0..block_count {
        let id = cursor.read_u32()?;
        let version = cursor.read_u16()?;
        let kind = block_kind_from_byte(cursor.read_u8()?)?;
        let name_len = cursor.read_u16()? as usize;
        let name = cursor.read_string(name_len, budget)?;
        let fields = if payload_version >= 2 {
            let field_count = cursor.read_u32()? as usize;
            preflight_manifest_count(field_count, 8, cursor.remaining())?;
            consume_manifest_items::<SchemaFieldDescriptor>(budget, field_count)?;
            let mut fields = Vec::new();
            fields
                .try_reserve_exact(field_count)
                .map_err(|_| manifest_allocation_failed::<SchemaFieldDescriptor>(field_count))?;
            for _ in 0..field_count {
                let id = cursor.read_u32()?;
                let wire_type = wire_type_from_byte(cursor.read_u8()?)?;
                let presence = field_presence_from_byte(cursor.read_u8()?)?;
                let name_len = cursor.read_u16()? as usize;
                let name = cursor.read_string(name_len, budget)?;
                fields.push(SchemaFieldDescriptor {
                    id,
                    name,
                    wire_type,
                    presence,
                });
            }
            fields
        } else {
            Vec::new()
        };
        blocks.push(SchemaBlockDescriptor {
            id,
            name,
            version,
            kind,
            fields,
        });
    }
    if cursor.remaining() != 0 {
        return Err(Error::InvalidSchemaManifest);
    }
    Ok(SchemaManifest {
        payload_version,
        format_version,
        endian,
        schema_hash,
        extension,
        index_policy,
        commit_policy,
        integrity_policy,
        recovery_policy,
        manifest_policy,
        compression_policy,
        blocks,
    })
}

pub(super) struct ManifestCursor<'a> {
    pub(super) payload: &'a [u8],
    pub(super) offset: usize,
}

impl ManifestCursor<'_> {
    pub(super) fn remaining(&self) -> usize {
        self.payload.len().saturating_sub(self.offset)
    }

    pub(super) fn read_exact(&mut self, len: usize) -> Result<&[u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(Error::InvalidSchemaManifest)?;
        if end > self.payload.len() {
            return Err(Error::InvalidSchemaManifest);
        }
        let value = &self.payload[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    pub(super) fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    pub(super) fn read_u16(&mut self) -> Result<u16> {
        let mut bytes = [0; 2];
        bytes.copy_from_slice(self.read_exact(2)?);
        Ok(u16::from_le_bytes(bytes))
    }

    pub(super) fn read_u32(&mut self) -> Result<u32> {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(self.read_exact(4)?);
        Ok(u32::from_le_bytes(bytes))
    }

    pub(super) fn read_u64(&mut self) -> Result<u64> {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(self.read_exact(8)?);
        Ok(u64::from_le_bytes(bytes))
    }

    pub(super) fn read_i32(&mut self) -> Result<i32> {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(self.read_exact(4)?);
        Ok(i32::from_le_bytes(bytes))
    }

    pub(super) fn read_string(
        &mut self,
        len: usize,
        budget: &mut MaterializationBudget,
    ) -> Result<String> {
        let bytes = self.read_exact(len)?;
        let value = std::str::from_utf8(bytes).map_err(|_| Error::InvalidSchemaManifest)?;
        let requested = u64::try_from(len).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "schema manifest string",
        })?;
        budget.consume(requested)?;
        let mut output = String::new();
        output
            .try_reserve_exact(len)
            .map_err(|_| Error::AllocationFailed {
                resource: "schema manifest string",
                requested,
            })?;
        output.push_str(value);
        Ok(output)
    }
}

pub(super) fn preflight_manifest_count(
    count: usize,
    minimum_item_len: usize,
    remaining: usize,
) -> Result<()> {
    let minimum_extent = count
        .checked_mul(minimum_item_len)
        .ok_or(Error::InvalidSchemaManifest)?;
    if minimum_extent > remaining {
        return Err(Error::InvalidSchemaManifest);
    }
    Ok(())
}

pub(super) fn consume_manifest_items<T>(
    budget: &mut MaterializationBudget,
    count: usize,
) -> Result<()> {
    let requested = count
        .checked_mul(size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "schema manifest output",
        })?;
    budget.consume(requested)
}

pub(super) fn manifest_allocation_failed<T>(count: usize) -> Error {
    let requested = count
        .checked_mul(size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .unwrap_or(u64::MAX);
    Error::AllocationFailed {
        resource: "schema manifest output",
        requested,
    }
}

pub(super) fn block_kind_byte(kind: BlockKind) -> u8 {
    match kind {
        BlockKind::Fixed => 1,
        BlockKind::Variable => 2,
        BlockKind::Matrix => 3,
        BlockKind::Internal => 4,
    }
}

pub(super) fn block_kind_from_byte(value: u8) -> Result<BlockKind> {
    match value {
        1 => Ok(BlockKind::Fixed),
        2 => Ok(BlockKind::Variable),
        3 => Ok(BlockKind::Matrix),
        4 => Ok(BlockKind::Internal),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

pub(super) fn wire_type_byte(wire_type: WireType) -> u8 {
    wire_type as u8
}

pub(super) fn wire_type_from_byte(value: u8) -> Result<WireType> {
    WireType::from_u16(u16::from(value)).ok_or(Error::InvalidSchemaManifest)
}

pub(super) fn field_presence_byte(presence: crate::FieldPresence) -> u8 {
    match presence {
        crate::FieldPresence::Required => 1,
        crate::FieldPresence::Defaulted => 2,
    }
}

pub(super) fn field_presence_from_byte(value: u8) -> Result<crate::FieldPresence> {
    match value {
        1 => Ok(crate::FieldPresence::Required),
        2 => Ok(crate::FieldPresence::Defaulted),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

pub(super) fn index_policy_byte(policy: IndexPolicy) -> u8 {
    (if policy.scan_on_open { 1 } else { 0 })
        | (if policy.checkpoint_on_flush {
            1 << 1
        } else {
            0
        })
        | (if policy.block_offset_chain { 1 << 2 } else { 0 })
        | (if policy.keyed_offset_chain { 1 << 3 } else { 0 })
        | (if policy.segment_on_flush { 1 << 4 } else { 0 })
        | (if policy.open_digest_on_flush {
            1 << 5
        } else {
            0
        })
        | (if policy.header_tails { 1 << 6 } else { 0 })
}

pub(super) fn index_policy_from_byte(value: u8) -> Result<IndexPolicy> {
    match value {
        1 => Ok(IndexPolicy::ScanOnOpen),
        2 => Ok(IndexPolicy::CheckpointOnFlush),
        3..=127 => Ok(IndexPolicy::new(
            value & 0x01 != 0,
            value & 0x02 != 0,
            value & 0x04 != 0,
            value & 0x08 != 0,
        )
        .with_segment_on_flush(value & 0x10 != 0)
        .with_open_digest_on_flush(value & 0x20 != 0)
        .with_header_tails(value & 0x40 != 0)),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

pub(super) fn commit_policy_byte(policy: CommitPolicy) -> u8 {
    match policy {
        CommitPolicy::None => 1,
        CommitPolicy::RecordFooter => 2,
        CommitPolicy::TransactionMarker(crate::TransactionMarkerMode::OnFlush) => 3,
        CommitPolicy::TransactionMarker(crate::TransactionMarkerMode::Explicit) => 4,
    }
}

pub(super) fn commit_policy_from_byte(value: u8) -> Result<CommitPolicy> {
    match value {
        1 => Ok(CommitPolicy::None),
        2 => Ok(CommitPolicy::RecordFooter),
        3 => Ok(CommitPolicy::TransactionMarker(
            crate::TransactionMarkerMode::OnFlush,
        )),
        4 => Ok(CommitPolicy::TransactionMarker(
            crate::TransactionMarkerMode::Explicit,
        )),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

pub(super) fn integrity_policy_byte(policy: IntegrityPolicy) -> u8 {
    match policy {
        IntegrityPolicy::None => 1,
        IntegrityPolicy::Crc32 => 2,
        IntegrityPolicy::Crc32WithHeader => 3,
    }
}

pub(super) fn integrity_policy_from_byte(value: u8) -> Result<IntegrityPolicy> {
    match value {
        1 => Ok(IntegrityPolicy::None),
        2 => Ok(IntegrityPolicy::Crc32),
        3 => Ok(IntegrityPolicy::Crc32WithHeader),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

pub(super) fn recovery_policy_byte(policy: RecoveryPolicy) -> u8 {
    match policy {
        RecoveryPolicy::Strict => 1,
        RecoveryPolicy::TruncateTail => 2,
        RecoveryPolicy::MarkTail => 3,
    }
}

pub(super) fn recovery_policy_from_byte(value: u8) -> Result<RecoveryPolicy> {
    match value {
        1 => Ok(RecoveryPolicy::Strict),
        2 => Ok(RecoveryPolicy::TruncateTail),
        3 => Ok(RecoveryPolicy::MarkTail),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

pub(super) fn manifest_policy_byte(policy: ManifestPolicy) -> u8 {
    match policy {
        ManifestPolicy::None => 1,
        ManifestPolicy::Embedded => 2,
    }
}

pub(super) fn manifest_policy_from_byte(value: u8) -> Result<ManifestPolicy> {
    match value {
        1 => Ok(ManifestPolicy::None),
        2 => Ok(ManifestPolicy::Embedded),
        _ => Err(Error::InvalidSchemaManifest),
    }
}

pub(super) fn encode_manifest_compression_policy(payload: &mut Vec<u8>, policy: CompressionPolicy) {
    match policy {
        CompressionPolicy::None => payload.push(0),
        CompressionPolicy::VariableBlocks(compression) => {
            payload.push(1);
            payload.push(compression_algorithm_byte(compression.algorithm));
            payload.push(compression_level_kind_byte(compression.level));
            payload.extend_from_slice(&compression_level_exact(compression.level).to_le_bytes());
            payload.push(compression_header_mode_byte(compression.header_mode));
            payload.extend_from_slice(&compression.min_uncompressed_len.to_le_bytes());
            payload.push(u8::from(compression.only_if_smaller));
            payload.extend_from_slice(&compression.max_uncompressed_len.to_le_bytes());
        }
    }
}

pub(super) fn decode_manifest_compression_policy(
    cursor: &mut ManifestCursor<'_>,
) -> Result<CompressionPolicy> {
    match cursor.read_u8()? {
        0 => Ok(CompressionPolicy::None),
        1 => {
            let algorithm = compression_algorithm_from_byte(cursor.read_u8()?)
                .map_err(|_| Error::InvalidSchemaManifest)?;
            let level_kind = cursor.read_u8()?;
            let level_exact = cursor.read_i32()?;
            let level = compression_level_from_parts(level_kind, level_exact)
                .map_err(|_| Error::InvalidSchemaManifest)?;
            let header_mode = compression_header_mode_from_byte(cursor.read_u8()?)
                .map_err(|_| Error::InvalidSchemaManifest)?;
            let min_uncompressed_len = cursor.read_u64()?;
            let only_if_smaller = match cursor.read_u8()? {
                0 => false,
                1 => true,
                _ => return Err(Error::InvalidSchemaManifest),
            };
            let max_uncompressed_len = cursor.read_u64()?;
            Ok(CompressionPolicy::VariableBlocks(VariableCompression {
                algorithm,
                level,
                header_mode,
                min_uncompressed_len,
                only_if_smaller,
                max_uncompressed_len,
            }))
        }
        _ => Err(Error::InvalidSchemaManifest),
    }
}

pub(super) fn prepare_user_record_payload<'a>(
    spec: FormatSpec,
    block_id: u32,
    kind: BlockKind,
    logical_payload: &'a [u8],
) -> Result<StoredPayload<'a>> {
    spec.read_limits.check(
        ReadLimitKey::LogicalPayloadLen,
        u64::try_from(logical_payload.len())
            .map_err(|_| Error::LengthOverflow { value: u64::MAX })?,
    )?;
    let Some(compression) = variable_compression_for_block(spec, block_id) else {
        return Ok(uncompressed_user_payload(logical_payload));
    };
    if kind != BlockKind::Variable {
        return Ok(uncompressed_user_payload(logical_payload));
    }

    let logical_len = logical_payload.len() as u64;
    if logical_len > compression.max_uncompressed_len {
        return Err(Error::DecompressedLengthLimitExceeded {
            actual: logical_len,
            limit: compression.max_uncompressed_len,
        });
    }
    if logical_len < compression.min_uncompressed_len {
        return Ok(uncompressed_user_payload(logical_payload));
    }

    let compressed =
        compress_with_algorithm(compression.algorithm, compression.level, logical_payload)?;
    let stored_bytes = match compression.header_mode {
        CompressionHeaderMode::RecordExplicit => {
            encode_compression_envelope(compression, logical_len, &compressed)?
        }
        CompressionHeaderMode::FileExplicit | CompressionHeaderMode::FormatContract => compressed,
    };

    if compression.only_if_smaller && stored_bytes.len() >= logical_payload.len() {
        return Ok(uncompressed_user_payload(logical_payload));
    }

    let uncompressed_len_hint = match u32::try_from(logical_len) {
        Ok(value) => value,
        Err(_) if compression.header_mode == CompressionHeaderMode::RecordExplicit => 0,
        Err(_) => return Err(Error::LengthOverflow { value: logical_len }),
    };
    Ok(StoredPayload {
        flags: RECORD_FLAG_COMPRESSED,
        uncompressed_len_hint,
        bytes: Cow::Owned(stored_bytes),
    })
}

/// The stored form of a payload that is stored exactly as it was handed in.
///
/// Borrows. The bytes are the caller's and are read, not kept: every caller
/// reborrows `&payload.bytes` into the record writer and drops the
/// `StoredPayload` on the same statement.
pub(super) fn uncompressed_user_payload(logical_payload: &[u8]) -> StoredPayload<'_> {
    StoredPayload {
        flags: 0,
        uncompressed_len_hint: 0,
        bytes: Cow::Borrowed(logical_payload),
    }
}

pub(super) fn record_logical_len_from_parts(
    spec: FormatSpec,
    block_id: u32,
    flags: u16,
    uncompressed_len_hint: u32,
    payload: &[u8],
) -> Result<u64> {
    if flags & RECORD_FLAG_COMPRESSED == 0 {
        return u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX });
    }
    let compression =
        variable_compression_for_block(spec, block_id).ok_or(Error::InvalidCompressionHeader)?;
    let logical_len = match compression.header_mode {
        CompressionHeaderMode::RecordExplicit => {
            let (_, logical_len, _) = decode_compression_envelope(payload)?;
            logical_len
        }
        CompressionHeaderMode::FileExplicit | CompressionHeaderMode::FormatContract => {
            if uncompressed_len_hint == 0 {
                return Err(Error::InvalidCompressionHeader);
            }
            u64::from(uncompressed_len_hint)
        }
    };
    if logical_len > compression.max_uncompressed_len {
        return Err(Error::DecompressedLengthLimitExceeded {
            actual: logical_len,
            limit: compression.max_uncompressed_len,
        });
    }
    Ok(logical_len)
}

pub(super) fn decode_record_payload(
    spec: FormatSpec,
    entry: &RecordIndexEntry,
    physical_payload: Vec<u8>,
) -> Result<Vec<u8>> {
    if !entry.is_compressed() {
        return Ok(physical_payload);
    }
    validate_record_entry(spec, entry)?;
    let compression = variable_compression_for_block(spec, entry.block_id)
        .ok_or(Error::InvalidCompressionHeader)?;
    let (algorithm, expected_len, compressed_payload) = match compression.header_mode {
        CompressionHeaderMode::RecordExplicit => decode_compression_envelope(&physical_payload)?,
        CompressionHeaderMode::FileExplicit | CompressionHeaderMode::FormatContract => {
            if entry.uncompressed_len_hint == 0 {
                return Err(Error::InvalidCompressionHeader);
            }
            (
                compression.algorithm,
                u64::from(entry.uncompressed_len_hint),
                physical_payload.as_slice(),
            )
        }
    };
    if expected_len > compression.max_uncompressed_len {
        return Err(Error::DecompressedLengthLimitExceeded {
            actual: expected_len,
            limit: compression.max_uncompressed_len,
        });
    }
    let decoded = decompress_with_algorithm(algorithm, compressed_payload, expected_len)?;
    let actual_len = decoded.len() as u64;
    if actual_len != expected_len {
        return Err(Error::DecompressedLengthMismatch {
            expected: expected_len,
            actual: actual_len,
        });
    }
    Ok(decoded)
}

pub(super) fn encode_compression_envelope(
    compression: VariableCompression,
    uncompressed_len: u64,
    compressed_payload: &[u8],
) -> Result<Vec<u8>> {
    let capacity =
        compressed_payload
            .len()
            .checked_add(20)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "compression envelope",
            })?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource: "compression envelope",
            requested: u64::try_from(capacity).unwrap_or(u64::MAX),
        })?;
    payload.extend_from_slice(COMPRESSION_ENVELOPE_MAGIC);
    payload.push(COMPRESSION_ENVELOPE_VERSION);
    payload.push(compression_algorithm_byte(compression.algorithm));
    payload.push(compression_level_kind_byte(compression.level));
    payload.push(0);
    payload.extend_from_slice(&compression_level_exact(compression.level).to_le_bytes());
    payload.extend_from_slice(&uncompressed_len.to_le_bytes());
    payload.extend_from_slice(compressed_payload);
    Ok(payload)
}

pub(super) fn decode_compression_envelope(
    payload: &[u8],
) -> Result<(CompressionAlgorithm, u64, &[u8])> {
    const ENVELOPE_PREFIX_LEN: usize = 4 + 1 + 1 + 1 + 1 + 4 + 8;
    if payload.len() < ENVELOPE_PREFIX_LEN || &payload[..4] != COMPRESSION_ENVELOPE_MAGIC {
        return Err(Error::InvalidCompressionHeader);
    }
    if payload[4] != COMPRESSION_ENVELOPE_VERSION || payload[7] != 0 {
        return Err(Error::InvalidCompressionHeader);
    }
    let algorithm = compression_algorithm_from_byte(payload[5])?;
    let level_kind = payload[6];
    let mut level_exact = [0; 4];
    level_exact.copy_from_slice(&payload[8..12]);
    let _level = compression_level_from_parts(level_kind, i32::from_le_bytes(level_exact))?;
    let mut len = [0; 8];
    len.copy_from_slice(&payload[12..20]);
    Ok((
        algorithm,
        u64::from_le_bytes(len),
        &payload[ENVELOPE_PREFIX_LEN..],
    ))
}

pub(super) fn variable_compression(spec: FormatSpec) -> Option<VariableCompression> {
    match spec.compression_policy {
        CompressionPolicy::None => None,
        CompressionPolicy::VariableBlocks(compression) => Some(compression),
    }
}

pub(super) fn variable_compression_for_block(
    spec: FormatSpec,
    block_id: u32,
) -> Option<VariableCompression> {
    spec.block_compression
        .iter()
        .find(|descriptor| descriptor.block_id == block_id)
        .map(|descriptor| descriptor.compression)
        .or_else(|| variable_compression(spec))
}

pub(super) fn uses_file_explicit_compression(spec: FormatSpec) -> bool {
    matches!(
        variable_compression(spec).map(|compression| compression.header_mode),
        Some(CompressionHeaderMode::FileExplicit)
    )
}

pub(super) fn uses_format_contract_compression(spec: FormatSpec) -> bool {
    matches!(
        variable_compression(spec).map(|compression| compression.header_mode),
        Some(CompressionHeaderMode::FormatContract)
    )
}

pub(super) fn validate_record_entry(spec: FormatSpec, entry: &RecordIndexEntry) -> Result<()> {
    let unknown_flags = entry.flags & !RECORD_KNOWN_FLAGS;
    if unknown_flags != 0 {
        return Err(Error::UnknownRecordFlags(unknown_flags));
    }
    if !entry.is_compressed() {
        return Ok(());
    }
    if entry.flags & RECORD_FLAG_INTERNAL != 0 || entry.block_id >= RESERVED_BLOCK_ID_START {
        return Err(Error::InvalidCompressionHeader);
    }
    let descriptor = spec
        .block(entry.block_id)
        .ok_or(Error::UnregisteredBlock(entry.block_id))?;
    if descriptor.kind != BlockKind::Variable {
        return Err(Error::InvalidCompressionHeader);
    }
    let compression = variable_compression_for_block(spec, entry.block_id)
        .ok_or(Error::InvalidCompressionHeader)?;
    ensure_compression_algorithm_available(compression.algorithm)
}

pub(crate) fn file_header_extensions(spec: FormatSpec) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    if let Some(compression) = variable_compression(spec)
        && compression.header_mode == CompressionHeaderMode::FileExplicit
    {
        payload.extend_from_slice(FILE_COMPRESSION_MAGIC);
        payload.push(FILE_COMPRESSION_VERSION);
        payload.push(compression_algorithm_byte(compression.algorithm));
        payload.push(compression_level_kind_byte(compression.level));
        payload.push(u8::from(compression.only_if_smaller));
        payload.extend_from_slice(&compression_level_exact(compression.level).to_le_bytes());
        payload.extend_from_slice(&compression.min_uncompressed_len.to_le_bytes());
        payload.extend_from_slice(&compression.max_uncompressed_len.to_le_bytes());
    }
    if spec.index_policy.header_tails {
        payload.extend_from_slice(&encode_cold_header_tails_region(spec)?);
    }
    if spec.liveness_policy == LivenessPolicy::FooterFlags {
        payload.extend_from_slice(&encode_liveness_block(0));
    }
    // Appended last, so every format that declares no region produces exactly
    // the bytes it produced before this existed.
    if spec.header_slots.is_declared() {
        payload.extend_from_slice(&encode_empty_header_slots_region(spec)?);
    }
    Ok(payload)
}

/// How many `(block_id, tail_offset)` entries one slot holds.
///
/// Fixed by the declaration, because the region's length is fixed at create and
/// the block set is fixed at compile time. This is the only reason the region
/// can be a constant-length in-place write at all.
pub(super) fn header_tails_capacity(spec: FormatSpec) -> usize {
    spec.blocks.len() + HEADER_TAILS_RESERVED_ENTRIES
}

pub(super) fn header_tails_slot_len(capacity: usize) -> Option<usize> {
    capacity
        .checked_mul(HEADER_TAILS_ENTRY_LEN)?
        .checked_add(HEADER_TAILS_SLOT_HEADER_LEN)?
        .checked_add(HEADER_TAILS_SLOT_TRAILER_LEN)
}

/// The framed length of the `VBTT` block a spec would write, or zero for a spec
/// that declares no region.
///
/// **One derivation, and the layout plan is the third consumer.** The encoder
/// below produces the region and the bound above refuses an over-large one;
/// `native_layout`'s published plan has to report the same number, because
/// `FormatSpec::effective_layout` is where a tool outside this crate learns
/// where the file header ends and the first record begins. Deriving it twice is
/// what let the plan describe a header short by the whole region.
///
/// Saturating rather than fallible, because the plan is infallible and a
/// saturated length is honest where a zero would read as "there is no region".
/// A spec that would saturate is refused at create by the encoder, which does
/// the same arithmetic checked.
pub(crate) fn header_tails_region_len(spec: FormatSpec) -> u64 {
    if !spec.index_policy.header_tails {
        return 0;
    }
    let capacity = header_tails_capacity(spec) as u64;
    let slot = capacity
        .saturating_mul(HEADER_TAILS_ENTRY_LEN as u64)
        .saturating_add(HEADER_TAILS_SLOT_HEADER_LEN as u64)
        .saturating_add(HEADER_TAILS_SLOT_TRAILER_LEN as u64);
    slot.saturating_mul(HEADER_TAILS_SLOTS as u64)
        .saturating_add(HEADER_TAILS_BLOCK_FRAMING_LEN as u64)
}

/// The whole `VBTT` block a spec would write, framing included, with both slots
/// cold.
///
/// A cold slot is framed and checksummed but claims nothing: `count` is zero, so
/// a reader that adopts it learns no tail and falls back exactly as it would for
/// a file with no region at all. That is what create writes, and what a
/// `replace_*` writes into the new file, because in both cases every recorded
/// offset would be a lie.
pub(super) fn encode_cold_header_tails_region(spec: FormatSpec) -> Result<Vec<u8>> {
    let capacity = header_tails_capacity(spec);
    let framed_len = usize::try_from(header_tails_region_len(spec))
        .map_err(|_| Error::InvalidFormatSpec("header_tails region length overflows"))?;
    // The region shares the extension budget with every other block, so the
    // check is against the whole region's framed length, not the payload. The
    // budget is 64 KiB unless the format declared `limits { header_extension }`.
    if header_tails_region_len(spec) > spec.read_limits.effective_max_file_header_extension_len() {
        return Err(Error::InvalidFormatSpec(
            "header_tails region exceeds the file-header extension limit; the format declares too many blocks",
        ));
    }
    let slot_len = header_tails_slot_len(capacity).ok_or(Error::InvalidFormatSpec(
        "header_tails region length overflows",
    ))?;
    let payload_len = framed_len - HEADER_TAILS_BLOCK_FRAMING_LEN;
    debug_assert_eq!(payload_len, slot_len * HEADER_TAILS_SLOTS);
    let payload_len_u32 = u32::try_from(payload_len)
        .map_err(|_| Error::InvalidFormatSpec("header_tails region length overflows"))?;

    let mut region = Vec::new();
    region
        .try_reserve_exact(framed_len)
        .map_err(|_| Error::AllocationFailed {
            resource: "header tail region",
            requested: framed_len as u64,
        })?;
    region.extend_from_slice(HEADER_TAILS_MAGIC);
    region.extend_from_slice(&payload_len_u32.to_le_bytes());
    for _ in 0..HEADER_TAILS_SLOTS {
        region.extend_from_slice(&encode_header_tails_slot(capacity, 0, 0, &[])?);
    }
    debug_assert_eq!(region.len(), framed_len);
    Ok(region)
}

/// One slot's bytes, exactly `header_tails_slot_len(capacity)` long.
///
/// `count = 0` is what a slot claims when it has nothing to say, and it covers
/// two cases that must not be told apart by a reader: the cold slot create
/// writes, and a table that does not fit the capacity fixed at create. The
/// second sets [`HEADER_TAILS_FLAG_OVERFLOW`] so the file records *why*, but it
/// still claims nothing — a partial table is one a reader would believe.
pub(super) fn encode_header_tails_slot(
    capacity: usize,
    generation: u64,
    commit_offset: u64,
    tails: &[(u32, u64)],
) -> Result<Vec<u8>> {
    let slot_len = header_tails_slot_len(capacity).ok_or(Error::InvalidFormatSpec(
        "header_tails region length overflows",
    ))?;
    let capacity_u32 = u32::try_from(capacity)
        .map_err(|_| Error::InvalidFormatSpec("header_tails region length overflows"))?;
    let overflow = tails.len() > capacity;
    let (flags, count) = if overflow {
        (HEADER_TAILS_FLAG_OVERFLOW, 0u32)
    } else {
        (0u16, tails.len() as u32)
    };
    let mut slot = Vec::new();
    slot.try_reserve_exact(slot_len)
        .map_err(|_| Error::AllocationFailed {
            resource: "header tail slot",
            requested: slot_len as u64,
        })?;
    slot.extend_from_slice(&HEADER_TAILS_VERSION.to_le_bytes());
    slot.extend_from_slice(&flags.to_le_bytes());
    slot.extend_from_slice(&capacity_u32.to_le_bytes());
    slot.extend_from_slice(&count.to_le_bytes());
    slot.extend_from_slice(&generation.to_le_bytes());
    slot.extend_from_slice(&commit_offset.to_le_bytes());
    if !overflow {
        for (block_id, offset) in tails {
            slot.extend_from_slice(&block_id.to_le_bytes());
            slot.extend_from_slice(&offset.to_le_bytes());
        }
    }
    // The unused entries are zeroed rather than left over from the slot this
    // one replaces: the region is overwritten in place, so anything not written
    // here is the *previous* generation's bytes, and a reader that mis-read
    // `count` would find a plausible table behind it.
    slot.resize(slot_len - HEADER_TAILS_SLOT_TRAILER_LEN, 0);
    let checksum = crc32_bytes(&slot)?;
    slot.extend_from_slice(&checksum.to_le_bytes());
    debug_assert_eq!(slot.len(), slot_len);
    Ok(slot)
}

/// One block of the file-header extension region.
pub(super) struct HeaderExtensionBlock<'a> {
    pub(super) magic: [u8; 4],
    /// Where the block starts within the region, so a caller that has to
    /// *replace* one can address it without re-deriving the framing.
    pub(super) offset: usize,
    /// The block's bytes *including* its magic and any length prefix, so a
    /// known block is compared against what this spec would write without
    /// re-encoding the framing.
    pub(super) encoded: &'a [u8],
}

/// Whether this build decodes a block, as opposed to stepping over it.
///
/// The whole point of the walk: a magic that is not on this list is skipped,
/// so a file carrying a block from a later release still opens here.
pub(super) fn is_known_header_extension_magic(magic: &[u8; 4]) -> bool {
    magic == FILE_COMPRESSION_MAGIC
        || magic == HEADER_TAILS_MAGIC
        || magic == LIVENESS_MAGIC
        || magic == HEADER_SLOTS_MAGIC
}

/// Whether the file is allowed to disagree with this spec about the block's
/// *contents*.
///
/// Every other known block is compared byte for byte, which is what makes a
/// header a fixed description of the file. `VBTT` cannot be: its whole purpose
/// is to be rewritten as the file grows, so a file open five commits later
/// legitimately carries bytes no spec would produce. Its framing — magic,
/// declared length, position — is still compared, so a region that changed size
/// or vanished is still refused.
///
/// The cost is real and worth writing down: these files never take the
/// `expected == extensions` fast path in [`validate_file_header_extensions`],
/// so every open walks the block list. It is a walk over a handful of blocks in
/// a region under 64 KiB, once per open.
/// The liveness block, framed, with its checksum.
///
/// Self-checking for the same reason the header-tail slot is: this is written
/// in place while unlocked readers may be reading it, Windows offers no atomic
/// regular-file write, and a torn one must read as "I cannot tell" rather than
/// as a value. A torn block is treated as dirty — the conservative direction,
/// because it makes recovery run when it might not have been needed rather than
/// skip when it was.
pub(super) fn encode_liveness_block(flags: u16) -> [u8; 8 + LIVENESS_PAYLOAD_LEN] {
    let mut block = [0u8; 8 + LIVENESS_PAYLOAD_LEN];
    block[..4].copy_from_slice(LIVENESS_MAGIC);
    block[4..8].copy_from_slice(&(LIVENESS_PAYLOAD_LEN as u32).to_le_bytes());
    block[8..10].copy_from_slice(&LIVENESS_BLOCK_VERSION.to_le_bytes());
    block[10..12].copy_from_slice(&flags.to_le_bytes());
    let checksum = liveness_checksum(&block[8..12]);
    block[12..16].copy_from_slice(&checksum.to_le_bytes());
    block
}

/// Not gated on the `integrity` feature, unlike the record checksums.
///
/// Four bytes of header, written once per writer open and once per release, is
/// not a place where a checksum is a cost worth declaring away — and the thing
/// it protects is a *recovery decision*, which a format that declared no
/// integrity policy needs just as much as one that did.
pub(super) fn liveness_checksum(bytes: &[u8]) -> u32 {
    let mut hash = 2_166_136_261u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

/// The liveness flags a file carries, or `None` if it carries no such block.
///
/// `Some(DIRTY)` for a torn or otherwise unreadable payload, deliberately: an
/// unreadable mark is not evidence of a clean release.
pub(super) fn decode_liveness_flags(header_len: u64, extensions: &[u8]) -> Result<Option<u16>> {
    let Some(extent) = mutable_header_block_extent(header_len, extensions, LIVENESS_MAGIC)? else {
        return Ok(None);
    };
    let start = extent.payload_in_extensions;
    let Some(payload) = extensions.get(start..start + LIVENESS_PAYLOAD_LEN) else {
        return Ok(Some(LIVENESS_FLAG_DIRTY));
    };
    let version = u16::from_le_bytes([payload[0], payload[1]]);
    let flags = u16::from_le_bytes([payload[2], payload[3]]);
    let stored = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    if version != LIVENESS_BLOCK_VERSION || stored != liveness_checksum(&payload[..4]) {
        return Ok(Some(LIVENESS_FLAG_DIRTY));
    }
    Ok(Some(flags))
}

/// The editable header region: magic, framing, and the entry directory.
///
/// `magic "VHSL" | len u32` then
/// `version u16 | flags u16 | capacity u32 | used u32 | checksum u32`
/// followed by `capacity` bytes of entries and zero padding. `used` says how
/// many of those bytes the entries occupy; everything past it is zero.
///
/// One entry is `block_id u32 | block_version u16 | reserved u16 | len u32`
/// then `len` payload bytes. Ids appear at most once — a second write of the
/// same block replaces the first rather than appending, which is what keeps a
/// fixed region from filling up under repeated edits.
pub(super) const HEADER_SLOTS_MAGIC: &[u8; 4] = b"VHSL";
pub(super) const HEADER_SLOTS_VERSION: u16 = 1;
/// The region is sealed: its checksum is written and it accepts no more writes.
pub(super) const HEADER_SLOTS_FLAG_SEALED: u16 = 0x0001;
/// Every flag this version defines. A region setting one outside this set was
/// written by something that means more by it than this build knows, and is
/// refused rather than half-understood — the same posture the digest takes.
pub(super) const HEADER_SLOTS_FLAGS_KNOWN: u16 = HEADER_SLOTS_FLAG_SEALED;
pub(super) const HEADER_SLOT_ENTRY_HEADER_LEN: usize = 4 + 2 + 2 + 4;

/// The region's checksum, and why it is not a crc32.
///
/// `crc32fast` is behind the `integrity` feature, and putting the region's
/// checksum behind it too would make [`HeaderSlotIntegrity::Rolling`] and
/// [`HeaderSlotIntegrity::Sealed`] silently do nothing in a default build —
/// a declared policy that is inert is exactly what "capabilities are options"
/// forbids. So this is an FNV-1a 32 written by hand, for the same reason and
/// by the same precedent as the liveness block's.
pub(super) fn header_slots_checksum(bytes: &[u8]) -> u32 {
    let mut hash = 2_166_136_261u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

/// A decoded view of the region's payload, borrowed from the header bytes.
pub(super) struct HeaderSlotsView<'a> {
    pub(super) flags: u16,
    pub(super) used: usize,
    pub(super) checksum: u32,
    /// The `capacity` bytes that hold the entries, padding included.
    pub(super) entries: &'a [u8],
}

impl HeaderSlotsView<'_> {
    pub(super) fn is_sealed(&self) -> bool {
        self.flags & HEADER_SLOTS_FLAG_SEALED != 0
    }

    /// What the checksum should be: everything in the payload except the four
    /// bytes holding the checksum itself.
    pub(super) fn expected_checksum(&self, capacity: usize) -> u32 {
        let mut covered = Vec::with_capacity(12 + self.entries.len());
        covered.extend_from_slice(&HEADER_SLOTS_VERSION.to_le_bytes());
        covered.extend_from_slice(&self.flags.to_le_bytes());
        covered.extend_from_slice(&(capacity as u32).to_le_bytes());
        covered.extend_from_slice(&(self.used as u32).to_le_bytes());
        covered.extend_from_slice(self.entries);
        header_slots_checksum(&covered)
    }
}

/// Frames the region's payload, or says why these bytes are not one.
///
/// Every field is checked against the declaration rather than trusted: a
/// capacity that disagrees with the spec means the file was written by a
/// different declaration, and a `used` past the capacity is a corrupt region
/// rather than a large one.
pub(super) fn decode_header_slots(spec: FormatSpec, payload: &[u8]) -> Result<HeaderSlotsView<'_>> {
    let capacity = spec.header_slots.capacity as usize;
    let prefix = crate::format::HEADER_SLOTS_PREFIX_LEN as usize;
    if payload.len() != prefix + capacity {
        return Err(Error::InvalidCompressionHeader);
    }
    let version = u16::from_le_bytes([payload[0], payload[1]]);
    if version != HEADER_SLOTS_VERSION {
        return Err(Error::InvalidCompressionHeader);
    }
    let flags = u16::from_le_bytes([payload[2], payload[3]]);
    if flags & !HEADER_SLOTS_FLAGS_KNOWN != 0 {
        return Err(Error::InvalidCompressionHeader);
    }
    let stored_capacity = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    if stored_capacity as usize != capacity {
        return Err(Error::InvalidCompressionHeader);
    }
    let used = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]) as usize;
    if used > capacity {
        return Err(Error::InvalidCompressionHeader);
    }
    let checksum = u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
    Ok(HeaderSlotsView {
        flags,
        used,
        checksum,
        entries: &payload[prefix..prefix + capacity],
    })
}

/// Walks the entries, handing each one to `visit` as
/// `(block_id, block_version, payload)`.
///
/// Stops at `used`; the padding past it is never framed. A truncated or
/// overrunning entry is refused rather than skipped, because a region that
/// cannot be walked is corrupt and answering from half of it would be worse
/// than saying so.
pub(super) fn walk_header_slot_entries(
    entries: &[u8],
    used: usize,
    mut visit: impl FnMut(u32, u16, &[u8]) -> Result<()>,
) -> Result<()> {
    let mut offset = 0usize;
    while offset < used {
        let rest = used - offset;
        if rest < HEADER_SLOT_ENTRY_HEADER_LEN {
            return Err(Error::InvalidCompressionHeader);
        }
        let head = &entries[offset..offset + HEADER_SLOT_ENTRY_HEADER_LEN];
        let block_id = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
        let block_version = u16::from_le_bytes([head[4], head[5]]);
        let reserved = u16::from_le_bytes([head[6], head[7]]);
        if reserved != 0 {
            return Err(Error::InvalidCompressionHeader);
        }
        let len = u32::from_le_bytes([head[8], head[9], head[10], head[11]]) as usize;
        let start = offset + HEADER_SLOT_ENTRY_HEADER_LEN;
        let end = start
            .checked_add(len)
            .ok_or(Error::InvalidCompressionHeader)?;
        if end > used {
            return Err(Error::InvalidCompressionHeader);
        }
        visit(block_id, block_version, &entries[start..end])?;
        offset = end;
    }
    Ok(())
}

/// The empty region a create writes: framed, zero entries, and unsealed.
pub(super) fn encode_empty_header_slots_region(spec: FormatSpec) -> Result<Vec<u8>> {
    let framed = crate::format::header_slots_region_len(spec);
    let framed_len = usize::try_from(framed)
        .map_err(|_| Error::InvalidFormatSpec("header_slots region length overflows"))?;
    let capacity = spec.header_slots.capacity as usize;
    let payload_len = framed_len - crate::format::HEADER_SLOTS_BLOCK_FRAMING_LEN as usize;
    let payload_len_u32 = u32::try_from(payload_len)
        .map_err(|_| Error::InvalidFormatSpec("header_slots region length overflows"))?;
    let mut region = Vec::new();
    region
        .try_reserve_exact(framed_len)
        .map_err(|_| Error::AllocationFailed {
            resource: "header slots region",
            requested: framed,
        })?;
    region.extend_from_slice(HEADER_SLOTS_MAGIC);
    region.extend_from_slice(&payload_len_u32.to_le_bytes());
    region.extend_from_slice(&encode_header_slots_payload(
        spec,
        0,
        &vec![0u8; capacity],
        false,
    )?);
    debug_assert_eq!(region.len(), framed_len);
    Ok(region)
}

/// Serializes the payload for a region holding `entries` (already padded to the
/// declared capacity), sealing it if asked.
///
/// The checksum is written for every policy and only *checked* for some: a
/// region that later turns its policy up should not have to be rewritten to
/// become verifiable, and writing it costs one pass over bytes already in hand.
pub(super) fn encode_header_slots_payload(
    spec: FormatSpec,
    used: usize,
    entries: &[u8],
    sealed: bool,
) -> Result<Vec<u8>> {
    let capacity = spec.header_slots.capacity as usize;
    if entries.len() != capacity {
        return Err(Error::InvalidFormatSpec("header_slots entry region length"));
    }
    let flags = if sealed { HEADER_SLOTS_FLAG_SEALED } else { 0 };
    let used_u32 =
        u32::try_from(used).map_err(|_| Error::InvalidFormatSpec("header_slots used overflows"))?;
    let capacity_u32 = u32::try_from(capacity)
        .map_err(|_| Error::InvalidFormatSpec("header_slots capacity overflows"))?;
    let mut covered = Vec::with_capacity(12 + capacity);
    covered.extend_from_slice(&HEADER_SLOTS_VERSION.to_le_bytes());
    covered.extend_from_slice(&flags.to_le_bytes());
    covered.extend_from_slice(&capacity_u32.to_le_bytes());
    covered.extend_from_slice(&used_u32.to_le_bytes());
    covered.extend_from_slice(entries);
    let checksum = header_slots_checksum(&covered);

    let mut payload = Vec::new();
    let payload_len = crate::format::HEADER_SLOTS_PREFIX_LEN as usize + capacity;
    payload
        .try_reserve_exact(payload_len)
        .map_err(|_| Error::AllocationFailed {
            resource: "header slots payload",
            requested: payload_len as u64,
        })?;
    payload.extend_from_slice(&covered[..12]);
    payload.extend_from_slice(&checksum.to_le_bytes());
    payload.extend_from_slice(entries);
    debug_assert_eq!(payload.len(), payload_len);
    Ok(payload)
}

pub(super) fn is_mutable_header_extension_magic(magic: &[u8; 4]) -> bool {
    magic == HEADER_TAILS_MAGIC || magic == LIVENESS_MAGIC || magic == HEADER_SLOTS_MAGIC
}

/// Walk the extension region as a block sequence.
///
/// The framing is `magic[4] | len: u32 | payload[len]`, with one exception:
/// `VCHD` shipped before the framing existed and has no length prefix, so it is
/// identified by magic and its payload length is the constant above. Every
/// block added after this walk carries the prefix, which is what makes an
/// unknown one skippable.
///
/// A truncated block, a length that overruns the region, or a repeated magic is
/// refused — skipping an unknown block is forward compatibility, but a region
/// that cannot be framed at all is a corrupt header.
pub(super) fn parse_file_header_extension_blocks(
    region: &[u8],
) -> Result<Vec<HeaderExtensionBlock<'_>>> {
    let mut blocks: Vec<HeaderExtensionBlock<'_>> = Vec::new();
    let mut offset = 0usize;
    while offset < region.len() {
        let start = offset;
        let magic: [u8; 4] = region
            .get(offset..offset + 4)
            .ok_or(Error::InvalidCompressionHeader)?
            .try_into()
            .map_err(|_| Error::InvalidCompressionHeader)?;
        offset += 4;
        let payload_len = if &magic == FILE_COMPRESSION_MAGIC {
            FILE_COMPRESSION_PAYLOAD_LEN
        } else {
            let len: [u8; 4] = region
                .get(offset..offset + 4)
                .ok_or(Error::InvalidCompressionHeader)?
                .try_into()
                .map_err(|_| Error::InvalidCompressionHeader)?;
            offset += 4;
            usize::try_from(u32::from_le_bytes(len)).map_err(|_| Error::InvalidCompressionHeader)?
        };
        offset = offset
            .checked_add(payload_len)
            .ok_or(Error::InvalidCompressionHeader)?;
        if offset > region.len() {
            return Err(Error::InvalidCompressionHeader);
        }
        if blocks.iter().any(|block| block.magic == magic) {
            return Err(Error::InvalidCompressionHeader);
        }
        blocks.push(HeaderExtensionBlock {
            magic,
            offset: start,
            encoded: &region[start..offset],
        });
    }
    Ok(blocks)
}

/// Blank out the header bytes a file is allowed to change after create.
///
/// `prefix` is the file's leading bytes from offset 0 — any length, so a caller
/// holding a fixed window shorter than the header can pass what it has.
/// `header_len` and `extensions` are what `read_file_header_parts` returned for
/// the same file.
///
/// This exists for the two places that bind a *derived* artefact to the primary
/// file by hashing its leading bytes. Both were written when the header was
/// written exactly once, and both say so out loud; an updatable region falsifies
/// that, and without this every in-place update would permanently invalidate
/// every published sidecar for the file.
///
/// **Only the mutable block's payload is blanked.** Its magic and its declared
/// length stay in the hash, so a region that changed size, changed identity or
/// disappeared is still caught — which is the property that matters, because
/// those are the changes that move `append_log_start` and with it every record
/// offset. What stops being detected is a change to the table's *contents*,
/// which is exactly what the option exists to make legal.
///
/// Returns the number of bytes it blanked, so a caller can assert it did
/// something.
pub(crate) fn blank_mutable_header_bytes(
    prefix: &mut [u8],
    header_len: u64,
    extensions: &[u8],
) -> Result<u64> {
    // Every mutable block, not the first. With two of them, blanking one left
    // the other's bytes in the identity hash, and a single flip there would
    // permanently invalidate every published sidecar for the file — the exact
    // defect this function exists to prevent, reintroduced by a second block.
    let mut blanked = 0u64;
    for (_, extent) in mutable_header_block_extents(header_len, extensions)? {
        let Ok(start) = usize::try_from(extent.payload_offset) else {
            continue;
        };
        // A caller's window may end inside the region, or before it starts.
        let Some(window) = prefix.get_mut(start..) else {
            continue;
        };
        let take = extent.payload_len.min(window.len());
        window[..take].fill(0);
        blanked = blanked.saturating_add(take as u64);
    }
    Ok(blanked)
}

/// Where the mutable extension block's payload lives, as a file offset.
///
/// **One derivation, two callers**: the blanking above, which has to skip these
/// bytes, and the commit-time writer, which has to hit exactly them. Computing
/// it twice is the shape of defect that put a wrong extension length in the
/// published layout plan, and it would be worse here — a writer and a hasher
/// that disagree by a few bytes fail silently in opposite directions.
///
/// `None` when the file carries no mutable block, which is every file whose
/// format does not declare `header_tails`.
pub(super) struct MutableHeaderExtent {
    /// Where the payload starts as a *file* offset, framing excluded — what the
    /// writer seeks to and what the hasher skips.
    pub(super) payload_offset: u64,
    /// The same payload's start *within the extension region*, for a caller
    /// holding the region rather than the file.
    pub(super) payload_in_extensions: usize,
    pub(super) payload_len: usize,
}

/// Every mutable block's extent, in the order they sit in the region.
///
/// There is more than one now — the header-tail table and the liveness dirty
/// bit — and the difference matters most to the *hasher*: the sidecar identity
/// windows have to skip **all** of them, because a single unskipped mutable
/// byte means one flip permanently invalidates every published sidecar for the
/// file, which is the defect task #39 fixed when there was only one block.
pub(super) fn mutable_header_block_extents(
    header_len: u64,
    extensions: &[u8],
) -> Result<Vec<([u8; 4], MutableHeaderExtent)>> {
    let mut found = Vec::new();
    if extensions.is_empty() {
        return Ok(found);
    }
    // The region sits at the end of the header, so its start is the header's
    // length less its own. Deriving it rather than recomputing the header
    // layout keeps this correct for a file whose region is longer than this
    // build would write - which is the whole point of skippable unknown blocks.
    let extension_len =
        u64::try_from(extensions.len()).map_err(|_| Error::InvalidCompressionHeader)?;
    let region_start = header_len
        .checked_sub(extension_len)
        .ok_or(Error::InvalidCompressionHeader)?;

    for block in parse_file_header_extension_blocks(extensions)? {
        if !is_mutable_header_extension_magic(&block.magic) {
            continue;
        }
        // Every mutable block carries the `magic | len: u32` framing; `VCHD` is
        // the one block that does not, and it is not mutable. Asserting it here
        // rather than assuming it keeps the arithmetic below honest if that
        // ever changes.
        debug_assert_ne!(&block.magic, FILE_COMPRESSION_MAGIC);
        let framing = HEADER_TAILS_BLOCK_FRAMING_LEN;
        let payload_len = block.encoded.len().saturating_sub(framing);
        let payload_in_extensions = block
            .offset
            .checked_add(framing)
            .ok_or(Error::InvalidCompressionHeader)?;
        let payload_offset = region_start
            .checked_add(
                u64::try_from(payload_in_extensions)
                    .map_err(|_| Error::InvalidCompressionHeader)?,
            )
            .ok_or(Error::InvalidCompressionHeader)?;
        found.push((
            block.magic,
            MutableHeaderExtent {
                payload_offset,
                payload_in_extensions,
                payload_len,
            },
        ));
    }
    Ok(found)
}

/// One named mutable block's extent, for a writer that needs to hit exactly it.
pub(super) fn mutable_header_block_extent(
    header_len: u64,
    extensions: &[u8],
    magic: &[u8; 4],
) -> Result<Option<MutableHeaderExtent>> {
    Ok(mutable_header_block_extents(header_len, extensions)?
        .into_iter()
        .find(|(found, _)| found == magic)
        .map(|(_, extent)| extent))
}

/// The extension region to write into the file a `replace_*` builds.
///
/// Everything the old file carried, including blocks this build does not know,
/// except that a mutable block is reset to cold. Copying `VBTT` forward would
/// publish a table of offsets measured against the *old* file, and
/// `replace_block` moves every record after the replaced one by a uniform
/// delta — so every entry in it would be wrong by that delta, and wrong in a
/// way that frames a real record at the wrong place rather than failing to
/// frame one. A cold table claims nothing and the next commit fills it.
///
/// The reset is length-preserving by construction: the cold region and the one
/// the file carries are both `encode_cold_header_tails_region`'s output for the
/// same spec, and a file whose region is a different length was refused at open.
pub(super) fn header_extensions_for_replacement(
    spec: FormatSpec,
    extensions: &[u8],
) -> Result<Vec<u8>> {
    if !spec.index_policy.header_tails {
        return Ok(extensions.to_vec());
    }
    let cold = encode_cold_header_tails_region(spec)?;
    let (offset, len) = {
        let blocks = parse_file_header_extension_blocks(extensions)?;
        let block = blocks
            .iter()
            .find(|block| &block.magic == HEADER_TAILS_MAGIC)
            .ok_or(Error::InvalidCompressionHeader)?;
        (block.offset, block.encoded.len())
    };
    if len != cold.len() {
        return Err(Error::InvalidCompressionHeader);
    }
    let mut out = extensions.to_vec();
    out.get_mut(offset..offset + len)
        .ok_or(Error::InvalidCompressionHeader)?
        .copy_from_slice(&cold);
    Ok(out)
}

/// Judge the region a file carries against the one this spec would write.
///
/// A block this build knows must be present exactly when it is expected and
/// must match byte for byte; a block it does not know is stepped over. The
/// equality fast path keeps the ordinary case — a file written by this build —
/// off the walk entirely.
///
/// This buys forward compatibility from the release that carries it, not
/// backward compatibility with the ones that do not: a reader older than this
/// walk still refuses any region it would not have written itself.
pub(super) fn validate_file_header_extensions(spec: FormatSpec, extensions: &[u8]) -> Result<()> {
    let expected = file_header_extensions(spec)?;
    if expected == extensions {
        return Ok(());
    }
    let present = parse_file_header_extension_blocks(extensions)?;
    let expected_blocks = parse_file_header_extension_blocks(&expected)?;
    for expected_block in &expected_blocks {
        let found = present
            .iter()
            .find(|block| block.magic == expected_block.magic)
            .ok_or(Error::InvalidCompressionHeader)?;
        if is_mutable_header_extension_magic(&expected_block.magic) {
            // Contents are the file's to change; the length is not. Comparing
            // it is what keeps every offset derived from `header_len` valid.
            if found.encoded.len() != expected_block.encoded.len() {
                return Err(Error::InvalidCompressionHeader);
            }
            continue;
        }
        if found.encoded != expected_block.encoded {
            return Err(Error::InvalidCompressionHeader);
        }
    }
    for block in &present {
        if !is_known_header_extension_magic(&block.magic) {
            continue;
        }
        if !expected_blocks
            .iter()
            .any(|expected_block| expected_block.magic == block.magic)
        {
            return Err(Error::InvalidCompressionHeader);
        }
    }
    Ok(())
}

pub(super) fn compression_algorithm_byte(algorithm: CompressionAlgorithm) -> u8 {
    match algorithm {
        CompressionAlgorithm::Zstd => 1,
    }
}

pub(super) fn compression_algorithm_from_byte(value: u8) -> Result<CompressionAlgorithm> {
    match value {
        1 => Ok(CompressionAlgorithm::Zstd),
        other => Err(Error::UnsupportedCompressionAlgorithm(other)),
    }
}

pub(super) fn compression_level_kind_byte(level: CompressionLevel) -> u8 {
    match level {
        CompressionLevel::Fast => 1,
        CompressionLevel::Default => 2,
        CompressionLevel::Best => 3,
        CompressionLevel::Exact(_) => 4,
    }
}

pub(super) fn compression_level_exact(level: CompressionLevel) -> i32 {
    match level {
        CompressionLevel::Fast | CompressionLevel::Default | CompressionLevel::Best => 0,
        CompressionLevel::Exact(value) => value,
    }
}

pub(super) fn compression_level_from_parts(kind: u8, exact: i32) -> Result<CompressionLevel> {
    match kind {
        1 => Ok(CompressionLevel::Fast),
        2 => Ok(CompressionLevel::Default),
        3 => Ok(CompressionLevel::Best),
        4 => Ok(CompressionLevel::Exact(exact)),
        _ => Err(Error::InvalidCompressionHeader),
    }
}

pub(super) fn compression_header_mode_byte(mode: CompressionHeaderMode) -> u8 {
    match mode {
        CompressionHeaderMode::RecordExplicit => 1,
        CompressionHeaderMode::FileExplicit => 2,
        CompressionHeaderMode::FormatContract => 3,
    }
}

pub(super) fn compression_header_mode_from_byte(value: u8) -> Result<CompressionHeaderMode> {
    match value {
        1 => Ok(CompressionHeaderMode::RecordExplicit),
        2 => Ok(CompressionHeaderMode::FileExplicit),
        3 => Ok(CompressionHeaderMode::FormatContract),
        _ => Err(Error::InvalidCompressionHeader),
    }
}

#[cfg(feature = "compression-zstd")]
pub(super) fn ensure_compression_algorithm_available(
    algorithm: CompressionAlgorithm,
) -> Result<()> {
    match algorithm {
        CompressionAlgorithm::Zstd => Ok(()),
    }
}

#[cfg(not(feature = "compression-zstd"))]
pub(super) fn ensure_compression_algorithm_available(
    _algorithm: CompressionAlgorithm,
) -> Result<()> {
    Err(Error::CompressionFeatureDisabled)
}

#[cfg(feature = "compression-zstd")]
pub(super) fn compress_with_algorithm(
    algorithm: CompressionAlgorithm,
    level: CompressionLevel,
    payload: &[u8],
) -> Result<Vec<u8>> {
    match algorithm {
        CompressionAlgorithm::Zstd => {
            zstd::bulk::compress(payload, level.to_zstd_level()).map_err(Error::Io)
        }
    }
}

#[cfg(not(feature = "compression-zstd"))]
pub(super) fn compress_with_algorithm(
    algorithm: CompressionAlgorithm,
    _level: CompressionLevel,
    _payload: &[u8],
) -> Result<Vec<u8>> {
    ensure_compression_algorithm_available(algorithm)?;
    unreachable!("compression algorithm availability returned Ok without a backend")
}

#[cfg(feature = "compression-zstd")]
pub(super) fn decompress_with_algorithm(
    algorithm: CompressionAlgorithm,
    payload: &[u8],
    expected_len: u64,
) -> Result<Vec<u8>> {
    let capacity = usize::try_from(expected_len).map_err(|_| Error::LengthOverflow {
        value: expected_len,
    })?;
    match algorithm {
        CompressionAlgorithm::Zstd => zstd::bulk::decompress(payload, capacity).map_err(Error::Io),
    }
}

#[cfg(not(feature = "compression-zstd"))]
pub(super) fn decompress_with_algorithm(
    algorithm: CompressionAlgorithm,
    _payload: &[u8],
    _expected_len: u64,
) -> Result<Vec<u8>> {
    ensure_compression_algorithm_available(algorithm)?;
    unreachable!("compression algorithm availability returned Ok without a backend")
}
