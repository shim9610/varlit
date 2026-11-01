//! Reader-local fixed-width temporary directories. The memory budget controls
//! buffering, never the number of rows that can be addressed.
use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};

#[derive(Debug)]
pub(crate) struct SpillRows<const N: usize> {
    file: Option<File>,
    spilled: usize,
    buffer: Vec<u8>,
    capacity: usize,
    read_cache: RefCell<(usize, Vec<u8>)>,
}
impl<const N: usize> SpillRows<N> {
    /// No heap buffering; used when a shared budget cannot spare one row.
    pub(crate) fn unbuffered() -> Self {
        Self {
            capacity: 0,
            ..Self::new(0)
        }
    }

    /// Transfer part of this buffer budget, not additional memory, to a
    /// secondary directory. Existing rows are durable before releasing RAM.
    pub(crate) fn lend_boundary_budget(&mut self) -> io::Result<usize> {
        let loan = (self.capacity / 2).min(16 * 1024);
        let retained = ((self.capacity - loan) / N * N).max(N);
        let available = self.capacity - retained;
        if available == 0 {
            return Ok(0);
        }
        if self.buffer.capacity() > retained && !self.buffer.is_empty() {
            if self.file.is_none() {
                self.file = Some(tempfile::tempfile()?);
            }
            let offset = (self.spilled as u64)
                .checked_mul(N as u64)
                .ok_or(io::ErrorKind::InvalidInput)?;
            let file = self.file.as_mut().unwrap();
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&self.buffer)?;
            self.spilled += self.buffer.len() / N;
        }
        if self.buffer.capacity() > retained {
            self.buffer = Vec::new();
        }
        if self.read_cache.get_mut().1.capacity() > retained.min(64 * 1024) {
            self.read_cache.get_mut().1 = Vec::new();
        }
        self.capacity = retained;
        Ok(available)
    }
    pub(crate) fn new(bytes: usize) -> Self {
        assert!(N != 0);
        Self {
            file: None,
            spilled: 0,
            buffer: Vec::new(),
            capacity: bytes.max(N) / N * N,
            read_cache: RefCell::new((0, Vec::new())),
        }
    }
    pub(crate) fn reserve_rows_hint(&mut self, count: usize) -> io::Result<()> {
        let bytes = count.saturating_mul(N).min(self.capacity);
        if bytes > self.buffer.capacity() {
            self.buffer
                .try_reserve_exact(bytes - self.buffer.len())
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        }
        Ok(())
    }
    pub(crate) fn len(&self) -> usize {
        self.spilled + self.buffer.len() / N
    }
    /// Search an already resident sorted page before probing the spill file.
    /// A miss outside its first/last keys makes no statement about other pages.
    pub(crate) fn cached_floor(&self, wanted: u64, key: impl Fn(&[u8]) -> u64) -> Option<[u8; N]> {
        fn search<const N: usize>(
            bytes: &[u8],
            wanted: u64,
            key: &impl Fn(&[u8]) -> u64,
        ) -> Option<[u8; N]> {
            let count = bytes.len() / N;
            if count == 0 || wanted < key(&bytes[..N]) || wanted > key(&bytes[(count - 1) * N..]) {
                return None;
            }
            // Fixed-size records are common, with occasional large internal
            // records between runs. Guess from the first adjacent spacing,
            // not the entire page span (whose last row can follow a large
            // checkpoint). Only exact equality proves membership; arbitrary
            // extents retain the binary search below.
            let first = key(&bytes[..N]);
            if count > 1 {
                let second = key(&bytes[N..2 * N]);
                if second > first {
                    let candidate = (wanted - first) / (second - first);
                    if candidate < count as u64 {
                        let at = candidate as usize * N;
                        let row = &bytes[at..at + N];
                        if key(row) == wanted {
                            return row.try_into().ok();
                        }
                    }
                }
            }
            let (mut low, mut high) = (0, count);
            while low < high {
                let middle = low + (high - low) / 2;
                if key(&bytes[middle * N..(middle + 1) * N]) <= wanted {
                    low = middle + 1;
                } else {
                    high = middle;
                }
            }
            bytes[(low - 1) * N..low * N].try_into().ok()
        }
        search(&self.buffer, wanted, &key)
            .or_else(|| search(&self.read_cache.borrow().1, wanted, &key))
    }
    /// All fallible work precedes the authoritative append. The caller may
    /// subsequently install exactly one row without I/O or allocation.
    pub(crate) fn reserve_one(&mut self) -> io::Result<()> {
        if self.buffer.len() >= self.capacity {
            if self.file.is_none() {
                self.file = Some(tempfile::tempfile()?);
            }
            let offset = (self.spilled as u64)
                .checked_mul(N as u64)
                .ok_or(io::ErrorKind::InvalidInput)?;
            let file = self.file.as_mut().unwrap();
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&self.buffer)?;
            self.spilled += self.buffer.len() / N;
            self.buffer.clear();
        }
        if self.buffer.capacity() - self.buffer.len() < N {
            let target = self
                .buffer
                .capacity()
                .saturating_mul(2)
                .max(N)
                .min(self.capacity);
            self.buffer
                .try_reserve_exact(target - self.buffer.len())
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        }
        Ok(())
    }
    pub(crate) fn push_reserved(&mut self, row: [u8; N]) {
        assert!(self.buffer.len() + N <= self.capacity);
        assert!(self.buffer.len() + N <= self.buffer.capacity());
        self.buffer.extend_from_slice(&row);
    }
    pub(crate) fn push(&mut self, row: [u8; N]) -> io::Result<()> {
        if self.capacity == 0 {
            if self.file.is_none() {
                self.file = Some(tempfile::tempfile()?);
            }
            let offset = (self.spilled as u64)
                .checked_mul(N as u64)
                .ok_or(io::ErrorKind::InvalidInput)?;
            let file = self.file.as_mut().unwrap();
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&row)?;
            self.spilled += 1;
            return Ok(());
        }
        self.reserve_one()?;
        self.push_reserved(row);
        Ok(())
    }
    /// Logical rollback only: no fallible I/O during an append rollback.
    /// Later spills overwrite any abandoned tail at its original offset.
    pub(crate) fn truncate(&mut self, len: usize) {
        if len >= self.len() {
            return;
        }
        self.read_cache.get_mut().1.clear();
        if len < self.spilled {
            self.spilled = len;
            self.buffer.clear();
        } else {
            self.buffer.truncate((len - self.spilled) * N);
        }
    }
    pub(crate) fn get(&self, index: usize) -> io::Result<Option<[u8; N]>> {
        self.get_window(index, 64 * 1024)
    }
    pub(crate) fn get_window(&self, index: usize, bytes: usize) -> io::Result<Option<[u8; N]>> {
        if self.capacity == 0 {
            return self.get_direct(index);
        }
        if index >= self.len() {
            return Ok(None);
        }
        let mut row = [0; N];
        if index >= self.spilled {
            let at = (index - self.spilled) * N;
            row.copy_from_slice(&self.buffer[at..at + N]);
        } else {
            let mut cache = self.read_cache.borrow_mut();
            if index >= cache.0 && index - cache.0 < cache.1.len() / N {
                let at = (index - cache.0) * N;
                row.copy_from_slice(&cache.1[at..at + N]);
                return Ok(Some(row));
            }
            let rows = (self.capacity.min(64 * 1024).min(bytes.max(N)) / N).max(1);
            // Small prediction windows are centered on the estimate so a
            // nearby target does not miss merely at a page boundary.
            let start = if bytes < 64 * 1024 {
                index.saturating_sub(rows / 2)
            } else {
                index / rows * rows
            };
            let bytes = (self.spilled - start).min(rows) * N;
            cache.1.clear();
            cache
                .1
                .try_reserve_exact(bytes)
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
            cache.1.resize(bytes, 0);
            let mut at = (start as u64)
                .checked_mul(N as u64)
                .ok_or(io::ErrorKind::InvalidInput)?;
            let result = (|| {
                let mut remaining = cache.1.as_mut_slice();
                while !remaining.is_empty() {
                    let n = match crate::snapshot::read_at(
                        self.file.as_ref().unwrap(),
                        remaining,
                        at,
                    ) {
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        r => r?,
                    };
                    if n == 0 {
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    at += n as u64;
                    remaining = &mut remaining[n..];
                }
                Ok(())
            })();
            if let Err(error) = result {
                cache.1.clear();
                return Err(error);
            }
            cache.0 = start;
            let at = (index - start) * N;
            row.copy_from_slice(&cache.1[at..at + N]);
        }
        Ok(Some(row))
    }

    /// Point probes must not fill a 64 KiB page for every binary-search step.
    /// This path borrows the append buffer and otherwise reads one stack row.
    pub(crate) fn get_direct(&self, index: usize) -> io::Result<Option<[u8; N]>> {
        if index >= self.len() {
            return Ok(None);
        }
        let mut row = [0; N];
        if index >= self.spilled {
            let at = (index - self.spilled) * N;
            row.copy_from_slice(&self.buffer[at..at + N]);
        } else {
            let mut at = (index as u64)
                .checked_mul(N as u64)
                .ok_or(io::ErrorKind::InvalidInput)?;
            let mut remaining = row.as_mut_slice();
            while !remaining.is_empty() {
                let n = match crate::snapshot::read_at(self.file.as_ref().unwrap(), remaining, at) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    r => r?,
                };
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                at += n as u64;
                remaining = &mut remaining[n..];
            }
        }
        Ok(Some(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lending_budget_preserves_rows_and_bounds_both_buffers() -> io::Result<()> {
        for budget in [0, 16, 64, 4096] {
            let mut original = SpillRows::<16>::new(budget);
            let initial = original.capacity;
            for i in 0u64..200 {
                let mut row = [0; 16];
                row[..8].copy_from_slice(&i.to_le_bytes());
                original.push(row)?;
            }
            original.get(0)?;
            let loan = original.lend_boundary_budget()?;
            let mut proof = if loan == 0 {
                SpillRows::<8>::unbuffered()
            } else {
                SpillRows::<8>::new(loan / 2)
            };
            assert_eq!(original.capacity + 2 * proof.capacity, initial);
            for i in 0u64..300 {
                proof.push(i.to_le_bytes())?;
            }
            assert!(original.buffer.capacity() + proof.buffer.capacity() <= initial);
            assert_eq!(proof.read_cache.borrow().1.capacity(), 0);
            for i in 0..200 {
                assert_eq!(
                    u64::from_le_bytes(original.get(i)?.unwrap()[..8].try_into().unwrap()),
                    i as u64
                );
            }
            for i in (0..300).rev() {
                assert_eq!(u64::from_le_bytes(proof.get_direct(i)?.unwrap()), i as u64);
                assert_eq!(
                    u64::from_le_bytes(proof.get_window(i, 48)?.unwrap()),
                    i as u64
                );
            }
            proof.truncate(13);
            proof.push(99u64.to_le_bytes())?;
            assert_eq!(u64::from_le_bytes(proof.get_direct(13)?.unwrap()), 99);
            assert!(proof.get_direct(14)?.is_none());
        }
        Ok(())
    }

    #[test]
    fn cached_floor_agrees_with_sorted_rows_after_spill_and_truncate() {
        let mut rows = SpillRows::<8>::new(32);
        for i in 0..40u64 {
            rows.push((i * 10).to_le_bytes()).unwrap();
        }
        for retained in [40, 23, 4] {
            rows.truncate(retained);
            for probe in 0..retained {
                rows.get(probe).unwrap();
                for wanted in 0..410u64 {
                    if let Some(row) = rows.cached_floor(wanted, |bytes| {
                        u64::from_le_bytes(bytes.try_into().unwrap())
                    }) {
                        let actual = u64::from_le_bytes(row);
                        assert!(actual < retained as u64 * 10);
                        assert_eq!(actual, wanted / 10 * 10);
                    }
                }
            }
        }
    }

    #[test]
    fn cached_floor_handles_irregular_record_extents() {
        let mut rows = SpillRows::<8>::new(1024);
        let mut keys = Vec::new();
        let mut offset = 0u64;
        for i in 0..100 {
            offset += 3 + (i * i % 31);
            keys.push(offset);
            rows.push(offset.to_le_bytes()).unwrap();
        }
        for wanted in 0..=offset + 1 {
            let found = rows.cached_floor(wanted, |bytes| {
                u64::from_le_bytes(bytes.try_into().unwrap())
            });
            if let Some(row) = found {
                assert_eq!(
                    Some(u64::from_le_bytes(row)),
                    keys.iter().copied().filter(|key| *key <= wanted).max()
                );
            } else {
                assert!(wanted < keys[0] || wanted > offset);
            }
        }
    }

    #[test]
    fn rollback_across_a_spill_keeps_the_prefix_and_replaces_the_tail() {
        let mut rows = SpillRows::<8>::new(16);
        for i in 0..10u64 {
            rows.push(i.to_le_bytes()).unwrap();
        }
        assert_eq!(rows.get(2).unwrap(), Some(2u64.to_le_bytes()));
        rows.truncate(3);
        for i in 3..12u64 {
            rows.push((i + 100).to_le_bytes()).unwrap();
        }
        for i in 0..12u64 {
            let expected = if i < 3 { i } else { i + 100 };
            assert_eq!(rows.get(i as usize).unwrap(), Some(expected.to_le_bytes()));
        }
        rows.truncate(0);
        rows.reserve_one().unwrap();
        rows.push_reserved(999u64.to_le_bytes());
        assert_eq!(rows.get(0).unwrap(), Some(999u64.to_le_bytes()));
        assert_eq!(rows.get(1).unwrap(), None);
    }
    #[test]
    fn reads_between_spills_do_not_move_the_append_position() {
        let mut rows = SpillRows::<8>::new(16);
        for i in 0..100u64 {
            rows.push(i.to_le_bytes()).unwrap();
            for j in [0, i / 2, i] {
                assert_eq!(rows.get(j as usize).unwrap(), Some(j.to_le_bytes()));
            }
            assert!(rows.buffer.len() <= 16);
        }
        assert_eq!(rows.get(100).unwrap(), None);
    }
}
