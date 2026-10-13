//! Owner-local record directory. Spill I/O precedes record publication, and
//! rollback only changes the logical row count. No shared cache or admission.
use super::{
    Error, FormatSpec, RecordDirectory, RecordIndexEntry, Result, SnapshotFile, fault_record_entry,
};
use crate::ReadLimit;
use crate::spill_rows::SpillRows;

pub(super) fn buffer_bytes(spec: FormatSpec) -> usize {
    match spec.read_limits.resolve().max_index_bytes {
        ReadLimit::Finite(bytes) => bytes.min(2 * 1024 * 1024) as usize,
        _ => 2 * 1024 * 1024,
    }
}

#[derive(Debug)]
#[must_use = "reserve before appending the authoritative record"]
pub struct ReservedIndexSlot(());

#[derive(Debug)]
pub struct ResidentIndex {
    slots: SpillRows<16>,
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
        let row = self.slots.get(position).ok()??;
        Some(u32::from_le_bytes(row[8..12].try_into().unwrap()))
    }
    pub(crate) fn truncate(&mut self, len: usize) {
        self.slots.truncate(len);
    }
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub(crate) fn entry_at(&self, position: usize) -> Result<RecordIndexEntry> {
        let row = self.slots.get(position)?.ok_or(Error::UnexpectedEof)?;
        fault_record_entry(
            &self.snapshot,
            self.spec,
            u64::from_le_bytes(row[..8].try_into().unwrap()),
            row[12] != 0,
        )
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
