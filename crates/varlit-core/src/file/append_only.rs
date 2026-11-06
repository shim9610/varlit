//! Session-bound native append writers and independently retained mappings.
use super::*;
use crate::fallible::{Arc, try_arc};
use std::time::Duration;
mod reader_gate;
mod session;
pub use session::{
    AppendOnlyFailure, AppendOnlySession, AppendOnlySessionStatus, AppendOnlyWriterState,
};
use session::{Protection, ReaderProtection, WriteAuthority};

/// One active append-only writer lease from a pinned session.
///
/// Closing a committed healthy writer returns its cached native state to the
/// session. Readers and their mappings can remain alive across reconnect.
#[derive(Debug)]
pub struct AppendOnlyWriter {
    file: Option<VarlitFile>,
    authority: Arc<WriteAuthority>,
}

impl AppendOnlyWriter {
    /// Admits a native writer and immediately acquires its session's writer slot.
    ///
    /// # Safety
    /// Exclude nonparticipating mutation/invalidation for the complete lifetime
    /// of all derived tokens, readers, mappings and references. This includes
    /// handles/callbacks in this process and other processes, after writer drop.
    pub unsafe fn new(file: VarlitFile) -> Result<Self> {
        // SAFETY: the same lasting external-ownership admission contract.
        let session = unsafe { AppendOnlySession::new(file)? };
        session.open_writer()
    }

    /// Retains write authority for reader-first use and future writer reconnect.
    pub fn session(&self) -> AppendOnlySession {
        session::session_from_writer(&self.authority)
    }

    /// Issues read-only capability without allocation or filesystem access.
    pub fn reader_token(&self) -> AppendOnlyReaderToken {
        AppendOnlyReaderToken {
            protection: Arc::clone(&self.authority.protection),
        }
    }

    /// Returns the writer without committing pending records. A pending tail or
    /// poisoned writer returns `AppendOnlyRecoveryRequired` and remains visible
    /// through session status. Existing confirmed views stay valid.
    pub fn close(mut self) -> Result<()> {
        self.return_file()
    }

    fn active_file(&mut self) -> &mut VarlitFile {
        self.file
            .as_mut()
            .expect("a live writer owns its native file")
    }

    fn report<T>(&mut self, operation: &'static str, result: &Result<T>) {
        if let Err(error) = result {
            let poisoned = self
                .file
                .as_ref()
                .is_some_and(|file| file.poison.is_refusing());
            if self.authority.failure(operation, error, poisoned).is_err() {
                self.active_file().poison.poison();
            }
        }
    }

    fn return_file(&mut self) -> Result<()> {
        match self.file.take() {
            Some(file) => self.authority.return_writer(file),
            None => Ok(()),
        }
    }

    /// Appends through the existing bounded native encoder. Normal success
    /// acquires no coordination lock; errors update the shared failure snapshot.
    #[inline]
    pub fn push_info<T: VarlitBlock>(&mut self, block: &T) -> Result<AppendInfo> {
        let result = self.active_file().push_info(block);
        self.report("push_info", &result);
        result
    }

    /// Appends a keyed record without changing its predecessor's bytes.
    #[inline]
    pub fn push_keyed_info<T>(&mut self, block: &T) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        let result = self.active_file().push_keyed_info(block);
        self.report("push_keyed_info", &result);
        result
    }

    /// Flushes according to the existing transaction-marker policy.
    pub fn flush(&mut self) -> Result<()> {
        let result = self.active_file().flush();
        self.report("flush", &result);
        result
    }

    /// Publishes pending records without a durability request.
    pub fn commit(&mut self) -> Result<AppendInfo> {
        let result = self.active_file().commit();
        self.report("commit", &result);
        result
    }

    /// Publishes and durably synchronizes pending records. A reported commit
    /// durability failure may still have published data; inspect the error.
    pub fn commit_durable(&mut self) -> Result<AppendInfo> {
        let result = self.active_file().commit_durable();
        self.report("commit_durable", &result);
        result
    }
}

impl Drop for AppendOnlyWriter {
    fn drop(&mut self) {
        let _ = self.return_file();
    }
}

/// Read-only capability tied to one pinned backing object. It grants no write
/// authority, does not block recovery by itself, and survives writer shutdown.
#[derive(Debug)]
pub struct AppendOnlyReaderToken {
    protection: Arc<Protection>,
}

impl AppendOnlyReaderToken {
    /// Opens an independently positioned reader on this session's pinned object.
    /// The original pathname must still identify that object; it is checked
    /// before parsing bytes. Existing opened readers do not reopen on follow.
    pub fn open_reader(&self) -> Result<AppendOnlyReader> {
        self.open_reader_with_resource_limits(crate::ReadLimits::MISSING)
    }

    /// Overlays supplied resource/read-check options while preserving defaults.
    /// Registration excludes recovery until this reader and all derived maps end.
    pub fn open_reader_with_resource_limits(
        &self,
        limits: crate::ReadLimits,
    ) -> Result<AppendOnlyReader> {
        let spec = self
            .protection
            .spec
            .with_resource_limits(limits)
            .resolve_entrypoint();
        spec.validate()?;
        ensure_native_open_limits(spec)?;
        let protection = Protection::reader(&self.protection)?;
        let file = OpenOptions::new().read(true).open(&self.protection.path)?;
        if super::publication::opened_file_identity_array(&file)? != self.protection.identity {
            return Err(Error::InvalidFormatSpec(
                "append-only reader backing object changed",
            ));
        }
        let mut scratch = BufferedScan::new(spec);
        let file = VarlitFile::open_readonly_file(
            spec,
            self.protection.path.try_clone()?,
            file,
            &mut scratch,
            DirectoryRetention::Keep,
        )?;
        Ok(AppendOnlyReader { file, protection })
    }

    /// Reads lifetime/error status without filesystem I/O or write authority.
    pub fn session_status(&self) -> Result<AppendOnlySessionStatus> {
        self.protection.status()
    }

    /// Waits for lifetime/error status, not for a new data publication.
    pub fn wait_for_change(
        &self,
        observed: AppendOnlySessionStatus,
        timeout: Duration,
    ) -> Result<AppendOnlySessionStatus> {
        self.protection.wait(observed, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Decoder, Encoder, VarlitDecode};

    #[derive(Debug, PartialEq)]
    pub(super) struct Number(pub(super) u64);
    impl VarlitEncode for Number {
        const WIRE_TYPE: WireType = WireType::U64;
        fn encode_varlit(&self, encoder: &mut Encoder) -> Result<()> {
            self.0.encode_varlit(encoder)
        }
    }
    impl VarlitDecode for Number {
        const WIRE_TYPE: WireType = WireType::U64;
        fn decode_varlit(decoder: &mut Decoder<'_>) -> Result<Self> {
            Ok(Self(u64::decode_varlit(decoder)?))
        }
    }
    impl VarlitBlock for Number {
        const ID: u32 = 81;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = false;
        const SCHEMA_FINGERPRINT: u64 = 81;
    }
    pub(super) fn spec() -> FormatSpec {
        FormatSpec::new(
            b"MMFAULT",
            1,
            Endian::Little,
            0,
            IndexPolicy::BlockOffsetChain,
            if cfg!(feature = "integrity") {
                IntegrityPolicy::Crc32
            } else {
                IntegrityPolicy::None
            },
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            &[crate::BlockDescriptor {
                id: 81,
                name: "Number",
                version: 1,
                kind: BlockKind::Fixed,
                fields: &[],
            }],
        )
        .with_block_identities(&[Number::IDENTITY])
        .with_commit_policy(CommitPolicy::TransactionMarker(
            crate::TransactionMarkerMode::Explicit,
        ))
    }

    #[test]
    fn admission_requires_a_healthy_writer_and_live_native_exclusion() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut file = spec().create(dir.path().join("poison"))?;
        file.poison.poison();
        // SAFETY: private test objects with no other participants.
        assert!(matches!(
            unsafe { AppendOnlyWriter::new(file) },
            Err(Error::WriterPoisoned(_))
        ));
        let mut file = spec().create(dir.path().join("unlocked"))?;
        file._lock.as_mut().unwrap().give_back_native_guard();
        // SAFETY: no other process/thread accesses this private object.
        assert!(matches!(
            unsafe { AppendOnlyWriter::new(file) },
            Err(Error::InvalidFormatSpec(_))
        ));
        let mut file = spec().create(dir.path().join("identity-a"))?;
        let mut other = spec().create(dir.path().join("identity-b"))?;
        std::mem::swap(&mut file._lock, &mut other._lock);
        // SAFETY: both objects are private; deliberately mismatched authority
        // must be rejected before a protected reader can be issued.
        assert!(matches!(
            unsafe { AppendOnlyWriter::new(file) },
            Err(Error::InvalidFormatSpec(_))
        ));
        Ok(())
    }

    #[test]
    fn failed_append_rollback_and_failed_rollback_preserve_captured_bytes() -> Result<()> {
        for fail_rollback in [false, true] {
            let dir = tempfile::tempdir()?;
            let file = spec().create(dir.path().join("fault"))?;
            // SAFETY: this private object's only writer is the facade.
            let mut writer = unsafe { AppendOnlyWriter::new(file)? };
            writer.push_info(&Number(19))?;
            writer.commit_durable()?;
            let mut reader = writer.reader_token().open_reader()?;
            let snapshot = reader.mmap_payloads()?;
            let captured = snapshot.block_payload_window::<Number>(0)?.unwrap();
            inject_write_fault(if fail_rollback {
                WriteFault::AppendAfterHeaderWithRollbackFailure
            } else {
                WriteFault::AppendAfterHeader
            });
            let failure = writer.push_info(&Number(23)).unwrap_err();
            assert_eq!(
                crate::decode_from_slice::<Number>(captured, Endian::Little)?,
                Number(19)
            );
            assert!(snapshot.block_payload_window::<Number>(1)?.is_none());
            if fail_rollback || cfg!(windows) {
                platform_tests::assert_rollback_failure(&failure, fail_rollback);
                assert!(matches!(
                    writer.commit_durable(),
                    Err(Error::WriterPoisoned(_))
                ));
            } else {
                assert!(matches!(failure, Error::Io(_)));
                assert_eq!(reader.follow()?, 0);
                writer.push_info(&Number(29))?;
                writer.commit_durable()?;
                assert!(reader.follow()? > 0);
                let next = reader.mmap_payloads()?;
                assert_eq!(
                    crate::decode_from_slice::<Number>(
                        next.block_payload_window::<Number>(1)?.unwrap(),
                        Endian::Little
                    )?,
                    Number(29)
                );
            }
        }
        Ok(())
    }
}

/// A worker-owned native reader retaining the session's writer exclusion.
#[derive(Debug)]
pub struct AppendOnlyReader {
    file: VarlitFile,
    protection: Arc<ReaderProtection>,
}

impl AppendOnlyReader {
    /// Reads writer lifetime/error state without a filesystem operation.
    pub fn session_status(&self) -> Result<AppendOnlySessionStatus> {
        self.protection.session.status()
    }

    /// Waits for lifetime/error changes; ordinary payload access takes no lock.
    pub fn wait_for_change(
        &self,
        observed: AppendOnlySessionStatus,
        timeout: Duration,
    ) -> Result<AppendOnlySessionStatus> {
        self.protection.session.wait(observed, timeout)
    }

    /// Follows newly confirmed records using the existing incremental reader.
    pub fn follow(&mut self) -> Result<u64> {
        self.file.follow()
    }

    /// Collects this block's entries through the existing bounded directory API.
    pub fn block_entries_into<T: VarlitBlock>(
        &self,
        out: &mut Vec<RecordIndexEntry>,
    ) -> Result<()> {
        self.file.block_entries_into::<T>(out)
    }

    /// Walks a block's chain newest first without copying the whole directory.
    pub fn block_chain(&self, block_id: u32) -> Result<BlockChain<'_>> {
        self.file.block_chain(block_id)
    }

    /// Maps a confirmed payload and retains protection independently of this reader.
    pub fn mmap_payload_window(&self, entry: &RecordIndexEntry) -> Result<GuardedMmapWindow> {
        if !entry.committed {
            return Err(Error::MmapEntryNotInSnapshot);
        }
        // SAFETY: admission excludes nonparticipants. This facade cannot mutate
        // captured records; marker-bounded snapshots exclude rollback-able tails.
        // The mapped extent excludes the mutable native file header. Its owner
        // retains the authoritative lock even if the writer/reader/token drops.
        let mapped = unsafe { self.file.mmap_payload_window(entry)? };
        Ok(GuardedMmapWindow {
            mapped,
            _protection: Arc::clone(&self.protection),
        })
    }

    /// Maps the fixed confirmed prefix, including the existing index-copy cost.
    pub fn mmap_payloads(&self) -> Result<GuardedMmapPayloads> {
        // SAFETY: the same protected append-only object and marker-bounded
        // snapshot as record windows. No growing matrix or editable handle can
        // enter this facade. Captured prefix records remain immutable.
        let mapped = unsafe { self.file.mmap_payloads()? };
        Ok(GuardedMmapPayloads {
            mapped,
            _protection: Arc::clone(&self.protection),
        })
    }
}

/// An owned mapped payload retaining protection until its final borrowed slice ends.
#[derive(Debug)]
pub struct GuardedMmapWindow {
    mapped: MmapPayloadWindow,
    _protection: Arc<ReaderProtection>,
}

impl GuardedMmapWindow {
    /// Returns the mapped payload bytes; no lock or reference-count operation.
    pub fn as_bytes(&self) -> &[u8] {
        self.mapped.as_bytes()
    }
}

/// A fixed native mmap snapshot retaining the append-only protection.
#[derive(Debug)]
pub struct GuardedMmapPayloads {
    mapped: MmapPayloads,
    _protection: Arc<ReaderProtection>,
}

impl GuardedMmapPayloads {
    /// Number of captured resident records, including internal records.
    pub fn len(&self) -> usize {
        self.mapped.len()
    }
    /// Whether the captured snapshot contains no resident records.
    pub fn is_empty(&self) -> bool {
        self.mapped.is_empty()
    }
    /// Captured record metadata; the existing snapshot does not grow on append.
    pub fn index_entries(&self) -> &[RecordIndexEntry] {
        self.mapped.index_entries()
    }
    /// Returns a captured payload with the existing CRC/structure policy.
    pub fn payload_window(&self, entry: &RecordIndexEntry) -> Result<&[u8]> {
        self.mapped.payload_window(entry)
    }
    /// Returns a block payload by ordinal without synchronization on each access.
    pub fn block_payload_window<T: VarlitBlock>(&self, index: usize) -> Result<Option<&[u8]>> {
        self.mapped.block_payload_window::<T>(index)
    }
}

#[cfg(test)]
#[path = "append_only_platform_tests.rs"]
mod platform_tests;
