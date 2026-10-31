//! Fallible Rust output buffers for the compression backend.
use crate::{CompressionLevel, Error, Result};

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
    // Preserve the backend's frame-size screen: a small frame must not cause
    // an allocation of a much larger caller-declared maximum.
    let capacity = match frame_output_len(payload) {
        Some(actual) if actual > maximum as u64 => {
            return Err(Error::DecompressedLengthMismatch {
                expected: maximum as u64,
                actual,
            });
        }
        Some(actual) => actual as usize,
        None => zstd::bulk::Decompressor::upper_bound(payload)
            .unwrap_or(maximum)
            .min(maximum),
    };
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

#[cfg(feature = "compression-zstd")]
fn frame_output_len(mut bytes: &[u8]) -> Option<u64> {
    let mut total = 0u64;
    while !bytes.is_empty() {
        let len = zstd::zstd_safe::find_frame_compressed_size(bytes).ok()?;
        if len == 0 {
            return None;
        }
        total = total.checked_add(zstd::zstd_safe::get_frame_content_size(bytes).ok()??)?;
        bytes = bytes.get(len..)?;
    }
    Some(total)
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
    pub(crate) fn decompress_chunk(&mut self, payload: &[u8], expected: usize) -> Result<&[u8]> {
        let exact = frame_output_len(payload);
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
    pub(crate) fn decompress_chunk(&mut self, _payload: &[u8], _expected: usize) -> Result<&[u8]> {
        Err(Error::CompressionFeatureDisabled)
    }
    #[cfg(feature = "compression-zstd")]
    pub(crate) fn decompress_append(
        &mut self,
        payload: &[u8],
        output: &mut Vec<u8>,
        expected: usize,
    ) -> Result<usize> {
        let start = output.len();
        let end = start
            .checked_add(expected)
            .filter(|end| *end <= output.capacity())
            .ok_or(Error::InvalidChunkedBytes)?;
        let written = match frame_output_len(payload) {
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
    ) -> Result<usize> {
        Err(Error::CompressionFeatureDisabled)
    }
}

#[cfg(all(test, feature = "compression-zstd"))]
mod tests {
    use super::*;

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
            let chunk = decoder.decompress_chunk(frame, expected)?;
            assert_eq!(chunk.len(), expected);
            assert!(
                chunk.iter().all(|byte| *byte == 23),
                "invalid scratch decoded bytes"
            );
            assert!(decoder.decompress_chunk(frame, expected - 1).is_err());
            let mut output = Vec::with_capacity(16384);
            output.extend_from_slice(b"prefix");
            let written = decoder.decompress_append(frame, &mut output, expected)?;
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
                    .decompress_append(frame, &mut output, expected - 1)
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
