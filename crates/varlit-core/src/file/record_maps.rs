// Record directories and the reads over them: `RecordDirectory`, `RecordMap`,
// walked record lists, read-ahead, pages and `RecordIndexEntry`.
// Included by `file.rs`, whose module it shares.

/// Summary source used by a lazy open, or the scan taken by an explicit
/// `*_lazy_or_scan_with_report` call. Strict lazy opens never report `FullScan`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LazyOpenSource {
    /// The tail table in the *file header* was read and the file corroborated
    /// it. A handful of records were framed — the commit marker it names, what
    /// follows that marker, and one per block tail — and no more.
    ///
    /// The route [`IndexPolicy::header_tails`] exists for: unlike the digest,
    /// nothing appended to the file can move this table or bury it.
    HeaderTails,
    /// The trailing digest record was framed; no user records were scanned.
    Digest,
    /// Explicit automatic-fallback mode reconstructed state by scanning.
    FullScan,
}

/// Whether an open keeps a record directory of its own.
///
/// Not a policy on the format: two callers of the same file want different
/// answers, and the one that wants none has to take the directory in the same
/// call or it could not read positionally at all. See
/// [`VarlitFile::open_readonly_without_directory`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectoryRetention {
    Keep,
    None,
}

/// Where a read resolves a record *position* to a record.
///
/// Every position-based read — `blocks`, `scan`, `keyed_blocks`, `metadata`,
/// `verify_all` and the rest — needs exactly two things from a directory: how
/// many records there are, and the entry at position `i`. Nothing else. Making
/// that a parameter rather than a field is what lets the directory stop being
/// the handle's business without any of those reads stopping working.
///
/// Two implementations, and they are the two answers to "who holds it":
///
/// * `ResidentIndex` — varlit holds it, and `record_at` faults the entry off
///   disk from a 16-byte slot.
/// * `[RecordIndexEntry]` — the host holds it, as
///   [`VarlitFile::index_entries_into`] or
///   [`VarlitFile::open_readonly_with_scratch`] left it. `record_at` is a
///   memcpy.
///
/// The second is the point, and it is stated plainly because the alternative
/// was nearly shipped: a handle that keeps no directory must not *lose* these
/// reads, it must *demand the directory from whoever has it*. That is the
/// difference between a capability with a cost and a capability withdrawn.
pub trait RecordDirectory {
    /// How many records this directory describes.
    fn record_count(&self) -> usize;

    /// The entry at `position`, or `UnexpectedEof` if there is none.
    fn record_at(&self, position: usize) -> Result<RecordIndexEntry>;

    /// The block id at `position`, when it can be told without building the
    /// entry. `None` means "ask `record_at`" and is always a correct answer.
    ///
    /// This exists for walks that want one block out of a directory. Producing
    /// an entry can read the file — the resident index rebuilds each one from
    /// its record header — so a walk that filters *after* `record_at` pays for
    /// every record in the file to keep the few that match. Directories that
    /// hold whole entries answer from those; the resident index answers from
    /// the field its slot carries; the default answers `None`, which costs a
    /// filtering caller nothing beyond what it already did.
    fn block_id_at(&self, _position: usize) -> Option<u32> {
        None
    }
}

/// Every entry of `dir` in order, fallible per item for the same reason
/// [`Kept::iter`](resident_index::Kept::iter) is: producing one may read a file.
///
/// A free function rather than a trait method with a default body, because a
/// `-> impl Iterator` method forces `Self: Sized` and the most useful directory
/// of all is `&[RecordIndexEntry]`, which is not.
pub fn directory_records<D: RecordDirectory + ?Sized>(
    dir: &D,
) -> impl Iterator<Item = Result<RecordIndexEntry>> + '_ {
    (0..dir.record_count()).map(|position| dir.record_at(position))
}

/// Whether the record at `position` can be passed over without building its
/// entry, given the block ids the caller is looking for.
///
/// Every walk that wants one block used to filter *after* `record_at`, which
/// was free when the resident index held whole entries and stopped being free
/// when it became one offset and one block id per record: producing an entry
/// is now a positional read of that record's header. A walk over a 600,000
/// record file that keeps five of them was reading 600,000 headers.
///
/// `false` when the directory cannot say (`block_id_at` returned `None`) —
/// the caller then does exactly what it did before, and its own check after
/// `record_at` is still the one that decides. This is an optimisation with no
/// authority: it never admits a record the caller's own filter would reject.
fn directory_skips<D: RecordDirectory + ?Sized>(dir: &D, position: usize, wanted: &[u32]) -> bool {
    match dir.block_id_at(position) {
        Some(id) => !wanted.contains(&id),
        None => false,
    }
}

impl RecordDirectory for resident_index::Kept<'_> {
    fn record_count(&self) -> usize {
        self.len()
    }

    fn record_at(&self, position: usize) -> Result<RecordIndexEntry> {
        self.entry_at(position)
    }

    fn block_id_at(&self, position: usize) -> Option<u32> {
        resident_index::Kept::block_id_at(*self, position)
    }
}

impl RecordDirectory for [RecordIndexEntry] {
    fn record_count(&self) -> usize {
        self.len()
    }

    fn record_at(&self, position: usize) -> Result<RecordIndexEntry> {
        self.get(position).cloned().ok_or(Error::UnexpectedEof)
    }

    fn block_id_at(&self, position: usize) -> Option<u32> {
        self.get(position).map(|entry| entry.block_id)
    }
}

impl RecordDirectory for Vec<RecordIndexEntry> {
    fn record_count(&self) -> usize {
        self.len()
    }

    fn record_at(&self, position: usize) -> Result<RecordIndexEntry> {
        self.as_slice().record_at(position)
    }
}

/// A record index the caller owns the memory for, walked only as far as the
/// caller's question needed.
///
/// [`VarlitFile::index_entries_into`] answers one question — "give me the whole
/// index" — and it answers it the same way whatever was actually being looked
/// for: `N` entries and two positional reads per record, to the end of the
/// file. This answers "walk until you find it, then stop". A map that finds its
/// record at position 12 of a million has read twelve records.
///
/// Three things make it the caller's rather than varlit's:
///
/// * **The buffer comes in.** `record_map` takes `&mut Vec<RecordIndexEntry>`
///   and never allocates one. Nothing is copied out of it either — the map
///   *is* the buffer, viewed.
/// * **The walk resumes.** A second question continues from where the first
///   stopped, so asking twice costs the records between the two answers, not
///   two walks. [`find`](Self::find) searches what is already framed before it
///   reads anything at all.
/// * **The caller decides when it goes away.** [`clear`](Self::clear) empties
///   it and rewinds; [`release`](Self::release) empties it and keeps walking
///   forward, which is how a bounded-memory pass over a file larger than RAM is
///   written. Dropping the map hands the buffer straight back.
///
/// It implements [`RecordDirectory`], so a map is a directory: everything
/// [`VarlitFile::with_directory`] serves — `blocks`, `scan`, `keyed_blocks`, the
/// rest — is answered against the prefix walked so far. That is what the trait
/// being the currency, rather than `Vec`, buys.
///
/// ```ignore
/// let mut buffer = Vec::new();
/// let mut map = file.record_map(&mut buffer)?;         // no read yet
/// let hit = map.find(|entry| entry.block_id == Note::ID)?;
/// if let Some(entry) = hit {
///     let note: Note = file.decode_block_into(&entry, &mut payload)?;
/// }
/// let points = file.with_directory(&map)?.block_entries_into::<Note>(&mut out)?;
/// map.clear();                                         // the caller's call
/// ```
///
/// **What it walks, and what it therefore cannot see.** Records in file order,
/// from the start of the append log to the handle's snapshot end. That end is
/// already the committed, complete one — the open scan established it — so a
/// torn trailing record, and under a transaction-marker policy an uncommitted
/// tail, are outside the walk rather than filtered out of it. Non-resident
/// blocks are framed and skipped, exactly as the open scan skips them, so a
/// completed map equals [`VarlitFile::index_entries`] entry for entry.
///
/// **What a partial index does not check.** A full open validates sequence
/// uniqueness across every record in the file. A map validates nothing beyond
/// the prefix it has read, because that is all it has read: a duplicate
/// sequence past the stopping point is not detected until a walk reaches it.
/// That is the cost of not reading the rest, and it is the only correctness
/// difference between a completed map and an open scan.
pub struct RecordMap<'a> {
    entries: &'a mut Vec<RecordIndexEntry>,
    spec: FormatSpec,
    snapshot: SnapshotFile,
    /// Where the append log begins, and where [`Self::clear`] rewinds to.
    start: u64,
    /// The offset the next [`Self::advance`] will frame.
    next: u64,
    /// One past the last byte the walk may touch: the handle's snapshot end.
    end: u64,
    accounting: ScanAccounting,
    window: ReadAhead,
}

impl<'a> RecordMap<'a> {
    fn new(file: &VarlitFile, entries: &'a mut Vec<RecordIndexEntry>) -> Result<Self> {
        Self::new_through(file, entries, ReadAhead::new()?)
    }

    /// [`Self::new`] reading ahead through a window the caller already has.
    fn new_through(
        file: &VarlitFile,
        entries: &'a mut Vec<RecordIndexEntry>,
        mut window: ReadAhead,
    ) -> Result<Self> {
        // Whatever an earlier walk left in it may since have been rewritten
        // in place.
        window.start = 0;
        window.len = 0;
        let start = append_log_start_for_file(file)?;
        entries.clear();
        let mut accounting = ScanAccounting::default();
        // The header bytes the walk skips are charged the way the open scan
        // charges them, so a `ScanBytes` ceiling means the same number of bytes
        // whichever route reached the record.
        accounting.advance(file.spec, start)?;
        Ok(Self {
            entries,
            spec: file.spec,
            snapshot: file.snapshot.clone(),
            start,
            next: start,
            end: file.snapshot.len(),
            accounting,
            window,
        })
    }

    /// The read-ahead window, for the handle to keep.
    pub(crate) fn into_window(self) -> ReadAhead {
        self.window
    }

    fn new_from(
        file: &VarlitFile,
        entries: &'a mut Vec<RecordIndexEntry>,
        offset: u64,
    ) -> Result<Self> {
        let mut map = Self::new(file, entries)?;
        if offset < map.start || offset > map.end {
            return Err(Error::InvalidFollowCursor(
                "record_map_from offset is outside this handle's append log",
            ));
        }
        // The bytes before `offset` are not framed by this walk, but they are
        // charged, so a `ScanBytes` ceiling keeps meaning bytes of file covered.
        map.accounting.advance(map.spec, offset - map.start)?;
        map.next = offset;
        Ok(map)
    }

    /// A walk that starts at `offset` and charges `ScanBytes` only for what it
    /// frames itself. A paged read is one of many walks over the same file, and
    /// charging each page for the bytes before it would make the ceiling a
    /// limit on how far into a file paging may go rather than on what one call
    /// reads.
    pub(crate) fn new_page(
        file: &VarlitFile,
        entries: &'a mut Vec<RecordIndexEntry>,
        from: Option<PageCursor>,
        window: ReadAhead,
    ) -> Result<Self> {
        let mut map = Self::new_through(file, entries, window)?;
        if let Some(cursor) = from {
            if cursor.offset < map.start || cursor.offset > map.end {
                return Err(Error::InvalidFollowCursor(
                    "page cursor is outside this handle's append log",
                ));
            }
            map.next = cursor.offset;
        }
        map.accounting = ScanAccounting::default();
        Ok(map)
    }

    /// The cursor a page that stopped here hands back: `None` once the walk
    /// has reached the end of the file.
    pub(crate) fn page_cursor(&self) -> Option<PageCursor> {
        (!self.is_complete()).then_some(PageCursor { offset: self.next })
    }

    /// Frames the next record, appends it, and returns it; `None` at the end of
    /// the walk.
    ///
    /// One record, two positional reads — the header, and the footer if the
    /// format has one. Nothing else is touched: a record's payload is not read
    /// to frame it, whatever its size.
    pub fn advance(&mut self) -> Result<Option<RecordIndexEntry>> {
        let Some(entry) = self.frame()? else {
            return Ok(None);
        };
        reserve_scanned_entry(self.spec, self.entries)?;
        self.entries.push(entry.clone());
        Ok(Some(entry))
    }

    /// Frames the next resident record and returns it without keeping it.
    /// The whole of `entry`'s record — header, payload and footer — when the
    /// walk's window still holds it from framing, so a reader of a small
    /// record needs no read of its own.
    pub(crate) fn buffered_record(&self, entry: &RecordIndexEntry) -> Option<&[u8]> {
        self.window
            .buffered(entry.record_offset, entry.whole_record_len()?)
    }

    pub(crate) fn frame(&mut self) -> Result<Option<RecordIndexEntry>> {
        loop {
            if self.next >= self.end {
                return Ok(None);
            }
            // Counted the way the open scan counts, because this is the same
            // unit of work: a header, an extent and a footer read to *build* an
            // index. `ResidentIndex::fault` deliberately does not count — it
            // rebuilds an entry of an index that already exists, and conflating
            // the two would make every `index_entries()` call look like a scan.
            note_record_framed();
            let entry = self.window.frame(&self.snapshot, self.spec, self.next)?;
            let record_end =
                entry
                    .checked_physical_end()
                    .map_err(|_| Error::ResourceArithmeticOverflow {
                        resource: "record extent",
                    })?;
            // A record whose extent runs past the snapshot end is not a record
            // this handle may see. The open scan cannot produce one — its end
            // *is* a record boundary — so this is a torn-file guard, not a
            // routine branch, and it stops rather than reporting corruption
            // the scan already decided to tolerate.
            if record_end > self.end {
                self.next = self.end;
                return Ok(None);
            }
            self.accounting
                .advance(self.spec, record_end.saturating_sub(self.next))?;
            self.next = record_end;
            // Framed and skipped, exactly as the open scan skips it: a
            // non-resident block's records are on disk and not in the index.
            if !record_is_resident(self.spec, entry.block_id) {
                continue;
            }
            return Ok(Some(entry));
        }
    }

    /// Walks forward until `want` accepts a record, and stops there.
    ///
    /// Searches what is already framed first, so a question the last one
    /// already paid for costs no read at all.
    pub fn find(
        &mut self,
        mut want: impl FnMut(&RecordIndexEntry) -> bool,
    ) -> Result<Option<RecordIndexEntry>> {
        if let Some(found) = self.entries.iter().find(|entry| want(entry)) {
            return Ok(Some(found.clone()));
        }
        while let Some(entry) = self.advance()? {
            if want(&entry) {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    /// Walks until the map holds `count` entries, or the file ends first.
    ///
    /// Returns how many it holds, which is `count` unless the file ran out.
    pub fn fill_to(&mut self, count: usize) -> Result<usize> {
        while self.entries.len() < count {
            if self.advance()?.is_none() {
                break;
            }
        }
        Ok(self.entries.len())
    }

    /// Walks to the end of the file.
    ///
    /// This is [`VarlitFile::index_entries_into`] spelled as the case it is —
    /// the one where the answer really is every record — and it costs the same.
    pub fn fill(&mut self) -> Result<usize> {
        while self.advance()?.is_some() {}
        Ok(self.entries.len())
    }

    /// The records framed so far, in file order.
    pub fn entries(&self) -> &[RecordIndexEntry] {
        self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the walk has reached the end of the file.
    ///
    /// A complete map is the whole index; an incomplete one is a prefix, and
    /// every read answered against it is answered against that prefix.
    pub fn is_complete(&self) -> bool {
        self.next >= self.end
    }

    /// The file offset the next [`Self::advance`] will frame.
    pub fn resume_offset(&self) -> u64 {
        self.next
    }

    /// Empties the buffer and rewinds the walk to the start of the file.
    ///
    /// The map is then what [`VarlitFile::record_map`] just returned. Use
    /// [`release`](Self::release) to free the entries without giving up the
    /// ground already covered.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.next = self.start;
    }

    /// Empties the buffer but keeps the walk where it is.
    ///
    /// This is the bounded-memory pass: `fill_to(k)`, use them, `release()`,
    /// repeat. Memory is the caller's `k` entries however long the file is, and
    /// no record is read twice.
    pub fn release(&mut self) {
        self.entries.clear();
    }
}

impl RecordDirectory for RecordMap<'_> {
    fn record_count(&self) -> usize {
        self.entries.len()
    }

    fn record_at(&self, position: usize) -> Result<RecordIndexEntry> {
        self.entries.as_slice().record_at(position)
    }
}

/// A forward walk's read-ahead: one positional read fills a window that then
/// serves the next frames' headers and footers, so a walk over small records
/// costs one read per window instead of two per record.
pub(crate) struct ReadAhead {
    bytes: Box<[u8; READ_AHEAD]>,
    start: u64,
    len: usize,
}

const READ_AHEAD: usize = 16 * 1024;

impl ReadAhead {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            bytes: crate::fallible::try_box([0u8; READ_AHEAD])?,
            start: 0,
            len: 0,
        })
    }

    /// The window's bytes for `offset..offset + len`, when it already holds
    /// all of them.
    pub(crate) fn buffered(&self, offset: u64, len: usize) -> Option<&[u8]> {
        let at = usize::try_from(offset.checked_sub(self.start)?).ok()?;
        self.bytes[..self.len].get(at..at.checked_add(len)?)
    }

    pub(crate) fn read(
        &mut self,
        snapshot: &SnapshotFile,
        offset: u64,
        into: &mut [u8],
    ) -> Result<()> {
        let end = offset
            .checked_add(into.len() as u64)
            .ok_or(Error::LengthOverflow { value: offset })?;
        if into.len() > READ_AHEAD {
            return snapshot.read_exact_at(offset, into);
        }
        if offset < self.start || end > self.start + self.len as u64 {
            let len =
                crate::width::index(snapshot.len().saturating_sub(offset).min(READ_AHEAD as u64))?;
            if len < into.len() {
                return Err(Error::UnexpectedEof);
            }
            snapshot.read_exact_at(offset, &mut self.bytes[..len])?;
            self.start = offset;
            self.len = len;
        }
        let at = crate::width::index(offset - self.start)?;
        into.copy_from_slice(&self.bytes[at..at + into.len()]);
        Ok(())
    }

    /// Frames the record at `offset` through the window.
    pub(crate) fn frame(
        &mut self,
        snapshot: &SnapshotFile,
        spec: FormatSpec,
        offset: u64,
    ) -> Result<RecordIndexEntry> {
        fault_record_entry_with::<{ crate::width::small(RECORD_HEADER_LEN) }>(
            snapshot.len(),
            spec,
            offset,
            true,
            |at, into| self.read(snapshot, at, into),
        )
    }
}

/// A handle's records, answered by reading the file instead of keeping a list.
///
/// This is what a handle that keeps no in-memory record list hands to every
/// position-based read: `record_count` and `record_at` are answered by framing
/// records off the file — a header (and footer) read each — and nothing is
/// kept but a cursor and the count. Reads walk positions in order, so the
/// cursor makes a whole pass one walk; going backwards restarts from the log
/// start.
///
/// The count is established when the walk is built, which is one pass over
/// the file: every read it serves is itself a pass, so that at most doubles
/// the reads and never adds memory.
pub(crate) struct WalkedRecords {
    spec: FormatSpec,
    snapshot: SnapshotFile,
    start: u64,
    end: u64,
    count: usize,
    /// The position the next frame produces, and the offset it frames at.
    cursor: std::cell::Cell<(usize, u64)>,
    window: std::cell::RefCell<ReadAhead>,
}

impl WalkedRecords {
    pub(crate) fn new(file: &VarlitFile) -> Result<Self> {
        let start = append_log_start_for_file(file)?;
        // A handle that already knows how many records the file holds skips
        // the counting pass.
        if let Some(count) = file.index.whole_file_count() {
            return Ok(Self {
                spec: file.spec,
                snapshot: file.snapshot.clone(),
                start,
                end: file.snapshot.len(),
                count,
                cursor: std::cell::Cell::new((0, start)),
                window: std::cell::RefCell::new(ReadAhead::new()?),
            });
        }
        let mut walk = Self {
            spec: file.spec,
            snapshot: file.snapshot.clone(),
            start,
            end: file.snapshot.len(),
            count: 0,
            cursor: std::cell::Cell::new((0, start)),
            window: std::cell::RefCell::new(ReadAhead::new()?),
        };
        let mut count = 0usize;
        let mut next = start;
        while walk.frame_from(&mut next)?.is_some() {
            count += 1;
        }
        walk.count = count;
        Ok(walk)
    }

    /// Frames the resident record at or after `next`, advancing `next` past
    /// it; `None` at the end of the log. The same rule `RecordMap::frame`
    /// applies, so a walk lists exactly what an open scan indexes.
    fn frame_from(&self, next: &mut u64) -> Result<Option<RecordIndexEntry>> {
        loop {
            if *next >= self.end {
                return Ok(None);
            }
            note_record_framed();
            let entry = self
                .window
                .borrow_mut()
                .frame(&self.snapshot, self.spec, *next)?;
            let record_end =
                entry
                    .checked_physical_end()
                    .map_err(|_| Error::ResourceArithmeticOverflow {
                        resource: "record extent",
                    })?;
            if record_end > self.end {
                *next = self.end;
                return Ok(None);
            }
            *next = record_end;
            if record_is_resident(self.spec, entry.block_id) {
                return Ok(Some(entry));
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.count
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = Result<RecordIndexEntry>> + '_ {
        let mut next = self.start;
        let mut left = self.count;
        std::iter::from_fn(move || {
            if left == 0 {
                return None;
            }
            left -= 1;
            Some(
                self.frame_from(&mut next)
                    .and_then(|entry| entry.ok_or(Error::UnexpectedEof)),
            )
        })
    }
}

impl RecordDirectory for WalkedRecords {
    fn record_count(&self) -> usize {
        self.count
    }

    fn record_at(&self, position: usize) -> Result<RecordIndexEntry> {
        if position >= self.count {
            return Err(Error::UnexpectedEof);
        }
        let (mut at, mut next) = self.cursor.get();
        if position < at {
            (at, next) = (0, self.start);
        }
        loop {
            let entry = self.frame_from(&mut next)?.ok_or(Error::UnexpectedEof)?;
            at += 1;
            if at - 1 == position {
                self.cursor.set((at, next));
                return Ok(entry);
            }
        }
    }
}

/// The record list a position-based read runs over: the handle's own when it
/// keeps one, the file itself when it does not.
// Short-lived and built once per read, so the walk is kept inline.
#[allow(clippy::large_enum_variant)]
pub(crate) enum RecordList<'a> {
    Kept(resident_index::Kept<'a>),
    Walked(WalkedRecords),
}

impl RecordList<'_> {
    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Kept(index) => index.len(),
            Self::Walked(walk) => walk.len(),
        }
    }

    pub(crate) fn iter(&self) -> Box<dyn Iterator<Item = Result<RecordIndexEntry>> + '_> {
        match self {
            Self::Kept(index) => Box::new(index.iter()),
            Self::Walked(walk) => Box::new(walk.iter()),
        }
    }
}

impl RecordDirectory for RecordList<'_> {
    fn record_count(&self) -> usize {
        self.len()
    }

    fn record_at(&self, position: usize) -> Result<RecordIndexEntry> {
        match self {
            Self::Kept(index) => index.record_at(position),
            Self::Walked(walk) => walk.record_at(position),
        }
    }

    fn block_id_at(&self, position: usize) -> Option<u32> {
        match self {
            Self::Kept(index) => index.block_id_at(position),
            Self::Walked(_) => None,
        }
    }
}

impl std::fmt::Debug for RecordMap<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecordMap")
            .field("framed", &self.entries.len())
            .field("resume_offset", &self.next)
            .field("complete", &self.is_complete())
            .finish()
    }
}

/// Where a paged read continues: the record boundary the next page starts at.
///
/// Taken from a [`Page`] and handed back to the matching `_from` method of the
/// same handle. It names a byte offset in the append log, so it stays valid
/// across appends to the file — a later page simply sees more records — and a
/// value that is not a record boundary is refused when the walk frames it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageCursor {
    offset: u64,
}

impl PageCursor {
    /// The append-log offset the next page starts at, which is also what
    /// [`VarlitFile::record_map_from`] takes.
    pub const fn offset(self) -> u64 {
        self.offset
    }
}

/// What one paged read wrote into the caller's buffer.
///
/// `filled` slots at the front of the buffer hold this page; the rest are
/// untouched. `next` is where the following page starts, or `None` when the
/// walk reached the end of the file. A full buffer whose remaining records turn
/// out not to match still reports `Some`, and the next call returns an empty,
/// final page: the walk does not read ahead to find out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct Page {
    pub filled: usize,
    pub next: Option<PageCursor>,
}

/// A [`VarlitFile`]'s reads, resolved through a caller-supplied directory.
///
/// Built by [`VarlitFile::with_directory`]. Every method is the handle's method
/// of the same name, answered against the borrowed directory; nothing here
/// allocates a directory of its own.
#[derive(Debug)]
pub struct DirectoryRead<'a, D: RecordDirectory + ?Sized> {
    file: &'a VarlitFile,
    directory: &'a D,
}

impl<D: RecordDirectory + ?Sized> DirectoryRead<'_, D> {
    /// How many records the supplied directory describes.
    pub fn record_count(&self) -> usize {
        self.directory.record_count()
    }

    pub fn scan(&self) -> impl Iterator<Item = Result<BlockEvent>> + '_ {
        VarlitFile::scan_in(self.directory)
    }

    pub fn metadata(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.file.metadata_in(self.directory, key)
    }

    pub fn all_metadata_into(&self, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
        self.file.all_metadata_into_in(self.directory, out)
    }

    pub fn schema_manifest(&self) -> Result<Option<SchemaManifest>> {
        self.file.schema_manifest_in(self.directory)
    }

    pub fn blocks<T: VarlitBlock>(&self) -> Result<BlockVec<T>> {
        self.file.blocks_in(self.directory)
    }

    pub fn block_entries_into<T: VarlitBlock>(
        &self,
        out: &mut Vec<RecordIndexEntry>,
    ) -> Result<()> {
        self.file.block_entries_into_in::<T, _>(self.directory, out)
    }

    pub fn decode_blocks_into<T: VarlitBlock>(&self, out: &mut Vec<T>) -> Result<()> {
        self.file.decode_blocks_into_in::<T, _>(self.directory, out)
    }

    pub fn blocks_migrated_into<From, To, M>(&self, out: &mut Vec<To>) -> Result<()>
    where
        From: VarlitBlock,
        To: VarlitBlock,
        M: VarlitMigration<From, To>,
    {
        self.file
            .blocks_migrated_into_in::<From, To, M, _>(self.directory, out)
    }

    pub fn keyed_blocks_into<T>(
        &self,
        entries: &mut Vec<RecordIndexEntry>,
        by_key: &mut HashMap<T::Key, RecordIndexEntry>,
    ) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash + Clone,
    {
        self.file
            .keyed_blocks_into_in::<T, _>(self.directory, entries, by_key)
    }

    pub fn materialized_keyed_blocks_into<T>(&self, out: &mut HashMap<T::Key, T>) -> Result<()>
    where
        T: VarlitMerge,
        T::Key: Eq + Hash,
    {
        self.file
            .materialized_keyed_blocks_into_in::<T, _>(self.directory, out)
    }

    pub fn key_tail_offsets_into<T>(&self, out: &mut HashMap<T::Key, u64>) -> Result<()>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file
            .key_tail_offsets_into_in::<T, _>(self.directory, out)
    }

    pub fn index_entries_into(&self, out: &mut Vec<RecordIndexEntry>) -> Result<()> {
        self.file.index_entries_into_in(self.directory, out)
    }

    pub fn verify_all(&self) -> Result<usize> {
        self.file.verify_all_in(self.directory)
    }

    /// Reads through an entry the caller holds; see
    /// [`VarlitFile::decode_block_into`]. Present here so a directory-supplied
    /// walk needs no second handle.
    pub fn decode_block_into<T: VarlitBlock>(
        &self,
        entry: &RecordIndexEntry,
        scratch: &mut Vec<u8>,
        out: &mut T,
    ) -> Result<()> {
        self.file.decode_block_into::<T>(entry, scratch, out)
    }
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct RecordIndexEntry {
    pub block_id: u32,
    pub block_version: u16,
    pub flags: u16,
    pub sequence: u64,
    pub record_offset: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub checksum: u32,
    pub uncompressed_len_hint: u32,
    pub footer_offset: Option<u64>,
    pub prev_same_block_offset: Option<u64>,
    pub prev_same_key_offset: Option<u64>,
    /// The trailing footer word this record carries.
    ///
    /// Always `0` under [`LivenessPolicy::None`], which is every format that
    /// did not opt in. Under [`LivenessPolicy::FooterFlags`] it is the record's
    /// mutable flags — the only part of a written record that can change
    /// without rewriting it, and the only field here that is not covered by the
    /// record's checksum.
    pub mutable_flags: u32,
    pub committed: bool,
}
