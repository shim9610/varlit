use std::collections::BTreeMap;

use super::heap::KeyedHeap;
use super::{REGION_BYTES, SLOTS_PER_PAGE};
use crate::matrix_tiles::store::memory::MemStore;

const RING: u64 = 1 << 20;
const SEED: [u64; 2] = [0x1234_5678, 0x9abc_def0];

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
    let mut heap = KeyedHeap::create(&mut store, RING, 50, SEED).unwrap();
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
            heap = KeyedHeap::open(&mut store, heap.meta).unwrap();
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
    let mut heap = KeyedHeap::create(&mut store, RING, 50, SEED).unwrap();
    let keys = 2000u64;
    let mut state = 3u64;
    let mut live_target = 0u64;
    for round in 0..40u64 {
        for key in 0..keys {
            // Sizes vary, so values move and leave holes of every size.
            let len = 64 + (random(&mut state) % 2000) as usize;
            let bytes = value(len, round ^ key);
            heap.put(&mut store, 1, &key.to_le_bytes(), &bytes).unwrap();
        }
        heap.publish(&mut store).unwrap();
        if round == 0 {
            live_target = heap.state().live;
        }
        let holes = heap.holes();
        assert!(
            holes <= (heap.state().live / 1000 * 50).max(REGION_BYTES) + REGION_BYTES,
            "round {round}: holes {holes} for live {}",
            heap.state().live
        );
    }
    let state = heap.state();
    // Everything allocated is live data, holes within the ratio, free
    // regions waiting for reuse and the current region's tail.
    assert!(
        state.heap <= state.live + state.live / 10 + 4 * REGION_BYTES,
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
    let mut heap = KeyedHeap::create(&mut store, RING, 50, SEED).unwrap();
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
    let mut heap = KeyedHeap::create(&mut store, RING, 50, SEED).unwrap();
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
            let heap = KeyedHeap::open(&mut reopened, meta).unwrap();
            check(&heap, &reopened, &model);
            assert!(
                heap.find(&reopened, 1, &399u64.to_le_bytes())
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn values_larger_than_half_a_region_get_their_own() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, SEED).unwrap();
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
    let reopened = KeyedHeap::open(&mut store, heap.meta).unwrap();
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
    let Some(found) = reader.find(store, 1, key)? else {
        return Ok(None);
    };
    let mut out = vec![0u8; found.chunk.payload_len as usize];
    reader.read_payload(store, &found, &mut out)?;
    Ok(Some(out))
}

#[test]
fn a_reader_keeps_its_generation_until_it_follows() {
    let mut store = MemStore::new();
    let mut heap = KeyedHeap::create(&mut store, RING, 50, SEED).unwrap();
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
    let mut heap = KeyedHeap::create(&mut store, RING, 50, SEED).unwrap();
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
    let mut heap = KeyedHeap::create(&mut store, 70 * 1024, 50, SEED).unwrap();
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
            heap = KeyedHeap::open(&mut store, heap.meta).unwrap();
        }
    }
    check(&heap, &store, &model);
}
