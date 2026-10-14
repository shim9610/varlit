//! The ordinary tests and libFuzzer execute the same bounded, independent oracle.
use std::{cell::RefCell, rc::Rc};
use varlit::experimental_matrix::{
    MatrixOptions, MatrixReader, MatrixReuseError, MatrixWriter, set_test_hook,
};
use varlit::{MatrixDimensions, MatrixKey, varlit_format};
varlit_format! {
    pub format PageModel {
        magic: b"RUSEFUZ2"; version:1; endian:little;
        dims { row:u32, col:u32, }
        commit: cell_bitmap {keyspace=[row,col];categories=[data,meta];};
        blocks {
            matrix Blob(id=1,dims=[row,col],category=data) { epoch:u64, bytes:[u8;5000], }
            matrix Tag(id=2,dims=[row,col],category=meta) { epoch:u64, }
        }
    }
}
const N: usize = 12;
const POINTS: &[&str] = &[
    "matrix_reuse.intent_durable",
    "matrix_reuse.payload_written",
    "matrix_reuse.slot_written",
    "matrix_reuse.retirement_prefix",
    "matrix_reuse.retirement_written",
    "matrix_reuse.data_durable",
    "matrix_reuse.head_prefix",
    "matrix_reuse.head_written",
    "matrix_reuse.head_durable",
    "matrix_reuse.reset",
];
fn byte(data: &[u8], i: usize) -> u8 {
    data.get(i).copied().unwrap_or(0)
}
fn key(i: usize) -> MatrixKey {
    MatrixKey::new(i as u64 / 2, i as u64 % 2)
}
fn blob(epoch: u64) -> Blob {
    Blob {
        epoch,
        bytes: [epoch as u8; 5000],
    }
}
fn spec() -> varlit::FormatSpec {
    PageModel::spec().with_growing_matrix_dimension("row", 2)
}
fn dims() -> MatrixDimensions {
    MatrixDimensions::from_pairs([("row", 2), ("col", 2)])
}
fn options(data: &[u8]) -> MatrixOptions {
    let page_bytes = [4096, 8192, 16384, 65536][byte(data, 0) as usize % 4];
    MatrixOptions {
        reuse: true,
        page_bytes,
        rows_per_chunk: [2, 4, 4096][byte(data, 1) as usize % 3],
        max_chunk_bytes: page_bytes * (1 + u64::from(byte(data, 2) % 3)),
        cache_bytes: [0, 4096, 262144][byte(data, 3) as usize % 3],
        read_retries: 4,
    }
}
fn check(r: &MatrixReader, values: &[Option<u64>; N], tags: &[Option<u64>; N]) {
    for i in 0..N {
        match (values[i], r.get::<Blob>(key(i))) {
            (Some(e), Ok(v)) => {
                assert_eq!(v.epoch, e);
                assert_eq!(v.bytes, [e as u8; 5000]);
            }
            (None, Err(MatrixReuseError::Missing)) => {}
            (expected, actual) => panic!("blob {i}: expected {expected:?}, got {actual:?}"),
        }
        match (tags[i], r.get::<Tag>(key(i))) {
            (Some(e), Ok(v)) => assert_eq!(v.epoch, e),
            (None, Err(MatrixReuseError::Missing)) => {}
            (expected, actual) => panic!("tag {i}: expected {expected:?}, got {actual:?}"),
        }
    }
    // One call spanning pages, chunks and row boundaries must agree with point reads.
    let mut out = vec![blob(0); N];
    let result = r.read_run_into(key(0), &mut out);
    if values.iter().all(Option::is_some) {
        result.unwrap();
        for (v, e) in out.iter().zip(values) {
            let e = e.unwrap();
            assert_eq!(v.epoch, e);
            assert_eq!(v.bytes, [e as u8; 5000]);
        }
    } else {
        assert!(matches!(result, Err(MatrixReuseError::Missing)));
    }
}
fn fill(w: &mut MatrixWriter, epoch: u64) {
    for i in 0..N {
        w.put(key(i), &blob(epoch)).unwrap();
        w.put(key(i), &Tag { epoch }).unwrap();
    }
    w.sync().unwrap();
}
// Reset even during panic/unwind in ordinary tests; hooks must never escape a case.
struct Hook;
impl Drop for Hook {
    fn drop(&mut self) {
        set_test_hook(None);
    }
}

pub fn run(data: &[u8], publication: bool) {
    let _hook = Hook;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pages");
    let opt = options(data);
    let mut w = MatrixWriter::create(spec(), &path, dims(), opt).unwrap();
    if !publication {
        let mut r = w.reader().unwrap();
        let mut pending = [None; N];
        let mut tags = pending;
        let mut confirmed = pending;
        let mut confirmed_tags = tags;
        for (step, op) in data
            .get(4..)
            .unwrap_or_default()
            .chunks(3)
            .take(64)
            .enumerate()
        {
            let i = byte(op, 1) as usize % N;
            let e = (step as u64 + 1) * 256 + u64::from(byte(op, 2));
            match op[0] % 9 {
                0 | 1 => {
                    w.put(key(i), &blob(e)).unwrap();
                    pending[i] = Some(e);
                }
                2 => {
                    let result = w.delete::<Blob>(key(i));
                    assert!(
                        result.is_ok()
                            || (pending[i].is_none()
                                && matches!(result, Err(MatrixReuseError::Missing)))
                    );
                    pending[i] = None;
                }
                3 => {
                    w.put(key(i), &Tag { epoch: e }).unwrap();
                    tags[i] = Some(e);
                }
                4 => {
                    let result = w.delete::<Tag>(key(i));
                    assert!(
                        result.is_ok()
                            || (tags[i].is_none()
                                && matches!(result, Err(MatrixReuseError::Missing)))
                    );
                    tags[i] = None;
                }
                5 => {
                    w.sync().unwrap();
                    confirmed = pending;
                    confirmed_tags = tags;
                }
                6 => {
                    drop(w);
                    w = MatrixWriter::open(spec(), &path, dims(), opt).unwrap();
                    pending = confirmed;
                    tags = confirmed_tags;
                }
                7 => {
                    r = MatrixReader::open(spec(), &path, dims(), opt).unwrap();
                }
                _ => {
                    if let Some(e) = pending[i] {
                        w.put(key(i), &blob(e)).unwrap();
                    }
                }
            }
            check(&r, &confirmed, &confirmed_tags);
        }
        w.sync().unwrap();
        check(&r, &pending, &tags);
        drop(w);
        let w = MatrixWriter::open(spec(), &path, dims(), opt).unwrap();
        check(&w.reader().unwrap(), &pending, &tags);
        return;
    }
    fill(&mut w, 1);
    fill(&mut w, 2);
    let r = MatrixReader::open(spec(), &path, dims(), opt).unwrap();
    match byte(data, 4) % 4 {
        0 => {
            // Every cut has an exact visibility oracle, not "old OR new".
            let index = byte(data, 5) as usize % POINTS.len();
            let point = POINTS[index];
            for i in 0..N {
                w.put(key(i), &blob(3)).unwrap();
                w.delete::<Tag>(key(i)).unwrap();
            }
            let occurrence = if (1..=4).contains(&index) {
                1 + byte(data, 6) % 2
            } else {
                1
            };
            let mut seen = 0;
            set_test_hook(Some(Box::new(move |p| {
                if p == point {
                    seen += 1;
                    if seen == occurrence {
                        return Err(std::io::Error::other("model cut"));
                    }
                }
                Ok(())
            })));
            assert!(
                w.sync().is_err(),
                "cut {point}, occurrence {occurrence}, input {data:?}"
            );
            set_test_hook(None);
            assert!(matches!(
                w.put(key(0), &blob(4)),
                Err(MatrixReuseError::Poisoned)
            ));
            let e = if index >= 7 { 3 } else { 2 };
            let tags = if index >= 7 { [None; N] } else { [Some(2); N] };
            check(&r, &[Some(e); N], &tags);
            drop(w);
            let mut w = MatrixWriter::open(spec(), &path, dims(), opt).unwrap();
            check(&r, &[Some(e); N], &tags);
            w.put(key(N - 1), &blob(4)).unwrap();
            w.sync().unwrap();
            let mut expected = [Some(e); N];
            expected[N - 1] = Some(4);
            check(&r, &expected, &tags);
        }
        1 => {
            // Reuse both physical slots after only part of a range has been copied.
            let w = Rc::new(RefCell::new(w));
            let writer = w.clone();
            let hit = Rc::new(RefCell::new(false));
            let fired = hit.clone();
            let mut skip = byte(data, 5) % 2;
            let last = 4 + u64::from(byte(data, 6) % 4);
            set_test_hook(Some(Box::new(move |p| {
                if p == "matrix_reuse.page_copied" && !*fired.borrow() {
                    if skip > 0 {
                        skip -= 1;
                    } else {
                        *fired.borrow_mut() = true;
                        for e in 3..=last {
                            fill(&mut writer.borrow_mut(), e);
                        }
                    }
                }
                Ok(())
            })));
            let mut out = vec![blob(0); N];
            r.read_run_into(key(0), &mut out).unwrap();
            set_test_hook(None);
            assert!(*hit.borrow());
            for v in out {
                assert_eq!(v.epoch, last);
                assert_eq!(v.bytes, [last as u8; 5000]);
            }
            check(&r, &[Some(last); N], &[Some(last); N]);
        }
        2 => {
            drop(w);
            // Damage a CURRENT payload/header while leaving both publication heads intact.
            // An older plausible value or zero must never be returned as a successful read.
            let mut bytes = std::fs::read(&path).unwrap();
            let stride = 64 + opt.page_bytes as usize;
            let candidates: Vec<_> = (12288..bytes.len())
                .step_by(stride)
                .filter(|&at| {
                    at + stride <= bytes.len()
                        && &bytes[at..at + 8] == b"STRSLOT2"
                        && u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap()) == 2
                })
                .collect();
            assert!(!candidates.is_empty());
            let at = candidates[byte(data, 5) as usize % candidates.len()];
            let offset = if byte(data, 6) & 1 == 0 {
                64 + byte(data, 7) as usize
            } else {
                24
            };
            bytes[at + offset] ^= 1 << (byte(data, 8) % 8);
            std::fs::write(&path, bytes).unwrap();
            let mut errors = 0;
            for i in 0..N {
                match r.get::<Blob>(key(i)) {
                    Ok(v) => {
                        assert_eq!(v.epoch, 2);
                        assert_eq!(v.bytes, [2; 5000]);
                    }
                    Err(_) => errors += 1,
                }
                match r.get::<Tag>(key(i)) {
                    Ok(v) => assert_eq!(v.epoch, 2),
                    Err(_) => errors += 1,
                }
            }
            assert!(errors > 0, "corrupted selected page was silently accepted");
        }
        _ => {
            for i in 0..N {
                w.put(key(i), &blob(3)).unwrap();
            }
            set_test_hook(Some(Box::new(|p| {
                if p == "matrix_reuse.slot_written" {
                    Err(std::io::Error::other("journal cut"))
                } else {
                    Ok(())
                }
            })));
            assert!(w.sync().is_err());
            set_test_hook(None);
            drop(w);
            check(&r, &[Some(2); N], &[Some(2); N]);
            let journal = path.with_file_name("pages.reuse-journal");
            let original = std::fs::read(&journal).unwrap();
            assert!(original.len() >= 4096 + 96);
            let mut damaged = original.clone();
            let records = (damaged.len() - 4096) / 96;
            let at = 4096 + (byte(data, 5) as usize % records) * 96 + byte(data, 6) as usize % 96;
            damaged[at] ^= 1 << (byte(data, 7) % 8);
            std::fs::write(&journal, damaged).unwrap();
            let primary = std::fs::read(&path).unwrap();
            assert!(MatrixWriter::open(spec(), &path, dims(), opt).is_err());
            assert_eq!(
                std::fs::read(&path).unwrap(),
                primary,
                "failed journal validation mutated primary"
            );
            std::fs::write(&journal, original).unwrap();
            let mut w = MatrixWriter::open(spec(), &path, dims(), opt).unwrap();
            check(&r, &[Some(2); N], &[Some(2); N]);
            w.put(key(N - 1), &blob(4)).unwrap();
            w.sync().unwrap();
            let mut expected = [Some(2); N];
            expected[N - 1] = Some(4);
            check(&r, &expected, &[Some(2); N]);
        }
    }
}

pub fn seeds(publication: bool) -> Vec<Vec<u8>> {
    if publication {
        let mut seeds = Vec::new();
        for page in 0..4 {
            for point in 0..10 {
                for occurrence in 0..2 {
                    seeds.push(vec![
                        page,
                        point % 3,
                        occurrence,
                        occurrence,
                        0,
                        point,
                        occurrence,
                        0,
                        0,
                    ]);
                }
            }
            for cache in 0..3 {
                for cut in 0..2 {
                    seeds.push(vec![page, cut, cache, cache, 1, cut, cache, 0, 0]);
                    for damage in 0..2 {
                        seeds.push(vec![page, cut, cache, cache, 2, cut, damage, 0, 0]);
                    }
                }
            }
        }
        for page in 0..4 {
            for offset in [0, 8, 16, 32, 60, 80, 95] {
                seeds.push(vec![page, page % 3, 0, 0, 3, 1, offset, 3]);
            }
        }
        return seeds;
    }
    (0..60)
        .map(|i| {
            let mut seed = vec![
                i % 4,
                (i / 4) % 3,
                (i / 12) % 3,
                (i / 3) % 3,
                i % 3,
                (i / 3) % 10,
                (i / 30) % 3,
                i,
                i % 8,
            ];
            if !publication {
                let mut state = u64::from(i) + 1;
                seed.truncate(4);
                for _ in 0..192 {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    seed.push(state as u8);
                }
            }
            seed
        })
        .collect()
}
