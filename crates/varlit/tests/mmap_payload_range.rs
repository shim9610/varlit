//! `mmap_payload_range`: mapping only a range of records.
//!
//! The range mapping must answer exactly what a whole-file mapping answers for
//! the records it covers, refuse every record outside it, take address space
//! and limits only for its own extent, and let a following reader map just
//! what was appended since its previous mapping.
#![cfg(feature = "mmap")]

use varlit::{
    Error, ReadLimit, RecordIndexEntry, VarlitBlock, VarlitEmpty, VarlitFile, varlit_format,
};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 710, version = 1, kind = "variable")]
struct Doc {
    #[varlit(field_id = 1)]
    id: u32,
    #[varlit(field_id = 2)]
    body: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 711, version = 1, kind = "fixed")]
struct Tick {
    t: u64,
    x: [u16; 32],
}

varlit_format! {
    pub struct RangeFormat {
        magic: b"MMRANGE1";
        version: 1;
        endian: little;
        blocks: [Doc, Tick];
    }
}

#[cfg(feature = "integrity")]
varlit_format! {
    pub struct CheckedRangeFormat {
        magic: b"MMRANGE2";
        version: 1;
        endian: little;
        integrity: crc32_with_header;
        blocks: [Doc, Tick];
    }
}

fn doc(i: u32) -> Doc {
    Doc {
        id: i,
        body: (0..(500 + i * 37)).map(|v| (v + i) as u8).collect(),
    }
}

fn tick(i: u64) -> Tick {
    Tick {
        t: i,
        x: std::array::from_fn(|j| (i as u16).wrapping_mul(7).wrapping_add(j as u16)),
    }
}

/// 30 `Doc`s interleaved with 30 `Tick`s: 60 records.
fn write(file: &mut VarlitFile, from: u32, to: u32) -> varlit::Result<()> {
    for i in from..to {
        file.push(&doc(i))?;
        file.push(&tick(u64::from(i)))?;
    }
    file.flush()
}

fn all_entries(file: &VarlitFile) -> varlit::Result<Vec<RecordIndexEntry>> {
    let mut entries = Vec::new();
    file.index_entries_into(&mut entries)?;
    Ok(entries)
}

/// Every record of `range` decodes through the range mapping exactly as
/// through the whole mapping and the copying read; every record outside it is
/// refused.
fn check_range(file: &VarlitFile, range: std::ops::Range<usize>) -> varlit::Result<()> {
    let entries = all_entries(file)?;
    // SAFETY: the test owns the file and does not modify it while mapped.
    let (whole, part) = unsafe {
        (
            file.mmap_payloads()?,
            file.mmap_payload_range(range.clone())?,
        )
    };
    assert_eq!(part.index_entries(), &entries[range.clone()]);
    assert_eq!(part.len(), range.len());
    let mut scratch = Vec::new();
    let (mut a, mut b, mut c) = (Doc::empty(), Doc::empty(), Doc::empty());
    let (mut ta, mut tb) = (Tick::empty(), Tick::empty());
    for (at, entry) in entries.iter().enumerate() {
        if !range.contains(&at) {
            assert!(
                matches!(
                    part.payload_window(entry),
                    Err(Error::MmapEntryNotInSnapshot)
                ),
                "record {at} outside {range:?}"
            );
            continue;
        }
        assert_eq!(part.payload_window(entry)?, whole.payload_window(entry)?);
        if entry.block_id == Doc::ID {
            part.decode_block_into(entry, &mut scratch, &mut a)?;
            whole.decode_block_into(entry, &mut scratch, &mut b)?;
            file.decode_block_into(entry, &mut scratch, &mut c)?;
            assert_eq!(a, b);
            assert_eq!(a, c);
        } else {
            part.decode_block_into(entry, &mut scratch, &mut ta)?;
            file.decode_block_into(entry, &mut scratch, &mut tb)?;
            assert_eq!(ta, tb);
        }
    }
    // Block positions count within the range.
    let ticks: Vec<_> = entries[range.clone()]
        .iter()
        .filter(|e| e.block_id == Tick::ID)
        .collect();
    for (i, entry) in ticks.iter().enumerate() {
        assert_eq!(
            part.block_payload_window::<Tick>(i)?,
            Some(whole.payload_window(entry)?)
        );
    }
    assert_eq!(part.block_payload_window::<Tick>(ticks.len())?, None);
    Ok(())
}

fn ranges(n: usize) -> Vec<std::ops::Range<usize>> {
    vec![0..n, 0..1, n - 1..n, 7..23, 1..n - 1, 30..31, 12..12]
}

#[test]
fn a_range_mapping_answers_exactly_what_the_whole_mapping_answers() -> varlit::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("range.varlit");
    let mut file = RangeFormat::create(&path)?;
    write(&mut file, 0, 30)?;
    drop(file);
    let file = RangeFormat::open_readonly(&path)?;
    assert_eq!(file.record_count()?, 60);
    for range in ranges(60) {
        check_range(&file, range)?;
    }
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn a_range_mapping_verifies_record_headers_and_footers_it_maps() -> varlit::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checked.varlit");
    let mut file = CheckedRangeFormat::create(&path)?;
    write(&mut file, 0, 30)?;
    drop(file);
    let file = CheckedRangeFormat::open_readonly(&path)?;
    for range in ranges(60) {
        check_range(&file, range)?;
    }
    Ok(())
}

#[test]
fn an_empty_range_maps_nothing_and_a_range_past_the_end_is_refused() -> varlit::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bounds.varlit");
    let mut file = RangeFormat::create(&path)?;
    write(&mut file, 0, 3)?;
    drop(file);
    let file = RangeFormat::open_readonly(&path)?;
    let entries = all_entries(&file)?;
    // SAFETY: the file is not modified while mapped.
    unsafe {
        let empty = file.mmap_payload_range(6..6)?;
        assert!(empty.is_empty());
        assert!(matches!(
            empty.payload_window(&entries[0]),
            Err(Error::MmapEntryNotInSnapshot)
        ));
        assert_eq!(empty.block_payload_window::<Doc>(0)?, None);
        assert!(matches!(
            file.mmap_payload_range(0..7),
            Err(Error::MmapRecordRangeOutOfBounds {
                start: 0,
                end: 7,
                records: 6
            })
        ));
        #[allow(clippy::reversed_empty_ranges)]
        let reversed = file.mmap_payload_range(4..2);
        assert!(matches!(
            reversed,
            Err(Error::MmapRecordRangeOutOfBounds { .. })
        ));
    }
    Ok(())
}

/// The mapping ceiling is charged for the range's extent: a file whose whole
/// append log exceeds it is still readable through ranges that fit, which is
/// how a file larger than the address space is read through mappings.
#[test]
fn the_mapping_ceiling_applies_to_the_range_not_the_file() -> varlit::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ceiling.varlit");
    let mut file = RangeFormat::create(&path)?;
    write(&mut file, 0, 30)?;
    drop(file);
    let entries = {
        let file = RangeFormat::open_readonly(&path)?;
        all_entries(&file)?
    };
    let extent = |r: std::ops::Range<usize>| {
        let last = &entries[r.end - 1];
        last.checked_physical_end().unwrap() - entries[r.start].record_offset
    };
    let window = 0..10;
    let ceiling = extent(window.clone());
    let spec = RangeFormat::spec();
    let mut limits = spec.read_limits;
    limits.max_mmap_len = ReadLimit::Finite(ceiling);
    let file = VarlitFile::open_readonly(spec.with_read_limits(limits), &path)?;
    // SAFETY: the file is not modified while mapped.
    unsafe {
        assert!(matches!(
            file.mmap_payloads(),
            Err(Error::LimitExceeded { .. })
        ));
        let mut start = 0;
        let mut read = 0;
        let (mut scratch, mut out) = (Vec::new(), Doc::empty());
        while start < entries.len() {
            let mut end = start + 1;
            while end < entries.len() && extent(start..end + 1) <= ceiling {
                end += 1;
            }
            let part = file.mmap_payload_range(start..end)?;
            for entry in part.index_entries() {
                if entry.block_id == Doc::ID {
                    part.decode_block_into(entry, &mut scratch, &mut out)?;
                    assert_eq!(out, doc(out.id));
                }
                read += 1;
            }
            start = end;
        }
        assert_eq!(read, entries.len());
        // One record past what fits is refused before mapping.
        assert!(matches!(
            file.mmap_payload_range(0..window.end + 1),
            Err(Error::LimitExceeded { .. })
        ));
    }
    Ok(())
}

/// A following reader maps only what each publication appended.
#[test]
fn a_following_reader_maps_only_the_appended_records() -> varlit::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("follow.varlit");
    let mut writer = RangeFormat::create(&path)?;
    write(&mut writer, 0, 5)?;
    let mut reader = RangeFormat::open_readonly(&path)?;
    let mut seen = 0;
    let mut ids = Vec::new();
    let (mut scratch, mut out) = (Vec::new(), Doc::empty());
    for round in 1..=4u32 {
        let now = reader.record_count()?;
        // SAFETY: records already published are never rewritten; the writer
        // only appends after them.
        let fresh = unsafe { reader.mmap_payload_range(seen..now)? };
        assert_eq!(fresh.len(), now - seen);
        for entry in fresh.index_entries() {
            if entry.block_id == Doc::ID {
                fresh.decode_block_into(entry, &mut scratch, &mut out)?;
                ids.push(out.id);
            }
        }
        seen = now;
        write(&mut writer, round * 5, round * 5 + 5)?;
        reader.follow()?;
    }
    assert_eq!(ids, (0..20).collect::<Vec<_>>());
    Ok(())
}
