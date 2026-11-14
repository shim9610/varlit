//! Variable records longer than 64 KiB carry a CRC per 64 KiB chunk, plain
//! or compressed per chunk; shorter ones keep the plain layout. Every write
//! path (native, indexed) produces them and every whole-record read path
//! reads them back, verified.
#![cfg(feature = "integrity")]

use varlit::{DiskIndexOptions, Error, VarlitEmpty, varlit_format};

varlit_format! {
    pub format Plain {
        magic: b"CHUNKPLN"; version: 1; endian: little; integrity: crc32_with_header;
        blocks { variable Blob(id = 1, key = [id], key_index = disk) { id: u64, bytes: Vec<u8> } }
    }
}

#[cfg(feature = "compression-zstd")]
varlit_format! {
    pub format Packed {
        magic: b"CHUNKZST"; version: 1; endian: little; integrity: crc32_with_header;
        compression: variable_blocks(
            zstd,
            level = default,
            header = record_explicit,
            min_len = 1,
            only_if_smaller = false,
            max_len = 67108864,
        );
        blocks { variable PBlob(id = 1) { id: u64, bytes: Vec<u8> } }
    }
}

fn bytes(len: usize, seed: u64) -> Vec<u8> {
    (0..len as u64)
        .map(|i| ((i * 2654435761 + seed) >> 7) as u8)
        .collect()
}

const SIZES: [usize; 5] = [100, 65_500, 65_536, 65_537, 300_001];

/// Whether a record carrying `len` bytes is chunked, where that is clear: the
/// field framing adds a few bytes, so sizes right at a chunk are left out.
fn chunked(len: usize) -> Option<bool> {
    match len {
        0..=60_000 => Some(false),
        65_537.. => Some(true),
        _ => None,
    }
}

#[test]
fn native_and_indexed_records_round_trip_at_chunk_boundaries() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let native = dir.path().join("native");
    let indexed = dir.path().join("indexed");
    {
        let mut w = Plain::spec().create_writer(&native)?;
        let mut iw = Plain::create_indexed_writer(&indexed, DiskIndexOptions::default())?;
        for (i, len) in SIZES.iter().enumerate() {
            let blob = Blob {
                id: i as u64,
                bytes: bytes(*len, i as u64),
            };
            w.push_keyed(&blob)?;
            iw.push_blob(&blob)?;
        }
        w.sync()?;
        iw.sync()?;
    }
    let r = Plain::spec().open_reader(&native)?;
    let mut entries = Vec::new();
    r.block_entries_into::<Blob>(&mut entries)?;
    let mut scratch = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        // The payload holds the id and the length prefix besides the bytes.
        if let Some(expected) = chunked(SIZES[i]) {
            assert_eq!(entry.is_chunked(), expected, "size {}", SIZES[i]);
        }
        let mut out = Blob::empty();
        r.decode_block_into(entry, &mut scratch, &mut out)?;
        assert_eq!(out.bytes, bytes(SIZES[i], i as u64));
    }
    let ir = Plain::open_indexed_reader(&indexed, DiskIndexOptions::default())?;
    for (i, len) in SIZES.iter().enumerate() {
        let mut out = Blob::empty();
        assert!(ir.get_blob(&(i as u64), &mut out)?);
        assert_eq!(out.bytes, bytes(*len, i as u64));
    }
    Ok(())
}

#[cfg(feature = "compression-zstd")]
#[test]
fn compressed_records_are_compressed_per_chunk() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("packed");
    {
        let mut w = Packed::spec().create_writer(&path)?;
        for (i, len) in SIZES.iter().enumerate() {
            // Compressible: a short repeating pattern.
            let pattern: Vec<u8> = (0..*len).map(|j| (j % 7) as u8).collect();
            w.push(&PBlob {
                id: i as u64,
                bytes: pattern,
            })?;
        }
        w.sync()?;
    }
    let r = Packed::spec().open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<PBlob>(&mut entries)?;
    let mut scratch = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        assert!(entry.is_compressed());
        if let Some(expected) = chunked(SIZES[i]) {
            assert_eq!(entry.is_chunked(), expected);
        }
        if entry.is_chunked() {
            assert!(
                entry.payload_len < SIZES[i] as u64 / 4,
                "chunks were not compressed"
            );
        }
        let mut out = PBlob::empty();
        r.decode_block_into(entry, &mut scratch, &mut out)?;
        assert!(
            out.bytes
                .iter()
                .enumerate()
                .all(|(j, b)| *b == (j % 7) as u8)
        );
        assert_eq!(out.bytes.len(), SIZES[i]);
    }
    Ok(())
}

/// Damage inside one chunk of a long record is found on read, whichever
/// chunk it is in.
#[test]
fn a_damaged_chunk_fails_the_read() -> varlit::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("damaged");
    let info = {
        let mut w = Plain::spec().create_writer(&path)?;
        let info = w.push_keyed_info(&Blob {
            id: 1,
            bytes: bytes(200_000, 3),
        })?;
        w.sync()?;
        info
    };
    // The middle of the third chunk.
    let at = info.payload_offset + 24 + 16 * 4 + 2 * 65_536 + 1000;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?;
    file.seek(SeekFrom::Start(at))?;
    file.write_all(&[0xA5])?;
    drop(file);
    let r = Plain::spec().open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<Blob>(&mut entries)?;
    let mut out = Blob::empty();
    let mut scratch = Vec::new();
    assert!(matches!(
        r.decode_block_into(&entries[0], &mut scratch, &mut out),
        Err(Error::ChecksumMismatch { .. })
    ));
    Ok(())
}

/// Reads a whole record a buffer at a time through the continuation API.
fn read_paged(
    read_into: &dyn Fn(&mut [u8]) -> varlit::Result<varlit::RecordPage>,
    read_from: &dyn Fn(varlit::RecordCursor, &mut [u8]) -> varlit::Result<varlit::RecordPage>,
    buffer: usize,
) -> varlit::Result<Vec<u8>> {
    let mut out = vec![0u8; buffer];
    let mut all = Vec::new();
    let mut page = read_into(&mut out)?;
    loop {
        all.extend_from_slice(&out[..page.filled]);
        let Some(next) = page.next else { break };
        assert_eq!(
            page.filled, buffer,
            "a page ended before its buffer was full"
        );
        page = read_from(next, &mut out)?;
    }
    assert_eq!(all.len() as u64, page.end);
    Ok(all)
}

const BUFFERS: [usize; 4] = [1, 1000, 65_536, 1 << 20];

/// Whatever the buffer, a paged read returns exactly the logical payload a
/// whole read returns, for short, long, and (with zstd) compressed records.
#[test]
fn paged_reads_match_whole_reads_for_every_buffer_size() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("paged");
    {
        let mut w = Plain::spec().create_writer(&path)?;
        for (i, len) in SIZES.iter().enumerate() {
            w.push_keyed(&Blob {
                id: i as u64,
                bytes: bytes(*len, i as u64),
            })?;
        }
        w.sync()?;
    }
    let r = Plain::spec().open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<Blob>(&mut entries)?;
    for entry in &entries {
        let mut whole = Vec::new();
        r.read_logical_payload_into(entry, &mut whole)?;
        for buffer in BUFFERS {
            let paged = read_paged(
                &|out| r.read_record_into(entry, out),
                &|cursor, out| r.read_record_from(cursor, out),
                buffer,
            )?;
            assert_eq!(paged, whole, "buffer {buffer}, record of {}", whole.len());
        }
    }
    #[cfg(feature = "compression-zstd")]
    {
        let path = dir.path().join("paged-zstd");
        {
            let mut w = Packed::spec().create_writer(&path)?;
            for (i, len) in SIZES.iter().enumerate() {
                w.push(&PBlob {
                    id: i as u64,
                    bytes: (0..*len).map(|j| (j % 7) as u8).collect(),
                })?;
            }
            w.sync()?;
        }
        let r = Packed::spec().open_reader(&path)?;
        let mut entries = Vec::new();
        r.block_entries_into::<PBlob>(&mut entries)?;
        for entry in &entries {
            let mut whole = Vec::new();
            r.read_logical_payload_into(entry, &mut whole)?;
            for buffer in BUFFERS {
                let paged = read_paged(
                    &|out| r.read_record_into(entry, out),
                    &|cursor, out| r.read_record_from(cursor, out),
                    buffer,
                )?;
                assert_eq!(paged, whole, "zstd buffer {buffer}");
            }
        }
    }
    Ok(())
}

/// A paged read hands out every chunk before the damaged one and fails on
/// that one: nothing it reports as filled is unchecked.
#[test]
fn a_paged_read_stops_at_the_damaged_chunk() -> varlit::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("damaged-paged");
    let info = {
        let mut w = Plain::spec().create_writer(&path)?;
        let info = w.push_keyed_info(&Blob {
            id: 1,
            bytes: bytes(300_000, 9),
        })?;
        w.sync()?;
        info
    };
    let r = Plain::spec().open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<Blob>(&mut entries)?;
    let mut whole = Vec::new();
    r.read_logical_payload_into(&entries[0], &mut whole)?;
    drop(r);
    // Damage the third chunk (5 chunks: header 24 + table 80).
    let at = info.payload_offset + 24 + 16 * 5 + 2 * 65_536 + 7;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?;
    file.seek(SeekFrom::Start(at))?;
    file.write_all(&[0x5A])?;
    drop(file);
    let r = Plain::spec().open_reader(&path)?;
    let mut out = vec![0u8; 65_536];
    let page = r.read_record_into(&entries[0], &mut out)?;
    assert_eq!(&out[..page.filled], &whole[..65_536]);
    let page = r.read_record_from(page.next.unwrap(), &mut out)?;
    assert_eq!(&out[..page.filled], &whole[65_536..131_072]);
    assert!(matches!(
        r.read_record_from(page.next.unwrap(), &mut out),
        Err(Error::ChecksumMismatch { .. })
    ));
    // A buffer spanning the damage fails as a whole.
    let mut big = vec![0u8; 1 << 20];
    assert!(r.read_record_into(&entries[0], &mut big).is_err());
    Ok(())
}

/// The indexed reader finds a key's record and reads it a buffer at a time.
#[test]
fn an_indexed_key_reads_through_a_caller_buffer() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("indexed-paged");
    {
        let mut w = Plain::create_indexed_writer(&path, DiskIndexOptions::default())?;
        w.push_blob(&Blob {
            id: 5,
            bytes: bytes(250_000, 5),
        })?;
        w.push_blob(&Blob {
            id: 6,
            bytes: bytes(10, 6),
        })?;
        w.sync()?;
    }
    let r = varlit::VarlitIndexedReader::open(
        Plain::spec(),
        &path,
        DiskIndexOptions::default(),
        Plain::disk_index_plan()?,
    )?;
    let entry = r.lookup_entry::<Blob>(&5)?.expect("present");
    let paged = read_paged(
        &|out| r.read_record_into(&entry, out),
        &|cursor, out| r.read_record_from(cursor, out),
        4096,
    )?;
    let mut decoded = Blob::empty();
    assert!(r.get::<Blob>(&5, &mut decoded)?);
    assert!(paged.len() > 250_000);
    assert!(paged.ends_with(&decoded.bytes));
    assert!(r.lookup_entry::<Blob>(&99)?.is_none());
    Ok(())
}

/// A record crossing anchors reads the same paged as whole.
#[test]
fn a_record_across_anchors_reads_paged() -> varlit::Result<()> {
    let spec = Plain::spec().with_anchors(varlit::AnchorPolicy::every(13, true));
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("anchored");
    {
        let mut w = spec.create_writer(&path)?;
        w.push_keyed(&Blob {
            id: 1,
            bytes: bytes(200_000, 1),
        })?;
        w.sync()?;
    }
    let r = spec.open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<Blob>(&mut entries)?;
    let mut whole = Vec::new();
    r.read_logical_payload_into(&entries[0], &mut whole)?;
    for buffer in [777, 65_536] {
        let paged = read_paged(
            &|out| r.read_record_into(&entries[0], out),
            &|cursor, out| r.read_record_from(cursor, out),
            buffer,
        )?;
        assert_eq!(paged, whole);
    }
    Ok(())
}

varlit_format! {
    pub format Frames {
        magic: b"CHUNKVEW"; version: 1; endian: little; integrity: crc32_with_header;
        blocks {
            variable Frame(id = 1) {
                seq: u64,
                pixels: Vec<u16>,
                note: String = default,
                label: String,
            }
        }
    }
}

/// Reads a span's data through a caller buffer of `buffer` bytes.
fn read_span_paged(
    r: &varlit::VarlitReader,
    entry: &varlit::RecordIndexEntry,
    span: varlit::FieldSpan,
    buffer: usize,
) -> varlit::Result<Vec<u8>> {
    let mut out = vec![0u8; buffer];
    let mut all = Vec::new();
    let mut page = r.read_span_into(entry, span, &mut out)?;
    loop {
        all.extend_from_slice(&out[..page.filled]);
        let Some(next) = page.next else { break };
        page = r.read_record_from(next, &mut out)?;
    }
    assert_eq!(all.len() as u64, span.len());
    Ok(all)
}

/// A view finds every field without reading the large ones; a scalar decodes
/// from what the view carries, a large field reads into the caller's buffer.
#[test]
fn a_view_reads_large_fields_into_caller_buffers() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("view");
    {
        let mut w = Plain::spec().create_writer(&path)?;
        for (i, len) in [100usize, 300_001].iter().enumerate() {
            w.push_keyed(&Blob {
                id: 70 + i as u64,
                bytes: bytes(*len, i as u64),
            })?;
        }
        w.sync()?;
    }
    let r = Plain::spec().open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<Blob>(&mut entries)?;
    for (i, (entry, len)) in entries.iter().zip([100usize, 300_001]).enumerate() {
        let view: BlobView = r.view_block::<Blob>(entry)?;
        assert_eq!(r.decode_field::<u64>(entry, view.id)?, 70 + i as u64);
        let data = view.bytes.data()?;
        assert_eq!(data.len(), len as u64);
        for buffer in [4096, 65_536] {
            assert_eq!(
                read_span_paged(&r, entry, data, buffer)?,
                bytes(len, i as u64)
            );
        }
    }
    Ok(())
}

/// A pixel column's packed values come out as raw little-endian bytes; an
/// absent defaulted field is `None`; a short string decodes from the view.
#[test]
fn a_view_exposes_packed_sequences_and_optional_fields() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("frames");
    let pixels: Vec<u16> = (0..40_000u32).map(|i| (i * 7) as u16).collect();
    {
        let mut w = Frames::spec().create_writer(&path)?;
        w.push(&Frame {
            seq: 9,
            pixels: pixels.clone(),
            note: String::new(),
            label: "cam-3".into(),
        })?;
        w.sync()?;
    }
    let r = Frames::spec().open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<Frame>(&mut entries)?;
    let entry = &entries[0];
    assert!(entry.is_chunked());
    let view = r.view_block::<Frame>(entry)?;
    assert_eq!(r.decode_field::<u64>(entry, view.seq)?, 9);
    assert_eq!(r.decode_field::<String>(entry, view.label)?, "cam-3");
    let raw = read_span_paged(&r, entry, view.pixels.data()?, 10_000)?;
    let expected: Vec<u8> = pixels.iter().flat_map(|p| p.to_le_bytes()).collect();
    assert_eq!(raw, expected);
    // `note` was written (it is empty, not absent): its span is there.
    assert!(view.note.is_some());
    // A view of another block's record is refused.
    let mut other = Vec::new();
    {
        let path = dir.path().join("plain");
        let mut w = Plain::spec().create_writer(&path)?;
        w.push_keyed(&Blob {
            id: 1,
            bytes: vec![1],
        })?;
        w.sync()?;
        drop(w);
        let pr = Plain::spec().open_reader(&path)?;
        pr.block_entries_into::<Blob>(&mut other)?;
    }
    assert!(r.view_block::<Frame>(&entries[0]).is_ok());
    let mut wrong = other[0].clone();
    wrong.block_id = 99;
    assert!(r.view_block::<Frame>(&wrong).is_err());
    Ok(())
}

/// An indexed key's record reads field by field through caller buffers.
#[test]
fn an_indexed_view_reads_a_field_through_a_caller_buffer() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("indexed-view");
    {
        let mut w = Plain::create_indexed_writer(&path, DiskIndexOptions::default())?;
        w.push_blob(&Blob {
            id: 8,
            bytes: bytes(150_000, 8),
        })?;
        w.sync()?;
    }
    // Through the generated typed reader.
    let r = Plain::open_indexed_reader(&path, DiskIndexOptions::default())?;
    let entry = r.lookup_entry::<Blob>(&8)?.expect("present");
    let view = r.view_block::<Blob>(&entry)?;
    assert_eq!(r.decode_field::<u64>(&entry, view.id)?, 8);
    let data = view.bytes.data()?;
    let mut out = vec![0u8; 7000];
    let mut all = Vec::new();
    let mut page = r.read_span_into(&entry, data, &mut out)?;
    loop {
        all.extend_from_slice(&out[..page.filled]);
        let Some(next) = page.next else { break };
        page = r.read_record_from(next, &mut out)?;
    }
    assert_eq!(all, bytes(150_000, 8));
    Ok(())
}

/// A view of a compressed, chunked record reads its fields the same way:
/// only the chunks holding what is asked for are decompressed.
#[cfg(feature = "compression-zstd")]
#[test]
fn a_view_of_a_compressed_record_reads_fields() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("view-zstd");
    let payload: Vec<u8> = (0..250_000).map(|j| (j % 11) as u8).collect();
    {
        let mut w = Packed::spec().create_writer(&path)?;
        w.push(&PBlob {
            id: 42,
            bytes: payload.clone(),
        })?;
        w.sync()?;
    }
    let r = Packed::spec().open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<PBlob>(&mut entries)?;
    let entry = &entries[0];
    assert!(entry.is_chunked() && entry.is_compressed());
    let view = r.view_block::<PBlob>(entry)?;
    assert_eq!(r.decode_field::<u64>(entry, view.id)?, 42);
    assert_eq!(
        read_span_paged(&r, entry, view.bytes.data()?, 3000)?,
        payload
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Typed decodes of chunked records fill the caller's value from storage
// ---------------------------------------------------------------------------

varlit_format! {
    pub format Mixed {
        magic: b"CHUNKMIX"; version: 1; endian: little; integrity: crc32_with_header;
        blocks { variable Frame2(id = 1) {
            id: u64,
            note: String,
            raw: Vec<u8>,
            samples: Vec<u16>,
            wide: Vec<f64>,
            flags: Vec<bool>,
            tags: Vec<String>,
            tail: u32,
        } }
    }
}

varlit_format! {
    pub format MixedBig {
        magic: b"CHUNKMIX"; version: 1; endian: big; integrity: crc32_with_header;
        blocks { variable Frame2Big(id = 1) {
            id: u64,
            note: String,
            raw: Vec<u8>,
            samples: Vec<u16>,
            wide: Vec<f64>,
            flags: Vec<bool>,
            tags: Vec<String>,
            tail: u32,
        } }
    }
}

// The same block id read with other field types: what the walk must refuse
// or default exactly as a decode of the whole payload does.
varlit_format! {
    pub format MixedOther {
        magic: b"CHUNKMIX"; version: 1; endian: little; integrity: crc32_with_header;
        blocks { variable Narrow(id = 1) { id: u64, note: String, raw: Vec<u8> } }
    }
}
varlit_format! {
    pub format MixedWrongWidth {
        magic: b"CHUNKMIX"; version: 1; endian: little; integrity: crc32_with_header;
        blocks { variable Widened(id = 1) { id: u64, note: String, raw: Vec<u8>, samples: Vec<u64> } }
    }
}
varlit_format! {
    pub format MixedNarrowWidth {
        magic: b"CHUNKMIX"; version: 1; endian: little; integrity: crc32_with_header;
        blocks { variable Narrowed(id = 1) {
            id: u64, note: String, raw: Vec<u8>, samples: Vec<u16>, wide: Vec<u16>,
        } }
    }
}
varlit_format! {
    pub format MixedWrongWire {
        magic: b"CHUNKMIX"; version: 1; endian: little; integrity: crc32_with_header;
        blocks { variable Rewired(id = 1) { id: u64, note: String, raw: String } }
    }
}
varlit_format! {
    pub format MixedMissing {
        magic: b"CHUNKMIX"; version: 1; endian: little; integrity: crc32_with_header;
        blocks { variable Wanting(id = 1) {
            id: u64, note: String, raw: Vec<u8>, samples: Vec<u16>, wide: Vec<f64>,
            flags: Vec<bool>, tags: Vec<String>, tail: u32, absent: Vec<u8>,
        } }
    }
}
varlit_format! {
    pub format MixedDefaulted {
        magic: b"CHUNKMIX"; version: 1; endian: little; integrity: crc32_with_header;
        blocks { variable Defaulted(id = 1) {
            id: u64, note: String, raw: Vec<u8>, samples: Vec<u16>, wide: Vec<f64>,
            flags: Vec<bool>, tags: Vec<String>, tail: u32, extra: Vec<u8> = default,
        } }
    }
}

macro_rules! frame {
    ($ty:ident, $seed:expr, $scale:expr) => {{
        let seed: u64 = $seed;
        let scale: usize = $scale;
        $ty {
            id: seed,
            note: "n".repeat(scale / 3 + 1),
            raw: bytes(scale, seed),
            samples: (0..scale / 2)
                .map(|i| (i as u64 * 31 + seed) as u16)
                .collect(),
            wide: (0..scale / 8)
                .map(|i| i as f64 * 0.5 - seed as f64)
                .collect(),
            flags: (0..scale / 16).map(|i| i % 3 == 0).collect(),
            tags: (0..5).map(|i| format!("tag-{i}-{seed}")).collect(),
            tail: seed as u32 ^ 0xA5A5,
        }
    }};
}

/// The value a decode of the whole logical payload gives, for comparison.
fn slice_decode<T: varlit::VarlitBlock>(
    r: &varlit::VarlitReader,
    entry: &varlit::RecordIndexEntry,
    endian: varlit::Endian,
) -> varlit::Result<T> {
    let mut payload = Vec::new();
    r.read_logical_payload_into(entry, &mut payload)?;
    varlit::decode_from_slice::<T>(&payload, endian)
}

/// Every field kind, at sizes on both sides of a chunk, decodes to exactly
/// what a decode of the whole payload gives, whatever the value held before,
/// and the scratch buffer never holds a large field.
#[test]
fn typed_decodes_of_chunked_records_match_a_whole_payload_decode() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("mixed");
    let scales = [10usize, 3_000, 40_000, 70_000, 200_000, 700_001];
    {
        let mut w = Mixed::spec().create_writer(&path)?;
        for (i, scale) in scales.iter().enumerate() {
            w.push(&frame!(Frame2, i as u64 + 1, *scale))?;
        }
        w.sync()?;
    }
    let r = Mixed::spec().open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<Frame2>(&mut entries)?;
    assert_eq!(entries.len(), scales.len());
    let mut scratch = Vec::new();
    // One value reused across records of every size, growing and shrinking.
    let mut out = Frame2::empty();
    for order in [[0usize, 1, 2, 3, 4, 5], [5, 0, 4, 1, 3, 2]] {
        for i in order {
            let entry = &entries[i];
            r.decode_block_into(entry, &mut scratch, &mut out)?;
            assert_eq!(
                out,
                frame!(Frame2, i as u64 + 1, scales[i]),
                "scale {}",
                scales[i]
            );
            assert_eq!(
                out,
                slice_decode::<Frame2>(&r, entry, varlit::Endian::Little)?
            );
            if entry.is_chunked() {
                // Only the fields without an in-place fill (the scalars and
                // the short tag list) pass through scratch.
                let mut fresh = Vec::new();
                r.decode_block_into(entry, &mut fresh, &mut out)?;
                assert!(
                    fresh.capacity() < 4 * 1024,
                    "scratch held a large field: {} bytes",
                    fresh.capacity()
                );
            }
        }
        scratch = Vec::new();
    }
    // The paged typed read takes the same path.
    let mut page_out = vec![Frame2::empty(); 2];
    let mut seen = Vec::new();
    let mut page = r.blocks::<Frame2>(&mut scratch, &mut page_out)?;
    loop {
        seen.extend_from_slice(&page_out[..page.filled]);
        match page.next {
            Some(next) => page = r.blocks_from::<Frame2>(next, &mut scratch, &mut page_out)?,
            None => break,
        }
    }
    assert_eq!(seen.len(), scales.len());
    for (i, value) in seen.iter().enumerate() {
        assert_eq!(*value, frame!(Frame2, i as u64 + 1, scales[i]));
    }
    Ok(())
}

/// A big-endian file's numeric vectors are converted in place.
#[test]
fn typed_decodes_convert_big_endian_vectors() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("mixed-big");
    {
        let mut w = MixedBig::spec().create_writer(&path)?;
        w.push(&frame!(Frame2Big, 9, 300_000))?;
        w.sync()?;
    }
    let r = MixedBig::spec().open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<Frame2Big>(&mut entries)?;
    assert!(entries[0].is_chunked());
    let mut out = Frame2Big::empty();
    r.decode_block_into(&entries[0], &mut Vec::new(), &mut out)?;
    assert_eq!(out, frame!(Frame2Big, 9, 300_000));
    assert_eq!(
        out,
        slice_decode::<Frame2Big>(&r, &entries[0], varlit::Endian::Big)?
    );
    Ok(())
}

/// Read as a block with other fields, a chunked record is refused, or its
/// unknown fields skipped and defaults applied, exactly as by a decode of
/// its whole payload.
#[test]
fn typed_decodes_refuse_what_a_whole_payload_decode_refuses() -> varlit::Result<()> {
    fn same<T: varlit::VarlitBlock + VarlitEmpty + PartialEq + std::fmt::Debug>(
        spec: varlit::FormatSpec,
        path: &std::path::Path,
    ) -> varlit::Result<Option<T>> {
        let r = spec.open_reader(path)?;
        let mut entries = Vec::new();
        r.block_entries_into::<T>(&mut entries)?;
        assert!(entries[0].is_chunked());
        let mut out = T::empty();
        let walked = r.decode_block_into(&entries[0], &mut Vec::new(), &mut out);
        let whole = slice_decode::<T>(&r, &entries[0], varlit::Endian::Little);
        match (walked, whole) {
            (Ok(()), Ok(whole)) => {
                assert_eq!(out, whole);
                Ok(Some(out))
            }
            (Err(walked), Err(whole)) => {
                assert_eq!(format!("{walked:?}"), format!("{whole:?}"));
                Ok(None)
            }
            (walked, whole) => panic!("walk {walked:?} but whole payload {:?}", whole.map(drop)),
        }
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("mixed-other");
    {
        let mut w = Mixed::spec().create_writer(&path)?;
        w.push(&frame!(Frame2, 4, 300_000))?;
        w.sync()?;
    }
    // Unknown fields are skipped without being read; a default applies.
    let narrow = same::<Narrow>(MixedOther::spec(), &path)?.expect("decodes");
    assert_eq!(narrow.raw, bytes(300_000, 4));
    let defaulted = same::<Defaulted>(MixedDefaulted::spec(), &path)?.expect("decodes");
    assert_eq!(defaulted.raw, bytes(300_000, 4));
    assert!(defaulted.extra.is_empty());
    // A count the data cannot hold, and data past the count.
    assert!(same::<Widened>(MixedWrongWidth::spec(), &path)?.is_none());
    assert!(same::<Narrowed>(MixedNarrowWidth::spec(), &path)?.is_none());
    // Bytes are not a string field; a required field is absent.
    assert!(same::<Rewired>(MixedWrongWire::spec(), &path)?.is_none());
    assert!(same::<Wanting>(MixedMissing::spec(), &path)?.is_none());
    Ok(())
}

/// The materialization limit refuses a large field before it is allocated.
#[test]
fn typed_decodes_charge_the_materialization_limit() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("mixed-limit");
    {
        let mut w = Mixed::spec().create_writer(&path)?;
        w.push(&frame!(Frame2, 2, 300_000))?;
        w.sync()?;
    }
    let mut limits = varlit::ReadLimits::standard();
    limits.max_materialized_bytes = varlit::ReadLimit::Finite(1_200_000);
    let spec = Mixed::spec().with_read_limits(limits);
    let r = spec.open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<Frame2>(&mut entries)?;
    let mut out = Frame2::empty();
    let walked = r.decode_block_into(&entries[0], &mut Vec::new(), &mut out);
    assert!(
        matches!(walked, Err(Error::LimitExceeded { .. })),
        "{walked:?}"
    );
    Ok(())
}
