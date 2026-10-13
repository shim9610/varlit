//! Reader-local, staged metadata deltas between immutable matrix generations.
//! Nothing visible changes until all I/O, integrity checks and reservations pass.
use super::*;
use crate::matrix_generation::{MatrixFile, View};
use crate::spill_rows::SpillRows;

pub(super) struct Changes<'a> {
    old: &'a View,
    next: &'a View,
    limits: ReadLimits,
}
impl Changes<'_> {
    /// The directory diff is sorted. Seek directly to each metadata region;
    /// payload-only changes do not cause payload reads during follow.
    fn units(&self, base: u64, len: u64, unit: u64) -> Result<SpillRows<8>> {
        let end = base.checked_add(len).ok_or(Error::InvalidMatrixLayout)?;
        let limit = match self.limits.resolve().max_matrix_bitmap_bytes {
            crate::ReadLimit::Finite(n) => n.min(4096) as usize,
            _ => 4096,
        };
        let mut result = SpillRows::new(limit);
        if len == 0 {
            return Ok(result);
        }
        let mut last = None;
        let mut old = [0; 4096];
        let mut next = [0; 4096];
        self.old.for_each_changed_page(
            self.next,
            base / 4096,
            (end - 1) / 4096,
            &mut |page| -> Result<()> {
                let start = (page * 4096).max(base);

                let stop = ((page + 1) * 4096).min(end);
                let n = (stop - start) as usize;
                self.old.read_exact_at(start, &mut old[..n])?;
                self.next.read_exact_at(start, &mut next[..n])?;
                for i in 0..n {
                    if old[i] != next[i] {
                        let index = (start + i as u64 - base) / unit;
                        if last != Some(index) {
                            result.push(index.to_le_bytes())?;
                            last = Some(index);
                        }
                    }
                }
                Ok(())
            },
        )?;
        Ok(result)
    }
    fn unchanged(&self, base: u64, len: u64) -> Result<()> {
        if self.units(base, len, 4096)?.len() != 0 {
            return Err(Error::InvalidMatrixLayout);
        }
        Ok(())
    }
}

/// O(changed slots + changed bitmap pages), never a clone of a live index.
/// Private fields prevent callers from manufacturing completeness evidence.
pub(super) struct BitmapDelta {
    directory: BitmapDirectory,
    // Only pages currently in the bounded cache need removal at installation.
    invalidate: Vec<u64>,
}
fn row(rows: &SpillRows<8>, at: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        rows.get(at)?.ok_or(Error::InvalidMatrixLayout)?,
    ))
}
fn mark(set: &mut BitmapDirectory, page: u64) -> Result<()> {
    if !set.contains(page)? {
        set.push(page)?;
    }
    Ok(())
}
impl BitmapDelta {
    pub(super) fn prepare(
        bits: &mut SparseBitmap,
        index: u64,
        changes: &Changes<'_>,
        _budget: &mut ResidentBitmapBudget,
        verify: bool,
    ) -> Result<Self> {
        let capacity = bits.page_count.min(PAGE_INDEX_MAX_ENTRIES);
        let mut header = [0; 8];
        let count = if capacity == 0 {
            0
        } else {
            changes.next.read_exact_at(index, &mut header)?;
            count_open_bitmap_bytes_read(8);
            page_index_header_count(u64::from_le_bytes(header), capacity)
                .ok_or(Error::MatrixFatalCorruption)?
        };
        let changed = changes.units(index + 8, count * 8, 8)?;
        let mut directory = bits.directory.clone();
        let mut affected = BitmapDirectory::default();
        let mut changed_slots = SpillRows::<8>::new(4096);
        let mut update = |slot: u64| -> Result<()> {
            changes
                .next
                .read_exact_at(index + 8 + slot * 8, &mut header)?;
            count_open_bitmap_bytes_read(8);
            let raw = u64::from_le_bytes(header);
            if raw == 0 || raw > bits.page_count {
                return Err(Error::MatrixFatalCorruption);
            }
            if let Some(old) = bits.directory.slot(slot)? {
                mark(&mut affected, old)?;
            }
            mark(&mut affected, raw - 1)?;
            directory.set_slot(slot, raw - 1)?;
            changed_slots.push(slot.to_le_bytes())?;
            Ok(())
        };
        for at in 0..changed.len() {
            update(row(&changed, at)?)?;
        }
        // Header growth can expose bytes that were already present unchanged.
        for slot in bits.directory.len()..count {
            update(slot)?;
        }
        for slot in count..bits.directory.len() {
            mark(
                &mut affected,
                bits.directory
                    .slot(slot)?
                    .ok_or(Error::InvalidMatrixLayout)?,
            )?;
        }
        directory.truncate(count);
        affected.visit_pages(|page, _| {
            let first = match bits.directory.first(page)? {
                Some(at) if at < count && directory.slot(at)? == Some(page) => Some(at),
                _ => None,
            };
            directory.set_first(page, first)
        })?;
        for at in 0..changed_slots.len() {
            let slot = row(&changed_slots, at)?;
            let page = directory.slot(slot)?.ok_or(Error::InvalidMatrixLayout)?;
            let first = directory.first(page)?.map_or(slot, |old| old.min(slot));
            directory.set_first(page, Some(first))?;
        }
        // Only crash-recovery duplicates require a bitmap-local survivor scan.
        if bits.directory.len() != bits.directory.distinct() && affected.len() != 0 {
            for slot in 0..count {
                let page = directory.slot(slot)?.ok_or(Error::InvalidMatrixLayout)?;
                if affected.contains(page)? {
                    let first = directory.first(page)?.map_or(slot, |old| old.min(slot));
                    directory.set_first(page, Some(first))?;
                }
            }
        }
        if let Some(backing) = &bits.backing {
            let pages = changes.units(backing.base_offset, bits.byte_len, BITMAP_PAGE_BYTES)?;
            for at in 0..pages.len() {
                mark(&mut affected, row(&pages, at)?)?;
            }
            if let Some(digest) = backing.digest_base {
                let pages =
                    changes.units(digest, bits.page_count * PAGE_DIGEST_LEN, PAGE_DIGEST_LEN)?;
                for at in 0..pages.len() {
                    mark(&mut affected, row(&pages, at)?)?;
                }
                if verify {
                    let mut bytes = [0; 4096];
                    affected.visit_pages(|page, _| {
                        let len = bits.page_len(page)? as usize;
                        changes.next.read_exact_at(
                            backing.base_offset + page * BITMAP_PAGE_BYTES,
                            &mut bytes[..len],
                        )?;
                        changes
                            .next
                            .read_exact_at(page_digest_offset(digest, page)?, &mut header)?;
                        let stored = u32::from_le_bytes(header[..4].try_into().unwrap());
                        let state = u32::from_le_bytes(header[4..].try_into().unwrap());
                        if !page_bytes_are_authentic(&bytes[..len], stored, state)? {
                            return Err(Error::MatrixFatalCorruption);
                        }
                        count_open_bitmap_bytes_read(len as u64 + 8);
                        count_open_bitmap_pages_visited(1);
                        Ok(())
                    })?;
                }
            }
        }
        let mut invalidate = Vec::new();
        for &page in bits.store().pages.keys() {
            if affected.contains(page)? {
                try_reserve_vec(&mut invalidate, 1, "matrix follow cache invalidation")?;
                invalidate.push(page);
            }
        }
        Ok(Self {
            directory,
            invalidate,
        })
    }
    pub(super) fn apply(self, bits: &mut SparseBitmap) {
        bits.directory = self.directory;
        let store = bits.store_mut();
        for page in self.invalidate {
            store.lru_unlink(page);
            if let Some(held) = store.pages.remove(&page) {
                store.ones = store.ones.saturating_sub(held.ones);
                if held.cached {
                    store.cached_bytes -= held.bytes.len() as u64;
                } else {
                    store.charged_bytes -= held.bytes.len() as u64;
                }
            }
        }
    }
}

pub(crate) struct MatrixDelta {
    commits: Vec<BitmapDelta>,
    validity: Vec<Option<BitmapDelta>>,
    source: Source,
    resident_index: u64,
}
impl MatrixDelta {
    pub(crate) fn prepare(
        layout: &mut MatrixLayout,
        old: &MatrixFile,
        next: &MatrixFile,
        layout_start: u64,
    ) -> Result<Self> {
        // An incomplete mirror cannot prove a delta complete. Keep the captured
        // generation on failure; a fresh open is the explicit recovery route.
        if layout
            .crc_findings
            .iter()
            .any(|f| f.severity == MatrixCorruptionSeverity::Fatal)
            || layout
                .commits
                .iter()
                .any(|c| c.quarantine_finding.is_some())
        {
            return Err(Error::MatrixFatalCorruption);
        }
        let changes = Changes {
            old: old.paged().unwrap(),
            next: next.paged().unwrap(),
            limits: layout.read_limits,
        };
        let header = read_header(&mut old.read_borrow(), layout_start)?;
        changes.unchanged(0, header.commit_map_off)?;
        if header.region_crc_len != 0 {
            changes.unchanged(header.region_crc_off, MCRC_HEADER_LEN)?;
        }
        let mut budget = ResidentBitmapBudget::resume(
            layout.read_limits,
            layout.resident_bitmap_bytes,
            layout.resident_page_index_bytes,
        );
        let verify = layout.read_limits.admit_matrix_metadata_verification();
        let mut commits = Vec::new();
        try_reserve_vec(&mut commits, layout.commits.len(), "matrix follow delta")?;
        for commit in &mut layout.commits {
            commits.push(BitmapDelta::prepare(
                &mut commit.bits,
                commit.index_offset,
                &changes,
                &mut budget,
                verify,
            )?);
        }
        let mut validity = Vec::new();
        try_reserve_vec(&mut validity, layout.blocks.len(), "matrix follow delta")?;
        for block in &mut layout.blocks {
            validity.push(match block.crc_valid_index_offset {
                Some(index) => Some(block.crc_valid_bits.prepare_follow(
                    index,
                    &changes,
                    &mut budget,
                )?),
                None => None,
            });
        }
        Ok(Self {
            commits,
            validity,
            source: next.source()?,
            resident_index: budget.index,
        })
    }
    pub(crate) fn apply(self, layout: &mut MatrixLayout) {
        for (delta, commit) in self.commits.into_iter().zip(&mut layout.commits) {
            delta.apply(&mut commit.bits);
        }
        for (delta, block) in self.validity.into_iter().zip(&mut layout.blocks) {
            if let Some(delta) = delta {
                block.crc_valid_bits.apply_follow(delta);
            }
        }
        layout.resident_page_index_bytes = self.resident_index;
        rebind_generation_backing(layout, &self.source);
        record_open_resident_bitmap_bytes(layout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    fn index_case(old_slots: &[u64], physical_slots: &[u64], new_slots: &[u64]) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.stratava");
        let base = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        base.set_len(4096).unwrap();
        let mut writer = MatrixFile::create(&path, &base, 4096, [1; 16], 64).unwrap();
        writer
            .write_all(
                &page_index_header_value(old_slots.len() as u64)
                    .unwrap()
                    .to_le_bytes(),
            )
            .unwrap();
        for &page in physical_slots {
            writer.write_all(&(page + 1).to_le_bytes()).unwrap();
        }
        writer.publish(4096).unwrap();
        let old = writer.refreshed().unwrap();
        let mut bits = SparseBitmap::new(8 * 32768).unwrap();
        let mut budget = ResidentBitmapBudget::new(ReadLimits::STANDARD);
        for &page in old_slots {
            bits.note_indexed_page(page, &mut budget).unwrap();
        }
        writer.seek(SeekFrom::Start(0)).unwrap();
        writer
            .write_all(
                &page_index_header_value(new_slots.len() as u64)
                    .unwrap()
                    .to_le_bytes(),
            )
            .unwrap();
        for &page in new_slots {
            writer.write_all(&(page + 1).to_le_bytes()).unwrap();
        }
        writer.publish(4096).unwrap();
        let next = writer.refreshed().unwrap();
        let changes = Changes {
            old: old.paged().unwrap(),
            next: next.paged().unwrap(),
            limits: ReadLimits::STANDARD,
        };
        let delta = BitmapDelta::prepare(&mut bits, 0, &changes, &mut budget, false).unwrap();
        assert_eq!(
            (0..bits.directory.len())
                .map(|at| bits.directory.slot(at).unwrap().unwrap())
                .collect::<Vec<_>>(),
            old_slots,
            "preparation must not change the visible mirror"
        );
        delta.apply(&mut bits);
        assert_eq!(
            (0..bits.directory.len())
                .map(|at| bits.directory.slot(at).unwrap().unwrap())
                .collect::<Vec<_>>(),
            new_slots
        );
        let mut expected = HashMap::new();
        for (at, &page) in new_slots.iter().enumerate() {
            expected.entry(page).or_insert(at as u64);
        }
        let mut actual = HashMap::new();
        bits.directory
            .visit_pages(|page, at| {
                actual.insert(page, at);
                Ok(())
            })
            .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(budget.index, bits.resident_index_bytes());
    }

    #[test]
    fn deltas_preserve_duplicate_survivors_swap_removals_and_header_only_growth() {
        index_case(&[2, 1, 2], &[2, 1, 2], &[1, 2]);
        index_case(&[2, 1, 2], &[2, 1, 2], &[1, 1]);
        index_case(&[0, 1, 2], &[0, 1, 2], &[2, 1]);
        index_case(&[0], &[0, 1, 2], &[0, 1, 2]);
        index_case(&[0], &[0, 1, 2, 3, 4], &[0, 1, 2, 5, 4]);
        index_case(&[0, 1, 2], &[0, 1, 2], &[]);
    }
}
