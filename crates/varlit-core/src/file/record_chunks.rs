//! Chunked variable records: a CRC for every 64 KiB of logical payload.
//!
//! A variable record whose logical payload is longer than one chunk (and whose
//! format keeps data checksums) is stored with [`RECORD_FLAG_CHUNKED`]:
//!
//! ```text
//! header (24 B): "VRCK" | version u8 | algorithm u8 | reserved u16 = 0
//!                | logical length u64 | chunk count u32 | CRC32 of bytes 0..20
//! table (16 B per chunk): stored end u64 (from the data start) | CRC32 of
//!                the chunk's logical bytes | reserved u32 = 0
//! data: the chunks, each its logical bytes (algorithm 0) or one
//!                independent compressed frame
//! ```
//!
//! A reader can therefore verify, and hand out, any one chunk without reading
//! the rest of the record, and never holds more than a chunk of it. The table
//! sits inside the payload, so every path that copies a payload byte for byte
//! (rewrites, replacement) carries it unchanged; the record checksum still
//! covers the whole stored payload.

use super::*;

/// Logical bytes per chunk.
pub(crate) const CHUNK_LEN: u64 = 64 * 1024;
pub(crate) const CHUNK_HEADER_LEN: u64 = 24;
pub(crate) const CHUNK_ENTRY_LEN: u64 = 16;
const CHUNK_MAGIC: &[u8; 4] = b"VRCK";
const CHUNK_VERSION: u8 = 1;
const ALGORITHM_NONE: u8 = 0;
/// The most a compressed chunk can take: zstd's bound for one chunk
/// (`n + n/256 + (128 KiB - n)/2048` for `n` under 128 KiB).
pub(crate) const MAX_STORED_CHUNK: u64 =
    CHUNK_LEN + CHUNK_LEN / 256 + (128 * 1024 - CHUNK_LEN) / 2048;

/// Whether a variable record of `logical_len` bytes is stored chunked.
pub(crate) fn chunking_applies(spec: FormatSpec, kind: BlockKind, logical_len: u64) -> bool {
    kind == BlockKind::Variable
        && spec.integrity_policy != IntegrityPolicy::None
        && logical_len > CHUNK_LEN
}

fn malformed(what: &'static str) -> Error {
    Error::InvalidCanonicalEncoding(what)
}

/// The decoded header of a chunked payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChunkHeader {
    pub(crate) algorithm: Option<CompressionAlgorithm>,
    pub(crate) logical_len: u64,
    pub(crate) count: u64,
}

impl ChunkHeader {
    fn new(algorithm: Option<CompressionAlgorithm>, logical_len: u64) -> Self {
        Self {
            algorithm,
            logical_len,
            count: logical_len.div_ceil(CHUNK_LEN),
        }
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let bytes = bytes
            .get(..CHUNK_HEADER_LEN as usize)
            .ok_or(malformed("chunked record header is short"))?;
        if &bytes[0..4] != CHUNK_MAGIC {
            return Err(malformed("chunked record header magic"));
        }
        let stored = u32::from_le_bytes(bytes[20..24].try_into().expect("four"));
        if crc32fast::hash(&bytes[..20]) != stored {
            return Err(malformed("chunked record header checksum"));
        }
        if bytes[4] != CHUNK_VERSION || bytes[6..8] != [0, 0] {
            return Err(malformed("chunked record header version"));
        }
        let algorithm = match bytes[5] {
            ALGORITHM_NONE => None,
            other => Some(compression_algorithm_from_byte(other)?),
        };
        let logical_len = u64::from_le_bytes(bytes[8..16].try_into().expect("eight"));
        let count = u64::from(u32::from_le_bytes(bytes[16..20].try_into().expect("four")));
        let header = Self::new(algorithm, logical_len);
        // Only records longer than one chunk are chunked, and the count is
        // what the length implies.
        if logical_len <= CHUNK_LEN || header.count != count {
            return Err(malformed("chunked record length and count disagree"));
        }
        Ok(header)
    }

    fn encode(&self) -> Result<[u8; CHUNK_HEADER_LEN as usize]> {
        let mut out = [0u8; CHUNK_HEADER_LEN as usize];
        out[0..4].copy_from_slice(CHUNK_MAGIC);
        out[4] = CHUNK_VERSION;
        out[5] = self
            .algorithm
            .map_or(ALGORITHM_NONE, compression_algorithm_byte);
        out[8..16].copy_from_slice(&self.logical_len.to_le_bytes());
        let count = u32::try_from(self.count).map_err(|_| Error::LengthOverflow {
            value: self.logical_len,
        })?;
        out[16..20].copy_from_slice(&count.to_le_bytes());
        let crc = crc32fast::hash(&out[..20]);
        out[20..24].copy_from_slice(&crc.to_le_bytes());
        Ok(out)
    }

    /// Where the chunk data starts in the payload.
    pub(crate) fn data_at(&self) -> u64 {
        CHUNK_HEADER_LEN + CHUNK_ENTRY_LEN * self.count
    }

    /// The logical length of chunk `index`.
    pub(crate) fn chunk_len(&self, index: u64) -> u64 {
        CHUNK_LEN.min(self.logical_len - index * CHUNK_LEN)
    }
}

/// The room [`lay_plain_chunks`] needs in front of a logical payload of up to
/// `max_logical` bytes, when that is small enough (at most 1 MiB of header and
/// table) for a writer to keep it in front of every payload it encodes.
pub(crate) fn plain_prefix_room(max_logical: u64) -> Option<usize> {
    let count = max_logical.div_ceil(CHUNK_LEN);
    let room = CHUNK_HEADER_LEN.checked_add(CHUNK_ENTRY_LEN.checked_mul(count)?)?;
    if room > 1 << 20 {
        return None;
    }
    usize::try_from(room).ok()
}

/// Writes the header and table of a plain (uncompressed) chunked payload
/// directly in front of the logical bytes at `buffer[room..]` and returns
/// where the stored payload starts.
///
/// `buffer[start..]` is then byte for byte what [`encode_chunked`] returns for
/// the same logical bytes without compression, and no logical byte has moved:
/// the payload is stored from the buffer it was encoded into instead of being
/// copied into a new one.
pub(crate) fn lay_plain_chunks(buffer: &mut [u8], room: usize) -> Result<usize> {
    let logical_len = buffer
        .len()
        .checked_sub(room)
        .ok_or(malformed("chunk room past the encoded payload"))? as u64;
    let header = ChunkHeader::new(None, logical_len);
    let prefix = usize::try_from(header.data_at()).map_err(|_| Error::LengthOverflow {
        value: header.data_at(),
    })?;
    let start = room.checked_sub(prefix).ok_or(malformed(
        "chunk table larger than the room set aside for it",
    ))?;
    let (front, logical) = buffer.split_at_mut(room);
    let front = &mut front[start..];
    front[..CHUNK_HEADER_LEN as usize].copy_from_slice(&header.encode()?);
    for (index, chunk) in logical.chunks(CHUNK_LEN as usize).enumerate() {
        let end = index as u64 * CHUNK_LEN + chunk.len() as u64;
        let at = CHUNK_HEADER_LEN as usize + index * CHUNK_ENTRY_LEN as usize;
        front[at..at + 8].copy_from_slice(&end.to_le_bytes());
        front[at + 8..at + 12].copy_from_slice(&crc32fast::hash(chunk).to_le_bytes());
        front[at + 12..at + 16].fill(0);
    }
    Ok(start)
}

/// Encodes `logical` as a chunked payload, compressing each chunk on its own
/// when `compression` is given.
pub(crate) fn encode_chunked(
    logical: &[u8],
    compression: Option<(CompressionAlgorithm, CompressionLevel)>,
) -> Result<Vec<u8>> {
    let header = ChunkHeader::new(
        compression.map(|(algorithm, _)| algorithm),
        logical.len() as u64,
    );
    let data_at = header.data_at() as usize;
    let mut out = Vec::new();
    let initial = data_at
        .checked_add(if compression.is_some() {
            0
        } else {
            logical.len()
        })
        .ok_or(Error::LengthOverflow {
            value: logical.len() as u64,
        })?;
    out.try_reserve_exact(initial)
        .map_err(|_| Error::AllocationFailed {
            resource: "chunked record payload",
            requested: initial as u64,
        })?;
    out.resize(data_at, 0);
    out[..CHUNK_HEADER_LEN as usize].copy_from_slice(&header.encode()?);
    let mut compressor = match compression {
        Some((CompressionAlgorithm::Zstd, level)) => {
            Some(crate::compression_buffers::Compressor::new(level)?)
        }
        None => None,
    };
    for (index, chunk) in logical.chunks(CHUNK_LEN as usize).enumerate() {
        let stored: &[u8] = match compressor.as_mut() {
            Some(compressor) => compressor.compress(chunk)?,
            None => chunk,
        };
        out.try_reserve(stored.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "chunked record payload",
                requested: stored.len() as u64,
            })?;
        out.extend_from_slice(stored);
        let end = (out.len() - data_at) as u64;
        let at = CHUNK_HEADER_LEN as usize + index * CHUNK_ENTRY_LEN as usize;
        out[at..at + 8].copy_from_slice(&end.to_le_bytes());
        out[at + 8..at + 12].copy_from_slice(&crc32fast::hash(chunk).to_le_bytes());
    }
    Ok(out)
}

/// Reads payload-relative bytes of one record.
pub(crate) type PayloadRead<'a> = &'a mut dyn FnMut(u64, &mut [u8]) -> Result<()>;

/// Where chunk `index` is stored, payload-relative, and its logical CRC.
fn chunk_extent(
    read: PayloadRead<'_>,
    header: &ChunkHeader,
    payload_len: u64,
    index: u64,
) -> Result<(u64, u64, u32)> {
    let entry = |read: PayloadRead<'_>, at: u64| -> Result<(u64, u32)> {
        let mut bytes = [0u8; CHUNK_ENTRY_LEN as usize];
        read(CHUNK_HEADER_LEN + at * CHUNK_ENTRY_LEN, &mut bytes)?;
        if bytes[12..16] != [0; 4] {
            return Err(malformed("chunked record table entry"));
        }
        Ok((
            u64::from_le_bytes(bytes[0..8].try_into().expect("eight")),
            u32::from_le_bytes(bytes[8..12].try_into().expect("four")),
        ))
    };
    let start = if index == 0 {
        0
    } else {
        entry(read, index - 1)?.0
    };
    let (end, crc) = entry(read, index)?;
    let data_at = header.data_at();
    let data_len = payload_len
        .checked_sub(data_at)
        .ok_or(malformed("chunked record table exceeds its payload"))?;
    let stored = end
        .checked_sub(start)
        .ok_or(malformed("chunked record table is not ascending"))?;
    let fits = match header.algorithm {
        None => stored == header.chunk_len(index),
        Some(_) => stored > 0 && stored <= MAX_STORED_CHUNK,
    };
    let last_ends = index + 1 != header.count || end == data_len;
    if !fits || end > data_len || !last_ends {
        return Err(malformed("chunked record table entry out of range"));
    }
    Ok((data_at + start, data_at + end, crc))
}

/// Reads chunk `index` of a chunked payload into `out` (exactly its logical
/// length) and verifies it: `out` holds checked bytes when this returns `Ok`.
/// `scratch` holds a compressed chunk; `base` is the payload's file offset,
/// for the error.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_chunk(
    read: PayloadRead<'_>,
    header: &ChunkHeader,
    payload_len: u64,
    base: u64,
    index: u64,
    out: &mut [u8],
    scratch: &mut Vec<u8>,
    decompressor: &mut Option<crate::compression_buffers::Decompressor>,
) -> Result<()> {
    debug_assert_eq!(out.len() as u64, header.chunk_len(index));
    let (start, end, crc) = chunk_extent(read, header, payload_len, index)?;
    match header.algorithm {
        None => read(start, out)?,
        Some(CompressionAlgorithm::Zstd) => {
            let stored = (end - start) as usize;
            scratch.clear();
            scratch
                .try_reserve_exact(stored)
                .map_err(|_| Error::AllocationFailed {
                    resource: "compressed chunk",
                    requested: stored as u64,
                })?;
            scratch.resize(stored, 0);
            read(start, scratch)?;
            if decompressor.is_none() {
                *decompressor = Some(crate::compression_buffers::Decompressor::new()?);
            }
            decompressor
                .as_mut()
                .expect("set above")
                .decompress_into(scratch, out)?;
        }
    }
    if crc32fast::hash(out) != crc {
        return Err(Error::ChecksumMismatch {
            offset: base + start,
        });
    }
    Ok(())
}

/// The logical bytes of an in-memory chunked payload whose chunks are stored
/// as they are (not compressed), every chunk verified where it lies: they
/// are back to back after the table, so nothing is copied. `None` for a
/// payload with compressed chunks.
#[cfg(feature = "mmap")]
pub(crate) fn chunked_in_place(payload: &[u8], max: u64) -> Result<Option<&[u8]>> {
    let header = ChunkHeader::decode(payload)?;
    if header.algorithm.is_some() {
        return Ok(None);
    }
    if header.logical_len > max {
        return Err(Error::DecompressedLengthLimitExceeded {
            actual: header.logical_len,
            limit: max,
        });
    }
    let mut read = |at: u64, buf: &mut [u8]| -> Result<()> {
        let from = usize::try_from(at).map_err(|_| malformed("chunked record extent"))?;
        let bytes = from
            .checked_add(buf.len())
            .and_then(|end| payload.get(from..end))
            .ok_or(malformed("chunked record extent past its payload"))?;
        buf.copy_from_slice(bytes);
        Ok(())
    };
    for index in 0..header.count {
        let (start, end, crc) = chunk_extent(&mut read, &header, payload.len() as u64, index)?;
        // The extent was checked to lie inside the payload.
        if crc32fast::hash(&payload[start as usize..end as usize]) != crc {
            return Err(Error::ChecksumMismatch { offset: start });
        }
    }
    let data_at = header.data_at() as usize;
    Ok(Some(
        &payload[data_at..data_at + header.logical_len as usize],
    ))
}

/// The whole logical payload of an in-memory chunked payload, every chunk
/// verified. `max` bounds the declared logical length before anything is
/// allocated for it.
pub(crate) fn decode_chunked(payload: &[u8], max: u64) -> Result<Vec<u8>> {
    let header = ChunkHeader::decode(payload)?;
    if header.logical_len > max {
        return Err(Error::DecompressedLengthLimitExceeded {
            actual: header.logical_len,
            limit: max,
        });
    }
    let logical = usize::try_from(header.logical_len).map_err(|_| Error::LengthOverflow {
        value: header.logical_len,
    })?;
    let mut out = Vec::new();
    out.try_reserve_exact(logical)
        .map_err(|_| Error::AllocationFailed {
            resource: "chunked record logical payload",
            requested: header.logical_len,
        })?;
    out.resize(logical, 0);
    let mut read = |at: u64, buf: &mut [u8]| -> Result<()> {
        let from = at as usize;
        let bytes = payload
            .get(from..from + buf.len())
            .ok_or(malformed("chunked record extent past its payload"))?;
        buf.copy_from_slice(bytes);
        Ok(())
    };
    let mut scratch = Vec::new();
    let mut decompressor = None;
    for index in 0..header.count {
        let from = (index * CHUNK_LEN) as usize;
        let len = header.chunk_len(index) as usize;
        read_chunk(
            &mut read,
            &header,
            payload.len() as u64,
            0,
            index,
            &mut out[from..from + len],
            &mut scratch,
            &mut decompressor,
        )?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    /// A payload laid out in place in front of its encoded bytes is byte for
    /// byte the one `encode_chunked` builds, whatever room was set aside and
    /// whatever was left in that room by an earlier record.
    #[test]
    fn plain_chunks_laid_in_place_match_the_copied_encoding() {
        let room = plain_prefix_room(16 * CHUNK_LEN).unwrap();
        for len in [
            CHUNK_LEN as usize + 1,
            2 * CHUNK_LEN as usize,
            3 * CHUNK_LEN as usize + 77,
            16 * CHUNK_LEN as usize,
        ] {
            let logical = sample(len);
            let mut buffer = vec![0xA5; room];
            buffer.extend_from_slice(&logical);
            let start = lay_plain_chunks(&mut buffer, room).unwrap();
            assert_eq!(
                &buffer[start..],
                &encode_chunked(&logical, None).unwrap()[..],
                "len {len}"
            );
            assert_eq!(&buffer[room..], &logical[..], "the payload did not move");
        }
        // A table larger than the room is refused, never written over the data.
        let mut buffer = vec![0; 8];
        buffer.extend_from_slice(&sample(2 * CHUNK_LEN as usize));
        assert!(lay_plain_chunks(&mut buffer, 8).is_err());
        assert_eq!(plain_prefix_room(u64::MAX), None);
    }

    #[test]
    // One compression setting without zstd, two with it.
    #[allow(clippy::single_element_loop)]
    fn chunked_payloads_round_trip_at_every_boundary() {
        for len in [
            CHUNK_LEN as usize + 1,
            2 * CHUNK_LEN as usize,
            3 * CHUNK_LEN as usize + 77,
        ] {
            let logical = sample(len);
            for compression in [
                None,
                #[cfg(feature = "compression-zstd")]
                Some((CompressionAlgorithm::Zstd, CompressionLevel::Default)),
            ] {
                let payload = encode_chunked(&logical, compression).unwrap();
                assert_eq!(decode_chunked(&payload, u64::MAX).unwrap(), logical);
            }
        }
    }

    #[test]
    fn a_damaged_chunk_is_found_and_only_that_chunk() {
        let logical = sample(3 * CHUNK_LEN as usize);
        let mut payload = encode_chunked(&logical, None).unwrap();
        let header = ChunkHeader::decode(&payload).unwrap();
        let at = header.data_at() as usize + CHUNK_LEN as usize + 10;
        payload[at] ^= 1;
        assert!(matches!(
            decode_chunked(&payload, u64::MAX),
            Err(Error::ChecksumMismatch { .. })
        ));
        let mut read = |at: u64, buf: &mut [u8]| -> Result<()> {
            buf.copy_from_slice(&payload[at as usize..at as usize + buf.len()]);
            Ok(())
        };
        let mut out = vec![0u8; CHUNK_LEN as usize];
        let mut scratch = Vec::new();
        let mut decompressor = None;
        let len = payload.len() as u64;
        read_chunk(
            &mut read,
            &header,
            len,
            0,
            0,
            &mut out,
            &mut scratch,
            &mut decompressor,
        )
        .unwrap();
        assert_eq!(out, logical[..CHUNK_LEN as usize]);
        assert!(
            read_chunk(
                &mut read,
                &header,
                len,
                0,
                1,
                &mut out,
                &mut scratch,
                &mut decompressor
            )
            .is_err()
        );
        read_chunk(
            &mut read,
            &header,
            len,
            0,
            2,
            &mut out,
            &mut scratch,
            &mut decompressor,
        )
        .unwrap();
    }

    #[test]
    fn a_damaged_header_or_table_is_refused() {
        let logical = sample(2 * CHUNK_LEN as usize + 5);
        let payload = encode_chunked(&logical, None).unwrap();
        for at in [0usize, 9, 17, 20, 24, 30, 40, 52] {
            let mut damaged = payload.clone();
            damaged[at] ^= 0x40;
            assert!(decode_chunked(&damaged, u64::MAX).is_err(), "byte {at}");
        }
        assert!(matches!(
            decode_chunked(&payload, CHUNK_LEN),
            Err(Error::DecompressedLengthLimitExceeded { .. })
        ));
    }
}

#[cfg(kani)]
#[path = "../verification/record_chunks.rs"]
mod resource_proofs;
