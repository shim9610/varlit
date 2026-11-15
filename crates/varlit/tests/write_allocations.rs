//! Appends reuse the writer's own buffer: once a handle has written one
//! record of each shape, further appends, deletes, flushes and syncs make no
//! allocator calls. That includes a record long enough to be stored in chunks,
//! whose chunk table is written in front of the bytes it was encoded into
//! instead of the payload being copied into a buffer of its own.
#![cfg(feature = "integrity")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use varlit::{VarlitEmpty, varlit_format};

struct CountingAllocator;

std::thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn count() {
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
}

// SAFETY: every call is forwarded unchanged to the system allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

varlit_format! {
    pub format Mixed {
        magic: b"WALLOCMX"; version: 1; endian: little; integrity: crc32_with_header;
        blocks {
            fixed Tick(id = 1) { at: u64, value: u64 }
            variable Note(id = 2, key = [id]) { id: u64, tag: String, body: Vec<u8>, }
        }
    }
}

#[test]
fn settled_appends_deletes_and_syncs_allocate_nothing() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut file = Mixed::spec().create(directory.path().join("w.varlit"))?;
    let mut event = Note::empty();
    event.tag.push_str("event");
    // One short record and one long enough to be stored in four chunks.
    let lengths = [900usize, 4 * 64 * 1024 + 123];
    event.body.resize(lengths[1], 7);
    let mut round = |file: &mut varlit::VarlitFile, batch: u64| -> varlit::Result<()> {
        for at in 0..64 {
            file.push(&Tick {
                at,
                value: at ^ batch,
            })?;
        }
        for (n, len) in lengths.iter().enumerate() {
            event.id = batch * 2 + n as u64;
            event.body.truncate(*len);
            event.body.resize(*len, n as u8);
            file.push(&event)?;
        }
        file.delete::<Note>(&(batch * 2))?;
        file.flush()?;
        file.sync()
    };
    for batch in 0..4 {
        round(&mut file, batch)?;
    }
    let before = ALLOCATIONS.with(Cell::get);
    for batch in 4..12 {
        round(&mut file, batch)?;
    }
    let made = ALLOCATIONS.with(Cell::get) - before;
    assert_eq!(made, 0, "settled writes made {made} allocator calls");
    Ok(())
}
