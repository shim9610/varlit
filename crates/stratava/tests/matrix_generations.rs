use stratava::{MatrixCellStatus, stratava_format};

#[test]
fn directory_budgets_preserve_snapshots_follow_and_compaction() -> stratava::Result<()> {
    for cache in [0, 4096, 1024 * 1024] {
        for integrity in [
            stratava::IntegrityPolicy::None,
            #[cfg(feature = "integrity")]
            stratava::IntegrityPolicy::Crc32,
        ] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("directory-budget.stratava");
            let spec = Generations::spec()
                .with_integrity_policy(integrity)
                .with_resource_limits(
                    stratava::ReadLimits::STANDARD.with_matrix_directory_cache_bytes(cache),
                );
            let mut writer = spec.create_writer_with_dims(
                &path,
                stratava::MatrixDimensions::from_pairs([("scan", 96 * 32768), ("ch", 1)]),
            )?;
            for i in 0..96 {
                let key = stratava::MatrixKey::new(i * 32768, 0);
                writer.write_matrix_cell(key, &Cell { value: i })?;
                writer.commit_matrix_cell::<Cell>(key)?;
            }
            writer.sync()?;
            let held = spec.open_reader(&path)?;
            let mut reader = spec.open_reader(&path)?;
            for i in 0..96 {
                let key = stratava::MatrixKey::new(i * 32768, 0);
                writer.write_matrix_cell(key, &Cell { value: i + 100 })?;
                writer.commit_matrix_cell::<Cell>(key)?;
            }
            reader.follow()?;
            assert_eq!(
                reader
                    .read_matrix_cell::<Cell>(stratava::MatrixKey::new(0, 0))?
                    .value,
                0
            );
            writer.sync()?;
            reader.follow()?;
            writer.compact_matrix()?;
            reader.follow()?;
            let cold = spec.open_reader(&path)?;
            for i in 0..96 {
                let key = stratava::MatrixKey::new(i * 32768, 0);
                assert_eq!(held.read_matrix_cell::<Cell>(key)?.value, i);
                assert_eq!(reader.read_matrix_cell::<Cell>(key)?.value, i + 100);
                assert_eq!(cold.read_matrix_cell::<Cell>(key)?.value, i + 100);
            }
            writer.clear_matrix_category("data")?;
            writer.sync()?;
            reader.follow()?;
            assert_eq!(
                reader.matrix_cell_status::<Cell>(stratava::MatrixKey::new(0, 0))?,
                MatrixCellStatus::NotCommitted
            );
        }
    }
    Ok(())
}
stratava_format! {
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
fn dirty_overwrites_and_aux_remain_invisible_until_sync_and_follow() -> stratava::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("matrix.stratava");
    let dims = GenerationsDims { scan: 2, ch: 2 };
    let key = CellKey { scan: 0, ch: 0 };
    let mut writer = Generations::create_writer_with_dims(&path, dims)?;
    writer.write_cell(key, &Cell { value: 100 })?;
    writer.commit_cell(key)?;
    writer.write_note_aux(0, &[1, 2, 3])?;
    writer.sync()?;
    let mut old = Generations::open_reader(&path)?;
    assert_eq!(old.cell(key)?.value, 100);
    writer.write_cell(key, &Cell { value: 200 })?;
    assert_eq!(old.cell(key)?.value, 100);
    writer.commit_cell(key)?;
    writer.write_note_aux(0, &[4, 5, 6])?;
    writer.flush()?;
    let dirty = Generations::open_reader(&path)?;
    assert_eq!(dirty.cell(key)?.value, 100);
    assert_eq!(dirty.read_note_aux(0, 3)?, [1, 2, 3]);
    old.follow()?;
    assert_eq!(old.cell(key)?.value, 100);
    writer.sync()?;
    assert_eq!(old.cell(key)?.value, 100);
    assert_eq!(dirty.cell(key)?.value, 100);
    old.follow()?;
    assert_eq!(old.cell(key)?.value, 200);
    assert_eq!(old.read_note_aux(0, 3)?, [4, 5, 6]);
    assert_eq!(old.cell_status(key)?, MatrixCellStatus::Committed);
    Ok(())
}

#[test]
fn readers_keep_one_generation_across_concurrent_overwrites_and_clear() -> stratava::Result<()> {
    let policies = [
        stratava::IntegrityPolicy::None,
        #[cfg(feature = "integrity")]
        stratava::IntegrityPolicy::Crc32,
    ];
    for integrity in policies {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("threads.stratava");
        let spec = Generations::spec().with_integrity_policy(integrity);
        let mut writer = spec.create_writer_with_dims(
            &path,
            stratava::MatrixDimensions::from_pairs([("scan", 1024), ("ch", 2)]),
        )?;
        for ch in 0..2 {
            let key = stratava::MatrixKey::new(ch * 1023, ch);
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
                        let a = reader
                            .read_matrix_cell::<Cell>(stratava::MatrixKey::new(0, 0))
                            .unwrap()
                            .value;
                        let b = reader
                            .read_matrix_cell::<Cell>(stratava::MatrixKey::new(1023, 1))
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
                    let key = stratava::MatrixKey::new(ch * 1023, ch);
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
            reader
                .read_matrix_cell::<Cell>(stratava::MatrixKey::new(0, 0))?
                .value,
            40
        );
        writer.sync()?;
        assert_eq!(
            reader
                .read_matrix_cell::<Cell>(stratava::MatrixKey::new(0, 0))?
                .value,
            40
        );
        reader.follow()?;
        assert_eq!(
            reader.matrix_cell_status::<Cell>(stratava::MatrixKey::new(0, 0))?,
            MatrixCellStatus::NotCommitted
        );
    }
    Ok(())
}

#[test]
fn reopened_writer_discards_unpublished_matrix_changes() -> stratava::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("recovery.stratava");
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
        writer
            .inner()
            .read_matrix_cell::<Cell>(stratava::MatrixKey::new(0, 0))?
            .value,
        17
    );
    Ok(())
}

#[test]
fn compaction_preserves_old_readers_and_follow_crosses_the_replacement() -> stratava::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("compact.stratava");
    let key = CellKey { scan: 0, ch: 0 };
    let mut writer =
        Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 1 })?;
    writer.write_cell(key, &Cell { value: 1 })?;
    writer.commit_cell(key)?;
    writer.sync()?;
    let mut old = Generations::open_reader(&path)?;
    for value in 2..30 {
        writer.write_cell(key, &Cell { value })?;
        writer.commit_cell(key)?;
        writer.sync()?;
    }
    let log = std::fs::read_dir(dir.path())?
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "vmg"))
        .unwrap();
    let before = log.metadata()?.len();
    writer.compact_matrix()?;
    let after = log.metadata()?.len();
    assert!(after < before / 2);
    assert_eq!(old.cell(key)?.value, 1);
    old.follow()?;
    assert_eq!(old.cell(key)?.value, 29);
    writer.write_cell(key, &Cell { value: 30 })?;
    writer.commit_cell(key)?;
    writer.sync()?;
    old.follow()?;
    assert_eq!(old.cell(key)?.value, 30);
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn publication_abort_child() -> stratava::Result<()> {
    let Some(path) = std::env::var_os("STRATAVA_MATRIX_GENERATION_CHILD") else {
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
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn crashes_at_every_publication_boundary_keep_a_complete_generation() -> stratava::Result<()> {
    for stage in [
        "data",
        "index",
        "data_sync",
        "head_prefix",
        "head",
        "head_sync",
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("crash.stratava");
        let mut writer =
            Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 2 })?;
        for ch in 0..2 {
            let key = CellKey { scan: 0, ch };
            writer.write_cell(key, &Cell { value: 1 })?;
            writer.commit_cell(key)?;
        }
        writer.write_note_aux(0, &[1])?;
        writer.sync()?;
        drop(writer);
        let trace = dir.path().join("abort-witness");
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "publication_abort_child", "--nocapture"])
            .env("STRATAVA_MATRIX_GENERATION_TRACE", &trace)
            .env("STRATAVA_MATRIX_GENERATION_CHILD", &path)
            .env("STRATAVA_MATRIX_GENERATION_ABORT", stage)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        assert!(
            !status.success(),
            "{stage}: child must terminate at injected boundary"
        );
        assert_eq!(std::fs::read_to_string(trace)?, stage);
        let reader = Generations::open_reader(&path)?;
        let a = reader.cell(CellKey { scan: 0, ch: 0 })?.value;
        let b = reader.cell(CellKey { scan: 0, ch: 1 })?.value;
        assert_eq!(a, b, "{stage}: mixed cell generations");
        assert_eq!(reader.read_note_aux(0, 1)?, [a as u8]);
        if ["data", "index", "data_sync", "head_prefix"].contains(&stage) {
            assert_eq!(a, 1, "{stage}");
        } else {
            assert_eq!(a, 2, "{stage}");
        }
        stratava::clear_stale_writer_lock(
            &path,
            stratava::WriterLockBreakPolicy::BreakIfProcessAbsent,
        )?;
        let mut recovered = Generations::open_writer(&path)?;
        recovered.write_cell(CellKey { scan: 0, ch: 0 }, &Cell { value: 3 })?;
        recovered.commit_cell(CellKey { scan: 0, ch: 0 })?;
        recovered.sync()?;
        assert_eq!(reader.cell(CellKey { scan: 0, ch: 0 })?.value, a);
    }
    Ok(())
}

#[test]
fn explicit_immediate_publishes_and_cache_capacity_is_configurable() -> stratava::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("immediate.stratava");
    let key = CellKey { scan: 0, ch: 0 };
    let mut writer =
        Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 1 })?;
    let mut reader = Generations::open_reader(&path)?;
    writer.write_cell(key, &Cell { value: 77 })?;
    writer.commit_cell(key)?;
    writer.immediate()?;
    assert_eq!(reader.cell_status(key)?, MatrixCellStatus::NotCommitted);
    reader.follow()?;
    assert_eq!(reader.cell(key)?.value, 77);
    for cache in [0, 4096, 65536] {
        let limits = stratava::ReadLimits::STANDARD.with_matrix_generation_cache_bytes(cache);
        let reader = Generations::spec().open_reader_with_resource_limits(&path, limits)?;
        assert_eq!(
            reader
                .read_matrix_cell::<Cell>(stratava::MatrixKey::new(0, 0))?
                .value,
            77
        );
        assert_eq!(
            reader.matrix_generation_path(),
            writer.matrix_generation_path()
        );
    }
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn publication_error_child() -> stratava::Result<()> {
    let Some(path) = std::env::var_os("STRATAVA_MATRIX_GENERATION_ERROR_CHILD") else {
        return Ok(());
    };
    let stage = std::env::var("STRATAVA_MATRIX_GENERATION_FAIL").unwrap();
    let mut writer = Generations::open_writer(&path)?;
    let mut notified = false;
    let error = writer
        .inner_mut()
        .write_matrix_cell_durable(stratava::MatrixKey::new(0, 0), &Cell { value: 2 }, |_| {
            notified = true;
            Ok(())
        })
        .unwrap_err();
    assert!(!notified);
    if stage == "head_sync" {
        assert!(matches!(
            error,
            stratava::Error::MatrixCommittedButDurabilityUnproven { .. }
        ));
    } else {
        assert!(matches!(
            error,
            stratava::Error::MatrixPublicationUncertain { .. }
        ));
    }
    assert!(matches!(
        writer.write_cell(CellKey { scan: 0, ch: 0 }, &Cell { value: 3 }),
        Err(stratava::Error::WriterPoisoned(_))
    ));
    let reader = Generations::open_reader(&path)?;
    assert_eq!(
        reader.cell(CellKey { scan: 0, ch: 0 })?.value,
        if ["head", "head_sync"].contains(&stage.as_str()) {
            2
        } else {
            1
        }
    );
    Ok(())
}
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn publication_io_errors_report_uncertainty_and_do_not_run_the_hook() -> stratava::Result<()> {
    for stage in [
        "data",
        "index",
        "data_sync",
        "head_prefix",
        "head",
        "head_sync",
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("error.stratava");
        let mut writer =
            Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 1 })?;
        writer.write_cell(CellKey { scan: 0, ch: 0 }, &Cell { value: 1 })?;
        writer.commit_cell(CellKey { scan: 0, ch: 0 })?;
        writer.sync()?;
        drop(writer);
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "publication_error_child", "--nocapture"])
            .env("STRATAVA_MATRIX_GENERATION_ERROR_CHILD", &path)
            .env("STRATAVA_MATRIX_GENERATION_FAIL", stage)
            .output()?;
        assert!(
            output.status.success(),
            "{stage}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[test]
fn matrix_only_sync_never_publishes_a_native_tail_that_recovery_will_truncate()
-> stratava::Result<()> {
    for mode in [
        stratava::TransactionMarkerMode::Explicit,
        stratava::TransactionMarkerMode::OnFlush,
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("marker.stratava");
        let spec =
            Generations::spec().with_commit_policy(stratava::CommitPolicy::TransactionMarker(mode));
        let dims = stratava::MatrixDimensions::from_pairs([("scan", 1), ("ch", 1)]);
        let key = stratava::MatrixKey::new(0, 0);
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
        assert_eq!(old.read_matrix_cell::<Cell>(key)?.value, 2);
        assert_eq!(old.blocks::<Log>()?.len(), 1);
        drop(writer);
        let mut recovered = spec.open_writer(&path)?;
        assert_eq!(recovered.read_matrix_cell::<Cell>(key)?.value, 2);
        let reopened = spec.open_reader(&path)?;
        assert_eq!(reopened.blocks::<Log>()?.len(), 1);
        recovered.push(&Log { value: 3 })?;
        recovered.commit_durable()?;
        old.follow()?;
        assert_eq!(old.blocks::<Log>()?.len(), 2);
    }
    Ok(())
}

#[test]
fn skipped_generations_update_growing_shrinking_and_reordered_bitmap_indexes()
-> stratava::Result<()> {
    for integrity in [
        stratava::IntegrityPolicy::None,
        #[cfg(feature = "integrity")]
        stratava::IntegrityPolicy::Crc32,
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("delta-model.stratava");
        let spec = Generations::spec().with_integrity_policy(integrity);
        let mut writer = spec.create_writer_with_dims(
            &path,
            stratava::MatrixDimensions::from_pairs([("scan", 32 * 32768), ("ch", 1)]),
        )?;
        let mut reader = spec.open_reader(&path)?;
        let mut state = 0x361794dbu64;
        let mut model = [None; 32];
        for batch in 0..36 {
            for _ in 0..9 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let page = ((state >> 32) % 32) as usize;
                let key = stratava::MatrixKey::new(page as u64 * 32768, 0);
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
                    let key = stratava::MatrixKey::new(page as u64 * 32768, 0);
                    assert_eq!(
                        reader.matrix_cell_status::<Cell>(key)?,
                        fresh.matrix_cell_status::<Cell>(key)?
                    );
                    if let Some(value) = expected {
                        assert_eq!(reader.read_matrix_cell::<Cell>(key)?.value, *value);
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

#[cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]
#[test]
fn follow_verifies_only_changed_pages_and_rebuilds_only_after_compaction() -> stratava::Result<()> {
    use stratava::MatrixRecoveryReport as Counters;
    for live_pages in [8, 256] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("delta-cost.stratava");
        let spec = Generations::spec().with_integrity_policy(stratava::IntegrityPolicy::Crc32);
        let mut writer = spec.create_writer_with_dims(
            &path,
            stratava::MatrixDimensions::from_pairs([("scan", live_pages * 32768), ("ch", 1)]),
        )?;
        for page in 0..live_pages {
            let key = stratava::MatrixKey::new(page * 32768, 0);
            writer.write_matrix_cell(key, &Cell { value: page })?;
            writer.commit_matrix_cell::<Cell>(key)?;
        }
        writer.sync()?;
        let mut reader = spec.open_reader(&path)?;
        let warm = stratava::MatrixKey::new((live_pages - 1) * 32768, 0);
        reader.read_matrix_cell::<Cell>(warm)?;
        let mut follow_us = Vec::new();
        let mut open_us = Vec::new();
        for value in 1..=12 {
            // A new bit within the first existing bitmap page; no index growth.
            let key = stratava::MatrixKey::new(value, 0);
            writer.write_matrix_cell(key, &Cell { value })?;
            writer.commit_matrix_cell::<Cell>(key)?;
            writer.sync()?;
            Counters::reset_matrix_integrity_counters();
            let start = std::time::Instant::now();
            reader.follow()?;
            follow_us.push(start.elapsed().as_micros());
            assert_eq!(
                Counters::matrix_open_bitmap_pages_visited(),
                1,
                "follow must not verify {live_pages} unchanged live pages"
            );
            Counters::reset_matrix_integrity_counters();
            reader.read_matrix_cell::<Cell>(warm)?;
            assert_eq!(
                Counters::matrix_lazy_fault_bytes_read(),
                0,
                "unchanged bitmap cache pages must survive follow"
            );
            let start = std::time::Instant::now();
            let fresh = spec.open_reader(&path)?;
            open_us.push(start.elapsed().as_micros());
            assert_eq!(fresh.read_matrix_cell::<Cell>(key)?.value, value);
        }
        follow_us.sort_unstable();
        open_us.sort_unstable();
        eprintln!(
            "matrix_follow_cost live_pages={live_pages} follow_median_us={} fresh_open_median_us={}",
            follow_us[6], open_us[6]
        );
        writer.compact_matrix()?;
        // Do not let a subsequent normal publication erase the marker.
        writer.write_matrix_aux("note", 0, &[42])?;
        writer.sync()?;
        Counters::reset_matrix_integrity_counters();
        reader.follow()?;
        assert!(Counters::matrix_open_bitmap_pages_visited() >= live_pages);
        writer.write_matrix_aux("note", 0, &[43])?;
        writer.sync()?;
        Counters::reset_matrix_integrity_counters();
        reader.follow()?;
        assert_eq!(Counters::matrix_open_bitmap_pages_visited(), 0);
    }
    Ok(())
}

#[test]
fn tiny_bitmap_budget_follows_growing_and_shrinking_indexes() -> stratava::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("delta-limit.stratava");
    let spec = Generations::spec().with_integrity_policy(stratava::IntegrityPolicy::None);
    let mut writer = spec.create_writer_with_dims(
        &path,
        stratava::MatrixDimensions::from_pairs([("scan", 2 * 32768), ("ch", 1)]),
    )?;
    let first = stratava::MatrixKey::new(0, 0);
    let second = stratava::MatrixKey::new(32768, 0);
    writer.write_matrix_cell(first, &Cell { value: 1 })?;
    writer.commit_matrix_cell::<Cell>(first)?;
    writer.sync()?;
    let limits = stratava::ReadLimits::STANDARD
        .with_max_matrix_bitmap_bytes(48)
        .with_matrix_metadata_residency(stratava::MatrixMetadataResidency::Lazy { cache_bytes: 0 })
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
    assert_eq!(reader.read_matrix_cell::<Cell>(first)?.value, 2);
    assert_eq!(reader.read_matrix_cell::<Cell>(second)?.value, 3);
    // Small metadata budgets do not turn file growth into an open/follow refusal.
    let reopened = spec.open_reader_with_resource_limits(&path, limits)?;
    assert_eq!(reopened.read_matrix_cell::<Cell>(second)?.value, 3);
    writer.clear_matrix_cell::<Cell>(second)?;
    writer.sync()?;
    reader.follow()?;
    assert_eq!(reader.read_matrix_cell::<Cell>(first)?.value, 2);
    assert_eq!(
        reader.matrix_cell_status::<Cell>(second)?,
        MatrixCellStatus::NotCommitted
    );
    Ok(())
}

#[test]
fn default_matrix_admission_is_independent_of_the_working_set() -> stratava::Result<()> {
    let policies = [
        stratava::IntegrityPolicy::None,
        #[cfg(feature = "integrity")]
        stratava::IntegrityPolicy::Crc32,
    ];
    for integrity in policies {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("wide-sparse.stratava");
        // Beyond the former 16M-cell and 8GiB slot-region defaults. Only two
        // cells and their metadata are written; this is not a physical load test.
        let rows = 1_100_000_000;
        let limits = stratava::ReadLimits::STANDARD
            .with_max_matrix_bitmap_bytes(64 * 1024)
            .with_matrix_metadata_residency(stratava::MatrixMetadataResidency::Lazy {
                cache_bytes: 4096,
            });
        let spec = Generations::spec()
            .with_integrity_policy(integrity)
            .with_resource_limits(limits);
        let mut writer = spec.create_writer_with_dims(
            &path,
            stratava::MatrixDimensions::from_pairs([("scan", rows), ("ch", 1)]),
        )?;
        let first = stratava::MatrixKey::new(0, 0);
        let last = stratava::MatrixKey::new(rows - 1, 0);
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
        assert_eq!(reader.read_matrix_cell::<Cell>(first)?.value, 1);
        assert_eq!(reader.read_matrix_cell::<Cell>(last)?.value, 2);
        let reopened = spec.open_reader(&path)?;
        assert_eq!(reopened.read_matrix_cell::<Cell>(last)?.value, 2);
    }
    Ok(())
}

#[test]
fn follow_spills_many_changed_bitmap_slots_without_publishing_dirty_state() -> stratava::Result<()>
{
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("many-deltas.stratava");
    let spec = Generations::spec().with_integrity_policy(stratava::IntegrityPolicy::None);
    const PAGES: u64 = 96;
    let mut writer = spec.create_writer_with_dims(
        &path,
        stratava::MatrixDimensions::from_pairs([("scan", PAGES * 32768), ("ch", 1)]),
    )?;
    writer.sync()?;
    let limits = stratava::ReadLimits::STANDARD
        .with_max_matrix_bitmap_bytes(16)
        .with_matrix_metadata_residency(stratava::MatrixMetadataResidency::Lazy { cache_bytes: 0 });
    let mut reader = spec.open_reader_with_resource_limits(&path, limits)?;
    for page in 0..PAGES {
        let key = stratava::MatrixKey::new(page * 32768, 0);
        writer.write_matrix_cell(key, &Cell { value: page + 1 })?;
        writer.commit_matrix_cell::<Cell>(key)?;
    }
    reader.follow()?;
    assert_eq!(
        reader.matrix_cell_status::<Cell>(stratava::MatrixKey::new(0, 0))?,
        MatrixCellStatus::NotCommitted
    );
    writer.sync()?;
    reader.follow()?;
    let cold = spec.open_reader_with_resource_limits(&path, limits)?;
    for page in 0..PAGES {
        let key = stratava::MatrixKey::new(page * 32768, 0);
        assert_eq!(reader.read_matrix_cell::<Cell>(key)?.value, page + 1);
        assert_eq!(cold.read_matrix_cell::<Cell>(key)?.value, page + 1);
    }
    Ok(())
}

#[test]
fn damaged_new_root_does_not_partially_advance_the_reader() -> stratava::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("delta-damage.stratava");
    let key = CellKey { scan: 0, ch: 0 };
    let mut writer =
        Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 1 })?;
    writer.write_cell(key, &Cell { value: 1 })?;
    writer.commit_cell(key)?;
    writer.sync()?;
    let mut reader = Generations::open_reader(&path)?;
    let generation = reader.matrix_generation();
    writer.write_cell(key, &Cell { value: 2 })?;
    writer.commit_cell(key)?;
    writer.sync()?;
    let mut log = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(writer.matrix_generation_path().unwrap())?;
    // The final appended page is the new root. Corrupt its checksum, then restore
    // it, so the retry exercises precisely the same candidate generation.
    let checksum = log.metadata()?.len() - 4;
    log.seek(SeekFrom::Start(checksum))?;
    let mut saved = [0; 4];
    log.read_exact(&mut saved)?;
    log.seek(SeekFrom::Start(checksum))?;
    log.write_all(&[saved[0] ^ 1, saved[1], saved[2], saved[3]])?;
    assert!(reader.follow().is_err());
    assert_eq!(reader.matrix_generation(), generation);
    assert_eq!(reader.cell(key)?.value, 1);
    log.seek(SeekFrom::Start(checksum))?;
    log.write_all(&saved)?;
    reader.follow()?;
    assert_eq!(reader.cell(key)?.value, 2);
    Ok(())
}

#[cfg(feature = "mmap")]
#[test]
fn matrix_cell_mapping_charges_only_its_window_and_cow_pages() -> stratava::Result<()> {
    let policies = [
        stratava::IntegrityPolicy::None,
        #[cfg(feature = "integrity")]
        stratava::IntegrityPolicy::Crc32,
    ];
    for integrity in policies {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("matrix-window.stratava");
        let spec = Generations::spec().with_integrity_policy(integrity);
        {
            let mut writer = spec.create_writer_with_dims(
                &path,
                stratava::MatrixDimensions::from_pairs([("scan", 4096), ("ch", 2)]),
            )?;
            for row in [0, 1024, 2048, 4095] {
                let key = stratava::MatrixKey::new(row, 1);
                writer.write_matrix_cell(key, &Cell { value: row + 100 })?;
                writer.commit_matrix_cell::<Cell>(key)?;
            }
            writer.sync()?;
        }
        let limited = spec.with_resource_limits(
            stratava::ReadLimits::STANDARD
                .with_max_mmap_len(8)
                .with_max_materialized_bytes(4096),
        );
        let reader = limited.open_reader(&path)?;
        // SAFETY: this fixture has no writer or external modifier.
        assert!(unsafe { reader.mmap_matrix() }.is_err());
        // SAFETY: the primary and its COW generation stay immutable.
        let first = unsafe { reader.mmap_matrix_cell::<Cell>(stratava::MatrixKey::new(0, 1))? };
        // SAFETY: same immutable generation; independent mapping owner.
        let last = unsafe { reader.mmap_matrix_cell::<Cell>(stratava::MatrixKey::new(4095, 1))? };
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
            unsafe { reader.mmap_matrix_cell::<Cell>(stratava::MatrixKey::new(1, 1)) },
            Err(stratava::Error::MatrixNotCommitted)
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

/// Opening a direct directory reads its header only. Increasing the number of
/// live pages must not trigger an enumeration, an in-memory mirror, or a spill.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn on_demand_open_reads_no_page_list_even_with_zero_directory_cache() -> stratava::Result<()> {
    use stratava::{MatrixMetadataVerification, MatrixRecoveryReport as Counters, ReadLimits};
    for cache in [0, 4096, 1024 * 1024] {
        let mut open_bytes = Vec::new();
        for pages in [16, 1024] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("direct-directory.stratava");
            let spec = Generations::spec().with_resource_limits(
                ReadLimits::STANDARD
                    .with_matrix_directory_cache_bytes(cache)
                    .with_matrix_metadata_verification(MatrixMetadataVerification::OnDemand),
            );
            let mut writer = spec.create_writer_with_dims(
                &path,
                stratava::MatrixDimensions::from_pairs([("scan", pages * 32768), ("ch", 1)]),
            )?;
            for page in 0..pages {
                let key = stratava::MatrixKey::new(page * 32768, 0);
                writer.write_matrix_cell(key, &Cell { value: page })?;
                writer.commit_matrix_cell::<Cell>(key)?;
            }
            writer.sync()?;
            Counters::reset_matrix_integrity_counters();
            let mut reader = spec.open_reader(&path)?;
            open_bytes.push(Counters::matrix_open_bitmap_bytes_read());
            assert_eq!(Counters::matrix_open_bitmap_pages_visited(), 0);
            for page in [pages - 1, 0, pages / 2, pages - 1] {
                Counters::reset_matrix_integrity_counters();
                assert_eq!(
                    reader
                        .read_matrix_cell::<Cell>(stratava::MatrixKey::new(page * 32768, 0))?
                        .value,
                    page
                );
                assert!(
                    Counters::matrix_open_bitmap_bytes_read() <= 4 * 4096,
                    "point lookup enumerated directory entries"
                );
            }
            let removed = stratava::MatrixKey::new((pages / 2) * 32768, 0);
            writer.clear_matrix_cell::<Cell>(removed)?;
            // A freshly opened reader still binds the old confirmed metadata.
            assert_eq!(
                spec.open_reader(&path)?
                    .read_matrix_cell::<Cell>(removed)?
                    .value,
                pages / 2
            );
            writer.sync()?;
            reader.follow()?;
            assert_eq!(
                reader.matrix_cell_status::<Cell>(removed)?,
                MatrixCellStatus::NotCommitted
            );
            let tail = stratava::MatrixKey::new((pages - 1) * 32768, 0);
            assert_eq!(reader.read_matrix_cell::<Cell>(tail)?.value, pages - 1);
        }
        assert_eq!(open_bytes[0], open_bytes[1]);
        assert!(
            open_bytes[1] <= 16,
            "open enumerated the live page list: {open_bytes:?}"
        );
    }
    Ok(())
}
