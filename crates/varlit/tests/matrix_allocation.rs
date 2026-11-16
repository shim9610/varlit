//! A matrix handle allocates its memory when it is opened. After that,
//! overwriting cells, committing them, publishing (through the ring, and
//! through an on-disk index when a publication outgrows the ring),
//! following, reading and checkpointing allocate nothing: the counting
//! allocator below sees every allocation the test thread makes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as StdCell;
use varlit::{MatrixDimensions, MatrixKey, VarlitEmpty, varlit_format};

struct Counting;

thread_local! {
    static COUNTING: StdCell<bool> = const { StdCell::new(false) };
    static COUNT: StdCell<u64> = const { StdCell::new(0) };
}

fn note() {
    let _ = COUNTING.try_with(|on| {
        if on.get() {
            let _ = COUNT.try_with(|count| count.set(count.get() + 1));
        }
    });
}

// SAFETY: the system allocator's layouts, unchanged; counting allocates
// nothing.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations `body` makes on this thread.
fn allocations(body: impl FnOnce() -> varlit::Result<()>) -> varlit::Result<u64> {
    COUNT.with(|count| count.set(0));
    COUNTING.with(|on| on.set(true));
    let result = body();
    COUNTING.with(|on| on.set(false));
    result?;
    Ok(COUNT.with(StdCell::get))
}

varlit_format! {
    pub format WideAlloc {
        magic: b"TILEDA01";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        blocks {
            matrix Wide(id = 1, dims = [row, col], category = data, tile = [4, 8]) {
                bytes: [u8; 1024],
            }
        }
    }
}

fn dims(rows: u64, cols: u64) -> MatrixDimensions {
    MatrixDimensions::from_pairs([("row", rows), ("col", cols)])
}

#[test]
fn matrix_data_paths_allocate_nothing_after_open() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("wide");
    let (rows, cols) = (128u64, 128u64);
    let fill = |writer: &mut varlit::VarlitWriter, keys: u64, value: u8| -> varlit::Result<()> {
        let cell = Wide {
            bytes: [value; 1024],
        };
        for index in 0..keys {
            let key = MatrixKey::new(index / cols, index % cols);
            writer.write_matrix_cell(key, &cell)?;
            writer.commit_matrix_cell::<Wide>(key)?;
        }
        writer.sync()
    };
    let mut writer = WideAlloc::spec().create_writer_with_dims(&path, dims(rows, cols))?;
    let mut reader = WideAlloc::spec().open_reader(&path)?;
    // Warm-up: the tiles, the directory and the overflow regions a full
    // rewrite needs are appended once.
    fill(&mut writer, rows * cols, 1)?;
    fill(&mut writer, rows * cols, 2)?;
    reader.follow()?;
    let mut out = Wide::empty();
    reader.read_matrix_cell::<Wide>(MatrixKey::new(0, 0), &mut out)?;
    // A handle's read scratch is sized by its first read.
    writer.read_matrix_cell::<Wide>(MatrixKey::new(0, 0), &mut out)?;

    // A full rewrite (16 MiB, past the ring) streams into an index that
    // reuses the regions; a small one goes through the ring.
    let large = allocations(|| fill(&mut writer, rows * cols, 3))?;
    let small = allocations(|| fill(&mut writer, 40, 4))?;
    let follow = allocations(|| reader.follow().map(drop))?;
    let reads = allocations(|| {
        for index in (0..rows * cols).step_by(97) {
            let key = MatrixKey::new(index / cols, index % cols);
            reader.read_matrix_cell::<Wide>(key, &mut out)?;
            let want = if index < 40 { 4 } else { 3 };
            assert!(out.bytes.iter().all(|b| *b == want), "cell {index}");
        }
        Ok(())
    })?;
    let writer_reads = allocations(|| {
        writer.read_matrix_cell::<Wide>(MatrixKey::new(rows - 1, cols - 1), &mut out)
    })?;
    assert_eq!(
        (large, small, follow, reads, writer_reads),
        (0, 0, 0, 0, 0),
        "allocations after open: (large publication, small publication, follow, reads, writer reads)"
    );
    Ok(())
}
