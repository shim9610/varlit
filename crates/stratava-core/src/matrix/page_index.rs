use super::*;

/// A bitmap lent out **for persisted-page-index maintenance only**.
///
/// Round 15 lent the raw `&mut SparseBitmap` for this
/// (`CrcValidEvidence::page_index_mirror_mut`) and held it to one call site
/// with a source gate. Re-verification showed what a `&mut SparseBitmap`
/// is: `*evidence.page_index_mirror_mut() = attacker_bits;` replaces a
/// block's whole CRC-validity map — completeness stays `true`, so the F-04
/// witness then reads bits no enumeration ever produced — and
/// `.page_index_mirror_mut().set(ordinal, true)` writes one in place. Both
/// were two lines or fewer, and both are exactly what deleting
/// `CrcValidEvidence::new` was meant to prevent.
///
/// The wrapper is the fix, and it is a compiler refusal rather than a
/// count: the borrow is a private field of this module, so outside it the
/// mirror supports **only** the index-maintenance operations declared
/// below. Assignment through it is a type error, and `SparseBitmap`'s own
/// methods — `set`, `get`, `clear`, `prepare_byte_write` — are unreachable
/// through it. Commit maps are wrapped the same way at the same call site,
/// so the shared helpers keep one signature.
pub(super) struct PageIndexMirror<'a> {
    bits: &'a mut SparseBitmap,
}

impl<'a> PageIndexMirror<'a> {
    /// Wraps a bitmap for index maintenance.
    ///
    /// Public to the file on purpose: it only ever *narrows* what its
    /// caller already holds. It takes a `&mut SparseBitmap` and hands back
    /// something strictly weaker, so it is no route out of an evidence
    /// type — the direction that matters is that nothing here hands a
    /// `&mut SparseBitmap` back.
    pub(super) fn new(bits: &'a mut SparseBitmap) -> Self {
        Self { bits }
    }

    /// Whether the persisted index already names `page`.
    pub(super) fn indexes_page(&self, page: u64, read: BitmapRead<'_>) -> Result<bool> {
        self.bits.indexes_page(page, read)
    }

    /// Set bits the page holding `byte_index` would have after the pending
    /// byte write — the test for "this page still needs an entry".
    pub(super) fn page_ones_after(
        &mut self,
        byte_index: u64,
        byte_value: u8,
        read: BitmapRead<'_>,
    ) -> Result<u64> {
        self.bits.page_ones_after(byte_index, byte_value, read)
    }
}

fn ensure_direct(
    file: &impl MatrixIo,
    base: u64,
    bits: &mut SparseBitmap,
    limits: ReadLimits,
) -> Result<()> {
    if bits.direct.is_none() {
        bits.direct = Some(DirectDirectory::new(
            file.source()?,
            base,
            bits.page_count,
            bits.index_len()?,
            limits.matrix_directory_budget(),
        )?);
        bits.directory.clear();
    }
    Ok(())
}

/// A bounded mutation of the original metadata. All reads, allocation and
/// offset arithmetic finish before the first authoritative write.
struct PreparedDirect {
    entries: [(u64, [u8; 8]); 3],
    used: usize,
    base: u64,
    header: Option<[u8; 8]>,
    count: u64,
}
impl PreparedDirect {
    fn append(base: u64, bits: &mut SparseBitmap, page: u64, publish: bool) -> Result<Self> {
        let index = bits.direct.as_mut().ok_or(Error::InvalidMatrixLayout)?;
        let count = index
            .len()
            .checked_add(1)
            .ok_or(Error::InvalidMatrixLayout)?;
        if count > bits.page_count {
            return Err(Error::InvalidMatrixLayout);
        }
        injected_mirror_reservation_failure(8)?;
        let plan = Self {
            entries: [
                (
                    page_index_entry_offset(base, count - 1)?,
                    entry_bytes(page)?,
                ),
                (index.reverse_offset(page)?, count.to_le_bytes()),
                (0, [0; 8]),
            ],
            used: 2,
            base,
            header: publish
                .then(|| page_index_header_value(count))
                .transpose()?
                .map(u64::to_le_bytes),
            count,
        };
        index.prepare_cache_update(&plan.entries[..plan.used], plan.header);
        Ok(plan)
    }
    fn remove(
        base: u64,
        bits: &mut SparseBitmap,
        page: u64,
        read: BitmapRead<'_>,
    ) -> Result<Option<Self>> {
        let index = bits.direct.as_mut().ok_or(Error::InvalidMatrixLayout)?;
        let Some(slot) = index.first(page, read)? else {
            return Ok(None);
        };
        let last = index
            .len()
            .checked_sub(1)
            .ok_or(Error::InvalidMatrixLayout)?;
        let moved = index.slot(last, read)?.ok_or(Error::InvalidMatrixLayout)?;
        let mut plan = Self {
            entries: [(0, [0; 8]); 3],
            used: 1,
            base,
            header: Some(page_index_header_value(last)?.to_le_bytes()),
            count: last,
        };
        plan.entries[0] = (index.reverse_offset(page)?, [0; 8]);
        if slot != last {
            plan.entries[1] = (page_index_entry_offset(base, slot)?, entry_bytes(moved)?);
            plan.entries[2] = (index.reverse_offset(moved)?, (slot + 1).to_le_bytes());
            plan.used = 3;
        }
        index.prepare_cache_update(&plan.entries[..plan.used], plan.header);
        Ok(Some(plan))
    }
    fn commit(self, file: &mut impl MatrixIo, bits: &mut SparseBitmap) -> Result<()> {
        for &(at, bytes) in &self.entries[..self.used] {
            write_entry_run(file, at, &bytes)?;
        }
        if let Some(header) = self.header {
            write_header_bytes(file, self.base, header)?;
        }
        bits.direct.as_mut().unwrap().install_changes(
            self.count,
            &self.entries[..self.used],
            self.header,
        );
        Ok(())
    }
}

/// Records `page` in the persisted page index before its bytes are written.
///
/// PERF-01: this is what lets open enumerate the written pages without a
/// `0..page_count` loop, including where the platform reports no allocation
/// map at all. The order matters: an index entry whose page never reached
/// the disk names a page that still reads as uninitialised zeros, which
/// open already accepts, whereas a written page with no entry would be
/// invisible to an open that has no allocation map to fall back on.
///
/// A first touch writes the forward entry, reverse entry and count. Later
/// bits in an already indexed page require no directory writes.
///
/// F-03: the whole mutation, including the compaction an already-full array
/// needs, is prepared before the first byte reaches the disk and installed
/// infallibly afterwards. There is no local ordering to get right here.
pub(super) fn record_entry(
    file: &mut impl MatrixIo,
    base: u64,
    mirror: PageIndexMirror<'_>,
    page: u64,
    budget: &mut ResidentBitmapBudget,
) -> Result<()> {
    let bits = mirror.bits;
    ensure_direct(file, base, bits, budget.limits)?;
    if bits.indexes_page(page, BitmapRead::new(file))? {
        return Ok(());
    }
    let _ = budget;
    PreparedDirect::append(base, bits, page, true)?.commit(file, bits)
}

/// Removes `page` from the persisted page index once its final set bit
/// clears.
///
/// F-03: this is what makes both the array and its in-memory mirror track
/// *live* pages instead of every page ever published. It runs strictly
/// after the bitmap byte that emptied the page is durable, because the safe
/// direction is a superset: an entry naming an all-zero page costs one
/// extra page read at open and nothing else, whereas dropping an entry for
/// a page that still holds committed bits would hide them.
///
/// `O(1)`: swap the final forward entry into the vacated slot, update both
/// reverse links and decrement the count. No live-list scan is needed.
pub(super) fn release_entry(
    file: &mut impl MatrixIo,
    base: u64,
    mirror: PageIndexMirror<'_>,
    page: u64,
    budget: &mut ResidentBitmapBudget,
) -> Result<()> {
    let bits = mirror.bits;
    ensure_direct(file, base, bits, budget.limits)?;
    if let Some(plan) = PreparedDirect::remove(base, bits, page, BitmapRead::new(file))? {
        // A failed multi-word update must poison the writer; never publish a
        // generation whose forward and reverse metadata disagree.
        plan.commit(file, bits)?;
    }
    Ok(())
}
pub(super) fn republish_direct(
    file: &mut impl MatrixIo,
    base: u64,
    bits: &mut SparseBitmap,
    page: u64,
    budget: &mut ResidentBitmapBudget,
) -> Result<()> {
    ensure_direct(file, base, bits, budget.limits)?;
    PreparedDirect::append(base, bits, page, false)?.commit(file, bits)
}

/// Writes an already-encoded run of consecutive entries.
///
/// Private to this module: this is the disk mutation that SHAPE A puts
/// behind a prepared value. It takes bytes rather than pages precisely so
/// that no arithmetic — and therefore no non-`Io` failure — remains at this
/// point. A compaction rewrite is one seek and one write rather than one per
/// entry.
fn write_entry_run(file: &mut impl MatrixIo, offset: u64, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    #[cfg(test)]
    if FAIL_NEXT_PAGE_INDEX_WRITE.replace(false) {
        return Err(Error::Io(std::io::Error::other(
            "injected matrix page index write failure",
        )));
    }
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    if take_injected_failure(&scaling_counters::PAGE_INDEX_ENTRY_WRITE_FAILURE) {
        return Err(Error::Io(std::io::Error::other(
            "injected matrix page index entry write failure",
        )));
    }

    file.seek(SeekFrom::Start(offset))?;
    file.write_all(bytes)?;
    Ok(())
}

/// Writes one raw occupancy-header slot.
///
/// Private for the same reason. The value is always encoded and validated
/// during preparation, which is what keeps [`PAGE_INDEX_REBUILD_MARKER`] —
/// deliberately not a representable count (F-02) — expressible without
/// exposing an unvalidated header write to the rest of the file.
fn write_header_bytes(file: &mut impl MatrixIo, base: u64, bytes: [u8; 8]) -> Result<()> {
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    if take_injected_failure(&scaling_counters::PAGE_INDEX_HEADER_WRITE_FAILURE) {
        return Err(Error::Io(std::io::Error::other(
            "injected matrix page index header write failure",
        )));
    }

    file.seek(SeekFrom::Start(base))?;
    file.write_all(&bytes)?;
    Ok(())
}

/// Fails a page-index *mirror* reservation, which is the allocation F-03
/// requires to happen before the first disk byte rather than after the last
/// one.
#[cfg(any(test, feature = "scalable-fault-injection"))]
fn injected_mirror_reservation_failure(requested: u64) -> Result<()> {
    if take_injected_failure(&scaling_counters::PAGE_INDEX_RESERVATION_FAILURE) {
        return Err(Error::AllocationFailed {
            resource: ReadLimitKey::MatrixBitmapBytes.resource(),
            requested,
        });
    }
    Ok(())
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn injected_mirror_reservation_failure(_requested: u64) -> Result<()> {
    Ok(())
}

/// Encodes one entry. `page + 1`, so a never-written slot (zero) is not a
/// valid page reference.
fn entry_bytes(page: u64) -> Result<[u8; 8]> {
    Ok(page
        .checked_add(1)
        .ok_or(Error::InvalidMatrixLayout)?
        .to_le_bytes())
}

/// The occupancy count that publishes a republished generation (F-02).
#[must_use = "a prepared page-index publication must be committed"]
pub(super) struct PreparedPublication {
    header: [u8; 8],
}

/// Encodes the count a republication will publish.
///
/// The mirror it publishes is already installed — each entry installed its
/// own — so committing this is a pure disk write with nothing to follow it.
pub(super) fn prepare_publication(bits: &SparseBitmap) -> Result<PreparedPublication> {
    Ok(PreparedPublication {
        header: page_index_header_value(bits.index_len()?)?.to_le_bytes(),
    })
}

impl PreparedPublication {
    pub(super) fn commit(self, file: &mut impl MatrixIo, base: u64) -> Result<()> {
        write_header_bytes(file, base, self.header)
    }
}

/// Publishes [`PAGE_INDEX_REBUILD_MARKER`] into the occupancy slot (F-02).
///
/// This is not a mirror mutation and takes no prepared value: it destroys
/// nothing on its own and is the step that makes everything after it
/// fail-closed, so there is no in-memory state that could disagree with it.
pub(super) fn write_rebuild_marker(file: &mut impl MatrixIo, base: u64) -> Result<()> {
    write_header_bytes(file, base, PAGE_INDEX_REBUILD_MARKER.to_le_bytes())
}
