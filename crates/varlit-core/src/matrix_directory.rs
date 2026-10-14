//! VMAT v5 directory: counted forward slots plus a sparse, directly addressed
//! page-to-slot array in the matrix file. No reconstructed mirror or scratch file.
//! The containing generation supplies immutable, checksummed pages. Queries
//! borrow the active writer view or use this reader's pinned confirmed source.
use super::*;
use std::cell::Cell;
use std::collections::VecDeque;

#[derive(Clone, Debug, Default)]
struct DirectoryCache {
    pages: HashMap<u64, Arc<[u8; 4096]>>,
    order: VecDeque<u64>,
}

#[derive(Clone, Debug)]
pub(super) struct DirectDirectory {
    source: Box<Source>,
    pool: MatrixReadPool,
    base: u64,
    capacity: u64,
    count: u64,
    recent: Cell<Option<(u64, Option<u64>)>>,
    cache: RefCell<DirectoryCache>,
    cache_entries: usize,
}
impl DirectDirectory {
    pub(super) fn new(
        source: Source,
        base: u64,
        capacity: u64,
        count: u64,
        cache_bytes: usize,
    ) -> Result<Self> {
        if count > capacity || capacity > PAGE_INDEX_MAX_ENTRIES {
            return Err(Error::InvalidMatrixLayout);
        }
        base.checked_add(index_page_array_len(capacity)?)
            .ok_or(Error::InvalidMatrixLayout)?;
        Ok(Self {
            source: Box::new(source),
            pool: MatrixReadPool::new(),
            base,
            capacity,
            count,
            recent: Cell::new(None),
            cache: RefCell::default(),
            cache_entries: cache_bytes / (4096 + 128),
        })
    }
    pub(super) fn rebind(&mut self, source: &Source) {
        self.recent.set(None);
        *self.cache.get_mut() = DirectoryCache::default();
        *self.source = source.clone();
        self.pool = MatrixReadPool::new();
    }
    pub(super) fn len(&self) -> u64 {
        self.count
    }
    pub(super) fn set_len(&mut self, count: u64) {
        self.count = count;
        self.recent.set(None);
        *self.cache.get_mut() = DirectoryCache::default();
    }
    /// Separate any immutable cache windows retained by a staged clone before
    /// authoritative writes. The common uniquely owned case allocates nothing.
    pub(super) fn prepare_cache_update(
        &mut self,
        entries: &[(u64, [u8; 8])],
        header: Option<[u8; 8]>,
    ) {
        for (at, _) in entries
            .iter()
            .copied()
            .chain(header.map(|bytes| (self.base, bytes)))
        {
            let page = (at - self.base) / 4096;
            if let Some(bytes) = self.cache.get_mut().pages.get_mut(&page) {
                let _ = Arc::make_mut(bytes);
            }
        }
    }
    /// Apply only changed words after persistence. No I/O or allocation; the
    /// rest of the bounded cache stays warm across writer metadata updates.
    pub(super) fn install_changes(
        &mut self,
        count: u64,
        entries: &[(u64, [u8; 8])],
        header: Option<[u8; 8]>,
    ) {
        self.count = count;
        self.recent.set(None);
        for (at, update) in entries
            .iter()
            .copied()
            .chain(header.map(|bytes| (self.base, bytes)))
        {
            let relative = at - self.base;
            let page = relative / 4096;
            let within = (relative % 4096) as usize;
            let cache = self.cache.get_mut();
            if let Some(bytes) = cache.pages.get_mut(&page) {
                if let Some(bytes) = Arc::get_mut(bytes) {
                    bytes[within..within + 8].copy_from_slice(&update);
                } else {
                    // Defensive fallback if future staging retains another Arc
                    // after preparation: discard only this window, never mutate
                    // a held snapshot or allocate after the file write.
                    cache.pages.remove(&page);
                    cache.order.retain(|&at| at != page);
                }
            }
        }
    }
    pub(super) fn reverse_offset(&self, page: u64) -> Result<u64> {
        if page >= self.capacity {
            return Err(Error::InvalidMatrixLayout);
        }
        page_index_entry_offset(
            self.base,
            self.capacity
                .checked_add(page)
                .ok_or(Error::InvalidMatrixLayout)?,
        )
    }
    fn word(&self, offset: u64, read: BitmapRead<'_>) -> Result<u64> {
        let reader = MatrixRegionReader::with_backing(&self.source, &self.pool, read);
        if self.cache_entries == 0 {
            let mut bytes = [0; 8];
            reader.read_exact_at(offset, &mut bytes)?;
            count_open_bitmap_bytes_read(8);
            return Ok(u64::from_le_bytes(bytes));
        }
        let relative = offset
            .checked_sub(self.base)
            .ok_or(Error::InvalidMatrixLayout)?;
        let page = relative / 4096;
        let within = (relative % 4096) as usize;
        if let Some(bytes) = self.cache.borrow().pages.get(&page) {
            return Ok(u64::from_le_bytes(
                bytes[within..within + 8].try_into().unwrap(),
            ));
        }
        let len = (index_page_array_len(self.capacity)? - page * 4096).min(4096) as usize;
        let mut bytes = [0; 4096];
        reader.read_exact_at(self.base + page * 4096, &mut bytes[..len])?;
        count_open_bitmap_bytes_read(len as u64);
        let value = u64::from_le_bytes(bytes[within..within + 8].try_into().unwrap());
        let mut cache = self.cache.borrow_mut();
        while cache.pages.len() >= self.cache_entries {
            if let Some(old) = cache.order.pop_front() {
                cache.pages.remove(&old);
            }
        }
        cache
            .pages
            .try_reserve(1)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix directory cache",
                requested: 4096,
            })?;
        cache
            .order
            .try_reserve(1)
            .map_err(|_| Error::AllocationFailed {
                resource: "matrix directory cache",
                requested: 8,
            })?;
        cache.pages.insert(page, Arc::new(bytes));
        cache.order.push_back(page);
        Ok(value)
    }

    pub(super) fn slot(&self, slot: u64, read: BitmapRead<'_>) -> Result<Option<u64>> {
        if slot >= self.count {
            return Ok(None);
        }
        let raw = self.word(page_index_entry_offset(self.base, slot)?, read)?;
        if raw == 0 || raw > self.capacity {
            return Err(Error::MatrixFatalCorruption);
        }
        Ok(Some(raw - 1))
    }
    pub(super) fn first(&self, page: u64, read: BitmapRead<'_>) -> Result<Option<u64>> {
        if let Some((old, slot)) = self.recent.get()
            && old == page
        {
            return Ok(slot);
        }
        let raw = self.word(self.reverse_offset(page)?, read)?;
        if raw == 0 {
            self.recent.set(Some((page, None)));
            return Ok(None);
        }
        let slot = raw - 1;
        if slot >= self.count || self.slot(slot, read)? != Some(page) {
            return Err(Error::MatrixFatalCorruption);
        }
        self.recent.set(Some((page, Some(slot))));
        Ok(Some(slot))
    }
    pub(super) fn visit(
        &self,
        read: BitmapRead<'_>,
        mut f: impl FnMut(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.visit_rows(read, false, |page, slot| {
            if self.word(self.reverse_offset(page)?, read)? != slot + 1 {
                return Err(Error::MatrixFatalCorruption);
            }
            f(page, slot)
        })
    }
    pub(super) fn visit_candidates(
        &self,
        read: BitmapRead<'_>,
        f: impl FnMut(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.visit_rows(read, true, f)
    }
    fn visit_rows(
        &self,
        read: BitmapRead<'_>,
        tolerate_damage: bool,
        mut f: impl FnMut(u64, u64) -> Result<()>,
    ) -> Result<()> {
        // Streaming enumeration needs only one 4 KiB window. It is never retained
        // as an entry map and never writes an auxiliary file.
        let reader = MatrixRegionReader::with_backing(&self.source, &self.pool, read);
        let mut buffer = [0; 4096];
        let mut at = 0;
        while at < self.count {
            let n = (self.count - at).min(512) as usize;
            reader.read_exact_at(
                page_index_entry_offset(self.base, at)?,
                &mut buffer[..n * 8],
            )?;
            count_open_bitmap_bytes_read(n as u64 * 8);
            for (i, bytes) in buffer[..n * 8].as_chunks::<8>().0.iter().enumerate() {
                let raw = u64::from_le_bytes(*bytes);
                if raw == 0 || raw > self.capacity {
                    if tolerate_damage {
                        continue;
                    }
                    return Err(Error::MatrixFatalCorruption);
                }
                let page = raw - 1;
                let slot = at + i as u64;
                f(page, slot)?;
            }
            at += n as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    #[test]
    fn prepared_word_updates_keep_unrelated_windows_and_cloned_cache_bytes() -> Result<()> {
        let mut file = tempfile::tempfile()?;
        file.set_len(index_page_array_len(1024)?)?;
        file.write_all(&page_index_header_value(3)?.to_le_bytes())?;
        for page in [1000u64, 1, 513] {
            file.write_all(&(page + 1).to_le_bytes())?;
        }
        let mut directory = DirectDirectory::new(
            Source::Raw(Arc::new(file.try_clone()?)),
            0,
            1024,
            3,
            2 * (4096 + 128),
        )?;
        for (slot, page) in [1000, 1, 513].into_iter().enumerate() {
            file.seek(SeekFrom::Start(directory.reverse_offset(page)?))?;
            file.write_all(&(slot as u64 + 1).to_le_bytes())?;
        }
        assert_eq!(directory.first(1000, BitmapRead::Stored)?, Some(0));
        let held = directory.clone();
        let entries = [
            (directory.reverse_offset(1000)?, [0; 8]),
            (page_index_entry_offset(0, 0)?, 514u64.to_le_bytes()),
            (directory.reverse_offset(513)?, 1u64.to_le_bytes()),
        ];
        let header = page_index_header_value(2)?.to_le_bytes();
        directory.prepare_cache_update(&entries, Some(header));
        // Preparation separates shared bytes but must not expose a mutation.
        assert_eq!(directory.first(1000, BitmapRead::Stored)?, Some(0));
        for (at, bytes) in entries.iter().copied().chain(Some((0, header))) {
            file.seek(SeekFrom::Start(at))?;
            file.write_all(&bytes)?;
        }
        directory.install_changes(2, &entries, Some(header));
        assert_eq!(directory.cache.borrow().pages.len(), 2);
        assert_eq!(directory.first(1000, BitmapRead::Stored)?, None);
        assert_eq!(directory.first(513, BitmapRead::Stored)?, Some(0));
        // These reads deliberately exercise the held cache, not a raw-file
        // snapshot guarantee (production snapshots use immutable COW views).
        held.recent.set(None);
        assert_eq!(held.first(1000, BitmapRead::Stored)?, Some(0));
        assert_eq!(held.first(513, BitmapRead::Stored)?, Some(2));
        Ok(())
    }
}
