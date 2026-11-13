/// One forward pass must answer exactly what the three walks it replaced
/// answered, on every shape that tells them apart.
///
/// `derive_index_state` folds `CheckpointCadence::from_index` (a reverse
/// walk stopping at the newest checkpoint), `SegmentCursor::from_index` (an
/// `rposition` for the newest segment) and `BlockTails::from_index` (a full
/// forward pass) into one traversal. The three rules are kept beside it as
/// `#[cfg(test)]` specifications precisely so this can compare against
/// them rather than against a restatement of the new code.
///
/// The shapes matter more than the count. An index with no checkpoint and
/// no segment is the case where both reverse walks degenerate to full
/// walks; a checkpoint at the very end and one at the very start bracket
/// the `position / 2` threshold; interleaved commit and segment records are
/// what separate "eligible" from "seen", since those two ids are the ones
/// the cadence does not count.
#[test]
fn derived_state_matches_the_three_rules_it_replaced() {
    fn entry(block_id: u32, sequence: u64) -> RecordIndexEntry {
        RecordIndexEntry {
            block_id,
            block_version: 1,
            flags: 0,
            sequence,
            record_offset: sequence * 16,
            payload_offset: sequence * 16 + 8,
            payload_len: 8,
            checksum: 0,
            uncompressed_len_hint: 0,
            footer_offset: None,
            prev_same_block_offset: None,
            prev_same_key_offset: None,
            mutable_flags: 0,
            committed: false,
        }
    }
    fn build(ids: &[u32]) -> Vec<RecordIndexEntry> {
        ids.iter()
            .enumerate()
            .map(|(position, id)| entry(*id, position as u64))
            .collect()
    }

    const USER: u32 = 7;
    let shapes: Vec<(&str, Vec<RecordIndexEntry>)> = vec![
        ("empty", build(&[])),
        (
            "no checkpoint, no segment",
            build(&[USER, USER, USER, USER]),
        ),
        ("checkpoint last", build(&[USER, USER, INDEX_BLOCK_ID])),
        (
            "checkpoint first",
            build(&[INDEX_BLOCK_ID, USER, USER, USER]),
        ),
        (
            "two checkpoints, newest wins",
            build(&[INDEX_BLOCK_ID, USER, INDEX_BLOCK_ID, USER, USER]),
        ),
        ("segment only", build(&[USER, SEGMENT_BLOCK_ID, USER])),
        (
            "segment last, so the cursor sits past the end",
            build(&[USER, USER, SEGMENT_BLOCK_ID]),
        ),
        (
            "commit and segment interleaved, neither is eligible",
            build(&[
                USER,
                COMMIT_BLOCK_ID,
                SEGMENT_BLOCK_ID,
                USER,
                COMMIT_BLOCK_ID,
                INDEX_BLOCK_ID,
                USER,
                SEGMENT_BLOCK_ID,
                USER,
            ]),
        ),
    ];

    for (name, index) in shapes {
        let derived = derive_index_state(infallible_entries(&index), true)
            .expect("borrowed entries cannot fail");
        let cursor = SegmentCursor::from_index(&index);
        assert_eq!(
            derived.segment_cursor.next_position, cursor.next_position,
            "{name}: segment cursor position diverged from the rposition"
        );
        assert_eq!(
            derived.segment_cursor.covered_start, cursor.covered_start,
            "{name}: segment coverage diverged from the rposition"
        );
        // `BlockTails` is `PartialEq`, so this compares the whole
        // structure — the ordered table as well as the tails — rather than
        // spot-checking ids the fused pass happens to get right.
        assert_eq!(
            derived.block_tails.as_ref().expect("tails were requested"),
            &BlockTails::from_index(&index),
            "{name}: block tails diverged from the forward walk"
        );
    }
}

/// What the two sidecar identity windows must stop covering, and what they
/// must keep covering.
///
/// Both windows hash the file's leading bytes to bind a derived artefact to
/// the primary. That was sound while the header was written once; the
/// header tail region makes it false, and without this blanking one commit
/// would permanently invalidate every published sidecar for the file.
///
/// The relaxation has to be exactly as narrow as that. This asserts three
/// things a looser version would get wrong: the magic and the declared
/// length stay in the window (they are what move `append_log_start`, so a
/// change to either has to keep failing); a block that is *not* mutable is
/// untouched even when it sits beside one that is; and a caller whose
/// window ends inside the region blanks what it holds rather than
/// overrunning or refusing.
#[test]
fn blanking_covers_the_mutable_payload_and_nothing_else() {
    const PAYLOAD_LEN: usize = 40;
    const HEADER_PREAMBLE: usize = 20;
    let mut extensions = Vec::new();
    // `VCHD` first: it predates the framing and carries no length prefix,
    // so it also checks that the walk's offsets survive a block of a
    // different shape sitting in front of the mutable one.
    extensions.extend_from_slice(FILE_COMPRESSION_MAGIC);
    extensions.extend_from_slice(&[0x11; FILE_COMPRESSION_PAYLOAD_LEN]);
    let vchd_len = extensions.len();
    extensions.extend_from_slice(HEADER_TAILS_MAGIC);
    extensions.extend_from_slice(&(PAYLOAD_LEN as u32).to_le_bytes());
    extensions.extend_from_slice(&[0x22; PAYLOAD_LEN]);
    // And a block *after* the mutable one, which the walk skips because it
    // does not know the magic. Without it the mutable payload runs to the
    // end of the region and "everything after it is untouched" asserts over
    // an empty slice — true of any implementation, including one that
    // blanked to the end.
    const TRAILING_LEN: usize = 12;
    extensions.extend_from_slice(b"VZZZ");
    extensions.extend_from_slice(&(TRAILING_LEN as u32).to_le_bytes());
    extensions.extend_from_slice(&[0x33; TRAILING_LEN]);

    let header_len = (HEADER_PREAMBLE + extensions.len()) as u64;
    let payload_start = HEADER_PREAMBLE + vchd_len + 8;
    assert!(
        payload_start + PAYLOAD_LEN < header_len as usize,
        "the assertion below has to have bytes to be about",
    );

    let mut prefix = vec![0xAAu8; header_len as usize];
    let blanked = blank_mutable_header_bytes(&mut prefix, header_len, &extensions)
        .expect("a well-framed region blanks");
    assert_eq!(blanked, PAYLOAD_LEN as u64);
    assert!(
        prefix[payload_start..payload_start + PAYLOAD_LEN]
            .iter()
            .all(|byte| *byte == 0),
        "the mutable payload is blanked",
    );
    assert!(
        prefix[..payload_start].iter().all(|byte| *byte == 0xAA),
        "everything before it — including `VBTT`'s own magic and declared \
         length — stays in the window",
    );
    assert!(
        prefix[payload_start + PAYLOAD_LEN..]
            .iter()
            .all(|byte| *byte == 0xAA),
        "and everything after it",
    );

    // A window that stops inside the region blanks its own tail and no more.
    let short = payload_start + 5;
    let mut clipped = vec![0xAAu8; short];
    let blanked = blank_mutable_header_bytes(&mut clipped, header_len, &extensions)
        .expect("a clipped window is not an error");
    assert_eq!(blanked, 5);
    assert!(clipped[payload_start..].iter().all(|byte| *byte == 0));

    // A window that ends before the region touches nothing.
    let mut early = vec![0xAAu8; HEADER_PREAMBLE];
    assert_eq!(
        blank_mutable_header_bytes(&mut early, header_len, &extensions).expect("no overrun"),
        0,
    );
    assert!(early.iter().all(|byte| *byte == 0xAA));

    // A region with no mutable block is left exactly as it is.
    let immutable = &extensions[..vchd_len];
    assert!(!immutable.is_empty());
    let header_len = (HEADER_PREAMBLE + immutable.len()) as u64;
    let mut untouched = vec![0xAAu8; header_len as usize];
    assert_eq!(
        blank_mutable_header_bytes(&mut untouched, header_len, immutable).expect("no block"),
        0,
    );
    assert!(untouched.iter().all(|byte| *byte == 0xAA));
}

use super::*;
// Only the fixtures below need it, so importing it at module scope
// would be an unused import in the non-test build — which is what it
// became when `_descriptor_for` was deleted as dead.
use crate::format::BlockDescriptor;
use crate::{VarlitDecode, VarlitEncode};

/// One index entry for a record of `payload_len` bytes at `record_offset`,
/// with no footer, as a checkpoint would hold it.
fn checkpoint_test_entry(record_offset: u64, payload_len: u64) -> RecordIndexEntry {
    RecordIndexEntry {
        block_id: 7,
        block_version: 1,
        flags: 0,
        sequence: record_offset,
        record_offset,
        payload_offset: record_offset + RECORD_HEADER_LEN,
        payload_len,
        checksum: 0,
        uncompressed_len_hint: 0,
        footer_offset: None,
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        mutable_flags: 0,
        committed: true,
    }
}

fn checkpoint_record_at(record_offset: u64) -> RecordIndexEntry {
    let mut entry = checkpoint_test_entry(record_offset, 0);
    entry.block_id = INDEX_BLOCK_ID;
    entry
}

// The records a checkpoint covers are contiguous, and the predicate used to
// compare `record_offset < previous_offset`. That accepts a *gap*: a
// one-entry checkpoint could name `covered_offset` far past the record it
// listed, and every record in between is on disk, absent from the
// checkpoint, and therefore absent from `index_entries()` and from every
// typed read.
#[test]
fn a_checkpoint_that_skips_records_is_refused() {
    let header_len = 32;
    let first = checkpoint_test_entry(header_len, 8);
    let skipped = checkpoint_test_entry(first.physical_end(), 8);
    let record = checkpoint_record_at(skipped.physical_end());

    let contiguous = IndexCheckpoint {
        covered_offset: record.record_offset,
        entries: vec![first.clone(), skipped.clone()],
    };
    assert!(
        validate_index_checkpoint(
            header_len,
            record.physical_end(),
            &record,
            &contiguous,
            contiguous.entries.len(),
            contiguous.entries.iter(),
        )
        .is_ok(),
        "a checkpoint that tiles its coverage must still validate",
    );

    // The same file, with the middle record simply left out. The observed
    // prefix is what the scan saw, so it agrees with the checkpoint - the
    // hole is only visible as an offset gap.
    let with_gap = IndexCheckpoint {
        covered_offset: record.record_offset,
        entries: vec![first.clone()],
    };
    assert!(matches!(
        validate_index_checkpoint(
            header_len,
            record.physical_end(),
            &record,
            &with_gap,
            with_gap.entries.len(),
            with_gap.entries.iter(),
        ),
        Err(Error::InvalidIndexCheckpoint),
    ));
}

// The same hole, moved to the far end: every entry is contiguous, and the
// checkpoint simply stops before its own record. Only the terminal
// `previous_offset == covered_offset` catches this one.
#[test]
fn a_checkpoint_that_stops_short_of_its_coverage_is_refused() {
    let header_len = 32;
    let first = checkpoint_test_entry(header_len, 8);
    let trailing = checkpoint_test_entry(first.physical_end(), 8);
    let record = checkpoint_record_at(trailing.physical_end());

    let short = IndexCheckpoint {
        covered_offset: record.record_offset,
        entries: vec![first.clone()],
    };
    assert!(matches!(
        validate_index_checkpoint(
            header_len,
            record.physical_end(),
            &record,
            &short,
            short.entries.len(),
            short.entries.iter(),
        ),
        Err(Error::InvalidIndexCheckpoint),
    ));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReplaceTestBlock {
    value: u64,
}

impl crate::VarlitEmpty for ReplaceTestBlock {
    fn empty() -> Self {
        Self { value: 0 }
    }
}

impl VarlitEncode for ReplaceTestBlock {
    const WIRE_TYPE: crate::WireType = crate::WireType::U64;

    fn encode_varlit(&self, encoder: &mut crate::Encoder) -> Result<()> {
        self.value.encode_varlit(encoder)
    }
}

impl VarlitDecode for ReplaceTestBlock {
    const WIRE_TYPE: crate::WireType = crate::WireType::U64;

    fn decode_varlit(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
        Ok(Self {
            value: u64::decode_varlit(decoder)?,
        })
    }
}

impl VarlitBlock for ReplaceTestBlock {
    const ID: u32 = 42;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 0x4649_4C45_0000_002A;
}

impl VarlitReplaceBlock for ReplaceTestBlock {
    fn validate_replacement(_old: &Self, _new: &Self) -> Result<()> {
        Ok(())
    }
}

/// The digest's sequence mark counts the digest record itself.
///
/// `Available(next)` means `next` is the number the digest is about to
/// take, so the mark once it is down is `next`. Storing `next - 1` reported
/// the digest's own sequence as unused, and a writer seeded from that
/// digest would hand it out a second time. The encode/decode pair is the
/// whole of what can be checked here without a writer, and it is where the
/// off-by-one lived.
#[test]
fn a_digest_sequence_mark_survives_the_round_trip_including_the_last_number() {
    for mark in [
        None,
        Some(0u64),
        Some(1),
        Some(u64::MAX - 1),
        Some(u64::MAX),
    ] {
        let payload =
            encode_digest_payload(test_spec(), mark, &[(7, 128)], 4096).expect("encode digest");
        let decoded = decode_digest_payload(&payload, 64, 4096, 4200).expect("decode digest");
        assert_eq!(
            decoded.sequence_high_water, mark,
            "a mark of {mark:?} did not survive the round trip"
        );
    }
}

/// `u64::MAX` is a real mark, not the absence of one.
///
/// It used to be the sentinel for "no mark", which made the two
/// indistinguishable on the wire and left `SequenceState::Exhausted`
/// unrepresentable — the decode arm that maps it was unreachable.
#[test]
fn the_last_sequence_number_is_not_the_absence_of_one() {
    let spec = test_spec();
    let present =
        encode_digest_payload(spec, Some(u64::MAX), &[(7, 128)], 4096).expect("encode present");
    let absent = encode_digest_payload(spec, None, &[(7, 128)], 4096).expect("encode absent");
    assert_ne!(present, absent, "a real u64::MAX mark encodes as no mark");
    assert_eq!(
        decode_digest_payload(&present, 64, 4096, 4200)
            .expect("decode")
            .sequence_high_water,
        Some(u64::MAX)
    );
    assert_eq!(
        decode_digest_payload(&absent, 64, 4096, 4200)
            .expect("decode")
            .sequence_high_water,
        None
    );
}

/// A flag this build does not define means the writer meant more by this
/// record than this build knows, so it is answered with the scan rather
/// than half-understood. The field was written as zero and never read,
/// which foreclosed exactly this evolution path.
#[test]
fn a_digest_with_an_unknown_flag_is_refused() {
    let mut payload =
        encode_digest_payload(test_spec(), Some(9), &[(7, 128)], 4096).expect("encode digest");
    payload[6..8].copy_from_slice(&(DIGEST_FLAGS_KNOWN | 0x0002).to_le_bytes());
    assert!(matches!(
        decode_digest_payload(&payload, 64, 4096, 4200),
        Err(Error::InvalidIndexSegment)
    ));
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReplaceString(String);

impl crate::VarlitEmpty for ReplaceString {
    fn empty() -> Self {
        Self(String::new())
    }
}

impl VarlitEncode for ReplaceString {
    const WIRE_TYPE: crate::WireType = crate::WireType::String;

    fn encode_varlit(&self, encoder: &mut crate::Encoder) -> Result<()> {
        self.0.encode_varlit(encoder)
    }
}

impl VarlitDecode for ReplaceString {
    const WIRE_TYPE: crate::WireType = crate::WireType::String;

    fn decode_varlit(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
        Ok(Self(String::decode_varlit(decoder)?))
    }
}

impl VarlitBlock for ReplaceString {
    const ID: u32 = 43;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Variable;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 0x4649_4C45_0000_002B;
}

impl VarlitReplaceBlock for ReplaceString {
    fn validate_replacement(_old: &Self, _new: &Self) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReplaceKeyed {
    key: u64,
    value: String,
}

impl VarlitEncode for ReplaceKeyed {
    const WIRE_TYPE: crate::WireType = <(u64, String) as VarlitEncode>::WIRE_TYPE;

    fn encode_varlit(&self, encoder: &mut crate::Encoder) -> Result<()> {
        (self.key, self.value.clone()).encode_varlit(encoder)
    }
}

impl VarlitDecode for ReplaceKeyed {
    const WIRE_TYPE: crate::WireType = <(u64, String) as VarlitDecode>::WIRE_TYPE;

    fn decode_varlit(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
        let (key, value) = <(u64, String)>::decode_varlit(decoder)?;
        Ok(Self { key, value })
    }
}

impl VarlitBlock for ReplaceKeyed {
    const ID: u32 = 44;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Variable;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = true;
    const SCHEMA_FINGERPRINT: u64 = 0x4649_4C45_0000_002C;
}

impl VarlitKeyedBlock for ReplaceKeyed {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.key
    }
}

impl VarlitReplaceBlock for ReplaceKeyed {
    fn validate_replacement(old: &Self, new: &Self) -> Result<()> {
        if old.key == new.key {
            Ok(())
        } else {
            Err(Error::ReplacementKeyMismatch)
        }
    }
}

fn digest_test_spec() -> FormatSpec {
    const BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: 12,
        name: "line",
        version: 1,
        kind: BlockKind::Variable,
        fields: &[],
    }];
    let spec = FormatSpec::new(
        b"VSDIG",
        1,
        Endian::Little,
        0,
        IndexPolicy::BlockOffsetChain,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(crate::ReadLimits::finite_all(u64::MAX));
    spec.with_index_policy(spec.index_policy.with_open_digest_on_flush(true))
}

/// The two open routes must agree on what sequence the next append takes.
///
/// This is the assertion the encode/decode round trip cannot make, and it
/// is where the off-by-one actually lived: the digest stored the high-water
/// mark as it stood *before* its own record, so a digest open reported the
/// digest's own sequence as still unused. The scan open frames that record
/// like any other and counts it.
///
/// Latent today only because the digest route builds read-only handles;
/// the day a writer is seeded from a digest it is a sequence handed out
/// twice. Pinned here rather than left to that day.
#[test]
fn the_digest_route_and_the_scan_route_agree_on_the_next_sequence() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("digest-sequence.varlit");
    let mut file = VarlitFile::create(digest_test_spec(), &path)?;
    for _ in 0..4 {
        file.push_info_for_test(12, b"row")?;
    }
    file.flush()?;
    drop(file);

    let scanned = VarlitFile::open_readonly(digest_test_spec(), &path)?;
    let (lazy, source) = VarlitFile::open_readonly_lazy_with_report(digest_test_spec(), &path)?;
    assert_eq!(
        source,
        LazyOpenSource::Digest,
        "the fixture fell back to the scan, so this case compares nothing",
    );
    assert_eq!(
        lazy.sequence_state, scanned.sequence_state,
        "the digest open disagrees with the scan on the next sequence",
    );
    Ok(())
}

/// The append path issues no `lseek` per record either.
///
/// `an_append_window_issues_no_metadata_syscall` pins the `fstat` half and
/// counts `metadata` only, so the `stream_position` the `AppendSnapshot`
/// took for its rollback cursor — an `lseek`, once per record — was
/// invisible to it. Nothing takes it now: the rollback restores
/// `AppendSnapshot::eof`, which the append already holds.
///
/// Measured at 1,000 records: **2,000 seeks before, 1,000 after.** The one
/// that remains is `append_record_at_end`'s own `seek(SeekFrom::End(0))`,
/// which is the check that refuses an append at any offset but the end and
/// is deliberately kept — removing it is the positional-write redesign, as
/// the comment there says. What went is the second one.
///
/// It first went by being *cached* rather than dropped, and this doc
/// carried the reason: the cursor and `eof` are not always the same number,
/// because a reopen's scan leaves the handle where it stopped reading
/// (measured 5493) while `eof` is the committed end (9143). That reason was
/// sound and the cache was not — the offset belongs to a file description
/// `VarlitFile::snapshot` shares and seeks (see `RecordFile`). What the
/// difference actually costs is one cold-path behaviour: a rollback of the
/// *first* append after an open now leaves the handle at the end of file
/// instead of at 5493. Nothing reads this handle sequentially — every
/// append seeks `SEEK_END`, every rewrite seeks its own offset, every read
/// is positional — so the restored offset is a tidiness property, and the
/// end of file is the tidier of the two.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn an_append_window_issues_no_seek_per_record() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("append-seeks.varlit");
    let mut file = VarlitFile::create(test_spec(), &path)?;

    file.push_info_for_test(12, b"warm")?;
    let _ = VarlitFile::take_record_file_seeks();

    const RECORDS: u64 = 1_000;
    for _ in 0..RECORDS {
        file.push_info_for_test(12, b"row")?;
    }
    let seeks = VarlitFile::take_record_file_seeks();
    assert!(
        seeks <= RECORDS,
        "the append window issued {seeks} seeks for {RECORDS} records; the per-record \
         `stream_position` is back",
    );
    Ok(())
}

/// The writer half of the lazy open: no scan, and it can still append.
///
/// Every other writer open calls `load_index` with `ScanIntent::Writer`, so
/// it frames every record in the file — at any size. The framing count is
/// the assertion because it is the cost: `records_framed` counts what the
/// scan does and the digest route does not.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn a_lazy_writer_open_frames_no_record_and_still_appends() -> Result<()> {
    let spec = digest_test_spec();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lazy-writer.varlit");
    let mut file = VarlitFile::create(spec, &path)?;
    for _ in 0..200 {
        file.push_info_for_test(12, b"row")?;
    }
    file.flush()?;
    drop(file);

    let before = VarlitFile::records_framed();
    let scanned = VarlitFile::open(spec, &path)?;
    let scan_frames = VarlitFile::records_framed() - before;
    drop(scanned);
    assert!(
        scan_frames >= 200,
        "the scanning writer open framed {scan_frames} records, so this case measures nothing",
    );

    let before = VarlitFile::records_framed();
    let (mut lazy, source) = VarlitFile::open_lazy_with_report(spec, &path)?;
    let lazy_frames = VarlitFile::records_framed() - before;
    assert_eq!(source, LazyOpenSource::Digest);
    assert_eq!(
        lazy_frames, 1,
        "the lazy writer open frames the digest record and nothing else",
    );

    // And it is a writer: the append path takes the sequence and the tail
    // the digest supplied, so the record lands where the scan says it does.
    let info = lazy.push_info_for_test(12, b"appended")?;
    lazy.flush()?;
    drop(lazy);

    let after = VarlitFile::open_readonly(spec, &path)?;
    assert_eq!(
        after.block_tail_offset(12),
        Some(info.record_offset),
        "the record appended through the lazy writer is not the block's newest",
    );
    // 200 records took 0..=199 and the digest took 200 — counting its own
    // sequence is what F1 fixed. A lazy writer seeded from a digest that
    // still reported 199 would hand out 200 a second time, which is the
    // reuse that fix was latent for until this open existed.
    assert_eq!(
        info.sequence, 201,
        "the lazy writer did not continue the digest's sequence"
    );
    Ok(())
}

/// The two index policies a lazy writer cannot serve are refused at open.
///
/// Both records serialize the resident index this handle does not keep, so
/// writing nothing would leave a file that opens slower than its spec says.
#[test]
fn a_lazy_writer_refuses_the_policies_it_cannot_serve() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lazy-writer-policy.varlit");
    let base = digest_test_spec();
    VarlitFile::create(base, &path)?.flush()?;

    let checkpointing = base.with_index_policy(base.index_policy.with_checkpoint_on_flush(true));
    assert!(matches!(
        VarlitFile::open_lazy(checkpointing, &path),
        Err(Error::LazyWriterIndexPolicy {
            policy: "checkpoint_on_flush"
        })
    ));

    // `segment_on_flush` needs crc32, and the base spec here declares no
    // integrity — without this the spec is refused by `validate` before the
    // check under test is reached, which is how the first version of this
    // case passed for the wrong reason.
    let segmenting = base
        .with_integrity_policy(IntegrityPolicy::Crc32)
        .with_index_policy(base.index_policy.with_segment_on_flush(true));
    assert!(matches!(
        VarlitFile::open_lazy(segmenting, &path),
        Err(Error::LazyWriterIndexPolicy {
            policy: "segment_on_flush"
        })
    ));
    Ok(())
}

/// A file whose every block is non-resident gets a digest at all.
///
/// The guard was `self.index.is_empty()` — "nothing written yet", measured
/// on the resident index. Declare every block non-resident and that index
/// is empty forever, so the file never got a *first* digest however much
/// was appended to it. The irony is exact: a non-resident block gives up
/// the resident directory and reaches its records through the block offset
/// chain instead, and the head of that chain is what the digest carries.
/// The option was inert for the one file it exists to serve.
///
/// Asserted through `_with_report` rather than by counting bytes, because
/// the symptom is silent — the fallback scan returns exactly the right
/// answer, just after framing every record in the file.
#[test]
fn an_all_non_resident_file_still_gets_a_digest() -> Result<()> {
    const RESIDENCY: &[crate::BlockResidencyDescriptor] = &[crate::BlockResidencyDescriptor {
        block_id: 12,
        resident: false,
    }];
    let spec = digest_test_spec().with_block_residency(RESIDENCY);

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("digest-all-non-resident.varlit");
    let mut file = VarlitFile::create(spec, &path)?;
    for _ in 0..4 {
        file.push_info_for_test(12, b"row")?;
    }
    file.flush()?;
    drop(file);

    let (lazy, source) = VarlitFile::open_readonly_lazy_with_report(spec, &path)?;
    assert_eq!(
        source,
        LazyOpenSource::Digest,
        "a file with no resident block never wrote a digest",
    );
    // And the digest is worth having here: the tail is the only way in.
    assert!(
        lazy.block_tail_offset(12).is_some(),
        "the digest carried no tail for the only block in the file",
    );
    Ok(())
}

/// A non-resident append still earns a fresh digest.
///
/// `needs_open_digest` answered "is the last thing in this file already a
/// digest" with `last_resident_block_id`, which a non-resident append does
/// not move. So after one digest, appending only non-resident records wrote
/// no further digest — and those records put the old digest out of last
/// place, which is the one thing that makes it unusable. Every
/// `open_readonly_lazy` from then on silently took the full scan.
///
/// Silent is the operative word: the answer stays correct, the cost does
/// not, and the tail table the digest carries exists precisely to reach
/// non-resident blocks. `_with_report` is what makes it assertable.
///
/// The fixture is deliberately *mixed* — one resident block, one not —
/// because an all-non-resident file exposes a second, separate gap this
/// case is not about: `needs_open_digest` also returns early on
/// `self.index.is_empty()`, and a file whose every block is non-resident
/// has an empty resident index, so it never gets a first digest at all.
/// That one is recorded, not fixed here.
#[test]
fn a_non_resident_append_still_earns_a_new_digest() -> Result<()> {
    const BLOCKS: &[BlockDescriptor] = &[
        BlockDescriptor {
            id: 12,
            name: "line",
            version: 1,
            kind: BlockKind::Variable,
            fields: &[],
        },
        BlockDescriptor {
            id: 13,
            name: "mark",
            version: 1,
            kind: BlockKind::Variable,
            fields: &[],
        },
    ];
    const RESIDENCY: &[crate::BlockResidencyDescriptor] = &[crate::BlockResidencyDescriptor {
        block_id: 12,
        resident: false,
    }];
    let spec = FormatSpec::new(
        b"VSDGN",
        1,
        Endian::Little,
        0,
        IndexPolicy::BlockOffsetChain,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(crate::ReadLimits::finite_all(u64::MAX))
    .with_block_residency(RESIDENCY);
    let spec = spec.with_index_policy(spec.index_policy.with_open_digest_on_flush(true));

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("digest-non-resident.varlit");
    let mut file = VarlitFile::create(spec, &path)?;
    // A resident record first, so there is a digest for the non-resident
    // appends to leave behind.
    file.push_info_for_test(13, b"resident")?;
    file.flush()?;

    for _ in 0..3 {
        file.push_info_for_test(12, b"more")?;
    }
    file.flush()?;
    drop(file);

    let (_lazy, source) = VarlitFile::open_readonly_lazy_with_report(spec, &path)?;
    assert_eq!(
        source,
        LazyOpenSource::Digest,
        "the second flush wrote no digest, so the lazy open fell back to the scan",
    );
    Ok(())
}

fn residency_test_spec() -> FormatSpec {
    const BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: 12,
        name: "line",
        version: 1,
        kind: BlockKind::Variable,
        fields: &[],
    }];
    const RESIDENCY: &[crate::BlockResidencyDescriptor] = &[crate::BlockResidencyDescriptor {
        block_id: 12,
        resident: false,
    }];
    FormatSpec::new(
        b"VSRES",
        1,
        Endian::Little,
        0,
        IndexPolicy::BlockOffsetChain,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_block_residency(RESIDENCY)
    .with_read_limits(crate::ReadLimits::finite_all(u64::MAX))
}

// §5.4. A rolled-back append of a non-resident record must leave nothing
// behind: the file length, the sequence and - the part residency makes
// load-bearing - the block tail, which is the only way back to that block's
// records.
//
// This does *not* exercise `BlockTails::restore`. Both rollback points sit
// above `block_tails.note_appended`, so a rollback always finds the tail
// where it started; the restore is unreachable and is kept for the ordering
// rather than for today. Reverting it to the old rebuild-from-index, or
// deleting it outright, leaves this test green, and that is recorded rather
// than papered over.
#[test]
fn a_rolled_back_non_resident_append_leaves_the_tail_alone() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("residency-rollback.varlit");
    let mut file = VarlitFile::create(residency_test_spec(), &path)?;

    file.write_record(12, 1, 0, b"first")?;
    let tail_before = file.block_tails.tail(12);
    assert!(
        tail_before.is_some(),
        "the tail is published for a non-resident block"
    );
    assert!(file.index.is_empty(), "and nothing is resident");
    let len_before = file.file.metadata()?.len();
    let sequence_before = file.sequence_state;

    inject_write_fault(WriteFault::AppendAfterHeader);
    assert!(matches!(
        file.write_record(12, 1, 0, b"rolled back"),
        Err(Error::Io(_))
    ));

    assert_eq!(
        file.block_tails.tail(12),
        tail_before,
        "the rolled-back record must not stay reachable through the tail",
    );
    assert_eq!(file.file.metadata()?.len(), len_before);
    assert_eq!(file.sequence_state, sequence_before);
    assert!(!file.poison.is_refusing());

    // The next append chains onto the surviving record, not the ghost.
    let info = file.push_info_for_test(12, b"second")?;
    assert_eq!(info.prev_same_block_offset, tail_before);
    Ok(())
}

// The first append of a block publishes no tail if it rolls back, so the
// next surviving record is still the head of the chain.
#[test]
fn a_rolled_back_first_append_leaves_no_tail() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("residency-first.varlit");
    let mut file = VarlitFile::create(residency_test_spec(), &path)?;
    assert_eq!(file.block_tails.tail(12), None);

    inject_write_fault(WriteFault::AppendAfterHeader);
    assert!(matches!(
        file.write_record(12, 1, 0, b"x"),
        Err(Error::Io(_))
    ));
    assert_eq!(
        file.block_tails.tail(12),
        None,
        "a rollback of the first append must leave no tail behind",
    );

    let info = file.push_info_for_test(12, b"real")?;
    assert_eq!(
        info.prev_same_block_offset, None,
        "the first surviving record names no predecessor",
    );
    Ok(())
}

#[test]
fn serialized_directory_cannot_authorize_marking_an_embedded_record() -> Result<()> {
    for budget in [26, 2 * 1024 * 1024] {
        let directory = tempfile::tempdir()?;
        let spec = test_spec()
            .with_index_policy(IndexPolicy::BlockOffsetChain)
            .with_liveness_policy(LivenessPolicy::FooterFlags)
            .with_resource_limits(crate::ReadLimits::STANDARD.with_max_index_bytes(budget));
        let donor_path = directory.path().join("donor");
        let mut donor = VarlitFile::create(spec, &donor_path)?;
        let embedded = donor.push_info_for_test(10, b"value")?;
        let bytes = std::fs::read(&donor_path)?;
        let bytes = &bytes[embedded.record_offset as usize..];
        let path = directory.path().join("outer");
        let mut writer = VarlitFile::create(spec, &path)?;
        let outer = writer.push_info_for_test(10, bytes)?;
        // Two more rows force the first private row to spill at the small
        // budget, and break the unrelated uniform-offset shortcut.
        let _ = writer.push_info_for_test(10, b"pad-one")?;
        let _ = writer.push_info_for_test(10, b"pad-two")?;
        let fake = fault_record_entry(&writer.snapshot, spec, outer.payload_offset, false)?;
        // The embedded frame carries the outer record's own sequence, so a
        // sequence check alone would accept it. The writer keeps no record
        // list to hint from; the refusal below has to come from the boundary
        // check itself.
        assert_eq!(fake.sequence, outer.sequence);
        let before = std::fs::read(&path)?;
        let outcome = writer.mark_record_dead(outer.payload_offset);
        assert!(
            matches!(outcome, Err(Error::InvalidCanonicalEncoding(_))),
            "budget {budget}: {outcome:?}"
        );
        assert_eq!(std::fs::read(&path)?, before);
        // Model a serialized segment claiming the embedded header is a record.
        writer.index = ResidentIndex::adopt_generation(
            &vec![fake],
            writer.snapshot.clone(),
            spec,
            false,
            None,
        )?;
        assert!(!writer.index.has_sequence_hint());
        let before = std::fs::read(&path)?;
        assert!(matches!(
            writer.mark_record_dead(outer.payload_offset),
            Err(Error::InvalidCanonicalEncoding(_))
        ));
        assert_eq!(std::fs::read(&path)?, before);
        assert!(matches!(
            writer.defragment(),
            Err(Error::InvalidCanonicalEncoding(_))
        ));
        assert_eq!(std::fs::read(&path)?, before);
        writer.mark_record_dead(outer.record_offset)?;
        assert!(writer.record_is_dead(outer.record_offset)?);
    }
    Ok(())
}

fn test_spec() -> FormatSpec {
    FormatSpec::new(
        b"VSTEST",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        &[],
    )
    .with_read_limits(crate::ReadLimits::finite_all(u64::MAX))
}

fn replace_test_spec() -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[
        BlockDescriptor {
            id: ReplaceTestBlock::ID,
            name: "ReplaceTestBlock",
            version: ReplaceTestBlock::VERSION,
            kind: BlockKind::Fixed,
            fields: ReplaceTestBlock::FIELDS,
        },
        BlockDescriptor {
            id: ReplaceString::ID,
            name: "ReplaceString",
            version: ReplaceString::VERSION,
            kind: BlockKind::Variable,
            fields: ReplaceString::FIELDS,
        },
        BlockDescriptor {
            id: ReplaceKeyed::ID,
            name: "ReplaceKeyed",
            version: ReplaceKeyed::VERSION,
            kind: BlockKind::Variable,
            fields: ReplaceKeyed::FIELDS,
        },
    ];

    FormatSpec::new(
        b"VSRPL",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_block_identities(&[
        ReplaceTestBlock::IDENTITY,
        ReplaceString::IDENTITY,
        ReplaceKeyed::IDENTITY,
    ])
    .with_read_limits(crate::ReadLimits::finite_all(u64::MAX))
}

fn replacement_policy_spec() -> FormatSpec {
    let spec = replace_test_spec()
        .with_index_policy(IndexPolicy::new(true, true, true, false))
        .with_commit_policy(CommitPolicy::RecordFooter);
    #[cfg(feature = "integrity")]
    {
        spec.with_integrity_policy(IntegrityPolicy::Crc32WithHeader)
    }
    #[cfg(not(feature = "integrity"))]
    {
        spec
    }
}

#[test]
fn replacement_info_translates_grow_and_shrink_offsets() -> Result<()> {
    let grow = ReplacementInfo {
        sequence: 7,
        record_offset: 100,
        old_payload_len: 8,
        new_payload_len: 18,
        old_physical_len: 40,
        new_physical_len: 50,
    };
    assert_eq!(grow.translate_record_offset(99)?, 99);
    assert_eq!(grow.translate_record_offset(100)?, 100);
    assert_eq!(grow.translate_record_offset(140)?, 150);

    let shrink = ReplacementInfo {
        old_payload_len: 18,
        new_payload_len: 8,
        old_physical_len: 50,
        new_physical_len: 40,
        ..grow
    };
    assert_eq!(shrink.translate_record_offset(150)?, 140);
    Ok(())
}

#[test]
fn replace_block_grows_shrinks_and_preserves_sequence_chains_and_snapshot() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("resized.varlit");
    let spec = replacement_policy_spec();
    let mut writer = VarlitFile::create(spec, &path)?;
    writer.push(&ReplaceString("a".into()))?;
    writer.flush()?;
    writer.push(&ReplaceTestBlock { value: 9 })?;
    writer.push(&ReplaceString("tail".into()))?;
    writer.flush()?;

    let old_reader = VarlitFile::open_readonly(spec, &path)?;
    let old_values = all_blocks!(old_reader, ReplaceString)?;
    let old_tail_offset = all_entries!(writer)
        .iter()
        .filter(|entry| entry.block_id == ReplaceString::ID)
        .nth(1)
        .expect("tail record")
        .record_offset;
    let sequence = all_entries!(writer)
        .iter()
        .find(|entry| entry.block_id == ReplaceString::ID)
        .expect("target record")
        .sequence;

    let grown = writer.replace_block(
        0,
        &ReplaceString("a replacement that is substantially longer".into()),
    )?;
    assert_eq!(grown.sequence, sequence);
    assert!(grown.new_physical_len > grown.old_physical_len);
    let translated_tail = grown.translate_record_offset(old_tail_offset)?;
    let entries = all_entries!(writer);
    let tail = entries
        .iter()
        .filter(|entry| entry.block_id == ReplaceString::ID)
        .nth(1)
        .expect("rewritten tail");
    assert_eq!(tail.record_offset, translated_tail);
    assert_eq!(tail.prev_same_block_offset, Some(grown.record_offset));
    assert_eq!(old_values.get_value(0)?, Some(ReplaceString("a".into())));

    let shrunk = writer.replace_block(0, &ReplaceString(String::new()))?;
    assert_eq!(shrunk.sequence, sequence);
    assert!(shrunk.new_physical_len < shrunk.old_physical_len);
    let equal = writer.replace_block(0, &ReplaceString("x".into()))?;
    let equal_again = writer.replace_block(0, &ReplaceString("y".into()))?;
    assert_eq!(equal.new_physical_len, equal_again.new_physical_len);

    let reopened = VarlitFile::open_readonly(spec, &path)?;
    assert_eq!(
        all_blocks!(reopened, ReplaceString)?.get_value(0)?,
        Some(ReplaceString("y".into()))
    );
    assert_eq!(
        all_blocks!(reopened, ReplaceString)?.get_value(1)?,
        Some(ReplaceString("tail".into()))
    );
    assert_no_rewrite_temps(directory.path())?;
    Ok(())
}

#[test]
fn replace_block_validates_key_before_publication() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("keyed.varlit");
    let spec = replace_test_spec();
    let mut writer = VarlitFile::create(spec, &path)?;
    writer.push(&ReplaceKeyed {
        key: 1,
        value: "old".into(),
    })?;
    let original = std::fs::read(&path)?;

    assert!(matches!(
        writer.replace_block(
            0,
            &ReplaceKeyed {
                key: 2,
                value: "new".into(),
            },
        ),
        Err(Error::ReplacementKeyMismatch)
    ));
    assert_eq!(std::fs::read(&path)?, original);
    assert_no_rewrite_temps(directory.path())?;
    Ok(())
}

#[test]
fn replace_block_translates_keyed_predecessors_after_target() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("keyed-chain.varlit");
    let spec =
        replacement_policy_spec().with_index_policy(IndexPolicy::new(true, true, true, true));
    let mut writer = VarlitFile::create(spec, &path)?;
    // the generic push refuses keyed blocks on a keyed-chaining
    // format; the maintaining path is the equivalent entry point.
    let first = writer.push_keyed_info(&ReplaceKeyed {
        key: 1,
        value: "a".into(),
    })?;
    let second = writer.push_with_prev_key_info(
        &ReplaceKeyed {
            key: 1,
            value: "b".into(),
        },
        Some(first.record_offset),
    )?;
    writer.push_with_prev_key_info(
        &ReplaceKeyed {
            key: 1,
            value: "c".into(),
        },
        Some(second.record_offset),
    )?;
    writer.flush()?;

    let info = writer.replace_block(
        0,
        &ReplaceKeyed {
            key: 1,
            value: "a much longer first value".into(),
        },
    )?;
    let snapshot = all_entries!(writer);
    let keyed: Vec<_> = snapshot
        .iter()
        .filter(|entry| entry.block_id == ReplaceKeyed::ID)
        .collect();
    assert_eq!(
        keyed[2].prev_same_key_offset,
        Some(info.translate_record_offset(second.record_offset)?)
    );
    VarlitFile::open_readonly(spec, &path)?;
    Ok(())
}

#[test]
fn replace_block_preserves_transaction_visibility_and_next_sequence() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("transaction.varlit");
    let spec = replace_test_spec()
        .with_index_policy(IndexPolicy::new(true, true, true, false))
        .with_commit_policy(CommitPolicy::TransactionMarker(
            crate::TransactionMarkerMode::Explicit,
        ));
    let mut writer = VarlitFile::create(spec, &path)?;
    writer.push(&ReplaceString("committed-old".into()))?;
    writer.commit()?;
    let old_reader = VarlitFile::open_readonly(spec, &path)?;
    writer.push(&ReplaceString("uncommitted".into()))?;
    let next_sequence = writer.sequence_state.available()?;

    let info = writer.replace_block(0, &ReplaceString("committed-new".into()))?;
    assert_eq!(info.sequence, 0);
    assert_eq!(writer.sequence_state.available()?, next_sequence);
    assert_eq!(
        all_blocks!(old_reader, ReplaceString)?.get_value(0)?,
        Some(ReplaceString("committed-old".into()))
    );
    let new_reader = VarlitFile::open_readonly(spec, &path)?;
    let visible = all_blocks!(new_reader, ReplaceString)?;
    assert_eq!(visible.len(), 1);
    assert_eq!(
        visible.get_value(0)?,
        Some(ReplaceString("committed-new".into()))
    );
    assert_eq!(writer.push(&ReplaceTestBlock { value: 11 })?, next_sequence);
    Ok(())
}

#[test]
fn replacement_publish_rebind_failure_is_explicit_and_poisoned() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("replacement-rebind.varlit");
    let spec = replace_test_spec();
    let mut writer = VarlitFile::create(spec, &path)?;
    writer.push(&ReplaceString("old".into()))?;
    let old_reader = VarlitFile::open_readonly(spec, &path)?;
    inject_write_fault(WriteFault::RebindAfterPublish);

    assert!(matches!(
        writer.replace_block(0, &ReplaceString("new and longer".into())),
        Err(Error::PublishedButRebindFailed { sequence: 0, .. })
    ));
    assert_eq!(
        all_blocks!(old_reader, ReplaceString)?.get_value(0)?,
        Some(ReplaceString("old".into()))
    );
    assert_eq!(
        all_blocks!(VarlitFile::open_readonly(spec, &path)?, ReplaceString)?.get_value(0)?,
        Some(ReplaceString("new and longer".into()))
    );
    assert!(matches!(writer.flush(), Err(Error::WriterPoisoned("file"))));
    assert_no_rewrite_temps(directory.path())?;
    Ok(())
}

#[test]
fn max_sequence_publishes_once_then_reopens_exhausted() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("max.varlit");
    let mut file = VarlitFile::create(test_spec(), &path)?;
    file.sequence_state = SequenceState::Available(u64::MAX);

    assert_eq!(
        file.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"max")?,
        u64::MAX
    );
    assert_eq!(file.sequence_state, SequenceState::Exhausted);
    let original_len = file.file.metadata()?.len();
    let original_cursor = file.file.stream_position()?;
    let original_index_len = file.index.len();
    assert!(matches!(
        file.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"again"),
        Err(Error::SequenceExhausted)
    ));
    assert_eq!(file.file.metadata()?.len(), original_len);
    assert_eq!(file.file.stream_position()?, original_cursor);
    assert_eq!(file.index.len(), original_index_len);
    drop(file);

    let mut reopened = VarlitFile::open(test_spec(), &path)?;
    assert_eq!(reopened.sequence_state, SequenceState::Exhausted);
    assert!(matches!(
        reopened.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"again"),
        Err(Error::SequenceExhausted)
    ));
    Ok(())
}

#[test]
fn append_failure_rolls_back_and_preserves_sequence() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("rollback.varlit");
    let mut file = VarlitFile::create(test_spec(), &path)?;
    // Somewhere no append would have left it, so that "restored the end of
    // file" and "left it where it found it" are different answers.
    file.file.seek_to(0)?;
    let original_len = file.file.metadata()?.len();
    inject_write_fault(WriteFault::AppendAfterHeader);

    assert!(matches!(
        file.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"failed"),
        Err(Error::Io(_))
    ));
    assert_eq!(file.file.metadata()?.len(), original_len);
    // The truncate put the file back; this is the handle put back with it.
    // Not the offset the append found — that was 0, and it is a number this
    // writer no longer has any way to know. See `AppendSnapshot::eof`.
    assert_eq!(file.file.stream_position()?, original_len);
    assert!(file.index.is_empty());
    assert_eq!(file.sequence_state, SequenceState::Available(0));
    assert!(!file.poison.is_refusing());
    assert_eq!(
        file.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"ok")?,
        0
    );
    Ok(())
}

/// A read through the snapshot leaves the writer's appends where they belong.
///
/// `VarlitFile::snapshot` is a `try_clone` of `VarlitFile::file`, so the two
/// share one open file description and one kernel offset. Every read and every
/// append on the primary is positional and logical (`PrimaryFile`), so nothing
/// depends on where that offset is: a layout scan's cursor can move it anywhere
/// and the next append still lands at the end.
#[test]
fn a_snapshot_read_does_not_displace_the_writers_appends() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("shared-description.varlit");
    let mut file = VarlitFile::create(test_spec(), &path)?;
    file.push_info_for_test(12, b"first")?;
    let after_first = file.snapshot.len();

    // What a layout scan does, on the description the writer appends to.
    let mut scan = file.snapshot.cursor_at(0)?;
    let mut magic = [0u8; 4];
    scan.read_exact_at(0, &mut magic)?;

    let info = file.push_info_for_test(12, b"second")?;
    assert_eq!(info.record_offset, after_first);
    assert!(file.snapshot.len() > after_first);
    Ok(())
}

#[test]
fn rollback_failure_poisons_mutation_flush_and_sync() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("poison.varlit");
    let mut file = VarlitFile::create(test_spec(), &path)?;
    inject_write_fault(WriteFault::AppendAfterHeaderWithRollbackFailure);

    assert!(matches!(
        file.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"failed"),
        Err(Error::WriteRollbackFailed {
            operation: "append record",
            ..
        })
    ));
    assert!(matches!(
        file.write_record(METADATA_BLOCK_ID, 1, RECORD_FLAG_INTERNAL, b"again"),
        Err(Error::WriterPoisoned("file"))
    ));
    assert!(matches!(file.flush(), Err(Error::WriterPoisoned("file"))));
    assert!(matches!(file.sync(), Err(Error::WriterPoisoned("file"))));
    Ok(())
}

#[test]
fn fixed_publish_rebind_failure_is_explicit_and_poisoned() -> Result<()> {
    assert_publish_rebind_failure(false)
}

#[test]
fn rewrite_publish_rebind_failure_is_explicit_and_poisoned() -> Result<()> {
    assert_publish_rebind_failure(true)
}

fn assert_publish_rebind_failure(rewrite: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join(if rewrite {
        "rewrite-rebind.varlit"
    } else {
        "fixed-rebind.varlit"
    });
    let spec = replace_test_spec();
    {
        let mut initial = VarlitFile::create(spec, &path)?;
        initial.push(&ReplaceTestBlock { value: 1 })?;
        initial.flush()?;
    }

    let old_reader = VarlitFile::open_readonly(spec, &path)?;
    let old_blocks = all_blocks!(old_reader, ReplaceTestBlock)?;
    let mut writer = VarlitFile::open(spec, &path)?;
    inject_write_fault(WriteFault::RebindAfterPublish);
    let error = if rewrite {
        writer
            .replace_rewrite(0, &ReplaceTestBlock { value: 2 })
            .expect_err("injected rewrite rebind failure must be returned")
    } else {
        writer
            .replace_fixed(0, &ReplaceTestBlock { value: 2 })
            .expect_err("injected fixed rebind failure must be returned")
    };
    match error {
        Error::PublishedButRebindFailed {
            sequence,
            source,
            parent_sync,
        } => {
            assert_eq!(sequence, 1);
            assert!(matches!(*source, Error::Io(_)));
            // no parent-directory sync failure happened here, so the
            // second durability fact is absent rather than fabricated.
            assert!(parent_sync.is_none());
        }
        other => panic!("unexpected publication error: {other:?}"),
    }

    assert_eq!(
        old_blocks.get_value(0)?,
        Some(ReplaceTestBlock { value: 1 })
    );
    assert_eq!(
        all_blocks!(VarlitFile::open_readonly(spec, &path)?, ReplaceTestBlock)?.get_value(0)?,
        Some(ReplaceTestBlock { value: 2 })
    );
    assert!(matches!(
        writer.push(&ReplaceTestBlock { value: 3 }),
        Err(Error::WriterPoisoned("file"))
    ));
    assert_no_rewrite_temps(directory.path())?;

    drop(writer);
    let mut reopened = VarlitFile::open(spec, &path)?;
    assert_eq!(reopened.push(&ReplaceTestBlock { value: 3 })?, 2);
    Ok(())
}

///  double fault, one per replacement API.
///
/// A replacement publication learns two independent durability facts in a
/// fixed order: whether the parent-directory sync succeeded, and whether
/// the writer could rebind to the published generation. Only the second
/// had a path out, so the pre-fix code called rebind with `?` and, when
/// both failed, returned `PublishedButRebindFailed` alone - silently
/// dropping the already-known fact that the published pathname may not
/// survive power loss. Both facts must now arrive together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplacementApi {
    Block,
    Fixed,
    Rewrite,
}

fn assert_double_publication_fault_reports_both_facts(api: ReplacementApi) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join(match api {
        ReplacementApi::Block => "double-block.varlit",
        ReplacementApi::Fixed => "double-fixed.varlit",
        ReplacementApi::Rewrite => "double-rewrite.varlit",
    });
    let spec = replace_test_spec();
    {
        let mut initial = VarlitFile::create(spec, &path)?;
        initial.push(&ReplaceTestBlock { value: 1 })?;
        initial.flush()?;
    }

    let mut writer = VarlitFile::open(spec, &path)?;
    // One arming produces both failures in the order the code sees them:
    // the parent-directory sync inside `replace_path_atomically`, then the
    // rebind that follows it.
    inject_write_fault(WriteFault::ParentSyncThenRebindAfterPublish);
    let error = match api {
        ReplacementApi::Block => writer
            .replace_block(0, &ReplaceTestBlock { value: 2 })
            .map(|info| info.sequence)
            .expect_err("injected double publication fault must be returned"),
        ReplacementApi::Fixed => writer
            .replace_fixed(0, &ReplaceTestBlock { value: 2 })
            .expect_err("injected double publication fault must be returned"),
        ReplacementApi::Rewrite => writer
            .replace_rewrite(0, &ReplaceTestBlock { value: 2 })
            .expect_err("injected double publication fault must be returned"),
    };
    inject_write_fault(WriteFault::None);

    match error {
        Error::PublishedButRebindFailed {
            source,
            parent_sync,
            ..
        } => {
            assert!(matches!(*source, Error::Io(_)), "rebind fact: {source:?}");
            let parent_sync = parent_sync.expect(
                "the parent-directory sync failure observed before the rebind must survive",
            );
            assert!(
                matches!(*parent_sync, Error::Io(_)),
                "parent-sync fact: {parent_sync:?}",
            );
            // The rendered message must carry both facts, because that is
            // what an operator reads out of a log line.
            let rendered = Error::PublishedButRebindFailed {
                sequence: 0,
                source,
                parent_sync: Some(parent_sync),
            }
            .to_string();
            assert!(
                rendered.contains("could not rebind"),
                "message lost the rebind fact: {rendered}"
            );
            assert!(
                rendered.contains("parent-directory sync"),
                "message lost the parent-sync fact: {rendered}"
            );
        }
        other => panic!("unexpected double-fault outcome: {other:?}"),
    }

    // Everything the single-fault contract already promised still holds:
    // publication happened, and the writer is poisoned.
    assert_eq!(
        all_blocks!(VarlitFile::open_readonly(spec, &path)?, ReplaceTestBlock)?.get_value(0)?,
        Some(ReplaceTestBlock { value: 2 })
    );
    assert!(matches!(
        writer.push(&ReplaceTestBlock { value: 3 }),
        Err(Error::WriterPoisoned("file"))
    ));
    assert_no_rewrite_temps(directory.path())?;
    Ok(())
}

#[test]
fn block_replacement_double_publication_fault_reports_both_facts() -> Result<()> {
    assert_double_publication_fault_reports_both_facts(ReplacementApi::Block)
}

#[test]
fn fixed_replacement_double_publication_fault_reports_both_facts() -> Result<()> {
    assert_double_publication_fault_reports_both_facts(ReplacementApi::Fixed)
}

#[test]
fn rewrite_replacement_double_publication_fault_reports_both_facts() -> Result<()> {
    assert_double_publication_fault_reports_both_facts(ReplacementApi::Rewrite)
}

#[cfg(windows)]
#[test]
fn windows_sharing_violation_preserves_generation_and_allows_retry() -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("sharing.varlit");
    let spec = replace_test_spec();
    {
        let mut initial = VarlitFile::create(spec, &path)?;
        initial.push(&ReplaceTestBlock { value: 1 })?;
        initial.flush()?;
    }

    let old_reader = VarlitFile::open_readonly(spec, &path)?;
    let old_blocks = all_blocks!(old_reader, ReplaceTestBlock)?;
    let mut writer = VarlitFile::open(spec, &path)?;
    let blocker = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&path)?;

    assert!(matches!(
        writer.replace_fixed(0, &ReplaceTestBlock { value: 2 }),
        Err(Error::Io(_))
    ));
    assert_eq!(
        all_blocks!(writer, ReplaceTestBlock)?.get_value(0)?,
        Some(ReplaceTestBlock { value: 1 })
    );
    assert_eq!(
        old_blocks.get_value(0)?,
        Some(ReplaceTestBlock { value: 1 })
    );
    assert_no_rewrite_temps(directory.path())?;

    drop(blocker);
    writer.replace_fixed(0, &ReplaceTestBlock { value: 2 })?;
    assert_eq!(
        old_blocks.get_value(0)?,
        Some(ReplaceTestBlock { value: 1 })
    );
    assert_eq!(
        all_blocks!(VarlitFile::open_readonly(spec, &path)?, ReplaceTestBlock)?.get_value(0)?,
        Some(ReplaceTestBlock { value: 2 })
    );
    assert_no_rewrite_temps(directory.path())?;
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_open_generation_rename_handles_every_utf16_alignment() -> Result<()> {
    let directory = tempfile::tempdir()?;
    // Four adjacent ASCII lengths cover every UTF-16 tail alignment in the
    // u64-backed FILE_RENAME_INFO allocation, including no padding at all.
    for suffix_len in 0..4 {
        let target = directory
            .path()
            .join(format!("generation{}", "x".repeat(suffix_len)));
        std::fs::write(&target, b"old")?;
        let mut old = File::open(&target)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory.path())?;
        std::io::Write::write_all(temporary.as_file_mut(), b"new")?;
        temporary.as_file().sync_all()?;
        let mut new = temporary.as_file().try_clone()?;
        std::io::Seek::seek(&mut new, std::io::SeekFrom::Start(0))?;
        let _durability =
            crate::file::publish_open_temp_path_atomically(temporary.into_temp_path(), &target)?;
        assert_eq!(std::fs::read(&target)?, b"new");
        let mut bytes = [0; 3];
        std::io::Read::read_exact(&mut old, &mut bytes)?;
        assert_eq!(&bytes, b"old");
        std::io::Read::read_exact(&mut new, &mut bytes)?;
        assert_eq!(&bytes, b"new");
    }
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_concurrent_replacefilew_keeps_whole_generations_and_old_handle() -> Result<()> {
    use std::sync::{Arc, Barrier};

    for round in 0..64u32 {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("target.bin");
        let replacement_a = directory.path().join("a.bin");
        let replacement_b = directory.path().join("b.bin");
        let old = vec![0x11; 16 * 1024];
        let a = vec![0xA5; 16 * 1024];
        let b = vec![0x5A; 16 * 1024];
        std::fs::write(&target, &old)?;
        std::fs::write(&replacement_a, &a)?;
        std::fs::write(&replacement_b, &b)?;
        let mut old_handle = File::open(&target)?;

        let barrier = Arc::new(Barrier::new(3));
        let first_barrier = Arc::clone(&barrier);
        let first_target = target.clone();
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            replace_path_atomically(&replacement_a, &first_target)
        });
        let second_barrier = Arc::clone(&barrier);
        let second_target = target.clone();
        let second = std::thread::spawn(move || {
            second_barrier.wait();
            replace_path_atomically(&replacement_b, &second_target)
        });
        barrier.wait();
        let first = first.join().expect("first ReplaceFileW thread panicked");
        let second = second.join().expect("second ReplaceFileW thread panicked");

        assert!(
            first.is_ok() || second.is_ok(),
            "round {round}: both replacements failed"
        );
        let unreconciled_indeterminate_loser = [&first, &second]
            .into_iter()
            .any(|result| matches!(result, Err(Error::ReplacePublicationIndeterminate { .. })));
        match std::fs::read(&target) {
            Ok(published) => {
                assert!(
                    published == old || published == a || published == b,
                    "round {round}: target was a partial or mixed generation (len={}, first={:?}, first_result={first:?}, second_result={second:?})",
                    published.len(),
                    published.first()
                );
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && unreconciled_indeterminate_loser =>
            {
                // Documented ReplaceFileW outcome: on 1176 with no backup
                // name, the losing replace can unlink the target name. The
                // loser must have reported the unreconciled
                // ReplacePublicationIndeterminate error  so its
                // caller preserves the temp file and poisons the writer.
            }
            Err(error) => panic!(
                "round {round}: reading target failed without an indeterminate loser \
                 (error={error:?}, first_result={first:?}, second_result={second:?})"
            ),
        }
        let mut retained = Vec::new();
        old_handle.seek(SeekFrom::Start(0))?;
        old_handle.read_to_end(&mut retained)?;
        assert_eq!(retained, old, "round {round}: old handle was rebound");
    }
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_replacefilew_rejects_retained_replacement_handle() -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let directory = tempfile::tempdir()?;
    let target = directory.path().join("target.bin");
    let replacement = directory.path().join("replacement.bin");
    let old = vec![0x11; 4 * 1024];
    let new = vec![0xA5; 4 * 1024];
    std::fs::write(&target, &old)?;
    std::fs::write(&replacement, &new)?;
    let mut retained = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&replacement)?;

    assert!(matches!(
        replace_path_atomically(&replacement, &target),
        Err(Error::Io(_))
    ));
    assert_eq!(std::fs::read(&target)?, old);
    retained.seek(SeekFrom::Start(0))?;
    let mut retained_bytes = Vec::new();
    retained.read_to_end(&mut retained_bytes)?;
    assert_eq!(retained_bytes, new);
    drop(retained);
    replace_path_atomically(&replacement, &target)?;
    assert_eq!(std::fs::read(&target)?, new);
    Ok(())
}

#[cfg(windows)]
#[test]
fn windows_external_truncate_returns_error_without_rebinding_or_panicking() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("truncate.varlit");
    let spec = replace_test_spec();
    {
        let mut writer = VarlitFile::create(spec, &path)?;
        writer.push(&ReplaceTestBlock { value: 7 })?;
        writer.flush()?;
    }

    let reader = VarlitFile::open_readonly(spec, &path)?;
    assert_eq!(all_blocks!(reader, ReplaceTestBlock)?.len(), 1);
    OpenOptions::new().write(true).open(&path)?.set_len(0)?;
    // The reader's snapshot still names the record; reading it now fails
    // instead of rebinding to the truncated file or panicking.
    assert!(all_blocks!(reader, ReplaceTestBlock).is_err());
    Ok(())
}

fn assert_no_rewrite_temps(directory: &Path) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let name = entry?.file_name();
        assert!(
            !name.to_string_lossy().contains(".rewrite."),
            "rewrite temp leaked after failed publication"
        );
    }
    Ok(())
}

#[test]
fn schema_manifest_rejects_huge_and_truncated_counts_before_iteration() {
    let mut prefix = Vec::new();
    prefix.extend_from_slice(&1u16.to_le_bytes());
    prefix.extend_from_slice(&1u16.to_le_bytes());
    prefix.push(Endian::Little.to_byte());
    prefix.extend_from_slice(&0u64.to_le_bytes());
    prefix.extend_from_slice(&[0, 0, 0, 0]);

    let mut huge = prefix.clone();
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    let mut budget = MaterializationBudget::new(test_spec());
    assert!(matches!(
        decode_schema_manifest(&huge, &mut budget),
        Err(Error::InvalidSchemaManifest)
    ));

    let mut truncated = prefix;
    truncated.extend_from_slice(&1u32.to_le_bytes());
    truncated.extend_from_slice(&[0; 8]);
    let mut budget = MaterializationBudget::new(test_spec());
    assert!(matches!(
        decode_schema_manifest(&truncated, &mut budget),
        Err(Error::InvalidSchemaManifest)
    ));
}

#[test]
fn checkpoint_rejects_oversized_extent_before_index_allocation() -> Result<()> {
    let entry = RecordIndexEntry {
        block_id: ReplaceTestBlock::ID,
        block_version: ReplaceTestBlock::VERSION,
        flags: 0,
        sequence: 0,
        record_offset: 32,
        payload_offset: u64::MAX - 1,
        payload_len: 4,
        checksum: 0,
        uncompressed_len_hint: 0,
        footer_offset: None,
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        mutable_flags: 0,
        committed: true,
    };
    let payload = encode_index_checkpoint_payload(test_spec(), &[entry], 128)?;

    let decoded = std::panic::catch_unwind(|| decode_index_checkpoint(test_spec(), &payload, 128));
    assert!(
        decoded.is_ok(),
        "oversized checkpoint extent caused a panic"
    );
    assert!(matches!(
        decoded.expect("checked above"),
        Err(Error::LengthOverflow { .. }) | Err(Error::InvalidIndexCheckpoint)
    ));
    Ok(())
}

/// `sync` on a file this handle created must make the pathname
/// durable, not only its contents - and must do so exactly once, never per
/// sync and never on the append path.
#[test]
fn create_then_sync_makes_the_new_pathname_durable_once() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("create-parent-sync.varlit");
    let spec = replace_test_spec();

    reset_parent_directory_sync_calls();
    let mut writer = VarlitFile::create(spec, &path)?;
    writer.push(&ReplaceTestBlock { value: 1 })?;
    writer.push(&ReplaceTestBlock { value: 2 })?;
    writer.flush()?;
    assert_eq!(
        parent_directory_sync_calls(),
        0,
        "creating and appending must not sync the parent directory"
    );

    writer.sync()?;
    assert_eq!(
        parent_directory_sync_calls(),
        1,
        "the first sync must make the created pathname durable"
    );

    writer.sync()?;
    writer.push(&ReplaceTestBlock { value: 3 })?;
    writer.flush()?;
    writer.sync()?;
    assert_eq!(
        parent_directory_sync_calls(),
        1,
        "the pathname is durable already; later syncs must not repeat it"
    );
    drop(writer);

    // A handle that merely opens an existing pathname created nothing, so
    // it owes no directory entry.
    reset_parent_directory_sync_calls();
    let mut reopened = VarlitFile::open(spec, &path)?;
    reopened.sync()?;
    assert_eq!(
        parent_directory_sync_calls(),
        0,
        "opening an existing pathname must not sync the parent directory"
    );
    Ok(())
}

/// wholesale tail construction must have no quadratic term. The
/// worst case is an index whose first appearances descend by block id,
/// which made every first-seen id shift the whole sorted vector.
#[test]
fn block_tail_construction_moves_nothing_for_reverse_ordered_block_ids() {
    const BLOCKS: u32 = 1_024;

    fn entry(block_id: u32, sequence: u64) -> RecordIndexEntry {
        RecordIndexEntry {
            block_id,
            block_version: 1,
            flags: 0,
            sequence,
            record_offset: sequence * 16,
            payload_offset: sequence * 16 + 8,
            payload_len: 8,
            checksum: 0,
            uncompressed_len_hint: 0,
            footer_offset: None,
            prev_same_block_offset: None,
            prev_same_key_offset: None,
            mutable_flags: 0,
            committed: false,
        }
    }

    let mut index = Vec::new();
    for (sequence, block_id) in (0..BLOCKS).rev().enumerate() {
        index.push(entry(block_id, sequence as u64));
    }
    // A second descending round proves the tails track the *newest* record
    // per block id, not the first.
    for (offset, block_id) in (0..BLOCKS).rev().enumerate() {
        index.push(entry(block_id, (BLOCKS as usize + offset) as u64));
    }

    let tails = BlockTails::from_index(&index);
    assert_eq!(tails.tails.len(), BLOCKS as usize);
    for (position, (block_id, _)) in tails.tails.iter().enumerate() {
        assert_eq!(*block_id, position as u32, "tails must stay sorted by id");
    }
    for entry in &index[BLOCKS as usize..] {
        assert_eq!(
            tails.tail(entry.block_id),
            Some(entry.record_offset),
            "tail must be the newest record for its block id"
        );
    }

    // Identical result to the incremental path, which is the maintained
    // invariant; the difference is only how much movement it costs.
    let mut incremental = BlockTails::new_empty();
    for entry in &index {
        incremental.note_appended(entry.block_id, entry.record_offset);
    }
    assert_eq!(incremental, tails);
}

#[test]
fn atomic_replace_invokes_parent_directory_sync() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let target = directory.path().join("parent-sync-target.bin");
    let replacement = directory.path().join("parent-sync-replacement.bin");
    std::fs::write(&target, b"old")?;
    std::fs::write(&replacement, b"new")?;
    OpenOptions::new()
        .write(true)
        .open(&replacement)?
        .sync_all()?;

    reset_parent_directory_sync_calls();
    replace_path_atomically(&replacement, &target)?;

    assert_eq!(parent_directory_sync_calls(), 1);
    assert_eq!(std::fs::read(target)?, b"new");

    let new_target = directory.path().join("parent-sync-new-target.bin");
    let new_replacement = directory.path().join("parent-sync-new-replacement.bin");
    std::fs::write(&new_replacement, b"first")?;
    OpenOptions::new()
        .write(true)
        .open(&new_replacement)?
        .sync_all()?;
    reset_parent_directory_sync_calls();
    replace_path_atomically(&new_replacement, &new_target)?;

    assert_eq!(parent_directory_sync_calls(), 1);
    assert_eq!(std::fs::read(new_target)?, b"first");
    Ok(())
}

/// PERF: an append must ask the kernel for nothing it already knows.
///
/// Two distinct per-record metadata syscalls used to sit on
/// `write_record_with_prev_key`: `self.file.metadata()?.len()` for the
/// `AppendSnapshot`'s end of file, and a second one inside
/// `checked_snapshot_bounds` reached through `self.snapshot.with_len(..)`.
/// Both facts are now taken from state the writer maintains and from the
/// write that just returned, so both counters must stay at zero across the
/// whole append window whatever the record count.
///
/// The open at the top is not incidental: it pins the snapshot counter
/// *nonzero* where the fstat is still correct, so a "fix" that simply
/// stopped incrementing the counter fails here rather than passing.
#[test]
fn an_append_window_issues_no_metadata_syscall() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("append-syscalls.varlit");

    let _ = VarlitFile::take_snapshot_bounds_fstats();
    let mut file = VarlitFile::create(test_spec(), &path)?;
    assert!(
        VarlitFile::take_snapshot_bounds_fstats() > 0,
        "binding a snapshot at open must still cost the fstat that proves the length",
    );

    let _ = VarlitFile::take_record_file_metadata_calls();
    for index in 0..1_000u32 {
        file.push_info_for_test(METADATA_BLOCK_ID, &index.to_le_bytes())?;
    }

    assert_eq!(
        VarlitFile::take_record_file_metadata_calls(),
        0,
        "the append took the end of file from the snapshot it maintains, not from an fstat",
    );
    assert_eq!(
        VarlitFile::take_snapshot_bounds_fstats(),
        0,
        "the snapshot rebind took the completed write as its proof, not an fstat",
    );

    // The records really are there and really are readable through the
    // rebound snapshot, so this is not "no syscalls because no work".
    assert_eq!(all_entries!(file).len(), 1_000);
    let last = all_entries!(file).last().expect("appended record").clone();
    assert_eq!(file.snapshot.len(), last.checked_physical_end()?);
    Ok(())
}

/// The witness must not be usable to shorten a snapshot past bytes it has
/// not proven: `with_written_len` is growth-only, which is what keeps it
/// safe to hand a value the filesystem was never asked about.
#[test]
fn a_written_witness_cannot_shrink_a_snapshot() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("witness-shrink.varlit");
    let mut file = VarlitFile::create(test_spec(), &path)?;
    file.push_info_for_test(METADATA_BLOCK_ID, b"payload")?;

    let len = file.snapshot.len();
    assert!(
        file.snapshot
            .with_written_len(crate::snapshot::WrittenThrough::after_write(len))
            .is_ok()
    );
    assert!(matches!(
        file.snapshot
            .with_written_len(crate::snapshot::WrittenThrough::after_write(len - 1)),
        Err(Error::SnapshotRangeOutOfBounds { .. }),
    ));
    Ok(())
}

/// PERF: `replace_block` must not rescan the prefix it has already written.
///
/// Every rewritten record validates its chain predecessor against the
/// prefix already in the new generation. That prefix is built in strictly
/// increasing `record_offset` order, so the lookup is a binary search; it
/// used to be `iter().find(..)`, which made the whole call `Theta(N^2)` for
/// any spec carrying a predecessor offset in its footer.
///
/// The assertion is a comparison count, not a wall clock. At N = 4096 the
/// two are ~50k against ~8.4M, and the gap grows by 2x per doubling.
/// **Can an index entry be rebuilt from its record?** Every field but one.
///
/// This is the precondition for a demand-filled index: if the entry the
/// file already holds can be reconstructed from the record it describes,
/// then the index does not have to keep the entry — only where to find it.
/// If it cannot, no amount of call-site refactoring makes the store lazy,
/// and finding that out by trying is cheaper than finding it out after the
/// swap.
///
/// The one field that cannot come back is `committed`. It is writer state
/// with no representation in the record, so a rebuild reports the record as
/// framed rather than as committed. That is why the directory a lazy store
/// would keep is `(record_offset, committed)` and not `record_offset`
/// alone — and this test is what says so, rather than a comment guessing it.
#[test]
fn every_index_entry_can_be_rebuilt_from_its_own_record() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("rebuild.varlit");
    let spec = replacement_policy_spec();
    assert!(
        spec.spec_needs_record_footer(),
        "a format without footers cannot exercise the footer half of a rebuild",
    );
    let mut writer = VarlitFile::create(spec, &path)?;
    for index in 0..64u64 {
        writer.push(&ReplaceString(format!("v{index}")))?;
        if index % 8 == 7 {
            writer.flush()?;
        }
    }
    writer.flush()?;
    drop(writer);

    let opened = VarlitFile::open_readonly(spec, &path)?;
    let resident = all_entries!(opened);
    assert!(resident.len() >= 64, "fixture is too small to say anything");

    let mut file = PrimaryFile::new(OpenOptions::new().read(true).open(&path)?, None);
    read_primary_header(spec, &mut file)?;
    let file_len = file.len()?;
    let mut accounting = ScanAccounting::default();
    let mut committed_differed = 0usize;
    for entry in resident.iter() {
        let rebuilt = match read_record_entry_at(
            spec,
            &mut file,
            file_len,
            entry.record_offset,
            ScanChecks {
                partial_boundary: None,
                checksum_boundary: None,
                verify_checksums: true,
            },
            &mut accounting,
        )? {
            RecordRead::Entry(rebuilt) => rebuilt,
            RecordRead::RecoverableTail(_) => {
                panic!("record at {} did not rebuild", entry.record_offset)
            }
        };
        if rebuilt.committed != entry.committed {
            committed_differed += 1;
        }
        // Everything else has to match exactly, field for field. Comparing
        // the whole struct with `committed` normalised is what makes this a
        // proof rather than a spot check.
        let normalised = RecordIndexEntry {
            committed: entry.committed,
            ..rebuilt
        };
        assert_eq!(
            &normalised, entry,
            "record at {} rebuilt into a different entry",
            entry.record_offset
        );
    }
    // And the same again through the positional `&self` reader, which is
    // the one a demand-filled index would actually use: `read_record_entry_at`
    // above takes `&mut File` and seeks, which no shared read path can do.
    let snapshot = SnapshotFile::new(file.try_clone()?)?;
    for entry in resident.iter() {
        let faulted = fault_record_entry(&snapshot, spec, entry.record_offset, entry.committed)?;
        assert_eq!(
            &faulted, entry,
            "positional fault of the record at {} produced a different entry",
            entry.record_offset
        );
    }
    println!(
        "(rebuild) {} entries rebuilt from their records, {committed_differed} differed only \
         in `committed`; all {} also matched through the positional &self reader",
        resident.len(),
        resident.len()
    );
    Ok(())
}

#[test]
fn replace_block_finds_predecessors_without_rescanning_the_prefix() -> Result<()> {
    const RECORDS: u64 = 4096;

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("replace-probes.varlit");
    let spec = replacement_policy_spec();
    assert!(
        spec.index_policy.block_offset_chain,
        "without a predecessor offset in the footer both lookup arms are skipped and there \
         is nothing to measure",
    );
    let mut writer = VarlitFile::create(spec, &path)?;
    for index in 0..RECORDS {
        writer.push(&ReplaceString(format!("v{index}")))?;
    }
    writer.flush()?;
    let entries = all_entries!(writer).len() as u64;

    let _ = VarlitFile::take_replacement_predecessor_probes();
    writer.replace_block(0, &ReplaceString("replaced".into()))?;
    let probes = VarlitFile::take_replacement_predecessor_probes();

    // At most ceil(log2 N) = 12 probes per chained record; 32 leaves room
    // for the internal records the rewrite also carries.
    let ceiling = 32 * entries;
    assert!(
        probes <= ceiling,
        "predecessor lookup cost {probes} probes over {entries} entries, ceiling {ceiling}; \
         a front-to-back scan of the growing prefix costs ~N^2/2",
    );

    // And the replacement actually happened, so this is not "cheap because
    // the validation was deleted".
    let replaced = VarlitFile::open_readonly(spec, &path)?;
    let values = all_blocks!(replaced, ReplaceString)?;
    assert_eq!(values.len() as u64, RECORDS);
    assert_eq!(values.get_value(0)?.expect("record 0").0, "replaced");
    assert_eq!(
        values
            .get_value(RECORDS as usize - 1)?
            .expect("last record")
            .0,
        format!("v{}", RECORDS - 1),
    );
    Ok(())
}
#[test]
fn keyed_tail_spill_allocation_failures_preserve_entries_and_retry() -> Result<()> {
    let mut failures = 0;
    let mut completed = false;
    for at in 1..128 {
        let mut tails = KeyedTailMap {
            spilled: Some(crate::disk_index::ScratchIndex::new(0)?),
            ..Default::default()
        };
        for key in 0..8u64 {
            tails.offsets.try_reserve(1).unwrap();
            tails.insert(key.to_be_bytes().to_vec(), key + 100);
        }
        let charge = tails.key_payload_bytes;
        let outcome = crate::pib_probe::with_allocation_refusal(at, || tails.spill());
        if outcome.is_err() {
            failures += 1;
            assert_eq!(tails.offsets.len(), 8);
            assert_eq!(tails.key_payload_bytes, charge);
            // Failed batches cannot publish a partial scratch-index root.
            for key in 0..8u64 {
                assert!(
                    tails
                        .spilled
                        .as_ref()
                        .unwrap()
                        .get(&key.to_be_bytes())?
                        .is_none()
                );
            }
            tails.spill()?;
        } else {
            completed = true;
        }
        assert!(tails.offsets.is_empty());
        assert_eq!(tails.key_payload_bytes, 0);
        for key in 0..8u64 {
            assert_eq!(tails.get(&key.to_be_bytes())?, Some(key + 100));
        }
        if completed {
            break;
        }
    }
    assert!(completed && failures >= 9);
    Ok(())
}

/// A segment-chained writer opened at an anchor keeps writing a chain the
/// segment walk accepts: the cursor it resumed with (from the anchor summary
/// or the scanned tail) names exactly the records each new segment covers.
#[cfg(feature = "integrity")]
#[test]
fn an_anchored_writer_open_keeps_the_segment_chain_exact() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("anchored-segments.varlit");
    let spec = test_spec()
        .with_integrity_policy(IntegrityPolicy::Crc32)
        .with_index_policy(IndexPolicy::SegmentOnFlush)
        .with_anchors(crate::AnchorPolicy::every(13, true));
    {
        let mut file = VarlitFile::create(spec, &path)?;
        for round in 0..60u64 {
            for _ in 0..40 {
                file.push_info_for_test(12, &round.to_le_bytes())?;
            }
            file.flush()?;
        }
    }
    let records_before = {
        let file = VarlitFile::open_readonly_scan(spec, &path)?;
        file.record_count()?
    };
    // Opened at an anchor (the file spans many 8 KiB spacings), then appended.
    {
        #[cfg(feature = "scalable-fault-injection")]
        let framed_before = VarlitFile::records_framed();
        let mut file = VarlitFile::open(spec, &path)?;
        #[cfg(feature = "scalable-fault-injection")]
        assert!(
            VarlitFile::records_framed() - framed_before < records_before as u64 / 4,
            "the writer open did not resume at an anchor"
        );
        assert!(file.index.whole_file_count() == Some(records_before));
        for _ in 0..3 {
            for _ in 0..40 {
                file.push_info_for_test(12, b"more")?;
            }
            file.flush()?;
        }
    }
    // The segment chain, walked on its own, must describe the same records as
    // a full scan.
    let mut primary = crate::primary_file::PrimaryFile::new(std::fs::File::open(&path)?, None);
    let (header_len, _) = read_primary_header(spec, &mut primary)?;
    let end = primary.len()?;
    let mut chained = Vec::new();
    walk_segment_chain(spec, &mut primary, header_len, &mut chained, end)?;
    let scanned = VarlitFile::open_readonly_scan(spec, &path)?;
    let mut all = Vec::new();
    scanned.index_entries_into(&mut all)?;
    assert_eq!(chained.len(), all.len());
    for (left, right) in chained.iter().zip(&all) {
        assert_eq!(left.record_offset, right.record_offset);
        assert_eq!(left.sequence, right.sequence);
    }
    Ok(())
}
