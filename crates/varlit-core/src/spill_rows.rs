//! Reader-local fixed-width temporary directories. The memory budget controls
//! buffering, never the number of rows that can be addressed.
use crate::spill_file::SpillFile;
use std::cell::RefCell;
use std::io::{self, Seek, SeekFrom, Write};

#[derive(Debug)]
pub(crate) struct SpillRows<const N: usize> {
    file: Option<SpillFile>,
    spilled: usize,
    buffer: Vec<u8>,
    capacity: usize,
    read_cache: RefCell<(usize, Vec<u8>)>,
}
// Most byte offsets and row counts fit a 64-bit product. Keep the exact
// 128-bit computation for the full-width cases rather than paying for it on
// every ordinary lookup.
#[inline]
pub(crate) fn scaled_position(delta: u64, intervals: usize, span: u64) -> u128 {
    debug_assert_ne!(span, 0);
    if let Ok(intervals) = u64::try_from(intervals)
        && let Some(product) = delta.checked_mul(intervals)
    {
        u128::from(product / span)
    } else {
        u128::from(delta) * intervals as u128 / u128::from(span)
    }
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
        // Round the loan down to complete source rows. Rounding the retained
        // share down could lend *more* than 16 KiB for unaligned row widths.
        let available = (self.capacity / 2).min(16 * 1024) / N * N;
        // The boundary directory needs one u64 write row and one read row.
        if available < 2 * std::mem::size_of::<u64>() {
            return Ok(0);
        }
        let retained = self.capacity - available;
        if self.buffer.capacity() > retained && !self.buffer.is_empty() {
            if self.file.is_none() {
                self.file = Some(SpillFile::new()?);
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
    // Account for the cache that a later read may allocate, not just today's
    // capacities. This is shared with the parent packed-directory budget.
    pub(crate) fn read_heap_bytes(&self) -> usize {
        self.buffer
            .capacity()
            .saturating_add(
                self.read_cache
                    .borrow()
                    .1
                    .capacity()
                    .max(if self.spilled == 0 {
                        0
                    } else {
                        self.capacity.min(64 * 1024)
                    }),
            )
    }

    /// Optional read-only promotion. The old and new buffers both fit the
    /// supplied allowance; refusal or short I/O leaves every old row intact.
    pub(crate) fn try_retain_for_reads(&mut self, budget: usize) -> io::Result<bool> {
        if self.spilled == 0 {
            return Ok(true);
        }
        let bytes = self
            .len()
            .checked_mul(N)
            .ok_or(io::ErrorKind::InvalidData)?;
        if bytes > budget.saturating_sub(self.read_heap_bytes()) {
            return Ok(false);
        }
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(bytes)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        buffer.resize(bytes, 0);
        let prefix = self.spilled * N;
        let mut consumed = 0;
        while consumed < prefix {
            let n = match crate::snapshot::read_at(
                self.file.as_ref().unwrap(),
                &mut buffer[consumed..prefix],
                consumed as u64,
            ) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                other => other?,
            };
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            consumed += n;
        }
        buffer[prefix..].copy_from_slice(&self.buffer);
        self.buffer = buffer;
        self.capacity = self.capacity.max(bytes);
        self.spilled = 0;
        self.file = None;
        self.read_cache.get_mut().1 = Vec::new();
        Ok(true)
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
            if count == 0 {
                return None;
            }
            let first = key(&bytes[..N]);
            if wanted < first {
                return None;
            }
            let last = key(&bytes[(count - 1) * N..]);
            if wanted > last {
                return None;
            }
            // Fixed-size records are common, with occasional large internal
            // records between runs. Guess from the first adjacent spacing,
            // not the entire page span (whose last row can follow a large
            // checkpoint). Only exact equality proves membership; arbitrary
            // extents retain the binary search below.
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
            // The standard slice search uses a fixed iteration schedule and
            // branchless bound selection for unpredictable irregular offsets.
            // It still compares real private rows, never an estimated boundary.
            let (rows, remainder) = bytes.as_chunks::<N>();
            debug_assert!(remainder.is_empty());
            let (mut low, mut high) = (0, count);
            // Large resident directories otherwise incur a full binary search
            // per mark. Two bounded estimates can locate a small real-key
            // bracket, but never authorize a record boundary by themselves.
            if count >= 1024 && last > first {
                let span = last - first;
                let intervals = count - 1;
                let estimate = scaled_position(wanted - first, intervals, span) as usize;
                let observed = key(&rows[estimate]);
                if observed == wanted {
                    return Some(rows[estimate]);
                }
                // Near-uniform extents often put the estimate beside the
                // answer. An actual adjacent-key bracket proves the floor.
                if observed < wanted && estimate + 1 < count {
                    let next = key(&rows[estimate + 1]);
                    if wanted <= next {
                        return Some(rows[estimate + usize::from(wanted == next)]);
                    }
                } else if observed > wanted && estimate > 0 && key(&rows[estimate - 1]) <= wanted {
                    return Some(rows[estimate - 1]);
                }
                let correction = scaled_position(wanted.abs_diff(observed), intervals, span)
                    .min(intervals as u128) as usize;
                let mut corrected = if wanted > observed {
                    estimate.saturating_add(correction).min(count - 1)
                } else {
                    estimate.saturating_sub(correction)
                };
                // A large irregular directory can leave the first correction
                // outside this small window. Refine only while observed error
                // falls by at least half; every answer still comes from a row.
                // The fixed iteration bound also covers hostile geometry.
                let mut previous_distance = wanted.abs_diff(observed);
                let mut last_probe = estimate;
                for _ in 0..3 {
                    if corrected == last_probe {
                        break;
                    }
                    let at = key(&rows[corrected]);
                    last_probe = corrected;
                    if at == wanted {
                        return Some(rows[corrected]);
                    }
                    if at < wanted && corrected + 1 < count {
                        let next = key(&rows[corrected + 1]);
                        if wanted <= next {
                            return Some(rows[corrected + usize::from(wanted == next)]);
                        }
                    } else if at > wanted && corrected > 0 && key(&rows[corrected - 1]) <= wanted {
                        return Some(rows[corrected - 1]);
                    }
                    let distance = wanted.abs_diff(at);
                    if distance >= previous_distance / 2 {
                        break;
                    }
                    previous_distance = distance;
                    let step = (scaled_position(distance, intervals, span).min(intervals as u128)
                        as usize)
                        .max(1);
                    corrected = if wanted > at {
                        corrected.saturating_add(step).min(count - 1)
                    } else {
                        corrected.saturating_sub(step)
                    };
                }
                if corrected != last_probe && key(&rows[corrected]) == wanted {
                    return Some(rows[corrected]);
                }
                let start = corrected.saturating_sub(16);
                let end = corrected.saturating_add(17).min(count);
                if key(&rows[start]) <= wanted && wanted <= key(&rows[end - 1]) {
                    (low, high) = (start, end);
                } else if observed < wanted {
                    low = estimate + 1;
                } else {
                    high = estimate;
                }
            }
            let end = low + rows[low..high].partition_point(|row| key(row) <= wanted);
            end.checked_sub(1).and_then(|at| rows.get(at)).copied()
        }
        search(&self.buffer, wanted, &key)
            .or_else(|| search(&self.read_cache.borrow().1, wanted, &key))
    }
    /// All fallible work precedes the authoritative append. The caller may
    /// subsequently install exactly one row without I/O or allocation.
    pub(crate) fn reserve_one(&mut self) -> io::Result<()> {
        if self.buffer.len() >= self.capacity {
            if self.file.is_none() {
                self.file = Some(SpillFile::new()?);
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
                self.file = Some(SpillFile::new()?);
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
    fn cached_floor_matches_ordered_rows_across_irregular_and_full_width_gaps() -> io::Result<()> {
        for count in [1usize, 2, 63, 64, 65, 256, 1024, 1025] {
            for shape in 0..3 {
                let keys: Vec<u64> = (0..count)
                    .scan(0u64, |previous, i| {
                        let key = match shape {
                            0 => *previous + 64 + (i as u64 * 8191 % 65536),
                            1 => i as u64 + if i >= count / 2 { 1u64 << 63 } else { 0 },
                            _ => {
                                (i as u128 * u128::from(u64::MAX)
                                    / count.saturating_sub(1).max(1) as u128)
                                    as u64
                            }
                        };
                        *previous = key;
                        Some(key)
                    })
                    .collect();
                let mut rows = SpillRows::<8>::new(count * 8);
                for key in &keys {
                    rows.push(key.to_le_bytes())?;
                }
                // Keep all small cases and sample both ends plus about 64
                // positions in large cases, including the prediction threshold.
                let probes = keys
                    .iter()
                    .step_by(if count >= 1024 { 16 } else { 1 })
                    .chain(keys.last());
                for wanted in probes.flat_map(|&key| {
                    [Some(key), key.checked_sub(1), key.checked_add(1)]
                        .into_iter()
                        .flatten()
                }) {
                    let expected = if wanted < keys[0] || wanted > keys[count - 1] {
                        None
                    } else {
                        keys[..keys.partition_point(|&key| key <= wanted)]
                            .last()
                            .copied()
                    };
                    let found = rows
                        .cached_floor(wanted, |row| u64::from_le_bytes(row.try_into().unwrap()))
                        .map(u64::from_le_bytes);
                    assert_eq!(
                        found, expected,
                        "count={count} shape={shape} wanted={wanted}"
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn read_promotion_is_bounded_and_preserves_retry_after_short_io() -> io::Result<()> {
        let mut rows = SpillRows::<8>::new(32);
        for n in 0..80u64 {
            rows.push((n * 17).to_le_bytes())?;
        }
        rows.get(0)?;
        let spilled = rows.spilled;
        let budget = rows.read_heap_bytes() + rows.len() * 8;
        assert!(!rows.try_retain_for_reads(budget - 1)?);
        assert_eq!(rows.spilled, spilled);
        let original: Vec<u8> = (0..spilled as u64)
            .flat_map(|n| (n * 17).to_le_bytes())
            .collect();
        rows.file
            .as_ref()
            .unwrap()
            .set_len((original.len() - 1) as u64)?;
        assert_eq!(
            rows.try_retain_for_reads(budget).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(rows.spilled, spilled);
        assert_eq!(rows.len(), 80);
        let file = rows.file.as_mut().unwrap();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&original)?;
        assert!(rows.try_retain_for_reads(budget)?);
        assert!(rows.file.is_none());
        assert!(rows.read_heap_bytes() <= budget);
        for n in (0..80).rev() {
            assert_eq!(rows.get(n)?, Some((n as u64 * 17).to_le_bytes()));
        }
        rows.truncate(12);
        rows.push(999u64.to_le_bytes())?;
        assert_eq!(rows.get(12)?, Some(999u64.to_le_bytes()));
        assert_eq!(rows.get(13)?, None);
        Ok(())
    }

    #[test]
    fn unaligned_rows_never_round_up_the_lent_boundary_allowance() -> io::Result<()> {
        assert_eq!(SpillRows::<13>::unbuffered().lend_boundary_budget()?, 0);
        for budget in [
            0,
            13,
            26,
            39,
            52,
            208,
            4096,
            65536,
            usize::MAX - 1,
            usize::MAX,
        ] {
            let mut original = SpillRows::<13>::new(budget);
            let initial = original.capacity;
            for i in 0..200u64 {
                let mut row = [0; 13];
                row[..8].copy_from_slice(&i.to_le_bytes());
                original.push(row)?;
            }
            original.get(0)?;
            let loan = original.lend_boundary_budget()?;
            assert!(loan <= 16 * 1024 && loan <= initial / 2);
            assert!(loan == 0 || loan >= 16);
            let mut proof = if loan == 0 {
                SpillRows::<8>::unbuffered()
            } else {
                SpillRows::<8>::new(loan / 2)
            };
            assert!(original.capacity + 2 * proof.capacity <= initial);
            for i in 0..200u64 {
                proof.push((i * 17).to_le_bytes())?;
            }
            for i in (0..200).rev() {
                assert_eq!(
                    u64::from_le_bytes(original.get(i)?.unwrap()[..8].try_into().unwrap()),
                    i as u64
                );
                assert_eq!(proof.get(i)?, Some((i as u64 * 17).to_le_bytes()));
            }
            assert!(
                original.buffer.capacity()
                    + proof.buffer.capacity()
                    + proof.read_cache.borrow().1.capacity()
                    <= initial
            );
        }
        Ok(())
    }

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
    #[test]
    fn cached_floor_bounds_probes_on_periodically_varying_extents() -> io::Result<()> {
        use std::cell::Cell;
        let period = if cfg!(miri) { 4096 } else { 65536 };
        let count = period * 2;
        let stride = period / 8 - 1;
        let mut rows = SpillRows::<8>::new(2 * 1024 * 1024);
        let mut offsets = Vec::new();
        let mut offset = 100u64;
        for n in 0..count {
            rows.push(offset.to_le_bytes())?;
            offsets.push(offset);
            offset += 72 + (n * stride % period) as u64;
        }
        for extent in [128, 96] {
            rows.push(offset.to_le_bytes())?;
            offsets.push(offset);
            offset += extent;
        }
        let selector_calls = Cell::new(0usize);
        let binary_calls = Cell::new(0usize);
        let queries = count / 16;
        for sample in 0..queries {
            let at = 16 * (sample * 31337 % queries + 1) - 1;
            let wanted = offsets[at];
            for probe in [wanted - 1, wanted, wanted + 1] {
                let end = offsets.partition_point(|&value| {
                    if probe == wanted {
                        binary_calls.set(binary_calls.get() + 1);
                    }
                    value <= probe
                });
                let expected = end.checked_sub(1).map(|at| offsets[at]);
                let actual = rows
                    .cached_floor(probe, |bytes| {
                        if probe == wanted {
                            selector_calls.set(selector_calls.get() + 1);
                        }
                        u64::from_le_bytes(bytes.try_into().unwrap())
                    })
                    .map(u64::from_le_bytes);
                assert_eq!(actual, expected);
            }
        }
        eprintln!(
            "fixture selector calls={} binary calls={}",
            selector_calls.get(),
            binary_calls.get()
        );
        // A resident mark should not do more offset probes than the standard
        // binary-search baseline on this irregular, recurring record layout.
        assert!(selector_calls.get() <= binary_calls.get());
        Ok(())
    }
    #[test]
    fn scaled_positions_match_full_width_arithmetic() {
        for delta in [0, 1, 2, u64::MAX / 2, u64::MAX - 1, u64::MAX] {
            for count in [0, 1, 2, 1023, usize::MAX / 2, usize::MAX] {
                for span in [1, 2, 1024, u64::MAX / 2, u64::MAX] {
                    assert_eq!(
                        scaled_position(delta, count, span),
                        u128::from(delta) * count as u128 / u128::from(span)
                    );
                }
            }
        }
    }
}
