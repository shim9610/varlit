//! The primary file in logical offsets.
//!
//! Every byte offset above the file I/O layer — record offsets, chains,
//! sidecar pointers, snapshot lengths — is *logical*: the append log as if it
//! were contiguous. An anchored file (`crate::anchor`) interleaves fixed
//! reserved ranges with that log, so its physical offsets differ. This type is
//! the one place the two meet. It deliberately does not dereference to
//! [`File`]: a length, seek, read, write or truncation has to go through a
//! method that translates, and the raw handle is reachable only through
//! [`PrimaryFile::raw`] for what offsets do not touch — locks, identity,
//! barriers and memory maps that translate their own windows.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::anchor::{Anchor, AnchorLayout, Segment};
use crate::{Error, Result};

/// Opens a data file at a caller's path, accepting only a regular file.
///
/// A FIFO with no peer, or a device, can block inside `open` itself, before
/// any check could run, and nothing the library does afterwards would end
/// that wait. On Unix the open therefore uses `O_NONBLOCK`, which returns at
/// once for those and which Unix ignores for regular files; the flag stays
/// set on the returned handle, where it has no effect. The type is then
/// checked on the open handle rather than on the path, so a name swapped
/// between a check and the open cannot bring a different object through.
/// Anything other than a regular file is refused with `InvalidInput`.
pub(crate) fn open_data_file(
    options: &mut std::fs::OpenOptions,
    path: impl AsRef<std::path::Path>,
) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    refuse_special_file(&file)?;
    Ok(file)
}

/// `InvalidInput` unless `file` is a regular file.
pub(crate) fn refuse_special_file(file: &File) -> std::io::Result<()> {
    if file.metadata()?.file_type().is_file() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file: Varlit reads and writes only regular files",
        ))
    }
}

#[derive(Debug)]
pub(crate) struct PrimaryFile {
    file: File,
    layout: Option<AnchorLayout>,
    /// The logical position of this handle's `Read`/`Seek` cursor.
    position: u64,
    /// The record sequential writes are currently writing, as
    /// `(start, sequence, end)`: what an anchor they reach describes.
    record: Option<(u64, u64, u64)>,
    /// The open summary for the anchor the declared record reaches, encoded
    /// when the record was declared. Only a record that reaches an anchor
    /// carries one, so a rewrite pays for it once per anchor spacing.
    summary: Option<Box<[u8]>>,
    /// A reader's bound: the file's `VPUB` block, read by every [`Self::len`].
    bound: Option<ReaderBound>,
    /// A writer's `VPUB` block, which `RecordFile::publish_end` moves after
    /// each publication. Never copied to a clone: one writer per file.
    published: Option<WriterPublished>,
    /// Whether syncs also flush the drive's write cache.
    flush: crate::DeviceFlush,
}

/// A writer's published end.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WriterPublished {
    /// Header offset of the two slots.
    pub slots_at: u64,
    /// Sequence of the newest slot.
    pub seq: u64,
    /// The end the newest slot publishes, `None` when it is inactive or
    /// blank.
    pub end: Option<u64>,
}

/// Where a reader finds the published end, and the bound it last read.
#[derive(Debug)]
struct ReaderBound {
    /// Header offset of the two slots.
    slots_at: u64,
    /// The start of the append log: never hidden.
    floor: u64,
    /// The bound last read, kept when a read finds both slots unreadable.
    last: std::sync::atomic::AtomicU64,
    /// The file's logical length when last measured: a bound within it
    /// needs no new measurement.
    seen: std::sync::atomic::AtomicU64,
}

/// Supplies the anchor a write reaches: given its index and logical offset,
/// the record spanning that offset, from the records being written.
pub(crate) trait AnchorSource {
    fn anchor(&mut self, k: u64, logical: u64) -> Result<Anchor>;

    /// The open summary to store with the anchor, when the writer knows one.
    fn summary(&self) -> Option<&dyn crate::anchor::SummarySource> {
        None
    }
}

/// For writes that cannot reach an anchor: header regions, and every write to
/// an unanchored file.
pub(crate) struct NoAnchor;
impl AnchorSource for NoAnchor {
    fn anchor(&mut self, _k: u64, _logical: u64) -> Result<Anchor> {
        Err(Error::InvalidCanonicalEncoding(
            "write reached an anchor without a record to describe it",
        ))
    }
}

/// The anchor for a write of whole records, given as `(start, sequence)` in
/// order and ending at `end`.
pub(crate) struct RecordsAnchor<'a> {
    pub records: &'a [(u64, u64)],
    pub end: u64,
    /// The state before the first of `records`, for an open to start from.
    /// Only meaningful while `records` is a single record.
    pub summary: Option<&'a dyn crate::anchor::SummarySource>,
}
impl AnchorSource for RecordsAnchor<'_> {
    fn summary(&self) -> Option<&dyn crate::anchor::SummarySource> {
        self.summary
    }

    fn anchor(&mut self, k: u64, logical: u64) -> Result<Anchor> {
        let index = self
            .records
            .partition_point(|(start, _)| *start <= logical)
            .checked_sub(1)
            .ok_or(Error::InvalidCanonicalEncoding(
                "anchor precedes the records being written",
            ))?;
        let (record_start, sequence) = self.records[index];
        let record_end = self
            .records
            .get(index + 1)
            .map_or(self.end, |(start, _)| *start);
        if logical >= record_end {
            return Err(Error::InvalidCanonicalEncoding(
                "anchor follows the records being written",
            ));
        }
        Ok(Anchor {
            k,
            logical,
            record_start,
            record_end,
            sequence,
        })
    }
}

impl PrimaryFile {
    pub(crate) fn new(file: File, layout: Option<AnchorLayout>) -> Self {
        Self {
            file,
            layout,
            position: 0,
            record: None,
            summary: None,
            bound: None,
            published: None,
            flush: crate::DeviceFlush::Full,
        }
    }

    /// The writer's published end state, if it took over a `VPUB` block.
    pub(crate) const fn writer_published(&self) -> Option<WriterPublished> {
        self.published
    }

    /// Takes over the `VPUB` slots at `slots_at`: later
    /// [`Self::write_published_end`] calls continue after the newest slot.
    pub(crate) fn adopt_writer_published(&mut self, slots_at: u64) -> Result<()> {
        use crate::published_end::{ACTIVE, SLOT_LEN, decode_slot};
        let mut slots = [0u8; 2 * SLOT_LEN];
        read_exact_at(&self.file, slots_at, &mut slots)?;
        let mut seq = 0u64;
        let mut end = None;
        for bytes in slots.as_chunks::<SLOT_LEN>().0 {
            if let Some(slot) = decode_slot(bytes)
                && slot.seq >= seq
            {
                seq = slot.seq;
                end = (slot.flags & ACTIVE != 0).then_some(slot.end);
            }
        }
        self.published = Some(WriterPublished { slots_at, seq, end });
        Ok(())
    }

    /// Writes the slot after the newest: publishing `Some(end)`, or
    /// withdrawing with `None`. Nothing when the handle owns no `VPUB` block
    /// or the newest slot already says so. One positional write of 24 bytes
    /// in the header, no barrier: a slot torn by a crash fails its checksum
    /// and leaves the previous one.
    pub(crate) fn write_published_end(&mut self, end: Option<u64>) -> Result<()> {
        use crate::published_end::{ACTIVE, SLOT_LEN, Slot, encode_slot};
        let Some(mut published) = self.published else {
            return Ok(());
        };
        if published.end == end {
            return Ok(());
        }
        let seq = published
            .seq
            .checked_add(1)
            .ok_or(crate::Error::LengthOverflow {
                value: published.seq,
            })?;
        let bytes = encode_slot(Slot {
            end: end.unwrap_or(0),
            seq,
            flags: if end.is_some() { ACTIVE } else { 0 },
        });
        // The slot after the newest: a torn write leaves the newest standing.
        let slot = crate::publication_protocol::slot_after(published.seq);
        let at = published.slots_at + (slot * SLOT_LEN) as u64;
        // The header precedes every anchor: a physical write.
        write_all_at(&self.file, at, &bytes)?;
        published.seq = seq;
        published.end = end;
        self.published = Some(published);
        Ok(())
    }

    /// Makes [`Self::len`] stop at the published end the `VPUB` slots at
    /// `slots_at` name. For read-only handles; `floor` is the start of the
    /// append log.
    pub(crate) fn bound_by_published_end(&mut self, slots_at: u64, floor: u64) {
        self.bound = Some(ReaderBound {
            slots_at,
            floor,
            last: std::sync::atomic::AtomicU64::new(floor),
            seen: std::sync::atomic::AtomicU64::new(0),
        });
    }

    /// Reads both `VPUB` slots, retrying a read whose slots do not hold (a
    /// writer may be rewriting one).
    fn read_published(&self, slots_at: u64) -> std::io::Result<crate::published_end::Published> {
        use crate::published_end::{Published, SLOT_LEN, read};
        let mut slots = [0u8; 2 * SLOT_LEN];
        let mut published = Published::Unreadable;
        for _ in 0..3 {
            read_exact_at(&self.file, slots_at, &mut slots)?;
            published = read(&slots);
            if published != Published::Unreadable {
                break;
            }
        }
        Ok(published)
    }

    /// Declares the record that following sequential writes belong to, so an
    /// anchor they reach can describe it. Rewriters call this before writing
    /// each record.
    pub(crate) fn begin_record(&mut self, start: u64, sequence: u64, end: u64) {
        self.record = Some((start, sequence, end));
        self.summary = None;
    }

    /// [`Self::begin_record`] for a writer that knows the state of the file
    /// before this record. When the record reaches an anchor, `summary` is
    /// encoded now and stored with that anchor, so an open can start there.
    /// A summary that does not fit, or memory for it that is not available,
    /// leaves the anchor without one: the open then scans, as it always may.
    pub(crate) fn begin_record_summarized(
        &mut self,
        start: u64,
        sequence: u64,
        end: u64,
        summary: &dyn crate::anchor::SummarySource,
    ) {
        self.begin_record(start, sequence, end);
        let Some(layout) = self.layout else {
            return;
        };
        if layout.anchors_before(end) == layout.anchors_before(start) {
            return;
        }
        let Ok(hole) = usize::try_from(layout.hole()) else {
            return;
        };
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(hole).is_err() {
            return;
        }
        bytes.resize(hole, 0);
        if summary.write_summary(&mut bytes) {
            self.summary = Some(bytes.into_boxed_slice());
        }
    }

    pub(crate) const fn layout(&self) -> Option<AnchorLayout> {
        self.layout
    }

    /// Installs the layout read from the file header. Only the header lies
    /// before it, and the header is never moved by an anchor.
    pub(crate) fn set_layout(&mut self, layout: Option<AnchorLayout>) {
        self.layout = layout;
    }

    /// The raw handle, for what physical offsets do not touch: locks,
    /// identity, barriers, and memory maps that translate their own windows.
    pub(crate) const fn raw(&self) -> &File {
        &self.file
    }

    pub(crate) fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            layout: self.layout,
            position: 0,
            record: None,
            summary: None,
            bound: self.bound.as_ref().map(|bound| ReaderBound {
                slots_at: bound.slots_at,
                floor: bound.floor,
                last: std::sync::atomic::AtomicU64::new(
                    bound.last.load(std::sync::atomic::Ordering::Relaxed),
                ),
                seen: std::sync::atomic::AtomicU64::new(
                    bound.seen.load(std::sync::atomic::Ordering::Relaxed),
                ),
            }),
            published: None,
            flush: self.flush,
        })
    }

    /// The logical length: the append log as written, without anchors and
    /// without an anchor the log has reached but not passed. A reader bound
    /// by a published end (see [`Self::bound_by_published_end`]) is told no
    /// more than that end.
    pub(crate) fn len(&self) -> std::io::Result<u64> {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(bound) = &self.bound else {
            return self.unbounded_len();
        };
        let published = self.read_published(bound.slots_at)?;
        // An end within the length last measured needs no new measurement:
        // a follower's idle poll is one read of the slots, as it was one
        // measurement before the published end existed.
        if let Some(len) =
            crate::published_end::known_len(bound.floor, published, bound.seen.load(Relaxed))
        {
            bound.last.store(len, Relaxed);
            return Ok(len);
        }
        let logical = self.unbounded_len()?;
        bound.seen.store(logical, Relaxed);
        let last = bound.last.load(Relaxed);
        let (len, next) = crate::published_end::visible_len(logical, bound.floor, published, last);
        bound.last.store(next, Relaxed);
        Ok(len)
    }

    /// The logical length of the file as written, whatever is published.
    pub(crate) fn unbounded_len(&self) -> std::io::Result<u64> {
        let physical = self.file.metadata()?.len();
        Ok(self
            .layout
            .map_or(physical, |layout| layout.logical_len(physical)))
    }

    /// Syncs under `flush` from now on; see [`crate::DeviceFlush`].
    pub(crate) fn with_device_flush(mut self, flush: crate::DeviceFlush) -> Self {
        self.flush = flush;
        self
    }

    pub(crate) fn sync_all(&self) -> std::io::Result<()> {
        crate::durability::sync_all(&self.file, self.flush)
    }

    pub(crate) fn sync_data(&self) -> std::io::Result<()> {
        crate::durability::sync_data(&self.file, self.flush)
    }

    /// Truncates or extends the log to logical length `len`. Anchors before
    /// the new end are kept; one at or after it is removed with the log.
    pub(crate) fn set_len(&self, len: u64) -> std::io::Result<()> {
        let physical = match self.layout {
            None => len,
            Some(layout) => layout
                .physical_end(len)
                .ok_or_else(|| std::io::Error::other("anchored length overflows"))?,
        };
        self.file.set_len(physical)
    }

    /// Tells the kernel the logical range `offset..offset + len` is about to
    /// be read whole, so it can fetch it with large requests ahead of a reader
    /// that walks it a fixed buffer at a time. Only a hint: the pages go to the
    /// kernel's cache, nothing is held by this process, and a refusal changes
    /// nothing but speed.
    #[allow(unsafe_code, reason = "a read-ahead hint to the kernel")]
    pub(crate) fn will_need(&self, offset: u64, len: u64) {
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
        {
            use std::os::fd::AsRawFd;
            let fd = self.file.as_raw_fd();
            let advise = |physical: u64, len: u64| {
                if let (Ok(at), Ok(len)) =
                    (libc::off_t::try_from(physical), libc::off_t::try_from(len))
                {
                    // SAFETY: `fd` is this handle's open file for the whole
                    // call, and `posix_fadvise` reads nothing but its integer
                    // arguments.
                    unsafe { libc::posix_fadvise(fd, at, len, libc::POSIX_FADV_WILLNEED) };
                }
            };
            match self.layout {
                None => advise(offset, len),
                Some(layout) => {
                    let _ = layout.split(offset, len, |segment| {
                        if let Segment::Data { physical, len, .. } = segment {
                            advise(physical, len);
                        }
                        Ok(())
                    });
                }
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
        let _ = (offset, len);
    }

    /// Reads as many bytes as one physical run holds, at logical `offset`.
    pub(crate) fn read_at(&self, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
        let Some(layout) = self.layout else {
            return crate::snapshot::read_at(&self.file, buffer, offset);
        };
        let mut first = None;
        let _ = layout.split(offset, buffer.len() as u64, |segment| {
            if first.is_none()
                && let Segment::Data { physical, len, .. } = segment
            {
                first = Some((physical, len));
            }
            Ok(())
        });
        let Some((physical, len)) = first else {
            return Ok(0);
        };
        let len = usize::try_from(len)
            .unwrap_or(buffer.len())
            .min(buffer.len());
        crate::snapshot::read_at(&self.file, &mut buffer[..len], physical)
    }

    /// Writes `bytes` at logical `offset`, filling every anchor the write
    /// reaches from `anchors` first. All-or-error, like `write_all`.
    pub(crate) fn write_all_at(
        &self,
        offset: u64,
        bytes: &[u8],
        anchors: &mut dyn AnchorSource,
    ) -> Result<()> {
        let Some(layout) = self.layout else {
            write_all_at(&self.file, offset, bytes)?;
            return Ok(());
        };
        // An anchor is at most 4 KiB (`AnchorPolicy`), encoded on the stack:
        // the append path allocates nothing per record or per anchor.
        let mut scratch = [0u8; 4096];
        layout.split(offset, bytes.len() as u64, |segment| {
            match segment {
                Segment::Data {
                    physical,
                    logical,
                    len,
                } => {
                    let from = usize::try_from(logical - offset).expect("within the write");
                    let to = from + usize::try_from(len).expect("within the write");
                    write_all_at(&self.file, physical, &bytes[from..to])?;
                }
                Segment::Anchor {
                    k,
                    physical,
                    logical,
                } => {
                    let anchor = anchors.anchor(k, logical)?;
                    let hole = usize::try_from(layout.hole())
                        .ok()
                        .filter(|hole| *hole <= scratch.len())
                        .ok_or(Error::InvalidCanonicalEncoding("anchor exceeds 4 KiB"))?;
                    anchor.encode(&mut scratch[..hole]);
                    if let Some(summary) = anchors.summary()
                        && summary.write_summary(&mut scratch[..hole])
                    {
                        crate::anchor::reseal(&mut scratch[..hole]);
                    }
                    write_all_at(&self.file, physical, &scratch[..hole])?;
                }
            }
            Ok(())
        })
    }

    /// Overwrites bytes already in the log at logical `offset`, leaving every
    /// anchor in the range as it is. For in-place rewrites of records whose
    /// anchors were written when they were appended.
    pub(crate) fn overwrite_at(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        let Some(layout) = self.layout else {
            write_all_at(&self.file, offset, bytes)?;
            return Ok(());
        };
        layout.split(offset, bytes.len() as u64, |segment| {
            if let Segment::Data {
                physical,
                logical,
                len,
            } = segment
            {
                let from = usize::try_from(logical - offset).expect("within the write");
                let to = from + usize::try_from(len).expect("within the write");
                write_all_at(&self.file, physical, &bytes[from..to])?;
            }
            Ok(())
        })
    }

    /// The raw handle, mutably, for regions no anchor reaches: the file header
    /// and a matrix file's matrix region, both before the append log.
    pub(crate) fn raw_mut(&mut self) -> &mut File {
        &mut self.file
    }

    /// Reads anchor `k` if the file holds it intact.
    /// Anchor `k` and the open summary it carries, if both are intact.
    pub(crate) fn anchor_summary(
        &self,
        k: u64,
        append_start: u64,
    ) -> Result<Option<(Anchor, crate::anchor::AnchorSummary)>> {
        let Some((anchor, bytes)) = self.anchor_bytes(k)? else {
            return Ok(None);
        };
        Ok(crate::anchor::AnchorSummary::decode(
            &bytes[..usize::try_from(self.layout.map_or(0, |l| l.hole())).unwrap_or(0)],
            append_start,
            anchor.record_start,
        )
        .map(|summary| (anchor, summary)))
    }

    /// Withdraws the open summary of every anchor whose summary covers the
    /// record at logical `from`: a record there was rewritten in place, and a
    /// summary that predates the rewrite no longer describes the file. The
    /// anchors themselves stay valid. One small write per anchor at or past
    /// the record.
    pub(crate) fn clear_summaries_from(&self, from: u64) -> Result<()> {
        let Some(layout) = self.layout else {
            return Ok(());
        };
        let physical_len = self.file.metadata()?.len();
        let mut k = layout.anchors_before(from).max(1);
        while let Some(physical) = layout.anchor_physical(k) {
            if physical.saturating_add(layout.hole()) > physical_len {
                break;
            }
            if let Some((anchor, mut bytes)) = self.anchor_bytes(k)?
                && anchor.record_start > from
            {
                let hole = usize::try_from(layout.hole()).expect("anchor fits memory");
                crate::anchor::clear_summary(&mut bytes[..hole]);
                write_all_at(&self.file, physical, &bytes[..hole])?;
            }
            k += 1;
        }
        Ok(())
    }

    fn anchor_bytes(&self, k: u64) -> Result<Option<(Anchor, [u8; 4096])>> {
        let Some(layout) = self.layout else {
            return Ok(None);
        };
        let Some(physical) = layout.anchor_physical(k) else {
            return Ok(None);
        };
        let hole = usize::try_from(layout.hole()).expect("anchor fits memory");
        if hole > 4096 || self.file.metadata()?.len() < physical.saturating_add(layout.hole()) {
            return Ok(None);
        }
        let mut bytes = [0u8; 4096];
        let mut done = 0;
        while done < hole {
            match crate::snapshot::read_at(
                &self.file,
                &mut bytes[done..hole],
                physical + done as u64,
            ) {
                Ok(0) => return Ok(None),
                Ok(n) => done += n,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Anchor::decode(&layout, k, &bytes[..hole]).map(|anchor| (anchor, bytes)))
    }

    pub(crate) fn anchor(&self, k: u64) -> Result<Option<Anchor>> {
        let mut bytes = [0u8; 4096];
        Ok(self.anchor_into(k, &mut bytes)?.map(|(anchor, _)| anchor))
    }

    /// Anchor `k` and its whole reserved range, read into `bytes`: the
    /// decoded anchor and the range's length, or `None` when the file has no
    /// intact anchor `k`. The range holds any summary the anchor carries.
    pub(crate) fn anchor_into(
        &self,
        k: u64,
        bytes: &mut [u8; 4096],
    ) -> Result<Option<(Anchor, usize)>> {
        let Some(layout) = self.layout else {
            return Ok(None);
        };
        let Some(physical) = layout.anchor_physical(k) else {
            return Ok(None);
        };
        let Some(hole) = usize::try_from(layout.hole())
            .ok()
            .filter(|hole| *hole <= bytes.len())
        else {
            return Ok(None);
        };
        let end = physical.saturating_add(layout.hole());
        if self.file.metadata()?.len() < end {
            return Ok(None);
        }
        let range = &mut bytes[..hole];
        let mut done = 0;
        while done < hole {
            match crate::snapshot::read_at(&self.file, &mut range[done..], physical + done as u64) {
                Ok(0) => return Ok(None),
                Ok(n) => done += n,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Anchor::decode(&layout, k, range).map(|anchor| (anchor, hole)))
    }
}

pub(crate) fn read_exact_at(file: &File, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < buffer.len() {
        match crate::snapshot::read_at(file, &mut buffer[done..], offset + done as u64) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => done += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_all_at(file: &File, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(bytes, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0usize;
        while done < bytes.len() {
            match file.seek_write(&bytes[done..], offset + done as u64) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(n) => done += n,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, offset, bytes);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "positional writes are unavailable on this platform",
        ))
    }
}

/// Unit tests build unanchored primaries straight from files; production code
/// has no conversion and has to say what layout a handle has.
#[cfg(test)]
impl From<File> for PrimaryFile {
    fn from(file: File) -> Self {
        Self::new(file, None)
    }
}

impl Read for PrimaryFile {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let n = self.read_at(buffer, self.position)?;
        self.position += n as u64;
        Ok(n)
    }
}

impl Seek for PrimaryFile {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let next = match from {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => self.len()?.checked_add_signed(delta),
        }
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid seek"))?;
        self.position = next;
        Ok(next)
    }
}

/// Sequential writes at the cursor. An anchor a write reaches describes the
/// record declared by [`PrimaryFile::begin_record`]; with none declared — a
/// file header, or an unanchored file — a write that would reach an anchor is
/// refused rather than written through it.
impl Write for PrimaryFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self.record {
            Some((start, sequence, end)) => {
                let records = [(start, sequence)];
                let encoded = self.summary.as_deref().map(crate::anchor::EncodedSummary);
                self.write_all_at(
                    self.position,
                    bytes,
                    &mut RecordsAnchor {
                        records: &records,
                        end,
                        summary: encoded
                            .as_ref()
                            .map(|summary| summary as &dyn crate::anchor::SummarySource),
                    },
                )
            }
            None => self.write_all_at(self.position, bytes, &mut NoAnchor),
        }
        .map_err(crate::Error::into_io)?;
        self.position += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_writes_flow_around_anchors_and_read_back_contiguously() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("anchored");
        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let layout = AnchorLayout::new(16, 256, 64).unwrap();
        let primary = PrimaryFile::new(file, Some(layout));
        // Records of 50 bytes each, sequence = index, from offset 16.
        let records: Vec<(u64, u64)> = (0..40).map(|i| (16 + i * 50, i)).collect();
        let bytes: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
        primary
            .write_all_at(
                16,
                &bytes,
                &mut RecordsAnchor {
                    records: &records,
                    end: 2016,
                    summary: None,
                },
            )
            .unwrap();
        assert_eq!(primary.len().unwrap(), 2016);
        let mut back = vec![0u8; 2000];
        let mut done = 0;
        while done < back.len() {
            done += primary
                .read_at(&mut back[done..], 16 + done as u64)
                .unwrap();
        }
        assert_eq!(back, bytes);
        // Every anchor names the record that spans it.
        for k in 1.. {
            let Some(at) = layout.anchor_logical(k) else {
                break;
            };
            if at >= 2016 {
                break;
            }
            let anchor = primary.anchor(k).unwrap().expect("written anchor");
            assert!(anchor.record_start <= at && at < anchor.record_end);
            assert_eq!((anchor.record_start - 16) % 50, 0);
            assert_eq!(anchor.sequence, (anchor.record_start - 16) / 50);
        }
        // Truncating logically drops the anchors after the new end.
        let cut = layout.anchor_logical(3).unwrap();
        primary.set_len(cut).unwrap();
        assert_eq!(primary.len().unwrap(), cut);
        assert_eq!(primary.anchor(3).unwrap(), None);
        assert!(primary.anchor(2).unwrap().is_some());
        // A write that reaches an anchor needs to say what it describes.
        assert!(primary.write_all_at(cut, &[1; 8], &mut NoAnchor).is_err());
    }
}
