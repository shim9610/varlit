# Reading a range from ChunkedBytes

`ChunkedBytes::stream_range` reads a decoded byte range without allocating its
complete output or a decoded chunk. It is an explicit alternative when
`decode_to_vec_limited` refuses whole-output materialization. Existing decoding
APIs and the VCHK file format are unchanged.

```rust,no_run
# fn read(chunks: &varlit::ChunkedBytes) -> varlit::Result<()> {
let wanted = 1024..4096; // absolute decoded offsets; end is exclusive
let mut buffer = [0u8; 8192];
let mut staged = std::fs::File::create("range.pending")?;
chunks.stream_range(wanted, &mut buffer, 8 * 1024 * 1024, |offset, bytes| {
    // offset is the position in the decoded blob, not in the staging file.
    let _ = offset;
    std::io::Write::write_all(&mut staged, bytes)?;
    Ok(())
})?;
// Publish staged output only after success. Remove/discard it after an error.
# Ok(()) }
```

Callers can match `DecompressedLengthLimitExceeded` or `AllocationFailed` from
whole-output decoding and then choose this API on the same `ChunkedBytes`.
For example, the application can make the fallback decision explicitly:

```rust,no_run
# fn fallback(chunks: &varlit::ChunkedBytes) -> varlit::Result<()> {
use std::io::Write;
use varlit::Error;
let wanted = 0..chunks.decoded_len().min(4096);
let mut staged = std::fs::File::create("range.pending")?;
match chunks.decode_to_vec_limited(64 * 1024) {
    Ok(full) => staged.write_all(&full[wanted.start as usize..wanted.end as usize])?,
    Err(Error::DecompressedLengthLimitExceeded { .. } | Error::AllocationFailed { .. }) => {
        let mut scratch = [0; 8192];
        chunks.stream_range(wanted, &mut scratch, 8 * 1024 * 1024, |_, bytes| {
            staged.write_all(bytes)?;
            Ok(())
        })?;
    }
    Err(error) => return Err(error),
}
// Only now publish staged output. On error, discard the staging file.
# Ok(()) }
```

`decoded_len()` gives the full decoded range endpoint, without authenticating
the content. An out-of-bounds or reversed range returns `InvalidChunkedRange`.
Empty ranges need no decoder or buffer. A nonempty range requires at least one
buffer byte; `BufferTooSmall` reports that requirement before any callback.
Increasing the buffer is optional and improves batching; it is never expanded
by the library.

Only chunks overlapping the range are decompressed. Their prefixes and suffixes
are still decoded to validate complete lengths and CRCs. Corruption in the
unselected chunks' contents is not checked. Seeking scans table entries before
the range, without decompressing those earlier chunks or constructing an index.

Delivery is **provisional until the method returns success**. A bad checksum at
the end of a chunk can follow callbacks, so a callback must not publish results
or perform irreversible actions on the assumption they are already verified.
Callback errors stop immediately and propagate unchanged. A retry starts the
requested range again; it does not resume after a previously delivered prefix.
Staged files belong to the caller and are not removed automatically.

`max_window_bytes` bounds the zstd history window, rounded down to a power of
two, with a 1024-byte minimum. For example, 8 MiB permits windows up to 8 MiB;
a larger blob is still readable if each selected frame fits that window.
A lower window policy can reject a valid frame even with a sufficiently large
output buffer. The cap is checked against every selected frame before streaming
and is also passed to zstd. This preflight includes legacy v0.4–v0.7 frames, whose native
streaming adapters bypass the modern window check. zstd does not support
streaming v0.1–v0.3 frames; whole-output decoding of older versions keeps its
existing compatibility behavior. The window ceiling is not a process memory
cap: zstd also allocates its context and block buffers. Those
native allocations can fail and are reported as errors. The encoded input is
already held by `ChunkedBytes`; this API does not stream that input from disk.
The callback also controls any memory it retains.

Nonempty reads require `compression-zstd` and `integrity`. No default logical
size cap is silently lifted: this separate API uses an explicit range and window
policy, and never requests the whole decoded size as a heap allocation. This
policy does not change the limits applied when acquiring/decoding the enclosing
record into a `ChunkedBytes` value.
