use std::borrow::Borrow;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use crate::collections::{MaterializationBudget, ensure_registered_block};
use crate::file::{
    NativeStreamScanner, PreparedStreamRecord, WriterLock, fresh_stream_creation_nonce,
    prepare_stream_creation_nonce_record, prepare_stream_manifest_record,
    prepare_stream_tombstone_record, prepare_stream_user_record_into, read_file_header,
    read_primary_header, write_stream_file_header,
};
use crate::primary_file::PrimaryFile;
use crate::scan_control::{ScanCancelled, ScanProgressDriver};
use crate::snapshot::WrittenThrough;
use crate::traits::KeyedBlockContract;
use crate::{
    AppendInfo, BlockEvent, BlockKind, CREATION_NONCE_BLOCK_ID, CommitPolicy, Error, FormatSpec,
    LayoutPreset, MANIFEST_BLOCK_ID, ManifestPolicy, ResourceLimits, Result, ScanOptions,
    ScanProgress, ScanProgressOptions, SnapshotFile, TOMBSTONE_BLOCK_ID, VarlitBlock,
    VarlitKeyedBlock,
};

const STREAM_WRITER_POISON_CONTEXT: &str = "stream";

// Mechanical enforcement of the writer poison check (round 12, ) lives in
// `crate::writer_permit`, which round 12's enforcement pass promoted out of
// this file so that `file.rs` and `layout.rs` hold the same type rather than
// three independent hand-written conventions. See that module's documentation
// for what the two tokens make impossible.
pub(crate) use crate::writer_permit::MutationPermit;
use crate::writer_permit::{GuardedWriter, PoisonFlag};

/// The witness this writer's mutating entry points demand. Typed by the writer
/// it speaks for, so a permit taken from some other writer — or, as round 12's
/// re-verification showed, from a throwaway `PoisonFlag` — is not this type.
pub(crate) type StreamMutationPermit<'a> = MutationPermit<'a, VarlitStreamWriter>;

pub(crate) const INTERNAL_BLOCK_IDS: [u32; 3] = [
    MANIFEST_BLOCK_ID,
    TOMBSTONE_BLOCK_ID,
    CREATION_NONCE_BLOCK_ID,
];

thread_local! {
    /// Armed native-append write failures whose rollback also fails (round 12,
    /// F-05).
    static INJECTED_APPEND_ROLLBACK_FAILURES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Consumes one armed write-plus-rollback append failure (round 12, ).
///
/// Returns `true` when the caller must behave as though the native chunk write
/// failed *and* the truncate-or-seek rollback that follows it failed too. That
/// is the pair  names: it is the shortest route to
/// `Error::WriteRollbackFailed`, and it is not reachable from outside the
/// crate on demand, because it needs two consecutive I/O failures on the same
/// open handle. (The snapshot-rebind failure further down the same function
/// has its own identical rollback, which the existing process-abort hooks
/// already cover.) Nothing is written and nothing is truncated when this
/// fires, so the on-disk file is exactly what a real failed-then-rolled-back
/// write leaves behind; only the writer's own state is driven onto the
/// poisoned path.
///
/// Inert without the `scalable-fault-injection` feature. Thread-local so
/// concurrently running tests cannot arm each other's writers.
#[inline]
fn take_injected_append_rollback_failure() -> bool {
    #[cfg(feature = "scalable-fault-injection")]
    {
        let armed = INJECTED_APPEND_ROLLBACK_FAILURES.with(|count| {
            let current = count.get();
            if current != 0 {
                count.set(current - 1);
            }
            current != 0
        });
        if armed {
            return true;
        }
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamOptions {
    pub limits: ResourceLimits,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            limits: ResourceLimits::MISSING,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchOptions {
    pub max_records: usize,
    pub max_bytes: usize,
}

impl BatchOptions {
    pub(crate) const fn validate(self) -> Result<Self> {
        if self.max_records == 0 {
            return Err(Error::InvalidBatchOptions {
                field: "max_records",
            });
        }
        if self.max_bytes == 0 {
            return Err(Error::InvalidBatchOptions { field: "max_bytes" });
        }
        Ok(self)
    }
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self {
            max_records: 16_384,
            max_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BatchAppendInfo {
    pub records: u64,
    pub first_sequence: Option<u64>,
    pub last_sequence: Option<u64>,
    pub start_offset: u64,
    pub end_offset: u64,
    pub write_calls: u64,
}

#[derive(Debug)]
pub struct BatchAppendError {
    pub written: BatchAppendInfo,
    pub source: Error,
}

impl std::fmt::Display for BatchAppendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "batch append stopped after {} written records: {}",
            self.written.records, self.source
        )
    }
}

impl std::error::Error for BatchAppendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamResidentState {
    pub declared_block_tails: usize,
    pub retained_record_entries: usize,
    pub retained_key_entries: usize,
    pub active_payload_bytes: u64,
}

/// How far a reader trails its file's confirmed prefix: what
/// [`VarlitStreamReader::follow`] would adopt now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotStatus {
    /// The end of the prefix this reader holds.
    pub committed_len: u64,
    /// Confirmed records past it.
    pub records_behind: u64,
    /// Confirmed bytes past it.
    pub bytes_behind: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StreamTail {
    pub(crate) block_id: u32,
    pub(crate) record_offset: u64,
    pub(crate) sequence: u64,
}

/// A reader owned by one reading thread. It is `Send`, but deliberately not
/// `Sync`: open a separate reader per thread instead of sharing this handle.
/// Reads use `&self`; following only mutates this handle's snapshot and cursor.
pub struct VarlitStreamReader {
    spec: FormatSpec,
    snapshot: SnapshotFile,
    /// Where this file's durable boundary is read again on follow.
    root: RootSource,
    /// The verified sync-marker prefix this handle has adopted.
    tail: crate::sync_marker::MarkedTail,
    // Cell is Send + !Sync. This zero-sized marker adds no runtime locking.
    _thread_owned: PhantomData<std::cell::Cell<()>>,
}

/// A generation `prepare_follow` read and checked, ready to install.
struct FollowedGeneration {
    snapshot: SnapshotFile,
    tail: crate::sync_marker::MarkedTail,
    added: u64,
}

impl VarlitStreamReader {
    /// Opens the confirmed prefix: the durable end the writer published, and
    /// every sync after it whose marker verifies. Nothing past the last
    /// verified marker is visible.
    pub fn open(spec: FormatSpec, path: impl AsRef<Path>, options: StreamOptions) -> Result<Self> {
        refuse_matrix_stream(spec)?;
        let spec = stream_spec(spec, options, false)?;
        let path = std::fs::canonicalize(path.as_ref())?;
        let mut file = PrimaryFile::new(
            crate::primary_file::open_data_file(OpenOptions::new().read(true), &path)?,
            None,
        );
        let (header_len, extensions) = read_primary_header(spec, &mut file)?;
        let root = RootSource {
            header_len,
            slots_at: crate::file::published_end_slots_at(header_len, &extensions)?,
        };
        let header = SnapshotFile::from_file_with_len(file, header_len)?;
        let (published, captured_len) = root.read(&header, || Ok(header.physical()?.len()))?;
        let physical = header.with_len(captured_len)?;
        let start = match published {
            StreamRoot::Published(end) => end,
            StreamRoot::Scan { takeover } => {
                scan_root(spec, &physical, header_len, takeover, &mut NoPosition)?
            }
        };
        let mut tail = crate::sync_marker::MarkedTail::new(start);
        tail.advance(spec, &physical)?;
        Ok(Self {
            spec,
            snapshot: physical.with_len(tail.committed())?,
            root,
            tail,
            _thread_owned: PhantomData,
        })
    }

    /// Adopt the latest confirmed prefix on this file object. Dirty
    /// working data remains invisible. Returns the additional native record
    /// count; only the newly confirmed records are framed.
    pub fn follow(&mut self) -> Result<u64> {
        self.follow_records()
    }

    /// Follows through the verified driver: everything fallible happens in
    /// `prepare_follow`, which only reads the handle, so a failed follow
    /// leaves the reader exactly as it was.
    fn follow_records(&mut self) -> Result<u64> {
        crate::publication_protocol::follow(self, Self::prepare_follow, |reader, followed| {
            match followed {
                Some(followed) => reader.adopt_follow(followed),
                None => 0,
            }
        })
    }

    fn prepare_follow(&self) -> Result<Option<FollowedGeneration>> {
        let (published, len) = self
            .root
            .read(&self.snapshot, || Ok(self.snapshot.physical()?.len()))?;
        let physical = self.snapshot.with_len(len)?;
        let mut tail = self.tail;
        // A newer published end lets the walk start there; otherwise the
        // walk continues from the last verified marker.
        if let StreamRoot::Published(end) = published
            && end > tail.committed()
        {
            tail = crate::sync_marker::MarkedTail::new(end);
        }
        tail.advance(self.spec, &physical)?;
        let from = self.snapshot.len();
        let to = tail.committed();
        if to <= from {
            return Ok(None);
        }
        let mut added = CountPosition(0);
        frame_until(self.spec, &physical, from, to, &mut added)?;
        Ok(Some(FollowedGeneration {
            snapshot: physical.with_len(to)?,
            tail,
            added: added.0,
        }))
    }

    /// Installs a prepared generation; infallible by construction.
    fn adopt_follow(&mut self, followed: FollowedGeneration) -> u64 {
        self.snapshot = followed.snapshot;
        self.tail = followed.tail;
        followed.added
    }

    /// Refresh and extend this handle's event cursor without rescanning its prefix.
    pub fn follow_events(&mut self, events: &mut StreamEvents) -> Result<u64> {
        events.validate_follow(&self.snapshot)?;
        let added = self.follow()?;
        events.extend_snapshot(self.snapshot.clone());
        Ok(added)
    }

    /// Refresh and resume a typed cursor at its existing position. EOF is
    /// resumable; a cursor that returned an error cannot be resumed.
    pub fn follow_blocks<T: VarlitBlock>(
        &mut self,
        blocks: &mut StreamingBlocks<T>,
    ) -> Result<u64> {
        blocks.validate_follow(&self.snapshot)?;
        let added = self.follow()?;
        blocks.extend_snapshot(self.snapshot.clone());
        Ok(added)
    }

    /// How far this handle trails the file's confirmed prefix: what
    /// [`Self::follow`] would adopt now. Reads the published end and frames
    /// only the records past this handle's end.
    pub fn snapshot_status(&self) -> Result<crate::SnapshotStatus> {
        let behind = self.prepare_follow()?;
        Ok(crate::SnapshotStatus {
            committed_len: self.snapshot.len(),
            records_behind: behind.as_ref().map_or(0, |followed| followed.added),
            bytes_behind: behind.map_or(0, |followed| {
                followed.snapshot.len().saturating_sub(self.snapshot.len())
            }),
        })
    }

    /// The EOF of this handle's confirmed generation.
    pub fn committed_len(&self) -> u64 {
        self.snapshot.len()
    }

    pub fn verify_all(&self) -> Result<u64> {
        self.verify_all_with_progress(silent_scan_options(), |_| {})
    }

    pub fn verify_all_with_progress<F>(&self, scan: ScanOptions<'_>, mut observer: F) -> Result<u64>
    where
        F: FnMut(ScanProgress),
    {
        let mut header_file = self.snapshot.reader();
        let header_eof = read_file_header(self.spec, &mut header_file)?;
        let mut progress = ScanProgressDriver::new(scan, header_eof, self.snapshot.len());
        progress.start(&mut observer).map_err(scan_cancelled)?;
        let mut scanner = NativeStreamScanner::from_snapshot(
            self.spec.with_strict_read_validation(),
            self.snapshot.clone(),
        )?;
        let mut records = 0u64;
        while scanner.next_entry()?.is_some() {
            records = records
                .checked_add(1)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "verified record count",
                })?;
            progress
                .record_completed(scanner.logical_eof(), &mut observer)
                .map_err(scan_cancelled)?;
        }
        if scanner.logical_eof() != self.snapshot.len() {
            return Err(Error::CorruptTail {
                offset: scanner.logical_eof(),
            });
        }
        progress.complete(&mut observer).map_err(scan_cancelled)?;
        Ok(records)
    }

    pub fn events(&self) -> Result<StreamEvents> {
        Ok(StreamEvents {
            scanner: NativeStreamScanner::from_snapshot(self.spec, self.snapshot.clone())?,
            finished: false,
            failed: false,
            _thread_owned: PhantomData,
        })
    }

    pub fn blocks<T: VarlitBlock>(&self) -> Result<StreamingBlocks<T>> {
        ensure_registered_block::<T>(self.spec)?;
        if T::KIND == BlockKind::Matrix {
            return Err(Error::StreamingUnsupported);
        }
        // Under CRC policies the sequential scanner would stream every
        // record's payload through the checksum, and the typed decode below
        // would then read the payload a second time. Scan frames without that
        // pre-pass: `read_logical_payload_snapshot` verifies the full record
        // checksum over the one payload read whose buffer feeds the typed
        // decode, and skipped foreign-block payloads are not read at all.
        // `verify_all` remains the whole-file integrity scan.
        Ok(StreamingBlocks {
            payload: Vec::new(),
            record_cache: std::cell::RefCell::new(crate::file::ChunkCache::for_spec(self.spec)?),
            scanner: NativeStreamScanner::from_snapshot_framing_only(
                self.spec,
                self.snapshot.clone(),
            )?,
            spec: self.spec,
            finished: false,
            failed: false,
            _marker: PhantomData,
        })
    }

    pub fn resident_state(&self) -> StreamResidentState {
        resident_state(self.spec)
    }
}

/// A cursor owned by its creating thread (`!Send + !Sync`).
#[derive(Debug)]
pub struct StreamEvents {
    scanner: NativeStreamScanner,
    finished: bool,
    failed: bool,
    _thread_owned: PhantomData<std::rc::Rc<()>>,
}

impl StreamEvents {
    pub(crate) fn validate_follow(&self, snapshot: &SnapshotFile) -> Result<()> {
        if self.failed {
            return Err(Error::InvalidFollowCursor(
                "cursor has already returned an error",
            ));
        }
        if !self.scanner.snapshot().same_file_object(snapshot)
            || self.scanner.snapshot().len() > snapshot.len()
        {
            return Err(Error::InvalidFollowCursor(
                "cursor does not belong to this reader",
            ));
        }
        Ok(())
    }

    pub(crate) fn extend_snapshot(&mut self, snapshot: SnapshotFile) {
        self.scanner.extend_snapshot(snapshot);
        self.finished = false;
    }
}

impl Iterator for StreamEvents {
    type Item = Result<BlockEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match self.scanner.next_entry() {
            Ok(Some(entry)) => Some(Ok(BlockEvent::from(&entry))),
            Ok(None) => {
                self.finished = true;
                None
            }
            Err(error) => {
                self.finished = true;
                self.failed = true;
                Some(Err(error))
            }
        }
    }
}

/// A typed cursor owned by its creating thread (`!Send + !Sync`).
#[derive(Debug)]
pub struct StreamingBlocks<T> {
    scanner: NativeStreamScanner,
    /// The reader's format contract, also preserved by the frame-only scanner.
    /// Typed decode verifies each consumed record's checksum against this spec.
    spec: FormatSpec,
    finished: bool,
    failed: bool,
    /// Reused across steps, so a walk of `N` records allocates one payload
    /// rather than `N`. This is the scalable family's per-record read, so it is
    /// the one place on it where that difference is unbounded.
    payload: Vec<u8>,
    /// The checked chunk a long record's fields are read through.
    record_cache: std::cell::RefCell<crate::file::ChunkCache>,
    _marker: PhantomData<(T, std::rc::Rc<()>)>,
}

impl<T> StreamingBlocks<T> {
    pub(crate) fn validate_follow(&self, snapshot: &SnapshotFile) -> Result<()> {
        if self.failed {
            return Err(Error::InvalidFollowCursor(
                "cursor has already returned an error",
            ));
        }
        if !self.scanner.snapshot().same_file_object(snapshot)
            || self.scanner.snapshot().len() > snapshot.len()
        {
            return Err(Error::InvalidFollowCursor(
                "cursor does not belong to this reader",
            ));
        }
        Ok(())
    }

    pub(crate) fn extend_snapshot(&mut self, snapshot: SnapshotFile) {
        self.scanner.extend_snapshot(snapshot);
        self.finished = false;
    }
}

impl<T: VarlitBlock> StreamingBlocks<T> {
    /// Decodes the next record of `T` into `out`, reusing what it owns.
    ///
    /// Returns `false` at the end of the snapshot, leaving `out` untouched. A
    /// returned error ends the walk (the cursor is then unusable for
    /// following); `out` is valid but unspecified.
    pub fn next_into(&mut self, out: &mut T) -> Result<bool> {
        let mut budget = MaterializationBudget::new(self.spec);
        let Some(entry) = self.advance()? else {
            return Ok(false);
        };
        // The single payload read: checked as it is decoded, large fields
        // straight into `out`.
        let decoded = crate::file::decode_record_into(
            self.spec,
            self.scanner.snapshot(),
            &entry,
            &mut self.payload,
            &mut budget,
            &self.record_cache,
            out,
        );
        self.settle(decoded).map(|()| true)
    }

    /// The next record as a fresh value; crate tests only.
    #[cfg(test)]
    pub(crate) fn next_value(&mut self) -> Option<Result<T>> {
        let mut budget = MaterializationBudget::new(self.spec);
        match self.advance() {
            Ok(None) => None,
            Err(error) => Some(Err(error)),
            Ok(Some(entry)) => {
                let decoded = entry
                    .logical_payload_len_snapshot(self.spec, self.scanner.snapshot())
                    .and_then(|logical_len| budget.consume(logical_len))
                    .and_then(|()| {
                        entry.read_logical_payload_snapshot_into(
                            self.spec,
                            self.scanner.snapshot(),
                            &mut self.payload,
                        )
                    })
                    .and_then(|()| {
                        budget.decode(&self.payload, T::ENDIAN.unwrap_or(self.spec.endian))
                    });
                Some(self.settle(decoded))
            }
        }
    }

    /// A decode error ends the walk like any other error.
    fn settle<R>(&mut self, result: Result<R>) -> Result<R> {
        if result.is_err() {
            self.finished = true;
            self.failed = true;
        }
        result
    }

    /// Finds the next record of `T`, charges its logical length to `budget`
    /// and reads it once (checksummed) into `self.payload`. `false` at the end.
    /// The next record of `T`, framed but not yet read.
    fn advance(&mut self) -> Result<Option<crate::RecordIndexEntry>> {
        if self.finished {
            return Ok(None);
        }
        // The scanner must not checksum payloads: the typed decode owns the
        // single, checksum-verified payload read.
        debug_assert!(
            !self.scanner.verifies_checksums(),
            "streaming typed scan requires a frame-only scanner",
        );
        loop {
            let entry = match self.scanner.next_entry() {
                Ok(Some(entry)) => entry,
                Ok(None) => {
                    self.finished = true;
                    return Ok(None);
                }
                Err(error) => {
                    self.finished = true;
                    self.failed = true;
                    return Err(error);
                }
            };
            if entry.block_id != T::ID {
                continue;
            }
            if entry.block_version != T::VERSION {
                self.finished = true;
                self.failed = true;
                return Err(Error::BlockVersionMismatch {
                    block_id: T::ID,
                    expected: T::VERSION,
                    actual: entry.block_version,
                });
            }
            return Ok(Some(entry));
        }
    }
}

/// Crate tests walk a cursor as an iterator of fresh values.
#[cfg(test)]
impl<T: VarlitBlock> Iterator for StreamingBlocks<T> {
    type Item = Result<T>;
    fn next(&mut self) -> Option<Result<T>> {
        self.next_value()
    }
}

pub struct VarlitStreamWriter {
    spec: FormatSpec,
    file: PrimaryFile,
    snapshot: SnapshotFile,
    sequence: Option<u64>,
    record_count: u64,
    block_tails: Vec<(u32, Option<StreamTail>)>,
    poison: PoisonFlag,
    /// Staging buffer for the single-record append path, reused for the life of
    /// the writer.
    ///
    /// The record is built here and written from here, so continuous append
    /// takes one allocation in total rather than one per record. It is cleared
    /// at the start of each record and never read across calls, so nothing
    /// depends on what it last held; it is a buffer, not state.
    record_buffer: Vec<u8>,
    immediate_state: crate::immediate::ImmediateState,
    /// The open link of this writer's sync-marker chain; `Some` once the
    /// writer is created or opened.
    /// See [`crate::sync_marker`].
    sync_chain: Option<crate::sync_marker::SyncChain>,
    _lock: WriterLock,
}

impl GuardedWriter for VarlitStreamWriter {
    fn poison_flag(&self) -> &PoisonFlag {
        &self.poison
    }
    fn poison_flag_mut(&mut self) -> &mut PoisonFlag {
        &mut self.poison
    }
}

impl VarlitStreamWriter {
    /// A permit through the stream's one poison flag, for the compile-fail
    /// instance-binding probes.
    #[cfg(feature = "scalable-fault-injection")]
    pub(crate) fn mutation_permit(
        &mut self,
        context: &'static str,
    ) -> Result<StreamMutationPermit<'_>> {
        self.writer_permit(context)
    }

    pub fn create(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: StreamOptions,
    ) -> Result<Self> {
        refuse_matrix_stream(spec)?;
        let path = canonical_write_path(path.as_ref())?;
        let mut writer = Self::create_unmanaged(spec, &path, options)?;
        // The nonce and manifest open the sync chain: they are published and
        // made durable with the file itself, so the first sync's marker
        // verifies from here and a reader never sees a record before it.
        writer
            .file
            .write_published_end(Some(writer.snapshot.len()))?;
        crate::scalable_fault_point("create.native_sync");
        let sync = writer.file.sync_all();
        crate::scalable_fault_point("create.native_sync");
        sync?;
        // The file is durable; its name is once the directory holding it is.
        // A failure here leaves a complete file a plain open resumes.
        crate::file::sync_parent_directory(&path).map_err(|error| {
            crate::fallible::boxed_path_error(
                error,
                &path,
                Error::ErrorContextAllocationFailed {
                    context: crate::AllocationFailureContext::ParentSyncPending,
                },
                |path, source| Error::PublishedButParentSyncPending { path, source },
            )
        })?;
        writer.begin_sync_chain();
        Ok(writer)
    }

    pub(crate) fn create_unmanaged(
        spec: FormatSpec,
        path: impl AsRef<Path>,
        options: StreamOptions,
    ) -> Result<Self> {
        let spec = stream_spec(spec, options, true)?;
        let path = canonical_write_path(path.as_ref())?;
        let mut lock = WriterLock::acquire(&path)?;
        let mut file = PrimaryFile::new(
            crate::primary_file::open_data_file(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false),
                &path,
            )?,
            None,
        );
        // Bind the authoritative object lock onto the freshly created file. The
        // acquire-time probe cannot lock a file that did not exist yet, so
        // without this a hard-link alias opened after creation would win a
        // second writer role. Mirrors VarlitFile::create_impl and
        // VarlitLayoutWriter::create_inner.
        //
        // the bind must happen before any destructive initialization.
        // Opening with `.truncate(true)` would clear the object inside the
        // pre-bind window, so a losing concurrent creator could truncate the
        // winner's freshly initialized file before its own bind fails. Open
        // without truncate, bind, then truncate through the bound handle.
        lock.bind_native(file.raw(), &path)?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let header_extensions = write_stream_file_header(spec, &mut file)?;
        let header_len = file.stream_position()?;
        adopt_published_end(&mut file, header_len, &header_extensions)?;
        let snapshot = SnapshotFile::new(file.try_clone()?)?;
        let mut writer = Self {
            spec,
            file,
            snapshot,
            sequence: Some(0),
            record_count: 0,
            block_tails: initial_block_tails(spec),
            poison: PoisonFlag::healthy(),
            record_buffer: Vec::new(),
            immediate_state: Default::default(),
            sync_chain: None,
            _lock: lock,
        };
        // stamp the per-create nonce as the very first record of the
        // log, before anything else can be appended. It has to live inside the
        // primary because an equal-length in-place rewrite preserves the OS
        // object, the schema hash and every header byte a witness looks at —
        // everything except the primary's own content. (The one header byte
        // range a witness no longer looks at is a declared `header_tails`
        // region's payload, which `primary_generation` blanks; the nonce sits
        // in a record after the header and is unaffected either way.) One
        // record at create; zero cost per append.
        {
            let sequence = writer.next_sequence()?;
            let offset = writer.snapshot.len();
            let record = prepare_stream_creation_nonce_record(
                spec,
                fresh_stream_creation_nonce(),
                sequence,
                offset,
            )?;
            writer.check_mutation(STREAM_WRITER_POISON_CONTEXT)?;
            writer
                .ensure_writable()?
                .append_prepared(CREATION_NONCE_BLOCK_ID, record)?;
        }
        if spec.manifest_policy == ManifestPolicy::Embedded {
            let sequence = writer.next_sequence()?;
            let offset = writer.snapshot.len();
            let record = prepare_stream_manifest_record(spec, sequence, offset)?;
            writer.check_mutation(STREAM_WRITER_POISON_CONTEXT)?;
            writer
                .ensure_writable()?
                .append_prepared(MANIFEST_BLOCK_ID, record)?;
        }
        // The nonce and manifest are the format's own records, not writes the
        // caller made: an immediate policy counts from the first caller write.
        writer.immediate_state.reset();
        Ok(writer)
    }

    /// Opens the writer at the file's confirmed prefix, recovering first.
    ///
    /// The durable boundary is the end the last sync published in the
    /// file's `VPUB` block. The state there (next sequence, record count,
    /// block tails) is the stream summary of the newest intact anchor before
    /// it, advanced over the records framed from that anchor on; a file with
    /// no such anchor is framed from its header. Every later sync whose marker
    /// verifies is kept, everything after the last one is cut, and the kept
    /// end is synced and published before the first append. A file with no
    /// published end (one written before the block existed, or last written by
    /// a native writer) is framed from its header to find it.
    pub fn open(spec: FormatSpec, path: impl AsRef<Path>, options: StreamOptions) -> Result<Self> {
        refuse_matrix_stream(spec)?;
        Self::open_recovered(spec, path.as_ref(), options)
    }

    fn open_recovered(spec: FormatSpec, path: &Path, options: StreamOptions) -> Result<Self> {
        let spec = stream_spec(spec, options, false)?;
        let path = std::fs::canonicalize(path)?;
        let lock = WriterLock::acquire(&path)?;
        let mut file = PrimaryFile::new(
            crate::primary_file::open_data_file(OpenOptions::new().read(true).write(true), &path)?,
            None,
        );
        let (header_len, extensions) = read_primary_header(spec, &mut file)?;
        let root = RootSource {
            header_len,
            slots_at: crate::file::published_end_slots_at(header_len, &extensions)?,
        };
        if let Some(slots_at) = root.slots_at {
            file.adopt_writer_published(slots_at)?;
        }
        // Logical: an anchored file's anchors are not part of the log.
        let header = SnapshotFile::from_file_with_len(file.try_clone()?, header_len)?;
        let (published, len) = root.read(&header, || Ok(file.len()?))?;
        let physical = header.with_len(len)?;
        let mut position = StreamPosition::initial(spec);
        let start = match published {
            StreamRoot::Published(end) => {
                let from = position.resume_from_anchor(&file, header_len, end)?;
                // Durable: a record before it that does not frame is damage,
                // not a torn tail.
                let reached = frame_until(spec, &physical, from, end, &mut position)?;
                if reached != end {
                    return Err(Error::CorruptTail { offset: reached });
                }
                end
            }
            StreamRoot::Scan { takeover } => {
                scan_root(spec, &physical, header_len, takeover, &mut position)?
            }
        };
        let mut tail = crate::sync_marker::MarkedTail::new(start);
        tail.advance(spec, &physical)?;
        let kept = tail.committed();
        let reached = frame_until(spec, &physical, start, kept, &mut position)?;
        if reached != kept {
            return Err(Error::CorruptTail { offset: reached });
        }
        crate::publication_protocol::restore(
            &mut file,
            |file| -> Result<()> {
                if len == kept {
                    return Ok(());
                }
                crate::scalable_fault_point("restore.native_truncate");
                let truncate = file.set_len(kept);
                crate::scalable_fault_point("restore.native_truncate");
                truncate?;
                Ok(())
            },
            |file| -> Result<()> {
                if len == start && kept == start {
                    return Ok(());
                }
                crate::scalable_fault_point("restore.native_sync");
                let sync = file.sync_all();
                crate::scalable_fault_point("restore.native_sync");
                sync?;
                Ok(())
            },
            // Published once durable, and made durable itself by the next
            // barrier. A crash before then leaves the earlier end, which
            // only lengthens the next open's walk.
            |file| file.write_published_end(Some(kept)),
        )?;
        file.seek(SeekFrom::Start(kept))?;
        let snapshot = SnapshotFile::from_file_with_len(file.try_clone()?, kept)?;
        let mut writer = Self {
            spec,
            file,
            snapshot,
            sequence: position.sequence,
            record_count: position.record_count,
            block_tails: position.block_tails,
            poison: PoisonFlag::healthy(),
            record_buffer: Vec::new(),
            immediate_state: Default::default(),
            sync_chain: None,
            _lock: lock,
        };
        writer.begin_sync_chain();
        Ok(writer)
    }

    pub fn push_info<T: VarlitBlock>(&mut self, value: &T) -> Result<AppendInfo> {
        if self.spec.index_policy.keyed_offset_chain && T::IS_KEYED {
            return Err(Error::StreamingUnsupported);
        }
        self.push_with_prev_key_info(value, None)
    }

    pub fn push_iter<T, I>(
        &mut self,
        values: I,
        options: BatchOptions,
    ) -> std::result::Result<BatchAppendInfo, BatchAppendError>
    where
        T: VarlitBlock,
        I: IntoIterator,
        I::Item: Borrow<T>,
    {
        let options = options.validate().map_err(|source| BatchAppendError {
            written: BatchAppendInfo {
                start_offset: self.snapshot.len(),
                end_offset: self.snapshot.len(),
                ..BatchAppendInfo::default()
            },
            source,
        })?;
        let start_offset = self.snapshot.len();
        let mut written = BatchAppendInfo {
            start_offset,
            end_offset: start_offset,
            ..BatchAppendInfo::default()
        };
        if let Err(source) = self.push_iter_inner::<T, I>(values, options, &mut written) {
            if written.records != 0 {
                self.poison.poison();
            }
            return Err(BatchAppendError { written, source });
        }
        Ok(written)
    }

    pub fn push_with_prev_key_info<T: VarlitBlock>(
        &mut self,
        value: &T,
        previous: Option<u64>,
    ) -> Result<AppendInfo> {
        self.check_mutation(STREAM_WRITER_POISON_CONTEXT)?;
        ensure_registered_block::<T>(self.spec)?;
        if self.spec.index_policy.keyed_offset_chain && T::IS_KEYED {
            return Err(Error::StreamingUnsupported);
        }
        if T::ID >= 0xFFFF_FF00 {
            return Err(Error::ReservedBlockId(T::ID));
        }
        if T::KIND == BlockKind::Matrix {
            return Err(Error::StreamingUnsupported);
        }
        let mandatory = T::IMMEDIATE || value.immediate_if();
        let sequence = self.next_sequence()?;
        let offset = self.snapshot.len();
        let previous_block = self.previous_block(T::ID);
        let mut buffer = std::mem::take(&mut self.record_buffer);
        buffer.clear();
        let prepared = prepare_stream_user_record_into(
            self.spec,
            value,
            sequence,
            offset,
            previous_block,
            previous,
            &mut buffer,
        );
        // Taken out and put back on every path, including the error one: the
        // buffer belongs to the writer, and a failed encode must not cost it.
        let result = match prepared {
            Ok(parts) => {
                let info = parts.info;
                self.ensure_writable()?
                    .append_prepared_chunk(&buffer, &[(T::ID, info)])
                    .map(|()| info)
            }
            Err(err) => Err(err),
        };
        self.record_buffer = buffer;
        let info = result?;
        self.apply_immediate::<T>(info, crate::ImmediateOperation::Append, mandatory)?;
        Ok(info)
    }

    pub fn delete_with_prev_key_info<T: VarlitKeyedBlock>(
        &mut self,
        key: &T::Key,
        previous: Option<u64>,
    ) -> Result<AppendInfo> {
        // every public keyed generic entry point evaluates the
        // compile-time keyedness contract post-monomorphization; the
        // registration below stays as the runtime backstop.
        let () = KeyedBlockContract::<T>::OK;
        self.check_mutation(STREAM_WRITER_POISON_CONTEXT)?;
        ensure_registered_block::<T>(self.spec)?;
        if self.spec.index_policy.keyed_offset_chain {
            return Err(Error::StreamingUnsupported);
        }
        let sequence = self.next_sequence()?;
        let offset = self.snapshot.len();
        let record = prepare_stream_tombstone_record::<T>(
            self.spec,
            key,
            sequence,
            offset,
            self.previous_block(TOMBSTONE_BLOCK_ID),
            previous,
        )?;
        let info = self
            .ensure_writable()?
            .append_prepared(TOMBSTONE_BLOCK_ID, record)?;
        self.apply_immediate::<T>(info, crate::ImmediateOperation::Delete, T::IMMEDIATE)?;
        Ok(info)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.check_mutation(STREAM_WRITER_POISON_CONTEXT)?;
        self.file.flush()?;
        Ok(())
    }

    /// Makes every preceding record durable and visible to readers: a sync
    /// marker closes the records since the last sync, one storage barrier
    /// makes them durable, and the end is published in the file's `VPUB`
    /// block.
    pub fn sync(&mut self) -> Result<()> {
        self.sync_with()
    }

    /// Syncs through the verified commit driver: readiness and the sync
    /// marker, the primary barrier, the publication, then the bookkeeping.
    fn sync_with(&mut self) -> Result<()> {
        use crate::publication_protocol::CommitStep;
        crate::publication_protocol::commit(|step| {
            match step {
                CommitStep::Prepare => {
                    self.check_mutation(STREAM_WRITER_POISON_CONTEXT)?;
                    self.append_sync_marker()?;
                }
                CommitStep::PrimarySync => {
                    crate::scalable_fault_point("sync.native_sync");
                    let sync = self.file.sync_all();
                    crate::scalable_fault_point("sync.native_sync");
                    sync?;
                }
                CommitStep::Publish => {
                    // The end of this sync, after its barrier: the durable
                    // boundary readers and the next open start from. One
                    // 24-byte header write, made durable by the next barrier;
                    // until then a crash leaves the previous slot, an earlier
                    // boundary, which only lengthens the walk. Best-effort for
                    // the same reason.
                    let _ = self.file.write_published_end(Some(self.snapshot.len()));
                }
                CommitStep::Poison => self.poison.poison(),
                CommitStep::Finish => self.immediate_state.reset(),
            }
            Ok(())
        })
    }

    /// Persist all preceding records and publish their end.
    pub fn immediate(&mut self) -> Result<()> {
        self.sync()
    }

    pub fn set_immediate_policy(&mut self, policy: crate::ImmediatePolicy) -> Result<()> {
        let _permit = self.ensure_writable()?;
        self.immediate_state.policy = policy.validate()?;
        Ok(())
    }

    fn apply_immediate<T: VarlitBlock>(
        &mut self,
        info: AppendInfo,
        operation: crate::ImmediateOperation,
        mandatory: bool,
    ) -> Result<()> {
        let bytes = self.snapshot.len().saturating_sub(info.record_offset);
        let event = self
            .immediate_state
            .event(T::ID, operation, info, bytes, 0, 0);
        if mandatory || self.immediate_state.policy.matches(event) {
            self.immediate().map_err(|source| {
                crate::immediate::appended_immediate_error(info.sequence, source)
            })?;
        }
        Ok(())
    }

    pub fn resident_state(&self) -> StreamResidentState {
        resident_state(self.spec)
    }

    fn push_iter_inner<T, I>(
        &mut self,
        values: I,
        options: BatchOptions,
        written: &mut BatchAppendInfo,
    ) -> Result<()>
    where
        T: VarlitBlock,
        I: IntoIterator,
        I::Item: Borrow<T>,
    {
        let _permit = self.ensure_writable()?;
        ensure_registered_block::<T>(self.spec)?;
        if T::ID >= 0xFFFF_FF00 {
            return Err(Error::ReservedBlockId(T::ID));
        }
        if T::KIND == BlockKind::Matrix
            || (self.spec.index_policy.keyed_offset_chain && T::IS_KEYED)
        {
            return Err(Error::StreamingUnsupported);
        }

        let max_records = options.max_records;
        let mut bytes = Vec::new();
        let mut records: Vec<(u32, AppendInfo)> = Vec::new();
        let mut next_sequence = self.sequence;
        let mut next_offset = self.snapshot.len();
        let mut previous_block = self.previous_block(T::ID);

        for value in values {
            let value = value.borrow();
            let mandatory = T::IMMEDIATE || value.immediate_if();
            let sequence = next_sequence.ok_or(Error::SequenceExhausted)?;
            // Straight into the chunk buffer. This used to encode into a
            // per-record `Vec`, copy it in here and drop it, so a batch of N
            // records took N allocations and N payload copies it did not need.
            let before = bytes.len();
            let parts = prepare_stream_user_record_into(
                self.spec,
                value,
                sequence,
                next_offset,
                previous_block,
                None,
                &mut bytes,
            )?;
            let exceeds_records = records.len() >= max_records;
            let exceeds_bytes = before != 0 && bytes.len() > options.max_bytes;
            if exceeds_records || exceeds_bytes {
                // Flush what preceded this record and keep this one as the head
                // of the next chunk. Its offset is still right: the flush
                // advances the native eof to exactly `next_offset`, which is
                // what it was prepared against.
                //
                // The move is one record wide, and it is the price of staging
                // in place — the alternative is knowing the encoded length
                // before encoding, which is what the per-record `Vec` was
                // really buying.
                self.check_mutation(STREAM_WRITER_POISON_CONTEXT)?;
                self.ensure_writable()?.append_prepared_chunk_summarized(
                    &bytes[..before],
                    &records,
                    written,
                )?;
                records.clear();
                bytes.copy_within(before.., 0);
                bytes.truncate(parts.len);
            }

            next_offset = next_offset.checked_add(parts.len as u64).ok_or(
                Error::ResourceArithmeticOverflow {
                    resource: "batch native offset",
                },
            )?;
            next_sequence = sequence.checked_add(1);
            previous_block = Some(parts.info.record_offset);
            records
                .try_reserve(1)
                .map_err(|_| Error::AllocationFailed {
                    resource: "batch record descriptors",
                    requested: std::mem::size_of::<(u32, AppendInfo)>() as u64,
                })?;
            records.push((T::ID, parts.info));

            let event = self.immediate_state.event(
                T::ID,
                crate::ImmediateOperation::Append,
                parts.info,
                parts.len as u64,
                records.len() as u64,
                bytes.len() as u64,
            );
            let immediate = mandatory || self.immediate_state.policy.matches(event);
            if immediate || bytes.len() >= options.max_bytes || records.len() >= max_records {
                self.check_mutation(STREAM_WRITER_POISON_CONTEXT)?;
                self.ensure_writable()?
                    .append_prepared_chunk_summarized(&bytes, &records, written)?;
                bytes.clear();
                records.clear();
                if immediate {
                    self.immediate().map_err(|source| {
                        crate::immediate::appended_immediate_error(sequence, source)
                    })?;
                    // The sync appended a marker: continue after it.
                    next_offset = self.snapshot.len();
                    next_sequence = self.sequence;
                }
            }
        }
        if !records.is_empty() {
            self.check_mutation(STREAM_WRITER_POISON_CONTEXT)?;
            self.ensure_writable()?
                .append_prepared_chunk_summarized(&bytes, &records, written)?;
        }
        Ok(())
    }

    /// Arms `count` injected native-append failures whose rollback also fails
    /// (round 12, ).
    ///
    /// Fault-testing hook only. Each armed failure makes the next prepared
    /// chunk append behave as
    /// though the native write failed *and* the truncate-or-seek that rolls it
    /// back failed too. That pair is the one path into
    /// `Error::WriteRollbackFailed`; the writer must poison itself so that the
    /// next single-record put or delete is refused.
    ///
    /// The observable contract under this fault is that the call returns
    /// `Error::WriteRollbackFailed` and that *every* later mutation of either
    /// writer - single-record put, single-record delete, and batch - is
    /// refused with `Error::WriterPoisoned`. Nothing is written and nothing is
    /// truncated, so the file is left exactly as a real rolled-back write
    /// leaves it. Thread-local.
    #[cfg(feature = "scalable-fault-injection")]
    #[doc(hidden)]
    pub fn inject_append_rollback_failures(count: u64) {
        INJECTED_APPEND_ROLLBACK_FAILURES.with(|armed| armed.set(count));
    }

    /// Checks the poison flag and issues the witness every mutating entry
    /// point demands. See [`writer_permit`].
    fn ensure_writable(&mut self) -> Result<StreamMutationPermit<'_>> {
        self.writer_permit(STREAM_WRITER_POISON_CONTEXT)
    }

    /// Opens the sync-marker chain at the current EOF, a confirmed end.
    pub(crate) fn begin_sync_chain(&mut self) {
        self.sync_chain = Some(crate::sync_marker::SyncChain::new(self.snapshot.len()));
    }

    /// Closes the open chain link with a sync marker, if the link is not
    /// empty. The marker is an ordinary append.
    pub(crate) fn append_sync_marker(&mut self) -> Result<Option<AppendInfo>> {
        self.check_mutation(STREAM_WRITER_POISON_CONTEXT)?;
        let Some(chain) = self.sync_chain.as_ref() else {
            return Ok(None);
        };
        let offset = self.snapshot.len();
        if chain.start() == offset {
            return Ok(None);
        }
        let payload = chain.payload(offset)?;
        let sequence = self.next_sequence()?;
        let record =
            crate::file::prepare_stream_sync_record(self.spec, &payload, sequence, offset)?;
        let info = self
            .ensure_writable()?
            .append_prepared(crate::sync_marker::SYNC_BLOCK_ID, record)?;
        self.sync_chain = Some(crate::sync_marker::SyncChain::new(self.snapshot.len()));
        Ok(Some(info))
    }

    pub(crate) fn next_sequence(&self) -> Result<u64> {
        self.sequence.ok_or(Error::SequenceExhausted)
    }

    pub(crate) fn previous_block(&self, block_id: u32) -> Option<u64> {
        if !self.spec.index_policy.block_offset_chain {
            return None;
        }
        // `block_tails` is sorted by block id; see `initial_block_tails`.
        self.block_tails
            .binary_search_by_key(&block_id, |(candidate, _)| *candidate)
            .ok()
            .and_then(|index| {
                self.block_tails[index]
                    .1
                    .as_ref()
                    .map(|tail| tail.record_offset)
            })
    }
}

/// A plain stream has no matrix engine and no keyed-storage heap, so the
/// public stream entry points refuse a format declaring matrix blocks or
/// keyed storage. A stream writer would otherwise append keyed blocks as
/// ordinary records and leave a file whose keyed storage cannot be opened.
fn refuse_matrix_stream(spec: FormatSpec) -> Result<()> {
    if spec.has_matrix_blocks() || spec.keyed_storage.is_some() {
        return Err(Error::StreamingUnsupported);
    }
    Ok(())
}

/// Lets a stream writer's file handle publish into the file's `VPUB` block,
/// when it has one (every stream-created file does; older ones do not).
fn adopt_published_end(file: &mut PrimaryFile, header_len: u64, extensions: &[u8]) -> Result<()> {
    if let Some(slots_at) = crate::file::published_end_slots_at(header_len, extensions)? {
        file.adopt_writer_published(slots_at)?;
    }
    Ok(())
}

fn stream_spec(spec: FormatSpec, options: StreamOptions, create: bool) -> Result<FormatSpec> {
    let spec = spec
        .with_resource_limits(options.limits)
        .resolve_entrypoint();
    spec.validate()?;
    let _ = create;
    if spec.commit_policy.is_transaction_marker()
        || spec.index_policy.checkpoint_on_flush
        || spec.layout.preset != LayoutPreset::VarlitNative
    {
        return Err(Error::StreamingUnsupported);
    }
    if !matches!(
        spec.commit_policy,
        CommitPolicy::None | CommitPolicy::RecordFooter
    ) {
        return Err(Error::StreamingUnsupported);
    }
    Ok(spec)
}

/// Derives the batch summary that *will* describe `summary` once `records` are
/// published, without touching `summary`.
///
/// This is deliberately a pure function rather than an in-place update: every
/// fallible step (all of it checked arithmetic) has to run *before* the native
/// write, so that the only thing left after publication is an infallible
/// assignment. An in-place update run after the write could fail with the
/// records already durable, and the caller would then be handed a summary that
/// under-reports what is in the file - which is exactly what
/// `BatchAppendError::written` is relied on to report accurately.
pub(crate) fn next_batch_summary(
    summary: &BatchAppendInfo,
    records: &[(u32, AppendInfo)],
    end_offset: u64,
) -> Result<BatchAppendInfo> {
    let first = records.first().expect("non-empty prepared chunk").1;
    let last = records.last().expect("non-empty prepared chunk").1;
    let mut next = *summary;
    next.first_sequence.get_or_insert(first.sequence);
    next.last_sequence = Some(last.sequence);
    next.records = next
        .records
        .checked_add(u64::try_from(records.len()).map_err(|_| {
            Error::ResourceArithmeticOverflow {
                resource: "batch record count",
            }
        })?)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "batch record count",
        })?;
    next.end_offset = end_offset;
    next.write_calls =
        next.write_calls
            .checked_add(1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "batch write call count",
            })?;
    Ok(next)
}

fn initial_block_tails(spec: FormatSpec) -> Vec<(u32, Option<StreamTail>)> {
    if !spec.index_policy.block_offset_chain {
        return Vec::new();
    }
    // Allocated once at open and uncharged: both terms are compile-time shape
    // -- the declared block count and a fixed array of internal ids -- so no
    // file content can size this.
    let mut tails = Vec::with_capacity(spec.blocks.len() + INTERNAL_BLOCK_IDS.len());
    tails.extend(spec.blocks.iter().map(|block| (block.id, None)));
    tails.extend(INTERNAL_BLOCK_IDS.map(|block_id| (block_id, None)));
    tails.sort_unstable_by_key(|(block_id, _)| *block_id);
    tails
}

fn set_tail(tails: &mut [(u32, Option<StreamTail>)], block_id: u32, offset: u64, sequence: u64) {
    // `tails` is sorted by block id; see `initial_block_tails`.
    if let Ok(index) = tails.binary_search_by_key(&block_id, |(candidate, _)| *candidate) {
        tails[index].1 = Some(StreamTail {
            block_id,
            record_offset: offset,
            sequence,
        });
    }
}

fn resident_state(spec: FormatSpec) -> StreamResidentState {
    StreamResidentState {
        declared_block_tails: if spec.index_policy.block_offset_chain {
            spec.blocks.len()
        } else {
            0
        },
        retained_record_entries: 0,
        retained_key_entries: 0,
        active_payload_bytes: 0,
    }
}

fn canonical_write_path(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(std::fs::canonicalize(path)?);
    }
    let parent = std::fs::canonicalize(
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?;
    let name = path
        .file_name()
        .ok_or(Error::InvalidFormatSpec("file path has no name"))?;
    Ok(parent.join(name))
}

/// Where a stream file's durable boundary is read from.
#[derive(Clone, Copy, Debug)]
struct RootSource {
    header_len: u64,
    /// The file's `VPUB` slots; `None` in a file written before the block
    /// existed.
    slots_at: Option<u64>,
}

/// What a stream file's `VPUB` block says about its durable boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamRoot {
    /// The end of the newest sync the writer published.
    Published(u64),
    /// No end is published, so the boundary is found by framing.
    /// `takeover` when a native writer withdrew the end a stream published.
    Scan { takeover: bool },
}

impl RootSource {
    /// Reads both slots once, through any snapshot of the file (the header
    /// precedes every record). A slot a writer is rewriting fails its
    /// checksum and the other one holds, so a concurrent read sees the
    /// previous end at worst.
    ///
    /// `len` is the file's length measured *after* this read: the writer
    /// publishes an end only once the file reaches it, so a later
    /// measurement reaches it too, and one that does not means records were
    /// lost.
    fn read(
        &self,
        snapshot: &SnapshotFile,
        len: impl FnOnce() -> Result<u64>,
    ) -> Result<(StreamRoot, u64)> {
        use crate::published_end::{ACTIVE, SLOT_LEN, decode_slot};
        let Some(slots_at) = self.slots_at else {
            return Ok((StreamRoot::Scan { takeover: false }, len()?));
        };
        let mut slots = [0u8; 2 * SLOT_LEN];
        snapshot.read_exact_at(slots_at, &mut slots)?;
        let len = len()?;
        let mut newest: Option<crate::published_end::Slot> = None;
        for bytes in slots.as_chunks::<SLOT_LEN>().0 {
            if let Some(slot) = decode_slot(bytes)
                && newest.is_none_or(|held| slot.seq > held.seq)
            {
                newest = Some(slot);
            }
        }
        let root = match newest {
            Some(slot) if slot.flags & ACTIVE != 0 => {
                // A published end is durable: a file that no longer reaches
                // it lost acknowledged records.
                if slot.end < self.header_len || slot.end > len {
                    return Err(Error::CorruptTail {
                        offset: slot.end.min(len),
                    });
                }
                StreamRoot::Published(slot.end)
            }
            Some(_) => StreamRoot::Scan { takeover: true },
            None => StreamRoot::Scan { takeover: false },
        };
        Ok((root, len))
    }
}

/// What framing a stream's records keeps track of on the way to its
/// boundary.
trait Track {
    fn see(&mut self, entry: &crate::RecordIndexEntry) -> Result<()>;
    /// Forgets everything seen, for a walk that starts over from the header.
    fn forget(&mut self);
}

/// A reader's walk: only where it ends matters.
struct NoPosition;

impl Track for NoPosition {
    fn see(&mut self, _entry: &crate::RecordIndexEntry) -> Result<()> {
        Ok(())
    }
    fn forget(&mut self) {}
}

/// A follow's walk: how many records it adopts.
struct CountPosition(u64);

impl Track for CountPosition {
    fn see(&mut self, _entry: &crate::RecordIndexEntry) -> Result<()> {
        self.0 += 1;
        Ok(())
    }
    fn forget(&mut self) {
        self.0 = 0;
    }
}

/// A writer's walk: the state it appends from.
struct StreamPosition {
    record_count: u64,
    sequence: Option<u64>,
    block_tails: Vec<(u32, Option<StreamTail>)>,
}

impl StreamPosition {
    fn initial(spec: FormatSpec) -> Self {
        Self {
            record_count: 0,
            sequence: Some(0),
            block_tails: initial_block_tails(spec),
        }
    }

    /// Takes the stream summary of the newest intact anchor before `root`
    /// and returns where framing resumes: that anchor's spanning record, or
    /// the header when no anchor before `root` carries one. Looks at a few
    /// anchors at most; the framing after the one it takes covers the rest.
    fn resume_from_anchor(
        &mut self,
        file: &PrimaryFile,
        header_len: u64,
        root: u64,
    ) -> Result<u64> {
        const TRIES: u32 = 4;
        let Some(layout) = file.layout() else {
            return Ok(header_len);
        };
        let mut bytes = [0u8; 4096];
        let mut k = layout.anchors_before(root);
        for _ in 0..TRIES {
            if k == 0 {
                break;
            }
            if let Some((anchor, len)) = file.anchor_into(k, &mut bytes)?
                && anchor.record_start >= header_len
                && anchor.record_end <= root
            {
                self.forget();
                let mut fits = true;
                let tails = &mut self.block_tails;
                let summary = crate::anchor::StreamSummary::decode(
                    &bytes[..len],
                    header_len,
                    anchor.record_start,
                    |block_id, record_offset, sequence| match tails
                        .binary_search_by_key(&block_id, |(id, _)| *id)
                    {
                        Ok(index) => {
                            tails[index].1 = Some(StreamTail {
                                block_id,
                                record_offset,
                                sequence,
                            });
                        }
                        Err(_) => fits = false,
                    },
                );
                if let Some(summary) = summary
                    && fits
                    && summary.next_sequence == anchor.sequence
                {
                    self.record_count = summary.records_before;
                    self.sequence = Some(summary.next_sequence);
                    return Ok(anchor.record_start);
                }
            }
            k -= 1;
        }
        self.forget();
        Ok(header_len)
    }
}

impl Track for StreamPosition {
    fn see(&mut self, entry: &crate::RecordIndexEntry) -> Result<()> {
        if self.sequence.is_none_or(|next| entry.sequence < next) {
            return Err(Error::CorruptTail {
                offset: entry.record_offset,
            });
        }
        self.sequence = entry.sequence.checked_add(1);
        self.record_count =
            self.record_count
                .checked_add(1)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "record count",
                })?;
        if entry.block_id != crate::sync_marker::SYNC_BLOCK_ID {
            set_tail(
                &mut self.block_tails,
                entry.block_id,
                entry.record_offset,
                entry.sequence,
            );
        }
        Ok(())
    }

    fn forget(&mut self) {
        self.record_count = 0;
        self.sequence = Some(0);
        for (_, tail) in &mut self.block_tails {
            *tail = None;
        }
    }
}

/// Frames the records in `[from, until)`, handing each to `track`, and
/// returns the end of the last one: `until`, unless a record before it does
/// not frame. Storage errors are returned; a record that does not frame
/// ends the walk.
fn frame_until(
    spec: FormatSpec,
    physical: &SnapshotFile,
    from: u64,
    until: u64,
    track: &mut impl Track,
) -> Result<u64> {
    if from >= until {
        return Ok(from);
    }
    let mut scanner =
        NativeStreamScanner::from_offset_framing_only(spec, physical.with_len(until)?, from)?;
    let mut end = from;
    loop {
        let entry = match scanner.next_entry() {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(Error::Io(error)) => return Err(Error::Io(error)),
            Err(_) => break,
        };
        track.see(&entry)?;
        end = entry.checked_physical_end()?;
    }
    Ok(end)
}

/// Finds the boundary of a file that publishes none, framing from its
/// header and handing every record before it to `track`.
///
/// A file a native writer took over has no markers past its stream part to
/// verify: every record that frames is kept. Otherwise the file predates the
/// published end, and its sync chain began where its state sidecar left off,
/// at the start of the first marker's link, which is where the walk verifies
/// from. A file with no marker, or whose first link does not start on a
/// record boundary, keeps every record that frames.
fn scan_root(
    spec: FormatSpec,
    physical: &SnapshotFile,
    header_len: u64,
    takeover: bool,
    track: &mut impl Track,
) -> Result<u64> {
    if !takeover && let Some(start) = first_link_start(spec, physical, header_len)? {
        if frame_until(spec, physical, header_len, start, track)? == start {
            return Ok(start);
        }
        track.forget();
    }
    frame_until(spec, physical, header_len, physical.len(), track)
}

/// The start of the link the file's first sync marker closes.
fn first_link_start(
    spec: FormatSpec,
    physical: &SnapshotFile,
    header_len: u64,
) -> Result<Option<u64>> {
    use crate::sync_marker::{SYNC_BLOCK_ID, SYNC_PAYLOAD_LEN};
    if header_len >= physical.len() {
        return Ok(None);
    }
    let mut scanner =
        NativeStreamScanner::from_offset_framing_only(spec, physical.clone(), header_len)?;
    loop {
        let entry = match scanner.next_entry() {
            Ok(Some(entry)) => entry,
            Ok(None) => return Ok(None),
            Err(Error::Io(error)) => return Err(Error::Io(error)),
            Err(_) => return Ok(None),
        };
        if entry.block_id != SYNC_BLOCK_ID {
            continue;
        }
        if entry.payload_len != SYNC_PAYLOAD_LEN as u64 {
            return Ok(None);
        }
        let mut payload = [0u8; SYNC_PAYLOAD_LEN];
        physical.read_exact_at(entry.payload_offset, &mut payload)?;
        return Ok(crate::sync_marker::decode(&payload)
            .map(|marker| marker.start)
            .filter(|start| (header_len..=entry.record_offset).contains(start)));
    }
}

/// The anchor a prepared chunk reaches: the record in it that spans the
/// anchor's logical offset, and the stream summary of the state before that
/// record (what a stream open resumes from).
struct ChunkAnchors<'a> {
    records: &'a [(u32, AppendInfo)],
    end: u64,
    /// The writer's tails and record count before this chunk.
    tails: &'a [(u32, Option<StreamTail>)],
    record_count: u64,
    /// The index in `records` of the record spanning the last anchor asked
    /// for: what [`Self::summary`] describes.
    spanning: Option<usize>,
}

impl crate::primary_file::AnchorSource for ChunkAnchors<'_> {
    fn anchor(&mut self, k: u64, logical: u64) -> Result<crate::anchor::Anchor> {
        let index = self
            .records
            .partition_point(|(_, info)| info.record_offset <= logical)
            .checked_sub(1)
            .ok_or(Error::InvalidCanonicalEncoding(
                "anchor precedes the chunk being written",
            ))?;
        let info = self.records[index].1;
        let record_end = self
            .records
            .get(index + 1)
            .map_or(self.end, |(_, next)| next.record_offset);
        self.spanning = Some(index);
        Ok(crate::anchor::Anchor {
            k,
            logical,
            record_start: info.record_offset,
            record_end,
            sequence: info.sequence,
        })
    }

    fn summary(&self) -> Option<&dyn crate::anchor::SummarySource> {
        self.spanning
            .map(|_| self as &dyn crate::anchor::SummarySource)
    }
}

impl crate::anchor::SummarySource for ChunkAnchors<'_> {
    /// The state before the spanning record: the writer's state before the
    /// chunk, advanced by the chunk's records before it. Computed from the
    /// borrowed slices, with nothing allocated.
    fn write_summary(&self, out: &mut [u8]) -> bool {
        let Some(index) = self.spanning else {
            return false;
        };
        let before = &self.records[..index];
        let tails = self.tails.iter().filter_map(|(block_id, base)| {
            before
                .iter()
                .rev()
                .find(|(id, _)| id == block_id)
                .map(|(_, info)| (*block_id, info.record_offset, info.sequence))
                .or_else(|| {
                    base.as_ref()
                        .map(|tail| (*block_id, tail.record_offset, tail.sequence))
                })
        });
        // `filter_map` loses the exact size the encoder bounds by; count once.
        let count = self
            .tails
            .iter()
            .filter(|(block_id, base)| {
                base.is_some() || before.iter().any(|(id, _)| id == block_id)
            })
            .count();
        crate::anchor::encode_stream_summary(
            out,
            self.record_count + index as u64,
            self.records[index].1.sequence,
            ExactCount {
                inner: tails,
                left: count,
            },
        )
    }
}

/// An iterator whose length was counted beforehand.
struct ExactCount<I> {
    inner: I,
    left: usize,
}

impl<I: Iterator> Iterator for ExactCount<I> {
    type Item = I::Item;
    fn next(&mut self) -> Option<Self::Item> {
        let item = self.inner.next()?;
        self.left = self.left.saturating_sub(1);
        Some(item)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl<I: Iterator> ExactSizeIterator for ExactCount<I> {}

pub(crate) const fn silent_scan_options() -> ScanOptions<'static> {
    ScanOptions {
        progress: ScanProgressOptions {
            every_records: None,
            every_bytes: None,
        },
        cancellation: None,
    }
}

pub(crate) const fn scan_cancelled(cancelled: ScanCancelled) -> Error {
    Error::ScanCancelled {
        progress: cancelled.progress,
    }
}

impl StreamMutationPermit<'_> {
    /// Publishes a prepared chunk and folds it into `written`.
    ///
    /// The summary arithmetic runs *before* the native write and the only step
    /// after publication is an infallible assignment, so `written` describes
    /// every record that reached the file on every path out of this call.
    ///
    /// `permit` is consumed: this is a mutating entry point reachable from
    /// another module, so the poison check ran for *this* chunk. See
    /// [`writer_permit`].
    pub(crate) fn append_prepared_chunk_summarized(
        &mut self,
        bytes: &[u8],
        records: &[(u32, AppendInfo)],
        written: &mut BatchAppendInfo,
    ) -> Result<()> {
        self.check()?;
        if records.is_empty() {
            return Ok(());
        }
        let byte_len =
            u64::try_from(bytes.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        // `append_prepared_chunk` requires the chunk to begin at the native
        // EOF and lands it contiguously, so this is the post-write EOF.
        let end_offset =
            self.snapshot
                .len()
                .checked_add(byte_len)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "file length",
                })?;
        let next = next_batch_summary(written, records, end_offset)?;
        self.append_prepared_chunk(bytes, records)?;
        *written = next;
        Ok(())
    }
    fn append_prepared(
        &mut self,
        block_id: u32,
        record: PreparedStreamRecord,
    ) -> Result<AppendInfo> {
        self.check()?;
        let info = record.info;
        self.append_prepared_chunk(&record.bytes, &[(block_id, info)])?;
        Ok(info)
    }
    /// Appends a prepared chunk at the native EOF, which is the authoritative
    /// commit point for every record it names.
    ///
    /// `permit` is consumed rather than borrowed: this method is the mutating
    /// entry point `indexed.rs` reaches for single-record puts and deletes, and
    ///  was that it performed the mutation without consulting the stream's
    /// poison flag at all. The witness cannot be produced without that check.
    /// See [`writer_permit`].
    pub(crate) fn append_prepared_chunk(
        &mut self,
        bytes: &[u8],
        records: &[(u32, AppendInfo)],
    ) -> Result<()> {
        self.check()?;
        if records.is_empty() {
            return Ok(());
        }
        let record_delta =
            u64::try_from(records.len()).map_err(|_| Error::ResourceArithmeticOverflow {
                resource: "record count",
            })?;
        let next_count = self.record_count.checked_add(record_delta).ok_or(
            Error::ResourceArithmeticOverflow {
                resource: "record count",
            },
        )?;
        let eof = self.snapshot.len();
        if records[0].1.record_offset != eof {
            return Err(Error::InvalidCanonicalEncoding(
                "prepared batch does not begin at native eof",
            ));
        }
        let byte_len =
            u64::try_from(bytes.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
        let new_eof = eof
            .checked_add(byte_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "file length",
            })?;
        // Not shadowed, and that is a measurement rather than an oversight.
        // Skipping this seek on the reasoning that the previous write left the
        // offset at `eof` was wrong on every append of a 200-record run, by
        // exactly 16 bytes each time: this handle is not the only writer to the
        // native file, and the other one does not move this handle's offset.
        // The records landed 16 bytes early and the file came out short.
        // Round 12, : an armed fault makes the write fail *and* its
        // rollback fail, which is the pair that poisons the writer below.
        // Nothing is written and nothing is truncated, so the file is exactly
        // what a real rolled-back write leaves behind.
        let injected_rollback_failure = take_injected_append_rollback_failure();
        let write = if injected_rollback_failure {
            Err(std::io::Error::other(
                "injected native append chunk write failure",
            ))
        } else {
            crate::scalable_fault_point("append.native_chunk_write");
            // Positional and logical: an anchor the chunk reaches is written
            // around it, describing the record that spans it.
            let write = self
                .file
                .write_all_at(
                    eof,
                    bytes,
                    &mut ChunkAnchors {
                        records,
                        end: new_eof,
                        tails: &self.block_tails,
                        record_count: self.record_count,
                        spanning: None,
                    },
                )
                .map_err(crate::Error::into_io);
            crate::scalable_fault_point("append.native_chunk_write");
            write
        };
        if let Err(error) = write {
            let (truncate_result, seek_result) = if injected_rollback_failure {
                (
                    Err(std::io::Error::other(
                        "injected native append rollback truncate failure",
                    )),
                    Err(std::io::Error::other(
                        "injected native append rollback seek failure",
                    )),
                )
            } else {
                (self.file.set_len(eof), Ok(eof))
            };
            if let Some(source) = truncate_result.err().or_else(|| seek_result.err()) {
                self.poison.poison();
                return Err(Error::WriteRollbackFailed {
                    operation: "append stream record",
                    source,
                });
            }
            return Err(error.into());
        }
        // The write returned `Ok`, which is proof the file reaches `new_eof` —
        // the fact `with_len`'s `fstat` was issued to learn.
        self.snapshot = match self
            .snapshot
            .with_written_len(WrittenThrough::after_write(new_eof))
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let truncate_result = self.file.set_len(eof);
                if let Some(source) = truncate_result.err() {
                    self.poison.poison();
                    return Err(Error::WriteRollbackFailed {
                        operation: "append stream record",
                        source,
                    });
                }
                return Err(error);
            }
        };
        if let Some(chain) = self.sync_chain.as_mut() {
            chain.extend_chunk(eof, bytes, records);
        }
        let last_sequence = records.last().expect("non-empty prepared chunk").1.sequence;
        self.sequence = last_sequence.checked_add(1);
        self.record_count = next_count;
        for (block_id, info) in records {
            set_tail(
                &mut self.block_tails,
                *block_id,
                info.record_offset,
                info.sequence,
            );
        }
        self.immediate_state.advance(records.len() as u64, byte_len);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    // The owning form exists for these tests and the in-crate probe; every
    // production path stages through the `_into` form.
    use crate::file::prepare_stream_user_record;
    use crate::{
        BlockDescriptor, Decoder, Encoder, Endian, IndexPolicy, IntegrityPolicy, ReadLimits,
        RecoveryPolicy, TransactionMarkerMode, VarlitDecode, VarlitEncode, WireType,
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestBlock(u64);

    impl VarlitEncode for TestBlock {
        const WIRE_TYPE: WireType = WireType::U64;

        fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
            self.0.encode_varlit(encoder)
        }
    }

    impl VarlitDecode for TestBlock {
        const WIRE_TYPE: WireType = WireType::U64;

        fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
            Ok(Self(u64::decode_varlit(decoder)?))
        }
    }

    impl VarlitBlock for TestBlock {
        const ID: u32 = 17;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = false;
        const SCHEMA_FINGERPRINT: u64 = 0x5354_5245_414D_0017;
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct KeyedTestBlock(u64);

    impl VarlitEncode for KeyedTestBlock {
        const WIRE_TYPE: WireType = WireType::U64;

        fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
            self.0.encode_varlit(encoder)
        }
    }

    impl VarlitDecode for KeyedTestBlock {
        const WIRE_TYPE: WireType = WireType::U64;

        fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
            Ok(Self(u64::decode_varlit(decoder)?))
        }
    }

    impl VarlitBlock for KeyedTestBlock {
        const ID: u32 = 18;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = true;
        const SCHEMA_FINGERPRINT: u64 = 0x5354_5245_414D_0018;
    }

    impl VarlitKeyedBlock for KeyedTestBlock {
        type Key = u64;

        fn key(&self) -> Self::Key {
            self.0
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct OversizedBlock;

    impl VarlitEncode for OversizedBlock {
        const WIRE_TYPE: WireType = WireType::Bytes;

        fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
            encoder.write_all(&[0xA5; 1024]);
            Ok(())
        }
    }

    impl VarlitDecode for OversizedBlock {
        const WIRE_TYPE: WireType = WireType::Bytes;

        fn decode_varlit(_decoder: &mut Decoder<'_>) -> Result<Self> {
            Ok(Self)
        }
    }

    impl VarlitBlock for OversizedBlock {
        const ID: u32 = 19;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Variable;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = false;
        const SCHEMA_FINGERPRINT: u64 = 0x5354_5245_414D_0019;
    }

    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: TestBlock::ID,
        name: "TestBlock",
        version: TestBlock::VERSION,
        kind: TestBlock::KIND,
        fields: &[],
    }];

    static KEYED_BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: KeyedTestBlock::ID,
        name: "KeyedTestBlock",
        version: KeyedTestBlock::VERSION,
        kind: KeyedTestBlock::KIND,
        fields: &[],
    }];

    static OVERSIZED_BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: OversizedBlock::ID,
        name: "OversizedBlock",
        version: OversizedBlock::VERSION,
        kind: OversizedBlock::KIND,
        fields: &[],
    }];

    fn spec(index_policy: IndexPolicy, commit_policy: CommitPolicy) -> FormatSpec {
        FormatSpec::new(
            b"VSTREAM",
            1,
            Endian::Little,
            0,
            index_policy,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .with_block_identities(&[TestBlock::IDENTITY])
        .with_commit_policy(commit_policy)
        .with_read_limits(ReadLimits::finite_all(u64::MAX))
    }

    fn keyed_spec() -> FormatSpec {
        FormatSpec::new(
            b"VSTRKEY",
            1,
            Endian::Little,
            0,
            IndexPolicy::KeyedOffsetChain,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            KEYED_BLOCKS,
        )
        .with_block_identities(&[KeyedTestBlock::IDENTITY])
        .with_commit_policy(CommitPolicy::RecordFooter)
        .with_read_limits(ReadLimits::finite_all(u64::MAX))
    }

    fn bounded_encode_spec() -> FormatSpec {
        FormatSpec::new(
            b"VSTRBND",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            OVERSIZED_BLOCKS,
        )
        .with_block_identities(&[OversizedBlock::IDENTITY])
        .with_read_limits(
            ReadLimits::STANDARD
                .with_max_logical_payload_len(16)
                .with_max_record_payload_len(16),
        )
    }

    #[test]
    fn confirmed_partial_record_is_an_error_for_both_cursors() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("confirmed-partial.varlit");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        let target = writer.push_info(&TestBlock(42))?.record_offset;
        writer.sync()?;
        drop(writer);
        // Open before mutation: the selected EOF has already been confirmed.
        let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
        let mut raw = std::fs::OpenOptions::new().write(true).open(&path)?;
        raw.seek(SeekFrom::Start(target + 16))?;
        raw.write_all(&9u64.to_le_bytes())?;
        drop(raw);
        // The sync marker after the record keeps the selected EOF past it, so
        // the shortened record frames against marker bytes instead of running
        // into the EOF: an error either way, whichever check reaches it first.
        assert!(reader.events()?.any(|event| event.is_err()));
        assert!(matches!(reader.blocks::<TestBlock>()?.next(), Some(Err(_))));
        Ok(())
    }

    #[test]
    fn anchors_carry_the_stream_state_before_their_spanning_record() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("summaries.varlit");
        // 4 KiB spacing, 256-byte anchors: a few hundred records cross many.
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::None)
            .with_anchors(crate::AnchorPolicy::every(12, true));
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        let records_at_create = writer.record_count;
        let mut written = Vec::new();
        for value in 0..600u64 {
            let info = writer.push_info(&TestBlock(value))?;
            written.push(info);
        }
        writer.sync()?;
        let mut file = PrimaryFile::new(std::fs::File::open(&path)?, None);
        let header_len = read_primary_header(spec, &mut file)?.0;
        let mut bytes = [0u8; 4096];
        let mut seen = 0;
        for k in 1.. {
            let Some((anchor, hole)) = file.anchor_into(k, &mut bytes)? else {
                break;
            };
            let mut tails = Vec::new();
            let summary = crate::anchor::StreamSummary::decode(
                &bytes[..hole],
                header_len,
                anchor.record_start,
                |id, offset, sequence| tails.push((id, offset, sequence)),
            )
            .expect("every stream anchor carries a summary");
            let before: Vec<_> = written
                .iter()
                .filter(|info| info.record_offset < anchor.record_start)
                .collect();
            assert_eq!(summary.next_sequence, anchor.sequence);
            assert_eq!(
                summary.records_before,
                records_at_create + before.len() as u64
            );
            let user_tail = tails.iter().find(|(id, ..)| *id == TestBlock::ID).copied();
            assert_eq!(
                user_tail,
                before
                    .last()
                    .map(|info| (TestBlock::ID, info.record_offset, info.sequence))
            );
            seen += 1;
        }
        assert!(seen >= 5, "the test must cross several anchors ({seen})");
        Ok(())
    }

    fn anchored_spec() -> FormatSpec {
        // 4 KiB spacing, 256-byte anchors: a few hundred records cross many.
        spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter)
            .with_anchors(crate::AnchorPolicy::every(12, true))
    }

    type WriterState = (u64, Option<u64>, Vec<(u32, Option<StreamTail>)>);

    fn writer_state(writer: &VarlitStreamWriter) -> WriterState {
        (
            writer.record_count,
            writer.sequence,
            writer.block_tails.clone(),
        )
    }

    /// The state a walk framing every record from the header reaches at `end`.
    fn framed_state(spec: FormatSpec, path: &Path, end: u64) -> Result<WriterState> {
        let mut file = PrimaryFile::new(fs::File::open(path)?, None);
        let header_len = read_primary_header(spec, &mut file)?.0;
        let len = file.len()?;
        let physical = SnapshotFile::from_file_with_len(file, len)?;
        let mut position = StreamPosition::initial(spec);
        assert_eq!(
            frame_until(spec, &physical, header_len, end, &mut position)?,
            end
        );
        Ok((
            position.record_count,
            position.sequence,
            position.block_tails,
        ))
    }

    fn published_slots_at(spec: FormatSpec, path: &Path) -> Result<u64> {
        let mut file = PrimaryFile::new(fs::File::open(path)?, None);
        let (header_len, extensions) = read_primary_header(spec, &mut file)?;
        Ok(
            crate::file::published_end_slots_at(header_len, &extensions)?
                .expect("a stream file carries the published end"),
        )
    }

    /// Rewrites `bytes` at `offset`, extending the file when it ends there.
    fn patch(path: &Path, offset: u64, bytes: &[u8]) -> Result<()> {
        let mut contents = fs::read(path)?;
        let at = usize::try_from(offset).expect("test offset");
        if contents.len() < at + bytes.len() {
            contents.resize(at + bytes.len(), 0);
        }
        contents[at..at + bytes.len()].copy_from_slice(bytes);
        fs::write(path, contents)?;
        Ok(())
    }

    fn flip_byte(path: &Path, offset: u64) -> Result<()> {
        let byte = fs::read(path)?[usize::try_from(offset).expect("test offset")];
        patch(path, offset, &[byte ^ 0x40])
    }

    fn read_values(spec: FormatSpec, path: &Path) -> Result<Vec<u64>> {
        let reader = VarlitStreamReader::open(spec, path, StreamOptions::default())?;
        Ok(reader
            .blocks::<TestBlock>()?
            .map(|block| block.map(|block| block.0))
            .collect::<Result<_>>()?)
    }

    #[test]
    fn a_reopened_writer_resumes_from_the_newest_anchor_and_matches_a_full_walk() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("resume.varlit");
        let spec = anchored_spec();
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        for value in 0..600u64 {
            writer.push_info(&TestBlock(value))?;
        }
        writer.sync()?;
        let end = writer.snapshot.len();
        let expected = writer_state(&writer);
        drop(writer);
        assert_eq!(framed_state(spec, &path, end)?, expected);

        crate::file::reset_stream_io_counters();
        let mut writer = VarlitStreamWriter::open(spec, &path, StreamOptions::default())?;
        let (_, entries, _) = crate::file::stream_io_counters();
        assert_eq!(writer_state(&writer), expected);
        // Framed from the anchor before the end, not from the header.
        assert!(
            entries < expected.0 / 4,
            "open framed {entries} of {} records",
            expected.0
        );
        writer.push_info(&TestBlock(600))?;
        writer.sync()?;
        drop(writer);
        assert_eq!(read_values(spec, &path)?, (0..601).collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn damaged_anchors_fall_back_to_an_older_one_and_then_to_the_header() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("damaged-anchor.varlit");
        let spec = anchored_spec();
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        for value in 0..600u64 {
            writer.push_info(&TestBlock(value))?;
        }
        writer.sync()?;
        let end = writer.snapshot.len();
        let layout = writer.file.layout().expect("anchored");
        let expected = writer_state(&writer);
        drop(writer);
        let newest = layout.anchors_before(end);
        assert!(newest > 6, "the test needs several anchors ({newest})");
        // The newest anchor first, then every anchor an open looks at.
        for damaged in [1, 6] {
            for k in newest + 1 - damaged..=newest {
                if damaged == 1 || k != newest {
                    flip_byte(&path, layout.anchor_physical(k).expect("anchor") + 20)?;
                }
            }
            let writer = VarlitStreamWriter::open(spec, &path, StreamOptions::default())?;
            assert_eq!(writer_state(&writer), expected, "{damaged} damaged");
        }
        assert_eq!(read_values(spec, &path)?, (0..600).collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn a_torn_published_slot_leaves_the_other_end_to_verify_from() -> Result<()> {
        use crate::published_end::{SLOT_LEN, decode_slot};
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("torn-slot.varlit");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        for value in 0..3u64 {
            writer.push_info(&TestBlock(value))?;
            writer.sync()?;
        }
        let end = writer.snapshot.len();
        let expected = writer_state(&writer);
        drop(writer);
        let slots_at = published_slots_at(spec, &path)?;
        let bytes = fs::read(&path)?;
        let newest = (0..2)
            .max_by_key(|slot| {
                let at = slots_at as usize + slot * SLOT_LEN;
                decode_slot(bytes[at..at + SLOT_LEN].try_into().unwrap()).map(|slot| slot.seq)
            })
            .unwrap();
        flip_byte(&path, slots_at + (newest * SLOT_LEN) as u64 + 3)?;
        // The other slot names the previous sync's end; the last marker
        // verifies from there.
        assert_eq!(read_values(spec, &path)?, [0, 1, 2]);
        let writer = VarlitStreamWriter::open(spec, &path, StreamOptions::default())?;
        assert_eq!(writer.snapshot.len(), end);
        assert_eq!(writer_state(&writer), expected);
        Ok(())
    }

    #[test]
    fn a_file_without_a_published_end_recovers_from_its_first_marker() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("legacy.varlit");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        writer.push_info(&TestBlock(1))?;
        writer.sync()?;
        writer.push_info(&TestBlock(2))?;
        writer.sync()?;
        let synced = writer.snapshot.len();
        writer.push_info(&TestBlock(3))?;
        drop(writer);
        // Blank slots: what a file written before the block existed says.
        let slots_at = published_slots_at(spec, &path)?;
        patch(&path, slots_at, &[0u8; 2 * crate::published_end::SLOT_LEN])?;
        assert_eq!(read_values(spec, &path)?, [1, 2]);
        let mut writer = VarlitStreamWriter::open(spec, &path, StreamOptions::default())?;
        assert_eq!(fs::metadata(&path)?.len(), synced);
        assert_eq!(writer_state(&writer), framed_state(spec, &path, synced)?);
        writer.push_info(&TestBlock(4))?;
        writer.sync()?;
        drop(writer);
        assert_eq!(read_values(spec, &path)?, [1, 2, 4]);
        Ok(())
    }

    #[test]
    fn a_file_without_markers_or_a_published_end_keeps_every_record_that_frames() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("unmarked.varlit");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        writer.push_info(&TestBlock(1))?;
        writer.push_info(&TestBlock(2))?;
        drop(writer);
        // Published at create, never synced since: readers see nothing yet.
        assert_eq!(read_values(spec, &path)?, Vec::<u64>::new());
        let slots_at = published_slots_at(spec, &path)?;
        patch(&path, slots_at, &[0u8; 2 * crate::published_end::SLOT_LEN])?;
        // And half a record after them, which does not frame.
        patch(&path, fs::metadata(&path)?.len(), &[0xA5; 7])?;
        let framed = fs::metadata(&path)?.len() - 7;
        assert_eq!(read_values(spec, &path)?, [1, 2]);
        let writer = VarlitStreamWriter::open(spec, &path, StreamOptions::default())?;
        assert_eq!(fs::metadata(&path)?.len(), framed);
        assert_eq!(writer_state(&writer), framed_state(spec, &path, framed)?);
        Ok(())
    }

    #[test]
    fn a_file_cut_below_its_published_end_is_refused() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("cut.varlit");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        writer.push_info(&TestBlock(1))?;
        writer.sync()?;
        let end = writer.snapshot.len();
        drop(writer);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_len(end - 1)?;
        assert!(matches!(
            VarlitStreamReader::open(spec, &path, StreamOptions::default()),
            Err(Error::CorruptTail { .. })
        ));
        assert!(matches!(
            VarlitStreamWriter::open(spec, &path, StreamOptions::default()),
            Err(Error::CorruptTail { .. })
        ));
        assert_eq!(fs::metadata(&path)?.len(), end - 1);
        Ok(())
    }

    #[test]
    fn each_sync_publishes_its_end_and_a_native_writer_withdraws_it() -> Result<()> {
        use crate::published_end::{Published, SLOT_LEN};
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("published.varlit");
        let spec = spec(IndexPolicy::ScanOnOpen, CommitPolicy::None);
        let published = |path: &Path| -> Result<Published> {
            let mut file = PrimaryFile::new(std::fs::File::open(path)?, None);
            let (header_len, extensions) = read_primary_header(spec, &mut file)?;
            let slots_at = crate::file::published_end_slots_at(header_len, &extensions)?
                .expect("a stream file carries the published end");
            let mut slots = [0u8; 2 * SLOT_LEN];
            crate::primary_file::read_exact_at(file.raw(), slots_at, &mut slots)?;
            Ok(crate::published_end::read(&slots))
        };
        let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
        // Create publishes the start of the chain with the file itself.
        assert_eq!(published(&path)?, Published::Bounded(writer.snapshot.len()));
        for round in 0..3u64 {
            writer.push_info(&TestBlock(round))?;
            writer.sync()?;
            assert_eq!(published(&path)?, Published::Bounded(writer.snapshot.len()));
        }
        drop(writer);
        // A native writer of a format without transaction markers bounds no
        // reader: it withdraws the stream writer's end when it takes over.
        let mut native = crate::VarlitFile::open(spec, &path)?;
        assert_eq!(published(&path)?, Published::Unbounded);
        native.push(&TestBlock(3))?;
        native.flush()?;
        drop(native);
        // The stream writer takes the file back: everything that frames is
        // kept, the native record included, and the end is published again.
        let writer = VarlitStreamWriter::open(spec, &path, StreamOptions::default())?;
        let end = writer.snapshot.len();
        assert_eq!(end, fs::metadata(&path)?.len());
        assert_eq!(published(&path)?, Published::Bounded(end));
        drop(writer);
        let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
        assert_eq!(
            reader.blocks::<TestBlock>()?.collect::<Result<Vec<_>>>()?,
            (0..4).map(TestBlock).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn stream_writer_matches_resident_native_bytes() -> Result<()> {
        #[allow(unused_mut)]
        let mut policies = vec![
            (
                IndexPolicy::ScanOnOpen,
                CommitPolicy::None,
                IntegrityPolicy::None,
            ),
            (
                IndexPolicy::BlockOffsetChain,
                CommitPolicy::RecordFooter,
                IntegrityPolicy::None,
            ),
        ];
        #[cfg(feature = "integrity")]
        policies.push((
            IndexPolicy::BlockOffsetChain,
            CommitPolicy::RecordFooter,
            IntegrityPolicy::Crc32WithHeader,
        ));
        for (index_policy, commit_policy, integrity_policy) in policies {
            let directory = tempfile::tempdir()?;
            let resident_path = directory.path().join("resident.varlit");
            let stream_path = directory.path().join("stream.varlit");
            let spec = spec(index_policy, commit_policy).with_integrity_policy(integrity_policy);
            let mut resident = crate::VarlitFile::create(spec, &resident_path)?;
            // a stream primary opens with an internal creation-nonce
            // record, so the resident file gets an equally shaped leading
            // record. Sequences and record offsets then line up exactly and the
            // user-record region must still match byte for byte.
            resident.write_record(
                crate::CREATION_NONCE_BLOCK_ID,
                1,
                crate::file::RECORD_FLAG_INTERNAL,
                &[0u8; 16],
            )?;
            let mut stream =
                VarlitStreamWriter::create(spec, &stream_path, StreamOptions::default())?;
            // A stream file's header also carries the published end, so its
            // records start later by exactly that block; relative to each
            // file's own header they are where the resident writer put them.
            let resident_header =
                crate::file::read_file_header(spec, &mut std::fs::File::open(&resident_path)?)?;
            let stream_header =
                crate::file::read_file_header(spec, &mut std::fs::File::open(&stream_path)?)?;
            for value in [3, 5, 8, 13] {
                let block = TestBlock(value);
                let (r, s) = (resident.push_info(&block)?, stream.push_info(&block)?);
                assert_eq!(r.sequence, s.sequence);
                assert_eq!(
                    r.record_offset - resident_header,
                    s.record_offset - stream_header
                );
                assert_eq!(r.payload_len, s.payload_len);
            }
            resident.flush()?;
            stream.flush()?;
            let resident_bytes = std::fs::read(&resident_path)?;
            let stream_bytes = std::fs::read(&stream_path)?;
            let nonce_record_len = crate::file::RECORD_HEADER_LEN
                + 16
                + if spec.spec_needs_record_footer() {
                    crate::file::RECORD_FOOTER_LEN
                } else {
                    0
                };
            let resident_user = usize::try_from(resident_header + nonce_record_len).unwrap();
            let stream_user = usize::try_from(stream_header + nonce_record_len).unwrap();
            // The published end, plus the extension length a header without
            // any other block did not carry.
            let grown = stream_header - resident_header;
            assert!(
                grown == crate::published_end::region_len()
                    || grown == crate::published_end::region_len() + 4,
                "the stream header is the resident one plus the published end ({grown} bytes)"
            );
            assert_eq!(
                resident_bytes.len() - resident_user,
                stream_bytes.len() - stream_user,
                "the user records take the same bytes in both writers"
            );
            if !spec.spec_needs_record_footer() {
                // Without footers no record names an absolute offset, so the
                // records are the same bytes.
                assert_eq!(
                    resident_bytes[resident_user..],
                    stream_bytes[stream_user..],
                    "user records must encode identically in the resident and streaming writers"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn batch_writer_coalesces_records_and_matches_single_append() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let single_path = directory.path().join("single.varlit");
        let batch_path = directory.path().join("batch.varlit");
        let spec = spec(IndexPolicy::ScanOnOpen, CommitPolicy::None);
        let values: Vec<_> = (0..100).map(TestBlock).collect();

        let mut single = VarlitStreamWriter::create(spec, &single_path, StreamOptions::default())?;
        for value in &values {
            single.push_info(value)?;
        }
        single.sync()?;

        let mut batch = VarlitStreamWriter::create(spec, &batch_path, StreamOptions::default())?;
        let info = batch
            .push_iter::<TestBlock, _>(
                &values,
                BatchOptions {
                    max_records: 16,
                    max_bytes: usize::MAX,
                },
            )
            .map_err(|error| error.source)?;
        batch.sync()?;

        assert_eq!(info.records, 100);
        // Sequence 0 belongs to the internal creation-nonce record.
        assert_eq!(info.first_sequence, Some(1));
        assert_eq!(info.last_sequence, Some(100));
        assert_eq!(info.write_calls, 7);
        assert_stream_bodies_match(spec, &single_path, &batch_path)?;
        Ok(())
    }

    /// Asserts two stream primaries are byte-identical apart from their
    /// per-create nonce, which is by construction unique to each create.
    fn assert_stream_bodies_match(spec: FormatSpec, left: &Path, right: &Path) -> Result<()> {
        let left_bytes = fs::read(left)?;
        let right_bytes = fs::read(right)?;
        assert_eq!(left_bytes.len(), right_bytes.len());
        let mut file = std::fs::File::open(left)?;
        let header_len = crate::file::read_file_header(spec, &mut file)?;
        let nonce_record_len = crate::file::RECORD_HEADER_LEN
            + 16
            + if spec.spec_needs_record_footer() {
                crate::file::RECORD_FOOTER_LEN
            } else {
                0
            };
        let header_end = usize::try_from(header_len).unwrap();
        let body_start = usize::try_from(header_len + nonce_record_len).unwrap();
        assert_eq!(left_bytes[..header_end], right_bytes[..header_end]);
        assert_eq!(left_bytes[body_start..], right_bytes[body_start..]);
        Ok(())
    }

    #[test]
    fn clean_reader_open_and_steady_appends_construct_no_scanner() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("constant-open.varlit");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        let values: Vec<_> = (0..64).map(TestBlock).collect();
        {
            let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
            writer
                .push_iter::<TestBlock, _>(&values, BatchOptions::default())
                .map_err(|error| error.source)?;
            writer.sync()?;
        }

        crate::file::reset_stream_io_counters();
        let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
        assert_eq!(crate::file::stream_io_counters(), (0, 0, 0));
        drop(reader);

        // The writer frames from the newest anchor (here the header) to the
        // published end once, at open; appends and syncs after it frame
        // nothing.
        let mut writer = VarlitStreamWriter::open(spec, &path, StreamOptions::default())?;
        crate::file::reset_stream_io_counters();
        writer
            .push_iter::<TestBlock, _>([TestBlock(64)], BatchOptions::default())
            .map_err(|error| error.source)?;
        writer.sync()?;
        assert_eq!(crate::file::stream_io_counters(), (0, 0, 0));
        drop(writer);

        let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
        let mut events = reader.events()?;
        assert!(events.next().transpose()?.is_some());
        let (scanners, entries, point_reads) = crate::file::stream_io_counters();
        assert_eq!(scanners, 1);
        assert_eq!(entries, 1);
        assert_eq!(point_reads, 0);
        Ok(())
    }

    #[test]
    fn open_recovers_the_synced_tail_and_cuts_the_unsynced_one() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("restore.varlit");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        let base_len;
        {
            let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
            writer.push_info(&TestBlock(1))?;
            writer.sync()?;
            base_len = std::fs::metadata(&path)?.len();
            writer.push_info(&TestBlock(2))?;
        }
        assert!(std::fs::metadata(&path)?.len() > base_len);
        // Open recovers instead of refusing: the synced record and its marker
        // are kept, the unsynced record is cut.
        let mut writer = VarlitStreamWriter::open(spec, &path, StreamOptions::default())?;
        assert_eq!(std::fs::metadata(&path)?.len(), base_len);
        writer.push_info(&TestBlock(3))?;
        writer.sync()?;
        drop(writer);

        let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
        assert_eq!(
            reader.blocks::<TestBlock>()?.collect::<Result<Vec<_>>>()?,
            [TestBlock(1), TestBlock(3)]
        );
        Ok(())
    }

    #[test]
    fn low_level_stream_methods_cannot_bypass_keyed_chain_indexing() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("keyed-bypass.varlit");
        let mut writer = VarlitStreamWriter::create(keyed_spec(), &path, StreamOptions::default())?;
        // Baseline rather than a literal: a fresh primary already holds its
        // internal creation-nonce record. The contract under test is
        // that the rejected calls append nothing on top of it.
        let baseline = writer.record_count;
        assert!(matches!(
            writer.push_with_prev_key_info(&KeyedTestBlock(1), None),
            Err(Error::StreamingUnsupported)
        ));
        assert!(matches!(
            writer.delete_with_prev_key_info::<KeyedTestBlock>(&1, None),
            Err(Error::StreamingUnsupported)
        ));
        assert_eq!(writer.record_count, baseline);
        Ok(())
    }

    #[test]
    fn scalable_writer_stops_encoding_at_the_runtime_payload_limit() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bounded-encode.varlit");
        let mut writer =
            VarlitStreamWriter::create(bounded_encode_spec(), &path, StreamOptions::default())?;
        let clean_len = fs::metadata(&path)?.len();
        let baseline = writer.record_count;
        assert!(matches!(
            writer.push_info(&OversizedBlock),
            Err(Error::LimitExceeded {
                resource: "logical payload length",
                actual: 1024,
                limit: 16,
            })
        ));
        assert_eq!(fs::metadata(&path)?.len(), clean_len);
        assert_eq!(writer.record_count, baseline);
        writer.sync()?;
        Ok(())
    }

    #[test]
    fn reopen_and_owned_iterators_remain_bounded() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bounded.varlit");
        let spec = spec(IndexPolicy::BlockOffsetChain, CommitPolicy::RecordFooter);
        {
            let mut writer = VarlitStreamWriter::create(spec, &path, StreamOptions::default())?;
            for value in 0..128 {
                writer.push_info(&TestBlock(value))?;
            }
            let state = writer.resident_state();
            assert_eq!(state.declared_block_tails, 1);
            assert_eq!(state.retained_record_entries, 0);
            assert_eq!(state.retained_key_entries, 0);
            assert_eq!(state.active_payload_bytes, 0);
            writer.sync()?;
        }
        {
            let mut writer = VarlitStreamWriter::open(spec, &path, StreamOptions::default())?;
            // Sequence 0 belongs to the internal creation-nonce record and
            // sequence 129 to the sync marker after the first 128 appends, so
            // the 129th user append carries sequence 130.
            assert_eq!(writer.push_info(&TestBlock(128))?.sequence, 130);
            writer.sync()?;
        }
        let reader = VarlitStreamReader::open(spec, &path, StreamOptions::default())?;
        let blocks = reader.blocks::<TestBlock>()?;
        drop(reader);
        let values = blocks.collect::<Result<Vec<_>>>()?;
        assert_eq!(values.len(), 129);
        assert_eq!(values.last(), Some(&TestBlock(128)));
        Ok(())
    }

    /// A variable payload large enough to exercise the bulk vector codec.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct SamplesBlock(Vec<u16>);

    impl VarlitEncode for SamplesBlock {
        const WIRE_TYPE: WireType = WireType::Seq;

        fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
            self.0.encode_varlit(encoder)
        }
    }

    impl VarlitDecode for SamplesBlock {
        const WIRE_TYPE: WireType = WireType::Seq;

        fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
            Ok(Self(Vec::<u16>::decode_varlit(decoder)?))
        }
    }

    impl VarlitBlock for SamplesBlock {
        const ID: u32 = 17;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Variable;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = false;
        const SCHEMA_FINGERPRINT: u64 = 0x5354_5245_414D_0117;
    }

    /// An over-limit payload is refused like the owning form and leaves the
    /// chunk buffer exactly as it was.
    #[test]
    fn in_place_record_staging_restores_the_buffer_on_a_limit_failure() -> Result<()> {
        let value = SamplesBlock(vec![5; 100]);
        let logical = 8 + 2 * 100;
        for limit in [0u64, 7, 8, 100, logical - 1, logical] {
            let spec = spec(IndexPolicy::ScanOnOpen, CommitPolicy::None)
                .with_resource_limits(ReadLimits::MISSING.with_max_logical_payload_len(limit));
            let mut out = vec![9u8; 40];
            let staged = prepare_stream_user_record_into(spec, &value, 1, 0, None, None, &mut out);
            let owned = prepare_stream_user_record(spec, &value, 1, 0, None, None);
            match (staged, owned) {
                (Ok(_), Ok(owned)) => {
                    assert_eq!(limit, logical);
                    assert_eq!(&out[40..], &owned.bytes[..]);
                }
                (
                    Err(Error::LimitExceeded {
                        actual: a,
                        limit: la,
                        resource: ra,
                    }),
                    Err(Error::LimitExceeded {
                        actual: b,
                        limit: lb,
                        resource: rb,
                    }),
                ) => {
                    assert_eq!((a, la, ra), (b, lb, rb), "limit {limit}");
                    assert_eq!(out, vec![9u8; 40], "buffer must be restored");
                }
                (staged, owned) => panic!(
                    "limit {limit}: staged {:?} vs owned {:?}",
                    staged.err(),
                    owned.err()
                ),
            }
        }
        Ok(())
    }

    #[test]
    fn non_increasing_sequences_are_streaming_unsupported() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("sequence.varlit");
        let spec = spec(IndexPolicy::ScanOnOpen, CommitPolicy::None);
        let mut file: crate::primary_file::PrimaryFile = std::fs::File::create(&path)?.into();
        crate::file::write_file_header(spec, &mut file)?;
        let first_offset = file.stream_position()?;
        let first = prepare_stream_user_record(spec, &TestBlock(1), 4, first_offset, None, None)?;
        file.write_all(&first.bytes)?;
        let second_offset = file.stream_position()?;
        let second = prepare_stream_user_record(spec, &TestBlock(2), 4, second_offset, None, None)?;
        file.write_all(&second.bytes)?;
        drop(file);

        let mut file = PrimaryFile::new(std::fs::File::open(&path)?, None);
        read_primary_header(spec, &mut file)?;
        let mut scanner = NativeStreamScanner::from_snapshot(spec, SnapshotFile::from_file(file)?)?;
        assert!(scanner.next_entry()?.is_some());
        assert!(matches!(
            scanner.next_entry(),
            Err(Error::StreamingUnsupported)
        ));
        Ok(())
    }

    #[test]
    fn unsupported_policy_is_rejected_before_create() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unsupported.varlit");
        let spec = spec(
            IndexPolicy::ScanOnOpen,
            CommitPolicy::TransactionMarker(TransactionMarkerMode::Explicit),
        );
        assert!(matches!(
            VarlitStreamWriter::create(spec, &path, StreamOptions::default()),
            Err(Error::StreamingUnsupported)
        ));
        assert!(!path.exists());
    }
}
