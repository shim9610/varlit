use std::collections::BTreeMap;

use super::heap::{KeyedHeap, Tuning};
use super::{MIN_BUDGET, PAGE_BYTES, State};
use crate::matrix_tiles::store::memory::MemStore;

const RING: u64 = 1 << 20;
const BUDGET: u64 = super::DEFAULT_BUDGET;
/// Small tree pages: deep trees from a few thousand keys. Two of the
/// largest cells still fit a page.
const SMALL_PAGES: Tuning = Tuning { node_bytes: 2400 };

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
            "block {block} key {key:?}"
        );
    }
    assert_eq!(heap.state().keys, model.len() as u64);
}

fn consistent(heap: &KeyedHeap, store: &MemStore, when: &str) {
    if let Err(error) = heap.check_consistency(store) {
        panic!("{when}: {error}: {:?}", heap.stats);
    }
}

fn random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn value(len: usize, fill: u64) -> Vec<u8> {
    (0..len).map(|i| (i as u64 ^ fill) as u8).collect()
}

#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn puts_deletes_and_reopens_match_a_model_across_splits() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, SMALL_PAGES);
    let mut model = Model::new();
    let mut state = 1u64;
    for round in 0..4u64 {
        for step in 0..1500u64 {
            let key = (random(&mut state) % 3000).to_le_bytes().to_vec();
            let block = 1 + (random(&mut state) % 2) as u32;
            if random(&mut state).is_multiple_of(5) {
                let removed = heap.delete(&mut store, block, &key).unwrap();
                assert_eq!(removed, model.remove(&(block, key)).is_some());
            } else {
                let len = match random(&mut state) % 8 {
                    0 => 6000,
                    1 => 1100,
                    _ => (random(&mut state) % 300) as usize,
                };
                let bytes = value(len, round * 10_000 + step);
                heap.put(&mut store, block, &key, &bytes).unwrap();
                model.insert((block, key), bytes);
            }
        }
        heap.publish(&mut store).unwrap();
        if round % 2 == 1 {
            heap = KeyedHeap::open(&mut store, heap.meta, SMALL_PAGES).unwrap();
        }
        check(&heap, &store, &model);
        consistent(&heap, &store, &format!("round {round}"));
    }
    assert!(heap.state().root.1 >= 3, "{:?}", heap.state());
}

/// Values of every length, around every boundary a cell and an overflow
/// chain have, and keys long enough to spill into overflow pages, read back.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn values_and_keys_of_any_length_round_trip() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
    let mut model = Model::new();
    let data = super::tree::OVERFLOW_DATA as usize;
    let lengths = [
        0,
        1,
        1000,
        1012,
        1013,
        1100,
        data,
        data + 256,
        data + 1500,
        3 * data + 7,
        1 << 20,
    ];
    for (index, len) in lengths.into_iter().enumerate() {
        let key = (index as u64).to_be_bytes().to_vec();
        let bytes = value(len, index as u64);
        heap.put(&mut store, 1, &key, &bytes).unwrap();
        model.insert((1, key), bytes);
    }
    // Long keys sharing long prefixes: separators that spill too.
    for index in 0..40u64 {
        let mut key = vec![b'k'; 5000];
        key.extend_from_slice(&index.to_be_bytes());
        let bytes = value(64, index);
        heap.put(&mut store, 2, &key, &bytes).unwrap();
        model.insert((2, key), bytes);
    }
    let mut huge = vec![7u8; 150_000];
    huge.push(1);
    heap.put(&mut store, 3, &huge, b"huge key").unwrap();
    model.insert((3, huge), b"huge key".to_vec());
    check(&heap, &store, &model);
    consistent(&heap, &store, "written");
    heap.publish(&mut store).unwrap();
    let heap = KeyedHeap::open(&mut store, heap.meta, Tuning::DEFAULT).unwrap();
    check(&heap, &store, &model);
    consistent(&heap, &store, "reopened");
}

/// Long keys on small pages: interior pages fill with spilled separators.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn long_keys_build_deep_trees() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, SMALL_PAGES);
    let mut model = Model::new();
    let mut state = 5u64;
    for step in 0..3000u64 {
        let mut key = vec![b'p'; 300 + (random(&mut state) % 300) as usize];
        key.extend_from_slice(&random(&mut state).to_be_bytes());
        let bytes = value((random(&mut state) % 200) as usize, step);
        heap.put(&mut store, 1, &key, &bytes).unwrap();
        model.insert((1, key), bytes);
        if step % 3 == 0 {
            let victim = model
                .keys()
                .nth((random(&mut state) % model.len() as u64) as usize)
                .cloned();
            if let Some((block, key)) = victim {
                assert!(heap.delete(&mut store, block, &key).unwrap());
                model.remove(&(block, key));
            }
        }
    }
    check(&heap, &store, &model);
    consistent(&heap, &store, "after the steps");
    assert!(heap.state().root.1 >= 3, "{:?}", heap.state());
}

/// Every key rewritten round after round, with values of new sizes: the
/// pages freed are taken again, and the storage stops growing.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn rewriting_every_key_does_not_grow_the_storage() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, MIN_BUDGET, Tuning::DEFAULT);
    let mut state = 11u64;
    let mut settled = 0;
    for round in 0..30u64 {
        for id in 0..2000u64 {
            let len = 64 + (random(&mut state) % 6000) as usize;
            heap.put(&mut store, 1, &id.to_le_bytes(), &value(len, round))
                .unwrap();
        }
        heap.publish(&mut store).unwrap();
        if round == 10 {
            settled = heap.state().pages;
            consistent(&heap, &store, "round 10");
        }
    }
    consistent(&heap, &store, "after the rounds");
    let pages = heap.state().pages;
    assert!(
        pages <= settled + settled / 10,
        "{pages} pages, {settled} at round 10: {:?} {:?}",
        heap.stats,
        heap.state()
    );
    assert!(heap.stats.pages_reused > 0, "{:?}", heap.stats);
}

/// Deleting most keys merges and frees leaves; their pages wait in the free
/// list, and new keys take them before any block is added.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn deletes_shrink_the_tree_and_new_keys_reuse_the_pages() {
    let mut store = MemStore::new();
    // One round for all the deletes: a crash shows the state before them.
    let mut heap = create(&mut store, super::MAX_BUDGET, SMALL_PAGES);
    let mut model = Model::new();
    for key in 0..20_000u64 {
        let bytes = value(20, key);
        heap.put(&mut store, 1, &key.to_be_bytes(), &bytes).unwrap();
        model.insert((1, key.to_be_bytes().to_vec()), bytes);
    }
    heap.publish(&mut store).unwrap();
    let peak = *heap.state();
    let published = model.clone();
    for key in 0..20_000u64 {
        if key % 50 != 0 {
            assert!(heap.delete(&mut store, 1, &key.to_be_bytes()).unwrap());
            model.remove(&(1, key.to_be_bytes().to_vec()));
        }
        if key % 2000 == 0 {
            consistent(&heap, &store, &format!("delete {key}"));
        }
    }
    assert!(
        heap.state().leaves * 4 <= peak.leaves,
        "{:?} after, {peak:?} at the peak",
        heap.state()
    );
    check(&heap, &store, &model);
    consistent(&heap, &store, "after the deletes");
    let pending = store.writes_since_sync();
    for keep in (0..pending).step_by((pending / 40).max(1)) {
        let mut crashed = store.crashed(keep, 3);
        let reopened = KeyedHeap::open(&mut crashed, heap.meta, SMALL_PAGES).unwrap();
        check(&reopened, &crashed, &published);
        consistent(&reopened, &crashed, &format!("crash at write {keep}"));
    }
    heap.publish(&mut store).unwrap();
    let mut heap = KeyedHeap::open(&mut store, heap.meta, SMALL_PAGES).unwrap();
    check(&heap, &store, &model);
    // As many new keys as there were: the free pages are taken first.
    for key in 20_000..39_000u64 {
        let bytes = value(20, key);
        heap.put(&mut store, 1, &key.to_be_bytes(), &bytes).unwrap();
        model.insert((1, key.to_be_bytes().to_vec()), bytes);
    }
    heap.publish(&mut store).unwrap();
    check(&heap, &store, &model);
    consistent(&heap, &store, "after growing again");
    assert!(
        heap.state().pages <= peak.pages + peak.pages / 10,
        "{:?} after growing again, {peak:?} at the peak",
        heap.state()
    );
}

/// A crash at any write after a publication leaves exactly the published
/// state once the writer reopens; a crash at any write of the publication
/// leaves one of the two generations whole.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn a_crash_after_any_write_keeps_a_publication_whole() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, SMALL_PAGES);
    let mut model = Model::new();
    for key in 0..300u64 {
        let bytes = value(50 + key as usize % 2000, key);
        heap.put(&mut store, 1, &key.to_le_bytes(), &bytes).unwrap();
        model.insert((1, key.to_le_bytes().to_vec()), bytes);
    }
    heap.publish(&mut store).unwrap();
    heap.ring.checkpoint(&mut store, u64::MAX).unwrap();
    let meta = heap.meta;
    // A second generation that rewrites, deletes, splits and merges.
    let mut next = model.clone();
    for key in 0..400u64 {
        if key % 3 == 0 {
            heap.delete(&mut store, 1, &key.to_le_bytes()).unwrap();
            next.remove(&(1, key.to_le_bytes().to_vec()));
        } else {
            let bytes = value(900 + key as usize, key + 1);
            heap.put(&mut store, 1, &key.to_le_bytes(), &bytes).unwrap();
            next.insert((1, key.to_le_bytes().to_vec()), bytes);
        }
    }
    let pending = store.writes_since_sync();
    assert!(pending > 0);
    for keep in (0..pending).step_by((pending / 40).max(1)) {
        for torn in [0usize, 5] {
            let mut crashed = store.crashed(keep, torn);
            let heap = KeyedHeap::open(&mut crashed, meta, SMALL_PAGES).unwrap();
            check(&heap, &crashed, &model);
            consistent(&heap, &crashed, &format!("crash at write {keep}"));
        }
    }
    heap.publish(&mut store).unwrap();
    let pending = store.writes_since_sync();
    for keep in 0..=pending {
        let mut crashed = store.crashed(keep, 3);
        let reopened = KeyedHeap::open(&mut crashed, meta, SMALL_PAGES).unwrap();
        let shows = |model: &Model| {
            reopened.state().keys == model.len() as u64
                && model.iter().all(|((block, key), value)| {
                    get(&reopened, &crashed, *block, key).as_deref() == Some(&value[..])
                })
        };
        assert!(
            shows(&model) || shows(&next),
            "crash at publication write {keep}"
        );
        consistent(
            &reopened,
            &crashed,
            &format!("crash at publication write {keep}"),
        );
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

/// A reader whose snapshot uses pages the writer has since freed and taken
/// again never reads the new contents as its own: it reads its own values
/// or expires.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn a_reader_never_sees_pages_taken_again() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, MIN_BUDGET, SMALL_PAGES);
    for key in 0..500u64 {
        heap.put(&mut store, 1, &key.to_le_bytes(), &value(5000, key))
            .unwrap();
    }
    heap.publish(&mut store).unwrap();
    let mut reader = super::reader::KeyedReader::open(&store, heap.meta).unwrap();
    for round in 1..6u64 {
        for key in 0..500u64 {
            heap.put(
                &mut store,
                1,
                &key.to_le_bytes(),
                &value(5000, key + 1000 * round),
            )
            .unwrap();
        }
        heap.publish(&mut store).unwrap();
    }
    assert!(heap.stats.pages_reused > 0, "{:?}", heap.stats);
    let mut expired = false;
    for key in 0..500u64 {
        match read(&mut reader, &store, &key.to_le_bytes()) {
            Ok(Some(found)) => assert_eq!(found, value(5000, key), "key {key}"),
            Err(crate::Error::MatrixSnapshotExpired { .. }) => expired = true,
            other => panic!("key {key}: {other:?}"),
        }
    }
    assert!(expired, "pages were taken again under the reader");
    reader.follow(&store).unwrap();
    assert_eq!(
        read(&mut reader, &store, &7u64.to_le_bytes()).unwrap(),
        Some(value(5000, 7 + 5000))
    );
}

/// Random puts, deletes, publications and reopens: the storage is
/// consistent and every key reads back.
#[test]
#[cfg_attr(miri, ignore = "thousands of ring writes; run natively")]
fn random_histories_stay_consistent() {
    for seed in 1..=4u64 {
        let mut store = MemStore::new();
        let mut heap = create(&mut store, MIN_BUDGET, SMALL_PAGES);
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
                // A put that ends a round publishes what came with it.
                model.insert((1, key), bytes);
                if heap.ring.published() != generation {
                    published = model.clone();
                }
            } else if roll < 88 {
                let generation = heap.ring.published();
                let removed = heap.delete(&mut store, 1, &key).unwrap();
                let expected = model.remove(&(1, key.clone())).is_some();
                assert_eq!(removed, expected, "seed {seed} step {step}");
                if heap.ring.published() != generation {
                    published = model.clone();
                }
            } else if roll < 97 {
                heap.publish(&mut store).unwrap();
                published = model.clone();
            } else {
                // Unpublished changes are lost; a reopened writer starts
                // from the last publication.
                heap = KeyedHeap::open(&mut store, meta, SMALL_PAGES).unwrap();
                model = published.clone();
            }
            if step % 25 == 0 {
                consistent(&heap, &store, &format!("seed {seed} step {step}"));
            }
        }
        check(&heap, &store, &model);
        consistent(&heap, &store, &format!("seed {seed} at the end"));
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
    consistent(&reopened, &store, "reopened");
}

/// The state's layout is little-endian whatever the platform.
#[test]
fn the_state_layout_is_the_same_bytes_on_every_platform() {
    let state = State {
        root: (PAGE_BYTES, 1),
        keys: 3,
        pages: 2,
        leaves: 1,
        free: 1,
        free_head: 2 * PAGE_BYTES,
        free_tail: 2 * PAGE_BYTES,
        ..State::default()
    };
    let encoded = state.encode();
    assert_eq!(&encoded[16..24], &3u64.to_le_bytes());
    assert_eq!(&encoded[56..64], &1u64.to_le_bytes());
    assert_eq!(State::decode(&encoded).unwrap(), state);
}

#[test]
#[ignore = "profiling"]
fn profile_updates() {
    let mut store = MemStore::new();
    let mut heap = create(&mut store, BUDGET, Tuning::DEFAULT);
    for key in 0..200_000u64 {
        heap.put(&mut store, 1, &key.to_le_bytes(), &value(44, key))
            .unwrap();
    }
    heap.publish(&mut store).unwrap();
    let mut state = 3u64;
    let t = std::time::Instant::now();
    for txn in 0..1000u64 {
        for _ in 0..20 {
            let key = random(&mut state) % 200_000;
            heap.put(&mut store, 1, &key.to_le_bytes(), &value(44, txn))
                .unwrap();
        }
        heap.publish(&mut store).unwrap();
    }
    eprintln!(
        "1000 txns of 20 puts {:?}: {:?} {:?}",
        t.elapsed(),
        heap.stats,
        heap.state()
    );
    let t = std::time::Instant::now();
    for _ in 0..20_000 {
        let key = random(&mut state) % 200_000;
        assert!(heap.find(&store, 1, &key.to_le_bytes()).unwrap().is_some());
    }
    eprintln!("20 000 lookups {:?}", t.elapsed());
}
