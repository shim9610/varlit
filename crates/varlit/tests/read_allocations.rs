//! Reads fill memory the caller provides: once a handle is open and its
//! buffers have been used once, a read allocates nothing.
//!
//! Each case runs the read once to settle the caller's own buffers (a value's
//! vectors, a scratch buffer) and the handle's, then counts allocator calls
//! over further identical reads.
#![cfg(all(feature = "integrity", feature = "compression-zstd"))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use varlit::{
    DiskIndexOptions, FormatSpec, MatrixDimensions, MatrixKey, StreamOptions, VarlitEmpty,
    varlit_format,
};

struct CountingAllocator;

std::thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

std::thread_local! {
    /// Set while a settled read is being counted with `READ_ALLOC_TRACE=1`:
    /// each allocation then prints where it came from.
    static TRACING: Cell<bool> = const { Cell::new(false) };
    static PRINTING: Cell<bool> = const { Cell::new(false) };
}

fn count_allocation() {
    // While a trace is being captured and printed, its own allocations are
    // neither counted nor traced.
    if PRINTING.try_with(Cell::get).unwrap_or(true) {
        return;
    }
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    if TRACING.try_with(Cell::get).unwrap_or(false) {
        PRINTING.set(true);
        let trace = std::backtrace::Backtrace::force_capture().to_string();
        let frames: Vec<&str> = trace
            .lines()
            .filter(|line| line.contains("crates/varlit") && !line.contains("read_allocations"))
            .take(8)
            .collect();
        eprintln!("READ_ALLOC_TRACE\n{}", frames.join("\n"));
        PRINTING.set(false);
    }
}

// SAFETY: every call is forwarded unchanged to the system allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count_allocation();
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Allocator calls made by `rounds` runs of `read`, after one settling run.
fn steady(rounds: u64, mut read: impl FnMut() -> varlit::Result<()>) -> varlit::Result<u64> {
    read()?;
    // Read before counting: looking the variable up allocates.
    // `all` traces every round, anything else the first.
    let trace = std::env::var("READ_ALLOC_TRACE")
        .ok()
        .map(|mode| mode == "all");
    let before = ALLOCATIONS.with(Cell::get);
    for round in 0..rounds {
        TRACING.set(trace == Some(true) || (trace == Some(false) && round == 0));
        let result = read();
        TRACING.set(false);
        result?;
    }
    Ok(ALLOCATIONS.with(Cell::get) - before)
}

varlit_format! {
    pub format Plain {
        magic: b"RALLOCPL"; version: 1; endian: little; integrity: crc32_with_header;
        blocks {
            fixed Tick(id = 1) { at: u64, value: f64 }
            variable Blob(id = 2, key = [id], key_index = disk) {
                id: u64, name: String, bytes: Vec<u8>, samples: Vec<u32>,
            }
        }
    }
}

varlit_format! {
    pub format Packed {
        magic: b"RALLOCZS"; version: 1; endian: little; integrity: crc32_with_header;
        compression: variable_blocks(
            zstd, level = default, header = record_explicit,
            min_len = 1, only_if_smaller = false, max_len = 67108864,
        );
        blocks { variable PBlob(id = 2) { id: u64, name: String, bytes: Vec<u8>, samples: Vec<u32>, } }
    }
}

varlit_format! {
    pub format Grid {
        magic: b"RALLOCMX"; version: 1; endian: little; schema_hash: computed;
        integrity: crc32_with_header;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks { matrix Slot(id = 1, dims = [row, col], category = data, tile = [4, 8]) { value: u64, } }
    }
}

macro_rules! blob {
    ($ty:ident, $id:expr, $len:expr) => {{
        let len: usize = $len;
        $ty {
            id: $id,
            name: "n".repeat(len / 7 + 3),
            bytes: (0..len).map(|i| (i % 13) as u8).collect(),
            samples: (0..len / 4).map(|i| i as u32 % 97).collect(),
        }
    }};
}

/// A short record (one checksum over all of it) and a long one (chunked).
const SIZES: [usize; 2] = [900, 300_000];

type Label = std::borrow::Cow<'static, str>;
type Case = (Label, u64);

fn report(title: &str, cases: &[Case]) {
    let mut failed = false;
    for (name, count) in cases {
        println!("READ_ALLOC {title:<10} {name:<34} {count}");
        failed |= *count != 0;
    }
    assert!(
        !failed,
        "{title}: a settled read allocated (see READ_ALLOC lines)"
    );
}

#[test]
fn settled_native_reads_allocate_nothing() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("plain");
    {
        let mut w = Plain::spec().create_writer(&path)?;
        w.push(&Tick { at: 1, value: 2.0 })?;
        for (i, len) in SIZES.iter().enumerate() {
            w.push_keyed(&blob!(Blob, i as u64, *len))?;
        }
        w.sync()?;
    }
    let r = Plain::spec().open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<Blob>(&mut entries)?;
    let mut ticks = Vec::new();
    r.block_entries_into::<Tick>(&mut ticks)?;
    let mut cases: Vec<Case> = Vec::new();
    let mut scratch = Vec::new();
    let mut tick = Tick::empty();
    cases.push((
        "fixed decode_block_into".into(),
        steady(8, || {
            r.decode_block_into(&ticks[0], &mut scratch, &mut tick)
        })?,
    ));
    for (entry, label) in entries.iter().zip(["short", "chunked"]) {
        let mut out = Blob::empty();
        let mut scratch = Vec::new();
        let name = |what: &str| -> Label { format!("{label} {what}").into() };
        let n = name("decode_block_into");
        cases.push((
            n,
            steady(8, || r.decode_block_into(entry, &mut scratch, &mut out))?,
        ));
        let n = name("read_logical_payload_into");
        cases.push((
            n,
            steady(8, || r.read_logical_payload_into(entry, &mut scratch))?,
        ));
        for buffer in [512usize, 64 * 1024, 1 << 20] {
            let mut buf = vec![0u8; buffer];
            let n = name(&format!("read_record_into {buffer}"));
            cases.push((
                n,
                steady(4, || {
                    let mut page = r.read_record_into(entry, &mut buf)?;
                    while let Some(next) = page.next {
                        page = r.read_record_from(next, &mut buf)?;
                    }
                    Ok(())
                })?,
            ));
        }
        let n = name("decode_field name");
        let mut settled = false;
        cases.push((
            n,
            steady(8, || {
                let view = r.view_block::<Blob>(entry)?;
                // Decoded into a new `String`: the one allocation here is
                // the value this call returns to its caller.
                let before = ALLOCATIONS.with(Cell::get);
                let text = r.decode_field::<String>(entry, view.name)?;
                let made = ALLOCATIONS.with(Cell::get) - before;
                assert_eq!(text, out.name);
                assert!(
                    !settled || made == 1,
                    "decode_field allocated beyond its value"
                );
                settled = true;
                ALLOCATIONS.with(|count| count.set(count.get() - 1));
                Ok(())
            })?,
        ));
        let mut buf = vec![0u8; 4096];
        let n = name("view_block + read_span_into");
        cases.push((
            n,
            steady(4, || {
                let view = r.view_block::<Blob>(entry)?;
                assert_eq!(r.decode_field::<u64>(entry, view.id)?, out.id);
                let mut page = r.read_span_into(entry, view.bytes.data()?, &mut buf)?;
                while let Some(next) = page.next {
                    page = r.read_record_from(next, &mut buf)?;
                }
                Ok(())
            })?,
        ));
    }
    let mut page_out = vec![Blob::empty(); 2];
    let mut payload = Vec::new();
    cases.push((
        "paged blocks".into(),
        steady(4, || {
            let mut page = r.blocks::<Blob>(&mut payload, &mut page_out)?;
            while let Some(next) = page.next {
                page = r.blocks_from::<Blob>(next, &mut payload, &mut page_out)?;
            }
            Ok(())
        })?,
    ));
    let mut index_page = vec![varlit::RecordIndexEntry::default(); 2];
    cases.push((
        "paged index_entries".into(),
        steady(4, || {
            let mut page = r.index_entries(&mut index_page)?;
            while let Some(next) = page.next {
                page = r.index_entries_from(next, &mut index_page)?;
            }
            Ok(())
        })?,
    ));
    let mut all_entries = Vec::new();
    cases.push((
        "index_entries_into".into(),
        steady(4, || r.index_entries_into(&mut all_entries))?,
    ));
    cases.push((
        "block_entries_into".into(),
        steady(4, || r.block_entries_into::<Blob>(&mut all_entries))?,
    ));
    let mut decoded: Vec<Blob> = Vec::new();
    cases.push((
        "decode_blocks_into".into(),
        steady(4, || r.decode_blocks_into::<Blob>(&mut decoded))?,
    ));
    let mut raw = Vec::new();
    cases.push((
        "read_payload_into chunked".into(),
        steady(4, || r.read_payload_into(&entries[1], &mut raw))?,
    ));
    // A keyed pass decodes each record to read its key. It does so into one
    // value it makes for the pass (a handle cannot keep a value of a type it
    // does not know), so a pass allocates that value's fields once, however
    // many records it visits: bounded here, and not counted as a read below.
    let mut tails = std::collections::HashMap::new();
    let mut by_key = std::collections::HashMap::new();
    let per_pass = [
        steady(4, || r.key_tail_offsets_into::<Blob>(&mut tails))? / 4,
        steady(4, || {
            r.keyed_blocks_into::<Blob>(&mut all_entries, &mut by_key)
        })? / 4,
    ];
    println!("READ_ALLOC native     keyed passes (one value each)      {per_pass:?}");
    assert!(
        per_pass.iter().all(|count| *count <= 8),
        "a keyed pass allocated more than its one scratch value: {per_pass:?}"
    );
    cases.push(("verify_all".into(), steady(4, || r.verify_all().map(drop))?));
    cases.push((
        "scan".into(),
        steady(4, || {
            for event in r.scan() {
                event?;
            }
            Ok(())
        })?,
    ));
    #[cfg(feature = "mmap")]
    {
        // SAFETY: the fixture is not modified while the mapping is alive.
        let mapped = unsafe { r.mmap_payloads()? };
        let mut out = Blob::empty();
        let mut scratch = Vec::new();
        for (entry, label) in entries.iter().zip(["mmap short", "mmap chunked"]) {
            let n: Label = format!("{label} decode_block_into").into();
            cases.push((
                n,
                steady(8, || {
                    mapped.decode_block_into(entry, &mut scratch, &mut out)
                })?,
            ));
        }
        cases.push((
            "mmap payload_window".into(),
            steady(8, || mapped.payload_window(&entries[0]).map(drop))?,
        ));
    }
    report("native", &cases);
    Ok(())
}

#[test]
fn settled_compressed_reads_allocate_nothing() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("packed");
    {
        let mut w = Packed::spec().create_writer(&path)?;
        for (i, len) in SIZES.iter().enumerate() {
            w.push(&blob!(PBlob, i as u64, *len))?;
        }
        w.sync()?;
    }
    let r = Packed::spec().open_reader(&path)?;
    let mut entries = Vec::new();
    r.block_entries_into::<PBlob>(&mut entries)?;
    let mut cases: Vec<Case> = Vec::new();
    for (entry, label) in entries.iter().zip(["short", "chunked"]) {
        let mut out = PBlob::empty();
        let mut scratch = Vec::new();
        let name = |what: &str| -> Label { format!("{label} {what}").into() };
        let n = name("decode_block_into");
        cases.push((
            n,
            steady(8, || r.decode_block_into(entry, &mut scratch, &mut out))?,
        ));
        let n = name("read_logical_payload_into");
        cases.push((
            n,
            steady(8, || r.read_logical_payload_into(entry, &mut scratch))?,
        ));
        for buffer in [512usize, 64 * 1024] {
            let mut buf = vec![0u8; buffer];
            let n = name(&format!("read_record_into {buffer}"));
            cases.push((
                n,
                steady(4, || {
                    let mut page = r.read_record_into(entry, &mut buf)?;
                    while let Some(next) = page.next {
                        page = r.read_record_from(next, &mut buf)?;
                    }
                    Ok(())
                })?,
            ));
        }
    }
    report("compressed", &cases);
    Ok(())
}

#[test]
fn settled_indexed_and_stream_reads_allocate_nothing() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("indexed");
    {
        let mut w = Plain::create_indexed_writer(&path, DiskIndexOptions::default())?;
        for (i, len) in SIZES.iter().enumerate() {
            w.push_blob(&blob!(Blob, i as u64, *len))?;
        }
        w.sync()?;
    }
    let r = Plain::open_indexed_reader(&path, DiskIndexOptions::default())?;
    let mut cases: Vec<Case> = Vec::new();
    let mut out = Blob::empty();
    cases.push((
        "get short".into(),
        steady(8, || r.get_blob(&0, &mut out).map(drop))?,
    ));
    cases.push((
        "get chunked".into(),
        steady(8, || r.get_blob(&1, &mut out).map(drop))?,
    ));
    cases.push((
        "lookup_entry".into(),
        steady(8, || r.lookup_entry::<Blob>(&1).map(drop))?,
    ));
    let entry = r.lookup_entry::<Blob>(&1)?.expect("present");
    let mut buf = vec![0u8; 4096];
    cases.push((
        "indexed read_record_into".into(),
        steady(4, || {
            let mut page = r.read_record_into(&entry, &mut buf)?;
            while let Some(next) = page.next {
                page = r.read_record_from(next, &mut buf)?;
            }
            Ok(())
        })?,
    ));
    cases.push((
        "indexed view_block + decode_field".into(),
        steady(8, || {
            let view = r.view_block::<Blob>(&entry)?;
            assert_eq!(r.decode_field::<u64>(&entry, view.id)?, 1);
            Ok(())
        })?,
    ));
    cases.push((
        "indexed verify_all".into(),
        steady(4, || r.verify_all().map(drop))?,
    ));
    drop(r);
    let path = dir.path().join("stream");
    {
        let mut w = Plain::create_stream_writer(&path, StreamOptions::default())?;
        // Two records of each size: the cursor settles on the first pair.
        for (i, len) in SIZES.iter().chain(SIZES.iter()).enumerate() {
            w.push_blob(&blob!(Blob, i as u64, *len))?;
        }
        w.sync()?;
    }
    let stream = Plain::open_stream_reader(&path, StreamOptions::default())?;
    let mut cursor = stream.blobs()?;
    // A cursor is single-pass: settle on the first record, count the second.
    assert!(cursor.next_into(&mut out)?);
    assert!(cursor.next_into(&mut out)?);
    let before = ALLOCATIONS.with(Cell::get);
    assert!(cursor.next_into(&mut out)?);
    assert!(cursor.next_into(&mut out)?);
    cases.push((
        "stream next_into".into(),
        ALLOCATIONS.with(Cell::get) - before,
    ));
    report("indexed", &cases);
    Ok(())
}

#[test]
fn settled_matrix_reads_allocate_nothing() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("grid");
    let spec: FormatSpec = Grid::spec();
    {
        let mut w = spec.create_writer_with_dims(
            &path,
            MatrixDimensions::from_pairs([("row", 16), ("col", 16)]),
        )?;
        for row in 0..16 {
            for col in 0..16 {
                let key = MatrixKey::new(row, col);
                w.write_matrix_cell(
                    key,
                    &Slot {
                        value: row * 16 + col,
                    },
                )?;
                w.commit_matrix_cell::<Slot>(key)?;
            }
        }
        w.sync()?;
    }
    let r = spec.open_reader(&path)?;
    let mut cases: Vec<Case> = Vec::new();
    let mut cell = Slot::empty();
    let mut at = 0u64;
    cases.push((
        "read_matrix_cell".into(),
        steady(64, || {
            at = (at + 37) % 256;
            r.read_matrix_cell::<Slot>(MatrixKey::new(at / 16, at % 16), &mut cell)
        })?,
    ));
    cases.push((
        "matrix_cell_status".into(),
        steady(16, || {
            at = (at + 37) % 256;
            r.matrix_cell_status::<Slot>(MatrixKey::new(at / 16, at % 16))
                .map(drop)
        })?,
    ));
    let mut slots = vec![Slot::empty(); 16 * 16];
    let mut raw = vec![0u8; 16 * 16 * 8];
    cases.push((
        "read_matrix_run_into".into(),
        steady(8, || {
            r.read_matrix_run_into::<Slot>(MatrixKey::new(3, 2), &mut slots[..13])
        })?,
    ));
    cases.push((
        "read_matrix_run_bytes_into".into(),
        steady(8, || {
            r.read_matrix_run_bytes_into::<Slot>(MatrixKey::new(3, 2), &mut raw[..13 * 8])
        })?,
    ));
    cases.push((
        "read_matrix_column_into".into(),
        steady(8, || {
            r.read_matrix_column_into::<Slot>(MatrixKey::new(1, 5), &mut slots[..12])
        })?,
    ));
    cases.push((
        "read_matrix_row_into".into(),
        steady(8, || r.read_matrix_row_into::<Slot>(9, &mut slots[..16]))?,
    ));
    let region = varlit::MatrixRegion::new(2..14, 3..15);
    // Whole in one page, and in pages of seven cells.
    cases.push((
        "read_matrix_region_into".into(),
        steady(8, || {
            r.read_matrix_region_into::<Slot>(region, &mut slots)
                .map(drop)
        })?,
    ));
    cases.push((
        "read_matrix_region_from pages".into(),
        steady(4, || {
            let mut page = r.read_matrix_region_into::<Slot>(region, &mut slots[..7])?;
            while let Some(next) = page.next {
                page = r.read_matrix_region_from::<Slot>(next, &mut slots[..7])?;
            }
            Ok(())
        })?,
    ));
    cases.push((
        "read_matrix_region_bytes_into".into(),
        steady(8, || {
            r.read_matrix_region_bytes_into::<Slot>(region, &mut raw)
                .map(drop)
        })?,
    ));
    let keys: Vec<MatrixKey> = (0..40u64)
        .map(|i| MatrixKey::new((i * 7) % 16, (i * 11) % 16))
        .collect();
    cases.push((
        "read_matrix_cells_into".into(),
        steady(8, || {
            r.read_matrix_cells_into::<Slot>(&keys, &mut slots[..keys.len()])
        })?,
    ));
    cases.push((
        "read_matrix_cells_bytes_into".into(),
        steady(8, || {
            r.read_matrix_cells_bytes_into::<Slot>(&keys, &mut raw[..keys.len() * 8])
        })?,
    ));
    report("matrix", &cases);
    Ok(())
}
