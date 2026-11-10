//! The one part of a written record that can change afterwards.
//!
//! Every other way a varlit record stops being the answer is keyed — a tombstone
//! names a block id and a *key*, never an offset — or destructive, where a
//! `replace_*` publishes a generation the record is simply not in. A block with
//! no key had no way to say "this record is dead", which is what a
//! defragmenting rewrite has to be told.
//!
//! `liveness: footer_flags` turns the footer's trailing `reserved` word into a
//! mutable flag word and takes it out of the record checksum. The exclusion is
//! the entire mechanism, and it buys three things that a flag in the *header*
//! would not:
//!
//! - the record's checksum stays valid when a flag is set, so nothing has to
//!   re-read the payload to recompute a CRC;
//! - `crc32_with_header` is unaffected, because that policy covers the header
//!   and the mutable word is in the footer;
//! - a reader that already framed the record does not disagree with the disk,
//!   because the seven fields that establish a record's identity are all header
//!   fields.
//!
//! What this file pins: the option is inert when off, including the strictness
//! the reserved word has always had; turning it on changes the schema hash;
//! it is refused where there is no footer to put the word in; the word really
//! is outside the checksum, and the same word really is inside it when the
//! option is off.

#![cfg(feature = "integrity")]

use varlit::{VarlitBlock, varlit_format};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 10, version = 1, kind = "fixed")]
struct Reading {
    value: u64,
}

#[derive(Clone, Debug, VarlitBlock)]
#[varlit(id = 11, version = 1, kind = "variable")]
struct Embedded {
    bytes: Vec<u8>,
}

varlit_format! {
    struct EmbeddedFormat {
        magic: b"VRDEADE1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        liveness: footer_flags;
        blocks: [Embedded];
    }
}

#[test]
fn marking_embedded_record_bytes_refuses_without_writing() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let donor = directory.path().join("donor.varlit");
    let path = directory.path().join("embedded.varlit");
    let spec = EmbeddedFormat::spec();
    let mut writer = spec.create(&donor)?;
    let record = writer.push_info(&Embedded { bytes: vec![7; 64] })?;
    writer.flush()?;
    drop(writer);
    let donor_bytes = std::fs::read(&donor)?;
    let embedded = donor_bytes
        [record.record_offset as usize..record.footer_offset.unwrap() as usize + 32]
        .to_vec();
    let mut writer = spec.create(&path)?;
    writer.push(&Embedded {
        bytes: embedded.clone(),
    })?;
    writer.flush()?;
    drop(writer);
    let mut writer = spec.open(&path)?;
    for rewritten in [false, true] {
        if rewritten {
            writer.defragment()?;
        }
        let before = std::fs::read(&path)?;
        let fake = before
            .windows(embedded.len())
            .position(|bytes| bytes == embedded)
            .unwrap() as u64;
        assert!(matches!(
            writer.mark_record_dead(fake),
            Err(varlit::Error::InvalidCanonicalEncoding(_))
        ));
        assert_eq!(std::fs::read(&path)?, before);
        writer.verify_all()?;
    }
    Ok(())
}

#[test]
fn lazy_writer_can_mark_a_real_historical_record() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lazy-mark.varlit");
    let spec = On::spec()
        .with_index_policy(varlit::IndexPolicy::BlockOffsetChain.with_open_digest_on_flush(true))
        .with_computed_schema_hash();
    let mut writer = spec.create(&path)?;
    let offset = writer.push_info(&Reading { value: 1 })?.record_offset;
    writer.push(&Reading { value: 2 })?;
    writer.flush()?;
    drop(writer);
    let (mut writer, source) = varlit::VarlitFile::open_lazy_with_report(spec, &path)?;
    assert_eq!(source, varlit::LazyOpenSource::Digest);
    writer.mark_record_dead(offset)?;
    assert!(writer.record_is_dead(offset)?);
    Ok(())
}

// A footer-bearing format, because the word lives in the footer.
varlit_format! {
    pub struct Off {
        magic: b"VRDEADA1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks: [Reading];
    }
}

varlit_format! {
    pub struct On {
        magic: b"VRDEADB1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        liveness: footer_flags;
        blocks: [Reading];
    }
}

// The crash-recovery pairing: keep a crashed writer's tail instead of cutting it.
varlit_format! {
    pub struct Marked {
        magic: b"VRDEADD1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(explicit);
        liveness: footer_flags;
        recovery: mark_tail;
        blocks: [Reading];
    }
}

// The same declaration with the header in the checksum, which is the policy the
// footer placement is supposed to leave alone.
varlit_format! {
    pub struct OnWithHeaderCrc {
        magic: b"VRDEADC1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        liveness: footer_flags;
        blocks: [Reading];
    }
}

fn write(spec: varlit::FormatSpec, path: &std::path::Path) -> varlit::Result<()> {
    let mut writer = spec.create(path)?;
    for value in 0..6 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    Ok(())
}

/// Off is byte-identical to a file written before the policy existed.
#[test]
fn the_option_is_inert_when_it_is_off() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let a = directory.path().join("a.varlit");
    let b = directory.path().join("b.varlit");
    write(Off::spec(), &a)?;
    write(Off::spec(), &b)?;
    assert_eq!(std::fs::read(&a)?, std::fs::read(&b)?);

    // And the trailing word is still zero on disk, which is what the decoder
    // refuses to see change while the option is off.
    let bytes = std::fs::read(&a)?;
    assert!(bytes.len() > 4);
    Ok(())
}

/// Turning it on changes the schema hash, because it changes what the checksum
/// covers — a build that ignored the flag would read every record as corrupt
/// rather than as incompatible, which is the wrong error.
#[test]
fn turning_it_on_changes_the_schema_hash() {
    assert_ne!(
        Off::spec().computed_schema_hash(),
        On::spec().computed_schema_hash(),
    );
}

/// The word is excluded from the checksum when the option is on, and covered by
/// it when the option is off. Measured by patching the byte and reopening.
#[test]
fn the_trailing_word_is_outside_the_checksum_only_when_the_option_is_on() -> varlit::Result<()> {
    fn patch_last_footer_byte(path: &std::path::Path) -> varlit::Result<()> {
        let mut bytes = std::fs::read(path)?;
        // The last record's footer ends the committed prefix; the mutable word
        // is its final four bytes. Setting the low byte of the *last* footer in
        // the file is enough — whichever record it belongs to, that record's
        // checksum either covers it or does not.
        let end = bytes.len();
        bytes[end - 4] = 1;
        std::fs::write(path, &bytes)?;
        Ok(())
    }

    fn reads_back(spec: varlit::FormatSpec, path: &std::path::Path) -> varlit::Result<usize> {
        let spec = spec.with_read_limits(
            spec.read_limits
                .with_integrity_verification(varlit::IntegrityVerification::AtOpen),
        );
        let file = varlit::VarlitFile::open_readonly(spec, path)?;
        Ok(file.blocks::<Reading>()?.len())
    }

    let directory = tempfile::tempdir()?;

    // On: the byte is outside the checksum, so the file still verifies.
    let on = directory.path().join("on.varlit");
    write(On::spec(), &on)?;
    patch_last_footer_byte(&on)?;
    let read = reads_back(On::spec(), &on);
    assert!(
        read.is_ok(),
        "the mutable word must be outside the checksum, got {read:?}",
    );

    // Off: the same byte is inside the checksum *and* is a reserved field the
    // decoder requires to be zero, so the file is refused. Either refusal is
    // the point — what must not happen is a clean read.
    let off = directory.path().join("off.varlit");
    write(Off::spec(), &off)?;
    patch_last_footer_byte(&off)?;
    assert!(
        reads_back(Off::spec(), &off).is_err(),
        "with the option off the trailing word is still checked",
    );
    Ok(())
}

/// `crc32_with_header` covers the header; the mutable word is in the footer, so
/// the two do not collide. This is why the word was not put in `flags`.
#[test]
fn the_header_checksum_policy_is_unaffected() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("hdr.varlit");
    write(OnWithHeaderCrc::spec(), &path)?;

    let mut bytes = std::fs::read(&path)?;
    let end = bytes.len();
    bytes[end - 4] = 0xFF;
    std::fs::write(&path, &bytes)?;

    let spec = OnWithHeaderCrc::spec();
    let spec = spec.with_read_limits(
        spec.read_limits
            .with_integrity_verification(varlit::IntegrityVerification::AtOpen),
    );
    let file = varlit::VarlitFile::open_readonly(spec, &path)?;
    assert_eq!(file.blocks::<Reading>()?.len(), 6);
    Ok(())
}

/// No footer, nowhere to put the word. Refused rather than silently inert.
#[test]
fn it_is_refused_where_there_is_no_footer() {
    let spec = Off::spec()
        .with_commit_policy(varlit::CommitPolicy::None)
        .with_index_policy(varlit::IndexPolicy::ScanOnOpen)
        .with_liveness_policy(varlit::LivenessPolicy::FooterFlags);
    let error = spec
        .validate()
        .expect_err("a footerless format must refuse");
    assert!(
        matches!(error, varlit::Error::InvalidFormatSpec(message) if message.contains("footer")),
        "the refusal must name the footer",
    );
}

/// The mark is durable, advisory, and does not disturb the record.
#[test]
fn a_marked_record_stays_readable_and_says_it_is_dead() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("mark.varlit");

    let mut writer = varlit::VarlitWriter::create(On::spec(), &path)?;
    let mut offsets = Vec::new();
    for value in 0..6 {
        offsets.push(writer.push_info(&Reading { value })?.record_offset);
    }
    writer.flush()?;
    let before = std::fs::metadata(&path)?.len();

    writer.mark_record_dead(offsets[2])?;
    writer.flush()?;
    drop(writer);

    // The file did not grow: the mark is four bytes rewritten in place.
    assert_eq!(std::fs::metadata(&path)?.len(), before);

    // It survives a reopen, and it is the only record marked.
    let spec = On::spec();
    let spec = spec.with_read_limits(
        spec.read_limits
            .with_integrity_verification(varlit::IntegrityVerification::AtOpen),
    );
    let file = varlit::VarlitFile::open_readonly(spec, &path)?;
    for (index, offset) in offsets.iter().enumerate() {
        assert_eq!(
            file.record_is_dead(*offset)?,
            index == 2,
            "record {index} liveness",
        );
        assert_eq!(
            file.record_entry_at(*offset)?.is_dead(),
            index == 2,
            "record {index} positional entry liveness",
        );
    }

    for entry in file.block_chain(Reading::ID)? {
        let entry = entry?;
        assert_eq!(entry.is_dead(), entry.record_offset == offsets[2]);
    }

    // Advisory: the record is still there, still read back, still correct.
    let readings: Vec<Reading> = file
        .blocks::<Reading>()?
        .iter()
        .collect::<varlit::Result<_>>()?;
    assert_eq!(readings.len(), 6);
    assert_eq!(readings[2], Reading { value: 2 });
    Ok(())
}

/// A format that did not opt in has no word to write, and says so.
#[test]
fn marking_is_refused_without_the_policy() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("nopolicy.varlit");
    let mut writer = varlit::VarlitWriter::create(Off::spec(), &path)?;
    let offset = writer.push_info(&Reading { value: 0 })?.record_offset;
    writer.flush()?;
    assert!(matches!(
        writer.mark_record_dead(offset),
        Err(varlit::Error::InvalidFormatSpec(_))
    ));
    Ok(())
}

/// Marking an internal record would make a defragmenter drop the records that
/// say where the commit boundary is.
#[test]
fn an_internal_record_cannot_be_marked() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("internal.varlit");
    let mut writer = varlit::VarlitWriter::create(On::spec(), &path)?;
    writer.push(&Reading { value: 0 })?;
    writer.flush()?;
    drop(writer);

    // The commit marker `on_flush` wrote is the last record in the file.
    let file = varlit::VarlitFile::open_readonly(On::spec(), &path)?;
    let mut buffer = Vec::new();
    let mut walk = file.record_map(&mut buffer)?;
    let mut marker = None;
    while let Some(entry) = walk.advance()? {
        if entry.block_id == varlit::COMMIT_BLOCK_ID {
            marker = Some(entry.record_offset);
        }
    }
    drop(file);
    let marker = marker.expect("on_flush wrote a commit marker");

    let mut writer = varlit::VarlitWriter::open(On::spec(), &path)?;
    assert!(matches!(
        writer.mark_record_dead(marker),
        Err(varlit::Error::ReservedBlockId(_))
    ));
    Ok(())
}

/// A damaged advisory flag must never turn maintenance into transaction rollback.
#[test]
fn defragment_refuses_dead_commit_without_publishing() -> varlit::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("dead-commit.varlit");
    let mut writer = On::spec().create(&path)?;
    let offset = writer.push_info(&Reading { value: 42 })?.record_offset;
    writer.flush()?;
    writer.sync()?;
    drop(writer);
    let reader = On::spec().open_readonly(&path)?;
    let mut buffer = Vec::new();
    let mut walk = reader.record_map(&mut buffer)?;
    let mut marker = None;
    while let Some(entry) = walk.advance()? {
        if entry.block_id == varlit::COMMIT_BLOCK_ID {
            marker = entry.footer_offset;
        }
    }
    let footer_offset = marker.expect("commit footer");
    drop(reader);
    let mut raw = std::fs::OpenOptions::new().write(true).open(&path)?;
    raw.seek(SeekFrom::Start(footer_offset + 28))?;
    raw.write_all(&1u32.to_le_bytes())?;
    drop(raw);
    let mut writer = On::spec().open(&path)?;
    let before = std::fs::read(&path)?;
    assert!(matches!(
        writer.defragment(),
        Err(varlit::Error::InvalidRecordFooter { .. })
    ));
    assert_eq!(std::fs::read(&path)?, before);
    drop(writer);
    let reopened = On::spec().open_readonly(&path)?;
    assert_eq!(
        reopened.read_block_at::<Reading>(offset)?,
        Reading { value: 42 }
    );
    Ok(())
}

#[test]
fn defragment_refuses_payload_damage_without_republishing() -> varlit::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    for integrity in [
        varlit::IntegrityPolicy::Crc32,
        varlit::IntegrityPolicy::Crc32WithHeader,
    ] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("damaged-payload.varlit");
        let spec = On::spec()
            .with_integrity_policy(integrity)
            .with_computed_schema_hash();
        let mut writer = spec.create(&path)?;
        let offset = writer.push_info(&Reading { value: 42 })?.record_offset;
        writer.flush()?;
        drop(writer);
        let mut raw = std::fs::OpenOptions::new().write(true).open(&path)?;
        raw.seek(SeekFrom::Start(offset + 32))?;
        raw.write_all(&43u64.to_le_bytes())?;
        drop(raw);
        let mut writer = spec.open(&path)?;
        let before = std::fs::read(&path)?;
        assert!(matches!(
            writer.defragment(),
            Err(varlit::Error::ChecksumMismatch { .. })
        ));
        assert_eq!(std::fs::read(&path)?, before);
        drop(writer);
        assert!(matches!(
            spec.open_readonly(&path)?.verify_all(),
            Err(varlit::Error::ChecksumMismatch { .. })
        ));
    }
    Ok(())
}

#[test]
fn defragment_preserves_active_readers_on_the_old_generation() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("readers.varlit");
    let mut writer = On::spec().create(&path)?;
    let offset = writer.push_info(&Reading { value: 42 })?.record_offset;
    for value in 0..4096 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    writer.mark_record_dead(offset)?;
    let ready = std::sync::Arc::new(std::sync::Barrier::new(5));
    std::thread::scope(|scope| -> varlit::Result<()> {
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let path = &path;
                let ready = &ready;
                scope.spawn(move || -> varlit::Result<()> {
                    let opened = On::spec().open_readonly(path);
                    ready.wait();
                    let reader = opened?;
                    for _ in 0..10_000 {
                        assert_eq!(reader.read_block_at::<Reading>(offset)?.value, 42);
                    }
                    Ok(())
                })
            })
            .collect();
        ready.wait();
        assert_eq!(writer.defragment()?.records_dropped, 1);
        for reader in readers {
            reader.join().expect("reader thread")?;
        }
        Ok(())
    })?;
    Ok(())
}

/// A clean release clears the dirty bit; an abandoned object leaves it set.
///
/// **What is simulated, and what is not.** A killed process releases the OS
/// object lock — the descriptor closes at exit — while leaving the header bit
/// set, and that combination is the whole verdict. The harness cannot kill a
/// process and keep its page cache, and leaking the handle is not a substitute:
/// `mem::forget` leaks the *lock* too, so the next open is refused by the lock
/// before the bit is ever consulted. (That refusal is itself correct, and it is
/// the two halves working together.) So the on-disk *state* is written by hand
/// here, the way `header_tail_region.rs` writes a torn slot by hand.
///
/// The patch deliberately does not import the writer's encoder: a test that
/// recomputes the layout is the only one that can catch the writer changing it.
#[test]
fn a_clean_close_is_told_apart_from_an_abandoned_one() -> varlit::Result<()> {
    /// FNV-1a 32, restated rather than imported.
    fn checksum(bytes: &[u8]) -> u32 {
        let mut hash = 2_166_136_261u32;
        for byte in bytes {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(16_777_619);
        }
        hash
    }

    /// Sets the liveness block's DIRTY flag, in place, by finding its magic.
    ///
    /// The fixtures above deliberately do not begin with `VLIV`: they did, and
    /// this search matched the *file magic* at offset 0 and patched the
    /// container marker, which failed as `UnsupportedContainer` several layers
    /// away from the cause.
    fn abandon(path: &std::path::Path) -> varlit::Result<()> {
        let mut bytes = std::fs::read(path)?;
        let at = bytes
            .windows(4)
            .position(|window| window == b"VLIV")
            .expect("a liveness format writes a VLIV block");
        // `magic(4) | len(4) | version(2) | flags(2) | crc(4)`
        let payload = at + 8;
        bytes[payload..payload + 2].copy_from_slice(&1u16.to_le_bytes());
        bytes[payload + 2..payload + 4].copy_from_slice(&1u16.to_le_bytes());
        let crc = checksum(&bytes[payload..payload + 4]);
        bytes[payload + 4..payload + 8].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(path, &bytes)?;
        Ok(())
    }

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("dirty.varlit");

    let mut writer = varlit::VarlitWriter::create(On::spec(), &path)?;
    writer.push(&Reading { value: 0 })?;
    writer.flush()?;
    assert!(
        !writer.opened_after_crash(),
        "a freshly created file has no predecessor to have crashed",
    );
    drop(writer);

    // A clean predecessor reads as clean.
    let second = varlit::VarlitWriter::open(On::spec(), &path)?;
    assert!(
        !second.opened_after_crash(),
        "the previous writer released cleanly",
    );
    drop(second);

    // The state a killed writer leaves: lock gone, bit set.
    abandon(&path)?;
    let third = varlit::VarlitWriter::open(On::spec(), &path)?;
    assert!(
        third.opened_after_crash(),
        "an object whose writer never released must read as abandoned",
    );
    drop(third);

    // And the verdict is not sticky: the third writer released cleanly, so the
    // fourth sees a clean predecessor. This is the assertion that catches a
    // `mark_writer_closed` that never runs.
    let fourth = varlit::VarlitWriter::open(On::spec(), &path)?;
    assert!(!fourth.opened_after_crash(), "the verdict is not sticky");
    Ok(())
}

/// A read-only handle never answers the crash question.
///
/// It cannot: answering needs the object lock it does not hold, and a set bit
/// on a file a healthy writer is appending to right now is not a crash.
#[test]
fn a_reader_does_not_claim_a_crash_verdict() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("reader.varlit");
    let mut writer = varlit::VarlitWriter::create(On::spec(), &path)?;
    writer.push(&Reading { value: 0 })?;
    writer.flush()?;
    drop(writer);

    let reader = varlit::VarlitFile::open_readonly(On::spec(), &path)?;
    assert!(!reader.opened_after_crash());
    Ok(())
}

/// A crashed writer's uncommitted tail is kept and flagged, not deleted.
#[test]
fn a_crashed_tail_is_marked_dead_instead_of_truncated() -> varlit::Result<()> {
    fn checksum(bytes: &[u8]) -> u32 {
        let mut hash = 2_166_136_261u32;
        for byte in bytes {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(16_777_619);
        }
        hash
    }
    fn abandon(path: &std::path::Path) -> varlit::Result<()> {
        let mut bytes = std::fs::read(path)?;
        let at = bytes
            .windows(4)
            .position(|window| window == b"VLIV")
            .expect("a liveness format writes a VLIV block");
        let payload = at + 8;
        bytes[payload..payload + 2].copy_from_slice(&1u16.to_le_bytes());
        bytes[payload + 2..payload + 4].copy_from_slice(&1u16.to_le_bytes());
        let crc = checksum(&bytes[payload..payload + 4]);
        bytes[payload + 4..payload + 8].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(path, &bytes)?;
        Ok(())
    }
    fn records(file: &varlit::VarlitFile) -> varlit::Result<usize> {
        let mut buffer = Vec::new();
        let mut walk = file.record_map(&mut buffer)?;
        let mut count = 0;
        while walk.advance()?.is_some() {
            count += 1;
        }
        Ok(count)
    }

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("crashed.varlit");

    // Committed work, then an uncommitted tail — under `explicit`, `flush`
    // writes no marker, so these records reach the file uncommitted.
    let mut writer = varlit::VarlitWriter::create(Marked::spec(), &path)?;
    for value in 0..4 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;
    let committed_len = std::fs::metadata(&path)?.len();

    let mut tail = Vec::new();
    for value in 4..7 {
        tail.push(writer.push_info(&Reading { value })?.record_offset);
    }
    writer.flush()?;
    drop(writer);
    let with_tail = std::fs::metadata(&path)?.len();
    assert!(
        with_tail > committed_len,
        "the uncommitted tail must have reached the file",
    );

    // The state a killed writer leaves.
    abandon(&path)?;

    // The recovery open keeps it.
    let recovered = varlit::VarlitWriter::open(Marked::spec(), &path)?;
    assert!(recovered.opened_after_crash());
    for offset in &tail {
        assert!(
            recovered.record_is_dead(*offset)?,
            "record at {offset} must be marked dead, not deleted",
        );
    }
    drop(recovered);

    // Kept, not cut — and the file ends at a commit point again, so a later
    // marker cannot retroactively commit anything.
    assert!(std::fs::metadata(&path)?.len() > with_tail);
    let file = varlit::VarlitFile::open_readonly(Marked::spec(), &path)?;
    let readings: Vec<Reading> = file
        .blocks::<Reading>()?
        .iter()
        .collect::<varlit::Result<_>>()?;
    assert_eq!(readings.len(), 7, "all seven records survived");
    assert!(records(&file)? >= 7);
    for (index, offset) in tail.iter().enumerate() {
        assert!(file.record_is_dead(*offset)?, "tail record {index}");
    }
    Ok(())
}

/// The same file under the default policy loses the tail, which is the
/// behaviour `mark_tail` exists to change.
#[test]
fn the_default_policy_still_truncates() -> varlit::Result<()> {
    let spec = Marked::spec().with_recovery_policy(varlit::RecoveryPolicy::TruncateTail);
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("cut.varlit");

    let mut writer = varlit::VarlitWriter::create(spec, &path)?;
    for value in 0..4 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;
    let committed_len = std::fs::metadata(&path)?.len();
    for value in 4..7 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);
    assert!(std::fs::metadata(&path)?.len() > committed_len);

    let reopened = varlit::VarlitWriter::open(spec, &path)?;
    drop(reopened);
    assert_eq!(
        std::fs::metadata(&path)?.len(),
        committed_len,
        "TruncateTail must still cut the tail",
    );
    Ok(())
}

/// `mark_tail` has nowhere to write its verdict without the liveness word, and
/// nothing to name a boundary with without a marker.
#[test]
fn mark_tail_refuses_without_its_prerequisites() {
    let no_word = Marked::spec().with_liveness_policy(varlit::LivenessPolicy::None);
    assert!(matches!(
        no_word.validate(),
        Err(varlit::Error::InvalidFormatSpec(m)) if m.contains("liveness")
    ));
    let no_marker = Marked::spec().with_commit_policy(varlit::CommitPolicy::None);
    assert!(matches!(
        no_marker.validate(),
        Err(varlit::Error::InvalidFormatSpec(m)) if m.contains("marker") || m.contains("footer")
    ));
}

#[test]
fn lazy_open_honors_explicit_at_open_verification() -> varlit::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lazy-verify.varlit");
    let spec = On::spec()
        .with_index_policy(varlit::IndexPolicy::BlockOffsetChain.with_open_digest_on_flush(true))
        .with_computed_schema_hash();
    let mut writer = spec.create(&path)?;
    let record = writer.push_info(&Reading { value: 1 })?;
    writer.flush()?;
    drop(writer);
    let strict = spec.with_read_limits(
        spec.read_limits
            .with_integrity_verification(varlit::IntegrityVerification::AtOpen),
    );
    let (reader, source) =
        varlit::VarlitFile::open_readonly_lazy_or_scan_with_report(strict, &path)?;
    assert_eq!(source, varlit::LazyOpenSource::FullScan);
    drop(reader);
    let mut file = std::fs::OpenOptions::new().write(true).open(&path)?;
    file.seek(SeekFrom::Start(record.payload_offset))?;
    file.write_all(&2u64.to_le_bytes())?;
    drop(file);
    assert!(matches!(
        varlit::VarlitFile::open_readonly_lazy_or_scan_with_report(strict, &path),
        Err(varlit::Error::ChecksumMismatch { .. })
    ));
    assert!(matches!(
        varlit::VarlitFile::open_lazy_or_scan_with_report(strict, &path),
        Err(varlit::Error::ChecksumMismatch { .. })
    ));
    Ok(())
}

#[test]
fn commit_records_require_the_internal_flag_and_supported_version() -> varlit::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    for version_damage in [false, true] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("commit-identity.varlit");
        let spec = On::spec();
        let mut writer = spec.create(&path)?;
        writer.push(&Reading { value: 1 })?;
        writer.flush()?;
        let mut buffer = Vec::new();
        let mut walk = writer.record_map(&mut buffer)?;
        let mut marker = None;
        while let Some(entry) = walk.advance()? {
            if entry.block_id == varlit::COMMIT_BLOCK_ID {
                marker = Some(entry.record_offset);
            }
        }
        let marker = marker.expect("commit marker");
        drop(writer);
        let mut file = std::fs::OpenOptions::new().write(true).open(&path)?;
        file.seek(SeekFrom::Start(marker + if version_damage { 4 } else { 6 }))?;
        file.write_all(&(if version_damage { 2u16 } else { 0u16 }).to_le_bytes())?;
        drop(file);
        assert!(matches!(
            varlit::VarlitFile::open_readonly(spec, &path),
            Err(varlit::Error::InvalidCanonicalEncoding(_))
        ));
    }
    Ok(())
}

#[test]
fn marking_between_appends_preserves_records_and_a_live_reader() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("mark-append.varlit");
    let spec = On::spec();
    let mut writer = spec.create(&path)?;
    let first = writer.push_info(&Reading { value: 11 })?;
    writer.flush()?;
    let reader = varlit::VarlitFile::open_readonly(spec, &path)?;
    let original_len = std::fs::metadata(&path)?.len();
    writer.mark_record_dead(first.record_offset)?;
    let second = writer.push_info(&Reading { value: 22 })?;
    assert!(second.record_offset >= original_len);
    writer.flush()?;
    assert!(reader.record_is_dead(first.record_offset)?);
    assert_eq!(
        reader
            .blocks::<Reading>()?
            .iter()
            .collect::<varlit::Result<Vec<_>>>()?,
        vec![Reading { value: 11 }]
    );
    drop(writer);
    let reopened = varlit::VarlitFile::open_readonly(spec, &path)?;
    assert_eq!(
        reopened
            .blocks::<Reading>()?
            .iter()
            .collect::<varlit::Result<Vec<_>>>()?,
        vec![Reading { value: 11 }, Reading { value: 22 }]
    );
    assert!(reopened.record_is_dead(first.record_offset)?);
    assert!(!reopened.record_is_dead(second.record_offset)?);
    Ok(())
}

#[test]
fn marking_refuses_a_damaged_internal_identity_without_writing() -> varlit::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("mark-identity.varlit");
    let mut writer = On::spec().create(&path)?;
    let entry = writer.push_info(&Reading { value: 11 })?;
    writer.flush()?;
    let mut raw = std::fs::OpenOptions::new().write(true).open(&path)?;
    raw.seek(SeekFrom::Start(entry.record_offset + 6))?;
    raw.write_all(&0x8000u16.to_le_bytes())?; // Internal flag on a user block.
    drop(raw);
    let before = std::fs::read(&path)?;
    assert!(matches!(
        writer.mark_record_dead(entry.record_offset),
        Err(varlit::Error::InvalidCanonicalEncoding(_))
    ));
    assert_eq!(std::fs::read(&path)?, before);
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn boundary_proof_reuses_reverse_random_repeated_and_failed_requests() -> varlit::Result<()> {
    use varlit::{IndexPolicy, ReadLimits, VarlitFile};
    // Exercise buffered, spilled, and zero-loan budgets with irregular frames.
    for budget in [0, 64, 32768] {
        for lazy in [false, true] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("boundary-work.varlit");
            let spec = EmbeddedFormat::spec()
                .with_index_policy(IndexPolicy::BlockOffsetChain.with_open_digest_on_flush(true))
                .with_block_residency(&[varlit::BlockResidencyDescriptor {
                    block_id: 11,
                    resident: false,
                }])
                .with_resource_limits(ReadLimits::STANDARD.with_max_index_bytes(budget));
            let mut writer = spec.create(&path)?;
            let mut offsets = Vec::new();
            for n in 0..2048 {
                offsets.push(
                    writer
                        .push_info(&Embedded {
                            bytes: vec![n as u8; n * 37 % 271],
                        })?
                        .record_offset,
                );
            }
            writer.flush()?;
            drop(writer);
            let mut writer = if lazy {
                VarlitFile::open_lazy(spec, &path)?
            } else {
                spec.open(&path)?
            };
            let held = spec.open_readonly(&path)?;
            VarlitFile::take_record_entry_faults();
            writer.mark_record_dead(offsets[2047])?;
            assert!(VarlitFile::take_record_entry_faults() <= 2050);
            // Interior offsets and an out-of-range target cannot grow the
            // frontier or force the already proved prefix to be replayed.
            for &at in offsets.iter().rev() {
                assert!(writer.mark_record_dead(at + 1).is_err());
                writer.mark_record_dead(at)?;
            }
            for n in 0..2048 {
                writer.mark_record_dead(offsets[(n * 977) % 2048])?;
            }
            assert!(writer.mark_record_dead(u64::MAX).is_err());
            let work = VarlitFile::take_record_entry_faults();
            assert_eq!(
                work, 4096,
                "one target frame per valid request, no prefix replay"
            );
            // Following append-only growth extends the proof, preserving old
            // membership. An old reader still observes its own generation.
            let next = writer
                .push_info(&Embedded {
                    bytes: vec![42; 73],
                })?
                .record_offset;
            writer.mark_record_dead(next)?;
            writer.mark_record_dead(offsets[123])?;
            assert!(VarlitFile::take_record_entry_faults() < 12);
            assert!(held.record_is_dead(offsets[123])?);
            assert_eq!(
                held.read_block_at::<Embedded>(offsets[123])?.bytes.len(),
                123 * 37 % 271
            );
        }
    }
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn marked_tail_recovery_frames_linearly_at_two_cardinalities() -> varlit::Result<()> {
    for count in [256, 4096] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("recovery-work.varlit");
        let mut writer = Marked::spec().create(&path)?;
        writer.push(&Reading { value: 99 })?;
        writer.commit()?;
        let mut offsets = Vec::new();
        for value in 0..count {
            offsets.push(writer.push_info(&Reading { value })?.record_offset);
        }
        writer.flush()?;
        drop(writer);
        let mut bytes = std::fs::read(&path)?;
        let at = bytes.windows(4).position(|x| x == b"VLIV").unwrap() + 8;
        bytes[at + 2..at + 4].copy_from_slice(&1u16.to_le_bytes());
        let crc = bytes[at..at + 4].iter().fold(2_166_136_261u32, |crc, b| {
            (crc ^ u32::from(*b)).wrapping_mul(16_777_619)
        });
        bytes[at + 4..at + 8].copy_from_slice(&crc.to_le_bytes());
        // Include a torn final frame to cover truncation before marking.
        bytes.extend_from_slice(b"partial");
        std::fs::write(&path, &bytes)?;
        varlit::VarlitFile::take_record_entry_faults();
        let recovered = Marked::spec().open(&path)?;
        let framed = varlit::VarlitFile::take_record_entry_faults();
        assert!(
            framed <= 2 * count + 16,
            "recovery must not rescan a prefix per mark: {framed}"
        );
        for offset in offsets {
            assert!(recovered.record_is_dead(offset)?);
        }
        drop(recovered);
        let reopened = Marked::spec().open_readonly(&path)?;
        assert_eq!(reopened.blocks::<Reading>()?.len(), count as usize + 1);
    }
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn boundary_proof_stops_on_corrupt_lengths_and_reuses_the_valid_prefix() -> varlit::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    for length in [0, 1, u64::MAX, u64::MAX - 64] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bad-length.varlit");
        let spec = On::spec().with_index_policy(
            varlit::IndexPolicy::BlockOffsetChain.with_open_digest_on_flush(true),
        );
        let mut writer = spec.create(&path)?;
        let mut offsets = Vec::new();
        for value in 0..128 {
            offsets.push(writer.push_info(&Reading { value })?.record_offset);
        }
        writer.flush()?;
        drop(writer);
        let mut writer = varlit::VarlitFile::open_lazy(spec, &path)?;
        let mut raw = std::fs::OpenOptions::new().write(true).open(&path)?;
        raw.seek(SeekFrom::Start(offsets[64] + 16))?;
        raw.write_all(&length.to_le_bytes())?;
        let before = std::fs::read(&path)?;
        varlit::VarlitFile::take_record_entry_faults();
        for _ in 0..16 {
            assert!(writer.mark_record_dead(offsets[127]).is_err());
        }
        assert!(varlit::VarlitFile::take_record_entry_faults() <= 64 + 16);
        assert_eq!(std::fs::read(&path)?, before);
    }
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn a_lazy_writers_own_append_needs_no_historical_boundary_scan() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("own-boundary.varlit");
    let spec = On::spec()
        .with_index_policy(varlit::IndexPolicy::BlockOffsetChain.with_open_digest_on_flush(true));
    let mut writer = spec.create(&path)?;
    for value in 0..4096 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);
    let mut writer = varlit::VarlitFile::open_lazy(spec, &path)?;
    let own = writer.push_info(&Reading { value: 9999 })?;
    varlit::VarlitFile::take_record_entry_faults();
    writer.mark_record_dead(own.record_offset)?;
    assert_eq!(varlit::VarlitFile::take_record_entry_faults(), 1);
    assert!(writer.record_is_dead(own.record_offset)?);
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn rewritten_boundaries_preserve_exact_membership_across_small_pages() -> varlit::Result<()> {
    for budget in [0, 4096, 32768] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("rewritten-boundaries.varlit");
        let spec = EmbeddedFormat::spec()
            .with_resource_limits(varlit::ReadLimits::STANDARD.with_max_index_bytes(budget));
        let mut writer = spec.create(&path)?;
        let mut offsets = Vec::new();
        for n in 0..2048 {
            offsets.push(
                writer
                    .push_info(&Embedded {
                        bytes: vec![n as u8; n * 37 % 271],
                    })?
                    .record_offset,
            );
        }
        writer.flush()?;
        writer.defragment()?;
        let before = std::fs::read(&path)?;
        for &at in &offsets {
            assert!(writer.mark_record_dead(at + 1).is_err());
        }
        assert_eq!(std::fs::read(&path)?, before);
        varlit::VarlitFile::take_record_entry_faults();
        for n in 0..2048 {
            writer.mark_record_dead(offsets[(n * 977) % 2048])?;
        }
        assert_eq!(varlit::VarlitFile::take_record_entry_faults(), 2048);
        writer.flush()?;
        drop(writer);
        let reader = spec.open_readonly(&path)?;
        for &at in &offsets {
            assert!(reader.record_is_dead(at)?);
        }
    }
    Ok(())
}
