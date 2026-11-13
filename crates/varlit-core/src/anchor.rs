//! Fixed-offset anchors in a native append log.
//!
//! An anchored file reserves `hole` bytes at every physical offset
//! `start + k * block` for `k >= 1`, where `start` is where the append log
//! begins. The bytes of the log itself — records, in the contiguous *logical*
//! offsets every record, chain and index names — flow around those reserved
//! ranges: a record that reaches a reserved range continues after it. Nothing
//! above the file I/O layer sees a reserved range; [`AnchorLayout`] translates.
//!
//! Each reserved range holds one [`Anchor`], written when the log first reaches
//! it. It describes only what was already true when it was written — where the
//! record spanning the boundary starts and ends, and its sequence — so it is
//! written once and never rewritten, like the rest of the log. Because its
//! position is arithmetic, a reader reaches anchor `k` with one read and no
//! index: from there it frames forward at the first whole record, and a binary
//! search over anchors finds a sequence number in `log2(file / block)` reads.
//!
//! A file shorter than `start + block` has no reserved range at all and is
//! byte-for-byte what an unanchored writer produces after its header.

use crate::{Error, Result};

/// Smallest anchor that holds an encoded [`Anchor`].
pub(crate) const MIN_ANCHOR_HOLE: u64 = 64;

pub(crate) const ANCHOR_MAGIC: &[u8; 4] = b"VANC";
const ANCHOR_VERSION: u16 = 1;

/// Whether new files of a format carry anchors, and how far apart.
///
/// Stored in the native file header's flags byte, which releases before
/// anchors required to be zero, so they refuse an anchored file instead of
/// reading its anchors as records. An existing file is always read and appended
/// to as its own header says, whatever the policy of the spec that opens it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnchorPolicy {
    flags: u8,
}

const ANCHORED: u8 = 0x80;
const SMALL_HOLE: u8 = 0x40;
const BLOCK_SHIFT_MASK: u8 = 0x3f;
const BLOCK_SHIFT_BASE: u32 = 10;

impl AnchorPolicy {
    /// No anchors: the file is the plain append log.
    pub const NONE: Self = Self { flags: 0 };
    /// A 4 KiB anchor — a page, so data after it keeps the page alignment it
    /// would have had — every 4 MiB.
    ///
    /// The spacing is what an open scans at most: it resumes at the newest
    /// anchor carrying a summary, so a file shorter than one spacing is read
    /// whole and a longer one from its last spacing. 4 MiB bounds that to
    /// 256 reads of the 16 KiB scan window for 0.1% of the file in anchors.
    /// (64 MiB, the earlier default, scanned up to 16 times as much for
    /// 0.006%.) Each file records its own spacing, so files written under
    /// the earlier default keep it.
    pub const DEFAULT: Self = Self::every(22, false);

    /// An anchor every `2^block_log2` bytes, 4 KiB long, or 256 bytes when
    /// `small_hole`. `block_log2` must be in `10..=40` and leave room for at
    /// least as much log as anchor between anchors; see [`Self::validate`].
    pub const fn every(block_log2: u8, small_hole: bool) -> Self {
        let shift = block_log2.saturating_sub(BLOCK_SHIFT_BASE as u8) & BLOCK_SHIFT_MASK;
        Self {
            flags: ANCHORED | if small_hole { SMALL_HOLE } else { 0 } | shift,
        }
    }

    pub const fn is_anchored(self) -> bool {
        self.flags & ANCHORED != 0
    }

    /// Spacing and size, when anchored.
    pub const fn spacing(self) -> Option<(u64, u64)> {
        if !self.is_anchored() {
            return None;
        }
        // The shift comes from a file header's flags byte and reaches 73;
        // `validate` refuses anything past 2^40, so saturating only keeps an
        // unvalidated policy from panicking.
        let block =
            match 1u64.checked_shl(BLOCK_SHIFT_BASE + (self.flags & BLOCK_SHIFT_MASK) as u32) {
                Some(block) => block,
                None => u64::MAX,
            };
        let hole = if self.flags & SMALL_HOLE != 0 {
            256
        } else {
            4096
        };
        Some((block, hole))
    }

    pub fn validate(self) -> Result<Self> {
        if self.is_anchored() && (self.flags & BLOCK_SHIFT_MASK) as u32 + BLOCK_SHIFT_BASE > 40 {
            return Err(Error::InvalidFormatSpec(
                "anchor spacing exceeds 2^40 bytes",
            ));
        }
        if let Some((block, hole)) = self.spacing() {
            AnchorLayout::new(0, block, hole)?;
        }
        Ok(self)
    }

    pub(crate) const fn flags(self) -> u8 {
        self.flags
    }

    /// The policy a file header's flags byte records.
    pub(crate) fn from_flags(flags: u8) -> Result<Self> {
        if flags != 0 && flags & ANCHORED == 0 {
            return Err(Error::InvalidCanonicalEncoding(
                "native file-header reserved flags must be zero",
            ));
        }
        Self { flags }.validate()
    }

    /// The layout of a file whose append log starts at `start`.
    pub(crate) fn layout(self, start: u64) -> Option<AnchorLayout> {
        let (block, hole) = self.spacing()?;
        AnchorLayout::new(start, block, hole).ok()
    }
}

/// Where a file's anchors are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnchorLayout {
    start: u64,
    block: u64,
    hole: u64,
}

impl AnchorLayout {
    pub fn new(start: u64, block: u64, hole: u64) -> Result<Self> {
        if hole < MIN_ANCHOR_HOLE || block <= hole || block - hole < hole {
            return Err(Error::InvalidFormatSpec(
                "anchor spacing must exceed twice the anchor size, which must be at least 64 bytes",
            ));
        }
        Ok(Self { start, block, hole })
    }

    pub const fn start(&self) -> u64 {
        self.start
    }
    pub const fn block(&self) -> u64 {
        self.block
    }
    pub const fn hole(&self) -> u64 {
        self.hole
    }
    const fn data(&self) -> u64 {
        self.block - self.hole
    }

    /// Physical offset of anchor `k` (`k >= 1`).
    pub fn anchor_physical(&self, k: u64) -> Option<u64> {
        if k == 0 {
            return None;
        }
        k.checked_mul(self.block)?.checked_add(self.start)
    }

    /// The logical offset an anchor sits at: the first log byte after it.
    pub fn anchor_logical(&self, k: u64) -> Option<u64> {
        if k == 0 {
            return None;
        }
        // Segment 0 holds `block` bytes, every later one `block - hole`.
        let before = (k - 1).checked_mul(self.data())?.checked_add(self.block)?;
        self.start.checked_add(before)
    }

    /// The anchor index whose logical position is the first at or after
    /// `logical`, as `(k, logical position)`.
    fn next_anchor(&self, logical: u64) -> Option<(u64, u64)> {
        let d = logical.saturating_sub(self.start);
        let k = if d <= self.block {
            1
        } else {
            (d - self.block).div_ceil(self.data()) + 1
        };
        Some((k, self.anchor_logical(k)?))
    }

    /// How many anchors lie before logical offset `end`: anchors
    /// `1..=anchors_before(end)` sit at logical offsets below it.
    pub fn anchors_before(&self, end: u64) -> u64 {
        let first = self.start.saturating_add(self.block);
        if end <= first {
            return 0;
        }
        (end - 1 - first) / self.data() + 1
    }

    /// Physical offset of the log byte at `logical`.
    pub fn physical(&self, logical: u64) -> Option<u64> {
        if logical < self.start {
            return Some(logical);
        }
        let d = logical - self.start;
        if d < self.block {
            return Some(logical);
        }
        let d = d - self.block;
        let k = d / self.data() + 1;
        let within = d % self.data();
        k.checked_mul(self.block)?
            .checked_add(self.start)?
            .checked_add(self.hole)?
            .checked_add(within)
    }

    /// Physical end of a log whose logical end is `logical_end`. An anchor
    /// the log has reached but not passed is not part of it yet.
    pub fn physical_end(&self, logical_end: u64) -> Option<u64> {
        if logical_end <= self.start.checked_add(self.block)? {
            return Some(logical_end);
        }
        let d = logical_end - self.start - self.block;
        let k = d / self.data() + 1;
        let within = d % self.data();
        let base = k.checked_mul(self.block)?.checked_add(self.start)?;
        if within == 0 {
            Some(base)
        } else {
            base.checked_add(self.hole)?.checked_add(within)
        }
    }

    /// Logical length of a file of `physical_len` bytes. A partly written
    /// anchor ends the log where the anchor starts.
    pub fn logical_len(&self, physical_len: u64) -> u64 {
        if physical_len <= self.start {
            return physical_len;
        }
        let p = physical_len - self.start;
        if p <= self.block {
            return physical_len;
        }
        let k = p / self.block;
        let r = p % self.block;
        let at = self
            .anchor_logical(k)
            .expect("an anchor before a physical length has a logical offset");
        at + r.saturating_sub(self.hole)
    }

    /// Splits the logical range `[offset, offset + len)` into physical runs,
    /// each `(physical offset, logical offset, len)`, and reports every anchor
    /// the range reaches as `(k, logical offset)` in order: an anchor at a
    /// logical offset inside `[offset, offset + len)` precedes the run that
    /// starts there.
    pub(crate) fn split(
        &self,
        offset: u64,
        len: u64,
        mut run: impl FnMut(Segment) -> Result<()>,
    ) -> Result<()> {
        let end = offset
            .checked_add(len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "anchored range",
            })?;
        let mut at = offset;
        while at < end {
            let overflow = || Error::ResourceArithmeticOverflow {
                resource: "anchored offset",
            };
            let (k, anchor) = self.next_anchor(at).ok_or_else(overflow)?;
            if anchor == at && at >= self.start.saturating_add(self.block) {
                run(Segment::Anchor {
                    k,
                    physical: self.anchor_physical(k).ok_or_else(overflow)?,
                    logical: at,
                })?;
            }
            // The run continues to the next anchor's logical position.
            let next = if anchor > at {
                anchor
            } else {
                self.anchor_logical(k + 1).ok_or_else(overflow)?
            };
            let stop = next.min(end);
            run(Segment::Data {
                physical: self.physical(at).ok_or_else(overflow)?,
                logical: at,
                len: stop - at,
            })?;
            at = stop;
        }
        Ok(())
    }
}

/// One piece of an anchored logical range; see [`AnchorLayout::split`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Segment {
    Data {
        physical: u64,
        logical: u64,
        len: u64,
    },
    Anchor {
        k: u64,
        physical: u64,
        logical: u64,
    },
}

/// What an anchor records: the record spanning or starting at its logical
/// offset, as known when the log reached it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// Which anchor this is (`k >= 1`).
    pub k: u64,
    /// Its logical offset; also implied by `k`.
    pub logical: u64,
    /// Start of the record that contains `logical` or starts at it.
    pub record_start: u64,
    /// End of that record: where the first record wholly after the anchor
    /// begins.
    pub record_end: u64,
    /// That record's sequence number.
    pub sequence: u64,
}

impl Anchor {
    /// Encodes into an anchor's reserved range. The rest of the range is zero.
    pub(crate) fn encode(&self, out: &mut [u8]) {
        out.fill(0);
        out[0..4].copy_from_slice(ANCHOR_MAGIC);
        out[4..6].copy_from_slice(&ANCHOR_VERSION.to_le_bytes());
        out[8..16].copy_from_slice(&self.k.to_le_bytes());
        out[16..24].copy_from_slice(&self.logical.to_le_bytes());
        out[24..32].copy_from_slice(&self.record_start.to_le_bytes());
        out[32..40].copy_from_slice(&self.record_end.to_le_bytes());
        out[40..48].copy_from_slice(&self.sequence.to_le_bytes());
        let crc = crc32fast::hash(&out[..out.len() - 4]);
        let at = out.len() - 4;
        out[at..].copy_from_slice(&crc.to_le_bytes());
    }

    /// Decodes anchor `k` of `layout` from its reserved range, or `None` when
    /// the range does not hold that anchor intact.
    pub(crate) fn decode(layout: &AnchorLayout, k: u64, bytes: &[u8]) -> Option<Self> {
        let n = bytes.len();
        if n as u64 != layout.hole()
            || &bytes[0..4] != ANCHOR_MAGIC
            || u16::from_le_bytes([bytes[4], bytes[5]]) != ANCHOR_VERSION
            || bytes[6..8] != [0; 2]
            || crc32fast::hash(&bytes[..n - 4])
                != u32::from_le_bytes(bytes[n - 4..].try_into().expect("four bytes"))
        {
            return None;
        }
        let read = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight"));
        let anchor = Self {
            k: read(8),
            logical: read(16),
            record_start: read(24),
            record_end: read(32),
            sequence: read(40),
        };
        (anchor.k == k
            && Some(anchor.logical) == layout.anchor_logical(k)
            && anchor.record_start <= anchor.logical
            && anchor.record_end > anchor.logical)
            .then_some(anchor)
    }
}

/// Where an anchor's open summary begins in its reserved range: right after
/// the fixed fields. Zero bytes there (what every anchor written without a
/// summary holds) read as "no summary".
const SUMMARY_AT: usize = 48;
/// flags(2) + reserved(2) + tail count(4) + records before(8) + high water(8)
/// + segment next position(8) + segment covered start(8).
const SUMMARY_PREFIX: usize = 40;
const SUMMARY_TAIL: usize = 12;
/// The summary is present and describes every record before `record_start`.
const SUMMARY_COMPLETE: u16 = 0x0001;
/// A sequence high-water mark is present.
const SUMMARY_SEQUENCE: u16 = 0x0002;
/// A segment record precedes the anchor; its cursor fields are present.
const SUMMARY_SEGMENT: u16 = 0x0004;

/// What an open needs to start at an anchor instead of at the log start: the
/// state of the file just before the record spanning the anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AnchorSummary {
    /// Resident records that start before `record_start`.
    pub(crate) records_before: u64,
    /// The highest sequence of any record before `record_start`.
    pub(crate) sequence_high_water: Option<u64>,
    /// Each block's newest record before `record_start`, ascending by id.
    pub(crate) tails: Vec<(u32, u64)>,
    /// For a segment-chained format: the resident position just past the
    /// newest segment record before `record_start`, and where that record
    /// ends — what the next segment covers from.
    pub(crate) segment: Option<(u64, u64)>,
}

/// Writes an anchor's open summary, on demand: only a write that reaches an
/// anchor pays for it.
pub(crate) trait SummarySource {
    /// Writes the summary into `out` (the whole reserved range, fixed fields
    /// already in place). Returns `false` when it does not fit or is not
    /// known, leaving the range without one.
    fn write_summary(&self, out: &mut [u8]) -> bool;
}

/// A summary encoded ahead of the write that stores it: a reserved range of
/// the anchor's size whose summary bytes are copied into the real anchor.
pub(crate) struct EncodedSummary<'a>(pub(crate) &'a [u8]);

impl SummarySource for EncodedSummary<'_> {
    fn write_summary(&self, out: &mut [u8]) -> bool {
        let end = out.len().saturating_sub(4);
        if self.0.len() != out.len() || end <= SUMMARY_AT {
            return false;
        }
        out[SUMMARY_AT..end].copy_from_slice(&self.0[SUMMARY_AT..end]);
        true
    }
}

/// Encodes a summary into a reserved range already holding the fixed fields.
/// `false` (range untouched) when the tails do not fit.
pub(crate) fn encode_summary(
    out: &mut [u8],
    records_before: u64,
    sequence_high_water: Option<u64>,
    segment: Option<(u64, u64)>,
    tails: impl ExactSizeIterator<Item = (u32, u64)>,
) -> bool {
    let room = out.len().saturating_sub(SUMMARY_AT + SUMMARY_PREFIX + 4);
    let count = tails.len();
    if count.saturating_mul(SUMMARY_TAIL) > room || u32::try_from(count).is_err() {
        return false;
    }
    let mut flags = SUMMARY_COMPLETE;
    if sequence_high_water.is_some() {
        flags |= SUMMARY_SEQUENCE;
    }
    if segment.is_some() {
        flags |= SUMMARY_SEGMENT;
    }
    let at = SUMMARY_AT;
    out[at..at + 2].copy_from_slice(&flags.to_le_bytes());
    out[at + 4..at + 8].copy_from_slice(&(count as u32).to_le_bytes());
    out[at + 8..at + 16].copy_from_slice(&records_before.to_le_bytes());
    out[at + 16..at + 24].copy_from_slice(&sequence_high_water.unwrap_or(0).to_le_bytes());
    let (next_position, covered_start) = segment.unwrap_or((0, 0));
    out[at + 24..at + 32].copy_from_slice(&next_position.to_le_bytes());
    out[at + 32..at + 40].copy_from_slice(&covered_start.to_le_bytes());
    let mut cursor = at + SUMMARY_PREFIX;
    for (id, offset) in tails {
        out[cursor..cursor + 4].copy_from_slice(&id.to_le_bytes());
        out[cursor + 4..cursor + 12].copy_from_slice(&offset.to_le_bytes());
        cursor += SUMMARY_TAIL;
    }
    true
}

/// Reseals a reserved range after its bytes changed.
pub(crate) fn reseal(out: &mut [u8]) {
    let at = out.len() - 4;
    let crc = crc32fast::hash(&out[..at]);
    out[at..].copy_from_slice(&crc.to_le_bytes());
}

/// Clears a summary in place (the anchor itself stays valid) and reseals.
pub(crate) fn clear_summary(out: &mut [u8]) {
    let end = out.len() - 4;
    out[SUMMARY_AT..end].fill(0);
    reseal(out);
}

impl AnchorSummary {
    /// The summary an intact anchor's range carries, or `None` when it holds
    /// none or one this build cannot trust. Every offset must name a record
    /// before `record_start` and inside the log, ids strictly ascending.
    pub(crate) fn decode(bytes: &[u8], append_start: u64, record_start: u64) -> Option<Self> {
        let at = SUMMARY_AT;
        if bytes.len() < at + SUMMARY_PREFIX + 4 {
            return None;
        }
        let flags = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        if flags & SUMMARY_COMPLETE == 0
            || flags & !(SUMMARY_COMPLETE | SUMMARY_SEQUENCE | SUMMARY_SEGMENT) != 0
            || bytes[at + 2..at + 4] != [0; 2]
        {
            return None;
        }
        let read = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight"));
        let count = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().expect("four")) as usize;
        let room = bytes.len() - 4 - at - SUMMARY_PREFIX;
        if count.checked_mul(SUMMARY_TAIL)? > room {
            return None;
        }
        let records_before = read(at + 8);
        // A record is at least a header long, so no more of them fit before
        // `record_start` than that allows.
        if records_before > record_start.saturating_sub(append_start) / 16 {
            return None;
        }
        let sequence_high_water = (flags & SUMMARY_SEQUENCE != 0).then(|| read(at + 16));
        let segment = (flags & SUMMARY_SEGMENT != 0).then(|| (read(at + 24), read(at + 32)));
        // The cursor names a position among the records before the anchor and
        // an end inside the log before `record_start`.
        if segment.is_some_and(|(position, covered)| {
            position == 0
                || position > records_before
                || covered <= append_start
                || covered > record_start
        }) {
            return None;
        }
        let mut tails = Vec::new();
        tails.try_reserve_exact(count).ok()?;
        let mut previous: Option<u32> = None;
        for index in 0..count {
            let base = at + SUMMARY_PREFIX + index * SUMMARY_TAIL;
            let id = u32::from_le_bytes(bytes[base..base + 4].try_into().expect("four"));
            let offset = read(base + 4);
            if previous.is_some_and(|last| id <= last)
                || offset < append_start
                || offset >= record_start
            {
                return None;
            }
            previous = Some(id);
            tails.push((id, offset));
        }
        Some(Self {
            records_before,
            sequence_high_water,
            tails,
            segment,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_round_trip_through_the_header_flags_byte() {
        assert_eq!(AnchorPolicy::from_flags(0).unwrap(), AnchorPolicy::NONE);
        assert_eq!(AnchorPolicy::DEFAULT.spacing(), Some((4 << 20, 4096)));
        for shift in 10..=40u8 {
            for small in [false, true] {
                let policy = AnchorPolicy::every(shift, small);
                match policy.validate() {
                    Ok(_) => assert_eq!(AnchorPolicy::from_flags(policy.flags()).unwrap(), policy),
                    Err(_) => assert!(AnchorPolicy::from_flags(policy.flags()).is_err()),
                }
            }
        }
        // A 1 KiB spacing has no room for a 4 KiB anchor.
        assert!(AnchorPolicy::every(10, false).validate().is_err());
        assert!(AnchorPolicy::every(10, true).validate().is_ok());
        // Unanchored flags other than zero are still refused.
        assert!(AnchorPolicy::from_flags(1).is_err());
    }

    #[test]
    fn every_header_flags_byte_is_refused_or_accepted_without_panicking() {
        // A damaged header can name a spacing exponent up to 73, past what a
        // u64 shift holds (found by fuzzing).
        for flags in 0..=u8::MAX {
            if let Ok(Some((block, _))) = AnchorPolicy::from_flags(flags).map(AnchorPolicy::spacing)
            {
                assert!(block <= 1 << 40);
            }
        }
        assert!(AnchorPolicy::from_flags(ANCHORED | BLOCK_SHIFT_MASK).is_err());
        assert_eq!(
            AnchorPolicy::every(73, false).spacing(),
            Some((u64::MAX, 4096))
        );
    }

    fn layout() -> AnchorLayout {
        AnchorLayout::new(100, 1000, 64).unwrap()
    }

    #[test]
    fn logical_and_physical_offsets_round_trip_around_every_anchor() {
        let layout = layout();
        let mut previous = None;
        for logical in 0..20_000u64 {
            let physical = layout.physical(logical).unwrap();
            if let Some(previous) = previous {
                assert!(physical > previous, "{logical}");
            }
            // No log byte lands in an anchor.
            if physical >= 1100 {
                assert!((physical - 100) % 1000 >= 64, "{logical} -> {physical}");
            }
            assert_eq!(layout.logical_len(physical + 1), logical + 1, "{logical}");
            assert_eq!(layout.physical_end(logical + 1), Some(physical + 1));
            previous = Some(physical);
        }
        // An anchor's logical position is where the log resumes after it.
        for k in 1..10 {
            let at = layout.anchor_logical(k).unwrap();
            assert_eq!(
                layout.physical(at).unwrap(),
                layout.anchor_physical(k).unwrap() + 64
            );
            // A log that ends just before the anchor does not include it.
            assert_eq!(layout.physical_end(at), layout.anchor_physical(k));
            // A torn anchor ends the log where the anchor starts.
            for cut in 0..64 {
                assert_eq!(
                    layout.logical_len(layout.anchor_physical(k).unwrap() + cut),
                    at
                );
            }
        }
    }

    #[test]
    fn counting_anchors_before_an_offset_matches_their_positions() {
        let layout = layout();
        for end in 0..20_000u64 {
            let expected = (1..100)
                .take_while(|k| layout.anchor_logical(*k).unwrap() < end)
                .count() as u64;
            assert_eq!(layout.anchors_before(end), expected, "{end}");
        }
    }

    #[test]
    fn splitting_reports_each_reached_anchor_once_before_its_bytes() {
        let layout = layout();
        for (offset, len) in [(0, 5000), (1099, 2), (1100, 1), (1000, 3000), (2036, 936)] {
            let mut logical = offset;
            let mut anchors = Vec::new();
            layout
                .split(offset, len, |segment| {
                    match segment {
                        Segment::Data {
                            physical,
                            logical: at,
                            len,
                        } => {
                            assert_eq!(at, logical);
                            assert_eq!(Some(physical), layout.physical(at));
                            assert_eq!(layout.physical(at + len - 1), Some(physical + len - 1));
                            logical += len;
                        }
                        Segment::Anchor { k, logical: at, .. } => {
                            assert_eq!(at, logical);
                            assert_eq!(layout.anchor_logical(k), Some(at));
                            anchors.push(k);
                        }
                    }
                    Ok(())
                })
                .unwrap();
            assert_eq!(logical, offset + len);
            let expected: Vec<u64> = (1..20)
                .filter(|k| {
                    let at = layout.anchor_logical(*k).unwrap();
                    at >= offset && at < offset + len
                })
                .collect();
            assert_eq!(anchors, expected, "{offset}+{len}");
        }
    }

    #[test]
    fn anchors_round_trip_and_reject_damage() {
        let layout = layout();
        let anchor = Anchor {
            k: 2,
            logical: layout.anchor_logical(2).unwrap(),
            record_start: layout.anchor_logical(2).unwrap() - 10,
            record_end: layout.anchor_logical(2).unwrap() + 30,
            sequence: 77,
        };
        let mut bytes = vec![0u8; 64];
        anchor.encode(&mut bytes);
        assert_eq!(Anchor::decode(&layout, 2, &bytes), Some(anchor));
        assert_eq!(Anchor::decode(&layout, 3, &bytes), None);
        for i in 0..64 {
            let mut damaged = bytes.clone();
            damaged[i] ^= 1;
            assert_eq!(Anchor::decode(&layout, 2, &damaged), None, "byte {i}");
        }
    }
}
