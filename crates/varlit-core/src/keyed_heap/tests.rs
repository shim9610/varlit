use std::collections::BTreeMap;

use super::heap::{KeyedHeap, Tuning};
use super::{
    CLASSES, ITEM_HEADER, MAX_ITEM, MIN_BUDGET, PAGE_BYTES, chain_bytes,
    chain_shape, class_of, class_size,
};
use crate::matrix_tiles::store::memory::MemStore;

const RING: u64 = 1 << 20;
const BUDGET: u64 = super::DEFAULT_BUDGET;
/// The writer's own work turned down: holes build up as in a large file.
const SLOW: Tuning = Tuning { fills_per_free: 1 };

type Model = BTreeMap<(u32, Vec<u8>), Vec<u8>>;

fn create(store: &mut MemStore, budget: u64, tuning: Tuning) -> KeyedHeap {
    KeyedHeap::create(store, RING, 50, budget, tuning).unwrap()
}

fn get(heap: &KeyedHeap, store: &MemStore, block: u32, key: &[u8]) -> Option<Vec<u8>> {
    let found = heap.find(store, block, key).unwrap()?;
    let mut out = vec![0u8; found.payload_len() as usize];
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

/// Whether `heap` shows exactly `model`.
fn shows(heap: &KeyedHeap, store: &MemStore, model: &Model) -> bool {
    heap.state().keys == model.len() as u64
        && model.iter().all(|((block, key), value)| {
            let Ok(Some(found)) = heap.find(store, *block, key) else {
                return false;
            };
            let mut out = vec![0u8; found.payload_len() as usize];
            heap.read_payload(store, &found, &mut out).is_ok() && out == *value
        })
}

fn consistent(heap: &KeyedHeap, store: &MemStore, when: &str) {
    if let Err(error) = heap.check_consistency(store) {
        panic!("{when}: {error}: {:?}", heap.stats);
    }
}

/// The pages in use (all but pooled ones) hold the live bytes, the logged
/// items, a partial page per class and the log, nothing more; and the
/// logged items stay within their bound. `largest` is the most bytes one
/// record of the test takes.
fn bounded(heap: &KeyedHeap, largest: u64, when: &str) {
    let state = heap.state();
    let used = (state.pages - state.pooled) * PAGE_BYTES;
    assert!(
        used <= heap.live_bytes() + heap.idle_bound(),
        "{when}: {used} bytes of pages in use for {} live (bound {}): {:?}",
        heap.live_bytes(),
        heap.idle_bound(),
        heap.stats
    );
    assert!(
        state.hole_bytes <= heap.logged_bound(largest),
        "{when}: {} bytes logged (bound {}): {:?}",
        state.hole_bytes,
        heap.logged_bound(largest),
        heap.stats
    );
    assert!(
        state.leaf_free <= heap.leaf_free_bound(),
        "{when}: {} bytes free in {} leaves (bound {}): {:?}",
        state.leaf_free,
        state.leaves,
        heap.leaf_free_bound(),
        heap.stats
    );
    assert!(
        state.interiors <= state.leaves / 8 + state.root.1,
        "{when}: {} interior pages for {} leaves",
        state.interiors,
        state.leaves
    );
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
fn classes_are_sixteen_byte_steps_then_a_sixteenth_apart() {
    const { assert!(CLASSES > 90 && CLASSES < 128) };
    assert_eq!(class_size(0), 48);
    assert_eq!(class_size(CLASSES - 1), MAX_ITEM);
    for class in 1..CLASSES {
        let (small, large) = (class_size(class - 1), class_size(class));
        assert!(large > small && large.is_multiple_of(16));
        // Rounding to a class wastes at most a sixteenth, or 16 bytes.
        assert!(large <= (small * 17 / 16).next_multiple_of(16).max(small + 16));
    }
    for len in [1, 48, 49, 255, 256, 257, 1000, 4097, MAX_ITEM - 1, MAX_ITEM] {
        let class = class_of(len).unwrap();
        assert!(class_size(class) >= len);
        assert!(class == 0 || class_size(class - 1) < len);
    }
    assert_eq!(class_of(MAX_ITEM + 1), None);
}

#[test]
fn records_split_into_full_items_and_one_that_fits() {
    let per_full = MAX_ITEM - ITEM_HEADER;
    assert_eq!(chain_shape(1).0, 0);
    assert_eq!(chain_shape(per_full).0, 0);
    assert_eq!(chain_shape(per_full + 1).0, 1);
    for total in [
        1u64,
        100,
        per_full,
        per_full + 1,
        5 * per_full,
        5 * per_full + 7,
        1 << 30,
    ] {
        let (full, last) = chain_shape(total);
        let rest = total - full * per_full;
        assert!(
            rest >= 1 && rest + ITEM_HEADER <= class_size(last),
            "{total}"
        );
        assert_eq!(chain_bytes(total), full * MAX_ITEM + class_size(last));
    }
}

#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn puts_deletes_and_reopens_match_a_model_across_splits() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
    let mut model = Model::new();
    let mut state = 1u64;
    // Enough keys to split leaves and interior pages, values of every size
    // class, inline and chained.
    for round in 0..4u64 {
        for step in 0..1500u64 {
            let key = (random(&mut state) % 3000).to_le_bytes().to_vec();
            let block = 1 + (random(&mut state) % 2) as u32;
            if random(&mut state).is_multiple_of(5) {
                let removed = heap.delete(&mut store, block, &key).unwrap();
                assert_eq!(removed, model.remove(&(block, key)).is_some());
            } else {
                let len = (random(&mut state) % 3000) as usize;
                let bytes = value(len, round * 10_000 + step);
                heap.put(&mut store, block, &key, &bytes).unwrap();
                model.insert((block, key), bytes);
            }
        }
        heap.publish(&mut store).unwrap();
        if round % 2 == 1 {
            heap = KeyedHeap::open(&mut store, heap.meta, Tuning::DEFAULT).unwrap();
        }
        check(&heap, &store, &model);
        consistent(&heap, &store, &format!("round {round}"));
    }
    assert!(heap.state().leaves > 4, "{:?}", heap.state());
}

#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn values_of_any_length_round_trip_as_chains() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
    let lengths = [0usize, 1, 7, 8000, 65_496, 65_497, 200_000, 3 << 20];
    for (index, &len) in lengths.iter().enumerate() {
        heap.put(&mut store, 1, &[index as u8], &value(len, index as u64))
            .unwrap();
    }
    // A key longer than one item.
    let long_key = value(150_000, 3);
    heap.put(&mut store, 2, &long_key, b"long key").unwrap();
    heap.publish(&mut store).unwrap();
    let reopened = KeyedHeap::open(&mut store, heap.meta, Tuning::DEFAULT).unwrap();
    for (index, &len) in lengths.iter().enumerate() {
        assert_eq!(
            get(&reopened, &store, 1, &[index as u8]),
            Some(value(len, index as u64)),
            "length {len}"
        );
    }
    assert_eq!(
        get(&reopened, &store, 2, &long_key).as_deref(),
        Some(&b"long key"[..])
    );
    // Shorter, longer, gone: chains of every shape are replaced and freed.
    heap.put(&mut store, 1, &[7], b"short now").unwrap();
    heap.put(&mut store, 1, &[0], &value(300_000, 1)).unwrap();
    assert!(heap.delete(&mut store, 2, &long_key).unwrap());
    heap.publish(&mut store).unwrap();
    assert_eq!(
        get(&heap, &store, 1, &[7]).as_deref(),
        Some(&b"short now"[..])
    );
    assert_eq!(get(&heap, &store, 1, &[0]), Some(value(300_000, 1)));
    assert_eq!(get(&heap, &store, 2, &long_key), None);
    consistent(&heap, &store, "after replacing chains");
}

/// Every value rewritten in one publication, round after round: the pages
/// stay within the bound after every put.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn bulk_rewrites_stay_within_the_bound() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, MIN_BUDGET, SLOW);
    for round in 0..8u64 {
        for key in 0..20_000u64 {
            heap.put(&mut store, 1, &key.to_le_bytes(), &value(500, key ^ round))
                .unwrap();
            bounded(&heap, chain_bytes(508), &format!("round {round} key {key}"));
        }
        heap.publish(&mut store).unwrap();
    }
    consistent(&heap, &store, "after the rounds");
}

/// Random keys with random sizes, round after round of the smallest budget:
/// holes of every size class, scattered. The pages stay within the bound
/// after every put.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn scattered_updates_stay_within_the_bound() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, MIN_BUDGET, SLOW);
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
        bounded(&heap, chain_bytes(2072), &format!("step {step}"));
        if step % 500 == 0 {
            heap.publish(&mut store).unwrap();
        }
    }
    consistent(&heap, &store, "after the updates");
}

/// Space a class gives up goes to the pool and is taken by other classes:
/// values that change size do not grow the file.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn space_moves_between_classes() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, MIN_BUDGET, Tuning::DEFAULT);
    for round in 0..12u64 {
        // Every round, every value in a different size class.
        let len = 100 + (round as usize % 6) * 700;
        for key in 0..4000u64 {
            heap.put(&mut store, 1, &key.to_le_bytes(), &value(len, round))
                .unwrap();
            bounded(
                &heap,
                chain_bytes(3608),
                &format!("round {round} key {key}"),
            );
        }
        heap.publish(&mut store).unwrap();
    }
    assert!(heap.stats.pages_reused > 0, "{:?}", heap.stats);
    consistent(&heap, &store, "after the rounds");
}

/// Deleting every key shrinks every class to nothing once the mark passes
/// the deletes; their pages wait in the pool.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn deleting_everything_empties_every_class() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, MIN_BUDGET, Tuning::DEFAULT);
    for key in 0..3000u64 {
        heap.put(&mut store, 1, &key.to_le_bytes(), &value(700, key))
            .unwrap();
    }
    heap.publish(&mut store).unwrap();
    for key in 0..3000u64 {
        assert!(heap.delete(&mut store, 1, &key.to_le_bytes()).unwrap());
    }
    heap.publish(&mut store).unwrap();
    // Each operation takes a little more than it frees; more operations
    // drive the rest.
    for round in 0..20_000u64 {
        heap.put(&mut store, 2, b"drive", &value(10, round))
            .unwrap();
        if round % 100 == 0 {
            heap.publish(&mut store).unwrap();
        }
        if heap.class_end(class_of(ITEM_HEADER + 708).unwrap()) == 0 {
            break;
        }
    }
    assert_eq!(
        heap.class_end(class_of(ITEM_HEADER + 708).unwrap()),
        0,
        "{:?}",
        heap.stats
    );
    assert_eq!(heap.state().keys, 1);
    assert!(heap.state().pooled > 0);
    consistent(&heap, &store, "after deleting");
}

/// The space follows the data held now, not the most it ever held: one
/// class filled, nearly all of it deleted, another class filled. The pages
/// in use stay within the bound over the current data at every step, and
/// the second class takes the first's pages from the pool.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn space_follows_the_data_held_now_not_its_peak() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, MIN_BUDGET, SLOW);
    // 40 000 items of the smallest class: many pages of one class.
    for key in 0..40_000u64 {
        heap.put(&mut store, 1, &key.to_le_bytes(), b"").unwrap();
        bounded(&heap, chain_bytes(8), &format!("fill key {key}"));
    }
    heap.publish(&mut store).unwrap();
    let peak = heap.state().pages;
    for key in 0..40_000u64 {
        if key % 100 != 0 {
            assert!(heap.delete(&mut store, 1, &key.to_le_bytes()).unwrap());
        }
        bounded(&heap, chain_bytes(8), &format!("delete key {key}"));
    }
    heap.publish(&mut store).unwrap();
    // Another class, as much data as the first held.
    for key in 0..1000u64 {
        heap.put(&mut store, 2, &key.to_le_bytes(), &value(2000, key))
            .unwrap();
        bounded(&heap, chain_bytes(2008), &format!("refill key {key}"));
    }
    heap.publish(&mut store).unwrap();
    assert!(heap.stats.pages_reused > 0, "{:?}", heap.stats);
    assert!(
        heap.state().pages <= peak + 40,
        "{} pages, peak {peak}",
        heap.state().pages
    );
    consistent(&heap, &store, "after the refill");
}

/// A crash at any write after a publication leaves exactly the published
/// state once the writer reopens.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn a_crash_after_any_write_keeps_the_last_publication() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
    let mut model = Model::new();
    for key in 0..300u64 {
        let bytes = value(50 + key as usize % 300, key);
        heap.put(&mut store, 1, &key.to_le_bytes(), &bytes).unwrap();
        model.insert((1, key.to_le_bytes().to_vec()), bytes);
    }
    heap.publish(&mut store).unwrap();
    heap.ring.checkpoint(&mut store, u64::MAX).unwrap();
    let meta = heap.meta;
    // A second generation that moves, deletes, fills holes and splits.
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
            let mut crashed = store.crashed(keep, torn);
            let heap = KeyedHeap::open(&mut crashed, meta, Tuning::DEFAULT).unwrap();
            check(&heap, &crashed, &model);
            consistent(&heap, &crashed, &format!("crash at write {keep}"));
        }
    }
}

/// Values written straight into holes (taken by puts or filled by moves),
/// at every write of the generation and of its publication: a crash shows
/// one of the two generations whole.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn values_written_into_holes_survive_a_crash() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
    let meta = heap.meta;
    let mut model = Model::new();
    for key in 0..300u64 {
        let bytes = value(5000, key);
        heap.put(&mut store, 1, &key.to_le_bytes(), &bytes).unwrap();
        model.insert((1, key.to_le_bytes().to_vec()), bytes);
    }
    // Deletes in place leave holes the next generation fills with moves.
    for key in (0..300u64).step_by(2) {
        heap.delete(&mut store, 1, &key.to_le_bytes()).unwrap();
        model.remove(&(1, key.to_le_bytes().to_vec()));
    }
    heap.publish(&mut store).unwrap();
    heap.ring.checkpoint(&mut store, u64::MAX).unwrap();
    let moved = heap.stats.moves + heap.stats.taken;
    let mut next = model.clone();
    for key in (1..300u64).step_by(4) {
        let bytes = value(5000, key ^ 0xabcd);
        heap.put(&mut store, 1, &key.to_le_bytes(), &bytes).unwrap();
        next.insert((1, key.to_le_bytes().to_vec()), bytes);
    }
    assert!(
        heap.stats.moves + heap.stats.taken > moved,
        "{:?}",
        heap.stats
    );
    let pending = store.writes_since_sync();
    for keep in (0..pending).step_by((pending / 40).max(1)) {
        for torn in [0usize, 5] {
            let mut crashed = store.crashed(keep, torn);
            let reopened = KeyedHeap::open(&mut crashed, meta, Tuning::DEFAULT).unwrap();
            assert!(shows(&reopened, &crashed, &model), "crash at write {keep}");
            consistent(&reopened, &crashed, &format!("crash at write {keep}"));
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
            consistent(
                &reopened,
                &crashed,
                &format!("crash at publication write {keep}"),
            );
        }
    }
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
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
    heap.put(&mut store, 1, b"k", b"one").unwrap();
    heap.publish(&mut store).unwrap();
    let mut reader = super::reader::KeyedReader::open(&store, heap.meta).unwrap();
    heap.put(&mut store, 1, b"k", b"two, longer").unwrap();
    heap.put(&mut store, 1, b"new", b"x").unwrap();
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
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
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
    // Everything published in place: the mark passes the reader.
    heap.ring.checkpoint(&mut store, u64::MAX).unwrap();
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

/// A reader whose snapshot points at items the writer has since freed and
/// reused never reads others' values as its own: it reads its
/// own values or expires.
#[test]
fn a_reader_never_sees_values_moved_into_space_it_points_at() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
    for key in 0..50u64 {
        heap.put(&mut store, 1, &key.to_le_bytes(), &value(5000, key))
            .unwrap();
    }
    heap.publish(&mut store).unwrap();
    let mut reader = super::reader::KeyedReader::open(&store, heap.meta).unwrap();
    for round in 1..4u64 {
        for key in (0..50u64).step_by(round as usize + 1) {
            heap.delete(&mut store, 1, &key.to_le_bytes()).unwrap();
            heap.put(
                &mut store,
                1,
                &(key + 1000 * round).to_le_bytes(),
                &value(5000, round),
            )
            .unwrap();
        }
        heap.publish(&mut store).unwrap();
        heap.ring.checkpoint(&mut store, u64::MAX).unwrap();
    }
    assert!(
        heap.stats.moves > 0 || heap.stats.shrinks > 0,
        "{:?}",
        heap.stats
    );
    for key in 0..50u64 {
        match read(&mut reader, &store, &key.to_le_bytes()) {
            Ok(Some(found)) => assert_eq!(found, value(5000, key)),
            Err(crate::Error::MatrixSnapshotExpired { .. }) => {}
            other => panic!("key {key}: {other:?}"),
        }
    }
}

/// The layouts are little-endian whatever the platform: these bytes are what
/// any build writes and reads.
#[test]
fn layouts_are_the_same_bytes_on_every_platform() {
    use super::{ClassEntry, Item, State, item_ref};
    let item = Item {
        len: 0x0102,
        head: false,
        separator: true,
        block_or_route: 0x0a0b_0c0d,
        key_len: 0,
        crc: 0x1122_3344,
        dead: 7,
        next: super::NO_ITEM,
        total_or_prev: item_ref(1, 3),
    };
    let bytes = item.encode();
    assert_eq!(&bytes[..4], &[0x02, 0x01, 0, 0xc0]);
    assert_eq!(&bytes[4..8], &[0x0d, 0x0c, 0x0b, 0x0a]);
    assert_eq!(&bytes[12..16], &[0x44, 0x33, 0x22, 0x11]);
    assert_eq!(&bytes[16..24], &7u64.to_le_bytes());
    assert_eq!(&bytes[24..32], &[0xff; 8]);
    assert_eq!(&bytes[32..40], &[3, 0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(Item::decode(&bytes), item);
    let entry = ClassEntry {
        end: 5,
        dir: (0x1000, 1),
    };
    assert_eq!(&entry.encode()[..8], &5u64.to_le_bytes());
    assert_eq!(ClassEntry::decode(&entry.encode()).unwrap(), entry);
    let state = State {
        root: (1, 1),
        keys: 3,
        pages: 2,
        leaves: 1,
        logged: 9,
        ..State::default()
    };
    let encoded = state.encode();
    assert_eq!(&encoded[16..24], &3u64.to_le_bytes());
    assert_eq!(&encoded[96..104], &9u64.to_le_bytes());
    assert_eq!(State::decode(&encoded).unwrap(), state);
}

/// Random puts, deletes, publications and reopens over a few hundred keys,
/// with the writer's own work turned down: the heap is consistent, every key
/// reads back, and after every operation the pages in use stay within the
/// bound over the data the heap holds now.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn random_histories_stay_consistent_and_bounded() {
    for seed in 1..=4u64 {
        let mut store = MemStore::new();
        let mut heap = create(&mut store, MIN_BUDGET, SLOW);
        let meta = heap.meta;
        let mut model = Model::new();
        let mut published = Model::new();
        let mut state = seed;
        for step in 0..3000u32 {
            let roll = random(&mut state) % 100;
            let key = (random(&mut state) % 300).to_le_bytes().to_vec();
            if roll < 70 {
                let len = match random(&mut state) % 10 {
                    0 => 100_000,
                    1..=3 => 4_000,
                    _ => (random(&mut state) % 600) as usize,
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
                    "seed {seed} step {step}: delete of {key:?}"
                );
            } else if roll < 97 {
                heap.publish(&mut store).unwrap();
                published = model.clone();
            } else {
                // Unpublished changes are lost; a reopened writer starts
                // from the last publication.
                heap = KeyedHeap::open(&mut store, meta, SLOW).unwrap();
                model = published.clone();
            }
            if step % 10 == 0 {
                consistent(&heap, &store, &format!("seed {seed} step {step}"));
            }
            bounded(
                &heap,
                chain_bytes(100_008),
                &format!("seed {seed} step {step}"),
            );
        }
        check(&heap, &store, &model);
    }
}

#[test]
fn a_few_puts_and_deletes_match_a_model() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
    let mut model = Model::new();
    for (key, len) in [(1u64, 5usize), (2, 70), (3, 0), (1, 200), (4, 33)] {
        let bytes = value(len, key);
        heap.put(&mut store, 1, &key.to_le_bytes(), &bytes).unwrap();
        model.insert((1, key.to_le_bytes().to_vec()), bytes);
    }
    assert!(heap.delete(&mut store, 1, &2u64.to_le_bytes()).unwrap());
    model.remove(&(1, 2u64.to_le_bytes().to_vec()));
    assert!(!heap.delete(&mut store, 1, &9u64.to_le_bytes()).unwrap());
    heap.publish(&mut store).unwrap();
    check(&heap, &store, &model);
    let reopened = KeyedHeap::open(&mut store, heap.meta, Tuning::DEFAULT).unwrap();
    check(&reopened, &store, &model);
}

/// The writer raises the mark, without putting anything in place, to fill
/// holes a reader's snapshot still points at: the reader expires instead
/// of reading the values moved into them.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn a_reader_older_than_a_raised_mark_expires() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
    for key in 0..4000u64 {
        heap.put(&mut store, 1, &key.to_le_bytes(), &value(5000, key))
            .unwrap();
    }
    heap.publish(&mut store).unwrap();
    let mut reader = super::reader::KeyedReader::open(&store, heap.meta).unwrap();
    let checkpointed = heap.ring.checkpointed();
    // More than the patience freed, published, then more operations.
    for key in (0..4000u64).step_by(2) {
        heap.delete(&mut store, 1, &key.to_le_bytes()).unwrap();
    }
    heap.publish(&mut store).unwrap();
    for key in 0..200u64 {
        heap.put(&mut store, 2, &key.to_le_bytes(), b"x").unwrap();
    }
    heap.publish(&mut store).unwrap();
    assert!(heap.stats.moves > 0, "{:?}", heap.stats);
    assert!(heap.ring.mark() > reader.snapshot());
    assert_eq!(heap.ring.checkpointed(), checkpointed, "{:?}", heap.stats);
    match read(&mut reader, &store, &1u64.to_le_bytes()) {
        Err(crate::Error::MatrixSnapshotExpired { .. }) => {}
        other => panic!("a reader older than the mark must expire: {other:?}"),
    }
    reader.follow(&store).unwrap();
    for key in (1..4000u64).step_by(2) {
        assert_eq!(
            read(&mut reader, &store, &key.to_le_bytes()).unwrap(),
            Some(value(5000, key))
        );
    }
}

/// Every key rewritten each round with a value of a new random size, one
/// publication per round, on a small ring: the pool chain stays intact and
/// the pages settle.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn rewriting_every_key_each_round_settles() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
    let mut state = 11u64;
    let mut settled = 0;
    for round in 0..40u64 {
        for id in 0..2000u64 {
            let len = 64 + (random(&mut state) % 2000) as usize;
            heap.put(&mut store, 1, &id.to_le_bytes(), &value(len, round))
                .unwrap();
            bounded(&heap, chain_bytes(2072), &format!("round {round} id {id}"));
        }
        heap.publish(&mut store).unwrap();
        consistent(&heap, &store, &format!("round {round}"));
        if round == 10 {
            settled = heap.state().pages;
        }
    }
    assert!(
        heap.state().pages <= settled + 4,
        "{} pages, {settled} at round 10: {:?}",
        heap.state().pages,
        heap.stats
    );
}

/// Deleting most keys empties leaves, which go back to the pool, and the
/// tree shrinks; every key left reads back, through a reopen too, and a
/// crash at any write of the deleting generation keeps the last
/// publication.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn deletes_shrink_the_tree() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
    let mut model = Model::new();
    for key in 0..20_000u64 {
        let bytes = value(20, key);
        heap.put(&mut store, 1, &key.to_be_bytes(), &bytes).unwrap();
        model.insert((1, key.to_be_bytes().to_vec()), bytes);
    }
    heap.publish(&mut store).unwrap();
    let peak = heap.state().leaves;
    let published = model.clone();
    for key in 0..20_000u64 {
        if key % 50 != 0 {
            assert!(heap.delete(&mut store, 1, &key.to_be_bytes()).unwrap());
            model.remove(&(1, key.to_be_bytes().to_vec()));
        }
        if key % 1000 == 0 {
            consistent(&heap, &store, &format!("delete {key}"));
        }
    }
    let leaves = heap.state().leaves;
    assert!(leaves * 4 <= peak, "{leaves} leaves, {peak} at the peak: {:?}", heap.stats);
    check(&heap, &store, &model);
    consistent(&heap, &store, "after the deletes");
    let pending = store.writes_since_sync();
    for keep in (0..pending).step_by((pending / 40).max(1)) {
        let mut crashed = store.crashed(keep, 3);
        let reopened = KeyedHeap::open(&mut crashed, heap.meta, Tuning::DEFAULT).unwrap();
        check(&reopened, &crashed, &published);
        consistent(&reopened, &crashed, &format!("crash at write {keep}"));
    }
    heap.publish(&mut store).unwrap();
    let reopened = KeyedHeap::open(&mut store, heap.meta, Tuning::DEFAULT).unwrap();
    check(&reopened, &store, &model);
    let mut heap = reopened;
    for key in 0..20_000u64 {
        let bytes = value(20, key + 7);
        heap.put(&mut store, 1, &key.to_be_bytes(), &bytes).unwrap();
        model.insert((1, key.to_be_bytes().to_vec()), bytes);
    }
    check(&heap, &store, &model);
    consistent(&heap, &store, "after growing again");
}
