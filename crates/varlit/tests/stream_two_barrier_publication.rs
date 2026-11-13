//! Stream publication across its barriers.
//!
//! A sync is one barrier on the primary, closed by a sync marker; the state
//! sidecar is confirmed with two more — its pages, then its head — only every
//! so often (`DiskIndexConfirmPolicy`). These rebuild on disk the states a crash
//! can leave and require readers to see every synced record and the writer to
//! recover without losing one.

use std::fs;
use std::path::Path;

use varlit::{Error, StreamOptions, StreamingBlocks, VarlitBlock, VarlitEmpty, varlit_format};

varlit_format! {
    pub format Feed {
        magic: b"FEED2B";
        version: 1;
        schema_hash: computed;
        index: block_offset_chain;
        blocks { fixed Sample(id = 1) { value: u64 } }
    }
}

/// The stream state sidecar sits beside the primary with a `.vks` suffix.
fn sidecar_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".vks");
    name.into()
}

const CONFIRMED: [usize; 2] = [4096, 8192];
const DIRTY: usize = 20480;
/// Heads written without a barrier of their own; a crash may lose any of them.
const UNSYNCED_HEADS: [usize; 3] = [12288, 16384, DIRTY];

fn lose_unsynced_heads(image: &mut [u8]) {
    for slot in UNSYNCED_HEADS {
        image[slot..slot + 64].fill(0);
    }
}

fn values(blocks: &mut StreamingBlocks<Sample>) -> varlit::Result<Vec<u64>> {
    let (mut out, mut all) = (Sample::empty(), Vec::new());
    while blocks.next_into(&mut out)? {
        all.push(out.value);
    }
    Ok(all)
}

fn read_all(path: &Path) -> varlit::Result<Vec<u64>> {
    let reader = Feed::open_stream_reader(path, StreamOptions::default())?;
    values(&mut reader.samples()?)
}

/// Confirms the state sidecar on every sync, so each sync is a generation.
fn confirming() -> StreamOptions {
    StreamOptions {
        state: varlit::DiskIndexOptions {
            confirm: varlit::DiskIndexConfirmPolicy::EVERY_SYNC,
            ..varlit::DiskIndexOptions::default()
        },
        ..StreamOptions::default()
    }
}

/// The newest confirmed head's record offset.
fn newest_record(image: &[u8]) -> usize {
    CONFIRMED
        .into_iter()
        .filter(|slot| &image[*slot..*slot + 8] == b"VIXHEAD3")
        .max_by_key(|slot| u64::from_le_bytes(image[slot + 24..slot + 32].try_into().unwrap()))
        .map(|slot| u64::from_le_bytes(image[slot + 8..slot + 16].try_into().unwrap()) as usize)
        .unwrap()
}

/// Generation 1 holds 1..=3 and is fully durable; generation 2 adds 4..=6.
/// Returns the sidecar image after each.
fn two_generations(path: &Path) -> varlit::Result<(Vec<u8>, Vec<u8>)> {
    let sidecar = sidecar_path(path);
    let mut writer = Feed::create_stream_writer(path, confirming())?;
    for value in 1..=3 {
        writer.push_sample(&Sample { value })?;
    }
    writer.sync()?;
    let first = fs::read(&sidecar)?;
    for value in 4..=6 {
        writer.push_sample(&Sample { value })?;
    }
    writer.sync()?;
    drop(writer);
    Ok((first, fs::read(&sidecar)?))
}

#[test]
fn a_completed_publication_keeps_two_head_copies() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("copies.varlit");
    let (_, second) = two_generations(&path)?;
    assert_eq!(
        second[CONFIRMED[0]..CONFIRMED[0] + 64],
        second[CONFIRMED[1]..CONFIRMED[1] + 64]
    );
    assert_eq!(&second[CONFIRMED[0]..CONFIRMED[0] + 8], b"VIXHEAD3");
    assert_eq!(read_all(&path)?, vec![1, 2, 3, 4, 5, 6]);
    Ok(())
}

/// Generation 2's confirmation never reached storage — neither head copy,
/// and none of the unsynced heads — but its sync did: the primary carries the
/// records and the marker that closed them. Readers see them from the
/// primary; the writer recovers them instead of cutting them away.
#[test]
fn a_lost_confirmation_loses_no_synced_record() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("lost-confirmation.varlit");
    let sidecar = sidecar_path(&path);
    let (first, second) = two_generations(&path)?;
    let mut image = second.clone();
    for slot in CONFIRMED {
        image[slot..slot + 64].copy_from_slice(&first[slot..slot + 64]);
    }
    lose_unsynced_heads(&mut image);
    fs::write(&sidecar, &image)?;

    assert_eq!(read_all(&path)?, vec![1, 2, 3, 4, 5, 6], "reader");
    let mut writer = Feed::open_stream_writer(&path, confirming())?;
    writer.push_sample(&Sample { value: 7 })?;
    writer.sync()?;
    drop(writer);
    assert_eq!(
        read_all(&path)?,
        (1..=7).collect::<Vec<_>>(),
        "after recovery"
    );
    Ok(())
}

/// The writer crashed mid-generation and its dirty checkpoint never reached
/// storage: the newest durable state is clean while the primary is longer.
/// Only the unsynced record is lost.
#[test]
fn a_generation_whose_dirty_checkpoint_was_lost_is_recovered() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("lost-dirty.varlit");
    let sidecar = sidecar_path(&path);
    {
        let mut writer = Feed::create_stream_writer(&path, confirming())?;
        writer.push_sample(&Sample { value: 1 })?;
        writer.sync()?;
        writer.push_sample(&Sample { value: 2 })?;
        writer.flush()?;
        // Dropped without `sync`: the record is never acknowledged.
    }
    let mut image = fs::read(&sidecar)?;
    lose_unsynced_heads(&mut image);
    fs::write(&sidecar, &image)?;

    assert_eq!(read_all(&path)?, vec![1]);
    let mut writer = Feed::open_stream_writer(&path, confirming())?;
    writer.push_sample(&Sample { value: 3 })?;
    writer.sync()?;
    drop(writer);
    assert_eq!(read_all(&path)?, vec![1, 3]);
    Ok(())
}

/// A generation whose barriers completed is durable data: damage to its
/// record is reported, never answered by silently rolling back to the previous
/// generation.
#[test]
fn damage_to_a_completed_generation_is_an_error_not_a_rollback() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("damaged.varlit");
    let sidecar = sidecar_path(&path);
    let (_, second) = two_generations(&path)?;
    let mut image = second.clone();
    let record = newest_record(&image);
    image[record + 40] ^= 0x10;
    fs::write(&sidecar, &image)?;
    assert!(read_all(&path).is_err());
    assert!(Feed::open_stream_writer(&path, confirming()).is_err());
    let _ = Sample::ID;
    let _ = Error::UnexpectedEof;
    Ok(())
}
