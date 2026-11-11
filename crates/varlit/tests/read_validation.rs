//! Trust choices change detection and I/O, not resource or publication guards.
#![cfg(feature = "integrity")]
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as LocalCell;
use std::io::{Read, Seek, SeekFrom, Write};
#[path = "common/matrix_image.rs"]
mod matrix_image;
use varlit::*;

struct Meter;
thread_local! {
    static DENY: LocalCell<bool> = const { LocalCell::new(false) };
    static REFUSED: LocalCell<usize> = const { LocalCell::new(0) };
}
fn refuse(n: usize) -> bool {
    let denied = DENY.try_with(|s| s.get() && n >= 65536).unwrap_or(false);
    if denied {
        let _ = REFUSED.try_with(|s| s.set(s.get() + 1));
    }
    denied
}
// SAFETY: unchanged System pointers/layouts; null refusal leaves realloc's old
// allocation live. TLS bookkeeping does not allocate.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if refuse(layout.size()) {
            std::ptr::null_mut()
        } else {
            unsafe { System.alloc(layout) }
        }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if refuse(layout.size()) {
            std::ptr::null_mut()
        } else {
            unsafe { System.alloc_zeroed(layout) }
        }
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, n: usize) -> *mut u8 {
        if refuse(n) {
            std::ptr::null_mut()
        } else {
            unsafe { System.realloc(p, layout, n) }
        }
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;
fn deny_large<T>(f: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            DENY.with(|s| s.set(false));
        }
    }
    REFUSED.with(|s| s.set(0));
    DENY.with(|s| s.set(true));
    let reset = Reset;
    let result = f();
    drop(reset);
    (result, REFUSED.with(LocalCell::get))
}
const POLICIES: [ReadValidation; 4] = [
    ReadValidation::FULL,
    ReadValidation::CRC_ONLY,
    ReadValidation::STRUCTURE_ONLY,
    ReadValidation::TRUSTED,
];
varlit_format! { pub format Log {
    magic: b"READOPT1"; version: 1; endian: little; integrity: crc32_with_header;
    index: keyed_offset_chain;
    blocks { variable Item(id = 1, key = [id], key_index = disk) { id: u64, bytes: Vec<u8>, } }
} }
varlit_format! { pub format Grid {
    magic: b"READOPT2"; version: 1; endian: little; integrity: crc32_with_header;
    dims { row: u32, col: u32, }
    commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
    blocks { matrix GridCell(id = 1, dims = [row, col], category = data) { value: u64, } }
} }
fn limits(p: ReadValidation) -> ReadLimits {
    ReadLimits::STANDARD.with_read_validation(p)
}
fn options(p: ReadValidation) -> DiskIndexOptions {
    DiskIndexOptions {
        limits: limits(p),
        cache_bytes: 1024 * 1024,
        ..DiskIndexOptions::default()
    }
}
fn flip(path: &std::path::Path, at: u64) -> Result<()> {
    let old = std::fs::read(path)?[at as usize];
    let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(at))?;
    f.write_all(&[old ^ 1])?;
    Ok(())
}
#[test]
fn native_and_indexed_reads_refuse_allocation_and_retry_in_every_mode() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("native");
    let value = Item {
        id: 4,
        bytes: vec![19; 65536],
    };
    {
        let mut w = Log::spec().create_writer(&path)?;
        w.push_keyed(&value)?;
        w.sync()?;
    }
    let indexed = dir.path().join("indexed");
    {
        let mut w = Log::create_indexed_writer(&indexed, DiskIndexOptions::default())?;
        w.push_item(&value)?;
        w.sync()?;
    }
    for p in POLICIES {
        let r = Log::spec()
            .with_resource_limits(limits(p))
            .open_reader(&path)?;
        let blocks = r.blocks::<Item>()?;
        let (failed, denied) = deny_large(|| opt(|o| blocks.get(0, o)));
        assert!(failed.is_err());
        assert!(denied > 0);
        assert_eq!(opt(|o| blocks.get(0, o))?, Some(value.clone()));
        let r =
            VarlitIndexedReader::open(Log::spec(), &indexed, options(p), Log::disk_index_plan()?)?;
        let (failed, denied) = deny_large(|| opt(|o| r.get::<Item>(&4, o)));
        assert!(failed.is_err());
        assert!(denied > 0);
        assert_eq!(opt(|o| r.get::<Item>(&4, o))?, Some(value.clone()));
    }
    Ok(())
}
#[test]
fn payload_damage_and_explicit_verification_have_distinct_contracts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("native");
    let offset;
    {
        let mut w = Log::spec().create_writer(&path)?;
        let info = w.push_keyed_info(&Item {
            id: 4,
            bytes: vec![19; 100],
        })?;
        w.sync()?;
        offset = info.payload_offset + info.payload_len - 20;
    }
    flip(&path, offset)?;
    for p in POLICIES {
        let r = Log::spec()
            .with_resource_limits(limits(p))
            .open_reader(&path)?;
        let got = opt(|o| r.blocks::<Item>()?.get(0, o));
        assert_eq!(got.is_err(), p.data_crc, "{p:?}: {got:?}");
        assert!(
            r.verify_all().is_err(),
            "explicit verification must override trust"
        );
        #[cfg(feature = "mmap")]
        {
            // SAFETY: all writers closed; no mutation until mapping is dropped.
            let mapped = unsafe { r.mmap_payloads()? };
            assert_eq!(mapped.block_payload_window::<Item>(0).is_err(), p.data_crc);
        }
    }
    Ok(())
}
#[test]
fn eight_independent_readers_keep_pending_edits_hidden_in_every_mode() -> Result<()> {
    use std::sync::mpsc;
    use std::time::Duration;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("shared");
    let mut w = Log::create_indexed_writer(&path, DiskIndexOptions::default())?;
    w.sync()?;
    let mut senders = Vec::new();
    let mut receivers = Vec::new();
    let mut workers = Vec::new();
    for p in POLICIES.into_iter().cycle().take(8) {
        let path = path.clone();
        let (tx, rx) = mpsc::channel();
        let (ack, done) = mpsc::channel();
        senders.push(tx);
        receivers.push(done);
        workers.push(std::thread::spawn(move || -> Result<()> {
            let mut r =
                VarlitIndexedReader::open(Log::spec(), path, options(p), Log::disk_index_plan()?)?;
            for (visible, pending) in rx {
                r.follow()?;
                for id in 0..visible {
                    // Refusal in one reader must preserve its generation and leave
                    // the other readers and the next writer publication usable.
                    let (failed, denied) = deny_large(|| opt(|o| r.get::<Item>(&id, o)));
                    assert!(failed.is_err());
                    assert!(denied > 0);
                    assert_eq!(
                        opt(|o| r.get::<Item>(&id, o))?.unwrap().bytes,
                        vec![id as u8; 65_537]
                    );
                }
                assert_eq!(opt(|o| r.get::<Item>(&pending, o))?, None);
                ack.send(()).unwrap();
            }
            Ok(())
        }));
    }
    for epoch in 0..4u64 {
        w.push_item(&Item {
            id: epoch,
            bytes: vec![epoch as u8; 65_537],
        })?;
        for tx in &senders {
            tx.send((epoch, epoch)).unwrap();
        }
        for rx in &receivers {
            rx.recv_timeout(Duration::from_secs(20)).unwrap();
        }
        w.sync()?;
        for tx in &senders {
            tx.send((epoch + 1, epoch + 1)).unwrap();
        }
        for rx in &receivers {
            rx.recv_timeout(Duration::from_secs(20)).unwrap();
        }
    }
    drop(senders);
    for worker in workers {
        worker.join().unwrap()?;
    }
    Ok(())
}
#[test]
fn matrix_scalar_run_column_and_mmap_honor_crc_choice_and_visibility() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("matrix");
    let first = 0x1a2b3c4d5e6f7081u64;
    {
        let mut w = Grid::spec().create_writer_with_dims(
            &path,
            MatrixDimensions::from_pairs([("row", 2), ("col", 2)]),
        )?;
        for i in 0..3 {
            let key = MatrixKey::new(i / 2, i % 2);
            w.write_matrix_cell(key, &GridCell { value: first + i })?;
            w.commit_matrix_cell::<GridCell>(key)?;
        }
        w.sync()?;
    }
    let mut image = matrix_image::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?;
    let mut data = Vec::new();
    image.read_to_end(&mut data)?;
    let offsets: Vec<_> = data
        .windows(8)
        .enumerate()
        .filter(|(_, b)| *b == first.to_le_bytes())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(offsets.len(), 1);
    image.seek(SeekFrom::Start(offsets[0] as u64))?;
    image.write_all(&[data[offsets[0]] ^ 1])?;
    image.sync_all()?;
    drop(image);
    for p in POLICIES {
        let r = Grid::spec()
            .with_resource_limits(limits(p))
            .open_reader(&path)?;
        assert_eq!(
            val(|o| r.read_matrix_cell::<GridCell>(MatrixKey::new(0, 0), o)).is_err(),
            p.data_crc
        );
        let mut row = [GridCell { value: 0 }, GridCell { value: 0 }];
        assert_eq!(
            r.read_matrix_run_into(MatrixKey::new(0, 0), &mut row)
                .is_err(),
            p.data_crc
        );
        assert_eq!(
            r.read_matrix_column_into(MatrixKey::new(0, 0), &mut row)
                .is_err(),
            p.data_crc
        );
        assert!(matches!(
            val(|o| r.read_matrix_cell::<GridCell>(MatrixKey::new(1, 1), o)),
            Err(Error::MatrixNotCommitted)
        ));
        #[cfg(feature = "mmap")]
        {
            // SAFETY: immutable backing object, all writers closed.
            assert_eq!(
                unsafe { r.mmap_matrix_cell::<GridCell>(MatrixKey::new(0, 0)) }.is_err(),
                p.data_crc
            );
        }
    }
    Ok(())
}

#[cfg(feature = "compression-zstd")]
#[test]
fn compressed_indexed_reads_keep_decoder_budgets_for_every_header_and_mode() -> Result<()> {
    const BLOCKS: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Item>()];
    for header in [
        CompressionHeaderMode::RecordExplicit,
        CompressionHeaderMode::FileExplicit,
        CompressionHeaderMode::FormatContract,
    ] {
        let spec = Log::spec()
            .with_compression_policy(CompressionPolicy::VariableBlocks(
                VariableCompression::zstd(header),
            ))
            .with_computed_schema_hash();
        let plan = DiskIndexPlan::canonical(spec, BLOCKS)?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("compressed");
        let value = Item {
            id: 7,
            bytes: vec![41; 65536],
        };
        {
            let mut w =
                VarlitIndexedWriter::create(spec, &path, DiskIndexOptions::default(), plan)?;
            w.push_info(&value)?;
            w.sync()?;
        }
        for p in POLICIES {
            let r = VarlitIndexedReader::open(spec, &path, options(p), plan)?;
            assert_eq!(opt(|o| r.get::<Item>(&7, o))?, Some(value.clone()));
            let mut limited = options(p);
            limited.limits = limited.limits.with_max_logical_payload_len(100);
            let r = VarlitIndexedReader::open(spec, &path, limited, plan)?;
            assert!(matches!(
                opt(|o| r.get::<Item>(&7, o)),
                Err(Error::LimitExceeded { .. })
            ));
        }
    }
    Ok(())
}

#[cfg(feature = "compression-zstd")]
#[test]
fn growing_compressed_matrix_runs_and_columns_cross_chunks_in_every_mode() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("growing");
    let spec = Grid::spec()
        .with_growing_matrix_dimension("row", 2)
        .with_chunk_compression(VariableCompression::zstd(
            CompressionHeaderMode::RecordExplicit,
        ));
    {
        let mut w = spec.create_writer_with_dims(
            &path,
            MatrixDimensions::from_pairs([("row", 2), ("col", 4)]),
        )?;
        for i in 0..28 {
            let key = MatrixKey::new(i / 4, i % 4);
            w.write_matrix_cell(key, &GridCell { value: i })?;
            w.commit_matrix_cell::<GridCell>(key)?;
        }
        w.sync()?;
    }
    for p in POLICIES {
        let r = spec.with_resource_limits(limits(p)).open_reader(&path)?;
        let mut run: Vec<_> = (0..23).map(|_| GridCell { value: 0 }).collect();
        r.read_matrix_run_into(MatrixKey::new(1, 0), &mut run)?;
        assert_eq!(
            run.iter().map(|c| c.value).collect::<Vec<_>>(),
            (4..27).collect::<Vec<_>>()
        );
        let mut col: Vec<_> = (0..7).map(|_| GridCell { value: 0 }).collect();
        r.read_matrix_column_into(MatrixKey::new(0, 2), &mut col)?;
        assert_eq!(
            col.iter().map(|c| c.value).collect::<Vec<_>>(),
            (0..7).map(|i| i * 4 + 2).collect::<Vec<_>>()
        );
    }
    Ok(())
}

#[test]
fn delete_reinsert_and_retained_snapshots_keep_their_confirmed_values() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("versions");
    let spec = Log::spec();
    let plan = Log::disk_index_plan()?;
    let mut w = VarlitIndexedWriter::create(spec, &path, DiskIndexOptions::default(), plan)?;
    w.push_info(&Item {
        id: 7,
        bytes: vec![11; 32],
    })?;
    w.sync()?;
    let mut readers = Vec::new();
    let mut pins = Vec::new();
    for p in POLICIES {
        let r = VarlitIndexedReader::open(spec, &path, options(p), plan)?;
        pins.push(r.blocks::<Item>()?);
        readers.push(r);
    }
    w.delete_info::<Item>(&7)?;
    for r in &mut readers {
        r.follow()?;
        assert_eq!(opt(|o| r.get::<Item>(&7, o))?.unwrap().bytes, vec![11; 32]);
    }
    w.sync()?;
    for r in &mut readers {
        r.follow()?;
        assert_eq!(opt(|o| r.get::<Item>(&7, o))?, None);
        assert!(matches!(
            r.lookup::<Item>(&7)?,
            DiskIndexEntry::Tombstone { .. }
        ));
    }
    w.push_info(&Item {
        id: 7,
        bytes: vec![22; 32],
    })?;
    for r in &mut readers {
        r.follow()?;
        assert_eq!(opt(|o| r.get::<Item>(&7, o))?, None);
    }
    w.sync()?;
    for r in &mut readers {
        r.follow()?;
        assert_eq!(opt(|o| r.get::<Item>(&7, o))?.unwrap().bytes, vec![22; 32]);
    }
    for mut pin in pins {
        let mut item = Item::empty();
        assert!(pin.next_into(&mut item)?);
        assert_eq!(item.bytes, vec![11; 32]);
        assert!(!pin.next_into(&mut item)?);
    }
    Ok(())
}

#[cfg(feature = "mmap")]
#[test]
fn mapped_windows_trust_captured_framing_but_keep_extent_checks() -> Result<()> {
    let dir = tempfile::tempdir()?;
    for (i, p) in POLICIES.into_iter().enumerate() {
        let path = dir.path().join(format!("window-{i}"));
        {
            let mut w = Log::spec().create_writer(&path)?;
            w.push_keyed(&Item {
                id: 1,
                bytes: vec![8; 32],
            })?;
            w.sync()?;
        }
        let r = Log::spec()
            .with_resource_limits(limits(p))
            .open_readonly(&path)?;
        let mut entries = Vec::new();
        r.block_entries_into::<Item>(&mut entries)?;
        let entry = &entries[0];
        // Change only the native header after capturing complete metadata.
        flip(&path, entry.record_offset)?;
        // SAFETY: mutation finished before constructing the mapping; there are
        // no further changes until every returned view is dropped.
        let window = unsafe { r.mmap_payload_window(entry) };
        assert_eq!(window.is_err(), p.structure);
        if let Ok(window) = window {
            assert_eq!(window.as_bytes().len() as u64, entry.payload_len);
        }
        let mut forged = entry.clone();
        forged.payload_offset = u64::MAX;
        // SAFETY: backing object stays immutable; invalid extents must refuse.
        assert!(unsafe { r.mmap_payload_window(&forged) }.is_err());
    }
    Ok(())
}

// Test-local adapters over the caller-owned-output read API: the assertions
// around each call keep their old `Option` / value shape.
/// Decodes into a fresh caller-owned value; `None` when the read reports absence.
fn opt<T: varlit::VarlitEmpty>(
    read: impl FnOnce(&mut T) -> varlit::Result<bool>,
) -> varlit::Result<Option<T>> {
    let mut out = T::empty();
    Ok(read(&mut out)?.then_some(out))
}

/// Decodes into a fresh caller-owned value.
fn val<T: varlit::VarlitEmpty>(
    read: impl FnOnce(&mut T) -> varlit::Result<()>,
) -> varlit::Result<T> {
    let mut out = T::empty();
    read(&mut out)?;
    Ok(out)
}
