#[path = "support/pages.rs"]
mod pages;

use varlit::{MatrixCellStatus, varlit_format};

#[test]
fn directory_budgets_preserve_snapshots_follow_and_compaction() -> varlit::Result<()> {
    for cache in [0, 4096, 1024 * 1024] {
        for integrity in [
            varlit::IntegrityPolicy::None,
            #[cfg(feature = "integrity")]
            varlit::IntegrityPolicy::Crc32,
        ] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("directory-budget.varlit");
            let spec = Generations::spec()
                .with_integrity_policy(integrity)
                .with_resource_limits(
                    varlit::ReadLimits::STANDARD.with_matrix_directory_cache_bytes(cache),
                );
            let mut writer = spec.create_writer_with_dims(
                &path,
                varlit::MatrixDimensions::from_pairs([("scan", 96 * 32768), ("ch", 1)]),
            )?;
            for i in 0..96 {
                let key = varlit::MatrixKey::new(i * 32768, 0);
                writer.write_matrix_cell(key, &Cell { value: i })?;
                writer.commit_matrix_cell::<Cell>(key)?;
            }
            writer.sync()?;
            let held = spec.open_reader(&path)?;
            let mut reader = spec.open_reader(&path)?;
            for i in 0..96 {
                let key = varlit::MatrixKey::new(i * 32768, 0);
                writer.write_matrix_cell(key, &Cell { value: i + 100 })?;
                writer.commit_matrix_cell::<Cell>(key)?;
            }
            reader.follow()?;
            assert_eq!(
                filled_out(|o| reader.read_matrix_cell::<Cell>(varlit::MatrixKey::new(0, 0), o))?
                    .value,
                0
            );
            writer.sync()?;
            reader.follow()?;
            writer.compact_matrix()?;
            reader.follow()?;
            let cold = spec.open_reader(&path)?;
            for i in 0..96 {
                let key = varlit::MatrixKey::new(i * 32768, 0);
                assert_eq!(
                    filled_out(|o| held.read_matrix_cell::<Cell>(key, o))?.value,
                    i
                );
                assert_eq!(
                    filled_out(|o| reader.read_matrix_cell::<Cell>(key, o))?.value,
                    i + 100
                );
                assert_eq!(
                    filled_out(|o| cold.read_matrix_cell::<Cell>(key, o))?.value,
                    i + 100
                );
            }
            writer.clear_matrix_category("data")?;
            writer.sync()?;
            reader.follow()?;
            assert_eq!(
                reader.matrix_cell_status::<Cell>(varlit::MatrixKey::new(0, 0))?,
                MatrixCellStatus::NotCommitted
            );
        }
    }
    Ok(())
}
varlit_format! {
    pub format Generations {
        magic: b"MGEN";
        version: 1;
        dims { scan: u32, ch: u32, }
        commit: cell_bitmap { keyspace = [scan, ch]; categories = [data]; };
        aux { note: 32, }
        blocks {
            fixed Log(id = 2) { value: u64, }
            matrix Cell(id = 1, dims = [scan, ch], category = data) { value: u64, }
        }
    }
}
#[test]
fn dirty_overwrites_and_aux_remain_invisible_until_sync_and_follow() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("matrix.varlit");
    let dims = GenerationsDims { scan: 2, ch: 2 };
    let key = CellKey { scan: 0, ch: 0 };
    let mut writer = Generations::create_writer_with_dims(&path, dims)?;
    writer.write_cell(key, &Cell { value: 100 })?;
    writer.commit_cell(key)?;
    writer.write_note_aux(0, &[1, 2, 3])?;
    writer.sync()?;
    let mut old = Generations::open_reader(&path)?;
    assert_eq!(filled_out(|o| old.cell(key, o))?.value, 100);
    writer.write_cell(key, &Cell { value: 200 })?;
    assert_eq!(filled_out(|o| old.cell(key, o))?.value, 100);
    writer.commit_cell(key)?;
    writer.write_note_aux(0, &[4, 5, 6])?;
    writer.flush()?;
    let dirty = Generations::open_reader(&path)?;
    assert_eq!(filled_out(|o| dirty.cell(key, o))?.value, 100);
    assert_eq!(dirty.read_note_aux(0, 3)?, [1, 2, 3]);
    old.follow()?;
    assert_eq!(filled_out(|o| old.cell(key, o))?.value, 100);
    writer.sync()?;
    assert_eq!(filled_out(|o| old.cell(key, o))?.value, 100);
    assert_eq!(filled_out(|o| dirty.cell(key, o))?.value, 100);
    old.follow()?;
    assert_eq!(filled_out(|o| old.cell(key, o))?.value, 200);
    assert_eq!(old.read_note_aux(0, 3)?, [4, 5, 6]);
    assert_eq!(old.cell_status(key)?, MatrixCellStatus::Committed);
    Ok(())
}

#[test]
fn readers_keep_one_generation_across_concurrent_overwrites_and_clear() -> varlit::Result<()> {
    let policies = [
        varlit::IntegrityPolicy::None,
        #[cfg(feature = "integrity")]
        varlit::IntegrityPolicy::Crc32,
    ];
    for integrity in policies {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("threads.varlit");
        let spec = Generations::spec().with_integrity_policy(integrity);
        let mut writer = spec.create_writer_with_dims(
            &path,
            varlit::MatrixDimensions::from_pairs([("scan", 1024), ("ch", 2)]),
        )?;
        for ch in 0..2 {
            let key = varlit::MatrixKey::new(ch * 1023, ch);
            writer.write_matrix_cell(key, &Cell { value: 0 })?;
            writer.commit_matrix_cell::<Cell>(key)?;
        }
        writer.sync()?;
        let ready = std::sync::Barrier::new(5);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let mut reader = spec.open_reader(&path).unwrap();
                let ready = &ready;
                scope.spawn(move || {
                    ready.wait();
                    for _ in 0..200 {
                        reader.follow().unwrap();
                        let a = filled_out(|o| {
                            reader.read_matrix_cell::<Cell>(varlit::MatrixKey::new(0, 0), o)
                        })
                        .unwrap()
                        .value;
                        let b = filled_out(|o| {
                            reader.read_matrix_cell::<Cell>(varlit::MatrixKey::new(1023, 1), o)
                        })
                        .unwrap()
                        .value;
                        assert_eq!(
                            a, b,
                            "two cells must come from the same captured generation"
                        );
                    }
                });
            }
            ready.wait();
            for generation in 1..=40 {
                for ch in 0..2 {
                    let key = varlit::MatrixKey::new(ch * 1023, ch);
                    writer
                        .write_matrix_cell(key, &Cell { value: generation })
                        .unwrap();
                    writer.commit_matrix_cell::<Cell>(key).unwrap();
                }
                writer.sync().unwrap();
            }
        });
        let mut reader = spec.open_reader(&path)?;
        writer.clear_matrix_category("data")?;
        assert_eq!(
            filled_out(|o| reader.read_matrix_cell::<Cell>(varlit::MatrixKey::new(0, 0), o))?.value,
            40
        );
        writer.sync()?;
        assert_eq!(
            filled_out(|o| reader.read_matrix_cell::<Cell>(varlit::MatrixKey::new(0, 0), o))?.value,
            40
        );
        reader.follow()?;
        assert_eq!(
            reader.matrix_cell_status::<Cell>(varlit::MatrixKey::new(0, 0))?,
            MatrixCellStatus::NotCommitted
        );
    }
    Ok(())
}

#[test]
fn reopened_writer_discards_unpublished_matrix_changes() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("recovery.varlit");
    let key = CellKey { scan: 0, ch: 0 };
    let mut writer =
        Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 1 })?;
    writer.write_cell(key, &Cell { value: 17 })?;
    writer.commit_cell(key)?;
    writer.sync()?;
    writer.write_cell(key, &Cell { value: 99 })?;
    writer.commit_cell(key)?;
    writer.flush()?;
    drop(writer);
    let writer = Generations::open_writer(&path)?;
    assert_eq!(
        filled_out(|o| writer
            .inner()
            .read_matrix_cell::<Cell>(varlit::MatrixKey::new(0, 0), o))?
        .value,
        17
    );
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn publication_abort_child() -> varlit::Result<()> {
    let Some(path) = std::env::var_os("VARLIT_MATRIX_GENERATION_CHILD") else {
        return Ok(());
    };
    let mut writer = Generations::open_writer(path)?;
    for ch in 0..2 {
        let key = CellKey { scan: 0, ch };
        writer.write_cell(key, &Cell { value: 2 })?;
        writer.commit_cell(key)?;
    }
    writer.write_note_aux(0, &[2])?;
    writer.sync()?;
    panic!("the selected publication boundary did not abort");
}

#[test]
fn explicit_immediate_publishes_and_cache_capacity_is_configurable() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("immediate.varlit");
    let key = CellKey { scan: 0, ch: 0 };
    let mut writer =
        Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 1 })?;
    let mut reader = Generations::open_reader(&path)?;
    writer.write_cell(key, &Cell { value: 77 })?;
    writer.commit_cell(key)?;
    writer.immediate()?;
    assert_eq!(reader.cell_status(key)?, MatrixCellStatus::NotCommitted);
    reader.follow()?;
    assert_eq!(filled_out(|o| reader.cell(key, o))?.value, 77);
    for cache in [0, 4096, 65536] {
        let limits = varlit::ReadLimits::STANDARD.with_matrix_generation_cache_bytes(cache);
        let reader = Generations::spec().open_reader_with_resource_limits(&path, limits)?;
        assert_eq!(
            filled_out(|o| reader.read_matrix_cell::<Cell>(varlit::MatrixKey::new(0, 0), o))?.value,
            77
        );
        assert_eq!(
            reader.matrix_generation_path(),
            writer.matrix_generation_path()
        );
    }
    Ok(())
}

#[test]
fn matrix_only_sync_never_publishes_a_native_tail_that_recovery_will_truncate() -> varlit::Result<()>
{
    for mode in [
        varlit::TransactionMarkerMode::Explicit,
        varlit::TransactionMarkerMode::OnFlush,
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("marker.varlit");
        let spec =
            Generations::spec().with_commit_policy(varlit::CommitPolicy::TransactionMarker(mode));
        let dims = varlit::MatrixDimensions::from_pairs([("scan", 1), ("ch", 1)]);
        let key = varlit::MatrixKey::new(0, 0);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        writer.write_matrix_cell(key, &Cell { value: 1 })?;
        writer.commit_matrix_cell::<Cell>(key)?;
        writer.push(&Log { value: 1 })?;
        writer.commit_durable()?;
        let mut old = spec.open_reader(&path)?;
        writer.write_matrix_cell(key, &Cell { value: 2 })?;
        writer.commit_matrix_cell::<Cell>(key)?;
        writer.push(&Log { value: 2 })?;
        writer.sync()?;
        old.follow()?;
        assert_eq!(
            filled_out(|o| old.read_matrix_cell::<Cell>(key, o))?.value,
            2
        );
        assert_eq!(crate::pages::all_blocks!(old, Log)?.len(), 1);
        drop(writer);
        let mut recovered = spec.open_writer(&path)?;
        assert_eq!(
            filled_out(|o| recovered.read_matrix_cell::<Cell>(key, o))?.value,
            2
        );
        let reopened = spec.open_reader(&path)?;
        assert_eq!(crate::pages::all_blocks!(reopened, Log)?.len(), 1);
        recovered.push(&Log { value: 3 })?;
        recovered.commit_durable()?;
        old.follow()?;
        assert_eq!(crate::pages::all_blocks!(old, Log)?.len(), 2);
    }
    Ok(())
}

#[test]
fn skipped_generations_update_growing_shrinking_and_reordered_bitmap_indexes() -> varlit::Result<()>
{
    for integrity in [
        varlit::IntegrityPolicy::None,
        #[cfg(feature = "integrity")]
        varlit::IntegrityPolicy::Crc32,
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("delta-model.varlit");
        let spec = Generations::spec().with_integrity_policy(integrity);
        let mut writer = spec.create_writer_with_dims(
            &path,
            varlit::MatrixDimensions::from_pairs([("scan", 32 * 32768), ("ch", 1)]),
        )?;
        let mut reader = spec.open_reader(&path)?;
        let mut state = 0x361794dbu64;
        let mut model = [None; 32];
        for batch in 0..36 {
            for _ in 0..9 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let page = ((state >> 32) % 32) as usize;
                let key = varlit::MatrixKey::new(page as u64 * 32768, 0);
                if state & 4 == 0 {
                    writer.clear_matrix_cell::<Cell>(key)?;
                    model[page] = None;
                } else {
                    writer.write_matrix_cell(key, &Cell { value: state })?;
                    writer.commit_matrix_cell::<Cell>(key)?;
                    model[page] = Some(state);
                }
            }
            if batch % 11 == 10 {
                writer.clear_matrix_category("data")?;
                model.fill(None);
            }
            writer.sync()?;
            // Skip the compaction's own generation, then ordinary publications.
            if batch == 12 || batch == 13 {
                writer.compact_matrix()?;
            }
            if batch % 3 == 2 {
                reader.follow()?;
                let fresh = spec.open_reader(&path)?;
                for (page, expected) in model.iter().enumerate() {
                    let key = varlit::MatrixKey::new(page as u64 * 32768, 0);
                    assert_eq!(
                        reader.matrix_cell_status::<Cell>(key)?,
                        fresh.matrix_cell_status::<Cell>(key)?
                    );
                    if let Some(value) = expected {
                        assert_eq!(
                            filled_out(|o| reader.read_matrix_cell::<Cell>(key, o))?.value,
                            *value
                        );
                    } else {
                        assert_eq!(
                            reader.matrix_cell_status::<Cell>(key)?,
                            MatrixCellStatus::NotCommitted
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn tiny_bitmap_budget_follows_growing_and_shrinking_indexes() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("delta-limit.varlit");
    let spec = Generations::spec().with_integrity_policy(varlit::IntegrityPolicy::None);
    let mut writer = spec.create_writer_with_dims(
        &path,
        varlit::MatrixDimensions::from_pairs([("scan", 2 * 32768), ("ch", 1)]),
    )?;
    let first = varlit::MatrixKey::new(0, 0);
    let second = varlit::MatrixKey::new(32768, 0);
    writer.write_matrix_cell(first, &Cell { value: 1 })?;
    writer.commit_matrix_cell::<Cell>(first)?;
    writer.sync()?;
    let limits = varlit::ReadLimits::STANDARD
        .with_max_matrix_bitmap_bytes(48)
        .with_matrix_metadata_residency(varlit::MatrixMetadataResidency::Lazy { cache_bytes: 0 })
        .with_matrix_generation_cache_bytes(0);
    let mut reader = spec.open_reader_with_resource_limits(&path, limits)?;
    let generation = reader.matrix_generation();
    writer.write_matrix_cell(first, &Cell { value: 2 })?;
    writer.commit_matrix_cell::<Cell>(first)?;
    writer.write_matrix_cell(second, &Cell { value: 3 })?;
    writer.commit_matrix_cell::<Cell>(second)?;
    writer.sync()?;
    reader.follow()?;
    assert_ne!(reader.matrix_generation(), generation);
    assert_eq!(
        filled_out(|o| reader.read_matrix_cell::<Cell>(first, o))?.value,
        2
    );
    assert_eq!(
        filled_out(|o| reader.read_matrix_cell::<Cell>(second, o))?.value,
        3
    );
    // Small metadata budgets do not turn file growth into an open/follow refusal.
    let reopened = spec.open_reader_with_resource_limits(&path, limits)?;
    assert_eq!(
        filled_out(|o| reopened.read_matrix_cell::<Cell>(second, o))?.value,
        3
    );
    writer.clear_matrix_cell::<Cell>(second)?;
    writer.sync()?;
    reader.follow()?;
    assert_eq!(
        filled_out(|o| reader.read_matrix_cell::<Cell>(first, o))?.value,
        2
    );
    assert_eq!(
        reader.matrix_cell_status::<Cell>(second)?,
        MatrixCellStatus::NotCommitted
    );
    Ok(())
}

#[test]
fn default_matrix_admission_is_independent_of_the_working_set() -> varlit::Result<()> {
    let policies = [
        varlit::IntegrityPolicy::None,
        #[cfg(feature = "integrity")]
        varlit::IntegrityPolicy::Crc32,
    ];
    for integrity in policies {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("wide-sparse.varlit");
        // Beyond the former 16M-cell and 8GiB slot-region defaults. Only two
        // cells and their metadata are written; this is not a physical load test.
        let rows = 1_100_000_000;
        let limits = varlit::ReadLimits::STANDARD
            .with_max_matrix_bitmap_bytes(64 * 1024)
            .with_matrix_metadata_residency(varlit::MatrixMetadataResidency::Lazy {
                cache_bytes: 4096,
            });
        let spec = Generations::spec()
            .with_integrity_policy(integrity)
            .with_resource_limits(limits);
        let mut writer = spec.create_writer_with_dims(
            &path,
            varlit::MatrixDimensions::from_pairs([("scan", rows), ("ch", 1)]),
        )?;
        let first = varlit::MatrixKey::new(0, 0);
        let last = varlit::MatrixKey::new(rows - 1, 0);
        writer.write_matrix_cell(first, &Cell { value: 1 })?;
        writer.commit_matrix_cell::<Cell>(first)?;
        writer.sync()?;
        let mut reader = spec.open_reader(&path)?;
        writer.write_matrix_cell(last, &Cell { value: 2 })?;
        writer.commit_matrix_cell::<Cell>(last)?;
        assert_eq!(
            reader.matrix_cell_status::<Cell>(last)?,
            MatrixCellStatus::NotCommitted
        );
        writer.sync()?;
        reader.follow()?;
        assert_eq!(
            filled_out(|o| reader.read_matrix_cell::<Cell>(first, o))?.value,
            1
        );
        assert_eq!(
            filled_out(|o| reader.read_matrix_cell::<Cell>(last, o))?.value,
            2
        );
        let reopened = spec.open_reader(&path)?;
        assert_eq!(
            filled_out(|o| reopened.read_matrix_cell::<Cell>(last, o))?.value,
            2
        );
    }
    Ok(())
}

#[test]
fn follow_spills_many_changed_bitmap_slots_without_publishing_dirty_state() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("many-deltas.varlit");
    let spec = Generations::spec().with_integrity_policy(varlit::IntegrityPolicy::None);
    const PAGES: u64 = 96;
    let mut writer = spec.create_writer_with_dims(
        &path,
        varlit::MatrixDimensions::from_pairs([("scan", PAGES * 32768), ("ch", 1)]),
    )?;
    writer.sync()?;
    let limits = varlit::ReadLimits::STANDARD
        .with_max_matrix_bitmap_bytes(16)
        .with_matrix_metadata_residency(varlit::MatrixMetadataResidency::Lazy { cache_bytes: 0 });
    let mut reader = spec.open_reader_with_resource_limits(&path, limits)?;
    for page in 0..PAGES {
        let key = varlit::MatrixKey::new(page * 32768, 0);
        writer.write_matrix_cell(key, &Cell { value: page + 1 })?;
        writer.commit_matrix_cell::<Cell>(key)?;
    }
    reader.follow()?;
    assert_eq!(
        reader.matrix_cell_status::<Cell>(varlit::MatrixKey::new(0, 0))?,
        MatrixCellStatus::NotCommitted
    );
    writer.sync()?;
    reader.follow()?;
    let cold = spec.open_reader_with_resource_limits(&path, limits)?;
    for page in 0..PAGES {
        let key = varlit::MatrixKey::new(page * 32768, 0);
        assert_eq!(
            filled_out(|o| reader.read_matrix_cell::<Cell>(key, o))?.value,
            page + 1
        );
        assert_eq!(
            filled_out(|o| cold.read_matrix_cell::<Cell>(key, o))?.value,
            page + 1
        );
    }
    Ok(())
}

#[cfg(feature = "mmap")]
#[test]
fn matrix_cell_mapping_charges_only_its_window_and_cow_pages() -> varlit::Result<()> {
    let policies = [
        varlit::IntegrityPolicy::None,
        #[cfg(feature = "integrity")]
        varlit::IntegrityPolicy::Crc32,
    ];
    for integrity in policies {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("matrix-window.varlit");
        let spec = Generations::spec().with_integrity_policy(integrity);
        {
            let mut writer = spec.create_writer_with_dims(
                &path,
                varlit::MatrixDimensions::from_pairs([("scan", 4096), ("ch", 2)]),
            )?;
            for row in [0, 1024, 2048, 4095] {
                let key = varlit::MatrixKey::new(row, 1);
                writer.write_matrix_cell(key, &Cell { value: row + 100 })?;
                writer.commit_matrix_cell::<Cell>(key)?;
            }
            writer.sync()?;
        }
        let limited = spec.with_resource_limits(
            varlit::ReadLimits::STANDARD
                .with_max_mmap_len(8)
                .with_max_materialized_bytes(4096),
        );
        let reader = limited.open_reader(&path)?;
        // SAFETY: this fixture has no writer or external modifier.
        assert!(unsafe { reader.mmap_matrix() }.is_err());
        // SAFETY: the primary and its COW generation stay immutable.
        let first = unsafe { reader.mmap_matrix_cell::<Cell>(varlit::MatrixKey::new(0, 1))? };
        // SAFETY: same immutable generation; independent mapping owner.
        let last = unsafe { reader.mmap_matrix_cell::<Cell>(varlit::MatrixKey::new(4095, 1))? };
        assert_eq!(
            u64::from_le_bytes(first.as_bytes().try_into().unwrap()),
            100
        );
        assert_eq!(
            u64::from_le_bytes(last.as_bytes().try_into().unwrap()),
            4195
        );
        assert!(last.offset() > first.offset());
        // An unset visibility bit must be refused, even if its slot maps.
        // SAFETY: the fixture remains immutable.
        assert!(matches!(
            unsafe { reader.mmap_matrix_cell::<Cell>(varlit::MatrixKey::new(1, 1)) },
            Err(varlit::Error::MatrixNotCommitted)
        ));
        drop(first);
        drop(reader);
        assert_eq!(
            u64::from_le_bytes(last.as_bytes().try_into().unwrap()),
            4195
        );
    }
    Ok(())
}

/// Runs a caller-owned-output read that always fills `out`, returning the value.
#[allow(dead_code)]
fn filled_out<T: varlit::VarlitEmpty, R>(
    read: impl FnOnce(&mut T) -> varlit::Result<R>,
) -> varlit::Result<T> {
    let mut out = T::empty();
    read(&mut out)?;
    Ok(out)
}

/// Runs a caller-owned-output read that reports presence; `None` when absent.
#[allow(dead_code)]
fn filled<T: varlit::VarlitEmpty>(
    read: impl FnOnce(&mut T) -> varlit::Result<bool>,
) -> varlit::Result<Option<T>> {
    let mut out = T::empty();
    Ok(read(&mut out)?.then_some(out))
}
