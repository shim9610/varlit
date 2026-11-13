use super::{Error, RecordOverwrite, ReservedIndexSlot, Result, opened_file_identity};
use crate::primary_file::{PrimaryFile, RecordsAnchor};
use crate::snapshot::WrittenThrough;
use std::fs::File;
use std::io::{Seek, SeekFrom};

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
    static HEADER_REGION_WRITES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one [`RecordFile::overwrite_header_region`] write on this thread.
/// Inert outside tests and without the `scalable-fault-injection` feature.
#[inline]
fn count_header_region_write() {
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    HEADER_REGION_WRITES.with(|count| count.set(count.get().saturating_add(1)));
}

/// Reads and clears this thread's header-region write count.
#[cfg(any(test, feature = "scalable-fault-injection"))]
pub(super) fn take_header_region_writes() -> u64 {
    HEADER_REGION_WRITES.with(|count| count.replace(0))
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
    file: PrimaryFile,
}

impl RecordFile {
    pub(super) fn new(file: PrimaryFile) -> Self {
        Self { file }
    }

    #[cfg(test)]
    pub(super) fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        self.file.raw().metadata()
    }

    /// The logical length of the file: the append log without anchors.
    pub(super) fn len(&self) -> std::io::Result<u64> {
        count_metadata_call();
        self.file.len()
    }

    /// The file's size on disk, anchors included: what a rewrite reports as
    /// bytes before and after. Lends no handle.
    pub(super) fn physical_len(&self) -> std::io::Result<u64> {
        Ok(self.file.raw().metadata()?.len())
    }

    /// The file's permissions, which a rewrite's replacement inherits.
    pub(super) fn permissions(&self) -> std::io::Result<std::fs::Permissions> {
        Ok(self.file.raw().metadata()?.permissions())
    }

    pub(super) fn anchor_layout(&self) -> Option<crate::anchor::AnchorLayout> {
        self.file.layout()
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
        std::io::Write::flush(&mut self.file)
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
        opened_file_identity(self.file.raw())
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
        summary: Option<&dyn crate::anchor::SummarySource>,
        after_header: impl FnOnce() -> Result<()>,
    ) -> Result<WrittenThrough> {
        // The writer's own end of file is the append offset. The handle holds
        // the object lock and every byte is written positionally, so asking
        // the kernel for the length on every record (an `fstat` each) checked
        // nothing the lock does not already guarantee.
        let offset = expected_offset;
        let end = [
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
        // The record header carries its sequence at bytes 8..16, little-endian.
        let sequence = header_bytes.get(8..16).map_or(0, |bytes| {
            u64::from_le_bytes(bytes.try_into().expect("eight bytes"))
        });
        let records = [(offset, sequence)];
        let mut anchors = RecordsAnchor {
            records: &records,
            end,
            summary,
        };
        let header_end = offset + header_bytes.len() as u64;
        let footer_bytes = footer.unwrap_or(&[]);
        let total = header_bytes.len() + payload.len() + footer_bytes.len();
        // A record that fits one page goes down in one write: header, payload
        // and footer staged on the stack, no allocation. A larger one is
        // written in its three parts, where the copy would cost more than the
        // two system calls it saves.
        const STAGED: usize = 4096;
        if total <= STAGED {
            let mut staged = [0u8; STAGED];
            staged[..header_bytes.len()].copy_from_slice(header_bytes);
            staged[header_bytes.len()..header_bytes.len() + payload.len()].copy_from_slice(payload);
            staged[header_bytes.len() + payload.len()..total].copy_from_slice(footer_bytes);
            self.file
                .write_all_at(offset, &staged[..total], &mut anchors)?;
            after_header()?;
        } else {
            self.file.write_all_at(offset, header_bytes, &mut anchors)?;
            after_header()?;
            self.file.write_all_at(header_end, payload, &mut anchors)?;
            if !footer_bytes.is_empty() {
                self.file.write_all_at(
                    header_end + payload.len() as u64,
                    footer_bytes,
                    &mut anchors,
                )?;
            }
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
    /// version refusal  and drops the block's keyed-tail map ,
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
        count_header_region_write();
        // The header precedes every anchor.
        self.file.overwrite_at(offset, bytes)?;
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
    /// Positional writes leave shared read cursors untouched. Append still
    /// rechecks EOF independently. Partial writes and interruptions are handled
    /// before success is returned; the four-byte word remains advisory and
    /// is not a transactional durability boundary.
    /// Overwrites bytes inside a matrix storage record. Those records keep
    /// their payload out of the record checksum and their framing never
    /// changes, so anchors and their summaries stay valid.
    pub(super) fn overwrite_matrix_bytes(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.file.overwrite_at(offset, bytes)
    }

    /// Fills `buf` from logical `offset`, past any snapshot this handle holds:
    /// matrix storage is read where it is, its ring says what is published.
    pub(super) fn read_logical_exact(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0;
        while done < buf.len() {
            match self.file.read_at(&mut buf[done..], offset + done as u64) {
                Ok(0) => return Err(Error::UnexpectedEof),
                Ok(n) => done += n,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// The primary handle, for a caller's durability barrier.
    pub(super) fn barrier_file(&mut self) -> &mut File {
        self.file.raw_mut()
    }

    pub(super) fn overwrite_record_mutable_flags(
        &mut self,
        offset: u64,
        bytes: [u8; 4],
    ) -> Result<()> {
        // An anchor the word happens to span stays as it is.
        self.file.overwrite_at(offset, &bytes)?;
        Ok(())
    }

    pub(super) fn overwrite_indexed_record(
        &mut self,
        write: RecordOverwrite,
        header_bytes: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        // In place: anchors inside the record were written with it and stay.
        // Their open summaries, and every later one, described the record as
        // it was (its sequence among them), so they are withdrawn first: a
        // crash between the two leaves an open that scans, never one that
        // trusts a stale summary.
        self.file.clear_summaries_from(write.record_offset())?;
        self.file
            .overwrite_at(write.record_offset(), header_bytes)?;
        self.file.overwrite_at(write.payload_offset(), payload)?;
        Ok(())
    }
}
