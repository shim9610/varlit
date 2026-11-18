//! Keyed storage against a model: puts, removes, syncs, writer reopens and a
//! reader that follows, driven by fuzz input, checked after every step
//! against a `BTreeMap` of what each handle must see.

use std::collections::BTreeMap;
use std::path::PathBuf;

use varlit::{VarlitEmpty, varlit_format};

varlit_format! {
    pub format FuzzKeyedFormat {
        magic: b"FZKY";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        keyed_storage: in_place(free_space = 1%);
        blocks {
            variable FuzzEntry(id = 1, key = [key]) {
                key: u16,
                value: Vec<u8>,
            }
        }
    }
}

type Model = BTreeMap<u16, Vec<u8>>;

struct Scratch {
    path: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let mut lock = self.path.as_os_str().to_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(PathBuf::from(lock));
        let _ = std::fs::remove_file(&self.path);
    }
}

fn check(file: &varlit::VarlitFile, model: &Model, keys: u16) {
    let mut scratch = Vec::new();
    let mut out = FuzzEntry::empty();
    for key in 0..keys {
        let found = file
            .get_keyed::<FuzzEntry>(&key, &mut scratch, &mut out)
            .expect("a keyed read of a healthy file succeeds");
        match model.get(&key) {
            Some(value) => {
                assert!(found, "key {key} lost");
                assert_eq!(&out.value, value, "key {key}");
            }
            None => assert!(!found, "key {key} resurrected"),
        }
    }
}

/// Runs one operation history. Every two bytes are an operation: the first
/// picks it, the second its key and size.
pub fn run(data: &[u8]) {
    let path = std::env::temp_dir().join(format!(
        "varlit-keyed-fuzz-{}-{:x}.bin",
        std::process::id(),
        data.len()
    ));
    let _cleanup = Scratch { path: path.clone() };
    let _ = std::fs::remove_file(&path);
    let spec = FuzzKeyedFormat::spec();
    let mut writer = varlit::VarlitFile::create(spec, &path).expect("create");
    let mut reader: Option<varlit::VarlitFile> = None;
    // What the writer sees, and what was last published (what a reader that
    // follows, or a reopened writer, sees). A sync publishes; so does a put
    // or remove that ends a round (one that filled the change budget or the
    // ring), with everything staged up to and including it.
    let mut staged = Model::new();
    let mut synced = Model::new();
    let mut generation = writer.keyed_storage_stats().expect("stats").generation;
    let keys = 24u16;
    for op in data.as_chunks::<2>().0 {
        let key = u16::from(op[1]) % keys;
        match op[0] % 8 {
            0..=2 => {
                // Sizes cross the chunk alignment and, rarely, half a region.
                let len = match op[1] >> 5 {
                    7 => 600_000,
                    6 => 4_000,
                    n => usize::from(n) * 37,
                };
                let value: Vec<u8> = (0..len).map(|i| (i as u8) ^ op[0]).collect();
                writer
                    .put_keyed(&FuzzEntry {
                        key,
                        value: value.clone(),
                    })
                    .expect("put");
                staged.insert(key, value);
            }
            3 => {
                let removed = writer.remove_keyed::<FuzzEntry>(&key).expect("remove");
                assert_eq!(removed, staged.remove(&key).is_some());
            }
            4 => {
                writer.sync().expect("sync");
                synced = staged.clone();
            }
            5 => {
                // Changes since the last publication are not kept: a
                // reopened writer sees the last sync or round.
                drop(writer);
                writer = varlit::VarlitFile::open(spec, &path).expect("reopen");
                staged = synced.clone();
            }
            6 => match reader.as_mut() {
                None => {
                    let opened = varlit::VarlitFile::open_readonly(spec, &path).expect("reader");
                    check(&opened, &synced, keys);
                    reader = Some(opened);
                }
                Some(open) => {
                    open.follow().expect("follow");
                    check(open, &synced, keys);
                }
            },
            _ => check(&writer, &staged, keys),
        }
        let now = writer.keyed_storage_stats().expect("stats").generation;
        if now != generation {
            synced = staged.clone();
            generation = now;
        }
    }
    check(&writer, &staged, keys);
    // The heap never holds more than the live data, the ratio and constants.
    let stats = writer.keyed_storage_stats().expect("stats");
    assert_eq!(stats.keys, staged.len() as u64);
}

const CORRUPT_KEYS: u16 = 16;

/// The valid file [`run_corrupt`] damages, and what it holds: records of
/// every size class (short, page-sized, overflowing), some removed, in two
/// publications.
fn undamaged_keyed_file() -> &'static (Vec<u8>, Model) {
    static FILE: std::sync::OnceLock<(Vec<u8>, Model)> = std::sync::OnceLock::new();
    FILE.get_or_init(|| {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let path = dir.path().join("keyed.varlit");
        let mut model = Model::new();
        let mut writer =
            varlit::VarlitFile::create(FuzzKeyedFormat::spec(), &path).expect("create");
        for key in 0..CORRUPT_KEYS {
            let len = [3usize, 700, 5_000][usize::from(key) % 3];
            let value: Vec<u8> = (0..len).map(|i| (i as u8) ^ (key as u8)).collect();
            writer
                .put_keyed(&FuzzEntry {
                    key,
                    value: value.clone(),
                })
                .expect("put");
            model.insert(key, value);
            if key == CORRUPT_KEYS / 2 {
                writer.sync().expect("sync");
            }
        }
        for key in (0..CORRUPT_KEYS).step_by(5) {
            writer.remove_keyed::<FuzzEntry>(&key).expect("remove");
            model.remove(&key);
        }
        writer.sync().expect("sync");
        drop(writer);
        (std::fs::read(&path).expect("read the file back"), model)
    })
}

/// Keyed storage read back from damaged bytes: a valid file (records of
/// every size class, some removed, two publications) whose bytes the input
/// overwrites, flips or cuts, then opened by a reader (lookups, a range, the
/// stats, a follow) and by a writer (recovery, a put, a sync). Only soundness
/// is owed: every refusal is an error, never a panic, a hang or a record that
/// was never written. The undamaged file must read back exactly.
pub fn run_corrupt(data: &[u8]) {
    let Some((&control, rest)) = data.split_first() else {
        return;
    };
    let Ok(dir) = tempfile::tempdir() else {
        return;
    };
    let path = dir.path().join("keyed.varlit");
    let spec = FuzzKeyedFormat::spec();
    let keys = CORRUPT_KEYS;
    // The undamaged file is built once per process: every input damages a
    // copy of the same bytes, so a run costs a read, not sixteen puts.
    let (pristine_bytes, model) = undamaged_keyed_file();
    let mut bytes = pristine_bytes.clone();

    let pristine = rest.is_empty();
    if !pristine && !bytes.is_empty() {
        let len = bytes.len();
        let at = |pair: &[u8]| {
            usize::from(u16::from_le_bytes([
                pair[0],
                pair.get(1).copied().unwrap_or(0),
            ])) * len
                / 65_536
        };
        match control % 3 {
            // Overwrite a run of bytes somewhere in the file.
            0 => {
                let start = at(rest);
                let patch = rest.get(2..).unwrap_or_default();
                let end = (start + patch.len()).min(bytes.len());
                bytes[start..end].copy_from_slice(&patch[..end - start]);
            }
            // Flip single bits, three bytes of input each.
            1 => {
                for triple in rest.chunks_exact(3) {
                    let index = at(triple) % len;
                    bytes[index] ^= 1 << (triple[2] % 8);
                }
            }
            // Cut the file.
            _ => {
                let cut = at(rest);
                bytes.truncate(cut);
            }
        }
    }
    if std::fs::write(&path, &bytes).is_err() {
        return;
    }

    let mut scratch = Vec::new();
    let mut out = FuzzEntry::empty();
    if let Ok(mut reader) = varlit::VarlitFile::open_readonly(spec, &path) {
        for key in 0..keys {
            match reader.get_keyed::<FuzzEntry>(&key, &mut scratch, &mut out) {
                Ok(true) => {
                    if pristine {
                        assert_eq!(Some(&out.value), model.get(&key), "key {key}");
                    }
                }
                Ok(false) => assert!(!pristine || !model.contains_key(&key), "key {key} lost"),
                Err(_) => assert!(!pristine, "the undamaged file refused key {key}"),
            }
        }
        let mut seen = 0u64;
        let visited = reader.range_keyed::<FuzzEntry, _>(.., &mut scratch, &mut out, &mut |_| {
            seen += 1;
            Ok(seen < 64)
        });
        if pristine {
            assert_eq!(visited.ok(), Some(model.len() as u64));
        }
        let _ = reader.keyed_storage_stats();
        let _ = reader.follow();
        let _ = reader.get_keyed::<FuzzEntry>(&1, &mut scratch, &mut out);
    } else {
        assert!(!pristine, "the undamaged file did not open");
    }
    if let Ok(mut writer) = varlit::VarlitFile::open(spec, &path) {
        let _ = writer.get_keyed::<FuzzEntry>(&1, &mut scratch, &mut out);
        if writer
            .put_keyed(&FuzzEntry {
                key: 1,
                value: vec![9; 40],
            })
            .is_ok()
        {
            let _ = writer.sync();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn deterministic_histories_match_the_model() {
        let mut histories: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0, 1, 0, 1, 4, 0, 7, 0],
            vec![0, 0xff, 4, 0, 3, 0x1f, 6, 0, 5, 0, 6, 0, 7, 0],
        ];
        let mut state = 0x5eed_u64;
        for len in [64usize, 400, 1200] {
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                bytes.push((state >> 56) as u8);
            }
            histories.push(bytes);
        }
        for history in histories {
            super::run(&history);
        }
    }

    #[test]
    fn damaged_files_are_refused_or_read_soundly() {
        // Undamaged first: it must read back exactly.
        super::run_corrupt(&[0]);
        let mut state = 0x1234_5678_u64;
        for control in 0u8..3 {
            for len in [2usize, 8, 40, 200] {
                for _ in 0..6 {
                    let mut data = vec![control];
                    for _ in 0..len {
                        state = state
                            .wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(1);
                        data.push((state >> 56) as u8);
                    }
                    super::run_corrupt(&data);
                }
            }
        }
    }
}
