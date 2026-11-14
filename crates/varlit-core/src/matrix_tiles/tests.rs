use super::geometry::{BlockGeom, Geometry};
use super::store::memory::MemStore;
use super::{TileReader, TileWriter};
use crate::{Error, MatrixKey};

const BLOCK: BlockGeom = BlockGeom {
    block_id: 7,
    stride: 8,
    tile: [2, 4],
};

fn geometry(growing: bool) -> Geometry {
    Geometry {
        dims: [16, 16],
        growing,
        blocks: vec![BLOCK],
        extra_len: 0,
    }
}

fn slot(value: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[..8].copy_from_slice(&value.to_le_bytes());
    let crc = crc32fast::hash(&out[..8]);
    out[8..].copy_from_slice(&crc.to_le_bytes());
    out
}

fn put(w: &mut TileWriter, store: &mut MemStore, key: MatrixKey, value: u64) {
    let (coord, index) = BLOCK.locate(key);
    let tile = w.tile(store, 0, coord, true).unwrap().unwrap();
    w.stage(store, tile + BLOCK.slot_at(index), &slot(value))
        .unwrap();
}

/// The value a writer sees for `key` (`None`: tile absent or never written).
fn mine(w: &mut TileWriter, store: &mut MemStore, key: MatrixKey) -> Option<u64> {
    let (coord, index) = BLOCK.locate(key);
    let tile = w.tile(store, 0, coord, false).unwrap()?;
    let mut bytes = [0u8; 12];
    w.read(store, tile + BLOCK.slot_at(index), &mut bytes)
        .unwrap();
    (bytes != [0; 12]).then(|| u64::from_le_bytes(bytes[..8].try_into().unwrap()))
}

fn seen(r: &mut TileReader, store: &MemStore, key: MatrixKey) -> crate::Result<Option<u64>> {
    let (coord, index) = BLOCK.locate(key);
    let Some(tile) = r.tile(store, 0, coord)? else {
        return Ok(None);
    };
    let mut bytes = [0u8; 12];
    r.read(store, tile + BLOCK.slot_at(index), &mut bytes)?;
    if bytes == [0; 12] {
        return Ok(None);
    }
    assert_eq!(
        crc32fast::hash(&bytes[..8]),
        u32::from_le_bytes(bytes[8..].try_into().unwrap()),
        "a torn slot reached a reader"
    );
    Ok(Some(u64::from_le_bytes(bytes[..8].try_into().unwrap())))
}

fn keys() -> Vec<MatrixKey> {
    (0..16)
        .flat_map(|r| (0..16).map(move |c| MatrixKey::new(r, c)))
        .collect()
}

#[test]
fn published_cells_read_back_and_unwritten_tiles_do_not_exist() {
    let store = MemStore::new();
    let mut w = TileWriter::create(&mut store.clone(), geometry(false), 1 << 20, [7; 16]).unwrap();
    let meta = w.meta;
    put(&mut w, &mut store.clone(), MatrixKey::new(3, 5), 35);
    put(&mut w, &mut store.clone(), MatrixKey::new(15, 15), 1515);
    assert_eq!(
        mine(&mut w, &mut store.clone(), MatrixKey::new(3, 5)),
        Some(35),
        "a writer sees its own staging"
    );
    assert_eq!(w.publish(&mut store.clone()).unwrap(), 1);
    let mut r = TileReader::open(&store, geometry(false), meta).unwrap();
    assert_eq!(
        seen(&mut r, &store, MatrixKey::new(3, 5)).unwrap(),
        Some(35)
    );
    assert_eq!(
        seen(&mut r, &store, MatrixKey::new(15, 15)).unwrap(),
        Some(1515)
    );
    assert_eq!(seen(&mut r, &store, MatrixKey::new(3, 4)).unwrap(), None);
    assert_eq!(seen(&mut r, &store, MatrixKey::new(8, 8)).unwrap(), None);
    // Two tiles exist, nothing else was allocated for the other 30.
    let tiles = store
        .0
        .borrow()
        .appends
        .iter()
        .filter(|(kind, ..)| *kind == super::RecordKind::Tile)
        .count();
    assert_eq!(tiles, 2);
}

#[test]
fn every_direction_grows_without_moving_a_tile() {
    let store = MemStore::new();
    let mut geom = geometry(true);
    geom.dims[1] = u64::from(u32::MAX);
    let mut w = TileWriter::create(&mut store.clone(), geom.clone(), 1 << 20, [7; 16]).unwrap();
    let keys = [
        MatrixKey::new(0, 0),
        MatrixKey::new(1_000_000, 3),
        MatrixKey::new(5, 2_000_000),
        MatrixKey::new(3_000_000_000, 4_000_000_000),
    ];
    for (i, key) in keys.iter().enumerate() {
        put(&mut w, &mut store.clone(), *key, i as u64 + 1);
        w.publish(&mut store.clone()).unwrap();
    }
    let mut r = TileReader::open(&store, geom, w.meta).unwrap();
    for (i, key) in keys.iter().enumerate() {
        assert_eq!(
            seen(&mut r, &store, *key).unwrap(),
            Some(i as u64 + 1),
            "{key:?}"
        );
    }
    let mut count = 0;
    w.for_each_tile(&store, 0, &mut |_| {
        count += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(count, 4);
}

#[test]
fn a_reader_keeps_its_snapshot_across_publications_and_checkpoints() {
    let store = MemStore::new();
    // A small ring, so checkpoints happen within the test.
    let mut w =
        TileWriter::create(&mut store.clone(), geometry(false), 80 * 1024, [7; 16]).unwrap();
    for key in keys() {
        put(&mut w, &mut store.clone(), key, 1);
    }
    w.publish(&mut store.clone()).unwrap();
    let mut old = TileReader::open(&store, geometry(false), w.meta).unwrap();
    let mut follower = TileReader::open(&store, geometry(false), w.meta).unwrap();
    let mut expired = false;
    for generation in 2..3000u64 {
        let key = MatrixKey::new(generation % 16, (generation * 7) % 16);
        put(&mut w, &mut store.clone(), key, generation);
        put(&mut w, &mut store.clone(), MatrixKey::new(0, 0), generation);
        w.publish(&mut store.clone()).unwrap();
        follower.follow(&store).unwrap();
        assert_eq!(
            seen(&mut follower, &store, MatrixKey::new(0, 0)).unwrap(),
            Some(generation)
        );
        assert_eq!(seen(&mut follower, &store, key).unwrap(), Some(generation));
        match seen(&mut old, &store, MatrixKey::new(0, 0)) {
            Ok(value) => {
                assert!(!expired, "an expired snapshot came back");
                assert_eq!(value, Some(1), "the old snapshot moved");
            }
            Err(Error::MatrixSnapshotExpired { .. }) => expired = true,
            Err(other) => panic!("{other}"),
        }
    }
    assert!(w.stats.checkpoints > 0, "the ring never filled");
    assert!(expired, "a snapshot older than the ring never expired");
    // Following brings it back.
    old.follow(&store).unwrap();
    assert_eq!(
        seen(&mut old, &store, MatrixKey::new(0, 0)).unwrap(),
        Some(2999)
    );
}

#[test]
fn the_ring_holds_the_disk_space_constant() {
    let store = MemStore::new();
    let mut w =
        TileWriter::create(&mut store.clone(), geometry(false), 80 * 1024, [7; 16]).unwrap();
    for key in keys() {
        put(&mut w, &mut store.clone(), key, 0);
    }
    w.publish(&mut store.clone()).unwrap();
    let len = store.len();
    for generation in 1..2_000u64 {
        put(
            &mut w,
            &mut store.clone(),
            MatrixKey::new(generation % 16, generation % 13),
            generation,
        );
        w.publish(&mut store.clone()).unwrap();
    }
    assert_eq!(store.len(), len, "the file grew with overwrites");
    // One barrier per publication, plus two per checkpoint.
    assert_eq!(
        w.stats.barriers,
        1 + 1 + 1999 + 2 * w.stats.checkpoints,
        "{:?}",
        w.stats
    );
}

#[test]
fn a_publication_larger_than_a_frame_spills_and_commits_whole() {
    const WIDE: BlockGeom = BlockGeom {
        block_id: 8,
        stride: 1024,
        tile: [2, 4],
    };
    let geom = Geometry {
        dims: [16, 16],
        growing: false,
        blocks: vec![WIDE],
        extra_len: 0,
    };
    let cell = |value: u8| {
        let mut out = vec![value; 1028];
        let crc = crc32fast::hash(&out[..1024]);
        out[1024..].copy_from_slice(&crc.to_le_bytes());
        out
    };
    let store = MemStore::new();
    let mut w = TileWriter::create(&mut store.clone(), geom.clone(), 1 << 20, [7; 16]).unwrap();
    // Tiles a publication appends are written in place at once; only
    // overwrites of published tiles go through the ring. Publish first.
    let put_all = |w: &mut TileWriter, value: u8| {
        for key in keys() {
            let (coord, index) = WIDE.locate(key);
            let tile = w.tile(&mut store.clone(), 0, coord, true).unwrap().unwrap();
            w.stage(&mut store.clone(), tile + WIDE.slot_at(index), &cell(value))
                .unwrap();
        }
    };
    put_all(&mut w, 4);
    assert_eq!(w.stats.frames, 0, "fresh tiles went through the ring");
    w.publish(&mut store.clone()).unwrap();
    let frames = w.stats.frames;
    put_all(&mut w, 5);
    assert!(
        w.stats.frames > frames + 1,
        "nothing spilled before the publication"
    );
    let mut r = TileReader::open(&store, geom.clone(), w.meta).unwrap();
    let read = |r: &mut TileReader, key: MatrixKey| {
        let (coord, index) = WIDE.locate(key);
        let tile = r.tile(&store, 0, coord).unwrap()?;
        let mut bytes = vec![0u8; 1028];
        r.read(&store, tile + WIDE.slot_at(index), &mut bytes)
            .unwrap();
        Some(bytes[0])
    };
    assert_eq!(
        read(&mut r, MatrixKey::new(0, 0)),
        Some(4),
        "spilled frames are not published"
    );
    w.publish(&mut store.clone()).unwrap();
    r.follow(&store).unwrap();
    for key in keys() {
        assert_eq!(read(&mut r, key), Some(5), "{key:?}");
    }
}

#[test]
fn discarded_changes_never_reach_a_reader_or_the_writer() {
    let store = MemStore::new();
    let mut w = TileWriter::create(&mut store.clone(), geometry(false), 1 << 20, [7; 16]).unwrap();
    put(&mut w, &mut store.clone(), MatrixKey::new(1, 1), 11);
    w.publish(&mut store.clone()).unwrap();
    put(&mut w, &mut store.clone(), MatrixKey::new(1, 1), 99);
    put(&mut w, &mut store.clone(), MatrixKey::new(9, 9), 99);
    w.discard();
    assert_eq!(
        mine(&mut w, &mut store.clone(), MatrixKey::new(1, 1)),
        Some(11)
    );
    assert_eq!(mine(&mut w, &mut store.clone(), MatrixKey::new(9, 9)), None);
    w.publish(&mut store.clone()).unwrap();
    let mut r = TileReader::open(&store, geometry(false), w.meta).unwrap();
    assert_eq!(
        seen(&mut r, &store, MatrixKey::new(1, 1)).unwrap(),
        Some(11)
    );
}

/// Every crash point inside one publication (frame half written, commit
/// frame missing, header torn) recovers either the state before it or the
/// whole publication, never a mix.
#[test]
fn a_crash_inside_a_publication_recovers_all_or_nothing() {
    let store = MemStore::new();
    let mut w = TileWriter::create(&mut store.clone(), geometry(false), 1 << 20, [7; 16]).unwrap();
    let meta = w.meta;
    for key in keys() {
        put(&mut w, &mut store.clone(), key, 1);
    }
    w.publish(&mut store.clone()).unwrap();
    let changed: Vec<MatrixKey> = keys().into_iter().step_by(3).collect();
    for key in &changed {
        put(&mut w, &mut store.clone(), *key, 2);
    }
    store.fail_sync_in(1);
    assert!(
        w.publish(&mut store.clone()).is_err(),
        "the injected crash did not stop the barrier"
    );
    let writes = store.writes_since_sync();
    assert!(writes > 0);
    let mut outcomes = [0usize; 2];
    for keep in 0..=writes {
        for torn in [0usize, 5, 13, 4000] {
            let crashed = store.crashed(keep, torn);
            let mut recovered =
                TileWriter::open(&mut crashed.clone(), geometry(false), meta).unwrap();
            let values: Vec<_> = changed
                .iter()
                .map(|key| mine(&mut recovered, &mut crashed.clone(), *key))
                .collect();
            let old = values.iter().all(|v| *v == Some(1));
            let new = values.iter().all(|v| *v == Some(2));
            assert!(old || new, "keep {keep} torn {torn}: {values:?}");
            outcomes[usize::from(new)] += 1;
            // And a reader of the recovered file agrees.
            let mut r = TileReader::open(&crashed, geometry(false), meta).unwrap();
            assert_eq!(seen(&mut r, &crashed, changed[0]).unwrap(), values[0]);
        }
    }
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "{outcomes:?}");
}

/// A crash anywhere inside a checkpoint loses nothing that was published.
#[test]
fn a_crash_inside_a_checkpoint_keeps_every_published_value() {
    for fail_at in [1u64, 2] {
        let store = MemStore::new();
        let mut w =
            TileWriter::create(&mut store.clone(), geometry(false), 1 << 20, [7; 16]).unwrap();
        let meta = w.meta;
        for key in keys() {
            put(&mut w, &mut store.clone(), key, 1);
        }
        w.publish(&mut store.clone()).unwrap();
        for generation in 2..30u64 {
            put(
                &mut w,
                &mut store.clone(),
                MatrixKey::new(generation % 16, generation % 11),
                generation,
            );
            w.publish(&mut store.clone()).unwrap();
        }
        let expected: Vec<_> = keys()
            .iter()
            .map(|key| mine(&mut w, &mut store.clone(), *key))
            .collect();
        store.fail_sync_in(fail_at);
        assert!(w.checkpoint_all(&mut store.clone()).is_err());
        let writes = store.writes_since_sync();
        for keep in 0..=writes {
            for torn in [0usize, 3, 9] {
                let crashed = store.crashed(keep, torn);
                let mut recovered =
                    TileWriter::open(&mut crashed.clone(), geometry(false), meta).unwrap();
                let got: Vec<_> = keys()
                    .iter()
                    .map(|key| mine(&mut recovered, &mut crashed.clone(), *key))
                    .collect();
                assert_eq!(got, expected, "barrier {fail_at}, keep {keep}, torn {torn}");
            }
        }
    }
}

/// A publication that allocates tiles writes their cells in place directly.
/// A crash at any point of it, before or after either barrier, recovers no
/// part of it or all of it.
#[test]
fn a_crash_while_allocating_tiles_recovers_all_or_nothing() {
    for fail_at in [1u64, 2] {
        let store = MemStore::new();
        let mut w =
            TileWriter::create(&mut store.clone(), geometry(false), 1 << 20, [7; 16]).unwrap();
        let meta = w.meta;
        put(&mut w, &mut store.clone(), MatrixKey::new(0, 0), 1);
        w.publish(&mut store.clone()).unwrap();
        let new_keys: Vec<MatrixKey> = keys().into_iter().filter(|k| k.scan >= 4).collect();
        for key in &new_keys {
            put(&mut w, &mut store.clone(), *key, 9);
        }
        store.fail_sync_in(fail_at);
        assert!(w.publish(&mut store.clone()).is_err());
        let writes = store.writes_since_sync();
        let mut outcomes = [0usize; 2];
        for keep in 0..=writes {
            for torn in [0usize, 11, 3000] {
                let crashed = store.crashed(keep, torn);
                let mut recovered =
                    TileWriter::open(&mut crashed.clone(), geometry(false), meta).unwrap();
                assert_eq!(
                    mine(&mut recovered, &mut crashed.clone(), MatrixKey::new(0, 0)),
                    Some(1)
                );
                let values: Vec<_> = new_keys
                    .iter()
                    .map(|key| mine(&mut recovered, &mut crashed.clone(), *key))
                    .collect();
                let none = values.iter().all(Option::is_none);
                let all = values.iter().all(|v| *v == Some(9));
                assert!(
                    none || all,
                    "barrier {fail_at} keep {keep} torn {torn}: {values:?}"
                );
                outcomes[usize::from(all)] += 1;
                let mut r = TileReader::open(&crashed, geometry(false), meta).unwrap();
                assert_eq!(seen(&mut r, &crashed, new_keys[0]).unwrap(), values[0]);
            }
        }
        if fail_at == 2 {
            assert!(outcomes[1] > 0, "the frames never landed: {outcomes:?}");
        }
    }
}

/// A primary recovery may cut the file back to its last acknowledged sync,
/// taking tiles a later matrix generation was published over. That
/// generation names the file length it needs, so a writer and a reader opened
/// on the cut file never adopt it, and new publications go on from the one
/// before.
#[test]
fn a_generation_needing_records_a_recovery_cut_off_is_not_adopted() {
    let store = MemStore::new();
    let mut w = TileWriter::create(&mut store.clone(), geometry(false), 1 << 20, [7; 16]).unwrap();
    let meta = w.meta;
    put(&mut w, &mut store.clone(), MatrixKey::new(0, 0), 1);
    w.publish(&mut store.clone()).unwrap();
    let acknowledged = store.len();
    // Generation 2 lives in tiles appended after `acknowledged`.
    let new_keys: Vec<MatrixKey> = keys().into_iter().filter(|k| k.scan >= 4).collect();
    for key in &new_keys {
        put(&mut w, &mut store.clone(), *key, 9);
    }
    put(&mut w, &mut store.clone(), MatrixKey::new(0, 0), 2);
    w.publish(&mut store.clone()).unwrap();
    assert!(store.len() > acknowledged);

    let cut = store.truncated(acknowledged);
    let mut recovered = TileWriter::open(&mut cut.clone(), geometry(false), meta).unwrap();
    assert_eq!(recovered.published(), 1);
    assert_eq!(
        mine(&mut recovered, &mut cut.clone(), MatrixKey::new(0, 0)),
        Some(1)
    );
    assert!(
        new_keys
            .iter()
            .all(|key| mine(&mut recovered, &mut cut.clone(), *key).is_none())
    );
    let mut r = TileReader::open(&cut, geometry(false), meta).unwrap();
    assert_eq!(seen(&mut r, &cut, MatrixKey::new(0, 0)).unwrap(), Some(1));
    assert_eq!(seen(&mut r, &cut, new_keys[0]).unwrap(), None);

    // The cut file grows again; the next generation follows the one kept.
    put(&mut recovered, &mut cut.clone(), new_keys[0], 5);
    put(&mut recovered, &mut cut.clone(), MatrixKey::new(0, 0), 3);
    assert_eq!(recovered.publish(&mut cut.clone()).unwrap(), 2);
    let mut again = TileWriter::open(&mut cut.clone(), geometry(false), meta).unwrap();
    assert_eq!(
        mine(&mut again, &mut cut.clone(), MatrixKey::new(0, 0)),
        Some(3)
    );
    assert_eq!(mine(&mut again, &mut cut.clone(), new_keys[0]), Some(5));
    assert_eq!(mine(&mut again, &mut cut.clone(), new_keys[1]), None);
}

/// A ring length outside the format's bounds, read from a metadata record,
/// is refused before anything is sized by it.
#[test]
fn a_ring_length_outside_the_format_bounds_is_refused() {
    let geom = geometry(false);
    for (len, ok) in [
        (super::wal::MIN_RING, true),
        (super::wal::MAX_RING_BYTES, true),
        (super::wal::MIN_RING - 8, false),
        (super::wal::MAX_RING_BYTES + 8, false),
    ] {
        let mut meta = vec![0u8; geom.meta_len() as usize];
        geom.encode_meta((4096, len), [1; 16], &mut meta);
        assert_eq!(geom.decode_meta(&meta).is_ok(), ok, "ring of {len}");
    }
}

/// The tile cache is one fixed table: an entry answers for exactly its tile,
/// a colliding tile replaces it, and nothing ever grows.
#[test]
fn the_tile_cache_is_a_fixed_table() {
    use super::geometry::TileCache;
    let geom = geometry(false);
    let mut cache = TileCache::for_geometry(&geom).unwrap();
    assert_eq!(cache.get(0, 7), None);
    cache.insert(0, 7, 4096);
    assert_eq!(cache.get(0, 7), Some(4096));
    assert_eq!(cache.get(1, 7), None);
    assert_eq!(cache.get(0, 8), None);
    // The same tile of another block is another entry, not the same slot:
    // across a spread of tiles the two blocks rarely evict each other.
    let mut kept = 0;
    for tile in 0..32u64 {
        cache.clear();
        cache.insert(0, tile, 1);
        cache.insert(1, tile, 2);
        assert_eq!(cache.get(1, tile), Some(2));
        kept += u32::from(cache.get(0, tile) == Some(1));
    }
    assert!(kept >= 28, "blocks collide systematically: {kept}/32 kept");
    // Far more tiles than slots: every answer is still the right tile's,
    // or a miss, and the last one inserted is always there.
    for tile in 0..200_000u64 {
        cache.insert(0, tile, tile * 3 + 1);
        assert_eq!(cache.get(0, tile), Some(tile * 3 + 1));
    }
    for tile in 0..200_000u64 {
        if let Some(at) = cache.get(0, tile) {
            assert_eq!(at, tile * 3 + 1);
        }
    }
    cache.clear();
    assert_eq!(cache.get(0, 199_999), None);
}
