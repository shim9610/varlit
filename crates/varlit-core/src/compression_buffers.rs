//! Fallible Rust output buffers for the compression backend.
use crate::{CompressionLevel, Error, Result};

#[cfg(any(feature = "compression-zstd", kani))]
#[path = "compression_steps.rs"]
mod resource_steps;

#[cfg(feature = "compression-zstd")]
fn context_allocation_error() -> std::io::Error {
    std::io::ErrorKind::OutOfMemory.into()
}

#[cfg(feature = "compression-zstd")]
fn backend_error(code: zstd::zstd_safe::ErrorCode) -> std::io::Error {
    // zstd returns negated ZSTD_ErrorCode values in size_t. Keep OOM itself
    // allocation-free; the ordinary diagnostic uses the backend's static name.
    let oom = (zstd::zstd_safe::zstd_sys::ZSTD_ErrorCode::ZSTD_error_memory_allocation as usize)
        .wrapping_neg();
    if code == oom {
        context_allocation_error()
    } else {
        std::io::Error::other(zstd::zstd_safe::get_error_name(code))
    }
}

#[cfg(feature = "compression-zstd")]
fn new_decoder() -> Result<zstd::zstd_safe::DCtx<'static>> {
    let mut context = zstd::zstd_safe::DCtx::try_create().ok_or_else(context_allocation_error)?;
    context.load_dictionary(&[]).map_err(backend_error)?;
    Ok(context)
}

pub(crate) struct Compressor {
    #[cfg(feature = "compression-zstd")]
    context: zstd::zstd_safe::CCtx<'static>,
    #[cfg(feature = "compression-zstd")]
    buffer: Vec<u8>,
}

impl Compressor {
    #[cfg(feature = "compression-zstd")]
    pub(crate) fn new(level: CompressionLevel) -> Result<Self> {
        let mut context =
            zstd::zstd_safe::CCtx::try_create().ok_or_else(context_allocation_error)?;
        context
            .set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(
                level.to_zstd_level(),
            ))
            .map_err(backend_error)?;
        context.load_dictionary(&[]).map_err(backend_error)?;
        Ok(Self {
            context,
            buffer: Vec::new(),
        })
    }
    #[cfg(not(feature = "compression-zstd"))]
    pub(crate) fn new(_level: CompressionLevel) -> Result<Self> {
        Err(Error::CompressionFeatureDisabled)
    }
    #[cfg(feature = "compression-zstd")]
    pub(crate) fn compress(&mut self, payload: &[u8]) -> Result<&[u8]> {
        let capacity = zstd::zstd_safe::compress_bound(payload.len());
        self.buffer.clear();
        self.buffer
            .try_reserve_exact(capacity)
            .map_err(|_| Error::AllocationFailed {
                resource: "compressed payload",
                requested: capacity as u64,
            })?;
        self.context
            .compress2(&mut self.buffer, payload)
            .map_err(backend_error)?;
        Ok(&self.buffer)
    }
    #[cfg(not(feature = "compression-zstd"))]
    pub(crate) fn compress(&mut self, _payload: &[u8]) -> Result<&[u8]> {
        Err(Error::CompressionFeatureDisabled)
    }
    #[cfg(feature = "compression-zstd")]
    pub(crate) fn into_buffer(self) -> Vec<u8> {
        self.buffer
    }
}

#[cfg(feature = "compression-zstd")]
pub(crate) fn decompress(payload: &[u8], maximum: usize) -> Result<Vec<u8>> {
    // Neither an outer limit nor a frame content-size field can authorize an
    // allocation larger than the complete blocks can possibly regenerate.
    let (declared, bound) = frame_sizes(payload)?;
    let capacity = resource_steps::output_capacity(declared, bound, maximum).ok_or(
        Error::DecompressedLengthMismatch {
            expected: maximum as u64,
            actual: declared.unwrap_or(bound),
        },
    )?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource: "decompressed payload",
            requested: capacity as u64,
        })?;
    new_decoder()?
        .decompress(&mut output, payload)
        .map_err(backend_error)?;
    Ok(output)
}

/// Bound output from complete zstd blocks, independently of the content-size
/// field. Raw/RLE blocks declare their actual length; compressed blocks can
/// regenerate at most 128 KiB (RFC 8878 section 3.1.1.2). This is an upper bound,
/// not proof of valid compressed contents: the backend still decodes/checks them.
#[cfg(feature = "compression-zstd")]
fn frame_sizes(mut bytes: &[u8]) -> Result<(Option<u64>, u64)> {
    let mut total = 0u64;
    let mut bound = 0u64;
    let mut known = true;
    while !bytes.is_empty() {
        let magic = bytes.get(..4).ok_or(Error::InvalidChunkedBytes)?;
        let magic = u32::from_le_bytes(magic.try_into().expect("four bytes"));
        if magic & 0xfffffff0 == 0x184d2a50 {
            let size = bytes.get(4..8).ok_or(Error::InvalidChunkedBytes)?;
            let size = u32::from_le_bytes(size.try_into().expect("four bytes")) as usize;
            let end = 8usize.checked_add(size).ok_or(Error::InvalidChunkedBytes)?;
            bytes = bytes.get(end..).ok_or(Error::InvalidChunkedBytes)?;
            continue;
        }
        let declared = zstd::zstd_safe::get_frame_content_size(bytes)
            .map_err(|_| Error::InvalidChunkedBytes)?;
        if magic == 0x1eb52ffd || (0xfd2fb522..=0xfd2fb527).contains(&magic) {
            // Legacy layouts differ. The backend's legacy bound is derived
            // from complete blocks (not its optional frame content-size hint).
            let len = zstd::zstd_safe::find_frame_compressed_size(bytes).map_err(backend_error)?;
            let frame = bytes
                .get(..len)
                .filter(|_| len != 0)
                .ok_or(Error::InvalidChunkedBytes)?;
            let frame_bound = zstd::zstd_safe::decompress_bound(frame).map_err(backend_error)?;
            if let Some(size) = declared {
                if size > frame_bound {
                    return Err(Error::InvalidChunkedBytes);
                }
                total = total.checked_add(size).ok_or(Error::InvalidChunkedBytes)?;
            } else {
                known = false;
            }
            bound = bound
                .checked_add(frame_bound)
                .ok_or(Error::InvalidChunkedBytes)?;
            bytes = &bytes[len..];
            continue;
        }
        let descriptor = *bytes.get(4).ok_or(Error::InvalidChunkedBytes)?;
        let single = descriptor & 0x20 != 0;
        let content_bytes = match descriptor >> 6 {
            0 => usize::from(single),
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let dictionary_bytes = [0, 1, 2, 4][(descriptor & 3) as usize];
        let mut offset = 5 + usize::from(!single) + dictionary_bytes + content_bytes;
        let mut frame_bound = 0u64;
        loop {
            let end = offset.checked_add(3).ok_or(Error::InvalidChunkedBytes)?;
            let header = bytes.get(offset..end).ok_or(Error::InvalidChunkedBytes)?;
            let header = u32::from_le_bytes([header[0], header[1], header[2], 0]);
            let (next, regenerated) = resource_steps::block_extent(offset, header, bytes.len())
                .ok_or(Error::InvalidChunkedBytes)?;
            offset = next;
            frame_bound = frame_bound
                .checked_add(regenerated)
                .ok_or(Error::InvalidChunkedBytes)?;
            if header & 1 != 0 {
                break;
            }
        }
        if descriptor & 4 != 0 {
            offset = offset.checked_add(4).ok_or(Error::InvalidChunkedBytes)?;
        }
        bytes = bytes.get(offset..).ok_or(Error::InvalidChunkedBytes)?;
        if let Some(size) = declared {
            if size > frame_bound {
                return Err(Error::InvalidChunkedBytes);
            }
            total = total.checked_add(size).ok_or(Error::InvalidChunkedBytes)?;
        } else {
            known = false;
        }
        bound = bound
            .checked_add(frame_bound)
            .ok_or(Error::InvalidChunkedBytes)?;
    }
    Ok((known.then_some(total), bound))
}

/// Screen both the container length and frame hints against complete blocks
/// before reserving the container-wide output. Unknown-size frames are supported.
pub(crate) fn validate_frame_output_len(payload: &[u8], expected: u64) -> Result<bool> {
    #[cfg(feature = "compression-zstd")]
    {
        let (declared, bound) = frame_sizes(payload)?;
        if let Some(actual) = declared
            && actual != expected
        {
            return Err(Error::DecompressedLengthMismatch { expected, actual });
        }
        if expected > bound {
            return Err(Error::InvalidChunkedBytes);
        }
        Ok(declared.is_some())
    }
    #[cfg(not(feature = "compression-zstd"))]
    {
        let _ = (payload, expected);
        Err(Error::CompressionFeatureDisabled)
    }
}

pub(crate) struct Decompressor {
    #[cfg(feature = "compression-zstd")]
    context: zstd::zstd_safe::DCtx<'static>,
    #[cfg(feature = "compression-zstd")]
    buffer: Vec<u8>,
}
impl Decompressor {
    #[cfg(feature = "compression-zstd")]
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            context: new_decoder()?,
            buffer: Vec::new(),
        })
    }
    #[cfg(not(feature = "compression-zstd"))]
    pub(crate) fn new() -> Result<Self> {
        Err(Error::CompressionFeatureDisabled)
    }
    #[cfg(feature = "compression-zstd")]
    pub(crate) fn decompress_chunk(
        &mut self,
        payload: &[u8],
        expected: usize,
        prevalidated_size: bool,
    ) -> Result<&[u8]> {
        let exact = if prevalidated_size {
            Some(expected as u64)
        } else {
            frame_sizes(payload)?.0
        };
        if let Some(actual) = exact
            && actual != expected as u64
        {
            return Err(Error::DecompressedLengthMismatch {
                expected: expected as u64,
                actual,
            });
        }
        self.buffer.clear();
        self.buffer
            .try_reserve_exact(expected)
            .map_err(|_| Error::AllocationFailed {
                resource: "decompressed chunk",
                requested: expected as u64,
            })?;
        let len = if exact.is_some() {
            self.context
                .decompress(&mut self.buffer, payload)
                .map_err(backend_error)?
        } else {
            self.buffer.resize(expected, 0);
            self.context
                .decompress(self.buffer.as_mut_slice(), payload)
                .map_err(backend_error)?
        };
        if len != expected {
            return Err(Error::DecompressedLengthMismatch {
                expected: expected as u64,
                actual: len as u64,
            });
        }
        Ok(&self.buffer)
    }
    #[cfg(not(feature = "compression-zstd"))]
    pub(crate) fn decompress_chunk(
        &mut self,
        _payload: &[u8],
        _expected: usize,
        _prevalidated_size: bool,
    ) -> Result<&[u8]> {
        Err(Error::CompressionFeatureDisabled)
    }
    #[cfg(feature = "compression-zstd")]
    pub(crate) fn decompress_append(
        &mut self,
        payload: &[u8],
        output: &mut Vec<u8>,
        expected: usize,
        prevalidated_size: bool,
    ) -> Result<usize> {
        let start = output.len();
        let end = start
            .checked_add(expected)
            .filter(|end| *end <= output.capacity())
            .ok_or(Error::InvalidChunkedBytes)?;
        let written = match if prevalidated_size {
            Some(expected as u64)
        } else {
            frame_sizes(payload)?.0
        } {
            Some(actual) if actual != expected as u64 => Err(Error::DecompressedLengthMismatch {
                expected: expected as u64,
                actual,
            }),
            Some(_) => {
                // The frame headers screen the exact output length. The
                // backend's safe Cursor WriteBuf appends directly to reserved
                // capacity and marks only initialized bytes as readable.
                let mut tail = std::io::Cursor::new(output);
                tail.set_position(start as u64);
                Ok(self
                    .context
                    .decompress(&mut tail, payload)
                    .map_err(backend_error)?)
            }
            None => {
                // Frames without an exact size retain a strict chunk-sized
                // destination, even when the final Vec has spare capacity.
                output.resize(end, 0);
                Ok(self
                    .context
                    .decompress(&mut output[start..end], payload)
                    .map_err(backend_error)?)
            }
        }?;
        if written != expected {
            return Err(Error::DecompressedLengthMismatch {
                expected: expected as u64,
                actual: written as u64,
            });
        }
        Ok(written)
    }
    #[cfg(not(feature = "compression-zstd"))]
    pub(crate) fn decompress_append(
        &mut self,
        _payload: &[u8],
        _output: &mut Vec<u8>,
        _expected: usize,
        _prevalidated_size: bool,
    ) -> Result<usize> {
        Err(Error::CompressionFeatureDisabled)
    }
}

#[cfg(all(test, feature = "compression-zstd"))]
mod tests {
    use super::*;

    #[test]
    fn block_bounds_cover_encoder_frames_and_reject_structural_amplification() -> Result<()> {
        for length in [0, 1, 255, 256, 1024, 131071, 131072, 131073, 1048576] {
            let mut state = 1u64;
            for pattern in 0..3 {
                let data: Vec<u8> = (0..length)
                    .map(|i| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        match pattern {
                            0 => 7,
                            1 => (i % 251) as u8,
                            _ => state as u8,
                        }
                    })
                    .collect();
                for known in [false, true] {
                    for checksum in [false, true] {
                        let mut encoder = zstd::bulk::Compressor::new(1)?;
                        encoder.include_contentsize(known)?;
                        encoder.include_checksum(checksum)?;
                        let encoded = encoder.compress(&data)?;
                        let (declared, bound) = frame_sizes(&encoded)?;
                        assert!(bound >= length as u64);
                        assert_eq!(declared, known.then_some(length as u64));
                        assert!(decompress(&encoded, length)? == data);
                        let mut joined = vec![0x50, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 1, 2, 3];
                        joined.extend_from_slice(&encoded);
                        joined.extend_from_slice(&encoded);
                        let (_, bound) = frame_sizes(&joined)?;
                        assert!(bound >= 2 * length as u64);
                        assert_eq!(decompress(&joined, 2 * length)?.len(), 2 * length);
                    }
                }
            }
        }
        // An RLE block may expand greatly, but never beyond the format's block
        // maximum. A block-length forgery is not permission to allocate either.
        let oversized_rle = [0x28, 0xb5, 0x2f, 0xfd, 0xa0, 0, 0, 0x40, 0, 3, 0, 0x80, 7];
        assert!(frame_sizes(&oversized_rle).is_err());
        for bytes in [
            &b"invalid"[..],
            &[0x50, 0x2a, 0x4d, 0x18, 0xff, 0xff, 0xff, 0xff],
        ] {
            assert!(frame_sizes(bytes).is_err());
        }
        Ok(())
    }

    #[test]
    fn append_preserves_prefix_and_bounds_known_unknown_and_concatenated_frames() -> Result<()> {
        let input = [23u8; 4096];
        let mut compressor = zstd::bulk::Compressor::new(1)?;
        let known = compressor.compress(&input)?;
        compressor.include_contentsize(false)?;
        let unknown = compressor.compress(&input)?;
        let mut concatenated = known.clone();
        concatenated.extend_from_slice(&known);
        let mut decoder = Decompressor::new()?;
        for (frame, expected) in [(&known, 4096), (&unknown, 4096), (&concatenated, 8192)] {
            let chunk = decoder.decompress_chunk(frame, expected, false)?;
            assert_eq!(chunk.len(), expected);
            assert!(
                chunk.iter().all(|byte| *byte == 23),
                "invalid scratch decoded bytes"
            );
            assert!(
                decoder
                    .decompress_chunk(frame, expected - 1, false)
                    .is_err()
            );
            let mut output = Vec::with_capacity(16384);
            output.extend_from_slice(b"prefix");
            let written = decoder.decompress_append(frame, &mut output, expected, false)?;
            assert_eq!(written, expected);
            assert!(output[..6] == *b"prefix", "prefix overwritten");
            assert!(
                output[6..].iter().all(|byte| *byte == 23),
                "invalid decoded bytes"
            );
            assert_eq!(output.len(), expected + 6);
            let mut output = Vec::with_capacity(16384);
            output.extend_from_slice(b"prefix");
            assert!(
                decoder
                    .decompress_append(frame, &mut output, expected - 1, false)
                    .is_err()
            );
            assert!(output[..6] == *b"prefix", "refusal overwrote prefix");
            assert!(output.len() < 6 + expected, "refusal exceeded chunk bound");
        }
        assert!(matches!(
            decompress(&known, 4095),
            Err(Error::DecompressedLengthMismatch { .. })
        ));
        assert!(decompress(&unknown, 4095).is_err());
        Ok(())
    }
}
