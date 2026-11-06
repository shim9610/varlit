#![cfg(feature = "mmap")]
#[path = "support/append_only_cases.rs"]
#[allow(dead_code)]
mod cases;
use cases::*;
use varlit::{AppendOnlyWriter, Endian, Error, ReadValidation, VarlitFile, decode_from_slice};

#[test]
fn empty_snapshot_debug_exposes_only_metadata_while_the_header_changes() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut writer = writer(&directory, default_validation())?;
    let mut reader = writer.reader_token().open_reader()?;
    let empty = reader.mmap_payloads()?;
    assert!(empty.is_empty());
    writer.push_info(&Value { value: 19 })?;
    writer.commit_durable()?;
    assert!(reader.follow()? > 0);
    assert!(empty.is_empty());
    assert!(empty.block_payload_window::<Value>(0)?.is_none());
    let debug = format!("{empty:?}");
    assert!(debug.contains("MmapPayloads { base: 0, records: 0"));
    assert!(!debug.contains("mmap: Mmap"));
    assert_eq!(entries(&reader)?.len(), 1);
    Ok(())
}

#[test]
fn on_flush_policy_publishes_without_changing_earlier_mapped_bytes() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let spec = spec().with_commit_policy(varlit::CommitPolicy::TransactionMarker(
        varlit::TransactionMarkerMode::OnFlush,
    ));
    let file = spec.create(directory.path().join("on-flush"))?;
    // SAFETY: this private object's only writer is the append-only facade.
    let mut writer = unsafe { AppendOnlyWriter::new(file)? };
    writer.push_info(&Value { value: 19 })?;
    writer.flush()?;
    let mut reader = writer.reader_token().open_reader()?;
    let mapped = reader.mmap_payload_window(&entries(&reader)?[0])?;
    writer.push_info(&Value { value: 23 })?;
    assert_eq!(reader.follow()?, 0);
    writer.flush()?;
    assert!(reader.follow()? > 0);
    assert_eq!(entries(&reader)?.len(), 2);
    assert_eq!(
        decode_from_slice::<Value>(mapped.as_bytes(), Endian::Little)?,
        Value { value: 19 }
    );
    Ok(())
}

#[test]
fn final_mapping_owns_writer_exclusion_even_after_every_handle_drops() -> varlit::Result<()> {
    let (directory, writer, reader) = populated()?;
    let token = writer.reader_token();
    let entry = entries(&reader)?.remove(0);
    let window = reader.mmap_payload_window(&entry)?;
    let snapshot = reader.mmap_payloads()?;
    let path = directory.path().join("session");
    let alias = directory.path().join("alias");
    std::fs::hard_link(&path, &alias)?;
    drop(writer);
    let another = token.open_reader()?;
    assert_eq!(entries(&another)?.len(), 1);
    drop(another);
    drop(token);
    drop(reader);
    assert_locked(&path);
    assert_locked(&alias);
    assert_eq!(
        decode_from_slice::<Value>(window.as_bytes(), Endian::Little)?,
        Value { value: 19 }
    );
    drop(window);
    assert_locked(&path);
    assert_eq!(
        decode_from_slice::<Value>(
            snapshot.block_payload_window::<Value>(0)?.unwrap(),
            Endian::Little
        )?,
        Value { value: 19 }
    );
    drop(snapshot);
    let reopened = VarlitFile::open(spec(), &alias)?;
    assert_eq!(
        reopened.blocks::<Value>()?.get(0)?,
        Some(Value { value: 19 })
    );
    Ok(())
}

#[test]
fn token_alone_keeps_the_file_excluded_from_mutation() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let writer = writer(&directory, default_validation())?;
    let token = writer.reader_token();
    drop(writer);
    assert_locked(&directory.path().join("session"));
    drop(token);
    VarlitFile::open(spec(), directory.path().join("session"))?;
    Ok(())
}

#[test]
fn concurrent_registration_release_and_unwind_keep_native_exclusion() -> varlit::Result<()> {
    let (directory, writer, reader) = populated()?;
    let keeper = reader.mmap_payload_window(&entries(&reader)?[0])?;
    let tokens: Vec<_> = (0..8).map(|_| writer.reader_token()).collect();
    drop(reader);
    drop(writer);
    std::thread::scope(|scope| -> varlit::Result<()> {
        let mut joins = Vec::new();
        for token in tokens {
            joins.push(scope.spawn(move || -> varlit::Result<()> {
                for _ in 0..16 {
                    let reader = token.open_reader()?;
                    let mapped = reader.mmap_payload_window(&entries(&reader)?[0])?;
                    drop(reader);
                    assert_eq!(
                        decode_from_slice::<Value>(mapped.as_bytes(), Endian::Little)?,
                        Value { value: 19 }
                    );
                    std::thread::yield_now();
                }
                Ok(())
            }));
        }
        for _ in 0..128 {
            assert_locked(&directory.path().join("session"));
            std::thread::yield_now();
        }
        for join in joins {
            join.join().unwrap()?;
        }
        Ok(())
    })?;
    assert_locked(&directory.path().join("session"));
    // The final owner may disappear during panic unwinding. Its native
    // exclusion must be released only after its mapping has been unmapped.
    let panic = std::panic::catch_unwind(move || {
        let _keeper = keeper;
        panic!("exercise last mapped owner cleanup");
    });
    assert!(panic.is_err());
    VarlitFile::open(spec(), directory.path().join("session"))?;
    Ok(())
}

#[test]
fn eight_workers_keep_old_borrows_while_following_only_confirmed_appends() -> varlit::Result<()> {
    use std::sync::mpsc;
    use std::time::Duration;
    let (_directory, mut writer, initial) = populated()?;
    drop(initial);
    let token = writer.reader_token();
    std::thread::scope(|scope| -> varlit::Result<()> {
        let (send, ready) = mpsc::channel();
        let mut commands = Vec::new();
        let mut joins = Vec::new();
        for _ in 0..8 {
            let (command, receive) = mpsc::channel();
            commands.push(command);
            let send = send.clone();
            let token = &token;
            joins.push(scope.spawn(move || -> varlit::Result<()> {
                let mut reader = token.open_reader()?;
                let window = reader.mmap_payload_window(&entries(&reader)?[0])?;
                let snapshot = reader.mmap_payloads()?;
                let bytes = window.as_bytes();
                send.send(0).unwrap();
                assert_eq!(receive.recv_timeout(Duration::from_secs(30)).unwrap(), 1);
                assert_eq!(reader.follow()?, 0);
                assert_eq!(entries(&reader)?.len(), 1);
                assert!(snapshot.block_payload_window::<Value>(1)?.is_none());
                send.send(1).unwrap();
                assert_eq!(receive.recv_timeout(Duration::from_secs(30)).unwrap(), 2);
                assert!(reader.follow()? > 0);
                let entries = entries(&reader)?;
                assert_eq!(entries.len(), 2);
                let next = reader.mmap_payload_window(&entries[1])?;
                assert_eq!(
                    decode_from_slice::<Value>(next.as_bytes(), Endian::Little)?,
                    Value { value: 23 }
                );
                assert_eq!(
                    decode_from_slice::<Value>(bytes, Endian::Little)?,
                    Value { value: 19 }
                );
                assert!(snapshot.block_payload_window::<Value>(1)?.is_none());
                Ok(())
            }));
        }
        drop(send);
        for _ in 0..8 {
            assert_eq!(ready.recv_timeout(Duration::from_secs(30)).unwrap(), 0);
        }
        writer.push_info(&Value { value: 23 })?;
        writer.flush()?;
        for command in &commands {
            command.send(1).unwrap();
        }
        for _ in 0..8 {
            assert_eq!(ready.recv_timeout(Duration::from_secs(30)).unwrap(), 1);
        }
        #[cfg(feature = "scalable-fault-injection")]
        {
            VarlitFile::inject_commit_durability_failures(1);
            assert!(matches!(
                writer.commit_durable(),
                Err(Error::CommittedButDurabilityUnproven { .. })
            ));
        }
        // Also retries the already-published durability request under injection.
        writer.commit_durable()?;
        for command in commands {
            command.send(2).unwrap();
        }
        for join in joins {
            join.join().unwrap()?;
        }
        Ok(())
    })
}

#[cfg(feature = "integrity")]
#[test]
fn matrix_storage_cannot_enter_the_append_only_facade() -> varlit::Result<()> {
    varlit::varlit_format! {
        pub format MutableMatrix {
            magic: b"MMEXCLUDE"; version: 1; integrity: crc32_with_header;
            dims { row: u32, column: u32, }
            commit: cell_bitmap { keyspace = [row, column]; categories = [data]; };
            blocks { matrix Cell(id = 82, dims = [row, column], category = data) { value: u64, } }
        }
    }
    let directory = tempfile::tempdir()?;
    let file = MutableMatrix::spec()
        .with_commit_policy(varlit::CommitPolicy::TransactionMarker(
            varlit::TransactionMarkerMode::Explicit,
        ))
        .create_with_dims(
            directory.path().join("matrix"),
            varlit::MatrixDimensions::from_pairs([("row", 1), ("column", 1)]),
        )?;
    // SAFETY: private test files; admission must refuse this mutable layout.
    assert!(matches!(
        unsafe { AppendOnlyWriter::new(file) },
        Err(Error::InvalidFormatSpec(_))
    ));
    Ok(())
}

#[test]
fn admission_refuses_unlocked_readers_and_markerless_formats() -> varlit::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("session");
    drop(spec().create(&path)?);
    let reader = VarlitFile::open_readonly(spec(), &path)?;
    // SAFETY: private test files; neither admission succeeds.
    assert!(matches!(
        unsafe { AppendOnlyWriter::new(reader) },
        Err(Error::InvalidFormatSpec(_))
    ));
    let markerless = SessionFormat::spec().create(&path)?;
    // SAFETY: private file, no other users.
    assert!(matches!(
        unsafe { AppendOnlyWriter::new(markerless) },
        Err(Error::InvalidFormatSpec(_))
    ));
    SessionFormat::spec().open(&path)?;
    Ok(())
}

#[test]
fn replaced_path_cannot_launder_a_token_to_another_backing_object() -> varlit::Result<()> {
    let (directory, mut writer, mut reader) = populated()?;
    let token = writer.reader_token();
    let window = reader.mmap_payload_window(&entries(&reader)?[0])?;
    let path = directory.path().join("session");
    std::fs::rename(&path, directory.path().join("retired"))?;
    let other = directory.path().join("other");
    drop(spec().create(&other)?);
    std::fs::rename(other, &path)?;
    assert!(matches!(
        token.open_reader(),
        Err(Error::InvalidFormatSpec(_))
    ));
    assert_eq!(
        decode_from_slice::<Value>(window.as_bytes(), Endian::Little)?,
        Value { value: 19 }
    );
    // A replacement's malformed metadata must never be parsed before identity
    // refusal. It is a different object, not the captured backing object.
    std::fs::write(&path, b"not a varlit file")?;
    assert!(matches!(
        token.open_reader(),
        Err(Error::InvalidFormatSpec(_))
    ));
    writer.push_info(&Value { value: 23 })?;
    writer.commit_durable()?;
    assert!(reader.follow()? > 0);
    assert_eq!(entries(&reader)?.len(), 2);
    drop(writer);
    drop(reader);
    drop(token);
    drop(window);
    Ok(())
}

#[test]
fn every_read_policy_refuses_fabricated_header_and_uncommitted_windows() -> varlit::Result<()> {
    for policy in [
        #[cfg(feature = "integrity")]
        ReadValidation::FULL,
        #[cfg(feature = "integrity")]
        ReadValidation::CRC_ONLY,
        ReadValidation::STRUCTURE_ONLY,
        ReadValidation::TRUSTED,
    ] {
        let directory = tempfile::tempdir()?;
        let mut writer = writer(&directory, default_validation())?;
        writer.push_info(&Value { value: 19 })?;
        writer.commit_durable()?;
        let token = writer.reader_token();
        let limits = varlit::ReadLimits::STANDARD.with_read_validation(policy);
        let mut reader = token.open_reader_with_resource_limits(limits)?;
        // Even a forged `committed = true` cannot authorize a real staged
        // record, either through follow or a reader opened after tail flush.
        let pending = writer.push_info(&Value { value: 19 })?;
        writer.flush()?;
        assert_eq!(reader.follow()?, 0);
        let after_tail = token.open_reader_with_resource_limits(limits)?;
        let mut forged = entries(&reader)?.remove(0);
        forged.sequence = pending.sequence;
        forged.record_offset = pending.record_offset;
        forged.payload_offset = pending.payload_offset;
        forged.payload_len = pending.payload_len;
        forged.footer_offset = pending.footer_offset;
        forged.prev_same_block_offset = pending.prev_same_block_offset;
        forged.prev_same_key_offset = pending.prev_same_key_offset;
        forged.committed = true;
        assert!(reader.mmap_payload_window(&forged).is_err());
        assert!(after_tail.mmap_payload_window(&forged).is_err());
        let reader = token.open_reader_with_resource_limits(
            varlit::ReadLimits::STANDARD.with_read_validation(policy),
        )?;
        let mut entry = entries(&reader)?.remove(0);
        entry.committed = false;
        assert!(reader.mmap_payload_window(&entry).is_err());
        entry.committed = true;
        entry.payload_offset = 0;
        assert!(reader.mmap_payload_window(&entry).is_err());
        entry = entries(&reader)?.remove(0);
        entry.payload_len = u64::MAX;
        assert!(reader.mmap_payload_window(&entry).is_err());
    }
    Ok(())
}

#[test]
fn mapping_limit_refusal_keeps_the_token_and_committed_data_usable() -> varlit::Result<()> {
    let (_directory, writer, reader) = populated()?;
    let token = writer.reader_token();
    let limited = token
        .open_reader_with_resource_limits(varlit::ReadLimits::STANDARD.with_max_mmap_len(0))?;
    assert!(matches!(
        limited.mmap_payloads(),
        Err(Error::LimitExceeded { .. })
    ));
    assert!(matches!(
        limited.mmap_payload_window(&entries(&reader)?.remove(0)),
        Err(Error::LimitExceeded { .. })
    ));
    let retry = token.open_reader()?;
    assert!(
        retry
            .mmap_payloads()?
            .block_payload_window::<Value>(0)?
            .is_some()
    );
    Ok(())
}

#[test]
fn partial_resource_overrides_preserve_format_defaults_and_allow_explicit_retry()
-> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let file = spec()
        .with_resource_defaults(varlit::ReadLimits::MISSING.with_max_mmap_len(1))
        .create(directory.path().join("partial"))?;
    // SAFETY: exclusively owned private backing object with no external writers.
    let mut writer = unsafe { AppendOnlyWriter::new(file)? };
    writer.push_info(&Value { value: 19 })?;
    writer.commit_durable()?;
    let token = writer.reader_token();
    let partial = token.open_reader_with_resource_limits(
        varlit::ReadLimits::MISSING.with_read_validation(ReadValidation::TRUSTED),
    )?;
    assert!(matches!(
        partial.mmap_payloads(),
        Err(Error::LimitExceeded { .. })
    ));
    let retry = token
        .open_reader_with_resource_limits(varlit::ReadLimits::MISSING.with_max_mmap_len(1024))?;
    assert!(
        retry
            .mmap_payloads()?
            .block_payload_window::<Value>(0)?
            .is_some()
    );
    Ok(())
}

#[test]
fn mapped_owners_can_move_between_workers_without_losing_protection() -> varlit::Result<()> {
    fn shareable<T: Send + Sync>() {}
    shareable::<varlit::GuardedMmapWindow>();
    shareable::<varlit::GuardedMmapPayloads>();
    let (directory, writer, reader) = populated()?;
    let mapped = reader.mmap_payload_window(&entries(&reader)?[0])?;
    drop(writer);
    drop(reader);
    std::thread::scope(|s| {
        s.spawn(move || {
            assert_locked(&directory.path().join("session"));
            assert!(!mapped.as_bytes().is_empty());
        })
        .join()
        .unwrap()
    });
    Ok(())
}
