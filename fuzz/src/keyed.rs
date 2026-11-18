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
}
