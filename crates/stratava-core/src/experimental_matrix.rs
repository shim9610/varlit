//! Experimental, opt-in matrix page reuse.
//!
//! `MatrixOptions::reuse` defaults to false, selecting ordinary snapshot readers.
//! With reuse enabled, each read retries against the latest confirmed generation;
//! it does not retain a historical snapshot. Only owned reads are exposed.
//! The experimental file format is separate from the native format and may change.
use crate::format::ReadLimitKey;
use crate::{
    BlockKind, Decoder, FormatSpec, MatrixDimensions, MatrixKey, StratavaMatrixBlock,
    StratavaReader, StratavaWriter,
};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const PAGE: u64 = 4096;
const HEADER: u64 = 3 * PAGE;
const SLOT_HEADER: u64 = 64;
const JOURNAL_ENTRY: u64 = 96;
const MAGIC: &[u8; 8] = b"STREUSE2";
const SLOT_MAGIC: &[u8; 8] = b"STRSLOT2";
const HEAD_MAGIC: &[u8; 8] = b"STRHEAD2";
const INTENT_MAGIC: &[u8; 8] = b"STRINTN2";

/// Thread-local I/O accounting for contract tests; counts positional calls, not
/// filesystem/device traffic. Absent from builds without fault instrumentation.
#[doc(hidden)]
#[cfg(feature = "scalable-fault-injection")]
#[derive(Default, Clone, Copy, Debug)]
pub struct TestIo {
    pub read_calls: u64,
    pub read_bytes: u64,
    pub write_calls: u64,
    pub write_bytes: u64,
}
#[cfg(feature = "scalable-fault-injection")]
thread_local! {static TEST_IO: Cell<TestIo> = Cell::new(TestIo::default());}
#[doc(hidden)]
#[cfg(feature = "scalable-fault-injection")]
pub fn take_test_io() -> TestIo {
    TEST_IO.with(|s| s.replace(TestIo::default()))
}

#[cfg(feature = "scalable-fault-injection")]
type TestHook = Box<dyn FnMut(&'static str) -> io::Result<()>>;
#[cfg(feature = "scalable-fault-injection")]
thread_local! {static TEST_HOOK: RefCell<Option<TestHook>> = RefCell::new(None);}
/// Thread-local deterministic interleaving/failure hook for dedicated tests only.
#[doc(hidden)]
#[cfg(feature = "scalable-fault-injection")]
pub fn set_test_hook(hook: Option<TestHook>) {
    TEST_HOOK.with(|slot| *slot.borrow_mut() = hook);
}
fn fault(point: &'static str) -> Result<()> {
    crate::scalable_fault_point(point);
    #[cfg(feature = "scalable-fault-injection")]
    TEST_HOOK.with(|slot| {
        let mut hook = slot.borrow_mut().take();
        let result = match hook.as_mut() {
            Some(h) => h(point),
            None => Ok(()),
        };
        *slot.borrow_mut() = hook;
        result
    })?;
    Ok(())
}

/// Independent build-time and runtime opt-in. No existing file is converted.
#[derive(Clone, Copy, Debug)]
pub struct MatrixOptions {
    /// False preserves the ordinary storage and explicit-follow contract.
    pub reuse: bool,
    /// First-axis rows per experimental chunk (also used for fixed matrices).
    pub rows_per_chunk: u64,
    /// Page payload bytes: a power of two from 4 KiB through 64 KiB.
    pub page_bytes: u64,
    /// Writer page-cache budget (legacy field name); no limit on logical chunk size.
    /// Must hold at least one page. Independent of total file size.
    pub max_chunk_bytes: u64,
    /// Reader-local cache capacity; zero disables retention.
    pub cache_bytes: u64,
    /// Maximum attempts per owned read while publications race it.
    pub read_retries: u32,
}
impl Default for MatrixOptions {
    fn default() -> Self {
        Self {
            reuse: false,
            rows_per_chunk: 4096,
            page_bytes: 4096,
            max_chunk_bytes: 16 * 1024 * 1024,
            cache_bytes: 2 * 1024 * 1024,
            read_retries: 8,
        }
    }
}
/// Experimental errors; a failed sync poisons that writer, requiring reopen.
#[derive(Debug, thiserror::Error)]
pub enum MatrixReuseError {
    #[error(transparent)]
    Native(#[from] crate::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("experimental matrix format: {0}")]
    Invalid(&'static str),
    #[error("experimental matrix writer is poisoned; drop and reopen for recovery")]
    Poisoned,
    #[error("matrix changed during {attempts} read attempts; retry the read")]
    Retry { attempts: u32 },
    #[error("cell is not committed")]
    Missing,
    #[error("experimental matrix limit exceeded: {0}")]
    Limit(&'static str),
    #[error("generation {generation} may have been published; reopen before retrying: {source}")]
    PublicationIndeterminate {
        generation: u64,
        #[source]
        source: Box<MatrixReuseError>,
    },
    #[error("generation {generation} is durable but post-publication cleanup failed: {source}")]
    Published {
        generation: u64,
        #[source]
        source: Box<MatrixReuseError>,
    },
}
type Result<T> = std::result::Result<T, MatrixReuseError>;
fn invalid(s: &'static str) -> MatrixReuseError {
    MatrixReuseError::Invalid(s)
}
fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).ok_or_else(|| invalid("offset overflow"))
}
fn mul(a: u64, b: u64) -> Result<u64> {
    a.checked_mul(b).ok_or_else(|| invalid("size overflow"))
}
fn align(n: u64, unit: u64) -> Result<u64> {
    mul(n.div_ceil(unit), unit)
}
fn alloc(n: u64) -> Result<Vec<u8>> {
    let n = usize::try_from(n).map_err(|_| MatrixReuseError::Limit("address space"))?;
    let mut v = Vec::new();
    v.try_reserve_exact(n)
        .map_err(|_| MatrixReuseError::Limit("allocation"))?;
    v.resize(n, 0);
    Ok(v)
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn put(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
fn seal(b: &mut [u8]) {
    let n = b.len() - 4;
    let crc = crc32fast::hash(&b[..n]);
    b[n..].copy_from_slice(&crc.to_le_bytes());
}
fn valid(b: &[u8]) -> bool {
    let n = b.len() - 4;
    crc32fast::hash(&b[..n]) == u32::from_le_bytes(b[n..].try_into().unwrap())
}
fn read_at(f: &File, mut at: u64, mut out: &mut [u8]) -> io::Result<()> {
    while !out.is_empty() {
        let n = match crate::snapshot::read_at(f, out, at) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => r?,
        };
        #[cfg(feature = "scalable-fault-injection")]
        TEST_IO.with(|s| {
            let mut v = s.get();
            v.read_calls += 1;
            v.read_bytes += n as u64;
            s.set(v);
        });
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        at += n as u64;
        out = &mut out[n..];
    }
    Ok(())
}
fn write_at(f: &File, mut at: u64, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let r = std::os::unix::fs::FileExt::write_at(f, bytes, at);
        #[cfg(windows)]
        let r = std::os::windows::fs::FileExt::seek_write(f, bytes, at);
        #[cfg(not(any(unix, windows)))]
        let r: io::Result<usize> = Err(io::ErrorKind::Unsupported.into());
        let n = match r {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => r?,
        };
        #[cfg(feature = "scalable-fault-injection")]
        TEST_IO.with(|s| {
            let mut v = s.get();
            v.write_calls += 1;
            v.write_bytes += n as u64;
            s.set(v);
        });
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        at += n as u64;
        bytes = &bytes[n..];
    }
    Ok(())
}
fn open_sidecar(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = opts.open(path)?;
    crate::file::verify_dedicated_lock_marker(path, &file)?;
    Ok(file)
}
fn companion(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(suffix);
    s.into()
}
fn path_hash(path: &Path) -> u64 {
    // Bind sidecar names to the original absolute primary path. Renames/hard-link
    // aliases require an explicit future migration, never an empty recovery log.
    let mut h = 0xcbf29ce484222325u64;
    for b in path.as_os_str().as_encoded_bytes() {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
    }
    h
}
#[derive(Clone)]
struct Block {
    id: u32,
    cols: u64,
    stride: u64,
    bitmap: u64,
    slots: u64,
}
#[derive(Clone)]
struct Geometry {
    spec: FormatSpec,
    options: MatrixOptions,
    blocks: Vec<Block>,
    bytes: u64,
    chunk_bytes: u64,
    cache_capacity: u64,
    material_limit: u64,
    stage_bytes: u64,
    stride: u64,
    fixed_rows: Option<u64>,
    hash: u64,
}
impl Geometry {
    fn new(spec: FormatSpec, dims: &MatrixDimensions, options: MatrixOptions) -> Result<Self> {
        let spec = spec.resolve_entrypoint();
        spec.validate()?;
        spec.read_limits.check(ReadLimitKey::FileLen, HEADER)?;
        if options.rows_per_chunk == 0
            || !options.page_bytes.is_power_of_two()
            || !(4096..=65536).contains(&options.page_bytes)
            || options.max_chunk_bytes < options.page_bytes
            || options.read_retries == 0
            || options.read_retries > 1024
        {
            return Err(invalid("zero geometry/budget or invalid retry count"));
        }
        if spec.matrix_blocks.is_empty()
            || spec.blocks.iter().any(|b| b.kind != BlockKind::Matrix)
            || !spec.matrix_aux.is_empty()
            || spec.chunk_compression.is_some()
        {
            return Err(invalid(
                "reuse currently supports uncompressed matrix-only schemas without auxiliary regions",
            ));
        }
        if spec.layout.preset != crate::LayoutPreset::StratavaNative
            || !spec.layout.parts.is_empty()
            || spec.commit_policy != crate::CommitPolicy::None
            || spec.manifest_policy != crate::ManifestPolicy::None
            || spec.header_slots.capacity != 0
            || !spec.block_compression.is_empty()
            || spec
                .matrix_commits
                .iter()
                .any(|c| c.kind != crate::MatrixCommitKind::Cell)
            || spec.block_identities.iter().any(|identity| identity.2)
        {
            return Err(invalid(
                "unsupported native layout, record policy or commit category in reuse schema",
            ));
        }
        for (i, b) in spec.matrix_blocks.iter().enumerate() {
            if spec.matrix_blocks[..i]
                .iter()
                .any(|other| other.category == b.category)
            {
                return Err(invalid(
                    "reuse requires a distinct cell category per matrix block",
                ));
            }
        }
        let first = spec.matrix_blocks[0].dimensions[0];
        let fixed_rows = if let Some(g) = spec.growing_matrix {
            if g.name != first {
                return Err(invalid("growing dimension must be the first matrix axis"));
            }
            None
        } else {
            Some(
                dims.get(first)
                    .ok_or_else(|| invalid("missing first dimension"))?,
            )
        };
        let mut blocks = Vec::new();
        let mut bytes = 0;
        let mut h = spec.computed_schema_hash();
        for descriptor in spec.matrix_blocks {
            if descriptor.dimensions[0] != first {
                return Err(invalid("matrix blocks must share the first axis"));
            }
            let cols = dims
                .get(descriptor.dimensions[1])
                .ok_or_else(|| invalid("missing second dimension"))?;
            if cols == 0 || descriptor.slot_stride == 0 {
                return Err(invalid("zero column extent or cell stride"));
            }
            let cells = mul(options.rows_per_chunk, cols)?;
            let bitmap = bytes;
            bytes = align(add(bytes, cells.div_ceil(8))?, options.page_bytes)?;
            let slots = bytes;
            bytes = align(
                add(bytes, mul(cells, descriptor.slot_stride)?)?,
                options.page_bytes,
            )?;
            blocks.push(Block {
                id: descriptor.block_id,
                cols,
                stride: descriptor.slot_stride,
                bitmap,
                slots,
            });
            for v in [u64::from(descriptor.block_id), cols, descriptor.slot_stride] {
                h = (h ^ v).wrapping_mul(0x100000001b3);
            }
        }
        h = (h ^ fixed_rows.unwrap_or(u64::MAX)).wrapping_mul(0x100000001b3);
        h = (h ^ options.rows_per_chunk).wrapping_mul(0x100000001b3);
        h = (h ^ options.page_bytes).wrapping_mul(0x100000001b3);
        let chunk_bytes = bytes;
        let bytes = options.page_bytes;
        let stride = add(SLOT_HEADER, bytes)?;
        let limit = spec
            .read_limits
            .require(ReadLimitKey::MaterializedBytes)?
            .unwrap_or(u64::MAX);
        let cell = spec
            .matrix_blocks
            .iter()
            .map(|b| b.slot_stride)
            .max()
            .unwrap_or(0);
        let overhead = add(stride, add(cell, 512)?)?;
        let stage_budget = (256 * 1024)
            .min(options.max_chunk_bytes)
            .min(limit.saturating_sub(overhead) / 2);
        let stage_bytes = mul((stage_budget / stride).max(1), stride)?;
        let available = limit.saturating_sub(add(overhead, stage_bytes)?);
        if available < bytes + 256 {
            return Err(MatrixReuseError::Limit("minimum page working set"));
        }
        let cache_capacity = (options.max_chunk_bytes.min(available) / (bytes + 256)).max(1);
        usize::try_from(cache_capacity)
            .map_err(|_| MatrixReuseError::Limit("writer cache address space"))?;
        Ok(Self {
            spec,
            options,
            blocks,
            bytes,
            chunk_bytes,
            cache_capacity,
            material_limit: limit,
            stage_bytes,
            stride,
            fixed_rows,
            hash: h,
        })
    }
    fn block<T: StratavaMatrixBlock>(&self) -> Result<&Block> {
        crate::matrix::ensure_matrix_block::<T>(self.spec)?;
        self.blocks
            .iter()
            .find(|b| b.id == T::ID)
            .ok_or_else(|| invalid("block absent"))
    }
    fn locate(&self, b: &Block, key: MatrixKey) -> Result<(u64, u64)> {
        if key.ch >= b.cols || self.fixed_rows.is_some_and(|n| key.scan >= n) {
            return Err(invalid("matrix key out of bounds"));
        }
        Ok((
            key.scan / self.options.rows_per_chunk,
            add(mul(key.scan % self.options.rows_per_chunk, b.cols)?, key.ch)?,
        ))
    }
    fn at(&self, id: u64, slot: u64) -> Result<u64> {
        add(HEADER, mul(add(mul(id, 2)?, slot)?, self.stride)?)
    }
    fn stage_at(&self, id: u64) -> Result<u64> {
        add(PAGE, mul(id, self.stride)?)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Head {
    generation: u64,
    rows: u64,
    end: u64,
}
fn head_bytes(h: Head, nonce: [u8; 16]) -> [u8; PAGE as usize] {
    let mut b = [0; PAGE as usize];
    b[..8].copy_from_slice(HEAD_MAGIC);
    b[8..24].copy_from_slice(&nonce);
    put(&mut b, 24, h.generation);
    put(&mut b, 32, h.rows);
    put(&mut b, 40, h.end);
    seal(&mut b);
    b
}
fn read_head(file: &File, nonce: [u8; 16]) -> Result<Head> {
    let mut found = None;
    for slot in 0..2 {
        let mut b = [0; PAGE as usize];
        read_at(file, PAGE * (slot + 1), &mut b)?;
        if &b[..8] == HEAD_MAGIC && b[8..24] == nonce && valid(&b) {
            let h = Head {
                generation: u64_at(&b, 24),
                rows: u64_at(&b, 32),
                end: u64_at(&b, 40),
            };
            if found.is_none_or(|old: Head| h.generation > old.generation) {
                found = Some(h);
            }
        }
    }
    let h = found.ok_or_else(|| invalid("no valid publication head"))?;
    if h.generation == u64::MAX || h.end < HEADER || file.metadata()?.len() < h.end {
        return Err(invalid("truncated confirmed extent"));
    }
    Ok(h)
}
fn read_slot(file: &File, at: u64, id: u64) -> Result<Option<(u64, u32, u64)>> {
    let mut b = [0; SLOT_HEADER as usize];
    match read_at(file, at, &mut b) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        r => r?,
    };
    parse_slot(&b, id)
}
fn parse_slot(b: &[u8], id: u64) -> Result<Option<(u64, u32, u64)>> {
    if b.iter().all(|v| *v == 0) {
        return Ok(None);
    }
    if &b[..8] != SLOT_MAGIC || !valid(b) || u64_at(b, 16) != id {
        return Err(invalid("slot header checksum or identity"));
    }
    Ok(Some((
        u64_at(b, 8),
        u32::from_le_bytes(b[24..28].try_into().unwrap()),
        u64_at(b, 32),
    )))
}
// Only a torn/absent header uses this slow, bounded-memory path. A durable
// intent contains the old committed header, so readers can still serve `base`
// while retirement metadata is being changed or the writer has crashed.
#[allow(clippy::type_complexity)]
fn original_slot(
    journal: &File,
    nonce: [u8; 16],
    h: Head,
    id: u64,
    slot: u64,
) -> Result<Option<Option<(u64, u32, u64)>>> {
    let mut b = [0; PAGE as usize];
    read_at(journal, 0, &mut b)?;
    if &b[..8] != INTENT_MAGIC || b[8..24] != nonce || !valid(&b) || u64_at(&b, 24) != h.generation
    {
        return Ok(None);
    }
    let count = u64_at(&b, 40);
    if journal.metadata()?.len() != add(PAGE, mul(count, JOURNAL_ENTRY)?)? {
        return Ok(None);
    }
    for i in 0..count {
        let mut entry = [0; JOURNAL_ENTRY as usize];
        read_at(journal, add(PAGE, mul(i, JOURNAL_ENTRY)?)?, &mut entry)?;
        if !valid(&entry) {
            return Ok(None);
        }
        if u64_at(&entry, 0) != id {
            continue;
        }
        let mut again = [0; PAGE as usize];
        read_at(journal, 0, &mut again)?;
        if b != again {
            return Ok(None);
        }
        if slot == u64_at(&entry, 8) {
            return Ok(Some(None));
        }
        let old = &entry[16..80];
        if old.iter().all(|b| *b == 0) {
            return Ok(Some(None));
        }
        if &old[..8] != SLOT_MAGIC || !valid(old) || u64_at(old, 16) != id {
            return Err(invalid("undo slot header"));
        }
        return Ok(Some(Some((
            u64_at(old, 8),
            u32::from_le_bytes(old[24..28].try_into().unwrap()),
            u64_at(old, 32),
        ))));
    }
    Ok(None)
}
fn slot_header(id: u64, generation: u64, payload: &[u8]) -> [u8; SLOT_HEADER as usize] {
    let mut b = [0; SLOT_HEADER as usize];
    b[..8].copy_from_slice(SLOT_MAGIC);
    put(&mut b, 8, generation);
    put(&mut b, 16, id);
    b[24..28].copy_from_slice(&crc32fast::hash(payload).to_le_bytes());
    put(&mut b, 32, u64::MAX);
    seal(&mut b);
    b
}
fn selected(file: &File, g: &Geometry, h: Head, id: u64) -> Result<Option<(u64, u32)>> {
    selected_with_undo(file, g, h, id, None)
}
fn selected_with_undo(
    file: &File,
    g: &Geometry,
    h: Head,
    id: u64,
    undo: Option<(&File, [u8; 16])>,
) -> Result<Option<(u64, u32)>> {
    let mut result = None;
    let mut retired = false;
    for slot in 0..2 {
        let read = read_slot(file, g.at(id, slot)?, id);
        let entry = match read {
            Ok(Some(v)) => Some(v),
            other => {
                if let Some((journal, nonce)) = undo {
                    match original_slot(journal, nonce, h, id, slot)? {
                        Some(v) => v,
                        None => other?,
                    }
                } else {
                    other?
                }
            }
        };
        if let Some((generation, crc, until)) = entry {
            retired |= until <= h.generation;
            if generation > 0
                && generation <= h.generation
                && h.generation < until
                && result.is_none_or(|(_, _, old)| generation > old)
            {
                result = Some((slot, crc, generation));
            }
        }
    }
    if result.is_none() && retired {
        return Err(invalid("confirmed slot missing; predecessor is retired"));
    }
    Ok(result.map(|(slot, crc, _)| (slot, crc)))
}
type PageTarget = (u64, [u8; SLOT_HEADER as usize]);
fn payload(file: &File, g: &Geometry, h: Head, id: u64) -> Result<(Vec<u8>, PageTarget)> {
    let mut bytes = alloc(g.bytes)?;
    let mut original = [0; SLOT_HEADER as usize];
    let target = if let Some((slot, crc)) = selected(file, g, h, id)? {
        let at = g.at(id, slot)?;
        read_at(file, at, &mut original)?;
        read_at(file, add(at, SLOT_HEADER)?, &mut bytes)?;
        if crc32fast::hash(&bytes) != crc {
            return Err(invalid("page payload checksum"));
        }
        1 - slot
    } else {
        0
    };
    Ok((bytes, (target, original)))
}
fn metadata(g: &Geometry, nonce: [u8; 16], path: &Path) -> [u8; PAGE as usize] {
    let mut b = [0; PAGE as usize];
    b[..8].copy_from_slice(MAGIC);
    b[8..24].copy_from_slice(&nonce);
    put(&mut b, 24, g.hash);
    put(&mut b, 32, g.bytes);
    put(&mut b, 40, g.options.rows_per_chunk);
    put(&mut b, 48, path_hash(path));
    put(&mut b, 56, g.chunk_bytes);
    seal(&mut b);
    b
}
fn inspect(file: &File, g: &Geometry, path: &Path) -> Result<[u8; 16]> {
    let mut b = [0; PAGE as usize];
    read_at(file, 0, &mut b)?;
    let nonce = b[8..24].try_into().unwrap();
    if b != metadata(g, nonce, path) {
        return Err(invalid(
            "reuse mode, schema, geometry, pathname or metadata checksum mismatch",
        ));
    }
    Ok(nonce)
}
fn sidecar(file: &File, nonce: [u8; 16]) -> Result<()> {
    let mut b = [0; PAGE as usize];
    read_at(file, 0, &mut b)?;
    if b != sidecar_header(nonce) {
        return Err(invalid("sidecar identity"));
    }
    Ok(())
}
fn sidecar_header(nonce: [u8; 16]) -> [u8; PAGE as usize] {
    let mut b = [0; PAGE as usize];
    b[..8].copy_from_slice(MAGIC);
    b[8..24].copy_from_slice(&nonce);
    seal(&mut b);
    b
}

/// Writer for either ordinary storage (`reuse=false`) or experimental reuse.
/// Dropping a reuse writer discards un-synced changes; `sync` is the publication boundary.
pub struct MatrixWriter {
    inner: WriterInner,
    spec: FormatSpec,
    dims: MatrixDimensions,
    options: MatrixOptions,
    path: PathBuf,
}
enum WriterInner {
    Native(Box<StratavaWriter>),
    Reuse(Box<ReuseWriter>),
}
struct OpenPage {
    bytes: Vec<u8>,
    target: Option<(u64, [u8; 64])>,
    logged: bool,
    dirty: bool,
}
struct LockedFile(File);
impl std::ops::Deref for LockedFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}
impl LockedFile {
    fn new(file: File) -> Result<Self> {
        // Reuse the native writer admission: on Windows a whole-file lock is
        // mandatory and would block readers. The existing guard locks only a
        // reserved byte outside every data extent on that platform.
        match crate::file::try_lock_native_guard(&file) {
            Ok(()) => Ok(Self(file)),
            Err(crate::file::WriterGuardLockError::WouldBlock) => {
                Err(io::Error::from(io::ErrorKind::WouldBlock).into())
            }
            Err(crate::file::WriterGuardLockError::Io(error)) => Err(error.into()),
        }
    }
}
impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = crate::file::unlock_native_guard(&self.0);
    }
}
struct ReuseWriter {
    file: LockedFile,
    stage: File,
    journal: File,
    geometry: Geometry,
    nonce: [u8; 16],
    head: Head,
    rows: u64,
    count: u64,
    pages: BTreeMap<u64, OpenPage>,
    eviction: VecDeque<u64>,
    hot: [Option<(u64, OpenPage)>; 2],
    hot_last: usize,
    poisoned: bool,
    publishing: Option<u64>,
    notify: Arc<AtomicU64>,
}
impl MatrixWriter {
    /// Creates a new file; the runtime option selects its on-disk format.
    pub fn create(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        dims: MatrixDimensions,
        options: MatrixOptions,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let inner = if !options.reuse {
            WriterInner::Native(Box::new(spec.create_writer_with_dims(&path, dims.clone())?))
        } else {
            let geometry = Geometry::new(spec, &dims, options)?;
            let file = LockedFile::new(File::create_new(&path)?)?;
            let path = path.canonicalize()?;
            let nonce = crate::file::fresh_stream_creation_nonce();
            let stage = File::create_new(companion(&path, ".reuse-work"))?;
            let journal = File::create_new(companion(&path, ".reuse-journal"))?;
            let head = Head {
                generation: 0,
                rows: 0,
                end: HEADER,
            };
            write_at(&file, 0, &metadata(&geometry, nonce, &path))?;
            write_at(&file, PAGE, &head_bytes(head, nonce))?;
            write_at(&file, 2 * PAGE, &head_bytes(head, nonce))?;
            for f in [&stage, &journal] {
                write_at(f, 0, &sidecar_header(nonce))?;
                f.sync_all()?;
            }
            file.sync_all()?;
            crate::file::sync_parent_directory(&path)?;
            WriterInner::Reuse(Box::new(ReuseWriter {
                file,
                stage,
                journal,
                geometry,
                nonce,
                head,
                rows: 0,
                count: 0,
                pages: BTreeMap::new(),
                eviction: VecDeque::new(),
                hot: [None, None],
                hot_last: 0,
                poisoned: false,
                publishing: None,
                notify: Arc::new(AtomicU64::new(0)),
            }))
        };
        Ok(Self {
            inner,
            spec,
            dims,
            options,
            path: path.canonicalize()?,
        })
    }
    /// Opens a writer and rolls back an interrupted reuse transaction before returning.
    pub fn open(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        dims: MatrixDimensions,
        options: MatrixOptions,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let inner = if !options.reuse {
            WriterInner::Native(Box::new(spec.open_writer(&path)?))
        } else {
            let geometry = Geometry::new(spec, &dims, options)?;
            let path = path.canonicalize()?;
            let file = LockedFile::new(OpenOptions::new().read(true).write(true).open(&path)?)?;
            let nonce = inspect(&file, &geometry, &path)?;
            let head = read_head(&file, nonce)?;
            let stage = open_sidecar(&companion(&path, ".reuse-work"))?;
            let journal = open_sidecar(&companion(&path, ".reuse-journal"))?;
            sidecar(&stage, nonce)?;
            let mut w = ReuseWriter {
                file,
                stage,
                journal,
                geometry,
                nonce,
                head,
                rows: head.rows,
                count: 0,
                pages: BTreeMap::new(),
                eviction: VecDeque::new(),
                hot: [None, None],
                hot_last: 0,
                poisoned: false,
                publishing: None,
                notify: Arc::new(AtomicU64::new(head.generation)),
            };
            w.recover()?;
            WriterInner::Reuse(Box::new(w))
        };
        Ok(Self {
            inner,
            spec,
            dims,
            options,
            path: path.canonicalize()?,
        })
    }
    /// Opens an independent reader. Reuse readers share only a generation notification
    /// with this writer, never their caches. Separately opened/process readers use the
    /// authoritative disk heads and have the same retry correctness contract.
    pub fn reader(&self) -> Result<MatrixReader> {
        let mut r = MatrixReader::open(self.spec, &self.path, self.dims.clone(), self.options)?;
        if let (WriterInner::Reuse(w), ReaderInner::Reuse(v)) = (&self.inner, &mut r.inner) {
            if v.nonce != w.nonce {
                return Err(invalid("writer pathname was replaced"));
            }
            v.notify = Some(w.notify.clone());
        }
        Ok(r)
    }
    /// Writes and marks one cell present in the next publication.
    pub fn put<T: StratavaMatrixBlock>(&mut self, key: MatrixKey, value: &T) -> Result<()> {
        match &mut self.inner {
            WriterInner::Native(w) => {
                w.write_matrix_cell(key, value)?;
                w.commit_matrix_cell::<T>(key)?;
            }
            WriterInner::Reuse(w) => w.put(key, value)?,
        }
        if T::IMMEDIATE || value.immediate_if() {
            self.sync()?;
        }
        Ok(())
    }
    /// Clears one cell in the next publication.
    pub fn delete<T: StratavaMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        match &mut self.inner {
            WriterInner::Native(w) => w.clear_matrix_cell::<T>(key)?,
            WriterInner::Reuse(w) => w.delete::<T>(key)?,
        }
        if T::IMMEDIATE {
            self.sync()?;
        }
        Ok(())
    }
    /// Durably publishes the batch. Reuse never waits for readers.
    /// An error after publication starts is indeterminate: reopen and inspect; do
    /// not assume rollback or blindly replay non-idempotent application actions.
    pub fn sync(&mut self) -> Result<()> {
        match &mut self.inner {
            WriterInner::Native(w) => Ok(w.sync()?),
            WriterInner::Reuse(w) => {
                if w.poisoned {
                    return Err(MatrixReuseError::Poisoned);
                }
                w.poisoned = true;
                match w.publish() {
                    Ok(()) => {
                        w.poisoned = false;
                        w.publishing = None;
                        Ok(())
                    }
                    Err(source) => match w.publishing {
                        Some(generation) if w.head.generation == generation => {
                            Err(MatrixReuseError::Published {
                                generation,
                                source: Box::new(source),
                            })
                        }
                        Some(generation) => Err(MatrixReuseError::PublicationIndeterminate {
                            generation,
                            source: Box::new(source),
                        }),
                        None => Err(source),
                    },
                }
            }
        }
    }
}
impl ReuseWriter {
    fn next(&self) -> Result<u64> {
        self.head
            .generation
            .checked_add(1)
            .filter(|next| *next < u64::MAX)
            .ok_or_else(|| invalid("generation exhausted"))
    }
    fn flush_page(&mut self, id: u64, c: &mut OpenPage) -> Result<()> {
        if !c.dirty {
            return Ok(());
        }
        let generation = self.next()?;
        let at = self.geometry.stage_at(id)?;
        self.log_page(id, c)?;
        write_at(&self.stage, add(at, SLOT_HEADER)?, &c.bytes)?;
        write_at(&self.stage, at, &slot_header(id, generation, &c.bytes))?;
        c.logged = true;
        c.dirty = false;
        Ok(())
    }
    fn log_page(&mut self, id: u64, c: &mut OpenPage) -> Result<()> {
        if !c.logged {
            let (target, original) = c
                .target
                .ok_or_else(|| invalid("unlogged page lacks original header"))?;
            let mut entry = [0; JOURNAL_ENTRY as usize];
            put(&mut entry, 0, id);
            put(&mut entry, 8, target);
            entry[16..80].copy_from_slice(&original);
            seal(&mut entry);
            write_at(
                &self.journal,
                add(PAGE, mul(self.count, JOURNAL_ENTRY)?)?,
                &entry,
            )?;
            self.count = add(self.count, 1)?;
        }
        c.logged = true;
        Ok(())
    }
    fn flush_pages(&mut self) -> Result<()> {
        for hot in &mut self.hot {
            if let Some((id, page)) = hot.take() {
                self.pages.insert(id, page);
            }
        }
        if !self.pages.values().any(|c| c.dirty) {
            return Ok(());
        }
        let mut pages = std::mem::take(&mut self.pages);
        let result = (|| {
            // Contiguous dirty pages share a stage write. The primary retains
            // per-page publication/retirement headers for independent reuse.
            let per_run = self.geometry.stage_bytes / self.geometry.stride;
            let mut buffer = alloc(mul(per_run, self.geometry.stride)?)?;
            let mut first = 0;
            let mut count = 0;
            for (&id, c) in &mut pages {
                if !c.dirty {
                    continue;
                }
                if count > 0 && (id != first + count || count == per_run) {
                    write_at(
                        &self.stage,
                        self.geometry.stage_at(first)?,
                        &buffer[..(count * self.geometry.stride) as usize],
                    )?;
                    count = 0;
                }
                if count == 0 {
                    first = id;
                }
                // Logging is independent from stage layout and remains one entry
                // per dirty page even if a page was evicted/reloaded repeatedly.
                self.log_page(id, c)?;
                let offset = (count * self.geometry.stride) as usize;
                buffer[offset..offset + SLOT_HEADER as usize].copy_from_slice(&slot_header(
                    id,
                    self.next()?,
                    &c.bytes,
                ));
                buffer[offset + SLOT_HEADER as usize..offset + self.geometry.stride as usize]
                    .copy_from_slice(&c.bytes);
                count += 1;
                c.dirty = false;
            }
            if count > 0 {
                write_at(
                    &self.stage,
                    self.geometry.stage_at(first)?,
                    &buffer[..(count * self.geometry.stride) as usize],
                )?;
            }
            Ok(())
        })();
        self.pages = pages;
        result
    }
    fn load(&mut self, id: u64) -> Result<&mut OpenPage> {
        if self.poisoned {
            return Err(MatrixReuseError::Poisoned);
        }
        // A sequential cell stream alternates between its data and bitmap page.
        // Keep those two owner-local pages directly accessible, avoiding tree
        // lookups for every cell. Both count toward the same bounded cache.
        if let Some(index) = self
            .hot
            .iter()
            .position(|p| p.as_ref().is_some_and(|p| p.0 == id))
        {
            self.hot_last = index;
            return Ok(&mut self.hot[index].as_mut().unwrap().1);
        }
        let index = self
            .hot
            .iter()
            .position(Option::is_none)
            .unwrap_or(1 - self.hot_last);
        if let Some((old_id, page)) = self.hot[index].take() {
            self.pages.insert(old_id, page);
        }
        let page = if let Some(page) = self.pages.remove(&id) {
            page
        } else {
            self.poisoned = true;
            let count = self.pages.len() + self.hot.iter().filter(|p| p.is_some()).count();
            if count as u64 >= self.geometry.cache_capacity {
                let old_id = self.eviction.pop_front().unwrap();
                let mut old = if let Some(page) = self.pages.remove(&old_id) {
                    page
                } else {
                    let old_index = self
                        .hot
                        .iter()
                        .position(|p| p.as_ref().is_some_and(|p| p.0 == old_id))
                        .unwrap();
                    self.hot[old_index].take().unwrap().1
                };
                self.flush_page(old_id, &mut old)?;
            }
            let at = self.geometry.stage_at(id)?;
            let (bytes, logged, target) =
                if let Some((generation, crc, _)) = read_slot(&self.stage, at, id)? {
                    if generation != self.next()? {
                        return Err(invalid("staging generation"));
                    }
                    let mut bytes = alloc(self.geometry.bytes)?;
                    read_at(&self.stage, add(at, SLOT_HEADER)?, &mut bytes)?;
                    if crc32fast::hash(&bytes) != crc {
                        return Err(invalid("staging checksum"));
                    }
                    (bytes, true, None)
                } else {
                    let (bytes, target) = payload(&self.file, &self.geometry, self.head, id)?;
                    (bytes, false, Some(target))
                };
            self.eviction.push_back(id);
            self.poisoned = false;
            OpenPage {
                bytes,
                target,
                logged,
                dirty: false,
            }
        };
        self.hot[index] = Some((id, page));
        self.hot_last = index;
        Ok(&mut self.hot[index].as_mut().unwrap().1)
    }
    fn put<T: StratavaMatrixBlock>(&mut self, key: MatrixKey, value: &T) -> Result<()> {
        if self.poisoned {
            return Err(MatrixReuseError::Poisoned);
        }
        let b = self.geometry.block::<T>()?.clone();
        let (chunk, cell) = self.geometry.locate(&b, key)?;
        let row_end = add(key.scan, 1)?;
        let base = mul(chunk, self.geometry.chunk_bytes)?;
        let start = add(base, add(b.slots, mul(cell, b.stride)?)?)?;
        let last = add(start, b.stride - 1)? / self.geometry.bytes;
        self.geometry.spec.read_limits.check(
            ReadLimitKey::FileLen,
            add(self.geometry.at(last, 1)?, self.geometry.stride)?,
        )?;
        let encoded = crate::codec::encode_to_vec_limited(
            value,
            T::ENDIAN.unwrap_or(self.geometry.spec.endian),
            add(b.stride, 1)?,
            "experimental matrix cell",
        )?;
        if encoded.len() as u64 != b.stride {
            return Err(invalid("cell encoding length"));
        }
        let mut offset = start;
        let mut rest = encoded.as_slice();
        while !rest.is_empty() {
            let page_bytes = self.geometry.bytes;
            let local = (offset % page_bytes) as usize;
            let take = rest.len().min(page_bytes as usize - local);
            let page = self.load(offset / page_bytes)?;
            if page.bytes[local..local + take] != rest[..take] {
                page.bytes[local..local + take].copy_from_slice(&rest[..take]);
                page.dirty = true;
            }
            offset += take as u64;
            rest = &rest[take..];
        }
        let bitmap = add(base, add(b.bitmap, cell / 8)?)?;
        let local = (bitmap % self.geometry.bytes) as usize;
        let page = self.load(bitmap / self.geometry.bytes)?;
        let bit = 1 << (cell % 8);
        if page.bytes[local] & bit == 0 {
            page.bytes[local] |= bit;
            page.dirty = true;
        }
        self.rows = self.rows.max(row_end);
        Ok(())
    }
    fn delete<T: StratavaMatrixBlock>(&mut self, key: MatrixKey) -> Result<()> {
        if self.poisoned {
            return Err(MatrixReuseError::Poisoned);
        }
        let b = self.geometry.block::<T>()?.clone();
        let (chunk, cell) = self.geometry.locate(&b, key)?;
        if key.scan >= self.rows {
            return Err(MatrixReuseError::Missing);
        }
        let at = add(
            mul(chunk, self.geometry.chunk_bytes)?,
            add(b.bitmap, cell / 8)?,
        )?;
        let local = (at % self.geometry.bytes) as usize;
        let page = self.load(at / self.geometry.bytes)?;
        let bit = 1 << (cell % 8);
        if page.bytes[local] & bit != 0 {
            page.bytes[local] &= !bit;
            page.dirty = true;
        }
        Ok(())
    }
    fn entry(&self, i: u64) -> Result<(u64, u64, [u8; 64])> {
        let mut b = [0; JOURNAL_ENTRY as usize];
        read_at(&self.journal, add(PAGE, mul(i, JOURNAL_ENTRY)?)?, &mut b)?;
        if !valid(&b) || u64_at(&b, 8) > 1 {
            return Err(invalid("recovery entry checksum or slot"));
        }
        let old: [u8; 64] = b[16..80].try_into().unwrap();
        if old.iter().any(|b| *b != 0)
            && (!valid(&old) || &old[..8] != SLOT_MAGIC || u64_at(&old, 16) != u64_at(&b, 0))
        {
            return Err(invalid("recovery original header"));
        }
        Ok((u64_at(&b, 0), u64_at(&b, 8), old))
    }
    fn reset(&mut self) -> Result<()> {
        // The complete old intent stays durable until every rollback write (or
        // the new publication) is durable. A reset never precedes that barrier.
        write_at(&self.journal, 0, &sidecar_header(self.nonce))?;
        self.journal.sync_all()?;
        self.journal.set_len(PAGE)?;
        self.stage.set_len(PAGE)?;
        self.count = 0;
        for c in self.pages.values_mut() {
            c.logged = false;
            c.dirty = false;
        }
        Ok(())
    }
    fn recover(&mut self) -> Result<()> {
        let mut b = [0; PAGE as usize];
        read_at(&self.journal, 0, &mut b)?;
        if b[..8] == *MAGIC {
            sidecar(&self.journal, self.nonce)?;
            self.reset()?;
            return Ok(());
        }
        if &b[..8] != INTENT_MAGIC || b[8..24] != self.nonce || !valid(&b) {
            return Err(invalid("recovery intent is corrupt; file left untouched"));
        }
        let base = u64_at(&b, 24);
        let generation = u64_at(&b, 32);
        let count = u64_at(&b, 40);
        if generation != add(base, 1)?
            || self.head.generation != base && self.head.generation != generation
            || self.journal.metadata()?.len() != add(PAGE, mul(count, JOURNAL_ENTRY)?)?
        {
            return Err(invalid("recovery intent extent or generation"));
        }
        // Validate the full journal before the first repair. No unbounded map.
        for i in 0..count {
            let (id, slot, old) = self.entry(i)?;
            self.geometry.at(id, slot)?;
            if old.iter().any(|b| *b != 0) && u64_at(&old, 8) > base {
                return Err(invalid("recovery original generation"));
            }
        }
        if self.head.generation == base {
            for i in 0..count {
                let (id, slot, old) = self.entry(i)?;
                let at = self.geometry.at(id, slot)?;
                // The journal names only the slot that was inactive at `base`.
                if at < self.file.metadata()?.len() {
                    let mut discarded = [0; SLOT_HEADER as usize];
                    if old.iter().any(|b| *b != 0) {
                        discarded = slot_header(id, 0, &[]);
                        put(&mut discarded, 32, base);
                        seal(&mut discarded);
                    }
                    write_at(&self.file, at, &discarded)?;
                }
                let old_at = self.geometry.at(id, 1 - slot)?;
                if old_at < self.file.metadata()?.len() {
                    write_at(&self.file, old_at, &old)?;
                }
            }
            self.file.sync_all()?;
        }
        self.reset()
    }
    fn publish(&mut self) -> Result<()> {
        self.flush_pages()?;
        if self.count == 0 {
            return Ok(());
        }
        let next = self.next()?;
        let mut intent = [0; PAGE as usize];
        intent[..8].copy_from_slice(INTENT_MAGIC);
        intent[8..24].copy_from_slice(&self.nonce);
        put(&mut intent, 24, self.head.generation);
        put(&mut intent, 32, next);
        put(&mut intent, 40, self.count);
        seal(&mut intent);
        write_at(&self.journal, 0, &intent)?;
        self.journal.sync_all()?;
        fault("matrix_reuse.intent_durable")?;
        let mut buffer = alloc(self.geometry.bytes)?;
        for i in 0..self.count {
            let (id, slot, old) = self.entry(i)?;
            if let Some(page) = self.pages.get(&id) {
                buffer.copy_from_slice(&page.bytes);
            } else {
                let stage_at = self.geometry.stage_at(id)?;
                let (generation, crc, _) = read_slot(&self.stage, stage_at, id)?
                    .ok_or_else(|| invalid("staging slot missing"))?;
                if generation != next {
                    return Err(invalid("staging generation"));
                }
                read_at(&self.stage, add(stage_at, SLOT_HEADER)?, &mut buffer)?;
                if crc32fast::hash(&buffer) != crc {
                    return Err(invalid("staging checksum"));
                }
            }
            let at = self.geometry.at(id, slot)?;
            // Current confirmed payload remains in the other slot. Old readers
            // that still refer to this one must fail the generation recheck.
            write_at(&self.file, add(at, SLOT_HEADER)?, &buffer)?;
            fault("matrix_reuse.payload_written")?;
            let new_header = slot_header(id, next, &buffer);
            write_at(&self.file, at, &new_header)?;
            if let Some(page) = self.pages.get_mut(&id) {
                page.target = Some((1 - slot, new_header));
            }
            fault("matrix_reuse.slot_written")?;
            // A retirement fence prevents a lost/zeroed newest header from
            // silently reviving the previous value. Its exact old bytes are
            // protected by the durable undo journal, including first allocation.
            let mut retired = if old.iter().all(|b| *b == 0) {
                slot_header(id, 0, &[])
            } else {
                old
            };
            put(&mut retired, 32, next);
            seal(&mut retired);
            let old_at = self.geometry.at(id, 1 - slot)?;
            write_at(&self.file, old_at, &retired[..40])?;
            fault("matrix_reuse.retirement_prefix")?;
            write_at(&self.file, old_at + 40, &retired[40..])?;
            fault("matrix_reuse.retirement_written")?;
        }
        self.file.sync_all()?;
        fault("matrix_reuse.data_durable")?;
        let head = Head {
            generation: next,
            rows: self.rows,
            end: self.file.metadata()?.len(),
        };
        self.publishing = Some(next);
        let head_bytes = head_bytes(head, self.nonce);
        let head_at = PAGE * (1 + next % 2);
        write_at(&self.file, head_at, &head_bytes[..64])?;
        fault("matrix_reuse.head_prefix")?;
        write_at(&self.file, head_at + 64, &head_bytes[64..])?;
        fault("matrix_reuse.head_written")?;
        self.file.sync_all()?;
        self.head = head;
        fault("matrix_reuse.head_durable")?;
        self.notify.store(next, Ordering::Release);
        self.reset()?;
        fault("matrix_reuse.reset")?;
        Ok(())
    }
}

/// Independent `Send + !Sync` reader. Reuse reads automatically follow, returning
/// only caller-owned values. A successful range call is from one generation.
///
/// ```compile_fail
/// use stratava_core::experimental_matrix::MatrixReader;
/// fn require_sync<T: Sync>() {}
/// require_sync::<MatrixReader>();
/// ```
pub struct MatrixReader {
    inner: ReaderInner,
    _not_sync: Cell<()>,
}
enum ReaderInner {
    Native(Box<StratavaReader>),
    Reuse(Box<ReuseReader>),
}
struct ReuseReader {
    file: File,
    journal: File,
    geometry: Geometry,
    nonce: [u8; 16],
    notify: Option<Arc<AtomicU64>>,
    cache: RefCell<Option<CachedPages>>,
}
struct CachedPages {
    generation: u64,
    first: u64,
    count: u64,
    bytes: Vec<u8>,
}
impl MatrixReader {
    /// Opens without taking writer admission. An option/format mismatch is refused.
    pub fn open(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        dims: MatrixDimensions,
        options: MatrixOptions,
    ) -> Result<Self> {
        let inner = if !options.reuse {
            ReaderInner::Native(Box::new(spec.open_reader(path)?))
        } else {
            let geometry = Geometry::new(spec, &dims, options)?;
            let path = path.as_ref().canonicalize()?;
            let file = File::open(&path)?;
            let nonce = inspect(&file, &geometry, &path)?;
            geometry
                .spec
                .read_limits
                .check(ReadLimitKey::FileLen, file.metadata()?.len())?;
            read_head(&file, nonce)?;
            ReaderInner::Reuse(Box::new(ReuseReader {
                journal: File::open(companion(&path, ".reuse-journal"))?,
                file,
                geometry,
                nonce,
                notify: None,
                cache: RefCell::new(None),
            }))
        };
        Ok(Self {
            inner,
            _not_sync: Cell::new(()),
        })
    }
    /// Reads one cell; reuse readers retry automatically when its generation changes.
    pub fn get<T: StratavaMatrixBlock>(&self, key: MatrixKey) -> Result<T> {
        match &self.inner {
            ReaderInner::Native(r) => Ok(r.read_matrix_cell(key)?),
            ReaderInner::Reuse(r) => {
                let b = r.geometry.block::<T>()?;
                let bytes = r.read(b, key, 1)?;
                Ok(Decoder::decode_from_slice_limited(
                    &bytes,
                    T::ENDIAN.unwrap_or(r.geometry.spec.endian),
                    r.decode_budget(bytes.len() as u64)?,
                )?)
            }
        }
    }
    /// Copies a contiguous range in declaration order. On a decode error discard
    /// the output. Reuse validates one generation for the complete range.
    pub fn read_run_into<T: StratavaMatrixBlock>(
        &self,
        start: MatrixKey,
        out: &mut [T],
    ) -> Result<()> {
        match &self.inner {
            ReaderInner::Native(r) => Ok(r.read_matrix_run_into(start, out)?),
            ReaderInner::Reuse(r) => {
                let b = r.geometry.block::<T>()?;
                let bytes = r.read(b, start, out.len() as u64)?;
                let output = mul(out.len() as u64, std::mem::size_of::<T>() as u64)?;
                let mut budget = r.decode_budget(add(bytes.len() as u64, output)?)?;
                for (dest, part) in out.iter_mut().zip(bytes.chunks(b.stride as usize)) {
                    *dest = Decoder::decode_from_slice_accounted(
                        part,
                        T::ENDIAN.unwrap_or(r.geometry.spec.endian),
                        &mut budget,
                    )?;
                }
                Ok(())
            }
        }
    }
    /// Explicit follow remains available when reuse is disabled. With reuse it
    /// returns the disk publication number; ordinary reads already auto-follow.
    pub fn follow(&mut self) -> Result<u64> {
        match &mut self.inner {
            ReaderInner::Native(r) => Ok(r.follow()?),
            ReaderInner::Reuse(r) => Ok(read_head(&r.file, r.nonce)?.generation),
        }
    }
}
impl ReuseReader {
    fn window_bytes(&self) -> u64 {
        let pair = self.geometry.stride * 2;
        // No retention still uses one pair as scratch. Bulk reads coalesce up
        // to 256 KiB, independent of logical chunk/file size.
        let limit = self.geometry.material_limit;
        self.geometry
            .options
            .cache_bytes
            .min(limit / 4)
            .clamp(pair, (256 * 1024).max(pair))
    }
    fn copy_bytes(&self, h: Head, mut at: u64, mut out: &mut [u8]) -> Result<()> {
        let g = &self.geometry;
        while !out.is_empty() {
            let id = at / g.bytes;
            let local = (at % g.bytes) as usize;
            let take = out.len().min(g.bytes as usize - local);
            let mut cache = self.cache.borrow_mut();
            if !cache.as_ref().is_some_and(|c| {
                c.generation == h.generation && c.first <= id && id - c.first < c.count
            }) {
                *cache = None;
                let wanted = (local as u64 + out.len() as u64).div_ceil(g.bytes);
                let count = wanted.min(self.window_bytes() / (2 * g.stride)).max(1);
                let mut bytes = alloc(mul(count, mul(2, g.stride)?)?)?;
                // The last inactive payload may not exist yet. Read up to the
                // captured confirmed extent, leaving never-allocated slots zero.
                let start = g.at(id, 0)?;
                let read = h.end.saturating_sub(start).min(bytes.len() as u64) as usize;
                if read > 0 {
                    read_at(&self.file, start, &mut bytes[..read])?;
                }
                *cache = Some(CachedPages {
                    generation: h.generation,
                    first: id,
                    count,
                    bytes,
                });
            }
            let c = cache.as_ref().unwrap();
            let base = ((id - c.first) * 2 * g.stride) as usize;
            let mut result = None;
            let mut retired = false;
            for slot in 0..2 {
                let offset = base + slot as usize * g.stride as usize;
                let header = &c.bytes[offset..offset + SLOT_HEADER as usize];
                let parsed = parse_slot(header, id);
                let entry = match parsed {
                    Ok(Some(v)) => Some(v),
                    other => match original_slot(&self.journal, self.nonce, h, id, slot)? {
                        Some(v) => v,
                        None => other?,
                    },
                };
                if let Some((generation, crc, until)) = entry {
                    retired |= until <= h.generation;
                    if generation > 0
                        && generation <= h.generation
                        && h.generation < until
                        && result.is_none_or(|(_, _, old)| generation > old)
                    {
                        result = Some((offset + SLOT_HEADER as usize, crc, generation));
                    }
                }
            }
            if let Some((offset, crc, _)) = result {
                let bytes = &c.bytes[offset..offset + g.bytes as usize];
                if crc32fast::hash(bytes) != crc {
                    return Err(invalid("page payload checksum"));
                }
                out[..take].copy_from_slice(&bytes[local..local + take]);
            } else if retired {
                return Err(invalid("confirmed page missing; predecessor is retired"));
            } else {
                out[..take].fill(0);
            }
            // Test-only cut inside the copy, after releasing the private cache borrow.
            // This allows a publisher to reuse both slots before the range completes.
            drop(cache);
            #[cfg(feature = "scalable-fault-injection")]
            fault("matrix_reuse.page_copied")?;
            at += take as u64;
            out = &mut out[take..];
        }
        Ok(())
    }
    fn decode_budget(&self, used: u64) -> Result<u64> {
        let limit = self
            .geometry
            .spec
            .read_limits
            .require(ReadLimitKey::MaterializedBytes)?
            .unwrap_or(u64::MAX);
        // A bounded read window, one scratch bitmap page and an undo payload.
        let used = add(
            used,
            add(self.window_bytes(), mul(self.geometry.bytes, 2)?)?,
        )?;
        limit
            .checked_sub(used)
            .ok_or(MatrixReuseError::Limit("reader working memory"))
    }
    fn read(&self, b: &Block, start: MatrixKey, count: u64) -> Result<Vec<u8>> {
        self.geometry.locate(b, start)?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let first = add(mul(start.scan, b.cols)?, start.ch)?;
        let end = add(first, count)?;
        if let Some(rows) = self.geometry.fixed_rows
            && end > mul(rows, b.cols)?
        {
            return Err(invalid("range exceeds fixed extent"));
        }
        let bytes = mul(count, b.stride)?;
        self.decode_budget(bytes)?;
        let mut out = alloc(bytes)?;
        let attempts = self.geometry.options.read_retries;
        let mut last_error = None;
        for _ in 0..attempts {
            let signal = self.notify.as_ref().map(|s| s.load(Ordering::Acquire));
            let before = read_head(&self.file, self.nonce)?;
            let result = (|| {
                if end > mul(before.rows, b.cols)? {
                    return Err(MatrixReuseError::Missing);
                }
                let cells = mul(self.geometry.options.rows_per_chunk, b.cols)?;
                let mut ordinal = first;
                let mut written = 0usize;
                while ordinal < end {
                    let id = ordinal / cells;
                    let local = ordinal % cells;
                    let take = (end - ordinal).min(cells - local);
                    let base = mul(id, self.geometry.chunk_bytes)?;
                    // Presence is paged independently, so value-only overwrites never
                    // rewrite the bitmap. Check at most one bitmap page at a time.
                    let mut cell = local;
                    while cell < local + take {
                        let bit_at = add(base, add(b.bitmap, cell / 8)?)?;
                        let bit_len = ((local + take).div_ceil(8) - cell / 8)
                            .min(self.geometry.bytes - bit_at % self.geometry.bytes);
                        let mut bits = alloc(bit_len)?;
                        self.copy_bytes(before, bit_at, &mut bits)?;
                        let stop = (local + take).min((cell / 8 + bit_len) * 8);
                        for (i, byte) in bits.iter().enumerate() {
                            let lo = if i == 0 { cell % 8 } else { 0 };
                            let hi = if i + 1 == bits.len() {
                                (stop - 1) % 8 + 1
                            } else {
                                8
                            };
                            let mask = ((1u16 << hi) - (1u16 << lo)) as u8;
                            if byte & mask != mask {
                                return Err(MatrixReuseError::Missing);
                            }
                        }
                        cell = stop;
                    }
                    let at = add(base, add(b.slots, mul(local, b.stride)?)?)?;
                    let len = mul(take, b.stride)? as usize;
                    self.copy_bytes(before, at, &mut out[written..written + len])?;
                    written += len;
                    ordinal += take;
                }
                Ok(())
            })();
            fault("matrix_reuse.read_copied")?;
            let after = read_head(&self.file, self.nonce)?;
            let signal_after = self.notify.as_ref().map(|s| s.load(Ordering::Acquire));
            if before == after && signal == signal_after {
                match result {
                    Ok(()) => {
                        if self.cache.borrow().as_ref().is_some_and(|c| {
                            c.bytes.len() as u64 > self.geometry.options.cache_bytes
                        }) {
                            *self.cache.borrow_mut() = None;
                        }
                        return Ok(out);
                    }
                    Err(MatrixReuseError::Missing) => {
                        if self.cache.borrow().as_ref().is_some_and(|c| {
                            c.bytes.len() as u64 > self.geometry.options.cache_bytes
                        }) {
                            *self.cache.borrow_mut() = None;
                        }
                        return Err(MatrixReuseError::Missing);
                    }
                    Err(e) => last_error = Some(e),
                }
            } else {
                last_error = None;
            }
            *self.cache.borrow_mut() = None;
        }
        if let Some(e) = last_error {
            return Err(e);
        }
        Err(MatrixReuseError::Retry { attempts })
    }
}
