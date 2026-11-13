//! Reading a record's logical payload into a caller's buffer, a buffer at a
//! time.
//!
//! The library never allocates in proportion to a length the file declares:
//! a call fills what `out` holds and returns where to continue. Only checked
//! bytes count as filled. A chunked record (`record_chunks`) is checked a
//! chunk at a time — straight into `out` when a whole chunk fits there,
//! through one 64 KiB staging buffer otherwise. Other records are checked
//! against their record checksum before any of their bytes are handed out.

use super::record_chunks::{CHUNK_HEADER_LEN, CHUNK_LEN, ChunkHeader, read_chunk};
use super::*;

/// Where a record read continues: the record, the logical offset in it,
/// and where the read stops (the record's end, or the end of a field).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecordCursor {
    record_offset: u64,
    offset: u64,
    end: u64,
}

impl RecordCursor {
    pub(crate) const fn new(record_offset: u64, offset: u64) -> Self {
        Self {
            record_offset,
            offset,
            end: u64::MAX,
        }
    }
    /// The record being read (its append-log offset).
    pub const fn record_offset(self) -> u64 {
        self.record_offset
    }
    /// The logical byte the next read starts at.
    pub const fn offset(self) -> u64 {
        self.offset
    }
    /// Where the read stops (`u64::MAX`: the record's end).
    pub const fn end(self) -> u64 {
        self.end
    }
}

/// One call's worth of a record read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordPage {
    /// Checked bytes written to the front of the caller's buffer.
    pub filled: usize,
    /// The logical offset this read stops at: the record's length, or the
    /// end of the field being read.
    pub end: u64,
    /// Where to continue, or `None` once the record is read to its end.
    pub next: Option<RecordCursor>,
}

fn past_end() -> Error {
    Error::InvalidCanonicalEncoding("record read starts past the record's end")
}

fn empty_buffer() -> Error {
    Error::BufferTooSmall {
        resource: "record read buffer",
        required: 1,
        provided: 0,
    }
}

/// The one checked chunk a handle keeps, so a buffer smaller than a chunk
/// does not read and check the chunk again on every call. Keyed by the
/// record's identity as framed (offset, sequence, checksum, length), never
/// by offset alone, so a rewritten file cannot serve it stale.
#[derive(Default)]
pub(crate) struct ChunkCache {
    key: Option<CacheKey>,
    bytes: Vec<u8>,
    /// A compressed chunk as stored, and the context that decodes it: kept
    /// so a walk over a record's chunks allocates them once.
    compressed: Vec<u8>,
    decompressor: Option<crate::compression_buffers::Decompressor>,
    /// Scratch a read that has no caller buffer to stage in borrows for its
    /// duration ([`lend_scratch`]).
    scratch: Vec<u8>,
    /// The read-ahead window paged walks frame records through.
    window: Option<ReadAhead>,
}

/// The handle's read-ahead window for one walk, or a new one when it is
/// out (or was lost to an earlier error).
pub(crate) fn lend_window(cache: &std::cell::RefCell<ChunkCache>) -> Result<ReadAhead> {
    match cache
        .try_borrow_mut()
        .ok()
        .and_then(|mut cache| cache.window.take())
    {
        Some(window) => Ok(window),
        None => ReadAhead::new(),
    }
}

pub(crate) fn return_window(cache: &std::cell::RefCell<ChunkCache>, window: ReadAhead) {
    if let Ok(mut cache) = cache.try_borrow_mut() {
        cache.window = Some(window);
    }
}

impl ChunkCache {
    /// Reads and decodes an unchunked compressed record into `out` through
    /// this handle's stored-bytes buffer and decompression context.
    pub(crate) fn decode_compressed_into(
        &mut self,
        spec: FormatSpec,
        snapshot: &SnapshotFile,
        entry: &RecordIndexEntry,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let stored = staging(&mut self.compressed, entry.payload_len as usize)?;
        snapshot.read_exact_at(entry.payload_offset, stored)?;
        entry.verify_snapshot_record(spec, snapshot, stored)?;
        header_codec::decode_compressed_payload_into(
            spec,
            entry,
            stored,
            out,
            &mut self.decompressor,
        )
    }
}

/// Takes the handle's scratch buffer for one call. A call that finds it
/// already lent (a read re-entered from a codec) gets an empty one of its
/// own, so nothing is ever shared.
pub(crate) fn lend_scratch(cache: &std::cell::RefCell<ChunkCache>) -> Vec<u8> {
    cache
        .try_borrow_mut()
        .map(|mut cache| std::mem::take(&mut cache.scratch))
        .unwrap_or_default()
}

/// Returns a buffer taken with [`lend_scratch`], keeping the larger of it
/// and whatever is there.
pub(crate) fn return_scratch(cache: &std::cell::RefCell<ChunkCache>, scratch: Vec<u8>) {
    if let Ok(mut cache) = cache.try_borrow_mut()
        && cache.scratch.capacity() < scratch.capacity()
    {
        cache.scratch = scratch;
    }
}

impl std::fmt::Debug for ChunkCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkCache")
            .field("key", &self.key)
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CacheKey {
    record_offset: u64,
    sequence: u64,
    checksum: u32,
    payload_len: u64,
    /// The chunk; [`WHOLE`] for a whole short record, [`CHECKED`] for a long
    /// unchunked record whose checksum has been streamed (no bytes kept).
    index: u64,
}

const WHOLE: u64 = u64::MAX;
const CHECKED: u64 = u64::MAX - 1;

impl CacheKey {
    fn of(entry: &RecordIndexEntry, index: u64) -> Self {
        Self {
            record_offset: entry.record_offset,
            sequence: entry.sequence,
            checksum: entry.checksum,
            payload_len: entry.payload_len,
            index,
        }
    }
}

impl ChunkCache {
    /// The cached bytes for `key`, or `fill` run to produce (and check) them.
    /// A failed fill leaves the cache empty.
    fn get_or_fill(
        &mut self,
        key: CacheKey,
        len: usize,
        fill: impl FnOnce(&mut [u8]) -> Result<()>,
    ) -> Result<&[u8]> {
        if self.key != Some(key) {
            self.key = None;
            let bytes = staging(&mut self.bytes, len)?;
            fill(bytes)?;
            self.key = Some(key);
        }
        Ok(&self.bytes[..len])
    }
}

fn staging(staged: &mut Vec<u8>, len: usize) -> Result<&mut [u8]> {
    // A whole compressed record may have left the staging longer or shorter;
    // only growth is reserved.
    if staged.len() < len {
        staged
            .try_reserve_exact(len - staged.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "record read staging",
                requested: len as u64,
            })?;
        staged.resize(len, 0);
    }
    Ok(&mut staged[..len])
}

/// Reads `entry`'s logical payload from `from` into `out` through `snapshot`.
pub(crate) fn fill_record(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    entry: &RecordIndexEntry,
    from: u64,
    out: &mut [u8],
    cache: &std::cell::RefCell<ChunkCache>,
) -> Result<RecordPage> {
    validate_record_entry(&spec, entry)?;
    let verify = spec.read_validation().data_crc && spec.integrity_policy != IntegrityPolicy::None;
    let mut cache = cache.try_borrow_mut().map_err(|_| {
        Error::InvalidCanonicalEncoding("record read re-entered on the same handle")
    })?;
    if entry.is_chunked() {
        return fill_chunked(snapshot, entry, from, out, verify, &mut cache);
    }
    if entry.is_compressed() {
        return fill_compressed(spec, snapshot, entry, from, out, &mut cache);
    }
    let total = entry.payload_len;
    let page = |filled: usize| RecordPage {
        filled,
        end: total,
        next: ((from + filled as u64) < total)
            .then(|| RecordCursor::new(entry.record_offset, from + filled as u64)),
    };
    if from > total {
        return Err(past_end());
    }
    let want = (total - from).min(out.len() as u64) as usize;
    if want == 0 {
        return if from == total {
            Ok(page(0))
        } else {
            Err(empty_buffer())
        };
    }
    if !verify {
        snapshot.read_exact_at(entry.payload_offset + from, &mut out[..want])?;
        return Ok(page(want));
    }
    if total <= CHUNK_LEN {
        // Small enough to check whole: into `out` when it all fits there,
        // through staging otherwise.
        let total = total as usize;
        if from == 0 && want == total {
            snapshot.read_exact_at(entry.payload_offset, &mut out[..total])?;
            entry.verify_snapshot_record(spec, snapshot, &out[..total])?;
            return Ok(page(want));
        }
        let bytes = cache.get_or_fill(CacheKey::of(entry, WHOLE), total, |bytes| {
            snapshot.read_exact_at(entry.payload_offset, bytes)?;
            entry.verify_snapshot_record(spec, snapshot, bytes)
        })?;
        out[..want].copy_from_slice(&bytes[from as usize..from as usize + want]);
        return Ok(page(want));
    }
    // A long record written before chunking: its one checksum covers all of
    // it, so stream the whole record through it once (remembered), then read
    // the part asked for.
    cache.get_or_fill(CacheKey::of(entry, CHECKED), 0, |_| {
        let footer = record_footer_bytes(snapshot, entry)?;
        let checksum = checksum_record_file(spec, &mut snapshot.reader(), entry, &footer)?;
        if checksum != entry.checksum {
            return Err(Error::ChecksumMismatch {
                offset: entry.record_offset,
            });
        }
        Ok(())
    })?;
    snapshot.read_exact_at(entry.payload_offset + from, &mut out[..want])?;
    Ok(page(want))
}

/// The footer bytes a record's checksum covers, as stored.
fn record_footer_bytes(snapshot: &SnapshotFile, entry: &RecordIndexEntry) -> Result<Vec<u8>> {
    let Some(footer_offset) = entry.footer_offset else {
        return Ok(Vec::new());
    };
    let mut footer = vec![0u8; RECORD_FOOTER_LEN as usize];
    snapshot.read_exact_at(footer_offset, &mut footer)?;
    Ok(footer)
}

fn fill_chunked(
    snapshot: &SnapshotFile,
    entry: &RecordIndexEntry,
    from: u64,
    out: &mut [u8],
    verify: bool,
    cache: &mut ChunkCache,
) -> Result<RecordPage> {
    if entry.payload_len < CHUNK_HEADER_LEN {
        return Err(Error::InvalidCompressionHeader);
    }
    let mut prefix = [0u8; CHUNK_HEADER_LEN as usize];
    snapshot.read_exact_at(entry.payload_offset, &mut prefix)?;
    let header = ChunkHeader::decode(&prefix)?;
    let total = header.logical_len;
    if from > total {
        return Err(past_end());
    }
    if from < total && out.is_empty() {
        return Err(empty_buffer());
    }
    let base = entry.payload_offset;
    let payload_len = entry.payload_len;
    let mut read = |at: u64, buf: &mut [u8]| -> Result<()> {
        let end = at
            .checked_add(buf.len() as u64)
            .filter(|end| *end <= payload_len)
            .ok_or(Error::InvalidCanonicalEncoding(
                "chunked record extent past its payload",
            ))?;
        let _ = end;
        snapshot.read_exact_at(base + at, buf)
    };
    // The handle's compression buffers, lent to this call and returned below.
    let mut scratch = std::mem::take(&mut cache.compressed);
    let mut decompressor = cache.decompressor.take();
    let mut position = from;
    let mut filled = 0usize;
    let mut walk = || -> Result<()> {
        while filled < out.len() && position < total {
            let index = position / CHUNK_LEN;
            let within = position % CHUNK_LEN;
            let len = header.chunk_len(index);
            let room = (out.len() - filled) as u64;
            if within == 0 && room >= len {
                let target = &mut out[filled..filled + len as usize];
                read_chunk_checked(
                    &mut read,
                    &header,
                    payload_len,
                    base,
                    index,
                    target,
                    &mut scratch,
                    &mut decompressor,
                    verify,
                )?;
                filled += len as usize;
                position += len;
            } else {
                let bytes =
                    cache.get_or_fill(CacheKey::of(entry, index), len as usize, |bytes| {
                        read_chunk_checked(
                            &mut read,
                            &header,
                            payload_len,
                            base,
                            index,
                            bytes,
                            &mut scratch,
                            &mut decompressor,
                            verify,
                        )
                    })?;
                let take = (len - within).min(room) as usize;
                out[filled..filled + take]
                    .copy_from_slice(&bytes[within as usize..within as usize + take]);
                filled += take;
                position += take as u64;
            }
        }
        Ok(())
    };
    let walked = walk();
    cache.compressed = scratch;
    cache.decompressor = decompressor;
    walked?;
    Ok(RecordPage {
        filled,
        end: total,
        next: (position < total).then(|| RecordCursor::new(entry.record_offset, position)),
    })
}

#[allow(clippy::too_many_arguments)]
fn read_chunk_checked(
    read: record_chunks::PayloadRead<'_>,
    header: &ChunkHeader,
    payload_len: u64,
    base: u64,
    index: u64,
    out: &mut [u8],
    scratch: &mut Vec<u8>,
    decompressor: &mut Option<crate::compression_buffers::Decompressor>,
    verify: bool,
) -> Result<()> {
    match read_chunk(
        read,
        header,
        payload_len,
        base,
        index,
        out,
        scratch,
        decompressor,
    ) {
        // With data checksums turned off, a mismatch is not this read's
        // business; the bytes were read and decoded all the same.
        Err(Error::ChecksumMismatch { .. }) if !verify => Ok(()),
        other => other,
    }
}

/// A compressed record without chunks (one shorter than a chunk, or written
/// before chunking): decoded whole, within the read limits, then copied.
fn fill_compressed(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    entry: &RecordIndexEntry,
    from: u64,
    out: &mut [u8],
    cache: &mut ChunkCache,
) -> Result<RecordPage> {
    // Decoded once into the handle's staging and kept: the pages of one
    // record do not decompress it again.
    let key = CacheKey::of(entry, WHOLE);
    if cache.key != Some(key) {
        cache.key = None;
        entry.logical_payload_len_snapshot(spec, snapshot)?;
        let mut logical = std::mem::take(&mut cache.bytes);
        let decoded = cache.decode_compressed_into(spec, snapshot, entry, &mut logical);
        cache.bytes = logical;
        decoded?;
        cache.key = Some(key);
    }
    let logical = &cache.bytes;
    let total = logical.len() as u64;
    if from > total {
        return Err(past_end());
    }
    if from < total && out.is_empty() {
        return Err(empty_buffer());
    }
    let want = (total - from).min(out.len() as u64) as usize;
    out[..want].copy_from_slice(&logical[from as usize..from as usize + want]);
    let position = from + want as u64;
    Ok(RecordPage {
        filled: want,
        end: total,
        next: (position < total).then(|| RecordCursor::new(entry.record_offset, position)),
    })
}

/// [`fill_record`] stopping at logical offset `end` (for a field).
pub(crate) fn fill_range(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    entry: &RecordIndexEntry,
    from: u64,
    end: u64,
    out: &mut [u8],
    cache: &std::cell::RefCell<ChunkCache>,
) -> Result<RecordPage> {
    if from > end {
        return Err(past_end());
    }
    let cap = (end - from).min(out.len() as u64) as usize;
    if cap == 0 {
        return if from == end {
            Ok(RecordPage {
                filled: 0,
                end,
                next: None,
            })
        } else {
            Err(empty_buffer())
        };
    }
    let page = fill_record(spec, snapshot, entry, from, &mut out[..cap], cache)?;
    let stop = page.end.min(end);
    let position = from + page.filled as u64;
    Ok(RecordPage {
        filled: page.filled,
        end: stop,
        next: (position < stop).then_some(RecordCursor {
            record_offset: entry.record_offset,
            offset: position,
            end,
        }),
    })
}

impl VarlitFile {
    /// Reads a record's logical payload (decompressed, if it is compressed)
    /// into `out`, as much as it holds. The page says how many bytes were
    /// filled and, while the record goes on, where to continue with
    /// [`Self::read_record_from`].
    ///
    /// Only checked bytes count as filled: a long record is checked 64 KiB at
    /// a time, so a buffer of any size reads it without the library holding
    /// more than one chunk; a short one is checked whole first. On error the
    /// buffer holds nothing to trust. Bounded by `out`, not by read limits.
    pub fn read_record_into(&self, entry: &RecordIndexEntry, out: &mut [u8]) -> Result<RecordPage> {
        fill_record(self.spec, &self.snapshot, entry, 0, out, &self.record_cache)
    }

    /// Continues a record read where `cursor` stopped.
    pub fn read_record_from(&self, cursor: RecordCursor, out: &mut [u8]) -> Result<RecordPage> {
        let entry = read_record_entry_positional(self.spec, &self.snapshot, cursor.record_offset)?;
        fill_range(
            self.spec,
            &self.snapshot,
            &entry,
            cursor.offset,
            cursor.end,
            out,
            &self.record_cache,
        )
    }
}
