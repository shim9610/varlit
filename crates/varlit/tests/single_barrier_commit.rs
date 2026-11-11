//! `transaction_marker(_, single_barrier)`: one storage barrier per durable
//! commit.
//!
//! Without the barrier between a transaction's records and its marker, storage
//! may persist the marker and lose a page of the records before it. These tests
//! reproduce exactly that on disk — the marker intact, a record of its
//! transaction damaged — and require every open route to end the file at the
//! newest transaction known durable instead of exposing the damaged one, and a
//! writer open to cut it off.
#![cfg(feature = "integrity")]

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use varlit::{Error, FormatSpec, IntegrityPolicy, VarlitBlock, VarlitFile, varlit_format};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable")]
struct Sample {
    #[varlit(field_id = 1)]
    tag: u64,
    #[varlit(field_id = 2)]
    body: Vec<u8>,
}

varlit_format! {
    struct Single {
        magic: b"SBARR001";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        recovery: truncate_tail;
        index: [scan_on_open, block_offset_chain];
        commit: transaction_marker(explicit, single_barrier);
        blocks: [Sample];
    }
}

varlit_format! {
    struct SingleTails {
        magic: b"SBARR002";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        recovery: truncate_tail;
        index: [scan_on_open, block_offset_chain, header_tails];
        commit: transaction_marker(explicit, single_barrier);
        blocks: [Sample];
    }
}

fn sample(tag: u64) -> Sample {
    Sample {
        tag,
        body: vec![tag as u8; 3000],
    }
}

/// Every committed sample's tag, oldest first, through the block chain (which
/// every open route serves, including the lazy ones that keep no directory).
fn tags(file: &VarlitFile) -> varlit::Result<Vec<u64>> {
    let (mut scratch, mut out, mut tags) = (
        Vec::new(),
        Sample {
            tag: 0,
            body: Vec::new(),
        },
        Vec::new(),
    );
    for entry in file.block_chain(Sample::ID)? {
        file.decode_block_into(&entry?, &mut scratch, &mut out)?;
        tags.push(out.tag);
    }
    tags.reverse();
    Ok(tags)
}

/// Writes transaction A (tags 0..3) durably, then transaction B (10..13) by
/// `commit_b`, and returns the record offsets of B's samples.
fn write_two(
    spec: FormatSpec,
    path: &Path,
    commit_b: impl FnOnce(&mut VarlitFile) -> varlit::Result<()>,
) -> varlit::Result<Vec<u64>> {
    let mut file = VarlitFile::create(spec, path)?;
    for tag in 0..3 {
        file.push(&sample(tag))?;
    }
    file.commit_durable()?;
    let mut b = Vec::new();
    for tag in 10..13 {
        b.push(file.push_info(&sample(tag))?.payload_offset);
    }
    commit_b(&mut file)?;
    drop(file);
    Ok(b)
}

/// Overwrites bytes inside a record's payload, as a lost page would.
fn tear(path: &Path, payload_offset: u64) {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(payload_offset + 100)).unwrap();
    let mut b = [0u8; 64];
    f.read_exact(&mut b).unwrap();
    for x in &mut b {
        *x = !*x;
    }
    f.seek(SeekFrom::Start(payload_offset + 100)).unwrap();
    f.write_all(&b).unwrap();
}

#[test]
fn an_intact_single_barrier_file_reads_every_commit() -> varlit::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    for spec in [Single::spec(), SingleTails::spec()] {
        let path = dir.path().join(format!("intact-{:?}", spec.magic));
        write_two(spec, &path, |f| f.commit_durable().map(|_| ()))?;
        let file = VarlitFile::open_readonly(spec, &path)?;
        assert_eq!(tags(&file)?, vec![0, 1, 2, 10, 11, 12]);
        let lazy = VarlitFile::open_readonly_lazy_or_scan(spec, &path)?;
        assert_eq!(tags(&lazy)?, vec![0, 1, 2, 10, 11, 12]);
        // The marker names what it commits.
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.windows(4).any(|w| w == b"VCM2"));
    }
    Ok(())
}

#[test]
fn a_torn_newest_transaction_is_not_committed_on_any_open_route() -> varlit::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    for spec in [Single::spec(), SingleTails::spec()] {
        let path = dir.path().join(format!("torn-{:?}", spec.magic));
        let b = write_two(spec, &path, |f| f.commit_durable().map(|_| ()))?;
        tear(&path, b[1]);
        let len_before = std::fs::metadata(&path).unwrap().len();

        let file = VarlitFile::open_readonly(spec, &path)?;
        assert_eq!(tags(&file)?, vec![0, 1, 2], "scan open");
        drop(file);
        let lazy = VarlitFile::open_readonly_lazy_or_scan(spec, &path)?;
        assert_eq!(tags(&lazy)?, vec![0, 1, 2], "lazy-or-scan open");
        drop(lazy);
        // A read-only open never changes the file.
        assert_eq!(std::fs::metadata(&path).unwrap().len(), len_before);

        // A writer open cuts the torn transaction off and carries on.
        let mut writer = VarlitFile::open(spec, &path)?;
        assert_eq!(tags(&writer)?, vec![0, 1, 2], "writer open");
        assert!(std::fs::metadata(&path).unwrap().len() < len_before);
        writer.push(&sample(20))?;
        writer.commit_durable()?;
        drop(writer);
        let file = VarlitFile::open_readonly(spec, &path)?;
        assert_eq!(tags(&file)?, vec![0, 1, 2, 20], "after recovery");
    }
    Ok(())
}

#[test]
fn a_header_tails_summary_does_not_vouch_for_a_torn_transaction() -> varlit::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let spec = SingleTails::spec();
    let path = dir.path().join("strict-lazy");
    let b = write_two(spec, &path, |f| f.commit_durable().map(|_| ()))?;
    // Intact: the strict lazy route serves it from the summary.
    assert_eq!(
        tags(&VarlitFile::open_readonly_lazy(spec, &path)?)?.len(),
        6
    );
    tear(&path, b[0]);
    // Torn: the summary names the newest marker, which no longer verifies.
    assert!(VarlitFile::open_readonly_lazy(spec, &path).is_err());
    Ok(())
}

/// A non-durable `commit()` is not a barrier: a crash during the next
/// `commit_durable` can tear either transaction, so both roll back to the
/// newest marker a completed barrier made durable.
#[test]
fn transactions_since_the_last_completed_barrier_roll_back_together() -> varlit::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let spec = Single::spec();
    let path = dir.path().join("mixed");
    let b = write_two(spec, &path, |f| {
        f.commit()?;
        f.push(&sample(30))?;
        f.commit_durable().map(|_| ())
    })?;
    {
        let file = VarlitFile::open_readonly(spec, &path)?;
        assert_eq!(tags(&file)?, vec![0, 1, 2, 10, 11, 12, 30]);
    }
    tear(&path, b[2]);
    let file = VarlitFile::open_readonly(spec, &path)?;
    assert_eq!(tags(&file)?, vec![0, 1, 2]);
    Ok(())
}

#[test]
fn a_single_barrier_requires_header_and_payload_checksums() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = Single::spec();
    spec.integrity_policy = IntegrityPolicy::Crc32;
    assert!(matches!(
        VarlitFile::create(spec, dir.path().join("weak")),
        Err(Error::InvalidFormatSpec(_))
    ));
}
