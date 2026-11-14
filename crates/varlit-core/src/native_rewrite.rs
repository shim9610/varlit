//! Bounded private rewrite directories. No scratch state is published.
use super::*;
use crate::packed_rows::PackedRows;
use crate::spill_rows::SpillRows;

// Native payload/footer positions are determined by record position and length.
// Retain their presence and validate that geometry before omitting duplicates.
pub(super) type RewriteRows = PackedRows<62>;

#[inline]
fn encode_rewrite_entry(entry: &RecordIndexEntry) -> Result<[u8; 62]> {
    if entry.record_offset.checked_add(RECORD_HEADER_LEN) != Some(entry.payload_offset)
        || entry
            .footer_offset
            .is_some_and(|at| entry.payload_offset.checked_add(entry.payload_len) != Some(at))
    {
        return Err(Error::InvalidRecordFooter {
            offset: entry.record_offset,
        });
    }
    let full = BufferedScan::encode(entry);
    let mut row = [0; 62];
    row[..24].copy_from_slice(&full[..24]);
    row[24..40].copy_from_slice(&full[32..48]);
    row[40..61].copy_from_slice(&full[56..77]);
    row[61] = u8::from(entry.footer_offset.is_some());
    Ok(row)
}
#[inline]
fn decode_rewrite_entry(row: &[u8; 62]) -> Result<RecordIndexEntry> {
    let record_offset = u64::from_le_bytes(row[16..24].try_into().unwrap());
    let payload_offset = record_offset
        .checked_add(RECORD_HEADER_LEN)
        .ok_or(Error::UnexpectedEof)?;
    let payload_len = u64::from_le_bytes(row[24..32].try_into().unwrap());
    let footer_offset = match row[61] {
        0 => None,
        1 => Some(
            payload_offset
                .checked_add(payload_len)
                .ok_or(Error::UnexpectedEof)?,
        ),
        _ => {
            return Err(Error::InvalidRecordFooter {
                offset: record_offset,
            });
        }
    };
    Ok(RecordIndexEntry {
        block_id: u32::from_le_bytes(row[..4].try_into().unwrap()),
        block_version: u16::from_le_bytes(row[4..6].try_into().unwrap()),
        flags: u16::from_le_bytes(row[6..8].try_into().unwrap()),
        sequence: u64::from_le_bytes(row[8..16].try_into().unwrap()),
        record_offset,
        payload_offset,
        payload_len,
        footer_offset,
        checksum: u32::from_le_bytes(row[32..36].try_into().unwrap()),
        uncompressed_len_hint: u32::from_le_bytes(row[36..40].try_into().unwrap()),
        prev_same_block_offset: nonzero_offset(u64::from_le_bytes(row[40..48].try_into().unwrap())),
        prev_same_key_offset: nonzero_offset(u64::from_le_bytes(row[48..56].try_into().unwrap())),
        committed: row[56] != 0,
        mutable_flags: u32::from_le_bytes(row[57..61].try_into().unwrap()),
    })
}

/// Disjoint increasing runs prove uniqueness without retaining sequences.
/// Replacing one record with a fresh high sequence at an end commonly creates two
/// such runs. Overlapping ranges require the external-sort fallback;
/// an overlap alone does not prove that any sequence was reused.
#[derive(Default)]
pub(super) struct SequenceRanges {
    current: Option<(u64, u64)>,
    finished: Option<(u64, u64)>,
    overlap: bool,
}
impl SequenceRanges {
    pub(super) fn note(&mut self, sequence: u64) {
        let current = match self.current {
            Some((low, high)) if sequence > high => (low, sequence),
            Some((low, high)) => {
                self.finished = Some(match self.finished {
                    Some((a, b)) => (a.min(low), b.max(high)),
                    None => (low, high),
                });
                (sequence, sequence)
            }
            None => (sequence, sequence),
        };
        if let Some((low, high)) = self.finished {
            self.overlap |= current.0 <= high && current.1 >= low;
        }
        self.current = Some(current);
    }
    pub(super) fn proves_unique(&self) -> bool {
        !self.overlap
    }
}

pub(super) struct RewriteDirectory {
    rows: RewriteRows,
    uniform: Option<resident_index::UniformPrefix>,
    last: Option<RecordIndexEntry>,
    maximum_sequence: u64,
    tails: [(u32, u64); 16],
    tail_count: usize,
}
impl RewriteDirectory {
    pub(super) fn new(spec: FormatSpec) -> Self {
        Self {
            // Construction benefits from larger delta-coded pages. Published
            // readers receive a separate compact directory with small pages;
            // keeping construction pages small only repeats headers/offsets.
            // Both representations remain within the existing shared budget.
            rows: PackedRows::new(resident_index::rewrite_buffer_bytes(spec)),
            uniform: None,
            last: None,
            maximum_sequence: 0,
            tails: [(0, 0); 16],
            tail_count: 0,
        }
    }
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }
    pub(super) fn into_resident(self, snapshot: SnapshotFile, spec: FormatSpec) -> ResidentIndex {
        ResidentIndex::from_rewrite_rows(self.rows, self.uniform, snapshot, spec)
    }
    pub(super) fn find_offset(&self, offset: u64) -> Result<Option<RecordIndexEntry>> {
        if let Some(last) = self
            .last
            .as_ref()
            .filter(|entry| entry.record_offset == offset)
        {
            note_replacement_predecessor_probe();
            return Ok(Some(last.clone()));
        }
        let row = self.rows.find_sorted(offset, |row| {
            note_replacement_predecessor_probe();
            u64::from_le_bytes(row[16..24].try_into().unwrap())
        })?;
        row.map(|row| decode_rewrite_entry(&row)).transpose()
    }
    pub(super) fn last(&self) -> Option<&RecordIndexEntry> {
        self.last.as_ref()
    }
    pub(super) fn maximum_sequence(&self) -> u64 {
        self.maximum_sequence
    }
    pub(super) fn cached_tails(&self) -> Option<&[(u32, u64)]> {
        self.tails.get(..self.tail_count)
    }
    pub(super) fn push(&mut self, entry: RecordIndexEntry) -> Result<()> {
        self.rows.push(encode_rewrite_entry(&entry)?)?;
        resident_index::UniformPrefix::observe(
            &mut self.uniform,
            self.rows.len() - 1,
            entry.record_offset,
        );
        self.maximum_sequence = self.maximum_sequence.max(entry.sequence);
        // Most formats have few block types. A fixed cache avoids rereading
        // the entire directory at every digest, without growing with records
        // or distinct block ids. Larger schemas retain the streamed fallback.
        if self.tail_count <= self.tails.len() {
            match self.tails[..self.tail_count].binary_search_by_key(&entry.block_id, |tail| tail.0)
            {
                Ok(at) => self.tails[at].1 = entry.record_offset,
                Err(at) => {
                    if self.tail_count < self.tails.len() {
                        self.tails.copy_within(at..self.tail_count, at + 1);
                        self.tails[at] = (entry.block_id, entry.record_offset);
                    }
                    self.tail_count += 1;
                }
            }
        }
        self.last = Some(entry);
        Ok(())
    }
}
impl RecordDirectory for RewriteDirectory {
    fn record_count(&self) -> usize {
        self.len()
    }
    fn record_at(&self, position: usize) -> Result<RecordIndexEntry> {
        if position.checked_add(1) == Some(self.len()) {
            return self.last.clone().ok_or(Error::UnexpectedEof);
        }
        let row = self.rows.get(position)?.ok_or(Error::UnexpectedEof)?;
        decode_rewrite_entry(&row)
    }
}

pub(super) struct OffsetRemap {
    rows: PackedRows<24>,
    last: Option<(u64, Option<u64>, Option<u64>)>,
}
impl OffsetRemap {
    pub(super) fn new(spec: FormatSpec) -> Self {
        Self {
            rows: PackedRows::new(
                resident_index::buffer_bytes(spec) - resident_index::rewrite_buffer_bytes(spec),
            ),
            last: None,
        }
    }
    pub(super) fn push(&mut self, old: u64, block: Option<u64>, key: Option<u64>) -> Result<()> {
        debug_assert!(self.last.is_none_or(|last| last.0 < old));
        let mut row = [0; 24];
        row[..8].copy_from_slice(&old.to_le_bytes());
        row[8..16].copy_from_slice(&block.unwrap_or(0).to_le_bytes());
        row[16..].copy_from_slice(&key.unwrap_or(0).to_le_bytes());
        self.rows.push(row)?;
        self.last = Some((old, block, key));
        Ok(())
    }
    pub(super) fn get(&self, old: u64, chain: Chain) -> Result<Option<u64>> {
        if let Some(last) = self.last.filter(|last| last.0 == old) {
            return Ok(if chain == Chain::Block {
                last.1
            } else {
                last.2
            });
        }
        let row = self
            .rows
            .find_sorted(old, |row| u64::from_le_bytes(row[..8].try_into().unwrap()))?;
        let at = if chain == Chain::Block { 8 } else { 16 };
        Ok(row.and_then(|row| {
            nonzero_offset(u64::from_le_bytes(row[at..at + 8].try_into().unwrap()))
        }))
    }
}

#[derive(Clone, Copy)]
pub(super) struct IndexPayload<'a> {
    pub(super) entries: &'a dyn RecordDirectory,
    pub(super) start: usize,
    pub(super) covered_start: Option<u64>,
    pub(super) record_offset: u64,
}
impl IndexPayload<'_> {
    pub(super) fn len(&self, spec: FormatSpec) -> Result<u64> {
        let count = self
            .entries
            .record_count()
            .checked_sub(self.start)
            .ok_or(Error::InvalidIndexCheckpoint)? as u64;
        spec.read_limits.check(ReadLimitKey::Records, count)?;
        count
            .checked_mul(SEGMENT_ENTRY_LEN)
            .and_then(|n| {
                n.checked_add(if self.covered_start.is_some() {
                    SEGMENT_PREFIX_LEN + SEGMENT_TRAILER_LEN
                } else {
                    INDEX_CHECKPOINT_PREFIX_LEN
                })
            })
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "rewrite checkpoint bytes",
            })
    }
    pub(super) fn write(&self, output: &mut crate::primary_file::PrimaryFile) -> Result<()> {
        let count = (self.entries.record_count() - self.start) as u64;
        if let Some(start) = self.covered_start {
            output.write_all(SEGMENT_MAGIC)?;
            output.write_all(&SEGMENT_VERSION.to_le_bytes())?;
            output.write_all(&[0; 2])?;
            output.write_all(&start.to_le_bytes())?;
            output.write_all(&count.to_le_bytes())?;
            output.write_all(&(self.start as u64).to_le_bytes())?;
        } else {
            output.write_all(INDEX_CHECKPOINT_MAGIC)?;
            output.write_all(&INDEX_CHECKPOINT_VERSION.to_le_bytes())?;
            output.write_all(&self.record_offset.to_le_bytes())?;
            output.write_all(&count.to_le_bytes())?;
        }
        let mut bytes = [0u8; 73 * 512];
        for start in (self.start..self.entries.record_count()).step_by(512) {
            let count = (self.entries.record_count() - start).min(512);
            for i in 0..count {
                let mut entry = self.entries.record_at(start + i)?;
                entry.committed |= self.covered_start.is_some();
                bytes[i * 73..(i + 1) * 73].copy_from_slice(&BufferedScan::encode(&entry)[..73]);
            }
            output.write_all(&bytes[..count * 73])?;
        }
        if self.covered_start.is_some() {
            output.write_all(&self.record_offset.to_le_bytes())?;
        }
        Ok(())
    }
}

pub(super) fn validate_checkpoint(
    spec: FormatSpec,
    file: &mut crate::primary_file::PrimaryFile,
    actual: &RecordIndexEntry,
    expected: &dyn RecordDirectory,
    count: usize,
    append_start: u64,
) -> Result<()> {
    let mut prefix = [0u8; 22];
    file.seek(SeekFrom::Start(actual.payload_offset))?;
    file.read_exact(&mut prefix)?;
    check_index_checkpoint_limits_for(spec, &prefix, false)?;
    let version = u16::from_le_bytes(prefix[4..6].try_into().unwrap());
    let row_len = match version {
        1 => 44,
        2 => 48,
        3 => 73,
        _ => return Err(Error::InvalidIndexCheckpoint),
    };
    if &prefix[..4] != INDEX_CHECKPOINT_MAGIC
        || u64::from_le_bytes(prefix[6..14].try_into().unwrap()) != actual.record_offset
        || u64::from_le_bytes(prefix[14..22].try_into().unwrap()) != count as u64
        || (count as u64)
            .checked_mul(row_len as u64)
            .and_then(|n| n.checked_add(22))
            != Some(actual.payload_len)
    {
        return Err(Error::InvalidIndexCheckpoint);
    }
    let mut bytes = [0u8; 73 * 512];
    let mut previous = append_start;
    for start in (0..count).step_by(512) {
        let n = (count - start).min(512);
        file.read_exact(&mut bytes[..n * row_len])?;
        for i in 0..n {
            let entry = read_index_entry_payload(&bytes[i * row_len..(i + 1) * row_len], version);
            if !record_headers_match(&entry, &expected.record_at(start + i)?)
                || entry.record_offset != previous
                || entry.record_offset.checked_add(RECORD_HEADER_LEN) != Some(entry.payload_offset)
            {
                return Err(Error::InvalidIndexCheckpoint);
            }
            previous = entry.checked_physical_end()?;
            if previous > actual.record_offset {
                return Err(Error::InvalidIndexCheckpoint);
            }
        }
    }
    if previous != actual.record_offset {
        return Err(Error::InvalidIndexCheckpoint);
    }
    Ok(())
}

/// Only the uncommon out-of-order case needs sorting. Sorted runs spill, and
/// pairwise merging keeps RAM independent of both row and run count.
pub(super) fn validate_sequences(directory: &dyn RecordDirectory, bytes: usize) -> Result<()> {
    let count = directory.record_count();
    if count < 2 {
        return Ok(());
    }
    let capacity = (bytes / 8).max(1).min(count);
    let mut chunk = Vec::<u64>::new();
    chunk
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource: "sequence uniqueness buffer",
            requested: (capacity as u64).saturating_mul(8),
        })?;
    let mut runs = SpillRows::<8>::new(bytes.min(64 * 1024));
    for start in (0..count).step_by(capacity) {
        chunk.clear();
        for i in start..count.min(start.saturating_add(capacity)) {
            chunk.push(directory.record_at(i)?.sequence);
        }
        chunk.sort_unstable();
        for pair in chunk.windows(2) {
            if pair[0] == pair[1] {
                return Err(duplicate());
            }
        }
        if capacity == count {
            return Ok(());
        }
        for &sequence in &chunk {
            runs.push(sequence.to_le_bytes())?;
        }
    }
    drop(chunk);
    let mut width = capacity;
    while width < count {
        let mut next = SpillRows::<8>::new(bytes.min(64 * 1024));
        let step = width.saturating_mul(2);
        for start in (0..count).step_by(step) {
            let middle = count.min(start.saturating_add(width));
            let end = count.min(start.saturating_add(step));
            let mut left = RunReader::new(&runs, start, middle);
            let mut right = RunReader::new(&runs, middle, end);
            let (mut a, mut b) = (left.next()?, right.next()?);
            while a.is_some() || b.is_some() {
                let value = match (a, b) {
                    (Some(x), Some(y)) if x == y => return Err(duplicate()),
                    (Some(x), Some(y)) if x < y => {
                        a = left.next()?;
                        x
                    }
                    (_, Some(y)) => {
                        b = right.next()?;
                        y
                    }
                    (Some(x), None) => {
                        a = left.next()?;
                        x
                    }
                    _ => unreachable!(),
                };
                next.push(value.to_le_bytes())?;
            }
        }
        runs = next;
        width = step;
    }
    Ok(())
}
fn duplicate() -> Error {
    Error::InvalidCanonicalEncoding("duplicate native record sequence")
}
struct RunReader<'a> {
    rows: &'a SpillRows<8>,
    position: usize,
    end: usize,
    buffer: [u64; 512],
    at: usize,
    len: usize,
}
impl<'a> RunReader<'a> {
    fn new(rows: &'a SpillRows<8>, position: usize, end: usize) -> Self {
        Self {
            rows,
            position,
            end,
            buffer: [0; 512],
            at: 0,
            len: 0,
        }
    }
    fn next(&mut self) -> Result<Option<u64>> {
        if self.at == self.len {
            self.len = (self.end - self.position).min(self.buffer.len());
            self.at = 0;
            for i in 0..self.len {
                self.buffer[i] = u64::from_le_bytes(
                    self.rows
                        .get(self.position + i)?
                        .ok_or(Error::UnexpectedEof)?,
                );
            }
            self.position += self.len;
        }
        if self.len == 0 {
            return Ok(None);
        }
        let value = self.buffer[self.at];
        self.at += 1;
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> FormatSpec {
        FormatSpec::new(
            b"VREWRITE",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            &[],
        )
        .with_resource_limits(crate::ReadLimits::STANDARD.with_max_index_bytes(256))
    }
    #[test]
    fn compact_rows_preserve_fields_and_refuse_inconsistent_geometry() -> Result<()> {
        let mut entry = read_index_entry_payload(&[0; 73], 3);
        entry.block_id = 17;
        entry.block_version = 9;
        entry.flags = 5;
        entry.sequence = u64::MAX;
        entry.record_offset = u64::MAX - RECORD_HEADER_LEN;
        entry.payload_offset = u64::MAX;
        entry.checksum = u32::MAX;
        entry.uncompressed_len_hint = 123;
        entry.prev_same_block_offset = Some(99);
        entry.prev_same_key_offset = Some(3);
        entry.mutable_flags = 7;
        entry.committed = true;
        for footer in [None, Some(u64::MAX)] {
            entry.footer_offset = footer;
            assert_eq!(decode_rewrite_entry(&encode_rewrite_entry(&entry)?)?, entry);
        }
        entry.footer_offset = Some(0);
        assert!(encode_rewrite_entry(&entry).is_err());
        entry.footer_offset = None;
        entry.payload_offset -= 1;
        assert!(encode_rewrite_entry(&entry).is_err());
        Ok(())
    }
    #[test]
    fn disjoint_sequence_runs_never_admit_a_duplicate() {
        // Exhaust every five-value history over a small alphabet, including
        // gaps, equal neighbours and duplicates separated by run boundaries.
        for mut encoded in 0..5usize.pow(5) {
            let mut ranges = SequenceRanges::default();
            let mut seen = [false; 5];
            let mut duplicate = false;
            for _ in 0..5 {
                let value = encoded % 5;
                encoded /= 5;
                duplicate |= seen[value];
                seen[value] = true;
                ranges.note(value as u64);
            }
            assert!(!ranges.proves_unique() || !duplicate);
        }
        for values in [[99, 1, 2, 3, 4], [9, 10, 1, 2, 3], [0, 1, 2, 3, 99]] {
            let mut ranges = SequenceRanges::default();
            for value in values {
                ranges.note(value);
            }
            assert!(ranges.proves_unique());
        }
    }
    #[test]
    fn rewrite_pages_preserve_entries_and_retry_across_refused_growth() -> Result<()> {
        let mut directory = RewriteDirectory::new(spec().with_resource_limits(
            crate::ReadLimits::STANDARD.with_max_index_bytes(2 * 1024 * 1024),
        ));
        let mut expected = Vec::new();
        for i in 0..600u64 {
            let mut entry = read_index_entry_payload(&[0; 73], 3);
            entry.block_id = (i % 7 + 1) as u32;
            entry.sequence = i + 1;
            entry.record_offset = 4096 + i * 160;
            entry.payload_offset = entry.record_offset + RECORD_HEADER_LEN;
            entry.payload_len = 32 + i % 17;
            entry.footer_offset = None;
            entry.prev_same_block_offset = i.checked_sub(7).map(|j| 4096 + j * 160);
            entry.prev_same_key_offset = i.checked_sub(11).map(|j| 4096 + j * 160);
            entry.committed = i % 2 == 0;
            if matches!(i, 256 | 512) {
                let result =
                    crate::pib_probe::with_allocation_refusal(1, || directory.push(entry.clone()));
                assert!(matches!(result, Err(Error::Io(e))
                    if e.kind() == std::io::ErrorKind::OutOfMemory));
                assert_eq!(directory.len(), i as usize);
                assert_eq!(
                    directory.record_at(i as usize - 1)?,
                    expected[i as usize - 1]
                );
            }
            directory.push(entry.clone())?;
            expected.push(entry);
        }
        for at in [0, 1, 254, 255, 256, 257, 510, 511, 512, 513, 598, 599] {
            assert_eq!(directory.record_at(at)?, expected[at]);
            assert_eq!(
                directory.find_offset(expected[at].record_offset)?,
                Some(expected[at].clone())
            );
            assert!(
                directory
                    .find_offset(expected[at].record_offset + 1)?
                    .is_none()
            );
        }
        Ok(())
    }

    #[test]
    fn digest_tail_cache_is_sorted_and_falls_back_when_full() -> Result<()> {
        let mut directory = RewriteDirectory::new(spec());
        for id in (0..16u32).rev() {
            let mut entry = read_index_entry_payload(&[0; 73], 3);
            entry.payload_offset = entry.record_offset + RECORD_HEADER_LEN;
            entry.block_id = id;
            entry.record_offset = 100 + u64::from(id);
            entry.payload_offset = entry.record_offset + RECORD_HEADER_LEN;
            entry.sequence = u64::from(id);
            directory.push(entry)?;
        }
        let expected: Vec<_> = (0..16u32).map(|id| (id, 100 + u64::from(id))).collect();
        assert_eq!(directory.cached_tails(), Some(expected.as_slice()));
        let mut entry = read_index_entry_payload(&[0; 73], 3);
        entry.payload_offset = entry.record_offset + RECORD_HEADER_LEN;
        entry.block_id = 7;
        entry.record_offset = 999;
        entry.payload_offset = entry.record_offset + RECORD_HEADER_LEN;
        entry.sequence = 30;
        directory.push(entry.clone())?;
        assert_eq!(directory.cached_tails().unwrap()[7], (7, 999));
        assert_eq!(directory.maximum_sequence(), 30);
        entry.block_id = 16;
        directory.push(entry.clone())?;
        assert!(directory.cached_tails().is_none());
        entry.block_id = 7;
        directory.push(entry)?;
        assert!(directory.cached_tails().is_none());
        Ok(())
    }
    #[test]
    fn external_sequence_validation_catches_duplicates_across_runs() -> Result<()> {
        for duplicate in [false, true] {
            let mut directory = RewriteDirectory::new(spec());
            for i in 0..129u64 {
                let mut entry = read_index_entry_payload(&[0; 73], 3);
                entry.payload_offset = entry.record_offset + RECORD_HEADER_LEN;
                entry.sequence = (i * 73) % 129;
                if duplicate && i == 128 {
                    entry.sequence = 0;
                }
                directory.push(entry)?;
            }
            let result = validate_sequences(&directory, 64);
            assert_eq!(
                result.is_ok(),
                !duplicate,
                "sequence external-merge verdict"
            );
        }
        Ok(())
    }
    #[test]
    fn remapping_keeps_distinct_chains_across_spills() -> Result<()> {
        let mut map = OffsetRemap::new(spec());
        for i in 1..100u64 {
            map.push(i * 100, Some(i * 80), (i % 3 != 0).then_some(i * 60))?;
        }
        for i in (1..100u64).rev() {
            assert_eq!(map.get(i * 100, Chain::Block)?, Some(i * 80));
            assert_eq!(
                map.get(i * 100, Chain::Key)?,
                (i % 3 != 0).then_some(i * 60)
            );
            assert_eq!(map.get(i * 100 + 1, Chain::Block)?, None);
        }
        Ok(())
    }
}
