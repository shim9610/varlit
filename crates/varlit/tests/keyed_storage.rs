//! Bounded keyed storage on native files: a key has one record, updated in
//! place, and the file follows the live data rather than the number of
//! updates (the growth the advisory describes).

use varlit::*;

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable", key = "id")]
struct Account {
    #[varlit(field_id = 1)]
    id: u64,
    #[varlit(field_id = 2)]
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 2, version = 1, kind = "variable", key = "name")]
struct Setting {
    #[varlit(field_id = 1)]
    name: String,
    #[varlit(field_id = 2)]
    value: u64,
}

varlit_format! { struct Format {
    magic: b"KEYSTORE"; version: 1; endian: little; integrity: crc32; blocks: [Account, Setting];
} }

fn spec() -> FormatSpec {
    Format::spec().with_keyed_storage(KeyedStorage::IN_PLACE.with_ring_bytes(1 << 20))
}

fn account(id: u64, len: usize, fill: u64) -> Account {
    Account {
        id,
        bytes: (0..len)
            .map(|index| (fill as usize ^ index) as u8)
            .collect(),
    }
}

fn get_account(file: &VarlitFile, id: u64) -> Option<Account> {
    let mut out = Account {
        id: 0,
        bytes: Vec::new(),
    };
    let mut scratch = Vec::new();
    file.get_keyed::<Account>(&id, &mut scratch, &mut out)
        .unwrap()
        .then_some(out)
}

fn random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[test]
fn puts_gets_and_removes_survive_reopen() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("keyed.varlit");
    {
        let mut file = VarlitFile::create(spec(), &path)?;
        for id in 0..500 {
            file.put_keyed(&account(id, 40 + id as usize % 300, id))?;
        }
        file.put_keyed(&Setting {
            name: "limit".into(),
            value: 7,
        })?;
        assert!(file.remove_keyed::<Account>(&3)?);
        assert!(!file.remove_keyed::<Account>(&3)?);
        // The writer sees its own changes before they are published.
        assert_eq!(get_account(&file, 4), Some(account(4, 44, 4)));
        assert_eq!(get_account(&file, 3), None);
        file.sync()?;
    }
    let file = VarlitFile::open(spec(), &path)?;
    for id in 0..500 {
        let expected = (id != 3).then(|| account(id, 40 + id as usize % 300, id));
        assert_eq!(get_account(&file, id), expected, "account {id}");
    }
    let mut setting = Setting {
        name: String::new(),
        value: 0,
    };
    assert!(file.get_keyed::<Setting>(&"limit".to_string(), &mut Vec::new(), &mut setting)?);
    assert_eq!(setting.value, 7);
    assert_eq!(file.keyed_storage_stats()?.keys, 500);
    Ok(())
}

#[test]
fn unsynced_changes_are_not_published() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("unsynced.varlit");
    {
        let mut file = VarlitFile::create(spec(), &path)?;
        file.put_keyed(&account(1, 10, 1))?;
        file.sync()?;
        file.put_keyed(&account(1, 20, 2))?;
        file.put_keyed(&account(2, 20, 2))?;
        // Dropped without a sync.
    }
    let file = VarlitFile::open_readonly(spec(), &path)?;
    assert_eq!(get_account(&file, 1), Some(account(1, 10, 1)));
    assert_eq!(get_account(&file, 2), None);
    Ok(())
}

#[test]
fn a_reader_keeps_its_generation_until_it_follows() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("reader.varlit");
    let mut writer = VarlitFile::create(spec(), &path)?;
    writer.put_keyed(&account(1, 10, 1))?;
    writer.sync()?;
    let mut reader = VarlitFile::open_readonly(spec(), &path)?;
    assert_eq!(get_account(&reader, 1), Some(account(1, 10, 1)));
    writer.put_keyed(&account(1, 30, 9))?;
    writer.sync()?;
    assert_eq!(get_account(&reader, 1), Some(account(1, 10, 1)));
    reader.follow()?;
    assert_eq!(get_account(&reader, 1), Some(account(1, 30, 9)));
    Ok(())
}

/// The advisory's workload, on a real file: every key updated round after
/// round with values of changing size. Before bounded keyed storage the file
/// grew by a round's data every round; now it settles and stays there.
#[test]
fn repeated_updates_do_not_grow_the_file() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("updates.varlit");
    let mut file = VarlitFile::create(spec(), &path)?;
    let mut state = 11u64;
    let keys = 2000u64;
    let rounds = 60u64;
    let mut settled = 0;
    for round in 0..rounds {
        for id in 0..keys {
            let len = 64 + (random(&mut state) % 2000) as usize;
            file.put_keyed(&account(id, len, round))?;
        }
        file.sync()?;
        if round == 20 {
            settled = std::fs::metadata(&path)?.len();
        }
    }
    let stats = file.keyed_storage_stats()?;
    let len = std::fs::metadata(&path)?.len();
    // One more heap region (a 1 MiB record) may still be taken while the
    // holes and free regions settle; nothing past that.
    assert!(
        len <= settled + (1 << 20) + 4096,
        "the file grew from {settled} to {len} bytes between rounds 20 and {rounds}: {stats:?}"
    );
    // Forty more rounds of about `live` bytes each wrote 40 times the live
    // data; the file holds it once, in pages a B-tree keeps at least half
    // full, plus the pages freed and not yet taken again, the ring, and the
    // regions that stage a publication larger than the ring (every page
    // rewritten each round here): a constant multiple of the live data.
    assert!(
        len < 12 * stats.live_bytes,
        "file {len} bytes for {} live: {stats:?}",
        stats.live_bytes
    );
    for id in (0..keys).step_by(97) {
        assert!(get_account(&file, id).is_some());
    }
    Ok(())
}

#[test]
fn formats_without_keyed_storage_refuse_keyed_calls() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("plain.varlit");
    let mut file = VarlitFile::create(Format::spec(), &path)?;
    assert!(matches!(
        file.put_keyed(&account(1, 1, 1)),
        Err(Error::InvalidFormatSpec(_))
    ));
    Ok(())
}

#[test]
fn keyed_storage_is_part_of_the_schema() {
    assert_ne!(
        spec().computed_schema_hash(),
        Format::spec().computed_schema_hash()
    );
    assert!(
        Format::spec()
            .with_keyed_storage(KeyedStorage::IN_PLACE.with_free_space_percent(9))
            .validate()
            .is_err()
    );
    assert!(
        Format::spec()
            .with_index_policy(IndexPolicy::KeyedOffsetChain)
            .with_keyed_storage(KeyedStorage::IN_PLACE)
            .validate()
            .is_err()
    );
}

varlit_format! {
    pub format Profiles {
        magic: b"PROFILES";
        version: 1;
        schema_hash: computed;
        integrity: crc32;
        keyed_storage: in_place(free_space = 3%);
        blocks {
            variable Profile(id = 1, key = [user]) { user: String, bio: String, }
            fixed Visit(id = 2) { at: u64, }
        }
    }
}

#[test]
fn the_declaration_generates_put_get_and_remove() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("profiles.varlit");
    assert_eq!(
        Profiles::spec().keyed_storage,
        Some(KeyedStorage::IN_PLACE.with_free_space_percent(3))
    );
    let mut writer = Profiles::create_writer(&path)?;
    writer.put_profile(&Profile {
        user: "ada".into(),
        bio: "first".into(),
    })?;
    writer.put_profile(&Profile {
        user: "ada".into(),
        bio: "second, longer than the first".into(),
    })?;
    // Unkeyed blocks still append to the log.
    writer.push_visit(&Visit { at: 1 })?;
    let mut scratch = Vec::new();
    let mut out = Profile::empty();
    assert!(writer.get_profile(&"ada".to_string(), &mut scratch, &mut out)?);
    assert_eq!(out.bio, "second, longer than the first");
    assert!(writer.remove_profile(&"ada".to_string())?);
    writer.put_profile(&Profile {
        user: "bob".into(),
        bio: "b".into(),
    })?;
    writer.sync()?;
    drop(writer);
    let reader = Profiles::open_reader(&path)?;
    assert!(!reader.get_profile(&"ada".to_string(), &mut scratch, &mut out)?);
    assert!(reader.get_profile(&"bob".to_string(), &mut scratch, &mut out)?);
    assert_eq!(out.bio, "b");
    Ok(())
}

#[test]
fn keyed_blocks_of_an_in_place_format_cannot_be_appended_to_the_log() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("mixed.varlit");
    let mut file = VarlitFile::create(spec(), &path)?;
    assert!(matches!(
        file.push_info(&account(1, 1, 1)),
        Err(Error::KeyedStorageRequiresPut { block_id: 1 })
    ));
    assert!(matches!(
        file.delete::<Account>(&1),
        Err(Error::KeyedStorageRequiresPut { block_id: 1 })
    ));
    Ok(())
}
