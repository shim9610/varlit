// Writer bookkeeping kept across appends: sequence state, the segment
// cursor, block and keyed tails, commit scopes and append snapshots.
// Included by `file.rs`, whose module it shares.

/// What one user record actually stores, and where those bytes live.
///
/// `bytes` is a [`Cow`] rather than a `Vec` because the four uncompressed
/// routes through [`prepare_user_record_payload`] — no compression declared for
/// the block (the default), a non-`Variable` block kind, a payload under
/// `min_uncompressed_len`, and `only_if_smaller` losing — store the caller's
/// bytes unchanged. Owning them meant one payload-sized allocation, memcpy and
/// free on every append of every default configuration, for a buffer every
/// caller immediately reborrows as `&payload.bytes` and drops. Only the arm
/// that actually produces new bytes (compression, and the envelope around it)
/// owns them.
#[derive(Clone, Debug)]
struct StoredPayload<'a> {
    flags: u16,
    uncompressed_len_hint: u32,
    bytes: Cow<'a, [u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SequenceState {
    Available(u64),
    Exhausted,
}

impl SequenceState {
    fn available(self) -> Result<u64> {
        match self {
            Self::Available(sequence) => Ok(sequence),
            Self::Exhausted => Err(Error::SequenceExhausted),
        }
    }

    fn after_publishing(sequence: u64) -> Self {
        match sequence.checked_add(1) {
            Some(next) => Self::Available(next),
            None => Self::Exhausted,
        }
    }
}

/// O(1) writer-side state for the internal segment chain.
///
/// A segment covers the records one commit point added, so writing one needs
/// two things: where in the resident index that run begins, and where it begins
/// on disk. Both are derivable by walking back to the newest segment record,
/// and neither is derived that way — the walk would be `Theta(g)` per commit
/// point and `Theta(N)` on a file that has records but no segment record yet,
/// which is the same shape  and  removed from the flush and
/// append paths.
///
/// Invariant: at all times this equals `SegmentCursor::from_index(&index)` for
/// the current resident index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SegmentCursor {
    /// Resident-index position just past the newest segment record: the first
    /// entry the next segment covers.
    next_position: usize,
    /// File offset just past the newest segment record, or `None` when the file
    /// holds no segment record and coverage therefore starts at the append log.
    covered_start: Option<u64>,
}

impl SegmentCursor {
    fn new_empty() -> Self {
        Self {
            next_position: 0,
            covered_start: None,
        }
    }

    /// Recovers the cursor with one reverse walk that stops at the newest
    /// segment record, so a reopen or a generation rebind pays the live tail
    /// once instead of every commit point paying it again.
    ///
    /// `physical_end` saturates rather than overflowing. The entries here come
    /// from a completed scan or a validated generation, so it cannot saturate
    /// in practice; if it ever did, the next segment would record a coverage
    /// start the following open cannot match, and that open falls back to the
    /// full scan rather than trusting the chain.
    /// The rule, kept as the specification `derive_index_state` must match.
    #[cfg(test)]
    fn from_index(entries: &[RecordIndexEntry]) -> Self {
        match entries
            .iter()
            .rposition(|entry| entry.block_id == SEGMENT_BLOCK_ID)
        {
            Some(position) => Self {
                next_position: position + 1,
                covered_start: Some(entries[position].physical_end()),
            },
            None => Self::new_empty(),
        }
    }

    /// Advances the cursor for the entry just pushed at `position`.
    ///
    /// `record_end` is the physical end the append path budgeted and checked
    /// before writing, so this stays infallible on the far side of the
    /// authoritative write (INVARIANT 3).
    /// The cursor of an open that resumed at an anchor: the scanned tail's
    /// own when a segment follows the anchor (its position shifted past the
    /// records before it), else the one the anchor's summary carried.
    fn rebased(tail: Self, base_records: usize, summary: Option<(u64, u64)>) -> Self {
        if tail.covered_start.is_some() {
            Self {
                next_position: tail.next_position + base_records,
                covered_start: tail.covered_start,
            }
        } else if let Some((position, covered)) = summary {
            Self {
                next_position: usize::try_from(position).unwrap_or(usize::MAX),
                covered_start: Some(covered),
            }
        } else {
            tail
        }
    }

    fn note_appended(&mut self, position: usize, block_id: u32, record_end: u64) {
        if block_id == SEGMENT_BLOCK_ID {
            self.next_position = position + 1;
            self.covered_start = Some(record_end);
        }
    }
}

/// O(1)-append / O(log B) resident block-offset tails.
///
/// The append path used to reverse-scan the resident index for the newest
/// record of the same block, costing `Theta(g)` per append (`g` = distance
/// back to that record) and `O(N*B)` overall, degenerating to `Theta(N^2)`
/// when every record carries a distinct block id. This mirrors exactly what
/// that scan recomputed - the record offset of the newest entry per block id -
/// and never reads the resident index on the append path.
///
/// `B` is the number of distinct block ids present, which is bounded by the
/// format's declared block set plus varlit's internal ids, so the sorted vector
/// stays tiny and stops allocating once every block id has appeared.
///
/// Invariant: at all times this equals `BlockTails::from_index(&index)` for
/// the current resident index. It is maintained at the single index append
/// site and rebuilt wherever the resident index is replaced or truncated
/// wholesale (open, recovery, generation rebinds, append rollback).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct BlockTails {
    /// Sorted by block id.
    tails: Vec<(u32, u64)>,
}

/// The open summary a native writer stores with an anchor its next record
/// reaches; see [`crate::anchor::AnchorSummary`].
struct WriterSummary<'a> {
    tails: &'a BlockTails,
    records_before: Option<usize>,
    sequence_high_water: Option<u64>,
    segment: &'a SegmentCursor,
}

impl crate::anchor::SummarySource for WriterSummary<'_> {
    fn write_summary(&self, out: &mut [u8]) -> bool {
        let Some(records) = self.records_before else {
            return false;
        };
        let segment = self
            .segment
            .covered_start
            .map(|covered| (self.segment.next_position as u64, covered));
        crate::anchor::encode_summary(
            out,
            records as u64,
            self.sequence_high_water,
            segment,
            self.tails.tails.iter().copied(),
        )
    }
}

/// The open summary a rewrite keeps for the file it is writing.
///
/// The same rule an open derives from the records themselves
/// ([`derive_index_state`] and the scan): every record counts, each block's
/// newest record is its tail, the highest sequence is the high-water mark, and
/// the newest segment record sets the cursor. A rewrite notes each record after
/// writing it, so at the start of the next record this is exactly "the file
/// before it" — what an anchor that record reaches has to describe.
struct RewriteSummary {
    tails: BlockTails,
    records: u64,
    sequence_high_water: Option<u64>,
    segment: Option<(u64, u64)>,
}

impl RewriteSummary {
    fn new() -> Self {
        Self {
            tails: BlockTails::new_empty(),
            records: 0,
            sequence_high_water: None,
            segment: None,
        }
    }

    fn note(&mut self, entry: &RecordIndexEntry) -> Result<()> {
        if self.tails.tails.len() == self.tails.tails.capacity()
            && self
                .tails
                .tails
                .binary_search_by_key(&entry.block_id, |(id, _)| *id)
                .is_err()
        {
            self.tails
                .tails
                .try_reserve(1)
                .map_err(|_| Error::AllocationFailed {
                    resource: "rewrite block tails",
                    requested: (self.tails.tails.len() as u64 + 1).saturating_mul(16),
                })?;
        }
        self.tails
            .note_appended(entry.block_id, entry.record_offset);
        self.records = self
            .records
            .checked_add(1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "rewrite record count",
            })?;
        self.sequence_high_water = Some(
            self.sequence_high_water
                .map_or(entry.sequence, |seen| seen.max(entry.sequence)),
        );
        if entry.block_id == SEGMENT_BLOCK_ID {
            self.segment = Some((self.records, entry.checked_physical_end()?));
        }
        Ok(())
    }
}

impl crate::anchor::SummarySource for RewriteSummary {
    fn write_summary(&self, out: &mut [u8]) -> bool {
        crate::anchor::encode_summary(
            out,
            self.records,
            self.sequence_high_water,
            self.segment,
            self.tails.tails.iter().copied(),
        )
    }
}

impl BlockTails {
    fn new_empty() -> Self {
        Self { tails: Vec::new() }
    }

    /// Recovers the tails with one forward pass over the resident index, so a
    /// reopen pays the pass exactly once instead of every append paying its own
    /// reverse scan.
    ///
    /// The pass keeps the newest offset per block id in a hash map and orders
    /// the distinct ids exactly once, so construction is `O(N + B log B)` time
    /// and `O(B)` memory. Replaying the pass through [`Self::note_appended`]
    /// instead - which is what this did originally - inserts every first-seen
    /// id into the sorted vector, and an index whose first appearances are in
    /// descending id order moves `0 + 1 + ... + (B - 1)` tuples. That made the
    /// real bound `O(N log B + B^2)`, not the `O(N log B)` the comment here
    /// used to claim. The append path keeps the sorted-vector
    /// insertion: it pays at most one insertion per distinct id over the whole
    /// life of the file and buys `O(log B)` lookups with no hashing on the hot
    /// path.
    /// The rule, kept as the specification `derive_index_state` must match.
    #[cfg(test)]
    fn from_index(index: &[RecordIndexEntry]) -> Self {
        note_block_tail_index_touches(index.len() as u64);
        let mut newest: HashMap<u32, u64> = HashMap::new();
        for entry in index {
            newest.insert(entry.block_id, entry.record_offset);
        }
        Self::from_newest(&newest).expect("test tail allocation")
    }

    /// Orders an already-collected newest-per-block map.
    ///
    /// The scan collects into a map rather than replaying `note_appended`, for
    /// the reason `from_index` does: replaying inserts every first-seen id into
    /// the sorted vector, and an index whose first appearances are in descending
    /// id order moves `0 + 1 + ... + (B - 1)` tuples.
    /// Adopts a table a digest already ordered.
    ///
    /// `decode_digest_payload` rejects a non-ascending id, so this cannot adopt
    /// an out-of-order table — which matters, because every lookup here is a
    /// binary search and an unsorted table would answer wrong rather than
    /// slowly.
    fn from_sorted(tails: Vec<(u32, u64)>) -> Self {
        debug_assert!(tails.windows(2).all(|pair| pair[0].0 < pair[1].0));
        Self { tails }
    }

    fn from_newest(newest: &HashMap<u32, u64>) -> Result<Self> {
        let mut tails = Vec::new();
        tails
            .try_reserve_exact(newest.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "block tails",
                requested: (newest.len() as u64).saturating_mul(16),
            })?;
        tails.extend(newest.iter().map(|(id, offset)| (*id, *offset)));
        tails.sort_unstable_by_key(|(block_id, _)| *block_id);
        Ok(Self { tails })
    }

    fn tail(&self, block_id: u32) -> Option<u64> {
        self.position(block_id)
            .map(|position| self.tails[position].1)
    }

    /// The table itself, sorted by block id, for the digest encoder.
    fn as_slice(&self) -> &[(u32, u64)] {
        &self.tails
    }

    fn position(&self, block_id: u32) -> Option<usize> {
        self.tails
            .binary_search_by_key(&block_id, |(id, _)| *id)
            .ok()
    }

    /// Puts a block's tail back where it was before an append that rolled
    /// back. `None` means the block had no record yet, so the entry is removed.
    fn restore(&mut self, block_id: u32, previous: Option<u64>) {
        match (self.position(block_id), previous) {
            (Some(position), Some(offset)) => self.tails[position].1 = offset,
            (Some(position), None) => {
                self.tails.remove(position);
            }
            (None, _) => {}
        }
    }

    /// Advances the tail for the entry just pushed. Updating a block id the
    /// table already holds writes one `u64` and allocates nothing; a block id
    /// is inserted at most once per distinct id in the file.
    fn note_appended(&mut self, block_id: u32, record_offset: u64) {
        match self.tails.binary_search_by_key(&block_id, |(id, _)| *id) {
            Ok(position) => self.tails[position].1 = record_offset,
            Err(position) => {
                // Every insertion shifts the tuples above it; counting them is
                // what makes the historical quadratic construction term
                // observable to a regression test.
                note_block_tail_entries_moved((self.tails.len() - position) as u64);
                self.tails.insert(position, (block_id, record_offset));
            }
        }
    }
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    static BLOCK_TAIL_INDEX_TOUCHES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Counts resident index entries examined by the block-offset-chain
/// predecessor machinery on this thread ( regression evidence). The
/// append path must never advance this counter. Inert without the
/// `scalable-fault-injection` feature.
#[inline]
fn note_block_tail_index_touches(count: u64) {
    #[cfg(feature = "scalable-fault-injection")]
    BLOCK_TAIL_INDEX_TOUCHES.with(|touches| touches.set(touches.get().saturating_add(count)));
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = count;
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    static BLOCK_TAIL_ENTRIES_MOVED: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Counts tuples displaced by sorted-vector insertion in the block-tail table
/// on this thread ( regression evidence). Index *visits* cannot see
/// this cost, which is why the quadratic construction term survived the
/// previous review. Inert without the `scalable-fault-injection` feature.
#[inline]
fn note_block_tail_entries_moved(count: u64) {
    #[cfg(feature = "scalable-fault-injection")]
    BLOCK_TAIL_ENTRIES_MOVED.with(|moved| moved.set(moved.get().saturating_add(count)));
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = count;
}

/// Lazily built resident keyed tails for the generic keyed append path
///
///
/// One tail map per keyed block id. The resident file is not generic over its
/// block types, so keys are held as their canonical internal key payload -
/// the same encoding tombstone records carry - rather than as `T::Key`. A map
/// is built on first use from [`VarlitFile::key_tail_offsets`] and then
/// maintained in O(1) per append, so the generic keyed entry points link
/// predecessors exactly like the generated keyed writer does. Any
/// caller-supplied predecessor (the generated writer's own path) invalidates
/// the cached map for that block id, because the file cannot learn which key
/// that record carried.
#[derive(Debug, Default)]
struct KeyedTails {
    tails: HashMap<u32, KeyedTailMap>,
}

/// One block id's tail map plus the running total of the key payload bytes it
/// owns.
///
/// The total is maintained incrementally - one addition on the append that
/// introduces a key, nothing at all when an existing key is overwritten - so
/// charging the cache against [`ReadLimits::max_keyed_tail_bytes`] stays O(1)
/// per append. Recomputing it by summing key lengths would be Theta(distinct
/// keys) on the append hot path and is exactly what this field exists to
/// avoid.
#[derive(Debug, Default)]
struct KeyedTailMap {
    offsets: HashMap<Vec<u8>, u64>,
    key_payload_bytes: u64,
    spilled: Option<crate::scratch_index::ScratchIndex>,
}

impl KeyedTailMap {
    fn get(&self, key: &[u8]) -> Result<Option<u64>> {
        if let Some(&offset) = self.offsets.get(key) {
            return Ok(Some(offset));
        }
        match &self.spilled {
            Some(index) => Ok(index
                .get(key)?
                .map(|row| u64::from_le_bytes(row[..8].try_into().unwrap()))),
            None => Ok(None),
        }
    }
    fn spill(&mut self) -> Result<()> {
        if self.offsets.is_empty() {
            return Ok(());
        }
        if self.spilled.is_none() {
            self.spilled = Some(crate::scratch_index::ScratchIndex::new(0)?);
        }
        let requested =
            allocation_bytes::<(Vec<u8>, [u8; 52])>(self.offsets.len(), "keyed tail spill batch")?;
        let mut updates = Vec::new();
        updates
            .try_reserve_exact(self.offsets.len())
            .map_err(|_| Error::AllocationFailed {
                resource: "keyed tail spill batch",
                requested,
            })?;
        for (key, &offset) in &self.offsets {
            let mut row = [0; 52];
            row[..8].copy_from_slice(&offset.to_le_bytes());
            updates.push((key.try_clone()?, row));
        }
        updates.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        self.spilled
            .as_mut()
            .unwrap()
            .apply_iter(updates.iter().map(|(key, row)| (key, row)))?;
        self.offsets.clear();
        self.key_payload_bytes = 0;
        Ok(())
    }
    fn reserve(&mut self, spec: FormatSpec, key: &[u8]) -> Result<()> {
        let limit = spec
            .read_limits
            .require(ReadLimitKey::KeyedTailBytes)?
            .unwrap_or(2 * 1024 * 1024)
            .min(2 * 1024 * 1024);
        if !self.offsets.contains_key(key) && self.charge_for(1, key.len() as u64)? > limit {
            self.spill()?;
        }
        // One bounded key is the minimum work unit even with a zero cache.
        self.offsets
            .try_reserve(1)
            .map_err(|_| Error::AllocationFailed {
                resource: "keyed tail offsets",
                requested: key.len() as u64,
            })?;
        Ok(())
    }
    fn insert(&mut self, key: Vec<u8>, offset: u64) {
        let bytes = key.len() as u64;
        if self.offsets.insert(key, offset).is_none() {
            self.key_payload_bytes += bytes;
        }
    }
    /// Bytes this map is accountable for: its inline storage plus the key
    /// payload heap it owns. Excludes `HashMap` control bytes and load-factor
    /// slack, which the charge deliberately does not claim to cover.
    fn charge_for(&self, additional_entries: usize, additional_key_bytes: u64) -> Result<u64> {
        let inline = allocation_bytes::<(Vec<u8>, u64)>(
            self.offsets.len().saturating_add(additional_entries),
            "keyed tail offsets",
        )?;
        inline
            .checked_add(self.key_payload_bytes)
            .and_then(|total| total.checked_add(additional_key_bytes))
            .ok_or(Error::AllocationFailed {
                resource: "keyed tail offsets",
                requested: u64::MAX,
            })
    }
}

impl KeyedTails {
    fn new_empty() -> Self {
        Self {
            tails: HashMap::new(),
        }
    }

    fn invalidate(&mut self, block_id: u32) {
        self.tails.remove(&block_id);
    }

    fn invalidate_all(&mut self) {
        self.tails.clear();
    }
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    /// Armed pre-append keyed-tail reservation failures (API3-01).
    static INJECTED_KEYED_TAIL_RESERVATION_FAILURES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
    /// Armed post-append keyed-tail reservation losses (API3-01).
    static INJECTED_KEYED_TAIL_COMMIT_LOSSES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
    /// Armed post-commit-marker durability failures (round 10, invariant 3).
    static INJECTED_COMMIT_DURABILITY_FAILURES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Consumes one armed keyed-tail reservation failure.
///
/// Inert without the `scalable-fault-injection` feature. Thread-local so
/// concurrently running tests cannot arm each other's writers.
#[inline]
fn take_injected_keyed_tail_reservation_failure(requested: u64) -> Result<()> {
    #[cfg(feature = "scalable-fault-injection")]
    {
        let armed = INJECTED_KEYED_TAIL_RESERVATION_FAILURES.with(|count| {
            let current = count.get();
            if current != 0 {
                count.set(current - 1);
            }
            current != 0
        });
        if armed {
            return Err(Error::AllocationFailed {
                resource: "keyed tail offsets",
                requested,
            });
        }
    }
    #[cfg(not(feature = "scalable-fault-injection"))]
    let _ = requested;
    Ok(())
}

/// Consumes one armed post-append keyed-tail reservation loss.
///
/// Models the otherwise unreachable state in which the slot reserved before
/// the append is no longer usable afterwards, so the infallible commit's
/// discard-the-cache fallback can be exercised deterministically. Inert
/// without the `scalable-fault-injection` feature.
#[inline]
fn take_injected_keyed_tail_commit_loss() -> bool {
    #[cfg(feature = "scalable-fault-injection")]
    {
        INJECTED_KEYED_TAIL_COMMIT_LOSSES.with(|count| {
            let current = count.get();
            if current != 0 {
                count.set(current - 1);
            }
            current != 0
        })
    }
    #[cfg(not(feature = "scalable-fault-injection"))]
    {
        false
    }
}

/// Consumes one armed post-commit-marker durability failure.
///
/// Models a `flush`/`fsync` that fails *after* the commit marker has been
/// appended, which a real filesystem will not produce on demand. Inert without
/// the `scalable-fault-injection` feature.
#[inline]
fn take_injected_commit_durability_failure() -> Result<()> {
    #[cfg(feature = "scalable-fault-injection")]
    {
        let armed = INJECTED_COMMIT_DURABILITY_FAILURES.with(|count| {
            let current = count.get();
            if current != 0 {
                count.set(current - 1);
            }
            current != 0
        });
        if armed {
            return Err(Error::Io(std::io::Error::other(
                "injected post-commit durability failure",
            )));
        }
    }
    Ok(())
}

/// The records a single-barrier commit marker names.
///
/// Counts and chains every record appended after the newest marker known to
/// be durable (the anchor) and that marker's segment/digest suffix — exactly
/// the run `scan::verify_single_barrier_commit` walks. The anchor moves only
/// when a `commit_durable` barrier completes, so a crash can tear nothing
/// before it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CommitScope {
    /// Whether this handle has established its anchor. A handle starts with
    /// one storage barrier so that whatever an earlier handle left without one
    /// is durable before anything names it as the anchor.
    initialized: bool,
    anchor: Option<u64>,
    count: u64,
    chain: u64,
}

impl CommitScope {
    pub(crate) const FRESH: Self = Self {
        initialized: false,
        anchor: None,
        count: 0,
        chain: 0,
    };

    fn note(&mut self, entry: &RecordIndexEntry) {
        self.count += 1;
        self.chain = scan::fold_commit_chain(
            self.chain,
            entry.record_offset,
            entry.sequence,
            entry.checksum,
        );
    }

    fn anchor_at(&mut self, marker_offset: Option<u64>) {
        *self = Self {
            initialized: true,
            anchor: marker_offset,
            count: 0,
            chain: 0,
        };
    }

    pub(crate) fn marker(&self) -> scan::CommitMarkerV2 {
        scan::CommitMarkerV2 {
            anchor: self.anchor,
            count: self.count,
            chain: self.chain,
        }
    }
}

struct AppendSnapshot {
    /// The end of file this append starts from, and the offset a rollback both
    /// truncates to and leaves the handle at.
    ///
    /// It used to carry a second number beside this one — the handle's actual
    /// offset, read with a per-record `stream_position` — so that a rollback
    /// could put the cursor back exactly where it found it. Reading it was an
    /// `lseek` per record; caching it instead was unsound, because the offset
    /// is shared with `VarlitFile::snapshot` (see `RecordFile`). Restoring `eof`
    /// costs neither: it is this field, and after any successful append the two
    /// numbers are the same one.
    eof: u64,
    sequence_state: SequenceState,
    index_len: usize,
    segment_cursor: SegmentCursor,
    /// The block this append is about to move the tail of, and where that tail
    /// pointed before. Two words, so the rollback needs no allocation.
    block_id: u32,
    previous_block_tail: Option<u64>,
    uncommitted_since_commit: bool,
    /// Whether any record has been appended since the last digest was written.
    ///
    /// `needs_open_digest` used to answer this with `last_resident_block_id`,
    /// which only moves for records the resident index carries. A workload
    /// flushing only non-resident records therefore left the flag reading
    /// "the digest is still last" while records piled up behind it — the
    /// digest went permanently stale and every `open_readonly_lazy` silently
    /// full-scanned. Non-resident blocks are exactly what the digest's tail
    /// table exists to reach, so that combination disabled the feature for its
    /// own purpose.
    appended_since_digest: bool,
    /// The records a single-barrier commit marker will name; see
    /// [`CommitScope`].
    commit_scope: CommitScope,
    /// The block id of the newest resident entry, maintained at the single
    /// index append site.
    ///
    /// Exists so that `needs_index_segment` and `needs_open_digest` can ask
    /// "is the last thing in this file already the record I am about to write"
    /// without faulting an entry off disk on every flush. A slot holds an
    /// offset and a committed bit, so the block id is not otherwise knowable
    /// without a read, and both predicates run on the append path.
    last_resident_block_id: Option<u32>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WriteFault {
    #[default]
    None,
    AppendAfterHeader,
    AppendAfterHeaderWithRollbackFailure,
    RollbackFailure,
    RebindAfterPublish,
    ///  double fault: the parent-directory sync fails *and* the rebind
    /// that follows the same publication fails.
    ///
    /// One arming produces both, because taking the parent-sync failure re-arms
    /// the injector as [`WriteFault::RebindAfterPublish`]. That ordering is the
    /// one the code under test sees: `replace_path_atomically` syncs the parent
    /// directory before it returns, and the writer rebinds afterwards.
    ParentSyncThenRebindAfterPublish,
}

#[cfg(test)]
std::thread_local! {
    static WRITE_FAULT: std::cell::Cell<WriteFault> = const {
        std::cell::Cell::new(WriteFault::None)
    };
}

#[cfg(test)]
fn inject_write_fault(fault: WriteFault) {
    WRITE_FAULT.set(fault);
}

#[cfg(test)]
fn fail_append_after_header_if_requested() -> std::io::Result<()> {
    WRITE_FAULT.with(|fault| match fault.get() {
        WriteFault::AppendAfterHeader => {
            fault.set(WriteFault::None);
            Err(std::io::Error::other("injected append write failure"))
        }
        WriteFault::AppendAfterHeaderWithRollbackFailure => {
            fault.set(WriteFault::RollbackFailure);
            Err(std::io::Error::other("injected append write failure"))
        }
        _ => Ok(()),
    })
}

#[cfg(test)]
fn fail_rollback_if_requested() -> std::io::Result<()> {
    WRITE_FAULT.with(|fault| {
        if fault.get() == WriteFault::RollbackFailure {
            fault.set(WriteFault::None);
            Err(std::io::Error::other("injected rollback failure"))
        } else {
            Ok(())
        }
    })
}

/// injects a parent-directory sync failure and arms the rebind failure
/// that must follow it in the same publication.
#[cfg(test)]
fn fail_parent_sync_if_requested() -> Result<()> {
    WRITE_FAULT.with(|fault| {
        if fault.get() == WriteFault::ParentSyncThenRebindAfterPublish {
            fault.set(WriteFault::RebindAfterPublish);
            Err(Error::Io(std::io::Error::other(
                "injected parent-directory sync failure",
            )))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
fn fail_rebind_after_publish_if_requested() -> std::io::Result<()> {
    WRITE_FAULT.with(|fault| {
        if fault.get() == WriteFault::RebindAfterPublish {
            fault.set(WriteFault::None);
            Err(std::io::Error::other(
                "injected post-publication rebind failure",
            ))
        } else {
            Ok(())
        }
    })
}
