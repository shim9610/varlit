//! Immutable, append-only index pages. Each handle owns its bounded cache.
//! Only the sole writer appends pages; published pages are never reused.
use super::{DiskIndexError, DiskIndexResult, LATEST_LEN};
use crate::fallible::Arc;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;

pub(super) const PAGE: usize = 4096;
const HEADER: usize = 32;
const LEAF_ENTRY: usize = 28 + LATEST_LEN;
const BRANCH_ENTRY: usize = 28 + 16;
const MAGIC: &[u8; 8] = b"VIXPAGE2";
fn entry_size(height: u8) -> usize {
    if height == 0 {
        LEAF_ENTRY
    } else {
        BRANCH_ENTRY
    }
}
fn fanout(height: u8) -> usize {
    (PAGE - HEADER) / entry_size(height)
}

fn corrupt(message: &'static str) -> DiskIndexError {
    DiskIndexError::CheckpointMismatch(message)
}

// Test-only syscall seam: production continues to call the native positional
// read directly. Fault state is thread-local, so other readers are unaffected.
#[cfg(test)]
#[derive(Clone, Copy)]
struct ReadFault {
    chunk: usize,
    fail_at: usize,
    calls: usize,
    interrupted: usize,
    kind: std::io::ErrorKind,
}
#[cfg(test)]
thread_local! {
    static READ_FAULT: std::cell::Cell<Option<ReadFault>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
fn with_read_fault<T>(
    chunk: usize,
    fail_at: usize,
    kind: std::io::ErrorKind,
    op: impl FnOnce() -> T,
) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            READ_FAULT.with(|s| s.set(None));
        }
    }
    assert!(chunk > 0);
    READ_FAULT.with(|s| {
        assert!(s.get().is_none());
        s.set(Some(ReadFault {
            chunk,
            fail_at,
            calls: 0,
            interrupted: 0,
            kind,
        }));
    });
    let _reset = Reset;
    let result = op();
    let calls = READ_FAULT.with(|s| {
        let f = s.get().unwrap();
        f.calls - f.interrupted
    });
    (result, calls)
}

pub(super) fn read_at(file: &File, mut offset: u64, mut bytes: &mut [u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        #[cfg(test)]
        let limit = READ_FAULT.with(|state| -> std::io::Result<usize> {
            let Some(mut fault) = state.get() else {
                return Ok(bytes.len());
            };
            fault.calls += 1;
            state.set(Some(fault));
            if fault.calls == fault.fail_at {
                // A zero-length syscall result exercises the real EOF branch.
                if fault.kind == std::io::ErrorKind::UnexpectedEof {
                    return Ok(0);
                }
                return Err(fault.kind.into());
            }
            Ok(bytes.len().min(fault.chunk))
        })?;
        #[cfg(test)]
        let target = &mut bytes[..limit];
        #[cfg(not(test))]
        let target = &mut *bytes;
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::read_at(file, target, offset);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_read(file, target, offset);
        #[cfg(not(any(unix, windows)))]
        let result: std::io::Result<usize> = Err(std::io::ErrorKind::Unsupported.into());
        let n = match result {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                #[cfg(test)]
                READ_FAULT.with(|s| {
                    if let Some(mut f) = s.get() {
                        f.interrupted += 1;
                        s.set(Some(f));
                    }
                });
                continue;
            }
            other => other?,
        };
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        offset = offset
            .checked_add(n as u64)
            .ok_or(std::io::ErrorKind::InvalidInput)?;
        bytes = &mut bytes[n..];
    }
    Ok(())
}

pub(super) fn write_at(file: &File, mut offset: u64, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::write_at(file, bytes, offset);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_write(file, bytes, offset);
        #[cfg(not(any(unix, windows)))]
        let result: std::io::Result<usize> = Err(std::io::ErrorKind::Unsupported.into());
        let n = match result {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        offset = offset
            .checked_add(n as u64)
            .ok_or(std::io::ErrorKind::InvalidInput)?;
        bytes = &bytes[n..];
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Root {
    pub offset: u64,
    pub count: u64,
}

#[derive(Clone, Debug)]
struct Key {
    offset: u64,
    len: usize,
    prefix: [u8; 16],
    decoded: Option<usize>,
}
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset && self.len == other.len && self.prefix == other.prefix
    }
}
impl Eq for Key {}
impl Key {
    fn cached<'a>(&'a self, packed: &'a [u8]) -> Option<&'a [u8]> {
        if self.len <= 16 {
            Some(&self.prefix[..self.len])
        } else {
            self.decoded
                .and_then(|at| packed.get(at..at.checked_add(self.len)?))
        }
    }
}

#[derive(Clone, Debug)]
struct Entry {
    key: Key,
    value: [u8; LATEST_LEN],
}

impl Entry {
    fn child(&self) -> Root {
        Root {
            offset: u64::from_le_bytes(self.value[..8].try_into().unwrap()),
            count: u64::from_le_bytes(self.value[8..16].try_into().unwrap()),
        }
    }
    fn branch(key: Key, root: Root) -> Self {
        let mut value = [0; LATEST_LEN];
        value[..8].copy_from_slice(&root.offset.to_le_bytes());
        value[8..16].copy_from_slice(&root.count.to_le_bytes());
        Self { key, value }
    }
}

#[derive(Debug)]
struct Node {
    height: u8,
    count: u64,
    entries: Vec<Entry>,
    key_bytes: Vec<u8>,
}

#[derive(Clone, Copy)]
enum Cached {
    Page(u64),
    Key(u64),
}

#[derive(Default)]
struct Cache {
    pages: HashMap<u64, Option<Node>>,
    keys: HashMap<u64, Option<Vec<u8>>>,
    order: VecDeque<Cached>,
    bytes: usize,
}

// A lease temporarily owns an existing cache value. Its occupied slot remains
// charged and cannot be evicted until Drop returns it without allocating.
// This replaces per-page/per-key Arc allocations with single ownership.
trait Resident: Sized {
    fn slots(cache: &mut Cache) -> &mut HashMap<u64, Option<Self>>;
    fn charge(&self) -> usize;
    fn tag(offset: u64) -> Cached;
}
impl Resident for Node {
    fn slots(cache: &mut Cache) -> &mut HashMap<u64, Option<Self>> {
        &mut cache.pages
    }
    fn charge(&self) -> usize {
        Reader::charge(self)
    }
    fn tag(offset: u64) -> Cached {
        Cached::Page(offset)
    }
}
impl Resident for Vec<u8> {
    fn slots(cache: &mut Cache) -> &mut HashMap<u64, Option<Self>> {
        &mut cache.keys
    }
    fn charge(&self) -> usize {
        self.capacity() + 128
    }
    fn tag(offset: u64) -> Cached {
        Cached::Key(offset)
    }
}
struct Lease<'a, T: Resident> {
    value: Option<T>,
    reader: &'a Reader,
    offset: u64,
    cached: bool,
}
impl<T: Resident> std::ops::Deref for Lease<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value.as_ref().expect("live cache lease")
    }
}
impl<T: Resident> Drop for Lease<'_, T> {
    fn drop(&mut self) {
        if self.cached {
            let mut cache = self.reader.cache.borrow_mut();
            let slot = T::slots(&mut cache)
                .get_mut(&self.offset)
                .expect("leased cache slot");
            *slot = self.value.take();
        }
    }
}
fn oom(_: std::collections::TryReserveError) -> DiskIndexError {
    std::io::Error::from(std::io::ErrorKind::OutOfMemory).into()
}
fn buffer(len: usize) -> DiskIndexResult<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(oom)?;
    bytes.resize(len, 0);
    Ok(bytes)
}
fn copy_bytes(bytes: &[u8]) -> DiskIndexResult<Vec<u8>> {
    let mut result = Vec::new();
    result.try_reserve_exact(bytes.len()).map_err(oom)?;
    result.extend_from_slice(bytes);
    Ok(result)
}

/// Send, not Sync: a reader's cache is never shared between reading threads.
pub(super) struct Reader {
    file: Arc<File>,
    end: u64,
    minimum_offset: u64,
    max_key: usize,
    verify_metadata: bool,
    budget: usize,
    cache: RefCell<Cache>,
    #[cfg(test)]
    page_reads: std::cell::Cell<usize>,
    #[cfg(test)]
    key_reads: std::cell::Cell<usize>,
}

impl Reader {
    pub fn new(file: Arc<File>, end: u64, max_key: usize, budget: usize) -> Self {
        Self {
            file,
            end,
            minimum_offset: PAGE as u64,
            max_key,
            verify_metadata: true,
            budget,
            cache: RefCell::new(Cache::default()),
            #[cfg(test)]
            page_reads: std::cell::Cell::new(0),
            #[cfg(test)]
            key_reads: std::cell::Cell::new(0),
        }
    }
    pub(super) fn with_metadata_verification(mut self, enabled: bool) -> Self {
        self.verify_metadata = enabled;
        self
    }
    pub(super) fn with_minimum_offset(mut self, minimum_offset: u64) -> Self {
        self.minimum_offset = minimum_offset;
        self
    }
    fn node(&self, root: Root) -> DiskIndexResult<Lease<'_, Node>> {
        if let Some(node) = self.cached::<Node>(root.offset) {
            if node.count != root.count {
                return Err(corrupt("index subtree count mismatch"));
            }
            return Ok(node);
        }
        self.retain(root.offset, self.read_node(root)?)
    }
    fn read_node(&self, root: Root) -> DiskIndexResult<Node> {
        if root.offset < self.minimum_offset
            || root
                .offset
                .checked_add(PAGE as u64)
                .is_none_or(|e| e > self.end)
        {
            return Err(corrupt("index page outside checkpoint extent"));
        }
        let mut bytes = [0; PAGE];
        read_at(&self.file, root.offset, &mut bytes)?;
        #[cfg(test)]
        self.page_reads.set(self.page_reads.get() + 1);
        if &bytes[..8] != MAGIC || bytes[9] != 0 || bytes[20..HEADER] != [0; 12] {
            return Err(corrupt("invalid index page header"));
        }
        let checksum = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        bytes[16..20].fill(0);
        if self.verify_metadata && crc32fast::hash(&bytes) != checksum {
            return Err(corrupt("index page checksum mismatch"));
        }
        let height = bytes[8];
        let len = u16::from_le_bytes(bytes[10..12].try_into().unwrap()) as usize;
        if height > 31 || len == 0 || len > fanout(height) || bytes[12..16] != [0; 4] {
            return Err(corrupt("invalid index page shape"));
        }
        if self.verify_metadata
            && bytes[HEADER + len * entry_size(height)..]
                .iter()
                .any(|x| *x != 0)
        {
            return Err(corrupt("nonzero index page padding"));
        }
        let mut entries = Vec::new();
        entries.try_reserve_exact(len).map_err(oom)?;
        let mut count = 0u64;
        for item in
            bytes[HEADER..HEADER + len * entry_size(height)].chunks_exact(entry_size(height))
        {
            let key = Key {
                offset: u64::from_le_bytes(item[..8].try_into().unwrap()),
                len: u32::from_le_bytes(item[8..12].try_into().unwrap()) as usize,
                prefix: item[12..28].try_into().unwrap(),
                decoded: None,
            };
            if key.len == 0 || key.len > self.max_key {
                return Err(corrupt("invalid index key length"));
            }
            if key.len <= 16 {
                if key.offset != 0 || key.prefix[key.len..].iter().any(|x| *x != 0) {
                    return Err(corrupt("invalid inline index key"));
                }
            } else if key.offset < self.minimum_offset
                || key
                    .offset
                    .checked_add(key.len as u64 + 4)
                    .is_none_or(|e| e > root.offset)
            {
                return Err(corrupt("invalid index key extent"));
            }
            let mut value = [0; LATEST_LEN];
            value[..item.len() - 28].copy_from_slice(&item[28..]);
            let entry = Entry { key, value };
            let increment = if height == 0 {
                1
            } else {
                let child = entry.child();
                if child.count == 0
                    || child.offset < self.minimum_offset
                    || child
                        .offset
                        .checked_add(PAGE as u64)
                        .is_none_or(|e| e > root.offset)
                    || entry.value[16..].iter().any(|x| *x != 0)
                {
                    return Err(corrupt("invalid index child"));
                }
                child.count
            };
            count = count
                .checked_add(increment)
                .ok_or_else(|| corrupt("index count overflow"))?;
            entries.push(entry);
        }
        if count != root.count {
            return Err(corrupt("index subtree count mismatch"));
        }
        // Validate order from inline prefixes whenever possible. Only equal
        // long prefixes require external bytes. Cache those ambiguous keys in
        // one packed allocation; distinct-prefix keys remain demand-loaded.
        let mut needed = [false; (PAGE - HEADER) / BRANCH_ENTRY];
        for (i, pair) in entries.windows(2).enumerate() {
            let left = &pair[0].key;
            let right = &pair[1].key;
            let order = left.prefix[..left.len.min(16)].cmp(&right.prefix[..right.len.min(16)]);
            if order.is_eq() && left.len > 16 && right.len > 16 {
                needed[i] = true;
                needed[i + 1] = true;
            } else if !order.then(left.len.cmp(&right.len)).is_lt() {
                return Err(corrupt("index keys are not strictly sorted"));
            }
        }
        let key_bytes = entries
            .iter()
            .zip(needed)
            .filter(|(_, needed)| *needed)
            .try_fold(0usize, |n, (e, _)| n.checked_add(e.key.len)?.checked_add(4));
        let mut packed = Vec::new();
        if let Some(len) = key_bytes.filter(|len| *len <= self.budget / 2)
            && len != 0
        {
            let mut bytes = buffer(len)?;
            let mut at = 0;
            let mut run_offset = 0u64;
            let mut run_start = 0usize;
            // Combine exactly adjacent extents, never pull unrelated key bytes
            // or gaps into memory merely to reduce system calls.
            for (entry, needed) in entries.iter().zip(needed) {
                if !needed {
                    continue;
                }
                let key = &entry.key;
                if at > run_start && run_offset + (at - run_start) as u64 != key.offset {
                    self.read_key_bytes(run_offset, &mut bytes[run_start..at])?;
                    run_start = at;
                }
                if at == run_start {
                    run_offset = key.offset;
                }
                at += key.len + 4;
            }
            if at > run_start {
                self.read_key_bytes(run_offset, &mut bytes[run_start..at])?;
            }
            let mut at = 0;
            for (entry, needed) in entries.iter().zip(needed) {
                if !needed {
                    continue;
                }
                let key = &entry.key;
                let checksum =
                    u32::from_le_bytes(bytes[at + key.len..at + key.len + 4].try_into().unwrap());
                if self.verify_metadata
                    && (crc32fast::hash(&bytes[at..at + key.len]) != checksum
                        || bytes[at..at + 16] != key.prefix)
                {
                    return Err(corrupt("index key checksum mismatch"));
                }
                at += key.len + 4;
            }
            packed = bytes;
            let mut at = 0;
            for (entry, needed) in entries.iter_mut().zip(needed) {
                if !needed {
                    continue;
                }
                entry.key.decoded = Some(at);
                at += entry.key.len + 4;
            }
        }
        for (i, pair) in entries.windows(2).enumerate() {
            if !needed[i] || !needed[i + 1] {
                continue;
            }
            let left = &pair[0].key;
            let right = &pair[1].key;
            if left.prefix != right.prefix {
                continue;
            }
            let ordered = if let Some(key) = right.cached(&packed) {
                self.compare(left, key, &packed)?.is_lt()
            } else {
                self.compare(left, self.long_key(right)?.as_slice(), &packed)?
                    .is_lt()
            };
            if !ordered {
                return Err(corrupt("index keys are not strictly sorted"));
            }
        }
        Ok(Node {
            height,
            count,
            entries,
            key_bytes: packed,
        })
    }
    fn cached<T: Resident>(&self, offset: u64) -> Option<Lease<'_, T>> {
        let mut cache = self.cache.borrow_mut();
        let value = T::slots(&mut cache).get_mut(&offset)?.take()?;
        Some(Lease {
            value: Some(value),
            reader: self,
            offset,
            cached: true,
        })
    }
    fn retain<T: Resident>(&self, offset: u64, value: T) -> DiskIndexResult<Lease<'_, T>> {
        let charge = value.charge();
        let mut cache = self.cache.borrow_mut();
        let cached = !T::slots(&mut cache).contains_key(&offset)
            && Self::reserve(&mut cache, charge, self.budget);
        if cached {
            // Reserve both structures before charging or installing a slot.
            cache.order.try_reserve(1).map_err(oom)?;
            T::slots(&mut cache).try_reserve(1).map_err(oom)?;
            T::slots(&mut cache).insert(offset, None);
            cache.order.push_back(T::tag(offset));
            cache.bytes += charge;
        }
        Ok(Lease {
            value: Some(value),
            reader: self,
            offset,
            cached,
        })
    }
    fn store<T: Resident>(&self, offset: u64, value: T) -> DiskIndexResult<()> {
        let charge = value.charge();
        let mut cache = self.cache.borrow_mut();
        debug_assert!(!T::slots(&mut cache).contains_key(&offset));
        if Self::reserve(&mut cache, charge, self.budget) {
            cache.order.try_reserve(1).map_err(oom)?;
            T::slots(&mut cache).try_reserve(1).map_err(oom)?;
            T::slots(&mut cache).insert(offset, Some(value));
            cache.order.push_back(T::tag(offset));
            cache.bytes += charge;
        }
        Ok(())
    }
    fn charge(node: &Node) -> usize {
        256 + node.entries.capacity() * std::mem::size_of::<Entry>() + node.key_bytes.capacity()
    }
    fn reserve(cache: &mut Cache, charge: usize, budget: usize) -> bool {
        if charge > budget {
            return false;
        }
        // Each queue entry is examined at most once. Pinned parents remain
        // charged; a full pinned cache simply leaves the child uncached.
        let mut remaining = cache.order.len();
        while cache.bytes.checked_add(charge).is_none_or(|n| n > budget) {
            if remaining == 0 {
                return false;
            }
            remaining -= 1;
            let tag = cache
                .order
                .pop_front()
                .expect("cache accounting has an owner");
            let released = match tag {
                Cached::Page(offset) => match cache.pages.entry(offset) {
                    std::collections::hash_map::Entry::Occupied(entry) if entry.get().is_some() => {
                        Some(Self::charge(&entry.remove().unwrap()))
                    }
                    _ => None,
                },
                Cached::Key(offset) => match cache.keys.entry(offset) {
                    std::collections::hash_map::Entry::Occupied(entry) if entry.get().is_some() => {
                        Some(entry.remove().unwrap().capacity() + 128)
                    }
                    _ => None,
                },
            };
            if let Some(bytes) = released {
                cache.bytes -= bytes;
            } else {
                cache.order.push_back(tag);
            }
        }
        true
    }
    fn read_key_bytes(&self, offset: u64, bytes: &mut [u8]) -> DiskIndexResult<()> {
        read_at(&self.file, offset, bytes)?;
        #[cfg(test)]
        self.key_reads.set(self.key_reads.get() + 1);
        Ok(())
    }
    fn long_key(&self, key: &Key) -> DiskIndexResult<Lease<'_, Vec<u8>>> {
        if let Some(bytes) = self.cached::<Vec<u8>>(key.offset) {
            if bytes.len() != key.len || bytes[..16] != key.prefix {
                return Err(corrupt("inconsistent cached index key"));
            }
            return Ok(bytes);
        }
        let len = key
            .len
            .checked_add(4)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
        let mut bytes = buffer(len)?;
        self.read_key_bytes(key.offset, &mut bytes)?;
        let checksum = u32::from_le_bytes(bytes[key.len..].try_into().unwrap());
        if self.verify_metadata
            && (crc32fast::hash(&bytes[..key.len]) != checksum || bytes[..16] != key.prefix)
        {
            return Err(corrupt("index key checksum mismatch"));
        }
        bytes.truncate(key.len);
        self.retain(key.offset, bytes)
    }
    fn key(&self, key: &Key, packed: &[u8]) -> DiskIndexResult<Vec<u8>> {
        if let Some(bytes) = key.cached(packed) {
            copy_bytes(bytes)
        } else {
            copy_bytes(&self.long_key(key)?)
        }
    }
    fn compare(
        &self,
        stored: &Key,
        key: &[u8],
        packed: &[u8],
    ) -> DiskIndexResult<std::cmp::Ordering> {
        if let Some(bytes) = stored.cached(packed) {
            return Ok(bytes.cmp(key));
        }
        let prefix = &stored.prefix[..stored.len.min(16)];
        let comparison = prefix.cmp(&key[..key.len().min(16)]);
        if comparison != std::cmp::Ordering::Equal || stored.len <= 16 || key.len() <= 16 {
            return Ok(comparison.then(stored.len.cmp(&key.len())));
        }
        Ok(self.long_key(stored)?.as_slice().cmp(key))
    }
    fn search(&self, node: &Node, key: &[u8]) -> DiskIndexResult<Result<usize, usize>> {
        let entries = &node.entries;
        let (mut low, mut high) = (0, entries.len());
        while low < high {
            let mid = low + (high - low) / 2;
            match self.compare(&entries[mid].key, key, &node.key_bytes)? {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(Ok(mid)),
            }
        }
        Ok(Err(low))
    }
    fn cached_search(
        cache: &Cache,
        node: &Node,
        key: &[u8],
    ) -> DiskIndexResult<Option<Result<usize, usize>>> {
        let (mut low, mut high) = (0, node.entries.len());
        while low < high {
            let mid = low + (high - low) / 2;
            let stored = &node.entries[mid].key;
            let order = if let Some(bytes) = stored.cached(&node.key_bytes) {
                bytes.cmp(key)
            } else {
                let prefix = stored.prefix[..stored.len.min(16)].cmp(&key[..key.len().min(16)]);
                if !prefix.is_eq() || stored.len <= 16 || key.len() <= 16 {
                    prefix.then(stored.len.cmp(&key.len()))
                } else if let Some(Some(bytes)) = cache.keys.get(&stored.offset) {
                    if bytes.len() != stored.len || bytes[..16] != stored.prefix {
                        return Err(corrupt("inconsistent cached index key"));
                    }
                    bytes.as_slice().cmp(key)
                } else {
                    return Ok(None);
                }
            };
            match order {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(Some(Ok(mid))),
            }
        }
        Ok(Some(Err(low)))
    }
    pub fn get(&self, mut root: Root, key: &[u8]) -> DiskIndexResult<Option<[u8; LATEST_LEN]>> {
        if root.offset == 0 {
            if root.count != 0 {
                return Err(corrupt("invalid empty index"));
            }
            return Ok(None);
        }
        enum Step {
            Done(Option<[u8; LATEST_LEN]>),
            Child(Root, u8, Key),
        }
        let step = |node: &Node, position: Result<usize, usize>| {
            if node.height == 0 {
                return Step::Done(position.ok().map(|i| node.entries[i].value));
            }
            let at = match position {
                Ok(i) => i,
                Err(0) => return Step::Done(None),
                Err(i) => i - 1,
            };
            Step::Child(
                node.entries[at].child(),
                node.height - 1,
                node.entries[at].key.clone(),
            )
        };
        let mut expected_height = None;
        let mut minimum: Option<Key> = None;
        loop {
            let validate = |node: &Node| -> DiskIndexResult<()> {
                if node.count != root.count {
                    return Err(corrupt("index subtree count mismatch"));
                }
                if !height_matches(expected_height, node.height) {
                    return Err(corrupt("index height mismatch"));
                }
                if minimum.as_ref().is_some_and(|k| *k != node.entries[0].key) {
                    return Err(corrupt("index separator mismatch"));
                }
                Ok(())
            };
            // Warm point reads borrow the resident value once. They neither
            // move cache entries nor increment reference counts. A cold long
            // key releases this borrow before the fallible cache fill below.
            let (cached, resident) = {
                let cache = self.cache.borrow();
                match cache.pages.get(&root.offset) {
                    Some(Some(node)) => {
                        validate(node)?;
                        (
                            Self::cached_search(&cache, node, key)?
                                .map(|position| step(node, position)),
                            true,
                        )
                    }
                    Some(None) => (None, true),
                    None => (None, false),
                }
            };
            let next = match cached {
                Some(step) => step,
                None if !resident => {
                    let node = self.read_node(root)?;
                    validate(&node)?;
                    let next = step(&node, self.search(&node, key)?);
                    self.store(root.offset, node)?;
                    next
                }
                None => {
                    let node = self.node(root)?;
                    validate(&node)?;
                    step(&node, self.search(&node, key)?)
                }
            };
            match next {
                Step::Done(value) => return Ok(value),
                Step::Child(child, height, key) => {
                    root = child;
                    expected_height = Some(height);
                    minimum = Some(key);
                }
            }
        }
    }
    pub fn visit(
        &self,
        root: Root,
        visitor: &mut impl FnMut(Vec<u8>, [u8; LATEST_LEN]) -> DiskIndexResult<()>,
    ) -> DiskIndexResult<()> {
        if root.offset == 0 {
            if root.count != 0 {
                return Err(corrupt("invalid empty index"));
            }
            return Ok(());
        }
        // Nodes occupy disjoint PAGE-byte extents in a valid snapshot. Include
        // historical pages and key extents: this conservative ceiling cannot
        // reject a valid tree, even with unaligned append offsets or old roots.
        self.visit_at(
            root,
            None,
            None,
            None,
            &mut (self.end / PAGE as u64),
            visitor,
        )
    }
    fn before(
        &self,
        left: &Key,
        left_packed: &[u8],
        right: &Key,
        right_packed: &[u8],
    ) -> DiskIndexResult<bool> {
        let order = left.prefix[..left.len.min(16)].cmp(&right.prefix[..right.len.min(16)]);
        if !order.is_eq() || left.len <= 16 || right.len <= 16 {
            return Ok(order.then(left.len.cmp(&right.len)).is_lt());
        }
        if let Some(bytes) = right.cached(right_packed) {
            Ok(self.compare(left, bytes, left_packed)?.is_lt())
        } else {
            Ok(self
                .compare(left, &self.long_key(right)?, left_packed)?
                .is_lt())
        }
    }
    fn visit_at(
        &self,
        root: Root,
        height: Option<u8>,
        minimum: Option<&Key>,
        upper: Option<(&Key, &[u8])>,
        remaining: &mut u64,
        visitor: &mut impl FnMut(Vec<u8>, [u8; LATEST_LEN]) -> DiskIndexResult<()>,
    ) -> DiskIndexResult<()> {
        take_visit(remaining)
            .ok_or_else(|| corrupt("index traversal exceeds checkpoint extent"))?;
        let node = self.node(root)?;
        if minimum.is_some_and(|key| *key != node.entries[0].key) {
            return Err(corrupt("index separator mismatch"));
        }
        if !height_matches(height, node.height) {
            return Err(corrupt("index height mismatch"));
        }
        if let Some((bound, packed)) = upper
            && !self.before(
                &node.entries.last().unwrap().key,
                &node.key_bytes,
                bound,
                packed,
            )?
        {
            return Err(corrupt("index child exceeds separator range"));
        }
        for (i, entry) in node.entries.iter().enumerate() {
            if node.height == 0 {
                visitor(self.key(&entry.key, &node.key_bytes)?, entry.value)?;
            } else {
                self.visit_at(
                    entry.child(),
                    Some(node.height - 1),
                    Some(&entry.key),
                    node.entries
                        .get(i + 1)
                        .map(|next| (&next.key, node.key_bytes.as_slice()))
                        .or(upper),
                    remaining,
                    visitor,
                )?;
            }
        }
        Ok(())
    }
}

/// Rewrites each affected node once per batch, retaining unchanged subtrees.
pub(super) fn apply(
    file: &File,
    reader: &Reader,
    root: Root,
    updates: &BTreeMap<Vec<u8>, [u8; LATEST_LEN]>,
) -> DiskIndexResult<Root> {
    apply_with_limit(file, reader, root, updates, None)
}

pub(super) fn apply_with_limit(
    file: &File,
    reader: &Reader,
    root: Root,
    updates: &BTreeMap<Vec<u8>, [u8; LATEST_LEN]>,
    maximum: Option<u64>,
) -> DiskIndexResult<Root> {
    if updates.is_empty() {
        return Ok(root);
    }
    let mut writer = Writer {
        file,
        reader,
        end: file.metadata()?.len().max(PAGE as u64),
        maximum,
    };
    let mut sorted = Vec::new();
    sorted.try_reserve_exact(updates.len()).map_err(oom)?;
    sorted.extend(updates.iter());
    let updates = sorted;
    let height = if root.offset == 0 {
        0
    } else {
        reader.node(root)?.height
    };
    let mut branches = writer.update(root, height, &updates)?;
    let mut level = height;
    while branches.len() > 1 {
        level = level
            .checked_add(1)
            .filter(|h| *h <= 31)
            .ok_or_else(|| corrupt("index height exhausted"))?;
        branches = writer.nodes(level, branches)?;
    }
    Ok(branches[0].child())
}

struct Writer<'a> {
    file: &'a File,
    reader: &'a Reader,
    end: u64,
    maximum: Option<u64>,
}

impl Writer<'_> {
    fn append(&mut self, bytes: &[u8]) -> DiskIndexResult<u64> {
        let start = self.end;
        self.end = start
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| corrupt("index file size overflow"))?;
        crate::spill_file::check_extent(self.maximum, self.end)?;
        write_at(self.file, start, bytes)?;
        Ok(start)
    }
    fn key(&mut self, bytes: &[u8]) -> DiskIndexResult<Key> {
        if bytes.is_empty() || bytes.len() > self.reader.max_key || bytes.len() > u32::MAX as usize
        {
            return Err(corrupt("invalid index update key"));
        }
        let mut prefix = [0; 16];
        prefix[..bytes.len().min(16)].copy_from_slice(&bytes[..bytes.len().min(16)]);
        let offset = if bytes.len() > 16 {
            let offset = self.append(bytes)?;
            self.append(&crc32fast::hash(bytes).to_le_bytes())?;
            offset
        } else {
            0
        };
        Ok(Key {
            offset,
            len: bytes.len(),
            prefix,
            decoded: None,
        })
    }
    fn nodes(&mut self, height: u8, entries: Vec<Entry>) -> DiskIndexResult<Vec<Entry>> {
        let mut output = Vec::new();
        output
            .try_reserve_exact(entries.len().div_ceil(fanout(height)))
            .map_err(oom)?;
        for chunk in entries.chunks(fanout(height)) {
            let mut bytes = [0; PAGE];
            bytes[..8].copy_from_slice(MAGIC);
            bytes[8] = height;
            bytes[10..12].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
            let mut count = 0u64;
            for (entry, into) in chunk
                .iter()
                .zip(bytes[HEADER..].chunks_exact_mut(entry_size(height)))
            {
                into[..8].copy_from_slice(&entry.key.offset.to_le_bytes());
                into[8..12].copy_from_slice(&(entry.key.len as u32).to_le_bytes());
                into[12..28].copy_from_slice(&entry.key.prefix);
                let value_len = into.len() - 28;
                into[28..].copy_from_slice(&entry.value[..value_len]);
                count = count
                    .checked_add(if height == 0 { 1 } else { entry.child().count })
                    .ok_or_else(|| corrupt("index count overflow"))?;
            }
            let checksum = crc32fast::hash(&bytes);
            bytes[16..20].copy_from_slice(&checksum.to_le_bytes());
            let offset = self.append(&bytes)?;
            output.push(Entry::branch(chunk[0].key.clone(), Root { offset, count }));
        }
        Ok(output)
    }
    fn update(
        &mut self,
        root: Root,
        height: u8,
        updates: &[(&Vec<u8>, &[u8; LATEST_LEN])],
    ) -> DiskIndexResult<Vec<Entry>> {
        let node = if root.offset == 0 {
            None
        } else {
            Some(self.reader.node(root)?)
        };
        if node.as_ref().is_some_and(|n| n.height != height) {
            return Err(corrupt("index height mismatch"));
        }
        let entries = node.as_ref().map_or(&[][..], |n| n.entries.as_slice());
        let packed = node.as_ref().map_or(&[][..], |n| n.key_bytes.as_slice());
        let mut output = Vec::new();
        output
            .try_reserve(entries.len().saturating_add(updates.len()))
            .map_err(oom)?;
        if height == 0 {
            let mut old = 0;
            for (key, value) in updates {
                while old < entries.len()
                    && self.reader.compare(&entries[old].key, key, packed)?.is_lt()
                {
                    output.push(entries[old].clone());
                    old += 1;
                }
                let stored_key = if old < entries.len()
                    && self.reader.compare(&entries[old].key, key, packed)?.is_eq()
                {
                    let k = entries[old].key.clone();
                    old += 1;
                    k
                } else {
                    self.key(key)?
                };
                output.push(Entry {
                    key: stored_key,
                    value: **value,
                });
            }
            output.extend_from_slice(&entries[old..]);
        } else {
            let mut consumed = 0;
            for (i, entry) in entries.iter().enumerate() {
                let begin = consumed;
                while consumed < updates.len()
                    && (i + 1 == entries.len()
                        || self
                            .reader
                            .compare(&entries[i + 1].key, updates[consumed].0, packed)?
                            .is_gt())
                {
                    consumed += 1;
                }
                if begin == consumed {
                    output.push(entry.clone());
                } else {
                    let children =
                        self.update(entry.child(), height - 1, &updates[begin..consumed])?;
                    output.try_reserve(children.len()).map_err(oom)?;
                    output.extend(children);
                }
            }
            if consumed != updates.len() {
                return Err(corrupt("unapplied index updates"));
            }
        }
        self.nodes(height, output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    fn fixture() -> (tempfile::TempDir, Arc<File>) {
        let dir = tempfile::tempdir().unwrap();
        let file = Arc::new(
            OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(dir.path().join("tree"))
                .unwrap(),
        );
        file.set_len(PAGE as u64).unwrap();
        (dir, file)
    }
    fn reader(file: &Arc<File>, budget: usize) -> Reader {
        Reader::new(file.clone(), file.metadata().unwrap().len(), 4096, budget)
    }
    fn value(id: u64) -> [u8; LATEST_LEN] {
        let mut value = [0; LATEST_LEN];
        value[..8].copy_from_slice(&id.to_le_bytes());
        value
    }
    fn repair_crc(bytes: &mut [u8; PAGE]) {
        bytes[16..20].fill(0);
        let crc = crc32fast::hash(bytes);
        bytes[16..20].copy_from_slice(&crc.to_le_bytes());
    }
    fn assert_cache_accounting(reader: &Reader) {
        let cache = reader.cache.borrow();
        let actual = cache
            .pages
            .values()
            .map(|n| Reader::charge(n.as_ref().unwrap()))
            .sum::<usize>()
            + cache
                .keys
                .values()
                .map(|k| k.as_ref().unwrap().capacity() + 128)
                .sum::<usize>();
        assert_eq!(cache.bytes, actual);
        assert!(actual <= reader.budget);
        assert_eq!(cache.order.len(), cache.pages.len() + cache.keys.len());
    }

    #[test]
    fn reserved_storage_pages_cannot_supply_nodes_or_long_keys() {
        for (count, key_len) in [(1u64, 8), (1, 128), (200, 8)] {
            let (_dir, file) = fixture();
            let updates = (0..count)
                .map(|id| (id.to_be_bytes().repeat(key_len / 8), value(id)))
                .collect();
            let root = apply(&file, &reader(&file, 0), Root::default(), &updates).unwrap();
            let key = vec![0; key_len];
            assert!(reader(&file, 0).get(root, &key).unwrap().is_some());
            let mut protected = reader(&file, 0).with_minimum_offset(24576);
            assert!(protected.get(root, &key).is_err());
            if count > 1 || key_len > 16 {
                // Move only the root into the allowed extent: its child/key
                // references still target reserved publication storage.
                let mut bytes = [0; PAGE];
                read_at(&file, root.offset, &mut bytes).unwrap();
                let offset = file.metadata().unwrap().len().max(24576);
                write_at(&file, offset, &bytes).unwrap();
                protected = reader(&file, 0).with_minimum_offset(24576);
                assert!(protected.get(Root { offset, ..root }, &key).is_err());
            }
        }
    }

    #[test]
    fn distinct_long_keys_are_loaded_on_demand_and_crc_checked() {
        fn assert_send<T: Send>() {}
        assert_send::<Reader>();
        let (_dir, file) = fixture();
        let updates: BTreeMap<_, _> = (0..40u64)
            .map(|id| (id.to_be_bytes().repeat(16), value(id)))
            .collect();
        let root = apply(
            &file,
            &reader(&file, 1024 * 1024),
            Root::default(),
            &updates,
        )
        .unwrap();
        let read = reader(&file, 1024 * 1024);
        let node = read.node(root).unwrap();
        let damaged_key_offset = node.entries[5].key.offset;
        drop(node);
        assert_eq!(
            read.key_reads.get(),
            0,
            "opening a page must not fetch unrelated long keys"
        );
        let query = 20u64.to_be_bytes().repeat(16);
        assert_eq!(read.get(root, &query).unwrap(), Some(value(20)));
        assert_eq!(read.key_reads.get(), 1);
        for _ in 0..100 {
            assert_eq!(read.get(root, &query).unwrap(), Some(value(20)));
        }
        assert_eq!(
            read.key_reads.get(),
            1,
            "repeated lookup should reuse the verified key"
        );
        assert_eq!(read.page_reads.get(), 1);
        assert_cache_accounting(&read);
        write_at(&file, damaged_key_offset + 20, &[0xff]).unwrap();
        let read = reader(&file, 1024 * 1024);
        assert_eq!(read.get(root, &query).unwrap(), Some(value(20)));
        assert!(read.get(root, &5u64.to_be_bytes().repeat(16)).is_err());
    }

    #[test]
    fn ambiguous_long_prefixes_still_validate_full_order() {
        let (_dir, file) = fixture();
        let updates: BTreeMap<_, _> = (0..40u64)
            .map(|id| {
                let mut key = vec![b'x'; 128];
                key.extend_from_slice(&id.to_be_bytes());
                (key, value(id))
            })
            .collect();
        let root = apply(
            &file,
            &reader(&file, 1024 * 1024),
            Root::default(),
            &updates,
        )
        .unwrap();
        let read = reader(&file, 1024 * 1024);
        read.node(root).unwrap();
        assert_eq!(
            read.key_reads.get(),
            1,
            "adjacent ambiguous keys share one positional read"
        );
        for (key, value) in &updates {
            assert_eq!(read.get(root, key).unwrap(), Some(*value));
        }
        assert_eq!(
            read.key_reads.get(),
            1,
            "adjacent ambiguous keys share one positional read"
        );
        let mut bytes = [0; PAGE];
        read_at(&file, root.offset, &mut bytes).unwrap();
        for i in 0..LEAF_ENTRY {
            bytes.swap(HEADER + i, HEADER + LEAF_ENTRY + i);
        }
        repair_crc(&mut bytes);
        write_at(&file, root.offset, &bytes).unwrap();
        assert!(
            reader(&file, 1024 * 1024).node(root).is_err(),
            "even a repaired page CRC must not hide unsorted keys"
        );
    }

    #[test]
    fn compact_branches_preserve_split_boundaries_and_old_roots() {
        let (_dir, file) = fixture();
        // One more than a full root's leaf capacity forces a second branch level.
        let count = fanout(0) * fanout(1) + 1;
        let initial: BTreeMap<_, _> = (0..count as u64)
            .map(|id| ((id * 2 + 2).to_be_bytes().to_vec(), value(id)))
            .collect();
        let old = apply(
            &file,
            &reader(&file, 1024 * 1024),
            Root::default(),
            &initial,
        )
        .unwrap();
        let pinned = reader(&file, 1024 * 1024);
        assert_eq!(pinned.node(old).unwrap().height, 2);
        let mut changes: BTreeMap<_, _> = initial
            .keys()
            .step_by(49)
            .map(|key| (key.clone(), value(u64::MAX)))
            .collect();
        changes.insert(0u64.to_be_bytes().to_vec(), value(7)); // new global minimum
        let root = apply(&file, &pinned, old, &changes).unwrap();
        let read = reader(&file, 1024 * 1024);
        // Keep the complete height-two tree and update set under Miri, while
        // querying both sides of every leaf boundary and every changed key.
        // Native/ASan still query all keys and all interleaved misses.
        let check_id = |id: usize| {
            !cfg!(miri)
                || id < 2
                || id + 2 >= count
                || id.is_multiple_of(fanout(0))
                || (id + 1).is_multiple_of(fanout(0))
                || id.is_multiple_of(49)
        };
        for (id, (key, before)) in initial.iter().enumerate() {
            if !check_id(id) {
                continue;
            }
            assert_eq!(pinned.get(old, key).unwrap(), Some(*before));
            assert_eq!(
                read.get(root, key).unwrap(),
                Some(*changes.get(key).unwrap_or(before))
            );
        }
        for id in 0..=count as u64 {
            if !check_id(id as usize) {
                continue;
            }
            assert_eq!(read.get(root, &(id * 2 + 1).to_be_bytes()).unwrap(), None);
        }
        assert_eq!(read.get(root, &0u64.to_be_bytes()).unwrap(), Some(value(7)));
        assert_eq!(pinned.get(old, &0u64.to_be_bytes()).unwrap(), None);
        let mut bytes = [0; PAGE];
        read_at(&file, root.offset, &mut bytes).unwrap();
        bytes[HEADER + 28..HEADER + 36].copy_from_slice(&root.offset.to_le_bytes());
        repair_crc(&mut bytes);
        write_at(&file, root.offset, &bytes).unwrap();
        assert!(
            reader(&file, 1024 * 1024)
                .get(root, &0u64.to_be_bytes())
                .is_err()
        );
    }

    #[test]
    fn mixed_page_and_key_eviction_stays_bounded() {
        let (_dir, file) = fixture();
        let updates: BTreeMap<_, _> = (0..500u64)
            .map(|id| (id.to_be_bytes().repeat(16), value(id)))
            .collect();
        let root = apply(
            &file,
            &reader(&file, 1024 * 1024),
            Root::default(),
            &updates,
        )
        .unwrap();
        for budget in [1024, 8192, 16384] {
            let read = reader(&file, budget);
            // Preserve all 500 stored long keys and all cache budgets. Miri
            // samples the same distributed permutation twice; native/ASan
            // still check both complete permutations. The sample traverses
            // more leaf pages than any of these caches can retain.
            let probes = if cfg!(miri) { 64u64 } else { 500 };
            for _ in 0..2 {
                for id in (0..probes).map(|n| n * 137 % 500) {
                    assert_eq!(
                        read.get(root, &id.to_be_bytes().repeat(16)).unwrap(),
                        Some(value(id))
                    );
                    assert_cache_accounting(&read);
                }
            }
        }
    }

    #[test]
    fn previous_page_format_is_rejected() {
        let (_dir, file) = fixture();
        let updates = BTreeMap::from([(b"key".to_vec(), value(1))]);
        let root = apply(
            &file,
            &reader(&file, 1024 * 1024),
            Root::default(),
            &updates,
        )
        .unwrap();
        let mut bytes = [0; PAGE];
        read_at(&file, root.offset, &mut bytes).unwrap();
        bytes[..8].copy_from_slice(b"VIXPAGE1");
        repair_crc(&mut bytes);
        write_at(&file, root.offset, &bytes).unwrap();
        assert!(reader(&file, 1024 * 1024).get(root, b"key").is_err());
    }
}

#[cfg(test)]
mod corruption_guards {
    use super::*;
    #[test]
    fn key_extent_overflow_returns_an_allocation_error_before_io() {
        let file = Arc::new(tempfile::tempfile().unwrap());
        let reader = Reader::new(file, 0, usize::MAX, 1024);
        let key = Key {
            offset: 0,
            len: usize::MAX,
            prefix: [0; 16],
            decoded: None,
        };
        assert!(matches!(
            reader.long_key(&key),
            Err(DiskIndexError::Io(error)) if error.kind() == std::io::ErrorKind::OutOfMemory
        ));
        assert_eq!(reader.key_reads.get(), 0);
    }
    fn page(height: u8, child: Root) -> [u8; PAGE] {
        let mut bytes = [0; PAGE];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8] = height;
        let count: u16 = if height == 0 { 1 } else { 2 };
        bytes[10..12].copy_from_slice(&count.to_le_bytes());
        for i in 0..count as usize {
            let at = HEADER + i * entry_size(height);
            bytes[at + 8..at + 12].copy_from_slice(&8u32.to_le_bytes());
            bytes[at + 12..at + 20].copy_from_slice(&((i + 1) as u64).to_be_bytes());
            if height != 0 {
                bytes[at + 28..at + 36].copy_from_slice(&child.offset.to_le_bytes());
                bytes[at + 36..at + 44].copy_from_slice(&child.count.to_le_bytes());
            }
        }
        let checksum = crc32fast::hash(&bytes);
        bytes[16..20].copy_from_slice(&checksum.to_le_bytes());
        bytes
    }
    fn image(height: u8) -> (Arc<File>, Root) {
        let file = Arc::new(tempfile::tempfile().unwrap());
        file.set_len(PAGE as u64).unwrap();
        let mut root = Root::default();
        for level in 0..=height {
            let bytes = page(level, root);
            let offset = file.metadata().unwrap().len();
            write_at(&file, offset, &bytes).unwrap();
            root = Root {
                offset,
                count: if level == 0 { 1 } else { root.count * 2 },
            };
        }
        (file, root)
    }
    #[test]
    fn hostile_child_with_short_reads_io_errors_and_allocation_refusal_recovers() {
        use std::io::ErrorKind;
        let (file, root) = image(1);
        let valid = page(0, Root::default());
        let mut hostile = valid;
        hostile[8] = 255;
        hostile[16..20].fill(0);
        let crc = crc32fast::hash(&hostile);
        hostile[16..20].copy_from_slice(&crc.to_le_bytes());
        let end = file.metadata().unwrap().len();
        let mut trials = 0;
        let mut io_errors = 0;
        let mut allocation_errors = 0;
        for verify_metadata in [true, false] {
            for chunk in [31, 4096] {
                // Fail before reading, inside the root, and inside the bad child;
                // MAX lets the CRC-valid hostile shape itself cause rejection.
                let root_calls = PAGE.div_ceil(chunk);
                for fail_at in [1, 2, root_calls + 2, usize::MAX] {
                    for kind in [ErrorKind::UnexpectedEof, ErrorKind::PermissionDenied] {
                        for allocation in [1, 2, 3, 4, usize::MAX] {
                            write_at(&file, PAGE as u64, &hostile).unwrap();
                            let read = Reader::new(file.clone(), end, 4096, 8192)
                                .with_metadata_verification(verify_metadata);
                            let (result, calls) = with_read_fault(chunk, fail_at, kind, || {
                                crate::pib_probe::with_allocation_refusal(allocation, || {
                                    read.get(root, &1u64.to_be_bytes())
                                })
                            });
                            assert!(result.is_err(), "hostile child accepted");
                            // A native syscall may return LESS than the supplied chunk
                            // (Miri exercises this). Each non-interrupted success
                            // advances at least one byte; at most one terminal error.
                            assert!(calls <= 2 * PAGE + 1, "unbounded read work: {calls}");
                            if let Err(DiskIndexError::Io(error)) = &result {
                                io_errors += usize::from(error.kind() == kind);
                                allocation_errors +=
                                    usize::from(error.kind() == ErrorKind::OutOfMemory);
                            }
                            // Repair only the rejected child. Retry on the SAME
                            // reader, including any partially warmed root cache.
                            write_at(&file, PAGE as u64, &valid).unwrap();
                            assert!(read.get(root, &1u64.to_be_bytes()).unwrap().is_some());
                            let cache = read.cache.borrow();
                            assert!(cache.pages.values().all(Option::is_some));
                            assert!(cache.keys.values().all(Option::is_some));
                            assert!(cache.bytes <= read.budget);
                            drop(cache);
                            drop(read);
                            assert_eq!(Arc::strong_count(&file), 1);
                            trials += 1;
                        }
                    }
                }
            }
        }
        assert!(io_errors > 0 && allocation_errors > 0);
        assert_eq!(trials, 160);
        println!(
            "COMPOUND_READ trials={trials} retries={trials} bounded=true strict_and_trusted=true"
        );
    }

    #[test]
    fn duplicated_subtrees_stop_before_repeated_leaf_delivery() {
        for height in [4, 8, 12] {
            let (file, root) = image(height);
            let bytes = file.metadata().unwrap().len();
            let read = Reader::new(file, bytes, 4096, 1024 * 1024);
            assert!(read.get(root, &1u64.to_be_bytes()).unwrap().is_some());
            let mut visits = 0;
            let result = read.visit(root, &mut |_, _| {
                visits += 1;
                Ok(())
            });
            assert!(
                result.is_err(),
                "shared child must not masquerade as a second subtree"
            );
            assert!(visits <= 1, "repeated leaf reached the visitor");
            assert!(
                read.get(root, &1u64.to_be_bytes()).unwrap().is_some(),
                "failure left a cache lease detached"
            );
        }
    }
    #[test]
    fn direct_cycles_are_rejected_by_lookup_and_visit() {
        let (file, mut root) = image(1);
        let bytes = page(1, root);
        write_at(&file, root.offset, &bytes).unwrap();
        root.count *= 2;
        let read = Reader::new(
            file.clone(),
            file.metadata().unwrap().len(),
            4096,
            1024 * 1024,
        );
        assert!(read.get(root, &1u64.to_be_bytes()).is_err());
        assert!(read.visit(root, &mut |_, _| Ok(())).is_err());
    }
    #[test]
    fn checksummed_hostile_heights_and_counts_terminate() {
        let mut cases = 0;
        for verify_metadata in [true, false] {
            for height in 1..=u8::MAX {
                for count in [1, u64::MAX / 2, u64::MAX] {
                    let file = Arc::new(tempfile::tempfile().unwrap());
                    let root = Root {
                        offset: PAGE as u64,
                        count,
                    };
                    // Self-references with freshly recomputed, valid CRCs.
                    write_at(&file, root.offset, &page(height, root)).unwrap();
                    let read = Reader::new(file, 2 * PAGE as u64, 4096, 0)
                        .with_metadata_verification(verify_metadata);
                    assert!(read.get(root, &1u64.to_be_bytes()).is_err());
                    let mut delivered = 0;
                    assert!(
                        read.visit(root, &mut |_, _| {
                            delivered += 1;
                            Ok(())
                        })
                        .is_err()
                    );
                    assert_eq!(delivered, 0);
                    cases += 1;
                }
            }
            // Valid, backward child extents reach the height check (cycles above
            // can be rejected earlier by the extent/shape checks).
            for height in 2..=31 {
                let (file, leaf) = image(0);
                let root = Root {
                    offset: 2 * PAGE as u64,
                    count: 2,
                };
                write_at(&file, root.offset, &page(height, leaf)).unwrap();
                let read = Reader::new(file, 3 * PAGE as u64, 4096, 0)
                    .with_metadata_verification(verify_metadata);
                assert!(read.get(root, &1u64.to_be_bytes()).is_err());
                assert!(
                    read.page_reads.get() >= 2,
                    "child height check was not reached"
                );
                let mut delivered = 0;
                assert!(
                    read.visit(root, &mut |_, _| {
                        delivered += 1;
                        Ok(())
                    })
                    .is_err()
                );
                assert_eq!(delivered, 0);
                cases += 1;
            }
        }
        println!("HOSTILE_INDEX valid_crc_cases={cases} delivered=0");
    }

    #[test]
    fn child_keys_cannot_overlap_the_next_separator() {
        for long in [false, true] {
            let file = Arc::new(tempfile::tempfile().unwrap());
            file.set_len(PAGE as u64).unwrap();
            let values: BTreeMap<_, _> = (0..150u64)
                .map(|n| {
                    let mut key = if long { vec![b'x'; 32] } else { Vec::new() };
                    key.extend_from_slice(&n.to_be_bytes());
                    (key, [0; LATEST_LEN])
                })
                .collect();
            let initial = Reader::new(file.clone(), PAGE as u64, 4096, 1024 * 1024);
            let root = apply(&file, &initial, Root::default(), &values).unwrap();
            let end = file.metadata().unwrap().len();
            let clean = Reader::new(file.clone(), end, 4096, 1024 * 1024);
            let parent = clean.node(root).unwrap();
            assert_eq!(parent.height, 1);
            let child = parent.entries[0].child();
            let next = &parent.entries[1].key;
            let mut bytes = [0; PAGE];
            read_at(&file, child.offset, &mut bytes).unwrap();
            let count = u16::from_le_bytes(bytes[10..12].try_into().unwrap()) as usize;
            let at = HEADER + (count - 1) * LEAF_ENTRY;
            bytes[at..at + 8].copy_from_slice(&next.offset.to_le_bytes());
            bytes[at + 8..at + 12].copy_from_slice(&(next.len as u32).to_le_bytes());
            bytes[at + 12..at + 28].copy_from_slice(&next.prefix);
            bytes[16..20].fill(0);
            let crc = crc32fast::hash(&bytes);
            bytes[16..20].copy_from_slice(&crc.to_le_bytes());
            write_at(&file, child.offset, &bytes).unwrap();
            let damaged = Reader::new(file, end, 4096, 1024 * 1024);
            // The leaf is locally sorted and CRC-valid; only its inherited
            // exclusive upper bound reveals this cross-page inconsistency.
            assert!(damaged.node(child).is_ok());
            let mut visits = 0;
            assert!(
                damaged
                    .visit(root, &mut |_, _| {
                        visits += 1;
                        Ok(())
                    })
                    .is_err()
            );
            assert_eq!(visits, 0);
        }
    }
    #[test]
    fn old_and_rewritten_roots_visit_each_key_once_with_tiny_caches() {
        let file = Arc::new(tempfile::tempfile().unwrap());
        file.set_len(PAGE as u64).unwrap();
        let values: BTreeMap<_, _> = (0..350u64)
            .map(|n| (n.to_be_bytes().to_vec(), [0; LATEST_LEN]))
            .collect();
        let initial = Reader::new(file.clone(), PAGE as u64, 4096, 1024);
        let old = apply(&file, &initial, Root::default(), &values).unwrap();
        let pinned = Reader::new(file.clone(), file.metadata().unwrap().len(), 4096, 1024);
        let changes = BTreeMap::from([(125u64.to_be_bytes().to_vec(), [1; LATEST_LEN])]);
        let new = apply(&file, &pinned, old, &changes).unwrap();
        let current = Reader::new(file.clone(), file.metadata().unwrap().len(), 4096, 1024);
        let node = current.node(new).unwrap();
        assert!(
            node.entries
                .windows(2)
                .any(|p| p[0].child().offset > p[1].child().offset)
        );
        drop(node);
        for (read, root) in [(&pinned, old), (&current, new)] {
            let mut count = 0u64;
            read.visit(root, &mut |key, _| {
                assert_eq!(key.as_slice(), count.to_be_bytes());
                count += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(count, 350);
            let failed = read.visit(root, &mut |_, _| {
                Err(std::io::Error::from(std::io::ErrorKind::OutOfMemory).into())
            });
            assert!(failed.is_err());
            assert!(read.get(root, &125u64.to_be_bytes()).unwrap().is_some());
        }
    }
}

#[cfg_attr(kani, kani::ensures(|result: &bool| *result == expected.is_none_or(|height| height == actual)))]
fn height_matches(expected: Option<u8>, actual: u8) -> bool {
    expected.is_none_or(|height| height == actual)
}

// Success consumes exactly one visit before reading attacker-controlled nodes.
#[cfg_attr(kani, kani::modifies(remaining))]
#[cfg_attr(kani, kani::ensures(|result: &Option<()>| {
    let before = old(*remaining);
    result.is_some() == (before != 0)
        && *remaining == if before == 0 { 0 } else { before - 1 }
}))]
fn take_visit(remaining: &mut u64) -> Option<()> {
    *remaining = remaining.checked_sub(1)?;
    Some(())
}

#[cfg(kani)]
#[path = "../verification/traversal.rs"]
mod resource_proofs;

#[cfg(kani)]
#[path = "../verification/traversal_contracts.rs"]
mod contract_proofs;
