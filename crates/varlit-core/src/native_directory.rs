//! Owner-local record directory. Spill I/O precedes record publication, and
//! rollback only changes the logical row count. No shared cache or admission.
use super::native_rewrite::RewriteRows;
use super::{
    Error, FormatSpec, RecordDirectory, RecordIndexEntry, Result, SnapshotFile, fault_record_entry,
};
use crate::ReadLimit;
use crate::spill_rows::SpillRows;

pub(super) fn buffer_bytes(spec: FormatSpec) -> usize {
    match spec.read_limits.resolve().max_index_bytes {
        ReadLimit::Finite(bytes) if bytes != u64::MAX => {
            usize::try_from(bytes).unwrap_or(usize::MAX)
        }
        _ => 2 * 1024 * 1024,
    }
}

pub(super) fn rewrite_buffer_bytes(spec: FormatSpec) -> usize {
    let bytes = buffer_bytes(spec);
    bytes / 10 * 7 + bytes % 10 * 7 / 10
}

#[derive(Debug)]
#[must_use = "reserve before appending the authoritative record"]
pub struct ReservedIndexSlot(());

#[derive(Debug)]
pub struct ResidentIndex {
    slots: SpillRows<16>,
    rewritten: Option<RewriteRows>,
    rewritten_len: usize,
    snapshot: SnapshotFile,
    spec: FormatSpec,
    retained: bool,
}
fn encode(entry: &RecordIndexEntry) -> [u8; 16] {
    let mut row = [0; 16];
    row[..8].copy_from_slice(&entry.record_offset.to_le_bytes());
    row[8..12].copy_from_slice(&entry.block_id.to_le_bytes());
    row[12] = u8::from(entry.committed);
    row
}
impl ResidentIndex {
    /// Keep the already written private directory after publication. Subsequent
    /// appends use the remaining share of the unchanged index buffer budget.
    pub(super) fn from_rewrite_rows(
        rows: RewriteRows,
        snapshot: SnapshotFile,
        spec: FormatSpec,
    ) -> Self {
        Self {
            rewritten_len: rows.len(),
            rewritten: Some(rows),
            slots: SpillRows::new(buffer_bytes(spec) - rewrite_buffer_bytes(spec)),
            snapshot,
            spec,
            retained: true,
        }
    }
    pub(crate) fn adopt_generation<D: RecordDirectory + ?Sized>(
        entries: &D,
        snapshot: SnapshotFile,
        spec: FormatSpec,
    ) -> Result<Self> {
        let mut result = Self::empty(snapshot, spec);
        result.slots.reserve_rows_hint(entries.record_count())?;
        for at in 0..entries.record_count() {
            result.slots.push(encode(&entries.record_at(at)?))?;
        }
        Ok(result)
    }
    pub(crate) fn empty(snapshot: SnapshotFile, spec: FormatSpec) -> Self {
        Self {
            slots: SpillRows::new(buffer_bytes(spec)),
            rewritten: None,
            rewritten_len: 0,
            snapshot,
            spec,
            retained: true,
        }
    }
    pub(crate) fn none_retained(snapshot: SnapshotFile, spec: FormatSpec) -> Self {
        Self {
            retained: false,
            ..Self::empty(snapshot, spec)
        }
    }
    pub(crate) fn is_retained(&self) -> bool {
        self.retained
    }
    pub(crate) fn rebind(&mut self, snapshot: SnapshotFile) {
        self.snapshot = snapshot;
    }
    pub(crate) fn reserve(
        &mut self,
        charge: impl FnOnce() -> Result<u64>,
    ) -> Result<ReservedIndexSlot> {
        charge()?;
        self.slots.reserve_one()?;
        Ok(ReservedIndexSlot(()))
    }
    pub(crate) fn install(&mut self, slot: ReservedIndexSlot, entry: &RecordIndexEntry) {
        let ReservedIndexSlot(()) = slot;
        self.slots.push_reserved(encode(entry));
    }
    /// A failed follow cannot leave a partially adopted directory. All old
    /// rows remain readable even if a spill succeeded before a later error.
    pub(crate) fn extend(
        &mut self,
        entries: impl Iterator<Item = Result<RecordIndexEntry>>,
    ) -> Result<()> {
        let original = self.len();
        let result = (|| {
            for entry in entries {
                self.slots.push(encode(&entry?))?;
            }
            Ok(())
        })();
        if result.is_err() {
            self.truncate(original);
        }
        result
    }
    pub(crate) fn block_id_at(&self, position: usize) -> Option<u32> {
        // None asks the caller to fault the entry (and propagate its error),
        // never to skip a record. This is only a filtering hint.
        let row = self.row(position).ok()??;
        Some(u32::from_le_bytes(row[8..12].try_into().unwrap()))
    }
    pub(crate) fn truncate(&mut self, len: usize) {
        self.slots.truncate(len.saturating_sub(self.rewritten_len));
        self.rewritten_len = self.rewritten_len.min(len);
    }
    pub(crate) fn len(&self) -> usize {
        self.rewritten_len + self.slots.len()
    }
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub(crate) fn entry_at(&self, position: usize) -> Result<RecordIndexEntry> {
        let row = self.row(position)?.ok_or(Error::UnexpectedEof)?;
        fault_record_entry(
            &self.snapshot,
            self.spec,
            u64::from_le_bytes(row[..8].try_into().unwrap()),
            row[12] != 0,
        )
    }
    fn row(&self, position: usize) -> std::io::Result<Option<[u8; 16]>> {
        if position >= self.rewritten_len {
            return self.slots.get(position - self.rewritten_len);
        }
        let Some(source) = self.rewritten.as_ref().unwrap().get(position)? else {
            return Ok(None);
        };
        let mut row = [0; 16];
        row[..8].copy_from_slice(&source[16..24]);
        row[8..12].copy_from_slice(&source[..4]);
        row[12] = source[56];
        Ok(Some(row))
    }
    pub(crate) fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = Result<RecordIndexEntry>> + ExactSizeIterator + '_ {
        (0..self.len()).map(|at| self.entry_at(at))
    }
    pub(crate) fn contains_block(&self, block_id: u32) -> Result<bool> {
        for entry in self.iter() {
            if entry?.block_id == block_id {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub(crate) fn last_entry_of_block(&self, block_id: u32) -> Result<Option<RecordIndexEntry>> {
        for entry in self.iter().rev() {
            let entry = entry?;
            if entry.block_id == block_id {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adopted_prefix_and_append_tail_rollback_independently() -> Result<()> {
        let spec = FormatSpec::builder()
            .magic(b"VROWTEST")
            .build()?
            .with_resource_limits(crate::ReadLimits::STANDARD.with_max_index_bytes(256));
        let snapshot = SnapshotFile::new(tempfile::tempfile()?)?;
        let mut packed = RewriteRows::new(128);
        for i in 0..300u64 {
            let mut row = [0; 62];
            row[..4].copy_from_slice(&(i as u32 + 1).to_le_bytes());
            row[16..24].copy_from_slice(&(100 + i * 80).to_le_bytes());
            packed.push(row)?;
        }
        let mut index = ResidentIndex::from_rewrite_rows(packed, snapshot, spec);
        let entry = super::super::read_index_entry_payload(&[0; 73], 3);
        let result = index.extend((0..30).map(|i| {
            if i == 29 {
                return Err(Error::UnexpectedEof);
            }
            Ok(entry.clone())
        }));
        assert!(result.is_err());
        assert_eq!(index.len(), 300);
        assert_eq!(index.block_id_at(299), Some(300));
        index.truncate(150);
        let mut appended = entry;
        appended.block_id = 999;
        appended.record_offset = 98765;
        let slot = index.reserve(|| Ok(0))?;
        index.install(slot, &appended);
        assert_eq!(index.len(), 151);
        assert_eq!(index.block_id_at(149), Some(150));
        assert_eq!(index.block_id_at(150), Some(999));
        assert_eq!(index.block_id_at(151), None);
        Ok(())
    }
}
