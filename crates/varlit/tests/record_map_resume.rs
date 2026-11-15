//! `record_map_from`: a forward reader continues after `follow()` from where
//! its last walk stopped.
//!
//! Without it a forward reader had two choices after a `follow()`: open a new
//! map and re-frame the whole file to reach the new records, or walk each
//! block's offset chain newest-first back to what it had seen and buffer and
//! reverse that run. The first reads the file again on every poll; the second
//! holds every record of a backlog in memory before the first one can be
//! handed on in order. These pin that a resumed walk frames only what was
//! appended, in file order, into a buffer that holds only that, and that an
//! offset this handle cannot vouch for is refused rather than walked.

#![cfg(feature = "integrity")]

use varlit::{VarlitBlock, VarlitEmpty, VarlitFile, varlit_format};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 10, version = 1, kind = "fixed")]
struct Reading {
    value: u64,
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 11, version = 1, kind = "variable")]
struct Label {
    #[varlit(field_id = 1)]
    text: String,
}

varlit_format! {
    pub struct ResumeFormat {
        magic: b"VRESUME1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        index: header_tails;
        commit: transaction_marker(on_flush);
        blocks: [Reading, Label];
    }
}

fn values(file: &VarlitFile, from: u64, out: &mut Vec<u64>) -> varlit::Result<u64> {
    let mut buffer = Vec::new();
    let mut map = file.record_map_from(&mut buffer, from)?;
    let mut scratch = Vec::new();
    let mut reading = Reading::empty();
    while let Some(entry) = map.advance()? {
        if entry.block_id == Reading::ID {
            file.decode_block_into(&entry, &mut scratch, &mut reading)?;
            out.push(reading.value);
        }
    }
    Ok(map.resume_offset())
}

fn start(file: &VarlitFile) -> varlit::Result<u64> {
    let mut buffer = Vec::new();
    Ok(file.record_map(&mut buffer)?.resume_offset())
}

/// Where a complete walk of this handle's snapshot ends.
fn end(file: &VarlitFile) -> varlit::Result<u64> {
    let mut buffer = Vec::new();
    let mut map = file.record_map(&mut buffer)?;
    map.fill()?;
    Ok(map.resume_offset())
}

#[test]
fn a_resumed_walk_frames_only_what_follow_added_in_file_order() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("resume.varlit");
    let mut writer = ResumeFormat::spec().create(&path)?;
    for value in 0..50 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;

    let mut reader = VarlitFile::open_readonly_lazy(ResumeFormat::spec(), &path)?;
    let mut seen = Vec::new();
    let at = values(&reader, start(&reader)?, &mut seen)?;
    assert_eq!(seen, (0..50).collect::<Vec<_>>());

    for value in 50..80 {
        writer.push(&Reading { value })?;
        if value % 7 == 0 {
            writer.push(&Label {
                text: format!("l{value}"),
            })?;
        }
    }
    writer.flush()?;
    // Not visible before follow: the walk is bounded by the handle's snapshot.
    let mut none = Vec::new();
    assert_eq!(values(&reader, at, &mut none)?, at);
    assert!(none.is_empty());

    assert!(reader.follow()? > 0);
    let mut buffer = Vec::new();
    let mut map = reader.record_map_from(&mut buffer, at)?;
    map.fill()?;
    let framed = map.len();
    drop(map);
    assert_eq!(framed, 30 + 5, "only the records appended since `at`");

    let mut more = Vec::new();
    let stop = values(&reader, at, &mut more)?;
    assert_eq!(more, (50..80).collect::<Vec<_>>());
    seen.extend(more);
    assert_eq!(seen, (0..80).collect::<Vec<_>>());
    assert_eq!(stop, end(&reader)?);
    Ok(())
}

#[test]
fn an_offset_outside_the_snapshot_is_refused() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("outside.varlit");
    let mut writer = ResumeFormat::spec().create(&path)?;
    writer.push(&Reading { value: 1 })?;
    writer.flush()?;
    let reader = VarlitFile::open_readonly_lazy(ResumeFormat::spec(), &path)?;
    let begin = start(&reader)?;
    let mut buffer = Vec::new();
    assert!(reader.record_map_from(&mut buffer, begin - 1).is_err());
    assert!(
        reader
            .record_map_from(&mut buffer, end(&reader)? + 1)
            .is_err()
    );
    // The end itself is a boundary: an empty walk, not an error.
    assert!(
        reader
            .record_map_from(&mut buffer, end(&reader)?)?
            .advance()?
            .is_none()
    );
    Ok(())
}

#[test]
fn an_offset_inside_a_record_does_not_frame() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("inside.varlit");
    let mut writer = ResumeFormat::spec().create(&path)?;
    for value in 0..4 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    let reader = VarlitFile::open_readonly_lazy(ResumeFormat::spec(), &path)?;
    let mut buffer = Vec::new();
    let mut map = reader.record_map_from(&mut buffer, start(&reader)? + 3)?;
    assert!(
        map.advance().is_err(),
        "a mid-record offset must not read as a record"
    );
    Ok(())
}
