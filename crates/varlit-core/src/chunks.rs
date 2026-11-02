use crate::{CompressionLevel, Error, ReadLimits, Result, VarlitDecode, VarlitEncode, WireType};

const CHUNKED_MAGIC: &[u8; 4] = b"VCHK";
const CHUNKED_VERSION: u16 = 1;
const CHUNKED_HEADER_LEN: usize = 32;
const CHUNK_ENTRY_LEN: usize = 16;
const ALGORITHM_ZSTD: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkedBytes {
    encoded: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChunkEntry {
    uncompressed_len: u32,
    stored_len: u32,
    crc32: u32,
}

impl ChunkedBytes {
    pub fn from_zstd_chunks(
        payload: &[u8],
        chunk_len: usize,
        level: CompressionLevel,
    ) -> Result<Self> {
        if chunk_len == 0 || chunk_len > u32::MAX as usize {
            return Err(Error::InvalidChunkedBytes);
        }
        let count = payload.len().div_ceil(chunk_len);
        let chunk_count = u32::try_from(count).map_err(|_| Error::InvalidChunkedBytes)?;
        let prefix_len = count
            .checked_mul(CHUNK_ENTRY_LEN)
            .and_then(|n| n.checked_add(CHUNKED_HEADER_LEN))
            .ok_or(Error::InvalidChunkedBytes)?;
        let mut encoded = Vec::new();
        encoded
            .try_reserve_exact(prefix_len)
            .map_err(|_| Error::AllocationFailed {
                resource: "chunked bytes encoding",
                requested: prefix_len as u64,
            })?;
        encoded.resize(prefix_len, 0);
        encoded[..4].copy_from_slice(CHUNKED_MAGIC);
        encoded[4..6].copy_from_slice(&CHUNKED_VERSION.to_le_bytes());
        encoded[6] = ALGORITHM_ZSTD;
        encoded[8..16].copy_from_slice(&(chunk_len as u64).to_le_bytes());
        encoded[16..24].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        encoded[24..28].copy_from_slice(&chunk_count.to_le_bytes());
        // One reusable compression buffer; never retain one capacity per chunk.
        if !payload.is_empty() {
            let mut compressor = crate::compression_buffers::Compressor::new(level)?;
            for (index, chunk) in payload.chunks(chunk_len).enumerate() {
                let compressed = compressor.compress(chunk)?;
                let stored_len =
                    u32::try_from(compressed.len()).map_err(|_| Error::InvalidChunkedBytes)?;
                let at = CHUNKED_HEADER_LEN + index * CHUNK_ENTRY_LEN;
                encoded[at..at + 4].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
                encoded[at + 4..at + 8].copy_from_slice(&stored_len.to_le_bytes());
                encoded[at + 8..at + 12].copy_from_slice(&crc32_bytes(chunk)?.to_le_bytes());
                encoded
                    .try_reserve(compressed.len())
                    .map_err(|_| Error::AllocationFailed {
                        resource: "chunked bytes encoding",
                        requested: (encoded.len() as u64).saturating_add(compressed.len() as u64),
                    })?;
                encoded.extend_from_slice(compressed);
            }
        }
        Self::from_encoded(encoded)
    }

    pub fn from_encoded(encoded: Vec<u8>) -> Result<Self> {
        validate_chunked_bytes(&encoded, false)?;
        Ok(Self { encoded })
    }

    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }

    pub fn into_encoded(self) -> Vec<u8> {
        self.encoded
    }

    /// Total decoded length declared by the structurally validated container.
    /// This does not decompress or authenticate its contents.
    pub fn decoded_len(&self) -> u64 {
        u64::from_le_bytes(self.encoded[16..24].try_into().expect("validated header"))
    }

    /// Stream a decoded byte range through caller-owned scratch.
    ///
    /// Unlike [`Self::decode_to_vec_limited`], this never allocates the complete
    /// output or a decoded chunk. `buffer` is reused without resizing; even one
    /// byte suffices. The callback receives the absolute decoded offset and a
    /// nonempty slice of this buffer. Bytes outside `range` are not delivered.
    ///
    /// Only overlapping chunks are decompressed, but each is decoded to its end
    /// to check its length and checksum. Other chunks' checksums are not checked.
    /// **Delivery is provisional until this method returns `Ok(())`:** a late
    /// checksum/decoder error can follow callbacks. Stage output before publishing
    /// it; do not use this callback for irreversible effects. Callback errors stop
    /// immediately and are returned unchanged. Retrying starts the range again.
    ///
    /// `max_window_bytes` bounds zstd's history window, rounded down to a power
    /// of two (minimum 1024). It is not a total allocator/RSS bound: zstd also
    /// allocates a context and bounded block buffers. Larger-window frames are
    /// refused by zstd. This explicit policy is independent of the whole-output
    /// limit used by `decode_to_vec_limited`, allowing a caller to choose this
    /// path after that operation refuses materialization. The encoded container
    /// remains resident in `self`; callback-owned allocations are caller policy.
    ///
    /// An empty in-bounds range needs neither scratch nor a decoder. A nonempty
    /// range with empty scratch returns [`Error::BufferTooSmall`] before delivery.
    ///
    /// ```no_run
    /// # fn example(chunks: &varlit_core::ChunkedBytes) -> varlit_core::Result<()> {
    /// let range = 1024..4096; // decoded byte offsets, exclusive end
    /// let mut scratch = [0; 8192];
    /// let mut staged = std::fs::File::create("selected-range.pending")?;
    /// chunks.stream_range(range, &mut scratch, 8 * 1024 * 1024, |_, bytes| {
    ///     std::io::Write::write_all(&mut staged, bytes)?;
    ///     Ok(())
    /// })?;
    /// // Only now has the selected data passed all overlapping chunk checks.
    /// # Ok(()) }
    /// ```
    pub fn stream_range(
        &self,
        range: std::ops::Range<u64>,
        buffer: &mut [u8],
        max_window_bytes: usize,
        mut visit: impl FnMut(u64, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let header = parse_header(&self.encoded)?;
        if range.start > range.end || range.end > header.uncompressed_len {
            return Err(Error::InvalidChunkedRange {
                start: range.start,
                end: range.end,
                len: header.uncompressed_len,
            });
        }
        if range.is_empty() {
            return Ok(());
        }
        if buffer.is_empty() {
            return Err(Error::BufferTooSmall {
                resource: "chunk streaming",
                required: 1,
                provided: 0,
            });
        }
        #[cfg(not(feature = "integrity"))]
        {
            let _ = (max_window_bytes, &mut visit);
            Err(Error::IntegrityFeatureDisabled)
        }
        #[cfg(feature = "integrity")]
        {
            let mut decoder =
                crate::compression_buffers::StreamDecompressor::new(max_window_bytes)?;
            let mut stored_at = table_extent(header.chunk_count as usize, self.encoded.len())
                .ok_or(Error::InvalidChunkedBytes)?;
            let mut decoded_at = 0u64;
            for index in 0..header.chunk_count as usize {
                let entry =
                    parse_entry(&self.encoded, CHUNKED_HEADER_LEN + index * CHUNK_ENTRY_LEN)?;
                let stored_end = stored_at
                    .checked_add(entry.stored_len as usize)
                    .ok_or(Error::InvalidChunkedBytes)?;
                let decoded_end = decoded_at
                    .checked_add(u64::from(entry.uncompressed_len))
                    .ok_or(Error::InvalidChunkedBytes)?;
                if decoded_end > range.start {
                    let stored = self
                        .encoded
                        .get(stored_at..stored_end)
                        .ok_or(Error::InvalidChunkedBytes)?;
                    crate::compression_buffers::validate_frame_output_len(
                        stored,
                        u64::from(entry.uncompressed_len),
                    )?;
                    let mut crc = crc32fast::Hasher::new();
                    decoder.decode(
                        stored,
                        buffer,
                        u64::from(entry.uncompressed_len),
                        |offset, bytes| {
                            crc.update(bytes);
                            let start = decoded_at + offset;
                            let end = start + bytes.len() as u64;
                            let first = start.max(range.start);
                            let last = end.min(range.end);
                            if first < last {
                                visit(
                                    first,
                                    &bytes[(first - start) as usize..(last - start) as usize],
                                )?;
                            }
                            Ok(())
                        },
                    )?;
                    let actual = crc.finalize();
                    if actual != entry.crc32 {
                        return Err(Error::ChunkChecksumMismatch {
                            chunk_index: index as u32,
                            expected: entry.crc32,
                            actual,
                        });
                    }
                }
                if decoded_end >= range.end {
                    break;
                }
                stored_at = stored_end;
                decoded_at = decoded_end;
            }
            Ok(())
        }
    }

    pub fn decode_to_vec(&self) -> Result<Vec<u8>> {
        let limit = ReadLimits::STANDARD
            .max_logical_payload_len
            .require_finite("logical payload bytes")?;
        self.decode_to_vec_limited(limit)
    }

    pub fn decode_to_vec_limited(&self, max_uncompressed_len: u64) -> Result<Vec<u8>> {
        let header = parse_header(&self.encoded)?;
        if header.uncompressed_len > max_uncompressed_len {
            return Err(Error::DecompressedLengthLimitExceeded {
                actual: header.uncompressed_len,
                limit: max_uncompressed_len,
            });
        }
        // Every chunk's declaration must fit the output bound derived from its
        // complete blocks; untrusted frame/container lengths alone cannot
        // authorize the full output reservation below.
        let frame_sizes_known = validate_chunked_bytes(&self.encoded, true)?;
        let output_len =
            usize::try_from(header.uncompressed_len).map_err(|_| Error::InvalidChunkedBytes)?;
        let entries_len = (header.chunk_count as usize)
            .checked_mul(CHUNK_ENTRY_LEN)
            .ok_or(Error::InvalidChunkedBytes)?;
        let mut payload_offset = CHUNKED_HEADER_LEN
            .checked_add(entries_len)
            .ok_or(Error::InvalidChunkedBytes)?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(output_len)
            .map_err(|_| Error::AllocationFailed {
                resource: "chunked bytes output",
                requested: header.uncompressed_len,
            })?;
        with_output_pages(&mut output, |output| {
            let mut decompressor = (header.chunk_count != 0)
                .then(crate::compression_buffers::Decompressor::new)
                .transpose()?;
            for index in 0..header.chunk_count as usize {
                let entry_offset = CHUNKED_HEADER_LEN + index * CHUNK_ENTRY_LEN;
                let entry = parse_entry(&self.encoded, entry_offset)?;
                let stored_len = entry.stored_len as usize;
                let end = payload_offset
                    .checked_add(stored_len)
                    .ok_or(Error::InvalidChunkedBytes)?;
                let stored = self
                    .encoded
                    .get(payload_offset..end)
                    .ok_or(Error::InvalidChunkedBytes)?;
                payload_offset = end;
                let start = output.len();
                let end = start
                    .checked_add(entry.uncompressed_len as usize)
                    .filter(|end| *end <= output_len)
                    .ok_or(Error::InvalidChunkedBytes)?;
                let decoder = decompressor.as_mut().ok_or(Error::InvalidChunkedBytes)?;
                // Multiple chunks reuse one chunk-sized scratch buffer: zstd
                // decodes into hot memory, then a bulk copy fills the final Vec.
                // A single chunk writes directly, avoiding a second full output.
                let (len, actual) = if header.chunk_count == 1 {
                    let len = decoder.decompress_append(
                        stored,
                        output,
                        entry.uncompressed_len as usize,
                        frame_sizes_known,
                    )?;
                    (len, crc32_bytes(&output[start..end])?)
                } else {
                    let chunk = decoder.decompress_chunk(
                        stored,
                        entry.uncompressed_len as usize,
                        frame_sizes_known,
                    )?;
                    let actual = crc32_bytes(chunk)?;
                    output.extend_from_slice(chunk);
                    (chunk.len(), actual)
                };
                if len != entry.uncompressed_len as usize {
                    return Err(Error::DecompressedLengthMismatch {
                        expected: entry.uncompressed_len as u64,
                        actual: len as u64,
                    });
                }
                if actual != entry.crc32 {
                    return Err(Error::ChunkChecksumMismatch {
                        chunk_index: index as u32,
                        expected: entry.crc32,
                        actual,
                    });
                }
            }
            if output.len() as u64 != header.uncompressed_len {
                return Err(Error::DecompressedLengthMismatch {
                    expected: header.uncompressed_len,
                    actual: output.len() as u64,
                });
            }
            Ok(())
        })?;
        Ok(output)
    }
}

// Scope page advice to filling an already reserved output. This does not
// reserve extra memory, keep a cache, alter the allocator, or change limits.
fn with_output_pages<T>(
    output: &mut Vec<u8>,
    fill: impl FnOnce(&mut Vec<u8>) -> Result<T>,
) -> Result<T> {
    let pages = OutputPages::new(output);
    fill(pages.output)
}
struct OutputPages<'a> {
    output: &'a mut Vec<u8>,
    #[cfg(all(target_os = "linux", target_arch = "x86_64", not(miri)))]
    advised: Option<(usize, usize)>,
}
impl<'a> OutputPages<'a> {
    fn new(output: &'a mut Vec<u8>) -> Self {
        #[cfg(all(target_os = "linux", target_arch = "x86_64", not(miri)))]
        let advised = {
            // Only complete 2 MiB pages wholly inside this allocation qualify.
            // Small outputs retain their ordinary path. Linux may decline the
            // hint; all decoding and allocation error semantics stay identical.
            const PAGE: usize = 2 * 1024 * 1024;
            let base = output.as_mut_ptr().addr();
            let range = base
                .checked_add(PAGE - 1)
                .zip(base.checked_add(output.capacity()))
                .map(|(start, end)| (start & !(PAGE - 1), end & !(PAGE - 1)))
                .filter(|(start, end)| output.capacity() >= 4 * PAGE && end > start);
            range.and_then(|(start, end)| {
                // SAFETY: the page-aligned interval lies entirely in the live,
                // exclusively borrowed Vec allocation. madvise changes kernel
                // paging policy; it neither reads nor initializes Rust bytes.
                let result = unsafe {
                    libc::madvise(
                        output.as_mut_ptr().with_addr(start).cast(),
                        end - start,
                        libc::MADV_HUGEPAGE,
                    )
                };
                (result == 0).then_some((start, end))
            })
        };
        Self {
            output,
            #[cfg(all(target_os = "linux", target_arch = "x86_64", not(miri)))]
            advised,
        }
    }
}
impl Drop for OutputPages<'_> {
    fn drop(&mut self) {
        #[cfg(all(target_os = "linux", target_arch = "x86_64", not(miri)))]
        if let Some((start, end)) = self.advised {
            let base = self.output.as_mut_ptr().addr();
            // The decoder reserves once and never grows its allocation. Keep
            // the hint reset bounded even if a future caller changes that rule.
            if base <= start
                && base
                    .checked_add(self.output.capacity())
                    .is_some_and(|limit| end <= limit)
            {
                // SAFETY: as above, the interval is within the still-live Vec.
                // Reset on success, decoding errors and panic before the Vec
                // can be dropped or returned for unrelated reuse.
                unsafe {
                    libc::madvise(
                        self.output.as_mut_ptr().with_addr(start).cast(),
                        end - start,
                        libc::MADV_NOHUGEPAGE,
                    );
                }
            }
        }
    }
}

/// Structural codec identity of [`ChunkedBytes`] (API-03).
///
/// Derived exactly the way the built-in container codecs in
/// [`crate::codec`] derive theirs: a tag naming this codec, folded with the
/// identity of the codec whose bytes it actually emits (`Vec<u8>`), and with
/// [`CHUNKED_VERSION`] as the arity component so a container-format version
/// bump changes the identity rather than silently reusing it. The value is a
/// const expression over compile-time constants only, so it is stable across
/// builds, distinct from `Vec<u8>`'s own `bytes` identity, and non-zero — which
/// is what `#[derive(VarlitBlock)]` requires of every field codec, and therefore
/// what makes a documented `ChunkedBytes` variable field compile.
const CHUNKED_BYTES_SCHEMA_ID: u64 = crate::codec::container_schema_id_with_arity(
    b"chunked_bytes",
    CHUNKED_VERSION as u64,
    &[<Vec<u8> as VarlitEncode>::SCHEMA_ID],
);

impl VarlitEncode for ChunkedBytes {
    const WIRE_TYPE: WireType = WireType::Bytes;
    const SCHEMA_ID: u64 = CHUNKED_BYTES_SCHEMA_ID;

    fn encode_varlit(&self, encoder: &mut crate::Encoder) -> Result<()> {
        self.encoded.encode_varlit(encoder)
    }
}

impl VarlitDecode for ChunkedBytes {
    const WIRE_TYPE: WireType = WireType::Bytes;
    // Symmetric codec: `decode_varlit` accepts exactly what `encode_varlit`
    // emits, so it declares the same identity, as the built-in codecs do.
    const SCHEMA_ID: u64 = CHUNKED_BYTES_SCHEMA_ID;

    fn decode_varlit(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
        Self::from_encoded(Vec::<u8>::decode_varlit(decoder)?)
    }
}

#[derive(Clone, Copy)]
struct ChunkedHeader {
    chunk_len: u32,
    uncompressed_len: u64,
    chunk_count: u32,
}

// Establish the real-input iteration bound before considering declared output.
fn table_extent(count: usize, input_len: usize) -> Option<usize> {
    let end = count
        .checked_mul(CHUNK_ENTRY_LEN)?
        .checked_add(CHUNKED_HEADER_LEN)?;
    (end <= input_len).then_some(end)
}

fn validate_chunked_bytes(encoded: &[u8], check_frames: bool) -> Result<bool> {
    let header = parse_header(encoded)?;
    let entries_end = table_extent(header.chunk_count as usize, encoded.len())
        .ok_or(Error::InvalidChunkedBytes)?;
    let expected_chunk_count = if header.uncompressed_len == 0 {
        0
    } else {
        ((header.uncompressed_len - 1) / u64::from(header.chunk_len)) + 1
    };
    if u64::from(header.chunk_count) != expected_chunk_count {
        return Err(Error::InvalidChunkedBytes);
    }
    let mut all_sizes_known = check_frames;
    let mut payload_len = 0usize;
    let mut uncompressed_len = 0u64;
    for (index, offset) in (CHUNKED_HEADER_LEN..entries_end)
        .step_by(CHUNK_ENTRY_LEN)
        .enumerate()
    {
        let entry = parse_entry(encoded, offset)?;
        let expected_uncompressed_len = if index + 1 == header.chunk_count as usize {
            header
                .uncompressed_len
                .checked_sub(u64::from(header.chunk_len) * index as u64)
                .ok_or(Error::InvalidChunkedBytes)?
        } else {
            u64::from(header.chunk_len)
        };
        if u64::from(entry.uncompressed_len) != expected_uncompressed_len || entry.stored_len == 0 {
            return Err(Error::InvalidChunkedBytes);
        }
        let prior = payload_len;
        payload_len = payload_len
            .checked_add(entry.stored_len as usize)
            .ok_or(Error::InvalidChunkedBytes)?;
        if check_frames {
            let start = entries_end
                .checked_add(prior)
                .ok_or(Error::InvalidChunkedBytes)?;
            let end = entries_end
                .checked_add(payload_len)
                .ok_or(Error::InvalidChunkedBytes)?;
            let stored = encoded.get(start..end).ok_or(Error::InvalidChunkedBytes)?;
            // Relocate the decoder's existing frame-size check before the
            // large allocation, then reuse that result in the decoding pass.
            all_sizes_known &= crate::compression_buffers::validate_frame_output_len(
                stored,
                u64::from(entry.uncompressed_len),
            )?;
        }
        uncompressed_len = uncompressed_len
            .checked_add(entry.uncompressed_len as u64)
            .ok_or(Error::InvalidChunkedBytes)?;
    }
    let expected_len = entries_end
        .checked_add(payload_len)
        .ok_or(Error::InvalidChunkedBytes)?;
    if expected_len != encoded.len() || uncompressed_len != header.uncompressed_len {
        return Err(Error::InvalidChunkedBytes);
    }
    Ok(all_sizes_known)
}

fn parse_header(encoded: &[u8]) -> Result<ChunkedHeader> {
    if encoded.len() < CHUNKED_HEADER_LEN || &encoded[0..4] != CHUNKED_MAGIC {
        return Err(Error::InvalidChunkedBytes);
    }
    let version = u16::from_le_bytes(encoded[4..6].try_into().expect("slice"));
    if version != CHUNKED_VERSION {
        return Err(Error::InvalidChunkedBytes);
    }
    let algorithm = encoded[6];
    if algorithm != ALGORITHM_ZSTD {
        return Err(Error::InvalidCompressionHeader);
    }
    let chunk_len = u64::from_le_bytes(encoded[8..16].try_into().expect("slice"));
    if encoded[7] != 0
        || encoded[28..32] != [0; 4]
        || chunk_len == 0
        || chunk_len > u64::from(u32::MAX)
    {
        return Err(Error::InvalidChunkedBytes);
    }
    Ok(ChunkedHeader {
        chunk_len: chunk_len as u32,
        uncompressed_len: u64::from_le_bytes(encoded[16..24].try_into().expect("slice")),
        chunk_count: u32::from_le_bytes(encoded[24..28].try_into().expect("slice")),
    })
}

fn parse_entry(encoded: &[u8], offset: usize) -> Result<ChunkEntry> {
    let end = offset
        .checked_add(CHUNK_ENTRY_LEN)
        .ok_or(Error::InvalidChunkedBytes)?;
    let entry = encoded.get(offset..end).ok_or(Error::InvalidChunkedBytes)?;
    if entry[12..16] != [0; 4] {
        return Err(Error::InvalidChunkedBytes);
    }
    Ok(ChunkEntry {
        uncompressed_len: u32::from_le_bytes(entry[0..4].try_into().expect("slice")),
        stored_len: u32::from_le_bytes(entry[4..8].try_into().expect("slice")),
        crc32: u32::from_le_bytes(entry[8..12].try_into().expect("slice")),
    })
}

#[cfg(feature = "integrity")]
fn crc32_bytes(bytes: &[u8]) -> Result<u32> {
    Ok(crc32fast::hash(bytes))
}

#[cfg(not(feature = "integrity"))]
fn crc32_bytes(_bytes: &[u8]) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

#[cfg(all(test, feature = "compression-zstd", feature = "integrity"))]
mod oom_tests {
    use super::*;
    #[test]
    fn streamed_chunk_encoding_preserves_the_existing_wire_bytes() -> Result<()> {
        for (length, chunk_len) in [(0usize, 1usize), (129, 1), (1025, 64), (65536, 65536)] {
            let input: Vec<_> = (0..length).map(|i| (i % 251) as u8).collect();
            let mut table = Vec::new();
            let mut payload = Vec::new();
            for chunk in input.chunks(chunk_len) {
                let stored = zstd::bulk::compress(chunk, CompressionLevel::Fast.to_zstd_level())?;
                table.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
                table.extend_from_slice(&(stored.len() as u32).to_le_bytes());
                table.extend_from_slice(&crc32fast::hash(chunk).to_le_bytes());
                table.extend_from_slice(&[0; 4]);
                payload.extend_from_slice(&stored);
            }
            let mut expected = b"VCHK".to_vec();
            expected.extend_from_slice(&[1, 0, 1, 0]);
            expected.extend_from_slice(&(chunk_len as u64).to_le_bytes());
            expected.extend_from_slice(&(length as u64).to_le_bytes());
            expected.extend_from_slice(&(length.div_ceil(chunk_len) as u32).to_le_bytes());
            expected.extend_from_slice(&[0; 4]);
            expected.extend_from_slice(&table);
            expected.extend_from_slice(&payload);
            let encoded =
                ChunkedBytes::from_zstd_chunks(&input, chunk_len, CompressionLevel::Fast)?;
            assert!(
                encoded.encoded() == expected,
                "chunk wire mismatch: length={length} chunk={chunk_len}"
            );
            assert!(
                encoded.decode_to_vec()? == input,
                "chunk roundtrip mismatch"
            );
        }
        Ok(())
    }
}

#[cfg(kani)]
#[path = "verification/chunks.rs"]
mod resource_proofs;

impl crate::TryClone for ChunkedBytes {
    fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            encoded: crate::TryClone::try_clone(&self.encoded)?,
        })
    }
}
