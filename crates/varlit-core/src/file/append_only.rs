//! Append-only ownership of a native file, including its last mapped reader.
use super::*;
use crate::fallible::{Arc, try_arc};

#[derive(Debug)]
struct Protection {
    spec: FormatSpec,
    path: PathBuf,
    identity: [u8; 16],
    // Never explicitly released/rebound by a writer: only the last owner drops it.
    _lock: Option<WriterLock>,
}

/// A native writer exposing only append and publication operations.
///
/// Requires the `mmap` feature and a non-matrix transaction-marker format.
/// Reader tokens and mappings retain its native writer lock after writer drop.
/// There is no mutable inner-handle accessor or conversion to an editable writer.
#[derive(Debug)]
pub struct AppendOnlyWriter {
    // Drop the writer (including its unmapped header updates) before protection.
    file: VarlitFile,
    protection: Arc<Protection>,
}

impl AppendOnlyWriter {
    /// Consumes an exclusively owned writer and restricts it to append-only use.
    ///
    /// Admission allocates shared protection once. Ordinary append and commit
    /// do not acquire a new lock or increment its reference count.
    ///
    /// # Safety
    ///
    /// Exclude writes, truncation and backing-object invalidation outside this
    /// facade for the entire lifetime of all derived tokens, readers, mappings
    /// and references, including after this writer is dropped. Advisory locks
    /// coordinate cooperating Varlit writers, not arbitrary external handles.
    /// This contract includes handles/callbacks in this process and filesystem
    /// actions in other processes. Cross-process token transfer is unsupported.
    pub unsafe fn new(mut file: VarlitFile) -> Result<Self> {
        if file.mode != OpenMode::ReadWrite
            || file
                ._lock
                .as_ref()
                .is_none_or(|lock| lock.native_guard.is_none())
        {
            return Err(Error::InvalidFormatSpec(
                "append-only admission requires a locked writer",
            ));
        }
        if file.poison.is_refusing() {
            return Err(Error::WriterPoisoned("append-only admission"));
        }
        if file.matrix.is_some()
            || file.spec.has_matrix_blocks()
            || file.spec.growing_matrix.is_some()
        {
            return Err(Error::InvalidFormatSpec(
                "append-only mappings do not support matrix storage",
            ));
        }
        if !file.spec.commit_policy.is_transaction_marker() {
            return Err(Error::InvalidFormatSpec(
                "append-only mappings require transaction markers",
            ));
        }
        let identity = file.file.object_identity_array()?;
        let native = file
            ._lock
            .as_ref()
            .and_then(|lock| lock.native_guard.as_ref())
            .expect("admission checked native exclusion");
        if super::publication::opened_file_identity_array(native)? != identity {
            return Err(Error::InvalidFormatSpec(
                "append-only writer lock protects another object",
            ));
        }
        let mut protection = try_arc(Protection {
            spec: file.spec,
            path: file.path.try_clone()?,
            identity,
            _lock: None,
        })?;
        // All fallible preparation completes while the original writer still
        // owns its lock. No clone can exist before this private assignment.
        let owner = Arc::get_mut(&mut protection).expect("new protection is unique");
        owner._lock = file._lock.take();
        Ok(Self { file, protection })
    }

    /// Issues a file-bound token without allocating or accessing the filesystem.
    pub fn reader_token(&self) -> AppendOnlyReaderToken {
        AppendOnlyReaderToken {
            protection: Arc::clone(&self.protection),
        }
    }

    /// Appends through the existing bounded native encoder.
    pub fn push_info<T: VarlitBlock>(&mut self, block: &T) -> Result<AppendInfo> {
        self.file.push_info(block)
    }

    /// Appends a keyed record without changing its predecessor's bytes.
    pub fn push_keyed_info<T>(&mut self, block: &T) -> Result<AppendInfo>
    where
        T: VarlitKeyedBlock,
        T::Key: Eq + Hash,
    {
        self.file.push_keyed_info(block)
    }

    /// Flushes according to the format's existing transaction-marker policy.
    pub fn flush(&mut self) -> Result<()> {
        self.file.flush()
    }

    /// Publishes the pending records without a durability request.
    pub fn commit(&mut self) -> Result<AppendInfo> {
        self.file.commit()
    }

    /// Publishes and durably synchronizes the pending records.
    pub fn commit_durable(&mut self) -> Result<AppendInfo> {
        self.file.commit_durable()
    }
}

/// Authority to open protected readers of exactly one backing file object.
///
/// May be shared between threads; create each reader on its owning worker.
/// Keeping a token alive also keeps the native writer exclusion alive.
#[derive(Debug)]
pub struct AppendOnlyReaderToken {
    protection: Arc<Protection>,
}

impl AppendOnlyReaderToken {
    /// Opens an independent read-only handle and verifies its backing identity.
    ///
    /// This initial API opens the session's original pathname once. A removed
    /// pathname can fail to open; replacement with another object is refused.
    /// Existing readers and mappings remain pinned to the original object.
    /// No caller-supplied path or file handle is accepted.
    pub fn open_reader(&self) -> Result<AppendOnlyReader> {
        self.open_reader_with_resource_limits(crate::ReadLimits::MISSING)
    }

    /// Selects this reader's resource limits and optional read-check policy.
    /// Missing fields preserve admitted format defaults; supplied fields replace
    /// them. File identity, confirmed bounds and ownership remain mandatory.
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
        Ok(AppendOnlyReader {
            file,
            protection: Arc::clone(&self.protection),
        })
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
    protection: Arc<Protection>,
}

impl AppendOnlyReader {
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
    _protection: Arc<Protection>,
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
    _protection: Arc<Protection>,
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
