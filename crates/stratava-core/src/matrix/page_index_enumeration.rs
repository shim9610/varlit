use super::*;

/// The outcome of enumerating one persisted page index.
///
/// Not `Copy` and not `Clone`: it is moved into the evidence it justifies.
#[derive(Debug)]
#[must_use = "the enumeration outcome is what makes a validity bitmap usable \
              as evidence; dropping it discards F-04's only input"]
pub struct PageIndexEnumeration {
    complete: bool,
}

impl PageIndexEnumeration {
    /// Every page the index named was located. Private on purpose — see the
    /// module documentation; this is the constructor whose `pub(super)`
    /// equivalent was the round-14 hole.
    fn fully_enumerated() -> Self {
        Self { complete: true }
    }

    /// The index could not be enumerated in full. Every path that reaches
    /// this has already pushed a `Fatal` finding.
    fn truncated_by_damage() -> Self {
        Self { complete: false }
    }

    /// Reading the outcome is unrestricted; *stating* it is not.
    pub(super) fn is_complete(&self) -> bool {
        self.complete
    }
}

/// Loads the persisted page index into `bits`.
///
/// F-06, the whole point of the v4 header: enumeration length comes from a
/// *validated occupancy count*, never from scanning for a terminator. The old
/// representation treated a zero or out-of-range entry as the successful end of
/// the array, so a single damaged entry silently truncated enumeration — every
/// later committed page went unvisited, its cells answered `NotCommitted`, and
/// no finding was produced at all. Damage now has exactly one outcome: the
/// entry is reported as a fatal finding and enumeration continues, so a damaged
/// index can cost visibility of one page but can never hide the rest.
///
/// The header slot carries its own redundancy, so a torn or flipped header is
/// detected too. There the true count is unknowable, so the index contributes
/// nothing and a fatal finding is raised; enumeration falls back to whatever
/// the allocation map can prove. Silence is never an outcome.
///
/// Cost is `O(count)` entries read — the live pages — and never `O(page_count)`.
///
/// F-04: the return value is whether the index was **complete** — whether every
/// page it named could be enumerated. `false` means the caller holds an
/// in-memory bitmap that is a *subset* of the published state, with the missing
/// pages reading as clear. That is safe for reading, because default open is
/// already fail-closed on the fatal finding this records, but it is not safe as
/// *evidence*: a recovery step that treats a clear bit as proof of absence would
/// erase state it never looked at. Callers that consume a bitmap as evidence
/// must propagate this.
pub(super) fn load_page_index(
    file: &mut impl MatrixIo,
    source: &PagedBitmapSource<'_>,
    bits: &mut SparseBitmap,
    sink: &mut PagedBitmapSink<'_>,
    _resource: &'static str,
    label: &str,
) -> Result<PageIndexEnumeration> {
    let capacity = bits.page_count.min(PAGE_INDEX_MAX_ENTRIES);
    if capacity == 0 {
        return Ok(PageIndexEnumeration::fully_enumerated());
    }
    // A region the filesystem proves to be a hole holds a zero header, which is
    // the encoding for "no entries". Reading it would answer the same thing.
    if !range_may_hold_data(source.extents, source.index_base, PAGE_INDEX_ENTRY_LEN) {
        bits.direct = Some(DirectDirectory::new(
            file.source()?,
            source.index_base,
            bits.page_count,
            0,
            sink.budget.limits.matrix_directory_budget(),
        )?);
        return Ok(PageIndexEnumeration::fully_enumerated());
    }
    let header = read_page_index_header(file, source.index_base)?;
    count_open_bitmap_bytes_read(PAGE_INDEX_ENTRY_LEN);
    // F-02: a whole-map republication marks the header before it empties the
    // entry region and only replaces the marker once the new generation is
    // durable, so seeing it means the index on disk names an unknown subset of
    // the published pages. Reading that subset as authoritative is precisely the
    // silent state loss the marker exists to prevent, so this is fatal and the
    // index contributes nothing.
    if header == PAGE_INDEX_REBUILD_MARKER {
        sink.findings.push(MatrixRecoveryFinding {
            kind: MatrixCorruptionKind::CommitMap,
            severity: MatrixCorruptionSeverity::Fatal,
            message: format!(
                "matrix page index for {label} is marked as an interrupted rebuild; the pages it \
             names are not the published set and the commit map must be rebuilt again"
            ),
        });
        return Ok(PageIndexEnumeration::truncated_by_damage());
    }
    let Some(count) = page_index_header_count(header, capacity) else {
        sink.findings.push(MatrixRecoveryFinding {
            kind: MatrixCorruptionKind::CommitMap,
            severity: MatrixCorruptionSeverity::Fatal,
            message: format!(
                "matrix page index header for {label} is damaged ({header:#018x}); the set of \
             published pages cannot be enumerated from it"
            ),
        });
        return Ok(PageIndexEnumeration::truncated_by_damage());
    };

    bits.direct = Some(DirectDirectory::new(
        file.source()?,
        source.index_base,
        bits.page_count,
        count,
        sink.budget.limits.matrix_directory_budget(),
    )?);
    if !sink.budget.limits.admit_matrix_metadata_verification() {
        return Ok(PageIndexEnumeration::fully_enumerated());
    }
    let verified = bits
        .direct
        .as_ref()
        .unwrap()
        .visit(BitmapRead::new(file), |_, _| Ok(()));
    match verified {
        Ok(()) => Ok(PageIndexEnumeration::fully_enumerated()),
        Err(Error::MatrixFatalCorruption) => {
            sink.findings.push(MatrixRecoveryFinding {
                kind: MatrixCorruptionKind::CommitMap,
                severity: MatrixCorruptionSeverity::Fatal,
                message: format!(
                    "matrix page index for {label} has inconsistent forward/reverse entries"
                ),
            });
            Ok(PageIndexEnumeration::truncated_by_damage())
        }
        Err(err) => Err(err),
    }
}
