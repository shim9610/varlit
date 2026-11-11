//! Varlit checkpoint publication. The confirmed and working heads are separate:
//! non-durable batches can never overwrite the last durable recovery anchor.
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
}

pub(super) use crate::publication_protocol::CheckpointKind as Publication;

#[derive(Clone, Copy)]
struct Head {
    offset: u64,
    end: u64,
    /// `(region_start, crc32 of [region_start, end))` for a v2 head.
    region: Option<(u64, u32)>,
}

fn encode_head(head: Head) -> [u8; 64] {
    let mut bytes = [0; 64];
    bytes[8..16].copy_from_slice(&head.offset.to_le_bytes());
    bytes[16..24].copy_from_slice(&head.end.to_le_bytes());
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
        let result = Self {
            file,
            writer,
            path: path.to_path_buf(),
            options,
            verified_head: std::sync::atomic::AtomicU64::new(0),
        };
        if create {
            let writer = result.writer()?;
            if writer.metadata()?.len() != 0 {
                return Err(corrupt("index already contains data"));
            }
            let mut header = [0; FILE_HEADER];
            header[..8].copy_from_slice(MAGIC);
            header[8..12].copy_from_slice(&5u32.to_le_bytes());
            let checksum = crc32fast::hash(&header[..12]);
            header[12..16].copy_from_slice(&checksum.to_le_bytes());
            write_at(writer, 0, &header)?;
        } else {
            let mut header = [0; 16];
            read_at(&result.file, 0, &mut header)?;
            if &header[..8] != MAGIC
                || read_u32(&header, 8) != 5
                || crc32fast::hash(&header[..12]) != read_u32(&header, 12)
            {
                return Err(corrupt("invalid Varlit index file header"));
            }
        }
        Ok(result)
    }
    pub fn writer(&self) -> DiskIndexResult<&File> {
        self.writer
            .as_ref()
            .map(|f| &f.0)
            .ok_or_else(|| corrupt("index handle is read-only"))
    }
    pub fn reader(&self, checkpoint: &Checkpoint) -> Reader {
        let budget = if checkpoint.root.slots.offset == 0 {
            self.options.cache_bytes
        } else {
            self.options.cache_bytes / 2
        };
        Reader {
            tree: tree::Reader::new(
                self.file.clone(),
                checkpoint.offset,
                self.options
                    .max_key_bytes
                    .saturating_add(DiskIndexKeyTag::ENCODED_LEN),
                budget,
            )
            .with_minimum_offset(FILE_HEADER as u64)
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
                checkpoint.offset,
                checkpoint.root.slots,
                self.options.cache_bytes / 2,
            ),
        }
    }
    fn head(&self, slot: u64) -> DiskIndexResult<Option<Head>> {
        let mut bytes = [0; 64];
        read_at(&self.file, slot, &mut bytes)?;
        Ok(parse_head(&bytes))
    }
    pub fn at(&self, offset: u64) -> DiskIndexResult<Checkpoint> {
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
            heads.sort_unstable_by_key(|(head, _)| std::cmp::Reverse(head.offset));
            // The newest confirmed head is the only one whose barrier a crash
            // can have interrupted: every older one was synced before the next
            // generation began. A v2 head proves its generation reached storage
            // whole; one that does not is skipped for the older confirmed head.
            let newest_confirmed = heads
                .iter()
                .filter(|(_, durable)| *durable)
                .map(|(head, _)| head.offset)
                .max();
            // Both slots naming the newest generation means its barrier
            // completed (the copy is written only afterwards): a checksum
            // failure there is damage to durable data, never a torn write.
            let copies = heads
                .iter()
                .filter(|(head, durable)| *durable && Some(head.offset) == newest_confirmed)
                .count();
            for &(head, durable) in heads.iter() {
                if durable
                    && Some(head.offset) == newest_confirmed
                    && head.region.is_some()
                    && !self.generation_intact(head)?
                {
                    if copies > 1 {
                        return Err(corrupt("durable generation fails its checksum"));
                    }
                    continue;
                }
                match self.at(head.offset) {
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
            [Some(a), Some(b)] if a.1.offset >= b.1.offset => (Some(a), Some(b)),
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
        let first = self.head(CONFIRMED[0])?.map(|h| (h.offset, h.end));
        let second = self.head(CONFIRMED[1])?.map(|h| (h.offset, h.end));
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
            read_at(
                &self.file,
                checkpoint.offset + ROOT_HEADER as u64 + (i * TAIL_LEN) as u64,
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
        })
    }
    pub fn apply(
        &mut self,
        base: &Checkpoint,
        updates: &BTreeMap<Vec<u8>, [u8; LATEST_LEN]>,
    ) -> DiskIndexResult<Root> {
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
