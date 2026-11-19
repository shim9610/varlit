// Encodings of logical payloads, internal keys and merge operations.
// Included by `file.rs`, whose module it shares.

/// Frames a metadata record's payload as `(key, value)` without copying either.
///
/// [`VarlitFile::write_metadata`] encodes `(String, Vec<u8>)` through the tuple
/// codec, which is exactly `u64 key_len | key | u64 value_len | value`, and
/// metadata goes through `write_record` rather than `write_user_record`, so it
/// is never compressed. That framing is what the borrowed pair reads back.
///
/// It exists so a lookup can compare the key *before* deciding to materialize
/// the value: the decoder allocates a `String` and a `Vec<u8>` per record,
/// which for a lookup is one copy per record that is thrown away. The errors
/// are the ones `decode_complete` raises on the same bytes —
/// [`Error::UnexpectedEof`], [`Error::LengthOverflow`], [`Error::InvalidUtf8`]
/// and [`Error::TrailingBytes`] — because the byte-for-byte answer must not
/// depend on which of the two readers looked.
///
/// The `0` materialization allowance is not a limit being enforced: nothing is
/// materialized here, so nothing is charged. Callers charge the record they
/// then decode.
fn split_metadata_payload(payload: &[u8], endian: Endian) -> Result<(&str, &[u8])> {
    let mut decoder = crate::Decoder::new_limited(payload, endian, 0);
    let key_len = decoder.read_len()?;
    if key_len > decoder.remaining() {
        return Err(Error::UnexpectedEof);
    }
    let key = decoder.read_exact(key_len)?;
    let key = std::str::from_utf8(key).map_err(|_| Error::InvalidUtf8)?;
    let value_len = decoder.read_len()?;
    if value_len > decoder.remaining() {
        return Err(Error::UnexpectedEof);
    }
    let value = decoder.read_exact(value_len)?;
    if decoder.remaining() != 0 {
        return Err(Error::TrailingBytes {
            remaining: decoder.remaining(),
        });
    }
    Ok((key, value))
}

/// Encodes a writer-supplied value with the format's logical-payload limit as
/// the encode hard bound.
///
/// Every resident writer entry point routes its value encoding through this
/// bound so an oversized value fails with the typed limit error while the
/// encoder stops buffering at the limit, instead of materializing an
/// arbitrarily large encoding first and only then failing the limit check.
fn encode_logical_payload_limited<T: VarlitEncode>(
    spec: FormatSpec,
    value: &T,
    endian: Endian,
) -> Result<Vec<u8>> {
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    encode_to_vec_limited(
        value,
        endian,
        logical_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )
}

/// Leaves exactly `room` bytes in `buffer`, zeroed the first time and kept
/// from then on, for an encoder to append after.
fn keep_room(buffer: &mut Vec<u8>, room: usize) -> Result<()> {
    if buffer.len() >= room {
        buffer.truncate(room);
        return Ok(());
    }
    buffer.clear();
    buffer
        .try_reserve(room)
        .map_err(|_| Error::AllocationFailed {
            resource: "record payload room",
            requested: room as u64,
        })?;
    buffer.resize(room, 0);
    Ok(())
}

/// Encodes a block's logical payload after what `buffer` already holds,
/// limited by the format's logical payload limit, which counts only the
/// appended bytes.
fn encode_logical_payload_limited_after<T: VarlitEncode>(
    spec: FormatSpec,
    value: &T,
    endian: Endian,
    buffer: &mut Vec<u8>,
) -> Result<()> {
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    let mut encoder = crate::codec::Encoder::new_limited_appending(
        endian,
        logical_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
        std::mem::take(buffer),
    );
    let encoded = value.encode_varlit(&mut encoder);
    let (output, finished) = encoder.into_buffer_and_result();
    *buffer = output;
    encoded.and(finished)
}

fn encode_internal_key_payload<T>(spec: FormatSpec, key: &T::Key) -> Result<Vec<u8>>
where
    T: VarlitKeyedBlock,
{
    let mut payload = Vec::new();
    encode_internal_key_payload_into::<T>(spec, key, &mut payload)?;
    Ok(payload)
}

/// [`encode_internal_key_payload`] into a buffer the caller owns: the
/// envelope and the key are written in place, so a writer that reuses its
/// buffer encodes a tombstone without allocating.
fn encode_internal_key_payload_into<T>(
    spec: FormatSpec,
    key: &T::Key,
    payload: &mut Vec<u8>,
) -> Result<()>
where
    T: VarlitKeyedBlock,
{
    encode_key_envelope_into::<T>(spec, spec.endian, key, payload)
}

/// The key of a keyed-storage record: the internal key envelope with the
/// declared key fields encoded big-endian, whatever the format's endian, so
/// that byte order follows the order of unsigned key values and keys put in
/// increasing order fill the B-tree's pages in order.
fn encode_keyed_storage_key_into<T>(
    spec: FormatSpec,
    key: &T::Key,
    payload: &mut Vec<u8>,
) -> Result<()>
where
    T: VarlitKeyedBlock,
{
    encode_key_envelope_into::<T>(spec, Endian::Big, key, payload)
}

fn encode_key_envelope_into<T>(
    spec: FormatSpec,
    endian: Endian,
    key: &T::Key,
    payload: &mut Vec<u8>,
) -> Result<()>
where
    T: VarlitKeyedBlock,
{
    const ENVELOPE_LEN: u64 = 12;
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    let key_limit = logical_limit
        .checked_sub(ENVELOPE_LEN)
        .ok_or(Error::LimitExceeded {
            resource: ReadLimitKey::LogicalPayloadLen.resource(),
            actual: ENVELOPE_LEN,
            limit: logical_limit,
        })?;
    payload.clear();
    payload
        .try_reserve(crate::width::small(ENVELOPE_LEN))
        .map_err(|_| Error::AllocationFailed {
            resource: "internal key payload",
            requested: ENVELOPE_LEN,
        })?;
    payload.extend_from_slice(&T::ID.to_le_bytes());
    payload.extend_from_slice(&[0; 8]);
    let mut encoder = crate::codec::Encoder::new_limited_appending(
        endian,
        key_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
        std::mem::take(payload),
    );
    let encoded = key.encode_varlit(&mut encoder);
    let (output, finished) = encoder.into_buffer_and_result();
    *payload = output;
    if let Err(error) = encoded.and(finished) {
        payload.clear();
        return Err(error);
    }
    let key_len = (payload.len() - crate::width::small(ENVELOPE_LEN)) as u64;
    payload[4..12].copy_from_slice(&key_len.to_le_bytes());
    let total_len = payload.len() as u64;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, total_len)?;
    Ok(())
}

fn decode_internal_key_payload<T>(
    endian: Endian,
    payload: &[u8],
    budget: &mut MaterializationBudget,
) -> Result<Option<T::Key>>
where
    T: VarlitKeyedBlock,
{
    let Some(key) = decode_native_internal_key_envelope(payload, T::ID)? else {
        return Ok(None);
    };
    Ok(Some(budget.decode(key, endian)?))
}

fn encode_internal_op_payload<T>(spec: FormatSpec, key: &T::Key, op: &T::Op) -> Result<Vec<u8>>
where
    T: VarlitMerge,
{
    const ENVELOPE_LEN: u64 = 20;
    let logical_limit = spec
        .read_limits
        .require(ReadLimitKey::LogicalPayloadLen)?
        .unwrap_or(u64::MAX);
    let key_limit = logical_limit
        .checked_sub(ENVELOPE_LEN)
        .ok_or(Error::LimitExceeded {
            resource: ReadLimitKey::LogicalPayloadLen.resource(),
            actual: ENVELOPE_LEN,
            limit: logical_limit,
        })?;
    let key_payload = encode_to_vec_limited(
        key,
        spec.endian,
        key_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )?;
    // the op encoder inherits whatever budget the key left over, so
    // the combined encoding can never buffer past the logical payload limit.
    let op_limit = key_limit - key_payload.len() as u64;
    let op_payload = encode_to_vec_limited(
        op,
        spec.endian,
        op_limit,
        ReadLimitKey::LogicalPayloadLen.resource(),
    )?;
    let total_len = ENVELOPE_LEN
        .checked_add(key_payload.len() as u64)
        .and_then(|len| len.checked_add(op_payload.len() as u64))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "internal op payload length",
        })?;
    spec.read_limits
        .check(ReadLimitKey::LogicalPayloadLen, total_len)?;
    let total_len_usize =
        usize::try_from(total_len).map_err(|_| Error::LengthOverflow { value: total_len })?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(total_len_usize)
        .map_err(|_| Error::AllocationFailed {
            resource: "internal op payload",
            requested: total_len,
        })?;
    payload.extend_from_slice(&T::ID.to_le_bytes());
    payload.extend_from_slice(&(key_payload.len() as u64).to_le_bytes());
    payload.extend_from_slice(&key_payload);
    payload.extend_from_slice(&(op_payload.len() as u64).to_le_bytes());
    payload.extend_from_slice(&op_payload);
    Ok(payload)
}

fn decode_internal_op_payload<T>(
    endian: Endian,
    payload: &[u8],
    budget: &mut MaterializationBudget,
) -> Result<Option<(T::Key, T::Op)>>
where
    T: VarlitMerge,
{
    let Some((key, op)) = decode_native_internal_op_envelope(payload, T::ID)? else {
        return Ok(None);
    };
    let key = budget.decode(key, endian)?;
    let op = budget.decode(op, endian)?;
    Ok(Some((key, op)))
}
