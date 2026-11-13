//! Owner-local record directory. Spill I/O precedes record publication, and
//! rollback only changes the logical row count. No shared cache or admission.
use super::native_rewrite::RewriteRows;
use super::{
    Error, FormatSpec, RecordDirectory, RecordIndexEntry, Result, SnapshotFile, fault_record_entry,
};
use crate::ReadLimit;
use crate::spill_rows::SpillRows;

// Private rows need no alignment padding or persistent wire representation.
const RESIDENT_ROW_BYTES: usize = 8 + 4 + 1;
type ResidentRow = [u8; RESIDENT_ROW_BYTES];

/// The in-memory working set a handle's spillable rows may use. A lower
/// `IndexBytes` ceiling lowers it; a higher one does not raise it past the
/// fixed working set, so a larger ceiling (the untrusted preset sets 1 GiB)
/// is not turned into a larger resident buffer per handle.
pub(super) fn buffer_bytes(spec: FormatSpec) -> usize {
    const WORKING_SET: usize = 2 * 1024 * 1024;
    match spec.read_limits.resolve().max_index_bytes {
        ReadLimit::Finite(bytes) => usize::try_from(bytes)
            .unwrap_or(usize::MAX)
            .min(WORKING_SET),
        _ => WORKING_SET,
    }
}

// The boundary certificate shares the existing directory allowance. Half of
// this small share is write buffering and half covers a future read cache.
pub(super) fn boundary_budget(spec: FormatSpec) -> usize {
    if spec.block_residency.iter().any(|block| !block.resident) {
        (buffer_bytes(spec) / 2).min(16 * 1024) / 16 * 16
    } else {
        0
    }
}
pub(super) fn slot_budget(spec: FormatSpec) -> usize {
    buffer_bytes(spec).saturating_sub(boundary_budget(spec))
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
    slots: SpillRows<RESIDENT_ROW_BYTES>,
    rewritten: Option<RewrittenRows>,
    rewritten_len: usize,
    snapshot: SnapshotFile,
    spec: FormatSpec,
    frames_verified: bool,
    sequence_hints_allowed: bool,
    boundary_proof: Option<BoundaryProof>,
    boundary_cache_unavailable: bool,
    boundary_allowance: usize,
    offset_hint: Option<(u64, u64)>,
    last_probe: usize,
    range_hint: Option<(usize, u64, u64)>,
    uniform: Option<UniformPrefix>,
    first_sequence_hint: Option<u64>,
    sequence_hint_disabled: bool,
    /// Records installed since open on a handle that keeps no directory. Such
    /// a handle stores no row at all — not in memory and not in a spill file —
    /// so this count is everything it knows: what the `Records` ceiling and an
    /// append rollback need.
    counted: usize,
    /// Whether `counted` is the whole file's resident record count (a writer
    /// that created the file, opened it with a scan, or published a rewrite)
    /// rather than only the records since a lazy open.
    counts_whole_file: bool,
    retained: bool,
}
// Full rewrite rows are needed while constructing and validating a new
// generation. Once published, the resident directory uses only offset, block
// id and committed state. Compact that private metadata under the *remaining*
// allowance before releasing the original; every failed attempt keeps it.
#[derive(Debug)]
enum RewrittenRows {
    Full(RewriteRows),
    Compact(crate::packed_rows::PackedRows<RESIDENT_ROW_BYTES>),
}
impl RewrittenRows {
    fn new(rows: RewriteRows, spec: FormatSpec) -> Self {
        // Keep the conversion overlap small even when the caller allows a large
        // directory. Release the full rows before promoting the compact data.
        let scratch = (buffer_bytes(spec) - rewrite_buffer_bytes(spec)).min(64 * 1024);
        if scratch < crate::packed_rows::PackedRows::<RESIDENT_ROW_BYTES>::minimum_buffer_bytes() {
            return Self::Full(rows);
        }
        let compact = (|| -> std::io::Result<_> {
            // Preserve compression for directories that cannot stay resident.
            // Construction still uses its larger pages; both representations
            // are charged to the same existing directory allowance.
            let mut compact = crate::packed_rows::PackedRows::with_page_rows(scratch, 16);
            for at in 0..rows.len() {
                let source = rows.get(at)?.ok_or(std::io::ErrorKind::UnexpectedEof)?;
                compact.push(Self::project(&source))?;
            }
            Ok(compact)
        })();
        match compact {
            Ok(mut compact) => {
                drop(rows);
                // This is optional cache residency, not publication. A refused
                // allocation or read retains the complete compact spill data.
                let _ = compact.try_retain_for_reads(rewrite_buffer_bytes(spec));
                Self::Compact(compact)
            }
            Err(_) => Self::Full(rows),
        }
    }
    fn project(source: &[u8; 62]) -> ResidentRow {
        let mut row = [0; RESIDENT_ROW_BYTES];
        row[..8].copy_from_slice(&source[16..24]);
        row[8..12].copy_from_slice(&source[..4]);
        row[12] = source[56];
        row
    }
    fn get(&self, at: usize) -> std::io::Result<Option<ResidentRow>> {
        match self {
            Self::Full(rows) => Ok(rows.get(at)?.as_ref().map(Self::project)),
            Self::Compact(rows) => rows.get(at),
        }
    }
    fn contains(&self, offset: u64) -> std::io::Result<bool> {
        match self {
            Self::Full(rows) => Ok(rows
                .find_sorted(offset, |row| {
                    u64::from_le_bytes(row[16..24].try_into().unwrap())
                })?
                .is_some()),
            Self::Compact(rows) => Ok(rows
                .find_sorted(offset, |row| {
                    u64::from_le_bytes(row[..8].try_into().unwrap())
                })?
                .is_some()),
        }
    }
}

fn encode(entry: &RecordIndexEntry) -> ResidentRow {
    let mut row = [0; RESIDENT_ROW_BYTES];
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
        uniform: Option<UniformPrefix>,
        snapshot: SnapshotFile,
        spec: FormatSpec,
    ) -> Self {
        let remaining = buffer_bytes(spec).saturating_sub(rewrite_buffer_bytes(spec));
        let boundary_allowance = boundary_budget(spec).min(remaining / 2 / 16 * 16);
        Self {
            rewritten_len: rows.len(),
            rewritten: Some(RewrittenRows::new(rows, spec)),
            slots: SpillRows::new(remaining.saturating_sub(boundary_allowance)),
            snapshot,
            spec,
            retained: true,
            frames_verified: true,
            sequence_hints_allowed: !spec.block_residency.iter().any(|block| !block.resident),
            boundary_proof: None,
            boundary_cache_unavailable: false,
            boundary_allowance,
            offset_hint: None,
            last_probe: usize::MAX,
            range_hint: None,
            uniform,
            first_sequence_hint: None,
            sequence_hint_disabled: false,
            counted: 0,
            counts_whole_file: false,
        }
    }
    pub(crate) fn adopt_generation<D: RecordDirectory + ?Sized>(
        entries: &D,
        snapshot: SnapshotFile,
        spec: FormatSpec,
        frames_verified: bool,
        boundaries: Option<BoundaryProof>,
    ) -> Result<Self> {
        let mut result = Self::empty(snapshot, spec);
        result.frames_verified = frames_verified;
        result.boundary_cache_unavailable = frames_verified
            && spec.block_residency.iter().any(|block| !block.resident)
            && boundaries.is_none();
        result.boundary_proof = boundaries;
        result.slots.reserve_rows_hint(entries.record_count())?;
        for at in 0..entries.record_count() {
            let entry = entries.record_at(at)?;
            result.slots.push(encode(&entry))?;
            if frames_verified {
                result.note_verified_offset(at, entry.record_offset);
            }
        }
        Ok(result)
    }
    pub(crate) fn empty(snapshot: SnapshotFile, spec: FormatSpec) -> Self {
        Self {
            slots: SpillRows::new(slot_budget(spec)),
            rewritten: None,
            rewritten_len: 0,
            snapshot,
            spec,
            retained: true,
            frames_verified: true,
            sequence_hints_allowed: !spec.block_residency.iter().any(|block| !block.resident),
            boundary_proof: None,
            boundary_cache_unavailable: false,
            boundary_allowance: boundary_budget(spec),
            offset_hint: None,
            last_probe: usize::MAX,
            range_hint: None,
            uniform: None,
            first_sequence_hint: None,
            sequence_hint_disabled: false,
            counted: 0,
            counts_whole_file: false,
        }
    }
    pub(crate) fn none_retained(snapshot: SnapshotFile, spec: FormatSpec) -> Self {
        Self {
            retained: false,
            frames_verified: false,
            ..Self::empty(snapshot, spec)
        }
    }
    /// A writer that created its file: no record list, and every frame it
    /// will hold is one it wrote.
    /// A handle that keeps no list over a generation of `records` resident
    /// records it has just published.
    pub(crate) fn none_retained_counting(
        snapshot: SnapshotFile,
        spec: FormatSpec,
        records: usize,
    ) -> Self {
        let mut result = Self::none_retained_created(snapshot, spec);
        result.counted = records;
        result.counts_whole_file = true;
        result
    }

    pub(crate) fn none_retained_created(snapshot: SnapshotFile, spec: FormatSpec) -> Self {
        Self {
            retained: false,
            counts_whole_file: true,
            ..Self::empty(snapshot, spec)
        }
    }
    /// A writer opened over an existing file: no record list, but the
    /// boundary knowledge its open already established (bounded by its own
    /// allowance) is kept for in-place replacement checks.
    ///
    /// `records` is how many resident records the open found: positions the
    /// handle derived at open (its segment cursor) count from the start of
    /// the file, so the count has to as well.
    pub(crate) fn none_retained_opened(
        snapshot: SnapshotFile,
        spec: FormatSpec,
        records: usize,
        frames_verified: bool,
        boundaries: Option<BoundaryProof>,
    ) -> Self {
        let mut result = Self::none_retained(snapshot, spec);
        result.counted = records;
        result.counts_whole_file = true;
        result.frames_verified = frames_verified;
        result.boundary_cache_unavailable = frames_verified
            && spec.block_residency.iter().any(|block| !block.resident)
            && boundaries.is_none();
        result.boundary_proof = boundaries;
        result
    }
    /// The whole file's resident record count, when this handle knows it
    /// without a list or a walk.
    pub(crate) fn whole_file_count(&self) -> Option<usize> {
        if self.retained {
            Some(self.len())
        } else {
            self.counts_whole_file.then_some(self.counted)
        }
    }
    pub(crate) fn is_retained(&self) -> bool {
        self.retained
    }
    pub(crate) fn rebind(&mut self, snapshot: SnapshotFile) {
        if snapshot.len() < self.snapshot.len() {
            self.boundary_proof = None;
        }
        self.snapshot = snapshot;
    }
    pub(crate) fn reserve(
        &mut self,
        charge: impl FnOnce() -> Result<u64>,
    ) -> Result<ReservedIndexSlot> {
        charge()?;
        // A handle without a directory reserves no row. The boundary proof
        // below is kept either way: it is bounded by its own allowance and is
        // what lets a writer trust its own appends without a scan.
        if self.retained {
            self.slots.reserve_one()?;
        }
        if !self.boundary_cache_unavailable
            && self.boundary_proof.is_none()
            && self
                .spec
                .block_residency
                .iter()
                .any(|block| !block.resident)
        {
            self.boundary_proof = Some(BoundaryProof::new(
                self.snapshot.len(),
                self.boundary_allowance,
            ));
        }
        if let Some(proof) = &mut self.boundary_proof {
            // This certificate is an optional cache, not a condition for
            // logging. A refused cache allocation/spill must not refuse data.
            proof.rows.truncate(proof.verified_rows);
            if proof.rows.push(self.snapshot.len().to_le_bytes()).is_err() {
                self.boundary_proof = None;
                self.boundary_cache_unavailable = true;
            }
        }
        Ok(ReservedIndexSlot(()))
    }
    pub(crate) fn install_verified_boundary(&mut self, offset: u64, end: u64) {
        if let Some(proof) = &mut self.boundary_proof {
            proof.publish_reserved(offset, end);
        }
    }
    pub(crate) fn install(&mut self, slot: ReservedIndexSlot, entry: &RecordIndexEntry) {
        let ReservedIndexSlot(()) = slot;
        if self.retained {
            self.slots.push_reserved(encode(entry));
        } else {
            self.counted += 1;
        }
        self.note_verified_offset(self.len() - 1, entry.record_offset);
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
                let entry = entry?;
                if self.retained {
                    self.slots.push(encode(&entry))?;
                } else {
                    self.counted += 1;
                }
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
        if len < 2 {
            self.offset_hint = None;
        }
        self.last_probe = usize::MAX;
        self.range_hint = None;
        if let Some(run) = &mut self.uniform {
            run.truncate(len);
        }
        if len == 0 {
            self.first_sequence_hint = None;
            self.sequence_hint_disabled = false;
        }
        if !self.retained {
            self.counted = self.counted.min(len);
            return;
        }
        self.slots.truncate(len.saturating_sub(self.rewritten_len));
        self.rewritten_len = self.rewritten_len.min(len);
    }
    pub(crate) fn len(&self) -> usize {
        if !self.retained {
            return self.counted;
        }
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
    fn note_verified_offset(&mut self, row: usize, offset: u64) {
        UniformPrefix::observe(&mut self.uniform, row, offset);
        if let Some((_, first, _)) = self.range_hint {
            self.range_hint = Some((self.len(), first, offset));
        }
    }

    pub(crate) fn has_sequence_hint(&self) -> bool {
        self.frames_verified
            && self.sequence_hints_allowed
            && !self.sequence_hint_disabled
            && self.rewritten_len == 0
            && self.slots.len() != 0
    }

    /// An untrusted header sequence only selects a candidate private row. Its
    /// actual offset must match; arithmetic alone never authenticates a frame.
    #[inline]
    pub(crate) fn matches_sequence_hint(&mut self, offset: u64, sequence: u64) -> Result<bool> {
        if !self.has_sequence_hint() || offset >= self.snapshot.len() {
            return Ok(false);
        }
        let first_sequence = match self.first_sequence_hint {
            Some(sequence) => sequence,
            None => self.initialize_sequence_hint()?,
        };
        let Some(row) = sequence_candidate_row(sequence, first_sequence) else {
            return Ok(false);
        };
        if let Some(local) = row.checked_sub(self.rewritten_len)
            && let Some(row) = self.slots.buffered_row(local)
        {
            return Ok(u64::from_le_bytes(row[..8].try_into().unwrap()) == offset);
        }
        Ok(self
            .row_direct(row)?
            .is_some_and(|row| u64::from_le_bytes(row[..8].try_into().unwrap()) == offset))
    }

    // Normal logging and reader follow maintain no sequence-hint state. Read
    // one verified first frame only when a mark actually needs the hint.
    #[cold]
    #[inline(never)]
    fn initialize_sequence_hint(&mut self) -> Result<u64> {
        let sequence = self.entry_at(0)?.sequence;
        self.first_sequence_hint = Some(sequence);
        Ok(sequence)
    }

    pub(crate) fn disable_sequence_hint(&mut self) {
        self.sequence_hint_disabled = true;
    }

    pub(crate) fn has_uniform_boundary(&self, offset: u64) -> bool {
        offset < self.snapshot.len()
            && self
                .uniform
                .as_ref()
                .is_some_and(|run| run.contains(offset))
    }

    /// Check actual membership. A serialized row is never a boundary witness.
    /// The slow path retains every discovered boundary within a borrowed
    /// directory budget, so earlier/random requests never restart the scan.
    #[inline]
    pub(crate) fn has_verified_boundary(&mut self, offset: u64) -> Result<bool> {
        Ok(offset < self.snapshot.len()
            && (self
                .uniform
                .as_ref()
                .is_some_and(|run| run.contains(offset))
                || (self.retained
                    && self.frames_verified
                    && self.offset_floor(offset)? == Some(offset))))
    }

    /// Whether the cached proof already covers `offset` (no read needed).
    pub(crate) fn boundary_cached(&self, offset: u64) -> Result<bool> {
        match &self.boundary_proof {
            Some(proof) => proof.contains(offset),
            None => Ok(false),
        }
    }

    /// How far the cached proof's forward scan has reached, if it has started.
    pub(crate) fn boundary_frontier(&self) -> Option<u64> {
        self.boundary_proof.as_ref().map(|proof| proof.next)
    }

    pub(crate) fn verify_boundary(&mut self, append_start: u64, offset: u64) -> Result<()> {
        if offset < append_start || offset >= self.snapshot.len() {
            return Err(invalid_boundary());
        }
        if let Some(proof) = &self.boundary_proof
            && proof.contains(offset)?
        {
            return Ok(());
        }
        self.extend_boundary_proof(append_start, offset)?;
        self.boundary_cache_unavailable = false;
        Ok(())
    }

    #[inline(never)]
    fn extend_boundary_proof(&mut self, append_start: u64, offset: u64) -> Result<()> {
        if self.boundary_proof.is_none() {
            let budget = if self.boundary_allowance != 0
                || self
                    .spec
                    .block_residency
                    .iter()
                    .any(|block| !block.resident)
            {
                self.boundary_allowance
            } else {
                let loan = self.slots.lend_boundary_budget()?;
                self.boundary_allowance = loan;
                loan
            };
            self.boundary_proof = Some(BoundaryProof::new(append_start, budget));
        }
        let proof = self.boundary_proof.as_mut().unwrap();
        if offset < proof.first {
            // A lazy writer may have a verified appended suffix but no older
            // prefix. Discover the missing prefix once using the same storage.
            proof.rows.truncate(0);
            proof.verified_rows = 0;
            proof.prefix_rows = 0;
            proof.prefix_last = append_start;
            proof.first = append_start;
            proof.last = append_start;
            proof.next = append_start;
        }
        // Appended suffix certificates may follow an undiscovered old gap.
        // Only a miss needs the gap; keep the monotone scan frontier separate.
        if proof.verified_rows > proof.prefix_rows {
            proof.verified_rows = proof.prefix_rows;
            proof.last = proof.prefix_last;
        }
        proof.rows.truncate(proof.verified_rows);
        if proof.next <= offset {
            // Reserve once, before any read cache exists. Growing this buffer
            // beside a live cache must not create a transient budget overlap.
            proof.rows.reserve_rows_hint(proof.buffer_rows)?;
            let mut reader = FrameWindow::new(&self.snapshot);
            while proof.next <= offset {
                let at = proof.next;
                reader.frame_start = at;
                let entry = super::fault_record_entry_with::<32>(
                    self.snapshot.len(),
                    self.spec,
                    at,
                    false,
                    |position, bytes| reader.read(position, bytes),
                )?;
                super::validate_record_entry(&self.spec, &entry)?;
                let end = entry.checked_physical_end()?;
                if end <= at || end > self.snapshot.len() {
                    return Err(Error::CorruptTail { offset: at });
                }
                // Publish the frontier only after the fallible row write. A
                // failed allocation/spill leaves a reusable, exact prefix.
                proof.rows.push(at.to_le_bytes())?;
                proof.last = at;
                proof.next = end;
                proof.verified_rows += 1;
                proof.prefix_rows = proof.verified_rows;
                proof.prefix_last = at;
            }
        }
        if proof.contains(offset)? {
            Ok(())
        } else {
            Err(invalid_boundary())
        }
    }

    fn offset_floor(&mut self, offset: u64) -> Result<Option<u64>> {
        if let Some(row) = self.slots.cached_floor(offset, |row| {
            u64::from_le_bytes(row[..8].try_into().unwrap())
        }) {
            return Ok(Some(u64::from_le_bytes(row[..8].try_into().unwrap())));
        }
        // Predict a row from the first adjacent stride, but authorize only
        // exact equality with the private directory row. The hint is scalar,
        // not another cache, and arbitrary layouts still use binary search.
        let hint = match self.offset_hint {
            Some(hint) => Some(hint),
            None if self.len() >= 2 && self.uniform.as_ref().is_none_or(|run| !run.closed) => {
                let first = u64::from_le_bytes(
                    self.row_direct(0)?.ok_or(Error::UnexpectedEof)?[..8]
                        .try_into()
                        .unwrap(),
                );
                let second = u64::from_le_bytes(
                    self.row_direct(1)?.ok_or(Error::UnexpectedEof)?[..8]
                        .try_into()
                        .unwrap(),
                );
                let hint = second
                    .checked_sub(first)
                    .filter(|&step| step != 0)
                    .map(|step| (first, step));
                self.offset_hint = hint;
                hint
            }
            _ => None,
        };
        if self.uniform.as_ref().is_none_or(|run| !run.closed)
            && let Some((first, step)) = hint
            && offset >= first
            && let Ok(guess) = usize::try_from((offset - first) / step)
            && guess < self.len()
        {
            // Sequential requests benefit from the existing page cache.
            // Random requests read a single row instead of 64 KiB per probe.
            let previous = std::mem::replace(&mut self.last_probe, guess);
            let row = if previous.abs_diff(guess) <= 256 {
                self.row(guess)?
            } else {
                self.row_direct(guess)?
            };
            if let Some(row) = row
                && u64::from_le_bytes(row[..8].try_into().unwrap()) == offset
            {
                return Ok(Some(offset));
            }
        }
        // Irregular records often have a stable average spacing. Check a
        // small existing cache window around that second hint before falling
        // back to single-row binary probes. Hints never authorize membership.
        let range = match self.range_hint {
            Some(range) if range.0 == self.len() => Some(range),
            _ if self.len() > 1 => {
                let first = u64::from_le_bytes(
                    self.row_direct(0)?.ok_or(Error::UnexpectedEof)?[..8]
                        .try_into()
                        .unwrap(),
                );
                let last = u64::from_le_bytes(
                    self.row_direct(self.len() - 1)?
                        .ok_or(Error::UnexpectedEof)?[..8]
                        .try_into()
                        .unwrap(),
                );
                let range = Some((self.len(), first, last));
                self.range_hint = range;
                range
            }
            _ => None,
        };
        if let Some((len, first, last)) = range
            && first < last
            && offset >= first
            && offset <= last
        {
            let guess =
                crate::spill_rows::scaled_position(offset - first, len - 1, last - first) as usize;
            if guess < self.rewritten_len {
                let rows = self.rewritten.as_ref().unwrap();
                if let Some(row) = rows.get(guess)?
                    && u64::from_le_bytes(row[..8].try_into().unwrap()) == offset
                {
                    // Reuse the actual key just read while priming the page.
                    // The estimate itself never proves membership.
                    return Ok(Some(offset));
                }
                if rows.contains(offset)? {
                    return Ok(Some(offset));
                }
            } else {
                let candidate = self
                    .slots
                    .get_window(guess - self.rewritten_len, 1024)?
                    .ok_or(Error::UnexpectedEof)?;
                if let Some(row) = self.slots.cached_floor(offset, |row| {
                    u64::from_le_bytes(row[..8].try_into().unwrap())
                }) {
                    return Ok(Some(u64::from_le_bytes(row[..8].try_into().unwrap())));
                }
                // Correct the estimate using the observed row and the global
                // average spacing. This handles locally uneven record sizes
                // without adding probes or memory. Exact membership still
                // comes from a private row, with bounded binary fallback.
                let at = u64::from_le_bytes(candidate[..8].try_into().unwrap());
                let distance =
                    (crate::spill_rows::scaled_position(offset.abs_diff(at), len - 1, last - first)
                        as usize)
                        .max(1);
                let nearby = if offset < at {
                    guess.saturating_sub(distance)
                } else {
                    guess.saturating_add(distance).min(len - 1)
                };
                if nearby >= self.rewritten_len {
                    self.slots.get_window(nearby - self.rewritten_len, 1024)?;
                    if let Some(row) = self.slots.cached_floor(offset, |row| {
                        u64::from_le_bytes(row[..8].try_into().unwrap())
                    }) {
                        return Ok(Some(u64::from_le_bytes(row[..8].try_into().unwrap())));
                    }
                }
            }
        }
        let (mut low, mut high) = (0, self.len());
        let mut found = None;
        while low < high {
            let middle = low + (high - low) / 2;
            let row = self.row_direct(middle)?.ok_or(Error::UnexpectedEof)?;
            let at = u64::from_le_bytes(row[..8].try_into().unwrap());
            if at == offset {
                return Ok(Some(at));
            }
            if at < offset {
                found = Some(at);
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(found)
    }
    fn row_direct(&self, position: usize) -> std::io::Result<Option<ResidentRow>> {
        if position >= self.rewritten_len {
            self.slots.get_direct(position - self.rewritten_len)
        } else {
            self.row(position)
        }
    }
    fn row(&self, position: usize) -> std::io::Result<Option<ResidentRow>> {
        if !self.retained {
            return Ok(None);
        }
        if position >= self.rewritten_len {
            return self.slots.get(position - self.rewritten_len);
        }
        self.rewritten.as_ref().unwrap().get(position)
    }
    pub(crate) fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = Result<RecordIndexEntry>> + ExactSizeIterator + '_ {
        (0..self.len()).map(|at| self.entry_at(at))
    }
}

// A constant-size exact proof for an observed arithmetic prefix. Every row
// extending it was individually established by framing or this writer's append;
// unlike offset_hint, this may authorize membership without reading a row.
#[derive(Debug)]
pub(super) struct UniformPrefix {
    first_row: usize,
    count: usize,
    start: u64,
    step: u64,
    last: u64,
    closed: bool,
}

#[cfg(kani)]
#[path = "verification/uniform_prefix.rs"]
mod uniform_prefix_proofs;
impl UniformPrefix {
    pub(super) fn observe(proof: &mut Option<Self>, row: usize, offset: u64) {
        match proof {
            Some(run) => run.push(row, offset),
            None => {
                *proof = Some(Self {
                    first_row: row,
                    count: 1,
                    start: offset,
                    step: 0,
                    last: offset,
                    closed: false,
                });
            }
        }
    }
    fn push(&mut self, row: usize, offset: u64) {
        if self.closed {
            return;
        }
        if self.count == 0 {
            *self = Self {
                first_row: row,
                count: 1,
                start: offset,
                step: 0,
                last: offset,
                closed: false,
            };
            return;
        }
        if row != self.first_row + self.count || offset <= self.last {
            self.closed = true;
            return;
        }
        if self.count == 1 {
            self.step = offset - self.start;
        }
        if self.last.checked_add(self.step) != Some(offset) {
            self.closed = true;
            return;
        }
        self.count += 1;
        self.last = offset;
    }
    fn contains(&self, offset: u64) -> bool {
        self.count != 0
            && offset >= self.start
            && offset <= self.last
            && (offset == self.start
                || (self.step != 0 && (offset - self.start).is_multiple_of(self.step)))
    }
    fn truncate(&mut self, len: usize) {
        let count = self.count.min(len.saturating_sub(self.first_row));
        if count < self.count || len == self.first_row + self.count {
            self.count = count;
            self.last = self.start + self.step * count.saturating_sub(1) as u64;
            self.closed = false;
        }
    }
}

// A candidate only: sequence gaps can make this wrong. Authorization always
// compares the actual private row offset, then falls back to boundary framing.
fn sequence_candidate_row(sequence: u64, first_sequence: u64) -> Option<usize> {
    usize::try_from(sequence.checked_sub(first_sequence)?).ok()
}

fn invalid_boundary() -> Error {
    Error::InvalidCanonicalEncoding("offset is not a native record boundary")
}

#[derive(Debug)]
pub(crate) struct BoundaryProof {
    rows: SpillRows<8>,
    next: u64,
    first: u64,
    last: u64,
    buffer_rows: usize,
    verified_rows: usize,
    prefix_rows: usize,
    prefix_last: u64,
}
impl BoundaryProof {
    pub(super) fn new(from: u64, budget: usize) -> Self {
        Self {
            rows: if budget == 0 {
                SpillRows::unbuffered()
            } else {
                SpillRows::new(budget / 2)
            },
            next: from,
            first: from,
            last: from,
            buffer_rows: budget / 2 / 8,
            verified_rows: 0,
            prefix_rows: 0,
            prefix_last: from,
        }
    }
    fn publish_reserved(&mut self, offset: u64, end: u64) {
        debug_assert_eq!(self.rows.len(), self.verified_rows + 1);
        self.last = offset;
        self.verified_rows += 1;
        if self.next == offset {
            self.next = end;
            self.prefix_rows = self.verified_rows;
            self.prefix_last = offset;
        }
    }
    pub(super) fn start(&self) -> u64 {
        self.first
    }
    pub(super) fn note(&mut self, offset: u64, end: u64) -> Result<()> {
        self.rows.push(offset.to_le_bytes())?;
        self.last = offset;
        self.next = end;
        self.verified_rows += 1;
        self.prefix_rows = self.verified_rows;
        self.prefix_last = offset;
        Ok(())
    }
    pub(super) fn truncate_to(&mut self, end: u64) -> Result<()> {
        let (mut low, mut high) = (0, self.verified_rows);
        while low < high {
            let middle = low + (high - low) / 2;
            let at = u64::from_le_bytes(self.rows.get_direct(middle)?.ok_or(Error::UnexpectedEof)?);
            if at < end {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        self.rows.truncate(low);
        self.verified_rows = low;
        self.next = end;
        self.prefix_rows = low;
        self.last = if low == 0 {
            self.first
        } else {
            u64::from_le_bytes(self.rows.get_direct(low - 1)?.ok_or(Error::UnexpectedEof)?)
        };
        self.prefix_last = self.last;
        Ok(())
    }

    fn contains(&self, offset: u64) -> Result<bool> {
        if offset < self.first || offset > self.last || self.verified_rows == 0 {
            return Ok(false);
        }
        if offset == self.first || offset == self.last {
            return Ok(true);
        }
        if let Some(row) = self
            .rows
            .cached_floor(offset, |row| u64::from_le_bytes(row.try_into().unwrap()))
        {
            return Ok(u64::from_le_bytes(row) == offset);
        }
        // A verified interpolation candidate usually lands in the right page
        // for fixed/locally similar records. It is only a hint: irregular
        // layouts fall back to logarithmic, single-row probes, never a walk.
        let guess = ((offset - self.first) as u128 * (self.rows.len() - 1) as u128
            / (self.last - self.first) as u128) as usize;
        if let Some(row) = self.rows.get_direct(guess)?
            && u64::from_le_bytes(row) == offset
        {
            return Ok(true);
        }
        if let Some(row) = self
            .rows
            .cached_floor(offset, |row| u64::from_le_bytes(row.try_into().unwrap()))
        {
            return Ok(u64::from_le_bytes(row) == offset);
        }
        self.rows.get(guess)?;
        if let Some(row) = self
            .rows
            .cached_floor(offset, |row| u64::from_le_bytes(row.try_into().unwrap()))
        {
            return Ok(u64::from_le_bytes(row) == offset);
        }
        let (mut low, mut high) = (0, self.rows.len());
        while low < high {
            let middle = low + (high - low) / 2;
            let at = u64::from_le_bytes(self.rows.get_direct(middle)?.ok_or(Error::UnexpectedEof)?);
            match at.cmp(&offset) {
                std::cmp::Ordering::Equal => return Ok(true),
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
            }
        }
        Ok(false)
    }
}

// A small stack window amortizes positional reads for adjacent native frames.
// Only this slow scan owns it, never the steady-state mutation or read path.
struct FrameWindow<'a> {
    snapshot: &'a SnapshotFile,
    bytes: [u8; 4096],
    start: u64,
    len: usize,
    frame_start: u64,
}
impl<'a> FrameWindow<'a> {
    fn new(snapshot: &'a SnapshotFile) -> Self {
        Self {
            snapshot,
            bytes: [0; 4096],
            start: 0,
            len: 0,
            frame_start: 0,
        }
    }
    fn read(&mut self, offset: u64, into: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(into.len() as u64)
            .ok_or(Error::LengthOverflow { value: offset })?;
        if offset < self.start || end > self.start + self.len as u64 {
            // Read ahead only at the footer of a small frame. Large payloads
            // still cost header/footer bytes, not 4 KiB of payload per record.
            let window = if offset > self.frame_start && offset - self.frame_start <= 512 {
                self.bytes.len()
            } else {
                into.len()
            };
            self.len = self
                .snapshot
                .len()
                .saturating_sub(offset)
                .min(window as u64) as usize;
            if self.len < into.len() {
                return Err(Error::UnexpectedEof);
            }
            self.snapshot
                .read_exact_at(offset, &mut self.bytes[..self.len])?;
            self.start = offset;
        }
        let at = (offset - self.start) as usize;
        into.copy_from_slice(&self.bytes[at..at + into.len()]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boundary_certificates_do_not_publish_reservations_and_truncate_exactly() -> Result<()> {
        let mut proof = BoundaryProof::new(100, 2048);
        for at in [100u64, 180, 260] {
            proof.note(at, at + 80)?;
        }
        proof.rows.push(340u64.to_le_bytes())?;
        for at in 0..500 {
            assert_eq!(proof.contains(at)?, [100, 180, 260].contains(&at));
        }
        proof.publish_reserved(340, 420);
        for at in 0..500 {
            assert_eq!(proof.contains(at)?, [100, 180, 260, 340].contains(&at));
        }
        proof.truncate_to(260)?;
        for at in 0..500 {
            assert_eq!(proof.contains(at)?, [100, 180].contains(&at));
        }
        assert_eq!((proof.next, proof.prefix_rows), (260, 2));
        Ok(())
    }
    #[test]
    fn appended_suffix_does_not_claim_an_unscanned_gap() -> Result<()> {
        let mut proof = BoundaryProof::new(100, 2048);
        proof.note(100, 180)?;
        proof.rows.push(500u64.to_le_bytes())?;
        proof.publish_reserved(500, 580);
        for at in 0..600 {
            assert_eq!(proof.contains(at)?, [100, 500].contains(&at));
        }
        assert_eq!(
            (proof.next, proof.prefix_rows, proof.verified_rows),
            (180, 1, 2)
        );
        Ok(())
    }
    #[test]
    fn resident_rows_keep_the_boundary_working_set_within_the_same_allowance() -> Result<()> {
        // A scaled version of 131,072 data rows plus final internal rows at
        // the ordinary 2-MiB boundary. Padding must not evict this working set.
        let spec = FormatSpec::builder()
            .magic(b"VROWWORK")
            .build()?
            .with_resource_limits(crate::ReadLimits::STANDARD.with_max_index_bytes(2048));
        let snapshot = SnapshotFile::new(tempfile::tempfile()?)?;
        let mut index = ResidentIndex::empty(snapshot, spec);
        let mut entry = super::super::read_index_entry_payload(&[0; 73], 3);
        let mut expected = Vec::new();
        for i in 0..130u64 {
            entry.record_offset = 100 + i * 80 + i % 3;
            entry.block_id = if i < 128 {
                7
            } else {
                super::super::SEGMENT_BLOCK_ID
            };
            entry.committed = i % 2 == 0;
            expected.push(encode(&entry));
            let slot = index.reserve(|| Ok(0))?;
            index.install(slot, &entry);
        }
        for row in expected {
            let offset = u64::from_le_bytes(row[..8].try_into().unwrap());
            // No I/O populates the read cache first: all rows must still be
            // present in the original bounded buffer, including their fields.
            assert_eq!(
                index.slots.cached_floor(offset, |row| {
                    u64::from_le_bytes(row[..8].try_into().unwrap())
                }),
                Some(row)
            );
        }
        Ok(())
    }

    #[test]
    fn compact_and_fallback_rewrite_rows_preserve_exact_membership_and_fields() -> Result<()> {
        for budget in [256, 65536] {
            let spec = FormatSpec::builder()
                .magic(b"VCMPTROW")
                .build()?
                .with_resource_limits(crate::ReadLimits::STANDARD.with_max_index_bytes(budget));
            let mut rows = RewriteRows::with_page_rows(rewrite_buffer_bytes(spec), 16);
            let mut expected = Vec::new();
            let mut offset = 100u64;
            for i in 0..128u64 {
                offset += 64 + i * 37 % 271;
                let mut row = [0; 62];
                row[..4].copy_from_slice(&(i as u32 % 3 + 1).to_le_bytes());
                row[16..24].copy_from_slice(&offset.to_le_bytes());
                row[56] = (i % 2) as u8;
                expected.push(RewrittenRows::project(&row));
                rows.push(row)?;
            }
            let rows = RewrittenRows::new(rows, spec);
            assert_eq!(matches!(rows, RewrittenRows::Compact(_)), budget == 65536);
            for (i, expected) in expected.iter().enumerate().rev() {
                let offset = u64::from_le_bytes(expected[..8].try_into().unwrap());
                assert_eq!(rows.get(i)?, Some(*expected));
                assert!(rows.contains(offset)?);
                assert!(!rows.contains(offset + 1)?);
                assert!(!rows.contains(offset - 1)?);
            }
            assert_eq!(rows.get(128)?, None);
        }
        Ok(())
    }

    #[test]
    fn rewritten_membership_hints_preserve_gaps_and_truncated_prefixes() -> Result<()> {
        for budget in [256, 65536] {
            let spec = FormatSpec::builder()
                .magic(b"VRWHINTS")
                .build()?
                .with_resource_limits(crate::ReadLimits::STANDARD.with_max_index_bytes(budget));
            let mut rows = RewriteRows::new(rewrite_buffer_bytes(spec));
            let mut offsets = Vec::new();
            let mut offset = 100u64;
            for i in 0..129u64 {
                offset += 64 + i * 37 % 271 + if i == 64 { 4096 } else { 0 };
                let mut row = [0; 62];
                row[..4].copy_from_slice(&7u32.to_le_bytes());
                row[16..24].copy_from_slice(&offset.to_le_bytes());
                rows.push(row)?;
                offsets.push(offset);
            }
            let file = tempfile::tempfile()?;
            file.set_len(offset + 64)?;
            let snapshot = SnapshotFile::new(file)?;
            let mut index = ResidentIndex::from_rewrite_rows(rows, None, snapshot, spec);
            for retained in [129, 65, 17, 1, 0] {
                index.truncate(retained);
                for probe in 0..129 {
                    let at = offsets[probe * 73 % 129];
                    for wanted in [at - 1, at, at + 1] {
                        assert_eq!(
                            index.has_verified_boundary(wanted)?,
                            offsets[..retained].binary_search(&wanted).is_ok(),
                            "budget={budget}, retained={retained}, wanted={wanted}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn uniform_prefix_never_authorizes_unobserved_or_truncated_offsets() {
        for break_at in 1..24 {
            let mut run = UniformPrefix {
                first_row: 7,
                count: 1,
                start: 100,
                step: 0,
                last: 100,
                closed: false,
            };
            let mut observed = vec![100u64];
            for n in 1..32 {
                let step = if n < break_at { 72 } else { 64 + n as u64 % 7 };
                let offset = observed.last().unwrap() + step;
                observed.push(offset);
                run.push(7 + n, offset);
                for probe in 0..=offset + 1 {
                    if run.contains(probe) {
                        assert!(observed.contains(&probe));
                    }
                }
            }
            for retained in (0..=32).rev() {
                run.truncate(7 + retained);
                for probe in 0..2400 {
                    if run.contains(probe) {
                        assert!(observed[..retained].contains(&probe));
                    }
                }
            }
            run.push(7, 8192);
            run.push(8, 9000);
            assert!(run.contains(8192) && run.contains(9000));
            assert!(!run.contains(8500));
        }
    }

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
        let mut index = ResidentIndex::from_rewrite_rows(packed, None, snapshot, spec);
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
