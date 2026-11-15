//! `MmapPayloads::decode_block_into`: decoding straight from a read-only
//! mapping must give exactly what the copying read gives, charge the same
//! limits, and refuse the same entries.
#![cfg(feature = "mmap")]

use varlit::{Error, ReadLimit, VarlitBlock, VarlitEmpty, VarlitFile, varlit_format};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 700, version = 1, kind = "variable")]
struct Doc {
    #[varlit(field_id = 1)]
    id: u32,
    #[varlit(field_id = 2)]
    body: Vec<u8>,
    #[varlit(field_id = 3)]
    tags: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 701, version = 1, kind = "fixed")]
struct Tick {
    t: u64,
    x: [u16; 64],
}

varlit_format! {
    pub struct PlainFormat {
        magic: b"MMDI1";
        version: 1;
        endian: little;
        blocks: [Doc, Tick];
    }
}

#[cfg(feature = "compression-zstd")]
varlit_format! {
    pub struct ZstdFormat {
        magic: b"MMDI2";
        version: 1;
        endian: little;
        compression: variable_blocks(
            zstd,
            level = default,
            header = record_explicit,
            min_len = 32,
            only_if_smaller = true,
            max_len = 1048576,
        );
        blocks: [Doc, Tick];
    }
}

fn doc(i: u32) -> Doc {
    Doc {
        id: i,
        // Repetitive so a compressing format actually compresses it.
        body: (0..4096u32).map(|v| (v % 7) as u8 + i as u8).collect(),
        tags: (0..i % 5).map(|k| format!("tag-{i}-{k}")).collect(),
    }
}

fn tick(i: u64) -> Tick {
    Tick {
        t: i,
        x: std::array::from_fn(|j| (i as u16).wrapping_mul(13).wrapping_add(j as u16)),
    }
}

/// Every record decoded through the mapping equals the copying read, into an
/// `out` that is reused (and therefore holds the previous record) each time.
fn check_equivalence(file: &VarlitFile) -> varlit::Result<()> {
    // SAFETY: the test owns the file and does not modify it while mapped.
    let map = unsafe { file.mmap_payloads()? };
    let mut docs = Vec::new();
    file.block_entries_into::<Doc>(&mut docs)?;
    let mut ticks = Vec::new();
    file.block_entries_into::<Tick>(&mut ticks)?;
    assert!(!docs.is_empty() && !ticks.is_empty());
    let (mut scratch_a, mut scratch_b) = (Vec::new(), Vec::new());
    let (mut mapped, mut copied) = (Doc::empty(), Doc::empty());
    for entry in &docs {
        map.decode_block_into(entry, &mut scratch_a, &mut mapped)?;
        file.decode_block_into(entry, &mut scratch_b, &mut copied)?;
        assert_eq!(mapped, copied);
    }
    let (mut mapped, mut copied) = (Tick::empty(), Tick::empty());
    for entry in &ticks {
        map.decode_block_into(entry, &mut scratch_a, &mut mapped)?;
        file.decode_block_into(entry, &mut scratch_b, &mut copied)?;
        assert_eq!(mapped, copied);
    }
    // An entry of another block type is refused, as by the copying read.
    let mut wrong = Tick::empty();
    assert!(matches!(
        map.decode_block_into(&docs[0], &mut scratch_a, &mut wrong),
        Err(Error::UnregisteredBlock(700))
    ));
    Ok(())
}

#[test]
fn mapped_decode_equals_the_copying_read() -> varlit::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.varlit");
    {
        let mut file = PlainFormat::create(&path)?;
        for i in 0..20 {
            file.push(&doc(i))?;
            file.push(&tick(i as u64))?;
        }
        file.flush()?;
    }
    let file = PlainFormat::open_readonly(&path)?;
    check_equivalence(&file)
}

#[cfg(feature = "compression-zstd")]
#[test]
fn mapped_decode_of_compressed_records_equals_the_copying_read() -> varlit::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("zstd.varlit");
    {
        let mut file = ZstdFormat::create(&path)?;
        for i in 0..20 {
            file.push(&doc(i))?;
            file.push(&tick(i as u64))?;
        }
        file.flush()?;
    }
    let file = ZstdFormat::open_readonly(&path)?;
    let mut entries = Vec::new();
    file.block_entries_into::<Doc>(&mut entries)?;
    assert!(
        entries.iter().all(|e| e.is_compressed()),
        "fixture must compress"
    );
    check_equivalence(&file)
}

/// The mapped read is admitted against the same materialization ceiling as
/// the copying read: both accept at the record's size and both refuse one
/// byte below it, before any decode.
#[test]
fn mapped_decode_refuses_at_the_same_materialization_limit() -> varlit::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("limit.varlit");
    let value = doc(3);
    {
        let mut file = PlainFormat::create(&path)?;
        file.push(&value)?;
        file.flush()?;
    }
    let len = {
        let file = PlainFormat::open_readonly(&path)?;
        let mut entries = Vec::new();
        file.block_entries_into::<Doc>(&mut entries)?;
        entries[0].payload_len
    };
    // The decoded value materializes more than its payload, so find the exact
    // ceiling the copying read needs and require the mapped read to agree.
    let accepts = |limit: u64| -> varlit::Result<(bool, bool)> {
        let spec = PlainFormat::spec();
        let mut limits = spec.read_limits;
        limits.max_materialized_bytes = ReadLimit::Finite(limit);
        let file = VarlitFile::open_readonly(spec.with_read_limits(limits), &path)?;
        let mut entries = Vec::new();
        file.block_entries_into::<Doc>(&mut entries)?;
        // SAFETY: the file is not modified while mapped.
        let map = unsafe { file.mmap_payloads()? };
        let mut scratch = Vec::new();
        let mut out = Doc::empty();
        let copied = file.decode_block_into(&entries[0], &mut scratch, &mut out);
        let mapped = map.decode_block_into(&entries[0], &mut scratch, &mut out);
        for result in [&copied, &mapped] {
            if let Err(error) = result {
                assert!(matches!(error, Error::LimitExceeded { .. }), "{error:?}");
            }
        }
        Ok((copied.is_ok(), mapped.is_ok()))
    };
    let (mut lo, mut hi) = (len, len * 4 + (1 << 16));
    assert_eq!(accepts(hi)?, (true, true));
    while lo < hi {
        let mid = (lo + hi) / 2;
        if accepts(mid)?.0 {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    assert_eq!(accepts(lo)?, (true, true));
    assert_eq!(accepts(lo - 1)?, (false, false));
    Ok(())
}
