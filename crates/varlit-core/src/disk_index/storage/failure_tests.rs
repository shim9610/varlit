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
