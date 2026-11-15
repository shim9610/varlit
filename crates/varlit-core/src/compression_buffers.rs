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

/// Streaming history belongs to zstd, never to a decoded-chunk Vec.
#[cfg(feature = "integrity")]
pub(crate) struct StreamDecompressor {
    #[cfg(feature = "compression-zstd")]
    context: zstd::zstd_safe::DCtx<'static>,
    #[cfg(feature = "compression-zstd")]
    max_window: u64,
}

/// Legacy buffered decoders bypass DCtx's WindowLogMax. Preflight every frame,
/// including concatenated legacy frames, before entering the native decoder.
/// v0.4/0.5 use a four-bit log plus 11; v0.6 uses plus 12. v0.7 and current
/// frames share the window descriptor/single-segment layout (upstream legacy
/// ZSTD*_getFrameParams and RFC 8878 section 3.1.1.1).
#[cfg(all(feature = "integrity", feature = "compression-zstd"))]
fn validate_stream_windows(mut bytes: &[u8], maximum: u64) -> Result<()> {
    while !bytes.is_empty() {
        let magic = u32::from_le_bytes(
            bytes
                .get(..4)
                .ok_or(Error::InvalidChunkedBytes)?
                .try_into()
                .expect("four bytes"),
        );
        let window = if magic & 0xfffffff0 == 0x184d2a50 {
            0
        } else {
            let descriptor = *bytes.get(4).ok_or(Error::InvalidChunkedBytes)?;
            match magic {
                0xfd2fb524 | 0xfd2fb525 => 1u64 << ((descriptor & 15) + 11),
                0xfd2fb526 => 1u64 << ((descriptor & 15) + 12),
                0xfd2fb527 | 0xfd2fb528 => if descriptor & 0x20 != 0 {
                    zstd::zstd_safe::get_frame_content_size(bytes)
                        .map_err(|_| Error::InvalidChunkedBytes)?
                        .ok_or(Error::InvalidChunkedBytes)?
                } else {
                    let window = *bytes.get(5).ok_or(Error::InvalidChunkedBytes)?;
                    let base = 1u64 << ((window >> 3) + 10);
                    base + (base >> 3) * u64::from(window & 7)
                }
                .max(1024),
                // zstd's streaming adapter does not support versions 0.1-0.3.
                _ => return Err(std::io::Error::from(std::io::ErrorKind::Unsupported).into()),
            }
        };
        if window > maximum {
            return Err(Error::LimitExceeded {
                resource: "zstd streaming window bytes",
                actual: window,
                limit: maximum,
            });
        }
        let len = zstd::zstd_safe::find_frame_compressed_size(bytes).map_err(backend_error)?;
        bytes = bytes
            .get(len..)
            .filter(|_| len != 0)
            .ok_or(Error::InvalidChunkedBytes)?;
    }
    Ok(())
}

#[cfg(feature = "integrity")]
impl StreamDecompressor {
    pub(crate) fn new(max_window_bytes: usize) -> Result<Self> {
        #[cfg(feature = "compression-zstd")]
        {
            if max_window_bytes < 1024 {
                return Err(Error::LimitExceeded {
                    resource: "zstd streaming window bytes",
                    actual: 1024,
                    limit: max_window_bytes as u64,
                });
            }
            let mut context = new_decoder()?;
            let log = max_window_bytes
                .ilog2()
                .min(if usize::BITS == 32 { 30 } else { 31 });
            context
                .set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(log))
                .map_err(backend_error)?;
            Ok(Self {
                context,
                max_window: 1u64 << log,
            })
        }
        #[cfg(not(feature = "compression-zstd"))]
        {
            let _ = max_window_bytes;
            Err(Error::CompressionFeatureDisabled)
        }
    }

    pub(crate) fn decode(
        &mut self,
        payload: &[u8],
        buffer: &mut [u8],
        expected: u64,
        mut visit: impl FnMut(u64, &[u8]) -> Result<()>,
    ) -> Result<()> {
        #[cfg(feature = "compression-zstd")]
        {
            use zstd::zstd_safe::{InBuffer, OutBuffer, ResetDirective};
            validate_stream_windows(payload, self.max_window)?;
            self.context
                .reset(ResetDirective::SessionOnly)
                .map_err(backend_error)?;
            let mut input = InBuffer {
                src: payload,
                pos: 0,
            };
            let mut decoded = 0u64;
            loop {
                let before = input.pos;
                let mut output = OutBuffer::around(&mut *buffer);
                let remaining = self
                    .context
                    .decompress_stream(&mut output, &mut input)
                    .map_err(backend_error)?;
                let written = output.pos();
                let next = resource_steps::stream_progress(
                    before,
                    input.pos,
                    payload.len(),
                    decoded,
                    written,
                    expected,
                )
                .ok_or(Error::InvalidChunkedBytes)?;
                if written != 0 {
                    visit(decoded, &buffer[..written])?;
                }
                decoded = next;
                if remaining == 0 && input.pos == payload.len() {
                    break;
                }
            }
            if decoded != expected {
                return Err(Error::DecompressedLengthMismatch {
                    expected,
                    actual: decoded,
                });
            }
            Ok(())
        }
        #[cfg(not(feature = "compression-zstd"))]
        {
            let _ = (payload, buffer, expected, &mut visit);
            Err(Error::CompressionFeatureDisabled)
        }
    }
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
    /// Decompresses one frame into `out`, which must be exactly its
    /// decompressed length. No allocation: the output is the window.
    #[cfg(feature = "compression-zstd")]
    pub(crate) fn decompress_into(&mut self, payload: &[u8], out: &mut [u8]) -> Result<()> {
        if let (Some(declared), _) = frame_sizes(payload)?
            && declared != out.len() as u64
        {
            return Err(Error::DecompressedLengthMismatch {
                expected: out.len() as u64,
                actual: declared,
            });
        }
        let len = self
            .context
            .decompress(out, payload)
            .map_err(backend_error)?;
        if len != out.len() {
            return Err(Error::DecompressedLengthMismatch {
                expected: out.len() as u64,
                actual: len as u64,
            });
        }
        Ok(())
    }
    #[cfg(not(feature = "compression-zstd"))]
    pub(crate) fn decompress_into(&mut self, _payload: &[u8], _out: &mut [u8]) -> Result<()> {
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
