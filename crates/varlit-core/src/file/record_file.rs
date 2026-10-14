use super::{Error, RecordOverwrite, ReservedIndexSlot, Result, opened_file_identity};
use crate::snapshot::WrittenThrough;
use std::fs::{File, Metadata};
use std::io::{Seek, SeekFrom, Write};

#[cfg(any(test, feature = "scalable-fault-injection"))]
std::thread_local! {
    /// `fstat`s this thread has issued through [`RecordFile::metadata`].
    ///
    /// Fault-testing hook only. The append path must not advance it: it
    /// takes the end of file from the snapshot the writer already
    /// maintains, and `append_record_at_end`'s `SEEK_END` check is what
    /// keeps that value honest.
    static RECORD_FILE_METADATA_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static RECORD_FILE_SEEKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one `lseek` this type issued on its own handle.
///
/// The append path's per-record `stream_position` was invisible to
/// `an_append_window_issues_no_metadata_syscall`, which counts `metadata`
/// only — so the syscall the PERF work removed had a twin the regression
/// test could not see.
#[inline]
fn count_record_file_seek() {
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    RECORD_FILE_SEEKS.with(|count| count.set(count.get().saturating_add(1)));
}

/// Reads and clears this thread's `RecordFile` seek count.
#[cfg(any(test, feature = "scalable-fault-injection"))]
pub(super) fn take_record_file_seeks() -> u64 {
    RECORD_FILE_SEEKS.with(|count| count.replace(0))
}

/// Counts one `RecordFile::metadata` syscall on this thread. Inert outside
/// tests and without the `scalable-fault-injection` feature.
#[inline]
fn count_metadata_call() {
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    RECORD_FILE_METADATA_CALLS.with(|count| count.set(count.get().saturating_add(1)));
}

/// Reads and clears this thread's `RecordFile::metadata` syscall count.
#[cfg(any(test, feature = "scalable-fault-injection"))]
pub(super) fn take_record_file_metadata_calls() -> u64 {
    RECORD_FILE_METADATA_CALLS.with(|count| count.replace(0))
}

/// The record handle, and the one place that may move its file offset.
///
/// Three writes leave this module and no others, pinned by
/// `enforcement_gates::the_primary_record_handle_is_only_written_through_its_three_gated_operations`:
/// [`RecordFile::append_record_at_end`], which seeks to the end itself;
/// [`RecordFile::overwrite_indexed_record`], which consumes a
/// `RecordOverwrite`; and [`RecordFile::overwrite_header_region`], which
/// writes a fixed-length region of the file header and no record byte.
///
/// # Why there is no cached cursor here
///
/// There was one, to spare the append path an `lseek` per record, and it
/// was unsound: `VarlitFile::snapshot` is a `try_clone` of this same handle,
/// and `try_clone` shares the *open file description* — so it shares the
/// offset. Everything derived from that snapshot moves the number this
/// type would have been claiming to know:
///
/// - `SnapshotFile::cursor_at` seeks its `BufReader` (the layout scan), and
/// - `try_clone_file` hands a sharing duplicate to `validate_generation_index`
///   (every `replace_*`) and to `verify_matrix_metadata`, both of which seek it,
/// - and on Windows `SnapshotFile::read_exact_at` is `seek_read`, which moves
///   the offset on **every positional read** — including from another thread,
///   under `&self`, while this handle is exclusively borrowed. No
///   invalidate-on-handout scheme can cover that one.
///
/// So the offset is not this type's to cache. The append path pays no
/// `lseek` for it anyway, because it no longer asks: the only consumer was
/// the rollback cursor, and `rollback_append` now restores the end of file
/// it truncates back to — a number it already holds.
#[derive(Debug)]
pub struct RecordFile {
    file: File,
    /// This reader's private Windows read handle, initialized without a
    /// mutex. Its local cache makes the owner Send + !Sync.
    matrix_read_pool: crate::matrix::MatrixReadPool,
    matrix_pages: Option<crate::matrix_generation::MatrixFile>,
}

impl RecordFile {
    pub(super) fn new(file: File) -> Self {
        Self {
            file,
            matrix_read_pool: crate::matrix::MatrixReadPool::new(),
            matrix_pages: None,
        }
    }

    pub(super) fn with_matrix(
        mut self,
        pages: Option<crate::matrix_generation::MatrixFile>,
    ) -> Self {
        self.matrix_pages = pages;
        self
    }
    pub(super) fn adopt_matrix(&mut self, mut pages: crate::matrix_generation::MatrixFile) {
        if let Some(old) = &mut self.matrix_pages {
            pages.inherit_cache(old);
        }
        self.matrix_pages = Some(pages);
    }
    pub(super) fn matrix_pages(&self) -> Option<&crate::matrix_generation::MatrixFile> {
        self.matrix_pages.as_ref()
    }
    pub(super) fn compact_matrix(&mut self) -> Result<()> {
        self.matrix_pages
            .as_mut()
            .ok_or(Error::MatrixLayoutMissing)?
            .compact()
    }
    pub(super) fn publish_matrix(&mut self, len: u64) -> Result<()> {
        if let Some(pages) = &mut self.matrix_pages {
            pages.publish(len)?;
        }
        Ok(())
    }
    pub(super) fn matrix_barrier_file(&mut self) -> Result<&mut File> {
        Ok(self
            .matrix_pages
            .as_mut()
            .ok_or(Error::MatrixLayoutMissing)?
            .barrier_file()?)
    }
    pub(super) fn metadata(&self) -> std::io::Result<Metadata> {
        count_metadata_call();
        self.file.metadata()
    }

    /// Asks the kernel where this handle's offset is.
    ///
    /// Test-only, and deliberately: no production path reads this. Its one
    /// caller was the rollback cursor, which is where the cache above came
    /// from — see that type's documentation for why it could not stay.
    #[cfg(test)]
    pub(super) fn stream_position(&mut self) -> std::io::Result<u64> {
        count_record_file_seek();
        self.file.stream_position()
    }

    /// Moves the handle's cursor. Reads and cursor restoration only: this
    /// type implements no write that follows the cursor.
    pub(super) fn seek_to(&mut self, offset: u64) -> std::io::Result<u64> {
        count_record_file_seek();
        self.file.seek(SeekFrom::Start(offset))
    }

    pub(super) fn set_len(&mut self, len: u64) -> std::io::Result<()> {
        self.file.set_len(len)
    }

    pub(super) fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }

    pub(super) fn sync_all(&self) -> std::io::Result<()> {
        self.file.sync_all()
    }

    pub(super) fn sync_data(&self) -> std::io::Result<()> {
        self.file.sync_data()
    }

    /// OS-object identity bytes of the open handle.
    ///
    /// Returned as bytes rather than as a `&File` on purpose: `&File`
    /// implements `Write`, so lending one out would reopen exactly the hole
    /// this module closes.
    pub(super) fn object_identity(&self) -> Result<Vec<u8>> {
        opened_file_identity(&self.file)
    }

    /// The named exception: the handle `crate::matrix`'s functions take.
    ///
    /// The matrix region is disjoint from the record region; its own
    /// shape-A enforcement is `matrix.rs`'s `mod page_index`. Every call
    /// site of this accessor must be an argument to a `crate::matrix::`
    /// call, which `enforcement_gates.rs` asserts.
    pub(super) fn matrix_region(&mut self) -> &mut crate::matrix_generation::MatrixFile {
        self.matrix_pages
            .as_mut()
            .expect("matrix generation attached")
    }

    /// Positional matrix reads through `&self`, without exposing `&File`
    /// or its `Write` implementation. The owner is Send + !Sync; each
    /// thread opens its own reader and owns its Windows handle cache.
    pub(super) fn matrix_region_reader(&self) -> crate::matrix::MatrixRegionReader<'_> {
        crate::matrix::MatrixRegionReader::with_io(
            self.matrix_pages
                .as_ref()
                .expect("matrix generation attached"),
            &self.matrix_read_pool,
        )
    }

    /// Appends one record at the current end of file.
    ///
    /// Seeks to the end itself and refuses if that is not `expected_offset`
    /// — the offset the caller budgeted, checked against limits and
    /// recorded in its rollback snapshot. A caller cannot use this to write
    /// into an already-indexed record even by passing an arbitrary offset.
    ///
    /// # Why it takes the reservation token
    ///
    /// Shape A is *"authoritative disk write, then fallible mirror
    /// update"*. Holding a [`ReservedIndexSlot`] is proof that the resident
    /// index has already been charged against `ReadLimitKey::IndexBytes`
    /// and `try_reserve`d for this record, so the only work left for the
    /// mirror after this write is an infallible `push`. Demanding it here
    /// is what makes the ordering a compile-time property: an append helper
    /// written next month cannot reach this function at all until it has
    /// done the fallible half, and the reservation therefore cannot be
    /// moved below the write. The token is zero-sized and is borrowed, not
    /// consumed, because the caller still needs it to install the entry
    /// afterwards.
    ///
    /// # Why it returns a [`WrittenThrough`]
    ///
    /// This function owns the offset the write actually went to and every
    /// byte count it wrote, so the fact "the file physically reaches
    /// `offset + bytes`" is created here, inside the module that owns the
    /// raw handle. Handing it back as a witness is what lets the caller
    /// rebind its snapshot without an `fstat` — see
    /// [`crate::snapshot::SnapshotFile::with_written_len`]. It is built
    /// from `offset`, the value `SEEK_END` returned, not from
    /// `expected_offset`: it describes where the bytes went, not where
    /// they were meant to go.
    pub(super) fn append_record_at_end(
        &mut self,
        _reserved: &ReservedIndexSlot,
        expected_offset: u64,
        header_bytes: &[u8],
        payload: &[u8],
        footer: Option<&[u8]>,
        after_header: impl FnOnce() -> Result<()>,
    ) -> Result<WrittenThrough> {
        count_record_file_seek();
        let offset = self.file.seek(SeekFrom::End(0))?;
        if offset != expected_offset {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "append offset is not the end of the file",
            )));
        }
        self.file.write_all(header_bytes)?;
        after_header()?;
        self.file.write_all(payload)?;
        if let Some(footer) = footer {
            self.file.write_all(footer)?;
        }
        let written = [
            header_bytes.len(),
            payload.len(),
            footer.map_or(0, <[u8]>::len),
        ]
        .into_iter()
        .try_fold(offset, |end, part| {
            u64::try_from(part)
                .ok()
                .and_then(|part| end.checked_add(part))
        })
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "appended record extent",
        })?;
        Ok(WrittenThrough::after_write(written))
    }

    /// Rewrites an already-indexed record's header and payload in place.
    ///
    /// Takes the [`RecordOverwrite`] by value; producing one performs the
    /// version refusal (F-01) and drops the block's keyed-tail map (F-02),
    /// both strictly before the first byte reaches disk.
    /// Overwrites a fixed-length region *inside the file header*.
    ///
    /// The third gated write, and the only one that does not touch a
    /// record. It exists for `IndexPolicy::header_tails`, whose region is
    /// written at create and rewritten at every commit point; the region's
    /// length never changes, so this never moves a record.
    ///
    /// Safe against the append cursor for the reason nothing here caches
    /// one: `append_record_at_end` re-seeks `SeekFrom::End(0)` on every
    /// call, and every read is positional. A single `write_all` is
    /// deliberate — on Linux a `write()` to a regular file is taken under
    /// the inode lock, so a concurrent unlocked reader sees the slot whole
    /// or not at all. Windows offers no equivalent, which is why every slot
    /// carries its own checksum.
    pub(super) fn overwrite_header_region(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        count_record_file_seek();
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(bytes)?;
        Ok(())
    }

    /// Overwrites a record footer's trailing mutable word, and nothing
    /// else.
    ///
    /// The fourth gated write, and the narrowest: it takes a fixed
    /// four-byte array rather than a slice, so it cannot be handed a run
    /// of record bytes by mistake, and the offset it is given is checked
    /// by the caller against a footer it has just framed.
    ///
    /// Safe against the append cursor for the reason the others are:
    /// `append_record_at_end` re-seeks `SeekFrom::End(0)` every call and
    /// every read is positional. A single `write_all` of four bytes, which
    /// Linux takes under the inode lock; Windows offers no equivalent, and
    /// unlike the header-tail slot there is no checksum here to catch a
    /// torn one — a four-byte write is not torn by any storage stack this
    /// targets, and the flags are advisory even if it were.
    pub(super) fn overwrite_record_mutable_flags(
        &mut self,
        offset: u64,
        bytes: [u8; 4],
    ) -> Result<()> {
        count_record_file_seek();
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&bytes)?;
        Ok(())
    }

    pub(super) fn overwrite_indexed_record(
        &mut self,
        write: RecordOverwrite,
        header_bytes: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        count_record_file_seek();
        count_record_file_seek();
        self.file.seek(SeekFrom::Start(write.record_offset()))?;
        self.file.write_all(header_bytes)?;
        self.file.seek(SeekFrom::Start(write.payload_offset()))?;
        self.file.write_all(payload)?;
        self.file.flush()?;
        Ok(())
    }
}
