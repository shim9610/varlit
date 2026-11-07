// Native file follow operations.

impl VarlitFile {
    /// Whether the pathname still resolves to the object this handle opened.
    ///
    /// A handle is bound to an *object*, not to a name. The replacement paths
    /// publish by renaming a new file over the pathname, and on a system where
    /// that leaves an open descriptor reading the old object — which is what
    /// makes a republish safe for readers in flight — a handle goes on serving
    /// a complete, self-consistent generation with nothing to announce that a
    /// newer one exists. [`follow`](Self::follow) answers `0` forever there by
    /// design. This is how a caller learns why.
    ///
    /// `false` also for a pathname that no longer exists: the handle is still
    /// readable, but nothing at that name is it.
    ///
    /// # `false` does not always mean "republished"
    ///
    /// It means "this name did not resolve to my object", and *any* failure to
    /// reach the name produces it — a descriptor limit, a permission change, a
    /// filesystem that went away. This never errs toward calling a stale handle
    /// fresh, which is the direction that matters, but it does mean a caller
    /// must not treat `false` as proof that a newer generation is there to be
    /// had. Concretely: if [`reopen_readonly`](Self::reopen_readonly) then
    /// fails, keep the handle you have and ask again later. It is still a
    /// complete, self-consistent view; discarding it because a transient
    /// `open` failed loses a working reader to a condition that has already
    /// passed.
    ///
    /// **This is a question, not a subscription.** Varlit does not check on
    /// every read — the check is a syscall and reads are the path that must
    /// stay cheap and take `&self` — so a handle that never asks is never told.
    /// A reader that loops has a natural place to ask (see
    /// [`follow`](Self::follow)); one that does not loop has to decide for
    /// itself when freshness matters.
    ///
    /// # Why the comparison is sound
    ///
    /// It compares the operating system's object identity — device and inode on
    /// Unix, volume and file index on Windows — and those are reused once the
    /// object they named is gone. That cannot fool this check, because *this
    /// handle holds the object open*: a live descriptor pins it, so its
    /// identity cannot be handed to a different file while there is anything
    /// here to compare against.
    ///
    /// # Cost
    ///
    /// One `open` and two metadata calls. Nothing is read and nothing is
    /// framed, so this is cheap enough to ask before each round of a polling
    /// loop — but it *is* a syscall, so it is a question the caller asks rather
    /// than one every read answers.
    pub fn is_current(&self) -> Result<bool> {
        let mine = self.file.object_identity()?;
        // Any failure to reach the pathname is `false`, not an error, and it
        // has to be *any* rather than `NotFound` alone. On Windows a file
        // deleted while this handle still holds it open goes delete-pending
        // with its directory entry intact, and `CreateFile` on that name then
        // fails `ERROR_ACCESS_DENIED` — so mapping only `NotFound` turned the
        // documented "removed pathname answers false" into a hard error on the
        // one platform where the case is easiest to hit, and would have failed
        // the ungated test for it on the Windows CI job.
        //
        // Same shape as `path_resolves_to_object`, which this is the public
        // half of: a name we cannot open is not this object.
        //
        // The cost of "any" is that a *transient* failure — a descriptor limit,
        // a permission change, a network filesystem blip — is reported as
        // "not current" too, and the caller cannot tell it from a republish.
        // That is the safe direction to be wrong in (it never claims a stale
        // handle is fresh), and the rustdoc tells callers what to do about it:
        // a `reopen_readonly` that then fails is a reason to keep the handle
        // and retry, not to discard it.
        Ok(path_resolves_to_object(&self.path, &mine))
    }

    /// A fresh read-only handle on whatever the pathname resolves to now.
    ///
    /// The other half of [`is_current`](Self::is_current). The new handle has
    /// independent caches; this reader stays usable until its owner drops it.
    /// Each thread manages its own reader and chooses when to replace it.
    ///
    /// # Which route it takes
    ///
    /// The one this handle is using. A handle that retains a record directory
    /// reopens with one; a handle that keeps none reopens without one, through
    /// the lazy route, so a format carrying `index: header_tails` or an open
    /// digest reopens at a cost that does not grow with the file.
    ///
    /// # Always read-only
    ///
    /// Including from a read-write handle, which is why the name says so. A
    /// second writer is refused by the object lock this one holds, and a read
    /// view of the published generation is the useful thing to hand back.
    pub fn reopen_readonly(&self) -> Result<Self> {
        if self.index.is_retained() {
            Self::open_readonly(self.spec, &self.path)
        } else {
            Ok(Self::open_readonly_lazy_with_report(self.spec, &self.path)?.0)
        }
    }

    /// Reclaims obsolete matrix pages without waiting for existing readers.
    /// Requires a clean, explicitly synced writer. Existing readers retain their
    /// old file until follow/drop; their generation remains readable.
    pub fn compact_matrix(&mut self) -> Result<()> {
        let _permit = self.ensure_write()?;
        let pages = self.file.matrix_pages().ok_or(Error::MatrixLayoutMissing)?;
        if self.open_chunk_is_unwritten()
            || pages.is_dirty()
            || pages.head().native_eof != self.snapshot.len()
        {
            return Err(Error::InvalidFormatSpec(
                "sync matrix changes before compaction",
            ));
        }
        if let Err(error) = self.file.compact_matrix() {
            self.poison.poison();
            return Err(error);
        }
        let source = crate::matrix_generation::MatrixIo::source(self.file.matrix_pages().unwrap())?;
        crate::matrix::rebind_generation_backing(self.matrix.as_mut().unwrap(), &source);
        Ok(())
    }

    /// Confirmed generation captured by this handle. Queries never advance it.
    pub fn matrix_generation(&self) -> Option<u64> {
        self.file.matrix_pages().map(|p| p.head().generation)
    }

    /// Required companion file; its name includes the primary's creation nonce.
    pub fn matrix_generation_path(&self) -> Option<&Path> {
        self.file.matrix_pages().map(|p| p.path())
    }

    /// Extends this handle to records appended since it opened.
    ///
    /// A handle fixes its snapshot length when it opens, and every read is
    /// positional against that length. Each reader queries through `&self`
    /// while the separate writer appends: a reader can
    /// never observe a record the writer has not finished. The cost is that it
    /// never observes one the writer *has* finished either — measured on a
    /// five-record file grown to fifteen, the open handle went on reporting 6
    /// records while a fresh open of the same path reported 18.
    ///
    /// Follows the committed tail without reopening. With a cooperating current
    /// writer, private pending progress visits only new complete frames; unchanged
    /// polls do not reframe them. Publication revalidates a cached batch once.
    /// Limits apply to that range; recovery or an unknown writer resets the cache.
    ///
    /// Matrix readers also adopt the confirmed page root, even if no record
    /// bytes were appended. Compare `matrix_generation()` to observe that case.
    /// Matrix metadata verification runs again according to the open policy.
    /// Returns the record bytes gained.
    ///
    /// # `0` is not one answer
    ///
    /// Four things produce it, and the return value does not say which:
    ///
    /// 1. Nothing was appended.
    /// 2. Bytes were appended but no commit boundary covers them yet — see
    ///    *Uncommitted records* below.
    /// 3. This handle's generation is finished: the pathname was republished
    ///    and the object this handle holds will never grow again, so every
    ///    later call answers `0` too.
    /// 4. This is a read-write handle, which gains nothing — see the last
    ///    section.
    ///
    /// Only (3) is a reason to do something, and only (3) is durable; (1) and
    /// (2) both resolve on their own when the writer gets there.
    /// [`is_current`](Self::is_current) is what separates it from the rest, and
    /// a streaming reader asks it exactly here, on a path that is idle by
    /// definition:
    ///
    /// ```text
    /// if reader.follow()? == 0 {
    ///     if !reader.is_current()? { reader = reader.reopen_readonly()?; }
    ///     wait();
    /// }
    /// ```
    ///
    /// **That idiom serves a reader that is looping, and only that reader.** A
    /// handle that never calls this — one that opened, and reads on demand
    /// without following anything — is never informed of a republish by any
    /// mechanism varlit has, because a handle learns it only by asking. See
    /// `known-limitations.md` §4.2b for what each shape of reader needs.
    ///
    /// # Uncommitted records are still not visible
    ///
    /// Under a `transaction_marker` policy a record becomes visible when a
    /// marker covers it, exactly as at open — the same `committed_prefix_len`
    /// boundary decides both. A file whose tail is appended but not yet
    /// committed answers `0`, and the same call answers the whole run once the
    /// marker lands.
    ///
    /// # What it deliberately does not do
    ///
    /// **Primary-file replacement requires reopening.** Follow retains the
    /// primary file object this handle opened. Matrix companion compaction is
    /// different: its persisted compaction epoch explicitly permits adopting
    /// the replacement companion while retaining that same primary object.
    ///
    /// **Matrix generations advance atomically with their confirmed log EOF.**
    /// Ordinary publications diff immutable roots and update only changed bitmap
    /// index entries, verification pages and cache entries. Only a changed
    /// compaction epoch rebuilds the whole layout. Failed preparation leaves
    /// the captured generation intact. Matrix-only advancement returns zero
    /// appended bytes; inspect `matrix_generation()` to distinguish it.
    ///
    /// **A read-write handle gains nothing and answers `0`.** varlit admits one
    /// writer per object, and that writer's own appends already extend its
    /// snapshot, so its end is the object's end by construction.
    pub fn follow(&mut self) -> Result<u64> {
        let _permit = self.ensure_not_poisoned()?;
        if self.mode == OpenMode::ReadWrite {
            return Ok(0);
        }
        // One short-lived staged value; keep ordinary follow allocation-free here.
        #[allow(clippy::large_enum_variant)]
        enum MatrixFollow {
            Compacted(crate::matrix::MatrixLayout),
            Delta(crate::matrix::MatrixDelta),
        }
        impl MatrixFollow {
            fn apply(self, layout: &mut Option<crate::matrix::MatrixLayout>) {
                match self {
                    Self::Compacted(next) => *layout = Some(next),
                    Self::Delta(delta) => delta.apply(layout.as_mut().unwrap()),
                }
            }
        }
        let mut matrix_next = if let Some(pages) = self.file.matrix_pages() {
            let mut next = pages.refreshed()?;
            if next.head().generation <= pages.head().generation {
                return Ok(0);
            }
            let mut base = self.snapshot.try_clone_file()?;
            let header_len = read_file_header(self.spec, &mut base)?;
            let layout_start = header_len
                .checked_add(MATRIX_CREATION_NONCE_REGION_LEN as u64)
                .ok_or(Error::InvalidMatrixLayout)?;
            let len = next.head().native_eof;
            let layout = if next.head().compaction_epoch != pages.head().compaction_epoch {
                MatrixFollow::Compacted(crate::matrix::read_layout_at_len(
                    self.spec,
                    &mut next,
                    layout_start,
                    len,
                )?)
            } else {
                MatrixFollow::Delta(crate::matrix::MatrixDelta::prepare(
                    self.matrix.as_mut().ok_or(Error::InvalidMatrixLayout)?,
                    pages,
                    &next,
                    layout_start,
                )?)
            };
            Some((layout, next))
        } else {
            None
        };
        let from = self.snapshot.len();
        // The object, not the pathname. A rename may have put a different file
        // at the path, and framing *that* file's bytes at this handle's offsets
        // is the one thing this must never do.
        let mut file = self.snapshot.try_clone_file()?;
        let physical_metadata = file.metadata()?;
        let file_len = matrix_next
            .as_ref()
            .map_or(physical_metadata.len(), |(_, p)| p.head().native_eof);
        if file_len < from {
            return Err(Error::InvalidMatrixLayout);
        }
        if file_len == from {
            if let Some((layout, pages)) = matrix_next.take() {
                layout.apply(&mut self.matrix);
                self.file.adopt_matrix(pages);
                self.chunk_directory = std::cell::OnceCell::new();
            }
            return Ok(0);
        }
        let modified = physical_metadata.modified().ok();
        let mut pending = match self.follow_scan.take() {
            Some(pending) => pending,
            None => crate::try_box(FollowScan::new(self.spec, from, &self.path)?)?,
        };
        let mut progress = pending.progress.take().expect("pending scan owns progress");
        let cached = progress.next > from;
        let epoch = pending.epoch();
        let unchanged_input = progress.from == from
            && pending.observed_len > from
            && file_len == pending.observed_len
            && modified.is_some()
            && modified == pending.observed_modified
            && epoch == pending.writer_epoch;
        if unchanged_input {
            pending.progress = Some(progress);
            self.follow_scan = Some(pending);
            if let Some((layout, pages)) = matrix_next.take() {
                layout.apply(&mut self.matrix);
                self.file.adopt_matrix(pages);
                self.chunk_directory = std::cell::OnceCell::new();
            }
            return Ok(0);
        }
        let reusable = progress.from == from
            && file_len >= pending.observed_len
            && (file_len != pending.observed_len || modified == pending.observed_modified)
            && (!cached || (epoch.is_some() && epoch == pending.writer_epoch));
        if !reusable {
            pending.rows.reset();
            progress = ScanProgress::new(self.spec, from, 0, Some(from))?;
        }
        let resumed = progress.next > from;
        progress = resume_record_scan(
            self.spec,
            &mut file,
            progress,
            ScanIntent::ReadOnly,
            &mut pending.rows,
            file_len,
        )?;
        // A pending prefix can change before it is committed (replacement or
        // writer recovery). Cached parse state must never authorize publication.
        // Replay once when a new commit is discovered, not on every idle poll.
        if resumed
            && progress
                .scanned
                .physical_end_at_commit
                .is_some_and(|end| end > from)
        {
            pending.rows.rewind_for_replay();
            drop(progress);
            progress = resume_record_scan(
                self.spec,
                &mut file,
                ScanProgress::new(self.spec, from, 0, Some(from))?,
                ScanIntent::ReadOnly,
                &mut pending.rows,
                file_len,
            )?;
        }
        pending.observed_len = file_len;
        pending.observed_modified = modified;
        pending.writer_epoch = epoch;
        let has_commit = !self.spec.commit_policy.is_transaction_marker()
            || progress
                .scanned
                .physical_end_at_commit
                .is_some_and(|end| end > from);
        if !has_commit {
            pending.progress = Some(progress);
            self.follow_scan = Some(pending);
            if let Some((layout, pages)) = matrix_next.take() {
                layout.apply(&mut self.matrix);
                self.file.adopt_matrix(pages);
                self.chunk_directory = std::cell::OnceCell::new();
            }
            return Ok(0);
        }
        pending.rows.finish(self.spec, &mut progress.scanned)?;
        let ScanProgress {
            scanned,
            all_sequences,
            ..
        } = progress;
        drop(all_sequences);
        let framed = &pending.rows;
        let logical_len = scanned.physical_end(from);
        if logical_len <= from {
            // A matrix generation can advance without a record commit marker.
            if let Some((layout, pages)) = matrix_next.take() {
                layout.apply(&mut self.matrix);
                self.file.adopt_matrix(pages);
                self.chunk_directory = std::cell::OnceCell::new();
            }
            return Ok(0);
        }
        // Sequences are file-global and strictly increasing, and the scan's own
        // uniqueness check only sees the run it framed. The run begins after
        // everything this handle already holds, so its lowest sequence must
        // too. This is the check that the run really is this object's
        // continuation rather than bytes that came from somewhere else.
        let last_entry = framed.last_entry()?;
        if scanned
            .minimum_sequence
            .is_some_and(|lowest| match self.sequence_state {
                SequenceState::Available(next) => lowest < next,
                SequenceState::Exhausted => true,
            })
        {
            return Err(Error::InvalidCanonicalEncoding(
                "followed records repeat a sequence this handle already holds",
            ));
        }
        let next_progress = ScanProgress::new(self.spec, logical_len, 0, Some(logical_len))?;
        let snapshot = self.snapshot.with_len(logical_len)?;
        if self.index.is_retained() {
            let first = self.index.len();
            let holding = first.checked_add(framed.record_count()).ok_or(
                Error::ResourceArithmeticOverflow {
                    resource: "record count",
                },
            )?;
            self.spec
                .read_limits
                .check(ReadLimitKey::Records, holding as u64)?;
            let mut checkpoint = self.checkpoint_cadence;
            let mut segment = self.segment_cursor;
            for (at, entry) in framed.entries().enumerate() {
                let entry = entry?;
                checkpoint.note_appended(first + at, entry.block_id);
                segment.note_appended(first + at, entry.block_id, entry.checked_physical_end()?);
            }
            // Fallible staging ends here. On scratch I/O failure extend rolls
            // back its logical length, preserving the complete old snapshot.
            self.index.extend(framed.entries())?;
            self.checkpoint_cadence = checkpoint;
            self.segment_cursor = segment;
        }
        self.snapshot = snapshot;
        // The entries just installed describe records past the end the old
        // snapshot allowed, and a read beyond a snapshot's logical length is
        // refused rather than served.
        let snapshot = self.snapshot.clone();
        self.index.rebind(snapshot);
        // Merged, not replaced: a block with no record in this range keeps the
        // tail it already had.
        for (block_id, record_offset) in scanned.newest_pairs() {
            self.block_tails.note_appended(block_id, record_offset);
        }
        self.sequence_state = scanned.sequence_state();
        if let Some(entry) = last_entry {
            self.last_resident_block_id = Some(entry.block_id);
        }
        // A record just adopted may be the newest of its key.
        self.keyed_tails.invalidate_all();
        pending.rows.reset();
        pending.progress = Some(next_progress);
        self.follow_scan = Some(pending);
        // And a matrix chunk record is an ordinary append-log record, so a run
        // may have brought new chunks. The chunk directory is built once and
        // cached in a `OnceCell`, which was sound to keep forever only while a
        // handle's snapshot could not grow — it can now. Measured before this
        // line existed: a followed handle answered `MatrixNotCommitted` for a
        // cell a fresh open of the same file read back.
        self.chunk_directory = std::cell::OnceCell::new();
        if let Some((layout, pages)) = matrix_next.take() {
            layout.apply(&mut self.matrix);
            self.file.adopt_matrix(pages);
        }
        Ok(logical_len - from)
    }
}
