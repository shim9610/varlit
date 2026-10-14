//! Bounded scan staging; caller-owned Vec APIs explicitly retain their owned
//! result limits. Ordinary opens and follows use temporary fixed-width rows.
use super::*;
use crate::spill_rows::SpillRows;

pub(super) trait ScanBuffer: RecordDirectory {
    fn reset(&mut self);
    fn reserve_entry(&mut self, spec: FormatSpec) -> Result<()>;
    fn push_entry(&mut self, entry: RecordIndexEntry) -> Result<()>;
    fn finish(&mut self, spec: FormatSpec, scanned: &mut ScannedIndex) -> Result<()>;
    fn checkpoint_limits(&self, spec: FormatSpec, prefix: &[u8]) -> Result<()>;
    fn segments(
        &mut self,
        spec: FormatSpec,
        file: &mut File,
        start: u64,
        end: u64,
    ) -> Result<Option<ScannedIndex>>;
    fn entries(
        &self,
    ) -> impl DoubleEndedIterator<Item = Result<RecordIndexEntry>> + ExactSizeIterator + '_ {
        (0..self.record_count()).map(|at| self.record_at(at))
    }
    fn last_entry(&self) -> Result<Option<RecordIndexEntry>> {
        self.record_count()
            .checked_sub(1)
            .map(|at| self.record_at(at))
            .transpose()
    }
}
impl ScanBuffer for Vec<RecordIndexEntry> {
    fn reset(&mut self) {
        self.clear();
    }
    fn reserve_entry(&mut self, spec: FormatSpec) -> Result<()> {
        reserve_scanned_entry(spec, self)
    }
    fn push_entry(&mut self, entry: RecordIndexEntry) -> Result<()> {
        self.push(entry);
        Ok(())
    }
    fn finish(&mut self, spec: FormatSpec, scanned: &mut ScannedIndex) -> Result<()> {
        validate_unique_sequences(self)?;
        if spec.commit_policy.is_transaction_marker() {
            self.truncate(committed_prefix_len(self).unwrap_or(0));
            for entry in self.iter_mut() {
                entry.committed = true;
            }
            scanned.commit_boundary_applied(self)?;
        }
        Ok(())
    }
    fn checkpoint_limits(&self, spec: FormatSpec, prefix: &[u8]) -> Result<()> {
        check_index_checkpoint_limits(spec, prefix)
    }
    fn segments(
        &mut self,
        spec: FormatSpec,
        file: &mut File,
        start: u64,
        end: u64,
    ) -> Result<Option<ScannedIndex>> {
        match walk_segment_chain(spec, file, start, self, end) {
            Ok(scanned) => Ok(Some(scanned)),
            Err(error @ Error::MissingResourceLimit { .. })
            | Err(error @ Error::TrustedUnboundedRequiresExplicitApi { .. }) => Err(error),
            Err(_) => {
                self.clear();
                Ok(None)
            }
        }
    }
}

pub(super) struct BufferedScan {
    rows: SpillRows<77>,
    spec: FormatSpec,
    committed: bool,
    kept: usize,
    marker_seen: bool,
    suffix: Vec<RecordIndexEntry>, // at most SEGMENT and DIGEST after a marker
    previous: Option<u64>,
    sequences: Option<crate::disk_index::ScratchIndex>,
}
impl BufferedScan {
    pub(super) fn new(spec: FormatSpec) -> Self {
        Self {
            rows: SpillRows::new(resident_index::buffer_bytes(spec)),
            spec,
            committed: false,
            kept: 0,
            marker_seen: false,
            suffix: Vec::new(),
            previous: None,
            sequences: None,
        }
    }
    fn encode(entry: &RecordIndexEntry) -> [u8; 77] {
        let mut row = [0; 77];
        row[..4].copy_from_slice(&entry.block_id.to_le_bytes());
        row[4..6].copy_from_slice(&entry.block_version.to_le_bytes());
        row[6..8].copy_from_slice(&entry.flags.to_le_bytes());
        for (at, value) in [
            (8, entry.sequence),
            (16, entry.record_offset),
            (24, entry.payload_offset),
            (32, entry.payload_len),
            (48, entry.footer_offset.unwrap_or(0)),
            (56, entry.prev_same_block_offset.unwrap_or(0)),
            (64, entry.prev_same_key_offset.unwrap_or(0)),
        ] {
            row[at..at + 8].copy_from_slice(&value.to_le_bytes());
        }
        row[40..44].copy_from_slice(&entry.checksum.to_le_bytes());
        row[44..48].copy_from_slice(&entry.uncompressed_len_hint.to_le_bytes());
        row[72] = u8::from(entry.committed);
        row[73..77].copy_from_slice(&entry.mutable_flags.to_le_bytes());
        row
    }
    fn remember_sequence(index: &mut crate::disk_index::ScratchIndex, sequence: u64) -> Result<()> {
        let key = sequence.to_be_bytes().to_vec();
        if index.get(&key)?.is_some() {
            return Err(Error::InvalidCanonicalEncoding(
                "duplicate native record sequence",
            ));
        }
        index.apply(&std::collections::BTreeMap::from([(key, [0; 52])]))
    }
}
impl RecordDirectory for BufferedScan {
    fn record_count(&self) -> usize {
        self.rows.len()
    }
    fn record_at(&self, position: usize) -> Result<RecordIndexEntry> {
        let row = self.rows.get(position)?.ok_or(Error::UnexpectedEof)?;
        let mut entry = read_index_entry_payload(&row, 3);
        entry.mutable_flags = u32::from_le_bytes(row[73..77].try_into().unwrap());
        entry.committed |= self.committed;
        Ok(entry)
    }
}
impl ScanBuffer for BufferedScan {
    fn reset(&mut self) {
        *self = Self::new(self.spec);
    }
    fn reserve_entry(&mut self, spec: FormatSpec) -> Result<()> {
        spec.read_limits
            .check(ReadLimitKey::Records, self.rows.len() as u64 + 1)?;
        self.rows.reserve_one()?;
        Ok(())
    }
    fn push_entry(&mut self, entry: RecordIndexEntry) -> Result<()> {
        if self.sequences.is_none() && self.previous.is_some_and(|prev| prev >= entry.sequence) {
            let mut index = crate::disk_index::ScratchIndex::new(0)?;
            for old in self.entries() {
                Self::remember_sequence(&mut index, old?.sequence)?;
            }
            self.sequences = Some(index);
        }
        if let Some(index) = &mut self.sequences {
            Self::remember_sequence(index, entry.sequence)?;
        }
        self.previous = Some(entry.sequence);
        if entry.block_id == COMMIT_BLOCK_ID {
            self.marker_seen = true;
            self.kept = self.rows.len() + 1;
            self.suffix.clear();
        } else if self.marker_seen
            && self.kept == self.rows.len()
            && ((entry.block_id == SEGMENT_BLOCK_ID && self.suffix.is_empty())
                || (entry.block_id == OPEN_DIGEST_BLOCK_ID
                    && self
                        .suffix
                        .last()
                        .is_none_or(|e| e.block_id == SEGMENT_BLOCK_ID)))
        {
            self.kept += 1;
            self.suffix.push(entry.clone());
        }
        self.rows.push_reserved(Self::encode(&entry));
        Ok(())
    }
    fn finish(&mut self, spec: FormatSpec, scanned: &mut ScannedIndex) -> Result<()> {
        if spec.commit_policy.is_transaction_marker() {
            self.rows.truncate(self.kept);
            self.committed = true;
            // The scan already captured every prefix tail at the marker. Only
            // the optional two bookkeeping records can extend that boundary.
            scanned.commit_boundary_applied(&self.suffix)?;
        }
        self.sequences = None;
        Ok(())
    }
    fn checkpoint_limits(&self, spec: FormatSpec, prefix: &[u8]) -> Result<()> {
        check_index_checkpoint_limits_for(spec, prefix, false)
    }
    fn segments(
        &mut self,
        spec: FormatSpec,
        file: &mut File,
        start: u64,
        end: u64,
    ) -> Result<Option<ScannedIndex>> {
        // Preserve the existing fast route while its decoded array fits the
        // staging budget. A larger chain falls back to bounded framing.
        let mut bounded_spec = spec;
        // Internal buffer sizing must preserve the entrypoint's trusted token.
        // Public overlay builders intentionally clear that authorization.
        bounded_spec.read_limits.max_index_bytes =
            crate::ReadLimit::Finite(resident_index::buffer_bytes(spec) as u64);
        let mut entries = Vec::new();
        let Some(scanned) = entries.segments(bounded_spec, file, start, end)? else {
            return Ok(None);
        };
        for entry in entries {
            self.rows.push(Self::encode(&entry))?;
        }
        Ok(Some(scanned))
    }
}
