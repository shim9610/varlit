//! Stream publication with two storage barriers: the primary, then one sidecar
//! sync that makes the body and a single confirmed head durable together.
//!
//! These rebuild on disk the states a crash can leave now that neither the
//! dirty checkpoint nor the confirmed body is synced on its own, and require
//! readers to see the last durable generation and the writer to recover.

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

/// Generation 1 holds 1..=3 and is fully durable; generation 2 adds 4..=6.
/// Returns the sidecar image after each.
fn two_generations(path: &Path) -> varlit::Result<(Vec<u8>, Vec<u8>)> {
    let sidecar = sidecar_path(path);
    let mut writer = Feed::create_stream_writer(path, StreamOptions::default())?;
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
    assert_eq!(&second[CONFIRMED[0]..CONFIRMED[0] + 8], b"VIXHEAD2");
    assert_eq!(read_all(&path)?, vec![1, 2, 3, 4, 5, 6]);
    Ok(())
}

/// The new head reached storage, its copy did not, and part of the
/// generation's sidecar bytes never arrived: the generation is not adopted, and
/// the writer restores to the last durable one.
#[test]
fn a_head_whose_generation_did_not_arrive_is_not_adopted() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    for damage in ["truncate", "flip"] {
        let path = dir.path().join(format!("torn-{damage}.varlit"));
        let sidecar = sidecar_path(&path);
        let (first, second) = two_generations(&path)?;
        let mut image = second.clone();
        // One slot keeps generation 1's head: the copy never reached storage.
        let kept = CONFIRMED[1];
        image[kept..kept + 64].copy_from_slice(&first[kept..kept + 64]);
        lose_unsynced_heads(&mut image);
        match damage {
            "truncate" => image.truncate(second.len() - 100),
            _ => {
                let at = first.len() + (second.len() - first.len()) / 2;
                image[at] ^= 0x10;
            }
        }
        fs::write(&sidecar, &image)?;

        assert_eq!(read_all(&path)?, vec![1, 2, 3], "{damage}: reader");
        // The primary still holds generation 2's records past generation 1's
        // committed end: an interrupted generation, restored rather than opened.
        assert!(matches!(
            Feed::open_stream_writer(&path, StreamOptions::default()),
            Err(Error::DiskIndex(_))
        ));
        let mut writer = Feed::restore_stream_writer(&path, StreamOptions::default())?;
        writer.push_sample(&Sample { value: 7 })?;
        writer.sync()?;
        drop(writer);
        assert_eq!(
            read_all(&path)?,
            vec![1, 2, 3, 7],
            "{damage}: after restore"
        );
    }
    Ok(())
}

/// The writer crashed mid-generation and its dirty checkpoint never reached
/// storage: the newest durable state is clean while the primary is longer.
#[test]
fn a_generation_whose_dirty_checkpoint_was_lost_is_restorable() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("lost-dirty.varlit");
    let sidecar = sidecar_path(&path);
    {
        let mut writer = Feed::create_stream_writer(&path, StreamOptions::default())?;
        writer.push_sample(&Sample { value: 1 })?;
        writer.sync()?;
        writer.push_sample(&Sample { value: 2 })?;
        writer.flush()?;
        // Dropped without `sync`: generation 2 is never published.
    }
    let mut image = fs::read(&sidecar)?;
    lose_unsynced_heads(&mut image);
    fs::write(&sidecar, &image)?;

    assert_eq!(read_all(&path)?, vec![1]);
    assert!(Feed::open_stream_writer(&path, StreamOptions::default()).is_err());
    let mut writer = Feed::restore_stream_writer(&path, StreamOptions::default())?;
    writer.push_sample(&Sample { value: 3 })?;
    writer.sync()?;
    drop(writer);
    assert_eq!(read_all(&path)?, vec![1, 3]);
    Ok(())
}

/// A generation whose barrier completed is durable data: damage to it is
/// reported, never answered by silently rolling back to the previous one.
#[test]
fn damage_to_a_completed_generation_is_an_error_not_a_rollback() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("damaged.varlit");
    let sidecar = sidecar_path(&path);
    let (first, second) = two_generations(&path)?;
    let mut image = second.clone();
    let at = first.len() + (second.len() - first.len()) / 2;
    image[at] ^= 0x10;
    fs::write(&sidecar, &image)?;
    assert!(read_all(&path).is_err());
    let _ = Sample::ID;
    Ok(())
}
