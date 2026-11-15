//! Varlit checkpoint publication. The confirmed and working heads are separate:
//! non-durable batches can never overwrite the last durable recovery anchor.
//!
//! Two file versions share this module.
//!
//! - **Version 5** sidecars are append-only. Every page, key, table extent and
//!   checkpoint record is written at the end of the file and never again; the
//!   newest head is the one naming the largest offset, and a confirmed head
//!   checksums every byte its generation appended (one barrier).
//! - **Version 6** sidecars reuse pages. Index pages, table extents,
//!   checkpoint records and the free list itself occupy whole pages handed out
//!   by a [`PageAlloc`]; only long keys, which are never freed, are appended.
//!   Heads and records carry a publication sequence that orders them, because
//!   offsets no longer do. A confirmation is two barriers — its pages, then
//!   its head — so a head on storage always names pages that are on storage,
//!   and records the free list as it stands. Pages a confirmed generation stops
//!   referencing are reused only once `DiskIndexOptions::reclaim_after` further
//!   generations are confirmed: crash recovery returns to the newest confirmed
//!   generation, never an older one, and a reader still holding an older
//!   generation by then sees [`DiskIndexError::PageReclaimed`] rather than
//!   rewritten bytes. A writer that opens truncates the file back to the
//!   confirmed generation's high-water mark: whatever lies past it was written
//!   by a generation that never confirmed.
use super::tree::{self, read_at, write_at};
use super::*;
use crate::fallible::Arc;
use std::fs::{File, OpenOptions};

#[cfg(test)]
mod failure_tests;
#[cfg(test)]
thread_local! {
    static RECONCILE_SYNC_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const MAGIC: &[u8; 8] = b"VARVEIX5";
const HEAD_MAGIC: &[u8; 8] = b"VIXHEAD1";
/// A head that also names the checksum of its generation's sidecar bytes:
/// everything appended since the previous confirmed checkpoint, up to the end
/// of the checkpoint it points at. Written by single-barrier publication.
const HEAD_MAGIC_V2: &[u8; 8] = b"VIXHEAD2";
const ROOT_MAGIC: &[u8; 8] = b"VIXROOT4";
const ROOT_HEADER: usize = 352;
/// Version 6 heads and checkpoint records.
const HEAD_MAGIC_V3: &[u8; 8] = b"VIXHEAD3";
const ROOT_MAGIC_V5: &[u8; 8] = b"VIXROOT5";
const ROOT_HEADER_V6: usize = 416;
const FREE_MAGIC: &[u8; 8] = b"VIXFREE1";
const FREE_HEADER: usize = 32;
const FREE_ENTRY: usize = 16;
const FREE_ENTRIES: usize = (tree::PAGE - FREE_HEADER) / FREE_ENTRY;
pub(super) const VERSION_APPEND: u32 = 5;
pub(super) const VERSION_REUSE: u32 = 6;
// Independently synced heads occupy separate 4096-byte regions.
pub(super) const CONFIRMED: [u64; 2] = [4096, 8192];
pub(super) const WORKING: [u64; 2] = [12288, 16384];
const DIRTY: u64 = 20480;
const FILE_HEADER: usize = 24576;

fn corrupt(message: &'static str) -> DiskIndexError {
    DiskIndexError::CheckpointMismatch(message)
}

struct WriterFile(File);
impl Drop for WriterFile {
    fn drop(&mut self) {
        let _ = crate::file::unlock_native_guard(&self.0);
    }
}

pub(super) struct Store {
    pub file: Arc<File>,
    writer: Option<WriterFile>,
    pub path: PathBuf,
    options: DiskIndexOptions,
    version: u32,
    /// Version 6 writers only: the page allocator, loaded from the newest
    /// confirmed generation on the first write.
    reuse: Option<ReuseState>,
    /// The newest confirmed head whose generation this handle has already
    /// checked, as its checkpoint offset (0 for none). Heads only move forward,
    /// so a generation is read for its checksum once per handle.
    verified_head: std::sync::atomic::AtomicU64,
}

#[derive(Clone, Copy, Default)]
pub(super) struct Root {
    pub tree: tree::Root,
    pub slots: slots::Root,
}
impl Root {
    pub fn count(self) -> u64 {
        self.tree.count + u64::from(self.slots.count)
    }
}
pub(super) struct Reader {
    pub tree: tree::Reader,
    pub slots: slots::Reader,
}
impl Reader {
    pub fn get(&self, root: Root, key: &[u8]) -> DiskIndexResult<Option<[u8; LATEST_LEN]>> {
        if let Some(value) = self.slots.get(key)? {
            return Ok(value);
        }
        self.tree.get(root.tree, key)
    }
}

#[derive(Clone)]
pub(super) struct Checkpoint {
    pub offset: u64,
    pub end: u64,
    pub root: Root,
    pub metadata: DiskIndexMetadata,
    /// Version 6 only (zero otherwise): publication order, the end of the
    /// file when the checkpoint was published, and its free list.
    pub seq: u64,
    pub high_water: u64,
    pub free: FreeRoot,
}

/// The first page of a version 6 free list and how many entries it holds.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) struct FreeRoot {
    pub offset: u64,
    pub entries: u64,
}

/// A version 6 writer's allocation state.
struct ReuseState {
    alloc: tree::PageAlloc,
    /// The next publication sequence.
    next_seq: u64,
    /// Pages holding this dirty generation's working and dirty records,
    /// freed once it is confirmed.
    transient: Vec<u64>,
    /// Pages only the newest confirmed checkpoint uses (its record and free
    /// list), freed by the next confirmation.
    retired: Vec<u64>,
}

pub(super) use crate::publication_protocol::CheckpointKind as Publication;

#[derive(Clone, Copy)]
struct Head {
    offset: u64,
    end: u64,
    /// `(region_start, crc32 of [region_start, end))` for a v2 head.
    region: Option<(u64, u32)>,
    /// The publication sequence of a v3 (version 6) head.
    seq: Option<u64>,
}

impl Head {
    /// Newer heads sort first: by sequence in version 6, where offsets are
    /// reused, by offset in version 5, where they only grow.
    fn order(&self) -> u64 {
        self.seq.unwrap_or(self.offset)
    }
}

fn encode_head(head: Head) -> [u8; 64] {
    let mut bytes = [0; 64];
    bytes[8..16].copy_from_slice(&head.offset.to_le_bytes());
    bytes[16..24].copy_from_slice(&head.end.to_le_bytes());
    if let Some(seq) = head.seq {
        bytes[..8].copy_from_slice(HEAD_MAGIC_V3);
        bytes[24..32].copy_from_slice(&seq.to_le_bytes());
        let checksum = crc32fast::hash(&bytes[..60]);
        bytes[60..64].copy_from_slice(&checksum.to_le_bytes());
        return bytes;
    }
    match head.region {
        None => {
            bytes[..8].copy_from_slice(HEAD_MAGIC);
            let checksum = crc32fast::hash(&bytes[..28]);
            bytes[28..32].copy_from_slice(&checksum.to_le_bytes());
        }
        Some((start, crc)) => {
            bytes[..8].copy_from_slice(HEAD_MAGIC_V2);
            bytes[24..32].copy_from_slice(&start.to_le_bytes());
            bytes[32..36].copy_from_slice(&crc.to_le_bytes());
            let checksum = crc32fast::hash(&bytes[..60]);
            bytes[60..64].copy_from_slice(&checksum.to_le_bytes());
        }
    }
    bytes
}

// Shared by file-backed recovery and portable corruption checks.
fn parse_head(bytes: &[u8; 64]) -> Option<Head> {
    if &bytes[..8] == HEAD_MAGIC_V3 {
        if bytes[32..60] != [0; 28] || crc32fast::hash(&bytes[..60]) != read_u32(bytes, 60) {
            return None;
        }
        let head = Head {
            offset: read_u64(bytes, 8),
            end: read_u64(bytes, 16),
            region: None,
            seq: Some(read_u64(bytes, 24)),
        };
        if head.offset < FILE_HEADER as u64
            || head
                .end
                .checked_sub(head.offset)
                .is_none_or(|n| n < ROOT_HEADER_V6 as u64)
        {
            return None;
        }
        return Some(head);
    }
    let region = if &bytes[..8] == HEAD_MAGIC {
        if bytes[24..28] != [0; 4]
            || bytes[32..] != [0; 32]
            || crc32fast::hash(&bytes[..28]) != read_u32(bytes, 28)
        {
            return None;
        }
        None
    } else if &bytes[..8] == HEAD_MAGIC_V2 {
        if bytes[36..60] != [0; 24] || crc32fast::hash(&bytes[..60]) != read_u32(bytes, 60) {
            return None;
        }
        Some((read_u64(bytes, 24), read_u32(bytes, 32)))
    } else {
        return None;
    };
    let head = Head {
        offset: read_u64(bytes, 8),
        end: read_u64(bytes, 16),
        region,
        seq: None,
    };
    if region.is_some_and(|(start, _)| start < FILE_HEADER as u64 || start > head.offset) {
        return None;
    }
    if head.offset < FILE_HEADER as u64
        || head
            .end
            .checked_sub(head.offset)
            .is_none_or(|n| n < ROOT_HEADER as u64)
    {
        return None;
    }
    Some(head)
}

impl Store {
    pub fn open(
        path: &Path,
        options: DiskIndexOptions,
        create: bool,
        writable: bool,
    ) -> DiskIndexResult<Self> {
        Self::open_version(path, options, create, writable, VERSION_REUSE)
    }

    /// Creates a version 5 (append-only) sidecar, as releases before page
    /// reuse wrote: for tests of the legacy format writers still maintain.
    #[cfg(test)]
    pub fn create_legacy(path: &Path, options: DiskIndexOptions) -> DiskIndexResult<Self> {
        Self::open_version(path, options, true, true, VERSION_APPEND)
    }

    fn open_version(
        path: &Path,
        options: DiskIndexOptions,
        create: bool,
        writable: bool,
        create_version: u32,
    ) -> DiskIndexResult<Self> {
        let writer = if writable {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(create)
                .truncate(false)
                .open(path)?;
            match crate::file::try_lock_native_guard(&file) {
                Ok(()) => {}
                Err(crate::file::WriterGuardLockError::WouldBlock) => {
                    return Err(DiskIndexError::Busy);
                }
                Err(crate::file::WriterGuardLockError::Io(error)) => return Err(error.into()),
            }
            Some(WriterFile(file))
        } else {
            None
        };
        // Independent read-only file description; snapshots cannot retain a writer lock.
        let file = crate::fallible::try_arc(File::open(path)?)?;
        if let Some(writer) = &writer
            && opened_sidecar_identity(&writer.0)? != opened_sidecar_identity(&file)?
        {
            return Err(DiskIndexError::CheckpointMismatch(
                "index pathname changed while opening writer",
            ));
        }
        let mut result = Self {
            file,
            writer,
            path: path.to_path_buf(),
            options,
            version: VERSION_REUSE,
            reuse: None,
            verified_head: std::sync::atomic::AtomicU64::new(0),
        };
        if create {
            let writer = result.writer()?;
            if writer.metadata()?.len() != 0 {
                return Err(corrupt("index already contains data"));
            }
            let mut header = [0; FILE_HEADER];
            header[..8].copy_from_slice(MAGIC);
            header[8..12].copy_from_slice(&create_version.to_le_bytes());
            let checksum = crc32fast::hash(&header[..12]);
            header[12..16].copy_from_slice(&checksum.to_le_bytes());
            write_at(writer, 0, &header)?;
            result.version = create_version;
            if create_version == VERSION_REUSE {
                // Nothing is published yet: the pool starts empty and the
                // first confirmation sets the birth of what follows it.
                result.reuse = Some(ReuseState {
                    alloc: tree::PageAlloc::new(1),
                    next_seq: 1,
                    transient: Vec::new(),
                    retired: Vec::new(),
                });
            }
        } else {
            let mut header = [0; 16];
            read_at(&result.file, 0, &mut header)?;
            let version = read_u32(&header, 8);
            if &header[..8] != MAGIC
                || !matches!(version, VERSION_APPEND | VERSION_REUSE)
                || crc32fast::hash(&header[..12]) != read_u32(&header, 12)
            {
                return Err(corrupt("invalid Varlit index file header"));
            }
            result.version = version;
        }
        Ok(result)
    }
    pub fn writer(&self) -> DiskIndexResult<&File> {
        self.writer
            .as_ref()
            .map(|f| &f.0)
            .ok_or_else(|| corrupt("index handle is read-only"))
    }
    pub(super) fn format(&self) -> tree::PageFormat {
        if self.version == VERSION_REUSE {
            tree::PageFormat::Reuse
        } else {
            tree::PageFormat::Append
        }
    }
    /// The extent a reader of `checkpoint` may reach: everything written
    /// before its record in version 5, everything up to its high-water mark in
    /// version 6.
    fn extent(&self, checkpoint: &Checkpoint) -> u64 {
        if self.version == VERSION_REUSE {
            checkpoint.high_water
        } else {
            checkpoint.offset
        }
    }
    pub fn reader(&self, checkpoint: &Checkpoint) -> Reader {
        let budget = if checkpoint.root.slots.offset == 0 {
            self.options.cache_bytes
        } else {
            self.options.cache_bytes / 2
        };
        let extent = self.extent(checkpoint);
        Reader {
            tree: tree::Reader::new(
                self.file.clone(),
                extent,
                self.options
                    .max_key_bytes
                    .saturating_add(DiskIndexKeyTag::ENCODED_LEN),
                budget,
            )
            .with_minimum_offset(FILE_HEADER as u64)
            .with_format(self.format())
            .with_metadata_verification(
                self.writer.is_some()
                    || self
                        .options
                        .limits
                        .read_validation
                        .is_none_or(|p| p.structure),
            ),
            slots: slots::Reader::new(
                self.file.clone(),
                extent,
                checkpoint.root.slots,
                self.options.cache_bytes / 2,
                self.format(),
            ),
        }
    }
    fn head(&self, slot: u64) -> DiskIndexResult<Option<Head>> {
        let mut bytes = [0; 64];
        read_at(&self.file, slot, &mut bytes)?;
        Ok(parse_head(&bytes))
    }
    pub fn at(&self, offset: u64) -> DiskIndexResult<Checkpoint> {
        if self.version == VERSION_REUSE {
            return self.at_reuse(offset);
        }
        let mut bytes = [0; ROOT_HEADER];
        if offset < FILE_HEADER as u64 {
            return Err(corrupt("invalid checkpoint offset"));
        }
        read_at(&self.file, offset, &mut bytes)?;
        if &bytes[..8] != ROOT_MAGIC || crc32fast::hash(&bytes[..348]) != read_u32(&bytes, 348) {
            return Err(corrupt("invalid checkpoint header"));
        }
        let metadata = decode_metadata(&bytes[32..332])?;
        let tails = read_u32(&bytes, 332);
        if tails != metadata.working_tail_count {
            return Err(DiskIndexError::TailCountMismatch {
                expected: metadata.working_tail_count,
                actual: tails,
            });
        }
        let len = ROOT_HEADER as u64 + u64::from(tails) * TAIL_LEN as u64;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| corrupt("checkpoint extent overflow"))?;
        if read_u64(&bytes, 8) != len || end > self.file.metadata()?.len() {
            return Err(corrupt("checkpoint exceeds file"));
        }
        let tree = tree::Root {
            offset: read_u64(&bytes, 16),
            count: read_u64(&bytes, 24),
            birth: 0,
        };
        if (tree.offset == 0) != (tree.count == 0)
            || tree.count > metadata.working.record_count
            || (tree.offset != 0
                && (tree.offset < FILE_HEADER as u64
                    || tree
                        .offset
                        .checked_add(tree::PAGE as u64)
                        .is_none_or(|n| n > offset)))
        {
            return Err(corrupt("invalid checkpoint tree root"));
        }
        let slots = slots::Root {
            offset: read_u64(&bytes, 336),
            count: read_u32(&bytes, 344),
            birth: 0,
        };
        let root = Root { tree, slots };
        if tree
            .count
            .checked_add(u64::from(slots.count))
            .is_none_or(|n| n > metadata.working.record_count)
            || (slots.offset == 0 && slots.count != 0)
            || (slots.offset != 0
                && (slots.offset < FILE_HEADER as u64
                    || slots.offset.checked_add(28).is_none_or(|n| n > offset)))
        {
            return Err(corrupt("invalid finite slot root"));
        }
        if metadata.state == DiskIndexState::Dirty
            && metadata.checkpoint.is_none_or(|c| {
                c.checkpoint_offset >= offset || c.checkpoint_offset < FILE_HEADER as u64
            })
        {
            return Err(corrupt("invalid rollback checkpoint offset"));
        }
        Ok(Checkpoint {
            offset,
            end,
            root,
            metadata,
            seq: 0,
            high_water: 0,
            free: FreeRoot::default(),
        })
    }
    /// A version 6 checkpoint record. Its page may have been reused since a
    /// head named it, so a record that does not parse is reported, and the
    /// caller compares its sequence with the head's.
    fn at_reuse(&self, offset: u64) -> DiskIndexResult<Checkpoint> {
        let mut bytes = [0; ROOT_HEADER_V6];
        if offset < FILE_HEADER as u64 {
            return Err(corrupt("invalid checkpoint offset"));
        }
        read_at(&self.file, offset, &mut bytes)?;
        if &bytes[..8] != ROOT_MAGIC_V5
            || bytes[396..412] != [0; 16]
            || crc32fast::hash(&bytes[..412]) != read_u32(&bytes, 412)
        {
            return Err(corrupt("invalid checkpoint header"));
        }
        let metadata = decode_metadata(&bytes[32..332])?;
        let tails = read_u32(&bytes, 332);
        if tails != metadata.working_tail_count {
            return Err(DiskIndexError::TailCountMismatch {
                expected: metadata.working_tail_count,
                actual: tails,
            });
        }
        let len = ROOT_HEADER_V6 as u64 + u64::from(tails) * TAIL_LEN as u64;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| corrupt("checkpoint extent overflow"))?;
        let high_water = read_u64(&bytes, 388);
        if read_u64(&bytes, 8) != len
            || end > high_water
            || high_water > self.file.metadata()?.len()
        {
            return Err(corrupt("checkpoint exceeds file"));
        }
        let within = |at: u64, len: u64| {
            at >= FILE_HEADER as u64 && at.checked_add(len).is_some_and(|n| n <= high_water)
        };
        let tree = tree::Root {
            offset: read_u64(&bytes, 16),
            count: read_u64(&bytes, 24),
            birth: read_u64(&bytes, 348),
        };
        if (tree.offset == 0) != (tree.count == 0)
            || (tree.offset == 0) != (tree.birth == 0)
            || tree.count > metadata.working.record_count
            || (tree.offset != 0 && !within(tree.offset, tree::PAGE as u64))
        {
            return Err(corrupt("invalid checkpoint tree root"));
        }
        let slots = slots::Root {
            offset: read_u64(&bytes, 336),
            count: read_u32(&bytes, 344),
            birth: read_u64(&bytes, 356),
        };
        if tree
            .count
            .checked_add(u64::from(slots.count))
            .is_none_or(|n| n > metadata.working.record_count)
            || (slots.offset == 0 && (slots.count != 0 || slots.birth != 0))
            || (slots.offset != 0 && !within(slots.offset, 28))
        {
            return Err(corrupt("invalid finite slot root"));
        }
        let free = FreeRoot {
            offset: read_u64(&bytes, 372),
            entries: read_u64(&bytes, 380),
        };
        if (free.offset == 0) != (free.entries == 0)
            || (free.offset != 0 && !within(free.offset, tree::PAGE as u64))
        {
            return Err(corrupt("invalid free list root"));
        }
        if metadata.state == DiskIndexState::Dirty
            && metadata.checkpoint.is_none_or(|c| {
                c.checkpoint_offset == offset || c.checkpoint_offset < FILE_HEADER as u64
            })
        {
            return Err(corrupt("invalid rollback checkpoint offset"));
        }
        Ok(Checkpoint {
            offset,
            end,
            root: Root { tree, slots },
            metadata,
            seq: read_u64(&bytes, 364),
            high_water,
            free,
        })
    }
    fn latest(&self, confirmed_only: bool) -> DiskIndexResult<Checkpoint> {
        // Retry only an incomplete publication. Old immutable checkpoints remain readable.
        for _ in 0..8 {
            let mut heads = [(
                Head {
                    offset: 0,
                    end: 0,
                    region: None,
                    seq: None,
                },
                false,
            ); 5];
            let mut head_count = 0;
            let mut has_confirmed_head = false;
            // Only confirmed heads are barriers. A dirty head is written
            // without one: a crashed generation is recovered from the newest
            // confirmed checkpoint whether or not its dirty head survived.
            for (slot, durable) in CONFIRMED.into_iter().map(|s| (s, true)).chain(
                [(WORKING[0], false), (WORKING[1], false), (DIRTY, false)]
                    .into_iter()
                    .filter(|_| !confirmed_only),
            ) {
                if let Some(head) = self.head(slot)? {
                    has_confirmed_head |= CONFIRMED.contains(&slot);
                    heads[head_count] = (head, durable);
                    head_count += 1;
                }
            }
            // Working/dirty heads are not substitutes for a confirmed anchor.
            // Both confirmed copies may be momentarily unreadable during a
            // concurrent publication, so retry the existing bounded scan. If
            // they stay invalid, recovery must refuse instead of promoting a
            // stale dirty checkpoint and truncating acknowledged primary data.
            if !has_confirmed_head {
                std::hint::spin_loop();
                continue;
            }
            let heads = &mut heads[..head_count];
            heads.sort_unstable_by_key(|(head, _)| std::cmp::Reverse(head.order()));
            // The newest confirmed head is the only one whose barrier a crash
            // can have interrupted: every older one was synced before the next
            // generation began. A v2 head proves its generation reached storage
            // whole; one that does not is skipped for the older confirmed head.
            let newest_confirmed = heads
                .iter()
                .filter(|(_, durable)| *durable)
                .map(|(head, _)| head.order())
                .max();
            // Both slots naming the newest generation means its barrier
            // completed (the copy is written only afterwards): a checksum
            // failure there is damage to durable data, never a torn write.
            let copies = heads
                .iter()
                .filter(|(head, durable)| *durable && Some(head.order()) == newest_confirmed)
                .count();
            for &(head, durable) in heads.iter() {
                if durable
                    && Some(head.order()) == newest_confirmed
                    && head.region.is_some()
                    && !self.generation_intact(head)?
                {
                    if copies > 1 {
                        return Err(corrupt("durable generation fails its checksum"));
                    }
                    continue;
                }
                // A version 6 head is only as good as the record its page still
                // holds: a reused page names another publication's sequence.
                let record = self.at(head.offset).and_then(|checkpoint| match head.seq {
                    Some(seq) if checkpoint.seq != seq => {
                        Err(corrupt("checkpoint page was reused"))
                    }
                    _ => Ok(checkpoint),
                });
                if !durable && head.seq.is_some() && record.is_err() {
                    continue;
                }
                match record {
                    Ok(checkpoint)
                        if checkpoint.end == head.end
                            && (!confirmed_only
                                || checkpoint.metadata.state == DiskIndexState::Clean) =>
                    {
                        return Ok(checkpoint);
                    }
                    // A durable head is only written after its checkpoint is synced.
                    // A valid head naming invalid data is corruption, never permission
                    // to silently forget an acknowledged generation.
                    Err(error) if durable => return Err(error),
                    Ok(_) if durable => return Err(corrupt("durable head/checkpoint mismatch")),
                    _ => {}
                }
            }
            std::hint::spin_loop();
        }
        Err(DiskIndexError::MetadataMissing)
    }
    /// The confirmed slot holding the newest head a crash cannot have torn,
    /// with that head: the newer slot unless its generation fails its
    /// checksum, then the older one.
    fn durable_confirmed_slot(&self) -> DiskIndexResult<Option<(u64, Head)>> {
        let mut found = [None, None];
        for (i, slot) in CONFIRMED.into_iter().enumerate() {
            found[i] = self.head(slot)?.map(|head| (slot, head));
        }
        let (newer, older) = match found {
            [Some(a), Some(b)] if a.1.order() >= b.1.order() => (Some(a), Some(b)),
            [Some(a), Some(b)] => (Some(b), Some(a)),
            [one, None] | [None, one] => (one, None),
        };
        match newer {
            Some((slot, head)) if self.generation_intact(head)? => Ok(Some((slot, head))),
            Some(_) => Ok(older),
            None => Ok(None),
        }
    }
    /// Whether every sidecar byte of a v2 head's generation matches the
    /// checksum the head names. Read once per head per handle.
    fn generation_intact(&self, head: Head) -> DiskIndexResult<bool> {
        use std::sync::atomic::Ordering;
        let Some((start, expected)) = head.region else {
            return Ok(true);
        };
        if self.verified_head.load(Ordering::Relaxed) == head.offset {
            return Ok(true);
        }
        if self.file.metadata()?.len() < head.end {
            return Ok(false);
        }
        let intact = region_crc(&self.file, start, head.end, None)? == expected;
        if intact {
            self.verified_head.store(head.offset, Ordering::Relaxed);
        }
        Ok(intact)
    }
    pub fn current(&self) -> DiskIndexResult<Checkpoint> {
        self.latest(false)
    }
    pub fn confirmed(&self) -> DiskIndexResult<Checkpoint> {
        self.latest(true)
    }
    pub(super) fn reconcile_confirmed_heads(&self) -> DiskIndexResult<()> {
        let first = self.head(CONFIRMED[0])?.map(|h| (h.order(), h.end));
        let second = self.head(CONFIRMED[1])?.map(|h| (h.order(), h.end));
        crate::publication_protocol::resume(first, second, || {
            #[cfg(test)]
            if RECONCILE_SYNC_FAILURE.with(|flag| flag.replace(false)) {
                return Err(std::io::Error::other("injected head reconciliation failure").into());
            }
            self.writer()?.sync_data()?;
            Ok(())
        })
    }
    pub fn state(
        &self,
        checkpoint: &Checkpoint,
        expected: Option<u32>,
    ) -> DiskIndexResult<DiskIndexPersistentState> {
        let metadata = checkpoint.metadata;
        if let Some(limit) = expected {
            validate_expected_tail_limit(metadata, limit)?;
        }
        let count = metadata.working_tail_count as usize;
        let bytes = count
            .checked_mul(TAIL_LEN)
            .ok_or_else(|| corrupt("tail extent overflow"))?;
        if bytes > self.options.batch.max_bytes {
            return Err(corrupt("tail state exceeds configured batch budget"));
        }
        let mut tails = Vec::new();
        tails
            .try_reserve_exact(count)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        let mut previous = None;
        for i in 0..count {
            let mut row = [0; TAIL_LEN];
            let header = if self.version == VERSION_REUSE {
                ROOT_HEADER_V6
            } else {
                ROOT_HEADER
            };
            read_at(
                &self.file,
                checkpoint.offset + header as u64 + (i * TAIL_LEN) as u64,
                &mut row,
            )?;
            let id = read_u32(&row, 0);
            if previous.is_some_and(|p| p >= id) {
                return Err(DiskIndexError::InvalidTail("tails are not canonical"));
            }
            previous = Some(id);
            tails.push(decode_tail(id, &row)?);
        }
        validate_working_tails(metadata, &tails)?;
        Ok(DiskIndexPersistentState { metadata, tails })
    }
    pub fn publish(
        &mut self,
        metadata: DiskIndexMetadata,
        tails: &[DiskIndexTail],
        root: Root,
        publication: Publication,
    ) -> DiskIndexResult<Checkpoint> {
        if self.version == VERSION_REUSE {
            return self.publish_reuse(metadata, tails, root, publication);
        }
        validate_metadata(metadata)?;
        validate_working_tails(metadata, tails)?;
        if tails
            .len()
            .checked_mul(TAIL_LEN)
            .is_none_or(|n| n > self.options.batch.max_bytes)
        {
            return Err(corrupt("tail state exceeds configured batch budget"));
        }
        let file = self.writer()?;
        let len = ROOT_HEADER + tails.len() * TAIL_LEN;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        bytes.resize(len, 0);
        bytes[..8].copy_from_slice(ROOT_MAGIC);
        bytes[8..16].copy_from_slice(&(len as u64).to_le_bytes());
        bytes[16..24].copy_from_slice(&root.tree.offset.to_le_bytes());
        bytes[24..32].copy_from_slice(&root.tree.count.to_le_bytes());
        bytes[32..332].copy_from_slice(&encode_metadata(metadata));
        bytes[332..336].copy_from_slice(&(tails.len() as u32).to_le_bytes());
        bytes[336..344].copy_from_slice(&root.slots.offset.to_le_bytes());
        bytes[344..348].copy_from_slice(&root.slots.count.to_le_bytes());
        let checksum = crc32fast::hash(&bytes[..348]);
        bytes[348..352].copy_from_slice(&checksum.to_le_bytes());
        for (tail, into) in tails
            .iter()
            .zip(bytes[ROOT_HEADER..].as_chunks_mut::<TAIL_LEN>().0)
        {
            into.copy_from_slice(&encode_tail(*tail));
        }
        let offset = file.metadata()?.len();
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| corrupt("checkpoint extent overflow"))?;
        // A confirmed head names its whole generation: every byte appended
        // since the previous confirmed checkpoint, then this body.
        // The slot holding the newest durable confirmed head is never the one
        // overwritten, so a crash in this publication always leaves it.
        let anchor = if matches!(publication, Publication::Confirmed) {
            self.durable_confirmed_slot()?
        } else {
            None
        };
        let region = if matches!(publication, Publication::Confirmed) {
            let start = anchor
                .map_or(FILE_HEADER as u64, |(_, head)| head.end)
                .min(offset);
            Some((start, region_crc(&self.file, start, offset, Some(&bytes))?))
        } else {
            None
        };
        let head = encode_head(Head {
            offset,
            end,
            region,
            seq: None,
        });
        let mut slot = 0;
        use crate::publication_protocol::CheckpointStep;
        crate::publication_protocol::checkpoint(publication, |step| -> DiskIndexResult<()> {
            match step {
                CheckpointStep::Body => {
                    #[cfg(feature = "scalable-fault-injection")]
                    if crate::scalable_fault::is_armed() {
                        crate::scalable_fault_point("checkpoint.record_prefix");
                        write_at(file, offset, &bytes[..bytes.len() / 2])?;
                        crate::scalable_fault_point("checkpoint.record_prefix");
                    }
                    crate::scalable_fault_point("checkpoint.record_write");
                    write_at(file, offset, &bytes)?;
                    crate::scalable_fault_point("checkpoint.record_write");
                }
                CheckpointStep::SelectHead => {
                    slot = match publication {
                        Publication::Dirty => DIRTY,
                        Publication::Confirmed => match anchor {
                            Some((kept, _)) => CONFIRMED[usize::from(kept == CONFIRMED[0])],
                            None => CONFIRMED[0],
                        },
                        Publication::Working => {
                            let first = self.head(WORKING[0])?.map_or(0, |h| h.offset);
                            let second = self.head(WORKING[1])?.map_or(0, |h| h.offset);
                            WORKING[crate::publication_protocol::older_head(first, second)]
                        }
                    };
                }
                CheckpointStep::Head => {
                    #[cfg(feature = "scalable-fault-injection")]
                    if crate::scalable_fault::is_armed() {
                        crate::scalable_fault_point("checkpoint.head_prefix");
                        write_at(file, slot, &head[..24])?;
                        crate::scalable_fault_point("checkpoint.head_prefix");
                    }
                    crate::scalable_fault_point("checkpoint.head_write");
                    write_at(file, slot, &head)?;
                    crate::scalable_fault_point("checkpoint.head_write");
                }
                CheckpointStep::HeadSync => {
                    crate::scalable_fault_point("checkpoint.head_sync");
                    file.sync_data()?;
                    crate::scalable_fault_point("checkpoint.head_sync");
                }
                // Version 5 confirms with one barrier, over the region CRC.
                CheckpointStep::BodySync => {}
            }
            Ok(())
        })?;
        // The barrier above made the head durable in one slot. A second copy
        // goes into the other slot without a barrier of its own: the next
        // publication's barrier (or writeback) persists it, so a completed
        // generation keeps two copies against the loss of one head without a
        // second sync per publication. Overwriting the older anchor is safe
        // now: the new head is already durable. A failed copy changes nothing
        // the next publication does not redo, so it is not an error.
        if matches!(publication, Publication::Confirmed) {
            let copy = CONFIRMED[usize::from(slot == CONFIRMED[0])];
            crate::scalable_fault_point("checkpoint.head_copy");
            let _ = write_at(file, copy, &head);
            crate::scalable_fault_point("checkpoint.head_copy");
        }
        Ok(Checkpoint {
            offset,
            end,
            root,
            metadata,
            seq: 0,
            high_water: 0,
            free: FreeRoot::default(),
        })
    }
    pub fn apply(
        &mut self,
        base: &Checkpoint,
        updates: &BTreeMap<Vec<u8>, [u8; LATEST_LEN]>,
    ) -> DiskIndexResult<Root> {
        if self.version == VERSION_REUSE {
            self.ensure_reuse()?;
            let reader = self.reader(base).tree;
            let reuse = self.reuse.as_mut().expect("loaded above");
            let file = &self
                .writer
                .as_ref()
                .ok_or_else(|| corrupt("index handle is read-only"))?
                .0;
            return Ok(Root {
                tree: tree::apply_reusing(
                    file,
                    &reader,
                    base.root.tree,
                    updates,
                    &mut reuse.alloc,
                )?,
                slots: base.root.slots,
            });
        }
        Ok(Root {
            tree: tree::apply(
                self.writer()?,
                &self.reader(base).tree,
                base.root.tree,
                updates,
            )?,
            slots: base.root.slots,
        })
    }

    /// Sets the birth of the pages a freshly created version 6 file is about
    /// to receive before its first publication (compaction).
    pub(super) fn set_building(&mut self, building: u64) -> DiskIndexResult<()> {
        match self.reuse.as_mut() {
            Some(reuse) if reuse.next_seq == 1 => {
                reuse.alloc.building = building;
                Ok(())
            }
            _ => Err(corrupt("only an unpublished index can be rebuilt into")),
        }
    }

    /// Applies updates to a tree no checkpoint of this file publishes yet:
    /// the rebuild of a compaction. Reads may reach anything written so far.
    pub(super) fn apply_unpublished(
        &mut self,
        root: tree::Root,
        updates: &BTreeMap<Vec<u8>, [u8; LATEST_LEN]>,
        max_key: usize,
        cache_bytes: usize,
    ) -> DiskIndexResult<tree::Root> {
        let end = self.file.metadata()?.len();
        let reader = tree::Reader::new(self.file.clone(), end, max_key, cache_bytes)
            .with_minimum_offset(FILE_HEADER as u64)
            .with_format(self.format());
        let file = self.writer()?.try_clone()?;
        match self.alloc()? {
            Some(alloc) => tree::apply_reusing(&file, &reader, root, updates, alloc),
            None => tree::apply(&file, &reader, root, updates),
        }
    }

    /// The page allocator of a version 6 writer, or `None` for version 5.
    pub(super) fn alloc(&mut self) -> DiskIndexResult<Option<&mut tree::PageAlloc>> {
        if self.version != VERSION_REUSE {
            return Ok(None);
        }
        self.ensure_reuse()?;
        Ok(self.reuse.as_mut().map(|reuse| &mut reuse.alloc))
    }

    /// Loads a version 6 writer's allocation state from the newest confirmed
    /// generation, once, and cuts the file back to that generation's
    /// high-water mark. Deferred to the first write so that recovery can still
    /// read the interrupted generation's records before they are cut.
    fn ensure_reuse(&mut self) -> DiskIndexResult<()> {
        if self.reuse.is_some() {
            return Ok(());
        }
        let file = self.writer()?.try_clone()?;
        let confirmed = self.confirmed()?;
        let mut alloc = tree::PageAlloc::new(confirmed.metadata.generation.saturating_add(1));
        let mut retired = Vec::new();
        retired.try_reserve(1).map_err(tree_oom)?;
        retired.push(confirmed.offset);
        self.read_free_list(confirmed.free, &mut alloc, &mut retired)?;
        alloc.release(
            confirmed
                .metadata
                .generation
                .saturating_sub(self.options.reclaim_after),
        )?;
        let mut next_seq = confirmed.seq;
        for slot in CONFIRMED.into_iter().chain(WORKING).chain([DIRTY]) {
            if let Some(head) = self.head(slot)? {
                next_seq = next_seq.max(head.order());
            }
        }
        if file.metadata()?.len() > confirmed.high_water {
            file.set_len(confirmed.high_water)?;
        }
        self.reuse = Some(ReuseState {
            alloc,
            next_seq: next_seq
                .checked_add(1)
                .ok_or(DiskIndexError::GenerationExhausted)?,
            transient: Vec::new(),
            retired,
        });
        Ok(())
    }

    fn read_free_list(
        &self,
        root: FreeRoot,
        alloc: &mut tree::PageAlloc,
        pages: &mut Vec<u64>,
    ) -> DiskIndexResult<()> {
        let mut at = root.offset;
        let mut remaining = root.entries;
        let file_len = self.file.metadata()?.len();
        let limit = file_len / tree::PAGE as u64 + 1;
        let mut visited = 0u64;
        while at != 0 {
            visited += 1;
            if visited > limit {
                return Err(corrupt("free list does not terminate"));
            }
            // A free-list page lies inside the file, past the header.
            if at < FILE_HEADER as u64
                || at
                    .checked_add(tree::PAGE as u64)
                    .is_none_or(|end| end > file_len)
            {
                return Err(corrupt("free list page is outside the sidecar"));
            }
            let mut page = [0u8; tree::PAGE];
            read_at(&self.file, at, &mut page)?;
            let count = read_u32(&page, 16) as usize;
            let checksum = read_u32(&page, 20);
            page[20..24].fill(0);
            if &page[..8] != FREE_MAGIC
                || count > FREE_ENTRIES
                || page[24..FREE_HEADER] != [0; 8]
                || crc32fast::hash(&page) != checksum
                || count as u64 > remaining
            {
                return Err(corrupt("invalid free list page"));
            }
            pages.try_reserve(1).map_err(tree_oom)?;
            pages.push(at);
            alloc.reusable.try_reserve(count).map_err(tree_oom)?;
            for i in 0..count {
                let entry = FREE_HEADER + i * FREE_ENTRY;
                let offset = read_u64(&page, entry);
                let tag = read_u64(&page, entry + 8);
                if offset < FILE_HEADER as u64
                    || offset
                        .checked_add(tree::PAGE as u64)
                        .is_none_or(|end| end > file_len)
                {
                    return Err(corrupt("invalid free list entry"));
                }
                if tag == 0 {
                    alloc.reusable.push(offset);
                } else {
                    alloc.pending.try_reserve(1).map_err(tree_oom)?;
                    alloc.pending.push((offset, tag));
                }
            }
            remaining -= count as u64;
            at = read_u64(&page, 8);
        }
        if remaining != 0 {
            return Err(corrupt("free list is shorter than its root"));
        }
        Ok(())
    }

    /// Takes a page for a record, table extent or free list page: a reusable
    /// page, else a new page at the end of the file.
    fn take_page(file: &File, alloc: &mut tree::PageAlloc) -> DiskIndexResult<u64> {
        if let Some(offset) = alloc.take() {
            return Ok(offset);
        }
        let offset = file.metadata()?.len().max(FILE_HEADER as u64);
        write_at(file, offset + tree::PAGE as u64 - 1, &[0])?;
        Ok(offset)
    }

    /// Writes the allocator's free pages as a chain of free list pages. The
    /// list pages come out of the free pages themselves, so they are taken
    /// before the list is encoded.
    fn write_free_list(
        file: &File,
        alloc: &mut tree::PageAlloc,
    ) -> DiskIndexResult<(FreeRoot, Vec<u64>)> {
        let mut pages: Vec<u64> = Vec::new();
        loop {
            let entries = alloc.reusable.len() + alloc.pending.len();
            let needed = entries.div_ceil(FREE_ENTRIES);
            if pages.len() >= needed {
                break;
            }
            pages.try_reserve(1).map_err(tree_oom)?;
            pages.push(Self::take_page(file, alloc)?);
        }
        let entries: Vec<(u64, u64)> = alloc
            .reusable
            .iter()
            .map(|&offset| (offset, 0))
            .chain(alloc.pending.iter().copied())
            .collect();
        let total = entries.len() as u64;
        for (i, (&page_offset, chunk)) in pages
            .iter()
            .zip(
                entries
                    .chunks(FREE_ENTRIES)
                    .chain(std::iter::repeat(&[][..])),
            )
            .enumerate()
        {
            let mut page = [0u8; tree::PAGE];
            page[..8].copy_from_slice(FREE_MAGIC);
            let next = pages.get(i + 1).copied().unwrap_or(0);
            page[8..16].copy_from_slice(&next.to_le_bytes());
            page[16..20].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
            for (j, (offset, tag)) in chunk.iter().enumerate() {
                let entry = FREE_HEADER + j * FREE_ENTRY;
                page[entry..entry + 8].copy_from_slice(&offset.to_le_bytes());
                page[entry + 8..entry + 16].copy_from_slice(&tag.to_le_bytes());
            }
            let checksum = crc32fast::hash(&page);
            page[20..24].copy_from_slice(&checksum.to_le_bytes());
            write_at(file, page_offset, &page)?;
        }
        Ok((
            FreeRoot {
                offset: pages.first().copied().unwrap_or(0),
                entries: total,
            },
            pages,
        ))
    }

    fn publish_reuse(
        &mut self,
        metadata: DiskIndexMetadata,
        tails: &[DiskIndexTail],
        root: Root,
        publication: Publication,
    ) -> DiskIndexResult<Checkpoint> {
        validate_metadata(metadata)?;
        validate_working_tails(metadata, tails)?;
        let len = ROOT_HEADER_V6 + tails.len() * TAIL_LEN;
        if len > tree::PAGE || tails.len() * TAIL_LEN > self.options.batch.max_bytes {
            return Err(corrupt("tail state exceeds one checkpoint page"));
        }
        self.ensure_reuse()?;
        let anchor = if matches!(publication, Publication::Confirmed) {
            self.durable_confirmed_slot()?
        } else {
            None
        };
        let file = self.writer()?.try_clone()?;
        let reuse = self.reuse.as_mut().expect("loaded above");
        let seq = reuse.next_seq;
        let confirmed = matches!(publication, Publication::Confirmed);
        let mut free_pages = Vec::new();
        let mut free = FreeRoot::default();
        if confirmed {
            // This confirmation stops every older generation's record and free
            // list from being needed, and the dirty generation's own records
            // were only ever read by this writer.
            let transient = std::mem::take(&mut reuse.transient);
            for page in transient {
                reuse.alloc.free(page, reuse.alloc.building)?;
            }
            let retired = std::mem::take(&mut reuse.retired);
            for page in retired {
                reuse.alloc.free(page, 0)?;
            }
        }
        let offset = Self::take_page(&file, &mut reuse.alloc)?;
        if confirmed {
            (free, free_pages) = Self::write_free_list(&file, &mut reuse.alloc)?;
        }
        let high_water = file.metadata()?.len();
        let mut bytes = [0u8; tree::PAGE];
        bytes[..8].copy_from_slice(ROOT_MAGIC_V5);
        bytes[8..16].copy_from_slice(&(len as u64).to_le_bytes());
        bytes[16..24].copy_from_slice(&root.tree.offset.to_le_bytes());
        bytes[24..32].copy_from_slice(&root.tree.count.to_le_bytes());
        bytes[32..332].copy_from_slice(&encode_metadata(metadata));
        bytes[332..336].copy_from_slice(&(tails.len() as u32).to_le_bytes());
        bytes[336..344].copy_from_slice(&root.slots.offset.to_le_bytes());
        bytes[344..348].copy_from_slice(&root.slots.count.to_le_bytes());
        bytes[348..356].copy_from_slice(&root.tree.birth.to_le_bytes());
        bytes[356..364].copy_from_slice(&root.slots.birth.to_le_bytes());
        bytes[364..372].copy_from_slice(&seq.to_le_bytes());
        bytes[372..380].copy_from_slice(&free.offset.to_le_bytes());
        bytes[380..388].copy_from_slice(&free.entries.to_le_bytes());
        bytes[388..396].copy_from_slice(&high_water.to_le_bytes());
        let checksum = crc32fast::hash(&bytes[..412]);
        bytes[412..416].copy_from_slice(&checksum.to_le_bytes());
        for (tail, into) in tails
            .iter()
            .zip(bytes[ROOT_HEADER_V6..len].as_chunks_mut::<TAIL_LEN>().0)
        {
            into.copy_from_slice(&encode_tail(*tail));
        }
        let end = offset + len as u64;
        let head = encode_head(Head {
            offset,
            end,
            region: None,
            seq: Some(seq),
        });
        let mut slot = 0;
        use crate::publication_protocol::CheckpointStep;
        crate::publication_protocol::checkpoint_with_body_barrier(
            publication,
            |step| -> DiskIndexResult<()> {
                match step {
                    CheckpointStep::Body => {
                        #[cfg(feature = "scalable-fault-injection")]
                        if crate::scalable_fault::is_armed() {
                            crate::scalable_fault_point("checkpoint.record_prefix");
                            write_at(&file, offset, &bytes[..len / 2])?;
                            crate::scalable_fault_point("checkpoint.record_prefix");
                        }
                        crate::scalable_fault_point("checkpoint.record_write");
                        write_at(&file, offset, &bytes[..len])?;
                        crate::scalable_fault_point("checkpoint.record_write");
                    }
                    CheckpointStep::BodySync => {
                        crate::scalable_fault_point("checkpoint.body_sync");
                        file.sync_data()?;
                        crate::scalable_fault_point("checkpoint.body_sync");
                    }
                    CheckpointStep::SelectHead => {
                        slot = match publication {
                            Publication::Dirty => DIRTY,
                            Publication::Confirmed => match anchor {
                                Some((kept, _)) => CONFIRMED[usize::from(kept == CONFIRMED[0])],
                                None => CONFIRMED[0],
                            },
                            Publication::Working => {
                                let first = self.head(WORKING[0])?.map_or(0, |h| h.order());
                                let second = self.head(WORKING[1])?.map_or(0, |h| h.order());
                                WORKING[crate::publication_protocol::older_head(first, second)]
                            }
                        };
                    }
                    CheckpointStep::Head => {
                        #[cfg(feature = "scalable-fault-injection")]
                        if crate::scalable_fault::is_armed() {
                            crate::scalable_fault_point("checkpoint.head_prefix");
                            write_at(&file, slot, &head[..24])?;
                            crate::scalable_fault_point("checkpoint.head_prefix");
                        }
                        crate::scalable_fault_point("checkpoint.head_write");
                        write_at(&file, slot, &head)?;
                        crate::scalable_fault_point("checkpoint.head_write");
                    }
                    CheckpointStep::HeadSync => {
                        crate::scalable_fault_point("checkpoint.head_sync");
                        file.sync_data()?;
                        crate::scalable_fault_point("checkpoint.head_sync");
                    }
                }
                Ok(())
            },
        )?;
        let reuse = self.reuse.as_mut().expect("loaded above");
        reuse.next_seq = seq
            .checked_add(1)
            .ok_or(DiskIndexError::GenerationExhausted)?;
        if confirmed {
            // As in version 5: a second, unsynced copy of the durable head.
            let copy = CONFIRMED[usize::from(slot == CONFIRMED[0])];
            crate::scalable_fault_point("checkpoint.head_copy");
            let _ = write_at(&file, copy, &head);
            crate::scalable_fault_point("checkpoint.head_copy");
            reuse
                .retired
                .try_reserve(free_pages.len() + 1)
                .map_err(tree_oom)?;
            reuse.retired.push(offset);
            reuse.retired.extend(free_pages);
            // Saturating: an exhausted generation is refused when the next
            // one is published, not here, after this one already is.
            reuse.alloc.building = metadata.generation.saturating_add(1);
            reuse.alloc.release(
                metadata
                    .generation
                    .saturating_sub(self.options.reclaim_after),
            )?;
        } else {
            reuse.transient.try_reserve(1).map_err(tree_oom)?;
            reuse.transient.push(offset);
        }
        Ok(Checkpoint {
            offset,
            end,
            root,
            metadata,
            seq,
            high_water,
            free,
        })
    }
}

fn tree_oom(_: std::collections::TryReserveError) -> DiskIndexError {
    std::io::Error::from(std::io::ErrorKind::OutOfMemory).into()
}

/// crc32 of the file bytes `[start, end)`, followed by `tail` when given (the
/// body about to be written at `end`).
fn region_crc(file: &File, start: u64, end: u64, tail: Option<&[u8]>) -> DiskIndexResult<u32> {
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut at = start;
    while at < end {
        let n = usize::try_from((end - at).min(buffer.len() as u64)).unwrap_or(buffer.len());
        read_at(file, at, &mut buffer[..n])?;
        hasher.update(&buffer[..n]);
        at += n as u64;
    }
    if let Some(tail) = tail {
        hasher.update(tail);
    }
    Ok(hasher.finalize())
}
