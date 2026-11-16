//! The checksum a record stores in its footer, computed from bytes in memory
//! or streamed from a file, for each integrity policy.
use super::*;

pub(super) fn checksum_record_file(
    spec: FormatSpec,
    file: &mut (impl Read + Seek),
    entry: &RecordIndexEntry,
    footer: &[u8],
) -> Result<u32> {
    let header = RecordHeaderFields {
        block_id: entry.block_id,
        block_version: entry.block_version,
        flags: entry.flags,
        sequence: entry.sequence,
        payload_len: entry.payload_len,
        checksum: 0,
        uncompressed_len_hint: entry.uncompressed_len_hint,
    };
    let header_bytes = if spec.integrity_policy == IntegrityPolicy::Crc32WithHeader {
        Some(encode_native_record_header(
            header,
            entry.record_offset,
            record_footer_len(spec),
        )?)
    } else {
        None
    };
    crc32_record_file(
        file,
        entry.payload_offset,
        if entry.flags & RECORD_FLAG_IN_PLACE != 0 {
            0
        } else {
            entry.payload_len
        },
        header_bytes.as_ref().map(|bytes| &bytes[..]).unwrap_or(&[]),
        checksummed_footer(spec, footer),
    )
}

#[cfg(feature = "integrity")]
pub(super) fn crc32_record_file(
    file: &mut (impl Read + Seek),
    payload_offset: u64,
    payload_len: u64,
    header: &[u8],
    footer: &[u8],
) -> Result<u32> {
    if payload_len <= 1024 {
        crc32_record_file_buffered::<1024>(file, payload_offset, payload_len, header, footer)
    } else {
        crc32_record_file_buffered::<STREAM_BUFFER_LEN>(
            file,
            payload_offset,
            payload_len,
            header,
            footer,
        )
    }
}

#[cfg(feature = "integrity")]
#[inline(never)]
pub(super) fn crc32_record_file_buffered<const BUFFER: usize>(
    file: &mut (impl Read + Seek),
    payload_offset: u64,
    payload_len: u64,
    header: &[u8],
    footer: &[u8],
) -> Result<u32> {
    const {
        assert!(BUFFER > 0);
    }
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(header);
    file.seek(SeekFrom::Start(payload_offset))?;
    let mut remaining = payload_len;
    let mut buffer = [0; BUFFER];
    while remaining != 0 {
        let chunk_len = usize::try_from(remaining.min(BUFFER as u64)).map_err(|_| {
            Error::ResourceArithmeticOverflow {
                resource: "checksum buffer length",
            }
        })?;
        file.read_exact(&mut buffer[..chunk_len])?;
        hasher.update(&buffer[..chunk_len]);
        remaining -= chunk_len as u64;
    }
    hasher.update(footer);
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
pub(super) fn crc32_record_file(
    _file: &mut (impl Read + Seek),
    _payload_offset: u64,
    _payload_len: u64,
    _header: &[u8],
    _footer: &[u8],
) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

/// How much of a record footer the checksum covers.
///
/// The whole thing under [`LivenessPolicy::None`], which is what every file
/// written before that policy existed carries. Under
/// [`LivenessPolicy::FooterFlags`] the trailing word is excluded, and that
/// exclusion is the entire mechanism: it is what lets a writer set a flag there
/// afterwards without re-reading the payload to recompute a CRC, and without
/// making every reader that already framed the record disagree with the disk.
///
/// The price, stated once: those four bytes are unauthenticated. A flipped bit
/// in them is not detected by anything, so the flags are advisory — a set flag
/// says "the writer marked this", never "these bytes are sound".
pub(super) fn checksummed_footer(spec: FormatSpec, footer: &[u8]) -> &[u8] {
    match spec.liveness_policy {
        LivenessPolicy::None => footer,
        LivenessPolicy::FooterFlags => footer
            .len()
            .checked_sub(crate::width::small(RECORD_FOOTER_MUTABLE_LEN))
            .map_or(footer, |end| &footer[..end]),
    }
}

pub(super) fn checksum_record_bytes(
    spec: FormatSpec,
    header: &[u8],
    payload: &[u8],
    footer: &[u8],
) -> Result<u32> {
    match spec.integrity_policy {
        IntegrityPolicy::None => Ok(0),
        IntegrityPolicy::Crc32 => {
            crc32_record_bytes(&[], payload, checksummed_footer(spec, footer))
        }
        IntegrityPolicy::Crc32WithHeader => {
            crc32_record_bytes(header, payload, checksummed_footer(spec, footer))
        }
    }
}

#[cfg(feature = "integrity")]
pub(super) fn crc32_bytes(payload: &[u8]) -> Result<u32> {
    Ok(crc32fast::hash(payload))
}

#[cfg(not(feature = "integrity"))]
pub(super) fn crc32_bytes(_payload: &[u8]) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

#[cfg(feature = "integrity")]
pub(super) fn crc32_record_bytes(header: &[u8], payload: &[u8], footer: &[u8]) -> Result<u32> {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(header);
    hasher.update(payload);
    hasher.update(footer);
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
pub(super) fn crc32_record_bytes(_header: &[u8], _payload: &[u8], _footer: &[u8]) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}
