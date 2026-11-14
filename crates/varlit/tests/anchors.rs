//! Fixed-offset anchors: records flow around reserved ranges at
//! `append_log_start + k * spacing`, every offset above the I/O layer stays
//! logical, and each anchor names the record spanning it.
//!
//! These use an 8 KiB spacing with 256-byte anchors so that small files cross
//! many anchors, with records both smaller and larger than the spacing.

#![cfg(feature = "integrity")]
#[path = "support/pages.rs"]
mod pages;

use varlit::{AnchorPolicy, FormatSpec, VarlitBlock, VarlitEmpty, VarlitFile, varlit_format};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 20, version = 1, kind = "fixed")]
struct Tick {
    value: u64,
    other: u64,
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 21, version = 1, kind = "variable")]
struct Blob {
    #[varlit(field_id = 1)]
    bytes: Vec<u8>,
}

varlit_format! {
    pub struct AnchoredFormat {
        magic: b"VANCHOR1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        recovery: truncate_tail;
        index: [scan_on_open, block_offset_chain];
        commit: transaction_marker(on_flush);
        blocks: [Tick, Blob];
    }
}

fn spec() -> FormatSpec {
    AnchoredFormat::spec().with_anchors(AnchorPolicy::every(13, true))
}

fn blob(i: u64) -> Blob {
    // Mostly small, every 50th larger than two anchor spacings.
    let len = if i % 50 == 49 {
        20_000
    } else {
        (i % 300) as usize
    };
    Blob {
        bytes: (0..len).map(|n| (n as u64 ^ i) as u8).collect(),
    }
}

fn write(path: &std::path::Path, records: u64) -> varlit::Result<()> {
    let mut file = spec().create(path)?;
    for i in 0..records {
        file.push(&Tick {
            value: i,
            other: !i,
        })?;
        file.push(&blob(i))?;
        if i % 97 == 0 {
            file.flush()?;
        }
    }
    file.flush()?;
    Ok(())
}

fn check_all(file: &VarlitFile, records: u64) -> varlit::Result<()> {
    let mut buffer = Vec::new();
    let mut map = file.record_map(&mut buffer)?;
    let mut scratch = Vec::new();
    let (mut tick, mut data) = (Tick::empty(), Blob::empty());
    let (mut ticks, mut blobs) = (0u64, 0u64);
    while let Some(entry) = map.advance()? {
        if entry.block_id == Tick::ID {
            file.decode_block_into(&entry, &mut scratch, &mut tick)?;
            assert_eq!(
                tick,
                Tick {
                    value: ticks,
                    other: !ticks
                }
            );
            ticks += 1;
        } else if entry.block_id == Blob::ID {
            file.decode_block_into(&entry, &mut scratch, &mut data)?;
            assert_eq!(data, blob(blobs));
            blobs += 1;
        }
    }
    assert_eq!((ticks, blobs), (records, records));
    Ok(())
}

#[test]
fn records_flow_around_anchors_and_read_back_whole() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("anchored.varlit");
    write(&path, 600)?;
    let file = VarlitFile::open_readonly(spec(), &path)?;
    let layout = file.anchor_layout().expect("an anchored file");
    assert_eq!((layout.block(), layout.hole()), (8192, 256));
    check_all(&file, 600)?;

    // Every anchor inside the file decodes and names the record spanning it.
    let physical = std::fs::metadata(&path)?.len();
    let mut anchors = 0;
    let mut k = 1;
    while let Some(at) = layout.anchor_logical(k) {
        if layout.anchor_physical(k).unwrap() + layout.hole() > physical {
            break;
        }
        let anchor = file.anchor(k)?.expect("an intact anchor");
        assert_eq!(anchor.logical, at);
        assert!(anchor.record_start <= at && at < anchor.record_end);
        anchors += 1;
        k += 1;
    }
    assert!(anchors > 20, "the fixture crossed only {anchors} anchors");
    // The log is the file minus its anchors.
    let mut buffer = Vec::new();
    let mut map = file.record_map(&mut buffer)?;
    map.fill()?;
    assert_eq!(physical, layout.physical_end(map.resume_offset()).unwrap());
    Ok(())
}

#[test]
fn a_sequence_is_reached_from_an_anchor_without_scanning_the_prefix() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("seek.varlit");
    write(&path, 600)?;
    let file = VarlitFile::open_readonly(spec(), &path)?;
    let mut buffer = Vec::new();
    for target in [0u64, 1, 333, 700, 1100, 1190] {
        let from = file.seek_sequence(target)?;
        let mut map = file.record_map_from(&mut buffer, from)?;
        let mut framed = 0u64;
        let found = loop {
            let entry = map.advance()?.expect("the target sequence exists");
            framed += 1;
            if entry.sequence >= target {
                break entry;
            }
        };
        assert!(found.sequence >= target && found.sequence <= target + 1);
        // At most one spacing's worth of records lies between the start
        // the anchors give and the target.
        assert!(framed < 400, "framed {framed} records to reach {target}");
    }
    Ok(())
}

#[test]
fn a_file_torn_inside_an_anchor_recovers_and_appends_through_it() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("torn.varlit");
    write(&path, 200)?;
    let layout = VarlitFile::open_readonly(spec(), &path)?
        .anchor_layout()
        .unwrap();
    // Cut the file in the middle of its last whole anchor.
    let physical = std::fs::metadata(&path)?.len();
    let k = (1..)
        .take_while(|k| layout.anchor_physical(*k).unwrap() + layout.hole() <= physical)
        .last()
        .unwrap();
    let cut = layout.anchor_physical(k).unwrap() + layout.hole() / 2;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_len(cut)?;

    let mut file = spec().open_recover(&path)?;
    assert!(
        file.anchor(k)?.is_none(),
        "a torn anchor is not part of the log"
    );
    let survived = crate::pages::all_entries!(file).len();
    // Recovery cut back to the last commit point; append well past the
    // anchor's logical offset again (more than one spacing of records).
    for i in 0..500 {
        file.push(&Tick {
            value: 10_000 + i,
            other: 0,
        })?;
    }
    file.flush()?;
    drop(file);
    let file = VarlitFile::open_readonly(spec(), &path)?;
    assert!(crate::pages::all_entries!(file).len() > survived);
    assert!(file.anchor(k)?.is_some(), "the append rewrote the anchor");
    Ok(())
}

#[test]
fn a_rewrite_keeps_the_layout_and_every_record() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("rewrite.varlit");
    write(&path, 300)?;
    // A same-size fixed replacement rewrites in place; the rest of the file,
    // anchors included, is untouched.
    let mut file = spec().open(&path)?;
    file.replace_block(
        150,
        &Tick {
            value: 150,
            other: !150,
        },
    )?;
    file.flush()?;
    drop(file);
    let file = VarlitFile::open_readonly(spec(), &path)?;
    assert!(file.anchor(1)?.is_some());
    check_all(&file, 300)
}

#[test]
fn an_unanchored_spec_reads_anchored_files_by_their_header() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("header.varlit");
    write(&path, 200)?;
    // The file's own header decides, not the spec that opens it.
    let plain = AnchoredFormat::spec().with_anchors(AnchorPolicy::NONE);
    let file = VarlitFile::open_readonly(plain, &path)?;
    assert!(file.anchor_layout().is_some());
    check_all(&file, 200)?;
    // And a file created unanchored stays unanchored when an anchored spec
    // appends to it.
    let legacy = directory.path().join("legacy.varlit");
    {
        let mut file = plain.create(&legacy)?;
        file.push(&Tick {
            value: 0,
            other: !0,
        })?;
        file.push(&blob(0))?;
        file.flush()?;
    }
    let mut file = spec().open(&legacy)?;
    assert!(file.anchor_layout().is_none());
    for i in 1..200 {
        file.push(&Tick {
            value: i,
            other: !i,
        })?;
        file.push(&blob(i))?;
    }
    file.flush()?;
    drop(file);
    check_all(&VarlitFile::open_readonly(spec(), &legacy)?, 200)
}

#[cfg(feature = "mmap")]
#[test]
fn mapped_windows_match_owned_reads_across_anchors() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("mapped.varlit");
    write(&path, 300)?;
    let file = VarlitFile::open_readonly(spec(), &path)?;
    // SAFETY: nothing else touches the file while the mapping lives.
    let mapped = unsafe { file.mmap_payloads()? };
    let mut out = Vec::new();
    for entry in mapped.index_entries() {
        file.read_payload_into(entry, &mut out)?;
        assert_eq!(mapped.payload_window(entry)?, &out[..]);
        // SAFETY: as above.
        let window = unsafe { file.mmap_payload_window(entry)? };
        assert_eq!(window.as_bytes(), &out[..]);
    }
    Ok(())
}

varlit::varlit_format! {
    pub format AnchoredIndex {
        magic: b"VANCIDX1"; version: 1; integrity: crc32_with_header; index: keyed_offset_chain;
        blocks {
            variable Entry(id = 30, key = [id], key_index = disk) { id: u64, payload: Vec<u8>, }
        }
    }
}

fn entry(id: u64, round: u64) -> Entry {
    let len = if id.is_multiple_of(40) {
        12_000
    } else {
        (id % 200) as usize
    };
    Entry {
        id,
        payload: (0..len).map(|n| (n as u64 ^ id ^ round) as u8).collect(),
    }
}

#[test]
fn an_indexed_primary_writes_reads_and_recovers_across_anchors() -> varlit::Result<()> {
    use varlit::{DiskIndexOptions, DiskIndexPlan, DiskIndexedBlock};
    use varlit::{VarlitIndexedReader, VarlitIndexedWriter};
    const BLOCKS: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Entry>()];
    let spec = AnchoredIndex::spec().with_anchors(AnchorPolicy::every(13, true));
    let plan = DiskIndexPlan::canonical(spec, BLOCKS)?;
    let options = DiskIndexOptions::default();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("indexed.varlit");
    let get = |reader: &VarlitIndexedReader, id: u64| -> varlit::Result<Option<Entry>> {
        let mut out = Entry::empty();
        Ok(reader.get::<Entry>(&id, &mut out)?.then_some(out))
    };

    let mut writer = VarlitIndexedWriter::create(spec, &path, options, plan)?;
    let mut reader = VarlitIndexedReader::open(spec, &path, options, plan)?;
    for round in 0..3u64 {
        for id in 0..300u64 {
            writer.push_info(&entry(id, round))?;
        }
        writer.sync()?;
        // The unconfirmed tail is read from the primary, across anchors.
        reader.follow()?;
        for id in [0, 1, 40, 199, 299] {
            assert_eq!(get(&reader, id)?, Some(entry(id, round)));
        }
    }
    let physical = std::fs::metadata(&path)?.len();
    let layout = varlit::AnchorLayout::new(0, 8192, 256)?;
    assert!(
        physical > 3 * layout.block(),
        "the fixture crossed too few anchors"
    );

    // Unsynced records at close: nothing is confirmed on the way out, which is
    // the state a crash leaves. The next writer open replays the synced tail
    // across anchors and cuts the unsynced records.
    for id in 0..50u64 {
        writer.push_info(&entry(id, 99))?;
    }
    drop(writer);
    drop(reader);
    let writer = VarlitIndexedWriter::open(spec, &path, options, plan)?;
    drop(writer);
    let reader = VarlitIndexedReader::open(spec, &path, options, plan)?;
    for id in [0, 40, 299] {
        assert_eq!(get(&reader, id)?, Some(entry(id, 2)));
    }
    Ok(())
}

/// Records an open frames, which resuming at an anchor bounds to the last
/// spacing.
#[cfg(feature = "scalable-fault-injection")]
fn framed_by(open: impl FnOnce() -> varlit::Result<()>) -> varlit::Result<u64> {
    let before = VarlitFile::records_framed();
    open()?;
    Ok(VarlitFile::records_framed() - before)
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn an_open_resumes_at_the_newest_summarized_anchor() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("resume.varlit");
    write(&path, 3_000)?;
    let total = {
        let file = VarlitFile::open_readonly_scan(spec(), &path)?;
        file.record_count()?
    };
    // Both opens frame at most a couple of spacings' worth of records, not the
    // file: 6,000 data records across dozens of 8 KiB anchors.
    let reader_frames = framed_by(|| VarlitFile::open_readonly(spec(), &path).map(drop))?;
    let writer_frames = framed_by(|| spec().open(&path).map(drop))?;
    assert!(
        reader_frames < 600 && writer_frames < 600,
        "an anchored open framed {reader_frames} / {writer_frames} of {total} records"
    );

    // And it is the same file: every record, the same count, intact chains.
    let file = VarlitFile::open_readonly(spec(), &path)?;
    assert_eq!(file.record_count()?, total);
    check_all(&file, 3_000)?;
    let chained = file.block_chain(Tick::ID)?.count();
    assert_eq!(
        chained, 3_000,
        "the tick chain reaches every tick from the seeded tail"
    );
    drop(file);

    // A writer opened this way continues the sequence and the chains.
    {
        let mut file = spec().open(&path)?;
        for i in 3_000..3_100 {
            file.push(&Tick {
                value: i,
                other: !i,
            })?;
            file.push(&blob(i))?;
        }
        file.flush()?;
    }
    let file = VarlitFile::open_readonly_scan(spec(), &path)?;
    check_all(&file, 3_100)?;
    let mut sequences: Vec<u64> = crate::pages::all_entries!(file)
        .iter()
        .map(|entry| entry.sequence)
        .collect();
    let count = sequences.len();
    sequences.sort_unstable();
    sequences.dedup();
    assert_eq!(sequences.len(), count, "no sequence was issued twice");
    assert_eq!(file.block_chain(Tick::ID)?.count(), 3_100);
    Ok(())
}

/// A whole-file rewrite writes its anchors with summaries too, so the first
/// open after it resumes near the end instead of scanning the new generation,
/// and lands on the same file a full scan sees.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn a_rewritten_generation_still_opens_at_its_newest_anchor() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("rewritten.varlit");
    write(&path, 3_000)?;
    {
        // The same value, so the contents stay checkable; the replacement
        // still rewrites every record into a new generation.
        let mut file = spec().open(&path)?;
        file.replace_block(
            1_500,
            &Tick {
                value: 1_500,
                other: !1_500,
            },
        )?;
    }
    let total = {
        let file = VarlitFile::open_readonly_scan(spec(), &path)?;
        check_all(&file, 3_000)?;
        file.record_count()?
    };
    let reader_frames = framed_by(|| VarlitFile::open_readonly(spec(), &path).map(drop))?;
    let writer_frames = framed_by(|| spec().open(&path).map(drop))?;
    assert!(
        reader_frames < 600 && writer_frames < 600,
        "an open after a rewrite framed {reader_frames} / {writer_frames} of {total} records"
    );

    let file = VarlitFile::open_readonly(spec(), &path)?;
    assert_eq!(file.record_count()?, total);
    check_all(&file, 3_000)?;
    assert_eq!(file.block_chain(Tick::ID)?.count(), 3_000);
    assert_eq!(file.block_chain(Blob::ID)?.count(), 3_000);
    drop(file);

    // A writer resumed from a rewrite's summary continues sequences and chains.
    {
        let mut file = spec().open(&path)?;
        for i in 3_000..3_050 {
            file.push(&Tick {
                value: i,
                other: !i,
            })?;
            file.push(&blob(i))?;
        }
        file.flush()?;
    }
    let file = VarlitFile::open_readonly_scan(spec(), &path)?;
    check_all(&file, 3_050)?;
    let mut sequences: Vec<u64> = crate::pages::all_entries!(file)
        .iter()
        .map(|entry| entry.sequence)
        .collect();
    let count = sequences.len();
    sequences.sort_unstable();
    sequences.dedup();
    assert_eq!(sequences.len(), count, "no sequence was issued twice");
    assert_eq!(file.block_chain(Tick::ID)?.count(), 3_050);
    Ok(())
}

#[test]
fn an_in_place_replacement_withdraws_the_summaries_after_it() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("withdraw.varlit");
    write(&path, 2_000)?;
    // Replace a tick well before the newest anchors: it takes a new, highest
    // sequence while staying where it is.
    {
        let mut file = spec().open(&path)?;
        file.replace_fixed(
            1_000,
            &Tick {
                value: 1_000,
                other: !1_000,
            },
        )?;
        file.flush()?;
    }
    // Reopen and append: the next sequence must be past the replaced one.
    {
        let mut file = spec().open(&path)?;
        file.push(&Tick {
            value: 2_000,
            other: !2_000,
        })?;
        file.flush()?;
    }
    let file = VarlitFile::open_readonly_scan(spec(), &path)?;
    let mut sequences: Vec<u64> = crate::pages::all_entries!(file)
        .iter()
        .map(|entry| entry.sequence)
        .collect();
    let count = sequences.len();
    sequences.sort_unstable();
    sequences.dedup();
    assert_eq!(sequences.len(), count, "a sequence was issued twice");
    Ok(())
}

#[test]
fn a_torn_tail_after_an_anchor_still_opens_to_the_last_commit() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("torn-tail.varlit");
    write(&path, 2_000)?;
    {
        let mut file = spec().open(&path)?;
        for i in 0..50 {
            file.push(&Tick {
                value: 90_000 + i,
                other: 0,
            })?;
        }
        // Not flushed: the tail is uncommitted, then torn.
    }
    let len = std::fs::metadata(&path)?.len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_len(len - 7)?;
    let file = VarlitFile::open_readonly(spec(), &path)?;
    check_all(&file, 2_000)?;
    drop(file);
    let mut writer = spec().open_recover(&path)?;
    writer.push(&Tick {
        value: 2_000,
        other: !2_000,
    })?;
    writer.flush()?;
    drop(writer);
    let file = VarlitFile::open_readonly(spec(), &path)?;
    assert_eq!(all_blocks_count(&file)?, 2_001);
    Ok(())
}

fn all_blocks_count(file: &VarlitFile) -> varlit::Result<usize> {
    Ok(crate::pages::all_blocks!(file, Tick)?.len())
}

/// Under the default policy an open reads at most about one spacing (4 MiB)
/// of records, whatever the file's length: a 34 MiB file opens by framing a
/// small fraction of its records, for a reader and for a writer.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn a_default_spec_open_reads_about_one_spacing() -> varlit::Result<()> {
    let spec = AnchoredFormat::spec();
    assert_eq!(spec.anchors, AnchorPolicy::DEFAULT);
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("default-spacing.varlit");
    const PAIRS: u64 = 8_000;
    {
        let mut file = spec.create(&path)?;
        for i in 0..PAIRS {
            file.push(&Tick {
                value: i,
                other: !i,
            })?;
            file.push(&Blob {
                bytes: vec![i as u8; 4_096],
            })?;
            if i % 500 == 0 {
                file.flush()?;
            }
        }
        file.flush()?;
    }
    let total = VarlitFile::open_readonly_scan(spec, &path)?.record_count()?;
    let reader = framed_by(|| VarlitFile::open_readonly(spec, &path).map(drop))?;
    let writer = framed_by(|| spec.open(&path).map(drop))?;
    // One 4 MiB spacing holds about 1,000 pairs; allow two spacings.
    assert!(
        reader < 4_200 && writer < 4_200,
        "a default-spec open framed {reader} / {writer} of {total} records"
    );
    let file = VarlitFile::open_readonly(spec, &path)?;
    assert_eq!(file.record_count()?, total);
    assert_eq!(file.block_chain(Tick::ID)?.count() as u64, PAIRS);
    // A lookup by sequence frames at most about one spacing too.
    let mut buffer = Vec::new();
    let mut worst = 0;
    for target in (0..PAIRS * 2).step_by(397) {
        let before = VarlitFile::records_framed();
        let mut map = file.record_map_from(&mut buffer, file.seek_sequence(target)?)?;
        while map.advance()?.expect("the sequence exists").sequence < target {}
        worst = worst.max(VarlitFile::records_framed() - before);
    }
    eprintln!("default spacing: open framed {reader} / {writer}, worst lookup framed {worst}");
    assert!(worst < 4_200, "a lookup framed {worst} records");
    Ok(())
}

/// Every lookup a reader makes after opening at an anchor — of records the
/// open never framed, anywhere between earlier anchors — answers exactly what
/// a full scan answers, and each costs at most about one spacing of framing.
/// Checked on a freshly appended file and on a rewritten generation, whose
/// anchors carry the rewrite's summaries.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn lookups_between_anchors_match_a_full_scan() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    for rewritten in [false, true] {
        let path = directory.path().join(if rewritten {
            "between-rewritten.varlit"
        } else {
            "between.varlit"
        });
        write(&path, 3_000)?;
        if rewritten {
            let mut file = spec().open(&path)?;
            // Tick #20 (the index counts this block's records), same value.
            file.replace_block(
                20,
                &Tick {
                    value: 20,
                    other: !20,
                },
            )?;
        }
        let scanned = VarlitFile::open_readonly_scan(spec(), &path)?;
        let truth = crate::pages::all_entries!(scanned);
        let opened_frames = framed_by(|| VarlitFile::open_readonly(spec(), &path).map(drop))?;
        let file = VarlitFile::open_readonly(spec(), &path)?;
        assert!(opened_frames < 600, "the open framed {opened_frames}");

        // The paged directory of the anchored handle is the scanned one.
        assert_eq!(
            crate::pages::all_entries!(file),
            truth,
            "rewritten={rewritten}"
        );
        assert_eq!(file.record_count()?, scanned.record_count()?);

        // Random access by offset: every record, decoded and checked.
        let (mut scratch, mut tick, mut data) = (Vec::new(), Tick::empty(), Blob::empty());
        let (mut ticks, mut blobs) = (0u64, 0u64);
        for entry in &truth {
            assert_eq!(&file.record_entry_at(entry.record_offset)?, entry);
            if entry.block_id == Tick::ID {
                file.read_block_at(entry.record_offset, &mut scratch, &mut tick)?;
                assert_eq!(
                    tick,
                    Tick {
                        value: ticks,
                        other: !ticks
                    }
                );
                ticks += 1;
            } else if entry.block_id == Blob::ID {
                file.read_block_at(entry.record_offset, &mut scratch, &mut data)?;
                assert_eq!(data, blob(blobs));
                blobs += 1;
            }
        }
        assert_eq!((ticks, blobs), (3_000, 3_000));

        // By sequence: every sequence, from the anchor before it.
        let mut buffer = Vec::new();
        let (mut worst, mut total) = (0u64, 0u64);
        for expected in &truth {
            let before = VarlitFile::records_framed();
            let from = file.seek_sequence(expected.sequence)?;
            let mut map = file.record_map_from(&mut buffer, from)?;
            let found = loop {
                let entry = map.advance()?.expect("the sequence exists");
                if entry.sequence >= expected.sequence {
                    break entry;
                }
            };
            let framed = VarlitFile::records_framed() - before;
            assert_eq!(&found, expected, "rewritten={rewritten}");
            worst = worst.max(framed);
            total += framed;
        }
        eprintln!(
            "rewritten={rewritten}: open framed {opened_frames}, {} sequence lookups framed {total} \
             (avg {:.1}, worst {worst})",
            truth.len(),
            total as f64 / truth.len() as f64
        );
        assert!(worst < 400, "a sequence lookup framed {worst} records");

        // A walk started at any anchor's record is the scan's suffix.
        let mut k = 1;
        while let Some(anchor) = file.anchor(k)? {
            let at = truth
                .iter()
                .position(|entry| entry.record_offset == anchor.record_start)
                .expect("an anchor names a record start");
            let mut map = file.record_map_from(&mut buffer, anchor.record_start)?;
            for expected in &truth[at..] {
                assert_eq!(&map.advance()?.expect("the suffix continues"), expected);
            }
            assert!(map.advance()?.is_none());
            k += 1;
        }
        assert!(k > 20, "only {k} anchors");

        // Chains walk back across every anchor from the seeded tails.
        assert_eq!(file.block_chain(Tick::ID)?.count(), 3_000);
        assert_eq!(file.block_chain(Blob::ID)?.count(), 3_000);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 40, version = 1, kind = "variable", key = "id")]
struct Account {
    #[varlit(field_id = 1)]
    id: u64,
    #[varlit(field_id = 2)]
    note: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 41, version = 1, kind = "variable")]
struct AccountOp {
    #[varlit(field_id = 1)]
    note: Vec<u8>,
}

impl varlit::VarlitMerge for Account {
    type Op = AccountOp;

    fn apply_op(&mut self, op: Self::Op) -> varlit::Result<()> {
        self.note = op.note;
        Ok(())
    }
}

varlit_format! {
    pub struct AnchoredKeyed {
        magic: b"VANCKEY1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        index: [scan_on_open, block_offset_chain];
        blocks: [Account, AccountOp];
    }
}

fn account(id: u64) -> Account {
    Account {
        id,
        note: vec![(id % 251) as u8; 64],
    }
}

/// A file large enough to open from an anchor keeps no record list, so merge
/// and compact must walk the file instead of reading a list that is not there.
/// These used to fail with `UnexpectedEof` once an input crossed an anchor.
#[test]
fn merge_and_compact_read_inputs_that_open_from_an_anchor() -> varlit::Result<()> {
    use varlit::{compact_keyed_file, compact_keyed_files, merge_keyed_files};
    let spec = AnchoredKeyed::spec().with_anchors(AnchorPolicy::every(13, true));
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("base.varlit");
    let delta = directory.path().join("delta.varlit");
    let merged = directory.path().join("merged.varlit");
    let compacted = directory.path().join("compacted.varlit");
    let direct = directory.path().join("direct.varlit");
    const RECORDS: u64 = 2_000;
    {
        let mut file = spec.create(&base)?;
        for id in 0..RECORDS {
            file.push(&account(id))?;
        }
        file.flush()?;
    }
    {
        let mut file = spec.create(&delta)?;
        for id in 0..RECORDS / 4 {
            file.push_op::<Account>(&id, &AccountOp { note: vec![7; 16] })?;
        }
        for id in RECORDS / 4..RECORDS / 2 {
            file.delete::<Account>(&id)?;
        }
        file.flush()?;
    }
    // The inputs span many 8 KiB anchor spacings.
    assert!(std::fs::metadata(&base)?.len() > 16 * 8192);

    merge_keyed_files::<Account, _>(spec, &base, &[&delta], &merged)?;
    compact_keyed_file::<Account, _>(spec, &merged, &compacted)?;
    compact_keyed_files::<Account, _>(spec, &base, &[&delta], &direct)?;

    let live = (RECORDS - RECORDS / 4) as usize;
    for path in [&merged, &compacted, &direct] {
        let file = spec.open_readonly(path)?;
        let values = file.materialized_keyed_blocks::<Account>()?;
        assert_eq!(values.len(), live);
    }
    Ok(())
}

/// A handle opened from an anchor keeps no record list, so `block_entries_into`
/// answers by reading the file. It must list exactly what a paged walk of every
/// record lists, in the same order.
#[test]
fn block_entries_without_a_record_list_match_a_paged_walk() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("chain.varlit");
    write(&path, 3_000)?;
    let file = spec().open_readonly(&path)?;
    let mut page = vec![varlit::RecordIndexEntry::default(); 512];
    for id in [20u32, 21] {
        let mut walked = Vec::new();
        let mut at = file.index_entries(&mut page)?;
        loop {
            walked.extend(
                page[..at.filled]
                    .iter()
                    .filter(|e| e.block_id == id)
                    .cloned(),
            );
            let Some(next) = at.next else { break };
            at = file.index_entries_from(next, &mut page)?;
        }
        let mut listed = Vec::new();
        if id == 20 {
            file.block_entries_into::<Tick>(&mut listed)?;
        } else {
            file.block_entries_into::<Blob>(&mut listed)?;
        }
        assert_eq!(listed.len(), 3_000);
        assert_eq!(listed, walked, "block {id}");
    }
    Ok(())
}
