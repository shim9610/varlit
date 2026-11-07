//! Explicit lost/torn-write images. This models storage outcomes, not real
//! power loss, and rejects a durable head whose referenced data did not persist.
use super::*;

fn images() -> (Vec<u8>, Vec<u8>, Checkpoint, Checkpoint) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checkpoints");
    let mut store = Store::open(&path, DiskIndexOptions::default(), true, true).unwrap();
    let metadata = DiskIndexMetadata::new_state(
        DiskIndexIdentity {
            schema_hash: 42,
            primary_fingerprint: [0; 32],
        },
        DiskIndexFrontier::empty(64),
        4,
    );
    let old = store
        .publish(metadata, &[], Root::default(), Publication::Confirmed)
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let metadata = DiskIndexMetadata {
        generation: 1,
        committed: DiskIndexFrontier::empty(128),
        working: DiskIndexFrontier::empty(128),
        ..metadata
    };
    let new = store
        .publish(metadata, &[], Root::default(), Publication::Confirmed)
        .unwrap();
    (before, std::fs::read(&path).unwrap(), old, new)
}
fn open_image(bytes: &[u8]) -> DiskIndexResult<Checkpoint> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image");
    std::fs::write(&path, bytes)?;
    Store::open(&path, DiskIndexOptions::default(), false, false)?.latest(true)
}

#[test]
fn torn_publication_heads_never_invent_a_confirmed_generation() {
    let (before, after, old, new) = images();
    assert_eq!(open_image(&before).unwrap().offset, old.offset);
    assert_eq!(open_image(&after).unwrap().offset, new.offset);
    for slot in CONFIRMED.map(|v| v as usize) {
        for cut in 0..=64 {
            let mut image = after.clone();
            for base in CONFIRMED.map(|v| v as usize) {
                image[base..base + 64].copy_from_slice(&before[base..base + 64]);
            }
            image[slot..slot + cut].copy_from_slice(&after[slot..slot + cut]);
            let actual = open_image(&image).unwrap();
            assert!(
                (actual.offset == old.offset && actual.metadata == old.metadata)
                    || (actual.offset == new.offset && actual.metadata == new.metadata),
                "publication combined different generations"
            );
        }
    }
    // A checksummed durable head with missing checkpoint bytes is corruption,
    // even if another older confirmed root would still be readable.
    for cut in old.end..new.end {
        let mut image = after.clone();
        let slot = CONFIRMED[0] as usize;
        image[slot..slot + 64].copy_from_slice(&before[slot..slot + 64]);
        image.truncate(cut as usize);
        assert!(
            open_image(&image).is_err(),
            "accepted missing durable bytes at {cut}"
        );
    }
    let mut image = after.clone();
    let working = WORKING[0] as usize;
    image[working..working + 64]
        .copy_from_slice(&after[CONFIRMED[0] as usize..CONFIRMED[0] as usize + 64]);
    for slot in CONFIRMED.map(|v| v as usize) {
        image[slot..slot + 64].fill(0);
    }
    assert!(
        open_image(&image).is_err(),
        "working head replaced the confirmed anchor"
    );
    println!(
        "torn head prefixes=130; missing durable prefixes={}",
        new.end - old.end
    );
}

#[test]
fn valid_checksums_do_not_admit_inconsistent_checkpoint_frontiers() {
    let (_, after, _, new) = images();
    let mut cases = Vec::new();
    let mut value = new.metadata;
    value.working.eof += 1;
    cases.push(value);
    let mut value = new.metadata;
    value.working.record_count += 1;
    cases.push(value);
    let mut value = new.metadata;
    value.working_tail_count = value.tail_limit + 1;
    cases.push(value);
    for metadata in cases {
        let mut image = after.clone();
        let offset = new.offset as usize;
        image[offset + 32..offset + 332].copy_from_slice(&encode_metadata(metadata));
        let crc = crc32fast::hash(&image[offset..offset + 348]);
        image[offset + 348..offset + 352].copy_from_slice(&crc.to_le_bytes());
        assert!(
            open_image(&image).is_err(),
            "accepted CRC-valid inconsistent metadata {metadata:?}"
        );
    }
}

#[test]
fn independently_torn_heads_never_mix_publication_generations() {
    let (before, after, old, new) = images();
    let mut cases = 0;
    for first_cut in 0..=64 {
        for second_cut in 0..=64 {
            for keep_new_checkpoint in [false, true] {
                let mut image = after.clone();
                for (slot, cut) in CONFIRMED.into_iter().zip([first_cut, second_cut]) {
                    let slot = slot as usize;
                    image[slot..slot + 64].copy_from_slice(&before[slot..slot + 64]);
                    image[slot..slot + cut].copy_from_slice(&after[slot..slot + cut]);
                }
                if !keep_new_checkpoint {
                    image.truncate(old.end as usize);
                }
                // Both anchors may be torn: rejection is permitted, invented
                // or mixed metadata is not. Complete images must be readable.
                match open_image(&image) {
                    Ok(actual) => {
                        assert!(
                            actual.offset == old.offset
                                || (keep_new_checkpoint && actual.offset == new.offset)
                        );
                        let expected = if actual.offset == old.offset {
                            &old
                        } else {
                            &new
                        };
                        assert_eq!(actual.metadata, expected.metadata);
                        assert_eq!(actual.end, expected.end);
                    }
                    Err(_) => assert!(
                        !(first_cut == 0 && second_cut == 0
                            || keep_new_checkpoint && first_cut == 64 && second_cut == 64)
                    ),
                }
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 8450);
    println!("PUBLICATION_IMAGES independent head prefixes=8450");
}

#[test]
fn divergent_clean_heads_require_sync_before_writer_resume() {
    let (before, mut after, _, new) = images();
    let slot = CONFIRMED[1] as usize;
    after[slot..slot + 64].copy_from_slice(&before[slot..slot + 64]);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("uncertain-clean-head");
    std::fs::write(&path, &after).unwrap();
    let store = Store::open(&path, DiskIndexOptions::default(), false, true).unwrap();
    assert_eq!(store.confirmed().unwrap().offset, new.offset);
    RECONCILE_SYNC_FAILURE.with(|flag| flag.set(true));
    assert!(store.reconcile_confirmed_heads().is_err());
    assert!(!RECONCILE_SYNC_FAILURE.with(|flag| flag.get()));
    store.reconcile_confirmed_heads().unwrap();
    assert_eq!(store.confirmed().unwrap().offset, new.offset);
}

#[test]
fn matching_clean_heads_do_not_add_a_resume_sync() {
    let (_, after, _, _) = images();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("matching-clean-heads");
    std::fs::write(&path, after).unwrap();
    let store = Store::open(&path, DiskIndexOptions::default(), false, true).unwrap();
    RECONCILE_SYNC_FAILURE.with(|flag| flag.set(true));
    store.reconcile_confirmed_heads().unwrap();
    assert!(
        RECONCILE_SYNC_FAILURE.with(|flag| flag.replace(false)),
        "normal resume issued an unnecessary sync"
    );
}

// Pure parsing counterpart to the native torn-head filesystem sweeps. This
// exercises the production parser; it does not simulate fsync or durability.
#[test]
fn head_parser_rejects_torn_and_hostile_words() {
    fn encoded(offset: u64, end: u64) -> [u8; 64] {
        let mut bytes = [0; 64];
        bytes[..8].copy_from_slice(HEAD_MAGIC);
        bytes[8..16].copy_from_slice(&offset.to_le_bytes());
        bytes[16..24].copy_from_slice(&end.to_le_bytes());
        let crc = crc32fast::hash(&bytes[..28]);
        bytes[28..32].copy_from_slice(&crc.to_le_bytes());
        bytes
    }
    let old = encoded(FILE_HEADER as u64, FILE_HEADER as u64 + ROOT_HEADER as u64);
    let new = encoded(0x12345678, 0x12345678 + ROOT_HEADER as u64);
    for (before, after) in [(old, new), (new, old)] {
        for cut in 0..=64 {
            let mut torn = before;
            torn[..cut].copy_from_slice(&after[..cut]);
            if let Some(head) = parse_head(&torn) {
                assert!(
                    (head.offset == read_u64(&old, 8) && head.end == read_u64(&old, 16))
                        || (head.offset == read_u64(&new, 8) && head.end == read_u64(&new, 16))
                );
            }
        }
    }
    for (offset, end) in [
        (0, 352),
        (24575, 24927),
        (24576, 24576),
        (24576, 24927),
        (u64::MAX, 0),
        (u64::MAX - 351, u64::MAX),
    ] {
        assert!(parse_head(&encoded(offset, end)).is_none());
    }
    for byte in 24..64 {
        let mut damaged = old;
        damaged[byte] ^= 1;
        assert!(parse_head(&damaged).is_none());
    }
    assert!(parse_head(&old).is_some());
    assert!(parse_head(&new).is_some());
}

#[test]
fn failed_resume_releases_writer_lock_while_snapshot_owners_survive() {
    let (before, mut after, _, checkpoint) = images();
    let slot = CONFIRMED[1] as usize;
    after[slot..slot + 64].copy_from_slice(&before[slot..slot + 64]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owner-lifetime");
    std::fs::write(&path, after).unwrap();
    let store = Store::open(&path, DiskIndexOptions::default(), false, true).unwrap();
    let file = store.file.clone();
    let readers: Vec<_> = (0..8).map(|_| store.reader(&checkpoint)).collect();
    let count = Arc::strong_count(&file);
    assert!(count > 8);
    RECONCILE_SYNC_FAILURE.with(|flag| flag.set(true));
    assert!(store.reconcile_confirmed_heads().is_err());
    assert_eq!(Arc::strong_count(&file), count);
    drop(store);
    let replacement = Store::open(&path, DiskIndexOptions::default(), false, true).unwrap();
    replacement.reconcile_confirmed_heads().unwrap();
    for reader in &readers {
        assert!(reader.get(checkpoint.root, b"absent").unwrap().is_none());
    }
    drop(replacement);
    drop(readers);
    assert_eq!(
        Arc::strong_count(&file),
        1,
        "snapshot retained a file owner"
    );
    // Consuming the final library owner transfers the actual File, without
    // leaving an Arc allocation or a hidden snapshot reference behind.
    drop(Arc::try_unwrap(file).expect("last file owner"));
}
