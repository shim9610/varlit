//! Bounded, lossless private rows for rewrite metadata. Pages delta-code words;
//! neither the page directory nor decoded pages grow with the record count.
use crate::{spill_file::SpillFile, spill_rows::SpillRows};
use std::{
    cell::RefCell,
    fs::File,
    io::{self, Seek, SeekFrom, Write},
};

const READ_PAGES: usize = 4;

#[derive(Debug)]
pub(crate) struct PackedRows<const N: usize> {
    pages: SpillRows<8>,
    file: Option<SpillFile>,
    file_end: u64,
    spilled_bytes: u64,
    retained: Vec<u8>,
    retained_limit: usize,
    page_rows: usize,
    buffer: Vec<[u8; N]>,
    encoded: Vec<u8>,
    cache: RefCell<Cache<N>>,
}
#[derive(Debug)]
struct Cache<const N: usize> {
    pages: [Page<N>; READ_PAGES],
    clock: u64,
}
#[derive(Debug)]
struct Page<const N: usize> {
    used: u64,
    start: usize,
    rows: Vec<[u8; N]>,
    encoded: Vec<u8>,
}
fn oom(_: std::collections::TryReserveError) -> io::Error {
    io::ErrorKind::OutOfMemory.into()
}
fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}
#[inline]
fn word(bytes: &[u8], at: usize) -> u64 {
    let mut raw = [0; 8];
    let part = &bytes[at..bytes.len().min(at + 8)];
    raw[..part.len()].copy_from_slice(part);
    u64::from_le_bytes(raw)
}
#[inline]
fn put_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 128 {
        out.push((value as u8) | 128);
        value >>= 7;
    }
    out.push(value as u8);
}
#[inline]
fn take_varint(input: &mut &[u8]) -> io::Result<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let (&byte, tail) = input.split_first().ok_or_else(invalid)?;
        *input = tail;
        if shift == 63 && byte > 1 {
            return Err(invalid());
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    Err(invalid())
}
impl<const N: usize> PackedRows<N> {
    pub(crate) fn new(bytes: usize) -> Self {
        Self::with_page_rows(bytes, 256)
    }
    pub(crate) fn with_page_rows(bytes: usize, maximum_rows: usize) -> Self {
        assert!(N > 0 && N <= 128);
        assert!((1..=256).contains(&maximum_rows));
        // Tail, encoded write scratch, and decoded/encoded read scratch share
        // the supplied budget. A minimum single-row working set is unavoidable.
        let encoded_row_bound = N.div_ceil(8) * 10 + N.div_ceil(64);
        let working_row_bytes = (READ_PAGES + 1) * (N + encoded_row_bound);
        let page_rows = (bytes / working_row_bytes).clamp(1, maximum_rows);
        let remaining = bytes.saturating_sub(page_rows * working_row_bytes);
        let directory_bytes = (remaining / 32).clamp(8, 16 * 1024);
        Self {
            // Split the remaining allowance between the page directory's
            // append buffer and read cache; both are independently bounded.
            pages: SpillRows::new(directory_bytes),
            file: None,
            file_end: 0,
            spilled_bytes: 0,
            retained: Vec::new(),
            retained_limit: remaining.saturating_sub(directory_bytes * 2),
            page_rows,
            buffer: Vec::new(),
            encoded: Vec::new(),
            cache: RefCell::new(Cache {
                pages: std::array::from_fn(|_| Page {
                    used: 0,
                    start: 0,
                    rows: Vec::new(),
                    encoded: Vec::new(),
                }),
                clock: 0,
            }),
        }
    }
    pub(crate) const fn minimum_buffer_bytes() -> usize {
        (READ_PAGES + 1) * (N + N.div_ceil(8) * 10 + N.div_ceil(64)) + 16
    }

    fn read_heap_bytes(&self) -> usize {
        let mut bytes = self
            .retained
            .capacity()
            .saturating_add(self.buffer.capacity().saturating_mul(N))
            .saturating_add(self.encoded.capacity())
            .saturating_add(self.pages.read_heap_bytes());
        for page in &self.cache.borrow().pages {
            bytes = bytes
                .saturating_add(page.rows.capacity().max(self.page_rows).saturating_mul(N))
                .saturating_add(page.encoded.capacity().max(self.encoded_bound()));
        }
        bytes
    }

    /// Reuse a released rewrite allowance for immutable compact rows. Peak
    /// allocation includes both copies and every future decoded-page cache.
    /// If it cannot fit, keep the existing spill representation unchanged.
    pub(crate) fn try_retain_for_reads(&mut self, budget: usize) -> io::Result<()> {
        let data_bytes = usize::try_from(self.file_end).map_err(|_| invalid())?;
        if self.spilled_bytes != 0 && data_bytes <= budget.saturating_sub(self.read_heap_bytes()) {
            let mut retained = Vec::new();
            retained.try_reserve_exact(data_bytes).map_err(oom)?;
            retained.resize(data_bytes, 0);
            self.read_encoded(&mut retained, 0)?;
            self.retained = retained;
            self.spilled_bytes = 0;
            self.file = None;
        }
        let other = self
            .read_heap_bytes()
            .saturating_sub(self.pages.read_heap_bytes());
        self.pages
            .try_retain_for_reads(budget.saturating_sub(other))?;
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.pages.len() * self.page_rows + self.buffer.len()
    }
    fn encoded_bound(&self) -> usize {
        self.page_rows * (N.div_ceil(8) * 10 + N.div_ceil(64))
    }
    fn flush(&mut self) -> io::Result<()> {
        debug_assert_eq!(self.buffer.len(), self.page_rows);
        self.pages.reserve_one()?;
        self.encoded.clear();
        self.encoded
            .try_reserve_exact(self.encoded_bound())
            .map_err(oom)?;
        let mut previous = [0; N];
        for row in &self.buffer {
            let mask_at = self.encoded.len();
            let mask_bytes = N.div_ceil(64);
            self.encoded.resize(mask_at + mask_bytes, 0);
            let mut mask = 0u16;
            let mut first_delta = None;
            for (i, at) in (0..N).step_by(8).enumerate() {
                let delta = word(row, at).wrapping_sub(word(&previous, at));
                if delta != 0 {
                    mask |= 1 << i;
                    // Offset remaps commonly move all three positions by the
                    // same distance. Spare mask bits encode that distance once.
                    if N == 24 && first_delta == Some(delta) {
                        mask |= 1 << (i + 3);
                    } else {
                        let signed = (delta << 1) ^ ((delta as i64 >> 63) as u64);
                        put_varint(signed, &mut self.encoded);
                    }
                    if first_delta.is_none() {
                        first_delta = Some(delta);
                    }
                }
            }
            self.encoded[mask_at..mask_at + mask_bytes]
                .copy_from_slice(&mask.to_le_bytes()[..mask_bytes]);
            previous = *row;
        }
        let end = self
            .file_end
            .checked_add(4 + self.encoded.len() as u64)
            .ok_or_else(invalid)?;
        let bytes = 4 + self.encoded.len();
        if self.retained.len().saturating_add(bytes) > self.retained_limit
            && !self.retained.is_empty()
        {
            if self.file.is_none() {
                self.file = Some(SpillFile::new()?);
            }
            let file = self.file.as_mut().unwrap();
            // A partial spill leaves the authoritative tail intact for retry.
            file.seek(SeekFrom::Start(self.spilled_bytes))?;
            file.write_all(&self.retained)?;
            self.spilled_bytes += self.retained.len() as u64;
            self.retained.clear();
        }
        if bytes <= self.retained_limit {
            let needed = self.retained.len() + bytes;
            if needed > self.retained.capacity() {
                let capacity = needed
                    .max(self.retained.capacity().saturating_mul(2))
                    .min(self.retained_limit);
                self.retained
                    .try_reserve_exact(capacity - self.retained.len())
                    .map_err(oom)?;
            }
            self.retained
                .extend_from_slice(&(self.encoded.len() as u32).to_le_bytes());
            self.retained.extend_from_slice(&self.encoded);
        } else {
            if self.file.is_none() {
                self.file = Some(SpillFile::new()?);
            }
            let file = self.file.as_mut().unwrap();
            file.seek(SeekFrom::Start(self.file_end))?;
            file.write_all(&(self.encoded.len() as u32).to_le_bytes())?;
            file.write_all(&self.encoded)?;
            self.spilled_bytes = end;
        }
        self.pages.push_reserved(self.file_end.to_le_bytes());
        self.file_end = end;
        self.buffer.clear();
        Ok(())
    }
    pub(crate) fn push(&mut self, row: [u8; N]) -> io::Result<()> {
        if self.buffer.len() == self.page_rows {
            self.flush()?;
        }
        if self.buffer.capacity() < self.page_rows {
            self.buffer
                .try_reserve_exact(self.page_rows - self.buffer.len())
                .map_err(oom)?;
        }
        self.buffer.push(row);
        Ok(())
    }
    pub(crate) fn get(&self, position: usize) -> io::Result<Option<[u8; N]>> {
        let spilled = self.pages.len() * self.page_rows;
        if position >= spilled {
            return Ok(self.buffer.get(position - spilled).copied());
        }
        let start = position / self.page_rows * self.page_rows;
        let mut caches = self.cache.borrow_mut();
        caches.clock = caches.clock.wrapping_add(1);
        let clock = caches.clock;
        if let Some(at) = caches
            .pages
            .iter()
            .position(|page| page.start == start && page.rows.len() == self.page_rows)
        {
            caches.pages[at].used = clock;
            return Ok(Some(caches.pages[at].rows[position - start]));
        }
        let replace = caches
            .pages
            .iter()
            .position(|page| page.rows.is_empty())
            .unwrap_or_else(|| {
                caches
                    .pages
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, page)| clock.wrapping_sub(page.used))
                    .unwrap()
                    .0
            });
        caches.pages[replace].used = clock;
        let cache = &mut caches.pages[replace];
        {
            cache.rows.clear();
            // Small pages serve point lookups; do not fill a directory cache
            // with thousands of unrelated offsets for each random probe.
            let page = position / self.page_rows;
            let at = u64::from_le_bytes(
                if self.page_rows <= 16 {
                    self.pages.get_direct(page)?
                } else {
                    self.pages.get(page)?
                }
                .ok_or_else(invalid)?,
            );
            let data_at = at
                .checked_add(4)
                .filter(|end| *end <= self.file_end)
                .ok_or_else(invalid)?;
            let mut header = [0; 4];
            self.read_encoded(&mut header, at)?;
            let bytes = u32::from_le_bytes(header) as usize;
            if bytes > self.encoded_bound()
                || data_at
                    .checked_add(bytes as u64)
                    .is_none_or(|end| end > self.file_end)
            {
                return Err(invalid());
            }
            cache.encoded.clear();
            cache.encoded.try_reserve_exact(bytes).map_err(oom)?;
            cache.encoded.resize(bytes, 0);
            self.read_encoded(&mut cache.encoded, data_at)?;
            cache.rows.try_reserve_exact(self.page_rows).map_err(oom)?;
            let Page { rows, encoded, .. } = cache;
            let mut input = encoded.as_slice();
            let mut previous = [0; N];
            let decoded = (|| {
                for _ in 0..self.page_rows {
                    let mask_bytes = N.div_ceil(64);
                    let mask_raw = input.get(..mask_bytes).ok_or_else(invalid)?;
                    let mut mask = [0; 2];
                    mask[..mask_bytes].copy_from_slice(mask_raw);
                    input = &input[mask_bytes..];
                    let mask = u16::from_le_bytes(mask);
                    let mask_bits = if N == 24 { 6 } else { N.div_ceil(8) };
                    if mask_bits < 16 && mask >> mask_bits != 0 {
                        return Err(invalid());
                    }
                    if N == 24 && ((mask >> 3) & !mask & 7) != 0 {
                        return Err(invalid());
                    }
                    let mut first_delta = None;
                    for (i, at) in (0..N).step_by(8).enumerate() {
                        if mask & (1 << i) != 0 {
                            let delta = if N == 24 && mask & (1 << (i + 3)) != 0 {
                                first_delta.ok_or_else(invalid)?
                            } else {
                                let encoded = take_varint(&mut input)?;
                                (encoded >> 1) ^ 0u64.wrapping_sub(encoded & 1)
                            };
                            if first_delta.is_none() {
                                first_delta = Some(delta);
                            }
                            let value = word(&previous, at).wrapping_add(delta).to_le_bytes();
                            let end = N.min(at + 8);
                            previous[at..end].copy_from_slice(&value[..end - at]);
                        }
                    }
                    rows.push(previous);
                }
                if !input.is_empty() {
                    return Err(invalid());
                }
                Ok(())
            })();
            if let Err(error) = decoded {
                cache.rows.clear();
                return Err(error);
            }
            cache.start = start;
        }
        Ok(Some(cache.rows[position - start]))
    }
    fn read_encoded(&self, mut bytes: &mut [u8], mut at: u64) -> io::Result<()> {
        if at < self.spilled_bytes {
            let count = bytes
                .len()
                .min((self.spilled_bytes - at).try_into().unwrap_or(usize::MAX));
            read_exact_at(
                self.file.as_ref().ok_or_else(invalid)?,
                &mut bytes[..count],
                at,
            )?;
            at += count as u64;
            bytes = &mut bytes[count..];
        }
        if !bytes.is_empty() {
            let start = usize::try_from(at - self.spilled_bytes).map_err(|_| invalid())?;
            let end = start.checked_add(bytes.len()).ok_or_else(invalid)?;
            bytes.copy_from_slice(self.retained.get(start..end).ok_or_else(invalid)?);
        }
        Ok(())
    }
    pub(crate) fn find_sorted(
        &self,
        wanted: u64,
        key: impl Fn(&[u8]) -> u64,
    ) -> io::Result<Option<[u8; N]>> {
        fn search<const N: usize>(
            rows: &[[u8; N]],
            wanted: u64,
            key: &impl Fn(&[u8]) -> u64,
        ) -> Option<[u8; N]> {
            rows.binary_search_by_key(&wanted, |row| key(row))
                .ok()
                .map(|at| rows[at])
        }
        if self.buffer.first().is_some_and(|row| wanted >= key(row)) {
            return Ok(search(&self.buffer, wanted, &key));
        }
        let (mut left, mut right) = (0, self.pages.len() * self.page_rows);
        let mut adjacent = true;
        while left < right {
            let probe = {
                let mut cache = self.cache.borrow_mut();
                let mut bounded = false;
                let mut before = false;
                for page_at in 0..READ_PAGES {
                    let page = &cache.pages[page_at];
                    let rows = &page.rows;
                    if rows.is_empty() || page.start < left || page.start + rows.len() > right {
                        continue;
                    }
                    if wanted < key(&rows[0]) {
                        right = page.start;
                        before = true;
                    } else if wanted > key(rows.last().unwrap()) {
                        left = page.start + rows.len();
                        before = false;
                    } else {
                        let result = search(rows, wanted, &key);
                        cache.clock = cache.clock.wrapping_add(1);
                        cache.pages[page_at].used = cache.clock;
                        return Ok(result);
                    }
                    bounded = true;
                }
                if left == right {
                    return Ok(None);
                }
                if adjacent && bounded {
                    adjacent = false;
                    if before { right - 1 } else { left }
                } else {
                    left + (right - left) / 2
                }
            };
            self.get(probe)?;
        }
        Ok(None)
    }
}
fn read_exact_at(file: &File, mut bytes: &mut [u8], mut at: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        let n = match crate::snapshot::read_at(file, bytes, at) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        at += n as u64;
        bytes = &mut bytes[n..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packed_read_promotion_keeps_bytes_and_failure_state_within_budget() -> io::Result<()> {
        let mut rows = PackedRows::<62>::with_page_rows(4096, 8);
        let mut expected = Vec::new();
        let mut state = 17u64;
        for i in 0..128u64 {
            let mut row = [0; 62];
            for byte in &mut row {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            row[..8].copy_from_slice(&(i * 19).to_le_bytes());
            rows.push(row)?;
            expected.push(row);
        }
        let spilled = rows.spilled_bytes;
        assert!(spilled > 0);
        let retained = rows.retained.clone();
        rows.try_retain_for_reads(0)?;
        assert_eq!(rows.spilled_bytes, spilled);
        let mut image = vec![0; rows.file_end as usize];
        rows.read_encoded(&mut image, 0)?;
        rows.file.as_ref().unwrap().set_len(spilled - 1)?;
        assert_eq!(
            rows.try_retain_for_reads(64 * 1024).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(rows.spilled_bytes, spilled);
        assert_eq!(rows.retained, retained);
        let file = rows.file.as_mut().unwrap();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&image[..spilled as usize])?;
        rows.try_retain_for_reads(64 * 1024)?;
        assert!(rows.file.is_none());
        assert_eq!(rows.spilled_bytes, 0);
        assert!(rows.read_heap_bytes() <= 64 * 1024);
        for i in (0..128).rev() {
            assert_eq!(rows.get(i)?, Some(expected[i]));
            assert_eq!(
                rows.find_sorted(i as u64 * 19, |r| word(r, 0))?,
                Some(expected[i])
            );
            assert_eq!(rows.find_sorted(i as u64 * 19 + 1, |r| word(r, 0))?, None);
        }
        Ok(())
    }

    #[test]
    fn retained_and_spilled_remaps_agree_when_chain_deltas_repeat() -> io::Result<()> {
        for budget in [0, 4096, 1024 * 1024] {
            let mut rows = PackedRows::<24>::new(budget);
            let mut expected = Vec::new();
            for i in 0..400u64 {
                let values = match i % 5 {
                    0 => [i * 136; 3],
                    1 => [i * 136, 0, 0],
                    2 => [0, i * 100, i * 100],
                    3 => [i * 136, i * 80, i * 60],
                    _ => [u64::MAX, 1, 1],
                };
                let mut row = [0; 24];
                for (target, value) in row.as_chunks_mut::<8>().0.iter_mut().zip(values) {
                    target.copy_from_slice(&value.to_le_bytes());
                }
                rows.push(row)?;
                expected.push(row);
            }
            for i in (0..expected.len()).rev() {
                assert_eq!(rows.get(i)?, Some(expected[i]));
            }
            if budget == 1024 * 1024 {
                assert!(rows.file.is_none(), "fitting metadata was written to disk");
            } else {
                assert!(rows.file.is_some(), "spill boundary was not exercised");
            }
        }
        Ok(())
    }
    #[test]
    fn packed_words_roundtrip_wraparound_and_partial_words_across_pages() -> io::Result<()> {
        for budget in [1, 700, 16384] {
            let mut rows = PackedRows::<77>::new(budget);
            let mut expected = Vec::new();
            let mut state = 7u64;
            let count = if cfg!(miri) { 48 } else { 1200 };
            for i in 0..count as u64 {
                let mut row = [0; 77];
                for byte in &mut row {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *byte = state as u8;
                }
                row[..8].copy_from_slice(&(i * 100).to_le_bytes());
                rows.push(row)?;
                expected.push(row);
            }
            for i in (0..count).rev().chain((0..count).map(|i| i * 73 % count)) {
                assert_eq!(rows.get(i)?, Some(expected[i]));
                let key = |row: &[u8]| u64::from_le_bytes(row[..8].try_into().unwrap());
                assert_eq!(rows.find_sorted(i as u64 * 100, key)?, Some(expected[i]));
                assert_eq!(rows.find_sorted(i as u64 * 100 + 1, key)?, None);
            }
            assert_eq!(rows.get(count)?, None);
        }
        Ok(())
    }
    #[test]
    fn corrupt_page_is_refused_and_never_cached_as_complete() -> io::Result<()> {
        let mut rows = PackedRows::<24>::new(512);
        for i in 0..100u64 {
            let mut row = [0; 24];
            row[..8].copy_from_slice(&i.to_le_bytes());
            rows.push(row)?;
        }
        let file = rows.file.as_mut().unwrap();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&u32::MAX.to_le_bytes())?;
        assert!(rows.get(0).is_err());
        assert!(rows.get(0).is_err());
        Ok(())
    }
}
