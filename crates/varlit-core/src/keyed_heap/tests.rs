use std::collections::BTreeMap;

use super::heap::{KeyedHeap, Tuning};
use super::{REGION_BYTES, SLOTS_PER_PAGE};
use crate::matrix_tiles::store::memory::MemStore;

const RING: u64 = 1 << 20;
const SEED: [u64; 2] = [0x1234_5678, 0x9abc_def0];
const BUDGET: u64 = super::DEFAULT_BUDGET;

type Model = BTreeMap<(u32, Vec<u8>), Vec<u8>>;

fn get(heap: &KeyedHeap, store: &MemStore, block: u32, key: &[u8]) -> Option<Vec<u8>> {
    let found = heap.find(store, block, key).unwrap()?;
    let mut out = vec![0u8; found.chunk.payload_len as usize];
    heap.read_payload(store, &found, &mut out).unwrap();
    Some(out)
}

fn check(heap: &KeyedHeap, store: &MemStore, model: &Model) {
    for ((block, key), value) in model {
        assert_eq!(
            get(heap, store, *block, key).as_deref(),
            Some(&value[..]),
            "key {key:?} of block {block}"
        );
    }
    assert_eq!(heap.state().keys, model.len() as u64);
}

fn random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn value(len: usize, fill: u64) -> Vec<u8> {
    (0..len)
        .map(|index| (fill as usize ^ index) as u8)
        .collect()
}

#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn puts_deletes_and_reopens_match_a_model_across_splits() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, BUDGET, SEED, Tuning::DEFAULT).unwrap();
    let mut model = Model::new();
    let mut state = 7u64;
    for step in 0..6000u64 {
        let block = (random(&mut state) % 2) as u32 + 1;
        let key = format!("key-{}", random(&mut state) % 1500).into_bytes();
        match random(&mut state) % 10 {
            0..=6 => {
                let len = [0usize, 1, 15, 16, 100, 700, 3000][(random(&mut state) % 7) as usize];
                let bytes = value(len, step);
                heap.put(&mut store, block, &key, &bytes).unwrap();
                model.insert((block, key), bytes);
            }
            _ => {
                let removed = heap.delete(&mut store, block, &key).unwrap();
                assert_eq!(removed, model.remove(&(block, key)).is_some());
            }
        }
        if step % 97 == 0 {
            heap.publish(&mut store).unwrap();
        }
        if step % 1000 == 999 {
            heap.publish(&mut store).unwrap();
            heap = KeyedHeap::open(&mut store, heap.meta, Tuning::DEFAULT).unwrap();
            check(&heap, &store, &model);
        }
    }
    check(&heap, &store, &model);
    assert!(heap.state().buckets() > 1, "the table never split");
    assert!(heap.state().buckets() * SLOTS_PER_PAGE as u64 >= heap.state().keys);
    for (block, key) in model.keys() {
        assert!(heap.find(&store, *block + 10, key).unwrap().is_none());
    }
}

/// The advisory's workload: updating the same keys over and over must not
/// grow the file with the number of updates.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn repeated_updates_keep_the_file_within_the_ratio() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, BUDGET, SEED, Tuning::DEFAULT).unwrap();
    let keys = 2000u64;
    let mut state = 3u64;
    let mut live_target = 0u64;
    for round in 0..40u64 {
        for key in 0..keys {
            // Sizes vary, so values move and leave holes of every size.
            let len = 64 + (random(&mut state) % 2000) as usize;
            let bytes = value(len, round ^ key);
            heap.put(&mut store, 1, &key.to_le_bytes(), &bytes).unwrap();
            assert_bounded(&heap, &format!("round {round} key {key}"));
        }
        heap.publish(&mut store).unwrap();
        if let Err(error) = heap.check_consistency(&store) {
            panic!("round {round}: {error}: {:?}", heap.stats);
        }
        if round == 0 {
            live_target = heap.state().live;
        }
        // Every round replaces every value, so the heap holds the live
        // values, the ones they replaced (retaken after a checkpoint), holes
        // within the ratio and a few regions of slack, round after round.
        let state = heap.state();
        assert!(
            state.heap <= 2 * state.live + state.live / 10 + 4 * REGION_BYTES,
            "round {round}: heap {} for live {}: {:?}",
            state.heap,
            state.live,
            heap.stats
        );
    }
    let state = heap.state();
    // Everything allocated is live data, holes within the ratio, free
    // regions waiting for reuse, the current region's tail, and the space
    // of the values one publication replaced, retaken after the next
    // checkpoint: here every round replaces all of them.
    assert!(
        state.heap <= 2 * state.live + state.live / 10 + 4 * REGION_BYTES,
        "heap {} for live {} after 40 rounds: {:?}",
        state.heap,
        state.live,
        heap.stats
    );
    assert!(heap.stats.regions_compacted > 0, "{:?}", heap.stats);
    // 40 rounds of updates, but the file holds about one round of data.
    let len = store.len();
    assert!(
        len < 3 * live_target + 8 * REGION_BYTES + RING,
        "file {len} bytes for about {live_target} live"
    );
}

#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn deleting_everything_returns_the_space_for_reuse() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, BUDGET, SEED, Tuning::DEFAULT).unwrap();
    for cycle in 0..10u64 {
        for key in 0..1000u64 {
            heap.put(&mut store, 1, &key.to_le_bytes(), &value(1000, cycle))
                .unwrap();
        }
        heap.publish(&mut store).unwrap();
        for key in 0..1000u64 {
            assert!(heap.delete(&mut store, 1, &key.to_le_bytes()).unwrap());
        }
        heap.publish(&mut store).unwrap();
        assert_eq!(heap.state().keys, 0);
        assert_eq!(heap.state().live, 0);
    }
    assert!(
        heap.state().heap <= 6 * REGION_BYTES,
        "{} heap bytes for one cycle of about 1 MiB: {:?}",
        heap.state().heap,
        heap.stats
    );
}

/// A crash at any write after a publication leaves exactly the published
/// state once the writer reopens.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn a_crash_after_any_write_keeps_the_last_publication() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, BUDGET, SEED, Tuning::DEFAULT).unwrap();
    let mut model = Model::new();
    for key in 0..300u64 {
        let bytes = value(50 + key as usize % 300, key);
        heap.put(&mut store, 1, &key.to_le_bytes(), &bytes).unwrap();
        model.insert((1, key.to_le_bytes().to_vec()), bytes);
    }
    heap.publish(&mut store).unwrap();
    let meta = heap.meta;
    // A second generation that moves, deletes and splits.
    for key in 0..400u64 {
        if key % 3 == 0 {
            heap.delete(&mut store, 1, &key.to_le_bytes()).unwrap();
        } else {
            heap.put(&mut store, 1, &key.to_le_bytes(), &value(900, key + 1))
                .unwrap();
        }
    }
    let pending = store.writes_since_sync();
    assert!(pending > 0);
    for keep in (0..pending).step_by((pending / 40).max(1)) {
        for torn in [0usize, 5] {
            let crashed = store.crashed(keep, torn);
            let mut reopened = crashed.clone();
            let heap = KeyedHeap::open(&mut reopened, meta, Tuning::DEFAULT).unwrap();
            check(&heap, &reopened, &model);
            heap.check_consistency(&reopened).unwrap();
            assert!(
                heap.find(&reopened, 1, &399u64.to_le_bytes())
                    .unwrap()
                    .is_none()
            );
        }
    }
}

/// Whether `heap` shows exactly `model`.
fn shows(heap: &KeyedHeap, store: &MemStore, model: &Model) -> bool {
    heap.state().keys == model.len() as u64
        && model.iter().all(|((block, key), value)| {
            let Ok(Some(found)) = heap.find(store, *block, key) else {
                return false;
            };
            let mut out = vec![0u8; found.chunk.payload_len as usize];
            heap.read_payload(store, &found, &mut out).is_ok() && out == *value
        })
}

/// New values written straight into space freed by generations already in
/// place, never through the ring: a crash before their publication keeps
/// the last one, and a crash during or after it shows one of the two
/// generations whole.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn values_written_straight_into_reused_space_survive_a_crash() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, BUDGET, SEED, Tuning::DEFAULT).unwrap();
    let meta = heap.meta;
    let mut model = Model::new();
    for round in 0..3u64 {
        for key in 0..200u64 {
            let bytes = value(300, key ^ (round << 8));
            heap.put(&mut store, 1, &key.to_le_bytes(), &bytes).unwrap();
            model.insert((1, key.to_le_bytes().to_vec()), bytes);
        }
        heap.publish(&mut store).unwrap();
        heap.ring.checkpoint(&mut store, u64::MAX).unwrap();
    }
    let (direct, reused) = (heap.stats.direct_writes, heap.stats.holes_reused);
    let mut next = model.clone();
    for key in 0..200u64 {
        let bytes = value(300, key ^ 0xabcd);
        heap.put(&mut store, 1, &key.to_le_bytes(), &bytes).unwrap();
        next.insert((1, key.to_le_bytes().to_vec()), bytes);
    }
    assert!(heap.stats.holes_reused > reused, "{:?}", heap.stats);
    assert!(heap.stats.direct_writes > direct, "{:?}", heap.stats);
    let pending = store.writes_since_sync();
    for keep in (0..pending).step_by((pending / 40).max(1)) {
        for torn in [0usize, 5] {
            let mut crashed = store.crashed(keep, torn);
            let reopened = KeyedHeap::open(&mut crashed, meta, Tuning::DEFAULT).unwrap();
            assert!(shows(&reopened, &crashed, &model), "crash at write {keep}");
            reopened.check_consistency(&crashed).unwrap();
        }
    }
    heap.publish(&mut store).unwrap();
    let pending = store.writes_since_sync();
    for keep in 0..=pending {
        for torn in [0usize, 5] {
            let mut crashed = store.crashed(keep, torn);
            let reopened = KeyedHeap::open(&mut crashed, meta, Tuning::DEFAULT).unwrap();
            assert!(
                shows(&reopened, &crashed, &model) || shows(&reopened, &crashed, &next),
                "crash at publication write {keep}"
            );
            reopened.check_consistency(&crashed).unwrap();
        }
    }
    let mut reopened = store.clone();
    let heap = KeyedHeap::open(&mut reopened, meta, Tuning::DEFAULT).unwrap();
    check(&heap, &reopened, &next);
}

#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn values_larger_than_half_a_region_get_their_own() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, BUDGET, SEED, Tuning::DEFAULT).unwrap();
    let big = value(3 * REGION_BYTES as usize / 2, 9);
    heap.put(&mut store, 1, b"big", &big).unwrap();
    heap.put(&mut store, 1, b"small", b"x").unwrap();
    heap.publish(&mut store).unwrap();
    assert_eq!(get(&heap, &store, 1, b"big").as_deref(), Some(&big[..]));
    heap.put(&mut store, 1, b"big", b"now small").unwrap();
    heap.publish(&mut store).unwrap();
    assert_eq!(
        get(&heap, &store, 1, b"big").as_deref(),
        Some(&b"now small"[..])
    );
    let reopened = KeyedHeap::open(&mut store, heap.meta, Tuning::DEFAULT).unwrap();
    assert_eq!(
        get(&reopened, &store, 1, b"small").as_deref(),
        Some(&b"x"[..])
    );
    assert!(store.len() >= big.len() as u64);
}

fn read(
    reader: &mut super::reader::KeyedReader,
    store: &MemStore,
    key: &[u8],
) -> crate::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let found = reader.get(store, 1, &mut out, 0, u64::MAX, &mut |buf| {
        buf.clear();
        buf.extend_from_slice(key);
        Ok(())
    })?;
    Ok(found.then_some(out))
}

#[test]
fn a_reader_keeps_its_generation_until_it_follows() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, BUDGET, SEED, Tuning::DEFAULT).unwrap();
    heap.put(&mut store, 1, b"k", b"one").unwrap();
    heap.publish(&mut store).unwrap();
    let mut reader = super::reader::KeyedReader::open(&store, heap.meta).unwrap();
    heap.put(&mut store, 1, b"k", b"two, longer").unwrap();
    heap.put(&mut store, 1, b"new", b"x").unwrap();
    // Staged, then published: neither reaches a reader that has not followed.
    assert_eq!(
        read(&mut reader, &store, b"k").unwrap().as_deref(),
        Some(&b"one"[..])
    );
    heap.publish(&mut store).unwrap();
    assert_eq!(
        read(&mut reader, &store, b"k").unwrap().as_deref(),
        Some(&b"one"[..])
    );
    assert_eq!(read(&mut reader, &store, b"new").unwrap(), None);
    assert!(reader.follow(&store).unwrap());
    assert_eq!(
        read(&mut reader, &store, b"k").unwrap().as_deref(),
        Some(&b"two, longer"[..])
    );
    assert_eq!(
        read(&mut reader, &store, b"new").unwrap().as_deref(),
        Some(&b"x"[..])
    );
}

#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn a_reader_the_ring_moved_past_expires_instead_of_mixing_generations() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, BUDGET, SEED, Tuning::DEFAULT).unwrap();
    heap.put(&mut store, 1, b"k", &value(100, 0)).unwrap();
    heap.publish(&mut store).unwrap();
    let mut reader = super::reader::KeyedReader::open(&store, heap.meta).unwrap();
    // Enough publications to wrap the ring and checkpoint past the reader.
    for round in 1..200u64 {
        for key in 0..20u64 {
            heap.put(&mut store, 1, &key.to_le_bytes(), &value(2000, round))
                .unwrap();
        }
        heap.put(&mut store, 1, b"k", &value(100, round)).unwrap();
        heap.publish(&mut store).unwrap();
    }
    match read(&mut reader, &store, b"k") {
        Err(crate::Error::MatrixSnapshotExpired { .. }) => {}
        other => panic!("a reader past the ring must expire: {other:?}"),
    }
    reader.follow(&store).unwrap();
    assert_eq!(
        read(&mut reader, &store, b"k").unwrap().as_deref(),
        Some(&value(100, 199)[..])
    );
}

/// A reader whose snapshot points at space the writer has since freed,
/// put in place and written new values into straight, never reads those
/// values as its own: it reads its own values or expires.
#[test]
fn a_reader_never_sees_values_written_into_space_it_points_at() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, BUDGET, SEED, Tuning::DEFAULT).unwrap();
    for key in 0..50u64 {
        heap.put(&mut store, 1, &key.to_le_bytes(), &value(200, key))
            .unwrap();
    }
    heap.publish(&mut store).unwrap();
    let mut reader = super::reader::KeyedReader::open(&store, heap.meta).unwrap();
    for round in 1..4u64 {
        for key in 0..50u64 {
            heap.put(
                &mut store,
                1,
                &key.to_le_bytes(),
                &value(200, key ^ (round << 8)),
            )
            .unwrap();
        }
        heap.publish(&mut store).unwrap();
        heap.ring.checkpoint(&mut store, u64::MAX).unwrap();
    }
    assert!(heap.stats.holes_reused > 0, "{:?}", heap.stats);
    for key in 0..50u64 {
        match read(&mut reader, &store, &key.to_le_bytes()) {
            Ok(Some(found)) => assert_eq!(found, value(200, key)),
            Err(crate::Error::MatrixSnapshotExpired { .. }) => {}
            other => panic!("key {key}: {other:?}"),
        }
    }
}

/// The layouts are little-endian whatever the platform: these bytes are what
/// any build writes and reads.
#[test]
fn layouts_are_the_same_bytes_on_every_platform() {
    use super::{Chunk, Region, Slot, State};
    let slot = Slot {
        route: 0x0403_0201,
        offset: 0x0807_0605,
        region: 0x100f_0e0d_0c0b_0a09,
    };
    assert_eq!(
        slot.encode(),
        [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
    );
    let chunk = Chunk {
        payload_len: 0x0403_0201,
        block_id: 7,
        key_len: 2,
        crc: 0xdead_beef,
    };
    assert_eq!(
        chunk.encode(),
        [1, 2, 3, 4, 7, 0, 0, 0, 2, 0, 0, 0, 0xef, 0xbe, 0xad, 0xde]
    );
    let region = Region {
        len: 1 << 20,
        live: 48,
        next_all: 0,
        next_free: 0,
        freed: 3,
    };
    let bytes = region.encode();
    assert_eq!(&bytes[..4], b"VHRG");
    assert_eq!(&bytes[8..16], &(1u64 << 20).to_le_bytes());
    assert_eq!(Region::decode(&bytes).unwrap(), region);
    let state = State {
        dir: (4096, 1),
        level: 2,
        split: 1,
        keys: 9,
        ..State::default()
    };
    let raw = state.encode();
    assert_eq!(&raw[..8], &4096u64.to_le_bytes());
    assert_eq!(State::decode(&raw).unwrap(), state);
    assert_eq!(state.buckets(), 5);
    // Route 0b101: low two bits 0b01 < split 1? no; bucket 1. Route 0b100:
    // low bits 0 < split, so the next level decides: bucket 4.
    assert_eq!(state.bucket_of(0b101), 1);
    assert_eq!(state.bucket_of(0b100), 4);
    assert_eq!(state.bucket_of(0b000), 0);
}

/// A small model run cheap enough for an interpreter.
#[test]
fn a_few_puts_and_deletes_match_a_model() {
    let mut store = MemStore::new();
    let mut heap =
        KeyedHeap::create(&mut store, 70 * 1024, 50, BUDGET, SEED, Tuning::DEFAULT).unwrap();
    let mut model = Model::new();
    for step in 0..60u64 {
        let key = (step % 17).to_le_bytes().to_vec();
        if step % 5 == 4 {
            assert_eq!(
                heap.delete(&mut store, 1, &key).unwrap(),
                model.remove(&(1, key)).is_some()
            );
        } else {
            let bytes = value((step * 7 % 90) as usize, step);
            heap.put(&mut store, 1, &key, &bytes).unwrap();
            model.insert((1, key), bytes);
        }
        if step % 20 == 19 {
            heap.publish(&mut store).unwrap();
            heap = KeyedHeap::open(&mut store, heap.meta, Tuning::DEFAULT).unwrap();
        }
    }
    check(&heap, &store, &model);
}

/// The writer's fixed structures, as small as they go: what overflows them
/// in a large file overflows them here.
const TINY: Tuning = Tuning { hole_cache: 4 };

fn assert_bounded(heap: &KeyedHeap, when: &str) {
    assert!(
        heap.idle() <= heap.idle_bound(),
        "{when}: idle {} over the bound {} (live {}, heap {}): {:?}",
        heap.idle(),
        heap.idle_bound(),
        heap.state().live,
        heap.state().heap,
        heap.stats
    );
}

/// Every value rewritten in one publication, many more of them than the
/// hole cache holds, round after round: the idle space stays within the
/// bound after every put, not only on average.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn bulk_rewrites_past_the_hole_cache_stay_within_the_bound() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, super::MIN_BUDGET, SEED, TINY).unwrap();
    // Ten times the bound's constant part, so the bound is about the data.
    for round in 0..8u64 {
        for key in 0..20_000u64 {
            heap.put(&mut store, 1, &key.to_le_bytes(), &value(500, key ^ round))
                .unwrap();
            assert_bounded(&heap, &format!("round {round} key {key}"));
        }
        heap.publish(&mut store).unwrap();
    }
}

/// Random puts, deletes, publications and reopens over a few hundred keys,
/// with every fixed structure as small as it goes: after every operation the
/// heap is consistent, every key reads back, and the idle space stays within
/// the bound over the most data the heap ever held (returning space after
/// deletes is not done yet).
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn random_histories_stay_consistent_and_bounded() {
    for seed in 1..=4u64 {
        let mut store = MemStore::new();
        let mut heap =
            KeyedHeap::create(&mut store, RING, 50, super::MIN_BUDGET, SEED, TINY).unwrap();
        let meta = heap.meta;
        let mut model = Model::new();
        let mut published = Model::new();
        let mut state = seed;
        let mut most_live = 0u64;
        for step in 0..3000u32 {
            let roll = random(&mut state) % 100;
            let key = (random(&mut state) % 300).to_le_bytes().to_vec();
            if roll < 70 {
                let len = match random(&mut state) % 10 {
                    0 => 40_000,
                    1..=3 => 4_000,
                    _ => 1 + (random(&mut state) % 600) as usize,
                };
                let bytes = value(len, random(&mut state));
                let generation = heap.ring.published();
                heap.put(&mut store, 1, &key, &bytes).unwrap();
                // A put that ends a round publishes what came before it.
                if heap.ring.published() != generation {
                    published = model.clone();
                }
                model.insert((1, key), bytes);
            } else if roll < 88 {
                let removed = heap.delete(&mut store, 1, &key).unwrap();
                let expected = model.remove(&(1, key.clone())).is_some();
                assert_eq!(
                    removed, expected,
                    "seed {seed} step {step}: delete of {key:?}: {:?}",
                    heap.stats
                );
            } else if roll < 97 {
                heap.publish(&mut store).unwrap();
                published = model.clone();
            } else {
                // Unpublished changes are lost; a reopened writer starts from
                // the last publication with empty caches.
                heap = KeyedHeap::open(&mut store, meta, TINY).unwrap();
                model = published.clone();
            }
            most_live = most_live.max(heap.state().live);
            if step % 10 == 0
                && let Err(error) = heap.check_consistency(&store)
            {
                panic!("seed {seed} step {step}: {error}: {:?}", heap.stats);
            }
            assert!(
                heap.state().heap <= most_live + heap.idle_bound(),
                "seed {seed} step {step}: heap {} for most live {most_live} (bound {}): {:?}",
                heap.state().heap,
                heap.idle_bound(),
                heap.stats
            );
        }
        check(&heap, &store, &model);
    }
}

/// Updates of random keys with random sizes, many more than the hole cache
/// holds, in rounds of the smallest budget: holes are scattered and of
/// every size, so walks and compaction must keep up. The idle space stays
/// within the bound after every put.
#[test]
#[ignore = "fails: with holes of every size and a cache that cannot hold them, \
            the writer does not find holes that fit and the idle space grows; \
            see docs/design/bounded-keyed-storage.md (Bounds at the extremes)"]
fn scattered_updates_past_the_hole_cache_stay_within_the_bound() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, super::MIN_BUDGET, SEED, TINY).unwrap();
    let mut state = 7u64;
    for key in 0..20_000u64 {
        let len = 64 + (random(&mut state) % 2000) as usize;
        heap.put(&mut store, 1, &key.to_le_bytes(), &value(len, key))
            .unwrap();
    }
    heap.publish(&mut store).unwrap();
    for step in 0..200_000u64 {
        let key = random(&mut state) % 20_000;
        let len = 64 + (random(&mut state) % 2000) as usize;
        heap.put(&mut store, 1, &key.to_le_bytes(), &value(len, step))
            .unwrap();
        assert_bounded(&heap, &format!("step {step}"));
        if step % 500 == 0 {
            heap.publish(&mut store).unwrap();
        }
    }
    heap.check_consistency(&store).unwrap();
}
