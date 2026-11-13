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
