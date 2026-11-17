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
    w.engine
        .stage(store, tile + BLOCK.slot_at(index), &slot(value))
        .unwrap();
}

/// The value a writer sees for `key` (`None`: tile absent or never written).
fn mine(w: &mut TileWriter, store: &mut MemStore, key: MatrixKey) -> Option<u64> {
    let (coord, index) = BLOCK.locate(key);
    let tile = w.tile(store, 0, coord, false).unwrap()?;
    let mut bytes = [0u8; 12];
    w.engine
        .read(store, tile + BLOCK.slot_at(index), &mut bytes)
        .unwrap();
    (bytes != [0; 12]).then(|| u64::from_le_bytes(bytes[..8].try_into().unwrap()))
}

fn seen(r: &mut TileReader, store: &MemStore, key: MatrixKey) -> crate::Result<Option<u64>> {
    let (coord, index) = BLOCK.locate(key);
    let Some(tile) = r.tile(store, 0, coord)? else {
        return Ok(None);
    };
    let mut bytes = [0u8; 12];
    r.reader
        .read(store, tile + BLOCK.slot_at(index), &mut bytes)?;
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
    assert_eq!(w.engine.publish(&mut store.clone()).unwrap(), 1);
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
        w.engine.publish(&mut store.clone()).unwrap();
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
    w.for_each_tile_ref(&store, 0, &mut |_| {
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
    w.engine.publish(&mut store.clone()).unwrap();
    let mut old = TileReader::open(&store, geometry(false), w.meta).unwrap();
    let mut follower = TileReader::open(&store, geometry(false), w.meta).unwrap();
    let mut expired = false;
    for generation in 2..3000u64 {
        let key = MatrixKey::new(generation % 16, (generation * 7) % 16);
        put(&mut w, &mut store.clone(), key, generation);
        put(&mut w, &mut store.clone(), MatrixKey::new(0, 0), generation);
        w.engine.publish(&mut store.clone()).unwrap();
        follower.reader.follow(&store).unwrap();
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
    assert!(w.engine.stats.checkpoints > 0, "the ring never filled");
    assert!(expired, "a snapshot older than the ring never expired");
    // Following brings it back.
    old.reader.follow(&store).unwrap();
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
    w.engine.publish(&mut store.clone()).unwrap();
    let len = store.len();
    for generation in 1..2_000u64 {
        put(
            &mut w,
            &mut store.clone(),
            MatrixKey::new(generation % 16, generation % 13),
            generation,
        );
        w.engine.publish(&mut store.clone()).unwrap();
    }
    assert_eq!(store.len(), len, "the file grew with overwrites");
    // One barrier per publication, plus two per checkpoint.
    assert_eq!(
        w.engine.stats.barriers,
        1 + 1 + 1999 + 2 * w.engine.stats.checkpoints,
        "{:?}",
        w.engine.stats
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
            w.engine
                .stage(&mut store.clone(), tile + WIDE.slot_at(index), &cell(value))
                .unwrap();
        }
    };
    put_all(&mut w, 4);
    assert_eq!(
        w.engine.stats.frames, 0,
        "fresh tiles went through the ring"
    );
    w.engine.publish(&mut store.clone()).unwrap();
    let frames = w.engine.stats.frames;
    put_all(&mut w, 5);
    // Staged bytes reach the file as they are staged: ring frames, then an
    // index once the publication outgrows the ring.
    assert!(
        w.engine.stats.frames > frames || w.engine.stats.indexed > 0,
        "nothing reached the file before the publication: {:?}",
        w.engine.stats
    );
    let mut r = TileReader::open(&store, geom.clone(), w.meta).unwrap();
    let read = |r: &mut TileReader, key: MatrixKey| {
        let (coord, index) = WIDE.locate(key);
        let tile = r.tile(&store, 0, coord).unwrap()?;
        let mut bytes = vec![0u8; 1028];
        r.reader
            .read(&store, tile + WIDE.slot_at(index), &mut bytes)
            .unwrap();
        Some(bytes[0])
    };
    assert_eq!(
        read(&mut r, MatrixKey::new(0, 0)),
        Some(4),
        "spilled frames are not published"
    );
    w.engine.publish(&mut store.clone()).unwrap();
    r.reader.follow(&store).unwrap();
    for key in keys() {
        assert_eq!(read(&mut r, key), Some(5), "{key:?}");
    }
}

#[test]
fn discarded_changes_never_reach_a_reader_or_the_writer() {
    let store = MemStore::new();
    let mut w = TileWriter::create(&mut store.clone(), geometry(false), 1 << 20, [7; 16]).unwrap();
    put(&mut w, &mut store.clone(), MatrixKey::new(1, 1), 11);
    w.engine.publish(&mut store.clone()).unwrap();
    put(&mut w, &mut store.clone(), MatrixKey::new(1, 1), 99);
    put(&mut w, &mut store.clone(), MatrixKey::new(9, 9), 99);
    w.discard();
    assert_eq!(
        mine(&mut w, &mut store.clone(), MatrixKey::new(1, 1)),
        Some(11)
    );
    assert_eq!(mine(&mut w, &mut store.clone(), MatrixKey::new(9, 9)), None);
    w.engine.publish(&mut store.clone()).unwrap();
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
    w.engine.publish(&mut store.clone()).unwrap();
    let changed: Vec<MatrixKey> = keys().into_iter().step_by(3).collect();
    for key in &changed {
        put(&mut w, &mut store.clone(), *key, 2);
    }
    store.fail_sync_in(1);
    assert!(
        w.engine.publish(&mut store.clone()).is_err(),
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
        w.engine.publish(&mut store.clone()).unwrap();
        for generation in 2..30u64 {
            put(
                &mut w,
                &mut store.clone(),
                MatrixKey::new(generation % 16, generation % 11),
                generation,
            );
            w.engine.publish(&mut store.clone()).unwrap();
        }
        let expected: Vec<_> = keys()
            .iter()
            .map(|key| mine(&mut w, &mut store.clone(), *key))
            .collect();
        store.fail_sync_in(fail_at);
        assert!(w.engine.checkpoint_all(&mut store.clone()).is_err());
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
        w.engine.publish(&mut store.clone()).unwrap();
        let new_keys: Vec<MatrixKey> = keys().into_iter().filter(|k| k.scan >= 4).collect();
        for key in &new_keys {
            put(&mut w, &mut store.clone(), *key, 9);
        }
        store.fail_sync_in(fail_at);
        assert!(w.engine.publish(&mut store.clone()).is_err());
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
    w.engine.publish(&mut store.clone()).unwrap();
    let acknowledged = store.len();
    // Generation 2 lives in tiles appended after `acknowledged`.
    let new_keys: Vec<MatrixKey> = keys().into_iter().filter(|k| k.scan >= 4).collect();
    for key in &new_keys {
        put(&mut w, &mut store.clone(), *key, 9);
    }
    put(&mut w, &mut store.clone(), MatrixKey::new(0, 0), 2);
    w.engine.publish(&mut store.clone()).unwrap();
    assert!(store.len() > acknowledged);

    let cut = store.truncated(acknowledged);
    let mut recovered = TileWriter::open(&mut cut.clone(), geometry(false), meta).unwrap();
    assert_eq!(recovered.engine.published(), 1);
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
    assert_eq!(recovered.engine.publish(&mut cut.clone()).unwrap(), 2);
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

/// A ring header whose checksum holds but whose offsets or generations are
/// impossible is refused, not followed: a checksum only proves the bytes
/// were written together, not that a sane writer wrote them.
#[test]
fn a_ring_header_with_impossible_fields_is_refused_not_followed() {
    use super::wal::{HEADER_SLOT, Ring, RingHeader};
    let hostile = |edit: &dyn Fn(&mut RingHeader)| {
        let store = MemStore::new();
        let mut w =
            TileWriter::create(&mut store.clone(), geometry(false), 1 << 20, [7; 16]).unwrap();
        let meta = w.meta;
        put(&mut w, &mut store.clone(), MatrixKey::new(1, 1), 11);
        w.engine.publish(&mut store.clone()).unwrap();
        let ring: Ring = w.engine.ring;
        let mut header = ring.read_header(&store).unwrap();
        header.seq += 2;
        edit(&mut header);
        let bytes = header.encode();
        {
            let mut shared = store.0.borrow_mut();
            for slot in 0..2u64 {
                let at = (ring.base + slot * HEADER_SLOT) as usize;
                shared.bytes[at..at + bytes.len()].copy_from_slice(&bytes);
            }
        }
        let reader = TileReader::open(&store, geometry(false), meta).map(|_| ());
        let writer = TileWriter::open(&mut store.clone(), geometry(false), meta).map(|_| ());
        (reader, writer)
    };
    let area = (1u64 << 20) - super::wal::FRAMES_AT;
    type Edit<'a> = &'a dyn Fn(&mut RingHeader);
    let cases: [(&str, Edit<'_>); 6] = [
        ("tail past the ring", &|h| h.tail = area + 8),
        ("tail far past the ring", &|h| h.tail = u64::MAX - 7),
        ("head past the ring", &|h| h.head = area + 8),
        ("unaligned tail", &|h| h.tail = 3),
        ("generations at the limit", &|h| {
            h.checkpointed = u64::MAX;
            h.mark = u64::MAX;
            h.published = u64::MAX;
        }),
        ("sequence at the limit", &|h| h.seq = u64::MAX),
    ];
    for (name, edit) in cases {
        let (reader, writer) = hostile(edit);
        assert!(
            matches!(reader, Err(Error::InvalidMatrixLayout)),
            "{name}: reader {reader:?}"
        );
        assert!(
            matches!(writer, Err(Error::InvalidMatrixLayout)),
            "{name}: writer {writer:?}"
        );
    }
}

/// A metadata record naming a ring outside the file is refused before any
/// ring offset is computed from it.
#[test]
fn a_ring_placed_outside_the_file_is_refused() {
    for base in [u64::MAX - 64, u64::MAX / 2, 1 << 40] {
        let store = MemStore::new();
        let w = TileWriter::create(&mut store.clone(), geometry(false), 1 << 20, [7; 16]).unwrap();
        let meta = w.meta as usize;
        {
            let mut shared = store.0.borrow_mut();
            shared.bytes[meta + 32..meta + 40].copy_from_slice(&base.to_le_bytes());
            let crc = crc32fast::hash(&shared.bytes[meta..meta + 64]);
            shared.bytes[meta + 64..meta + 68].copy_from_slice(&crc.to_le_bytes());
        }
        assert!(matches!(
            TileReader::open(&store, geometry(false), meta as u64).map(|_| ()),
            Err(Error::InvalidMatrixLayout)
        ));
        assert!(matches!(
            TileWriter::open(&mut store.clone(), geometry(false), meta as u64).map(|_| ()),
            Err(Error::InvalidMatrixLayout)
        ));
    }
}

/// Every key maps to exactly one slot of exactly one tile, and back, at the
/// smallest, the odd and the largest shapes a spec may declare. Small tiles
/// are checked cell by cell; the largest at their corners and edges.
#[test]
fn every_key_has_one_slot_in_one_tile_at_every_admitted_shape() {
    use crate::format::{MATRIX_MAX_TILE_BYTES, MATRIX_MAX_TILE_CELLS};
    let shapes: [([u64; 2], u64); 9] = [
        ([1, 1], 1),
        ([1, 7], 3),
        ([5, 3], 8),
        ([16, 16], 12),
        ([1, MATRIX_MAX_TILE_CELLS], 60),
        ([MATRIX_MAX_TILE_CELLS, 1], 60),
        ([1024, 1024], 60),
        ([1 << 13, 1 << 7], 60),
        ([1, 1], MATRIX_MAX_TILE_BYTES - 4),
    ];
    for (tile, stride) in shapes {
        let geom = BlockGeom {
            block_id: 1,
            stride,
            tile,
        };
        let [rows, cols] = tile;
        assert!(geom.cells() <= MATRIX_MAX_TILE_CELLS);
        assert!(geom.cells() * geom.slot_len() <= MATRIX_MAX_TILE_BYTES);
        let edge = |n: u64| -> Vec<u64> {
            if n <= 16 {
                (0..n).collect()
            } else {
                vec![0, 1, 2, n / 2 - 1, n / 2, n - 3, n - 2, n - 1]
            }
        };
        let mut slots = std::collections::BTreeSet::new();
        for row in edge(rows) {
            for col in edge(cols) {
                let slot = BlockGeom::slot_index(row, col, cols);
                assert!(slot < geom.cells(), "{tile:?} {row} {col}");
                assert!(slots.insert(slot), "{tile:?}: two cells share slot {slot}");
                assert_eq!((slot / cols, slot % cols), (row, col));
                assert!(geom.slot_at(slot) >= geom.slots_at());
                assert!(geom.slot_at(slot) + geom.slot_len() <= geom.tile_bytes());
                assert!(slot / 8 < geom.bitmap_len());
                // The same cell in tiles far apart, up to the last key.
                for (tile_row, tile_col) in [
                    (0, 0),
                    (3, 9),
                    ((u64::MAX - rows + 1) / rows, (u64::MAX - cols + 1) / cols),
                ] {
                    let key = MatrixKey::new(tile_row * rows + row, tile_col * cols + col);
                    let (coord, located) = geom.locate(key);
                    assert_eq!((coord.row, coord.col, located), (tile_row, tile_col, slot));
                }
            }
        }
    }
    let last = BlockGeom {
        block_id: 1,
        stride: 8,
        tile: [3, 5],
    }
    .locate(MatrixKey::new(u64::MAX, u64::MAX));
    assert_eq!((last.0.row, last.0.col), (u64::MAX / 3, u64::MAX / 5));
}

/// Directory pages carry no checksum: an offset or height read from one is
/// refused when it cannot be a place in a file, before anything is computed
/// from it.
#[test]
fn directory_offsets_that_are_not_file_offsets_are_refused() {
    let build = || {
        let store = MemStore::new();
        let mut w =
            TileWriter::create(&mut store.clone(), geometry(false), 1 << 20, [7; 16]).unwrap();
        put(&mut w, &mut store.clone(), MatrixKey::new(1, 1), 11);
        w.engine.publish(&mut store.clone()).unwrap();
        w.engine.checkpoint(&mut store.clone(), u64::MAX).unwrap();
        let meta = w.meta;
        let root_at = (meta + geometry(false).root_at(0)) as usize;
        (store, meta, root_at)
    };
    let coord = BLOCK.locate(MatrixKey::new(1, 1)).0;
    let refused = |store: &MemStore, meta: u64| {
        let mut r = TileReader::open(store, geometry(false), meta).unwrap();
        assert!(matches!(
            r.tile(store, 0, coord),
            Err(Error::InvalidMatrixLayout)
        ));
        let mut seen = 0;
        assert!(matches!(
            r.for_each_tile(store, 0, &mut |_| {
                seen += 1;
                Ok(())
            }),
            Err(Error::InvalidMatrixLayout)
        ));
        assert_eq!(seen, 0);
        let mut w = TileWriter::open(&mut store.clone(), geometry(false), meta).unwrap();
        assert!(matches!(
            w.tile(&mut store.clone(), 0, coord, true),
            Err(Error::InvalidMatrixLayout)
        ));
    };
    // A root page that is not a file offset.
    let (store, meta, root_at) = build();
    store.0.borrow_mut().bytes[root_at..root_at + 8].copy_from_slice(&(u64::MAX - 3).to_le_bytes());
    refused(&store, meta);
    // A leaf entry (a tile) that is not a file offset.
    let (store, meta, root_at) = build();
    {
        let mut shared = store.0.borrow_mut();
        let page = u64::from_le_bytes(shared.bytes[root_at..root_at + 8].try_into().unwrap());
        let index = coord.morton().unwrap() as usize;
        let at = page as usize + index * 8;
        assert_ne!(shared.bytes[at..at + 8], [0; 8], "the tile's entry");
        shared.bytes[at..at + 8].copy_from_slice(&(u64::MAX - 8).to_le_bytes());
    }
    refused(&store, meta);
    // A height no directory has.
    let (store, meta, root_at) = build();
    store.0.borrow_mut().bytes[root_at + 8..root_at + 16].copy_from_slice(&u64::MAX.to_le_bytes());
    refused(&store, meta);
}

/// A block of 1 KiB cells, 64 x 64 of them: one overwrite of every cell is
/// about 4 MiB, sixty times the smallest ring.
const WIDE_CELLS: BlockGeom = BlockGeom {
    block_id: 9,
    stride: 1024,
    tile: [4, 8],
};

fn wide_geometry() -> Geometry {
    Geometry {
        dims: [64, 64],
        growing: false,
        blocks: vec![WIDE_CELLS],
        extra_len: 0,
    }
}

fn wide_cell(value: u8) -> Vec<u8> {
    let mut out = vec![value; 1028];
    let crc = crc32fast::hash(&out[..1024]);
    out[1024..].copy_from_slice(&crc.to_le_bytes());
    out
}

fn wide_keys() -> impl Iterator<Item = MatrixKey> {
    (0..64).flat_map(|r| (0..64).map(move |c| MatrixKey::new(r, c)))
}

fn wide_put_all(w: &mut TileWriter, store: &MemStore, value: u8) {
    for key in wide_keys() {
        let (coord, index) = WIDE_CELLS.locate(key);
        let tile = w.tile(&mut store.clone(), 0, coord, true).unwrap().unwrap();
        w.engine
            .stage(
                &mut store.clone(),
                tile + WIDE_CELLS.slot_at(index),
                &wide_cell(value),
            )
            .unwrap();
    }
}

/// The value a reader sees in a wide cell, after checking the cell is whole.
fn wide_seen(r: &mut TileReader, store: &MemStore, key: MatrixKey) -> crate::Result<Option<u8>> {
    let (coord, index) = WIDE_CELLS.locate(key);
    let Some(tile) = r.tile(store, 0, coord)? else {
        return Ok(None);
    };
    let mut bytes = vec![0u8; 1028];
    r.reader
        .read(store, tile + WIDE_CELLS.slot_at(index), &mut bytes)?;
    assert!(
        bytes[..1024].iter().all(|b| *b == bytes[0]),
        "{key:?} mixes two publications"
    );
    Ok(Some(bytes[0]))
}

#[test]
fn a_publication_larger_than_the_ring_streams_through_overflow_and_commits_whole() {
    let store = MemStore::new();
    let mut w = TileWriter::create(
        &mut store.clone(),
        wide_geometry(),
        super::wal::MIN_RING,
        [7; 16],
    )
    .unwrap();
    let meta = w.meta;
    // The first load appends fresh tiles and never touches the ring.
    wide_put_all(&mut w, &store, 1);
    w.engine.publish(&mut store.clone()).unwrap();
    let mut before = TileReader::open(&store, wide_geometry(), meta).unwrap();

    // Overwriting every cell is sixty rings' worth of bytes in one publication.
    wide_put_all(&mut w, &store, 2);
    // Later spills fill the region the first one opened, and each new region
    // is at least as large as what streamed before it: a few regions, not
    // one per spill.
    assert!(
        (1..=4).contains(&w.engine.stats.overflow_appended),
        "the publication took {} regions: {:?}",
        w.engine.stats.overflow_appended,
        w.engine.stats
    );
    assert_eq!(
        wide_seen(&mut before, &store, MatrixKey::new(63, 63)).unwrap(),
        Some(1),
        "streamed bytes reached a reader before the commit"
    );
    w.engine.publish(&mut store.clone()).unwrap();
    for key in wide_keys() {
        assert_eq!(
            wide_seen(&mut before, &store, key).unwrap(),
            Some(1),
            "{key:?}"
        );
    }
    before.reader.follow(&store).unwrap();
    for key in wide_keys() {
        assert_eq!(
            wide_seen(&mut before, &store, key).unwrap(),
            Some(2),
            "{key:?}"
        );
    }

    // A reopened writer and a new reader find the publication.
    let mut reopened = TileWriter::open(&mut store.clone(), wide_geometry(), meta).unwrap();
    let mut fresh = TileReader::open(&store, wide_geometry(), meta).unwrap();
    for key in wide_keys().step_by(97) {
        assert_eq!(
            wide_seen(&mut fresh, &store, key).unwrap(),
            Some(2),
            "{key:?}"
        );
        let (coord, index) = WIDE_CELLS.locate(key);
        let tile = reopened
            .tile(&mut store.clone(), 0, coord, false)
            .unwrap()
            .unwrap();
        let mut bytes = vec![0u8; 1028];
        reopened
            .engine
            .read(&store, tile + WIDE_CELLS.slot_at(index), &mut bytes)
            .unwrap();
        assert_eq!(bytes[0], 2);
    }
}

#[test]
fn overflow_regions_are_reused_once_no_reader_can_need_them() {
    let store = MemStore::new();
    let mut w = TileWriter::create(
        &mut store.clone(),
        wide_geometry(),
        super::wal::MIN_RING,
        [7; 16],
    )
    .unwrap();
    let meta = w.meta;
    wide_put_all(&mut w, &store, 1);
    w.engine.publish(&mut store.clone()).unwrap();
    wide_put_all(&mut w, &store, 2);
    w.engine.publish(&mut store.clone()).unwrap();
    // A reader holding the second generation while the writer moves on.
    let mut held = TileReader::open(&store, wide_geometry(), meta).unwrap();
    let appended = w.engine.stats.overflow_appended;
    let len = store.len();
    for value in 3..8 {
        wide_put_all(&mut w, &store, value);
        w.engine.publish(&mut store.clone()).unwrap();
    }
    assert!(w.engine.stats.overflow_reused > 0, "no region was reused");
    assert!(
        w.engine.stats.overflow_appended <= appended * 2,
        "five more large publications kept appending regions: {} after {appended}",
        w.engine.stats.overflow_appended
    );
    let grown = store.len() - len;
    assert!(
        grown <= 2 * (len / 2).max(8 << 20),
        "the file grew {grown} bytes for five rewrites of the same cells"
    );
    // The held reader sees its own generation or is told it expired; it
    // never reads a reused region's newer bytes as its own.
    for key in wide_keys().step_by(53) {
        match wide_seen(&mut held, &store, key) {
            Ok(value) => assert_eq!(value, Some(2), "{key:?}"),
            Err(Error::MatrixSnapshotExpired { .. }) => {}
            Err(error) => panic!("{error}"),
        }
    }
    held.reader.follow(&store).unwrap();
    for key in wide_keys() {
        assert_eq!(
            wide_seen(&mut held, &store, key).unwrap(),
            Some(7),
            "{key:?}"
        );
    }
}

/// A crash at any point of a publication that streamed through overflow
/// regions recovers the state before it or the whole publication.
#[test]
fn a_crash_inside_a_streamed_publication_recovers_all_or_nothing() {
    let mut outcomes = [0usize; 2];
    for failing_sync in 1..=3 {
        let store = MemStore::new();
        let mut w = TileWriter::create(
            &mut store.clone(),
            wide_geometry(),
            super::wal::MIN_RING,
            [7; 16],
        )
        .unwrap();
        let meta = w.meta;
        wide_put_all(&mut w, &store, 1);
        w.engine.publish(&mut store.clone()).unwrap();
        wide_put_all(&mut w, &store, 2);
        store.fail_sync_in(failing_sync);
        if w.engine.publish(&mut store.clone()).is_ok() {
            continue;
        }
        let writes = store.writes_since_sync();
        let step = (writes / 12).max(1);
        for keep in (0..=writes).step_by(step).chain([writes]) {
            for torn in [0usize, 3000] {
                let crashed = store.crashed(keep, torn);
                TileWriter::open(&mut crashed.clone(), wide_geometry(), meta).unwrap();
                let mut r = TileReader::open(&crashed, wide_geometry(), meta).unwrap();
                let values: Vec<_> = wide_keys()
                    .step_by(31)
                    .map(|key| wide_seen(&mut r, &crashed, key).unwrap())
                    .collect();
                let old = values.iter().all(|v| *v == Some(1));
                let new = values.iter().all(|v| *v == Some(2));
                assert!(
                    old || new,
                    "sync {failing_sync} keep {keep} torn {torn}: {values:?}"
                );
                outcomes[usize::from(new)] += 1;
            }
        }
    }
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "{outcomes:?}");
}

fn wide_put(
    w: &mut TileWriter,
    store: &MemStore,
    keys: impl Iterator<Item = MatrixKey>,
    value: u8,
) {
    for key in keys {
        let (coord, index) = WIDE_CELLS.locate(key);
        let tile = w.tile(&mut store.clone(), 0, coord, true).unwrap().unwrap();
        w.engine
            .stage(
                &mut store.clone(),
                tile + WIDE_CELLS.slot_at(index),
                &wide_cell(value),
            )
            .unwrap();
    }
}

fn wide_rows(rows: std::ops::Range<u64>) -> impl Iterator<Item = MatrixKey> {
    rows.flat_map(|r| (0..64).map(move |c| MatrixKey::new(r, c)))
}

/// A writer whose top half was written twice, the second time streamed
/// through an overflow region that a checkpoint then freed.
fn wide_with_free_overflow() -> (MemStore, TileWriter) {
    let store = MemStore::new();
    let mut w = TileWriter::create(
        &mut store.clone(),
        wide_geometry(),
        super::wal::MIN_RING,
        [7; 16],
    )
    .unwrap();
    wide_put(&mut w, &store, wide_rows(0..32), 1);
    w.engine.publish(&mut store.clone()).unwrap();
    wide_put(&mut w, &store, wide_rows(0..32), 2);
    w.engine.publish(&mut store.clone()).unwrap();
    assert!(w.engine.stats.overflow_appended > 0);
    w.engine.checkpoint_all(&mut store.clone()).unwrap();
    (store, w)
}

#[test]
fn new_tiles_fill_free_overflow_space_and_the_space_survives_a_reopen() {
    let (store, mut w) = wide_with_free_overflow();
    let meta = w.meta;
    let len = store.len();
    // The bottom half's tiles are new: they go into the free region.
    wide_put(&mut w, &store, wide_rows(32..64), 3);
    assert!(
        w.engine.stats.carved > 0,
        "no tile was placed in free overflow space"
    );
    w.engine.publish(&mut store.clone()).unwrap();
    let tile_bytes = WIDE_CELLS.tile_bytes();
    let grown = store.len() - len;
    assert!(
        grown < 64 * tile_bytes,
        "the file grew {grown} bytes for 64 new tiles of {tile_bytes} bytes"
    );
    let regions = w.engine.overflow_regions();
    let mut r = TileReader::open(&store, wide_geometry(), meta).unwrap();
    for key in wide_keys() {
        let want = if key.scan < 32 { 2 } else { 3 };
        assert_eq!(
            wide_seen(&mut r, &store, key).unwrap(),
            Some(want),
            "{key:?}"
        );
    }

    // A reopened writer finds the region, with the carved space recorded,
    // and streams its next large publication into what is left of it.
    let mut again = TileWriter::open(&mut store.clone(), wide_geometry(), meta).unwrap();
    assert_eq!(again.engine.overflow_regions(), regions);
    let len = store.len();
    wide_put(&mut again, &store, wide_rows(0..8), 4);
    again.engine.publish(&mut store.clone()).unwrap();
    assert_eq!(
        again.engine.stats.overflow_appended, 0,
        "{:?}",
        again.engine.stats
    );
    assert!(
        again.engine.stats.overflow_reused > 0,
        "{:?}",
        again.engine.stats
    );
    assert_eq!(store.len(), len, "the reopened writer grew the file");
    r.reader.follow(&store).unwrap();
    for key in wide_keys() {
        let want = match key.scan {
            0..8 => 4,
            8..32 => 2,
            _ => 3,
        };
        assert_eq!(
            wide_seen(&mut r, &store, key).unwrap(),
            Some(want),
            "{key:?}"
        );
    }
    again.engine.checkpoint_all(&mut store.clone()).unwrap();
    let mut fresh = TileReader::open(&store, wide_geometry(), meta).unwrap();
    for key in wide_keys() {
        let want = match key.scan {
            0..8 => 4,
            8..32 => 2,
            _ => 3,
        };
        assert_eq!(
            wide_seen(&mut fresh, &store, key).unwrap(),
            Some(want),
            "{key:?}"
        );
    }
}

/// A crash at any point of a publication whose new tiles were placed in free
/// overflow space recovers the state before it or the whole publication,
/// and the reopened writer's record of the carved space agrees.
#[test]
fn a_crash_while_tiles_fill_overflow_space_recovers_all_or_nothing() {
    let mut outcomes = [0usize; 2];
    for failing_sync in 1..=3 {
        let (store, mut w) = wide_with_free_overflow();
        let meta = w.meta;
        let before = w.engine.overflow_regions();
        wide_put(&mut w, &store, wide_rows(32..64), 3);
        assert!(w.engine.stats.carved > 0);
        let after = w.engine.overflow_regions();
        store.fail_sync_in(failing_sync);
        if w.engine.publish(&mut store.clone()).is_ok() {
            continue;
        }
        let writes = store.writes_since_sync();
        let step = (writes / 12).max(1);
        for keep in (0..=writes).step_by(step).chain([writes]) {
            for torn in [0usize, 3000] {
                let crashed = store.crashed(keep, torn);
                let reopened =
                    TileWriter::open(&mut crashed.clone(), wide_geometry(), meta).unwrap();
                let mut r = TileReader::open(&crashed, wide_geometry(), meta).unwrap();
                let top: Vec<_> = wide_rows(0..32)
                    .step_by(31)
                    .map(|key| wide_seen(&mut r, &crashed, key).unwrap())
                    .collect();
                assert!(top.iter().all(|v| *v == Some(2)), "{top:?}");
                let bottom: Vec<_> = wide_rows(32..64)
                    .step_by(31)
                    .map(|key| wide_seen(&mut r, &crashed, key).unwrap())
                    .collect();
                if bottom.iter().all(Option::is_none) {
                    assert_eq!(reopened.engine.overflow_regions(), before, "keep {keep}");
                    outcomes[0] += 1;
                } else {
                    assert!(bottom.iter().all(|v| *v == Some(3)), "{bottom:?}");
                    assert_eq!(reopened.engine.overflow_regions(), after, "keep {keep}");
                    outcomes[1] += 1;
                }
            }
        }
    }
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "{outcomes:?}");
}

/// A checkpoint of every publication copies a range once when later
/// publications of the same checkpoint rewrite it whole: the newest value
/// lands, and the older copies are not written at all.
#[test]
fn a_checkpoint_copies_a_rewritten_range_once() {
    let mut store = MemStore::new();
    let mut w = TileWriter::create(&mut store, geometry(false), 1 << 20, [7; 16]).unwrap();
    let key = MatrixKey::new(3, 5);
    // The tile and its directory entry first, in place.
    put(&mut w, &mut store, key, 1);
    w.engine.publish(&mut store).unwrap();
    w.engine.checkpoint_all(&mut store).unwrap();
    for generation in 2..=20u64 {
        put(&mut w, &mut store, key, generation);
        w.engine.publish(&mut store).unwrap();
    }
    let before = w.engine.stats.in_place_writes;
    w.engine.checkpoint_all(&mut store).unwrap();
    assert_eq!(w.engine.stats.in_place_writes - before, 1);
    assert_eq!(mine(&mut w, &mut store, key), Some(20));
    let mut r = TileReader::open(&store, geometry(false), w.meta).unwrap();
    assert_eq!(seen(&mut r, &store, key).unwrap(), Some(20));
}
