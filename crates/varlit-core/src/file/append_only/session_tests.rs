use super::super::tests::{Number, spec};
use super::*;

fn prepared() -> Result<(tempfile::TempDir, AppendOnlySession)> {
    let directory = tempfile::tempdir()?;
    let file = spec().create(directory.path().join("session"))?;
    // SAFETY: all modifications of the private object use this session.
    Ok((directory, unsafe { AppendOnlySession::new(file)? }))
}

#[test]
fn failed_append_status_is_latched_across_close_recovery_and_reconnect() -> Result<()> {
    let (_directory, session) = prepared()?;
    let mut writer = session.open_writer()?;
    writer.push_info(&Number(19))?;
    writer.commit_durable()?;
    let token = session.reader_token();
    let reader = token.open_reader()?;
    let mapped = reader.mmap_payloads()?;
    let before = reader.session_status()?;
    inject_write_fault(WriteFault::AppendAfterHeaderWithRollbackFailure);
    assert!(matches!(
        writer.push_info(&Number(23)),
        Err(Error::WriteRollbackFailed { .. })
    ));
    let failed = reader.wait_for_change(before, Duration::ZERO)?;
    assert_eq!(failed.writer, AppendOnlyWriterState::Active);
    assert!(failed.recovery_required);
    let facts = failed.last_error.unwrap();
    assert_eq!(facts.operation, "push_info");
    assert_eq!(facts.category, "append rollback failed");
    assert_eq!(facts.writer_epoch, 1);
    assert_eq!(
        crate::decode_from_slice::<Number>(
            mapped.block_payload_window::<Number>(0)?.unwrap(),
            Endian::Little
        )?,
        Number(19)
    );
    drop(writer);
    assert_eq!(
        token.session_status()?.writer,
        AppendOnlyWriterState::Closed
    );
    assert!(matches!(
        session.recover(),
        Err(Error::AppendOnlyReadersActive)
    ));
    drop(mapped);
    drop(reader);
    session.recover()?;
    let mut writer = session.open_writer()?;
    let status = token.session_status()?;
    assert_eq!(status.writer_epoch, 2);
    assert!(!status.recovery_required);
    assert_eq!(status.last_error, Some(facts));
    writer.push_info(&Number(29))?;
    writer.commit_durable()?;
    Ok(())
}

#[test]
fn recovery_excludes_reader_registration_and_writer_handoff_until_complete() -> Result<()> {
    let (_directory, session) = prepared()?;
    let token = session.reader_token();
    let (entered, ready) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    std::thread::scope(|scope| -> Result<()> {
        let authority = &session;
        let recovery = scope.spawn(move || {
            authority.recover_using(move |spec, path, file| {
                entered.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(10)).unwrap();
                let mut scratch = BufferedScan::new(spec);
                VarlitFile::open_writer_file_with_scratch(spec, path, file, &mut scratch)
            })
        });
        ready.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            token.session_status()?.writer,
            AppendOnlyWriterState::Recovering
        );
        assert!(matches!(
            token.open_reader(),
            Err(Error::AppendOnlySessionBusy)
        ));
        assert!(matches!(
            session.open_writer(),
            Err(Error::AppendOnlySessionBusy)
        ));
        assert!(matches!(session.close(), Err(Error::AppendOnlySessionBusy)));
        assert!(matches!(
            session.recover(),
            Err(Error::AppendOnlySessionBusy)
        ));
        release.send(()).unwrap();
        recovery.join().unwrap()?;
        Ok(())
    })?;
    token.open_reader()?;
    session.open_writer()?.close()?;
    Ok(())
}

#[test]
fn recovery_panic_releases_registration_gate_and_can_be_retried() -> Result<()> {
    let (_directory, session) = prepared()?;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        session.recover_using(|_, _, _| panic!("interrupt native recovery"))
    }));
    assert!(panic.is_err());
    let status = session.status()?;
    assert_eq!(status.writer, AppendOnlyWriterState::Closed);
    assert!(status.recovery_required);
    assert_eq!(status.last_error.unwrap().category, "recovery interrupted");
    session.reader_token().open_reader()?;
    assert!(matches!(
        session.open_writer(),
        Err(Error::AppendOnlyRecoveryRequired)
    ));
    session.recover()?;
    session.open_writer()?.close()?;
    Ok(())
}

#[test]
fn failed_recovery_retries_without_path_reopen_and_keeps_parent_sync_obligation() -> Result<()> {
    let (_directory, session) = prepared()?;
    let error =
        session.recover_using(|_, _, _| Err(std::io::Error::other("recovery I/O failure").into()));
    assert!(matches!(error, Err(Error::Io(_))));
    assert!(session.status()?.recovery_required);
    assert_eq!(
        session.status()?.last_error.unwrap().io_kind,
        Some(std::io::ErrorKind::Other)
    );
    session.recover()?;
    let slot = session.authority.slot()?;
    assert!(slot.file.as_ref().unwrap().pending_pathname_parent_sync);
    drop(slot);
    let mut writer = session.open_writer()?;
    writer.push_info(&Number(19))?;
    writer.commit_durable()?;
    writer.close()?;
    assert!(
        !session
            .authority
            .slot()?
            .file
            .as_ref()
            .unwrap()
            .pending_pathname_parent_sync
    );
    Ok(())
}

#[test]
fn huge_wait_deadline_is_refused_and_zero_timeout_is_finite() -> Result<()> {
    let (_directory, session) = prepared()?;
    let observed = session.status()?;
    assert_eq!(session.wait_for_change(observed, Duration::ZERO)?, observed);
    assert!(matches!(
        session.wait_for_change(observed, Duration::MAX),
        Err(Error::ResourceArithmeticOverflow { .. })
    ));
    Ok(())
}

#[test]
fn counter_saturation_and_coordination_poison_fail_without_losing_authority() -> Result<()> {
    let (_directory, session) = prepared()?;
    session.authority.slot()?.epoch = u64::MAX;
    assert!(matches!(
        session.open_writer(),
        Err(Error::ResourceArithmeticOverflow { .. })
    ));
    assert!(session.authority.slot()?.file.is_some());
    session.authority.slot()?.epoch = 0;
    session.authority.protection.signal.lock().unwrap().revision = u64::MAX;
    let observed = session.status()?;
    let writer = session.open_writer()?;
    assert_eq!(
        session.wait_for_change(observed, Duration::ZERO)?.writer,
        AppendOnlyWriterState::Active
    );
    writer.close()?;
    assert_eq!(session.status()?.revision, u64::MAX);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _slot = session.authority.slot.lock().unwrap();
        panic!("poison coordination while retaining native authority");
    }));
    assert!(panic.is_err());
    assert!(matches!(
        session.open_writer(),
        Err(Error::AppendOnlySessionPoisoned)
    ));
    assert!(matches!(
        session.recover(),
        Err(Error::AppendOnlySessionPoisoned)
    ));
    assert!(matches!(
        spec().open(&session.authority.protection.path),
        Err(Error::WriterLockHeld(_))
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn recovery_uses_the_pinned_object_after_path_replacement() -> Result<()> {
    let (directory, session) = prepared()?;
    let original = directory.path().join("original");
    let path = directory.path().join("session");
    let mut writer = session.open_writer()?;
    writer.push_info(&Number(19))?;
    writer.commit_durable()?;
    writer.push_info(&Number(23))?;
    assert!(matches!(
        writer.close(),
        Err(Error::AppendOnlyRecoveryRequired)
    ));
    std::fs::rename(&path, &original)?;
    // The original object remains exclusively session-owned. A different
    // pathname occupant must neither acquire its read capability nor be rebuilt.
    let replacement_path = directory.path().join("replacement");
    let mut replacement = spec().create(&replacement_path)?;
    replacement.push_info(&Number(41))?;
    replacement.commit_durable()?;
    drop(replacement);
    std::fs::rename(&replacement_path, &path)?;
    let before = std::fs::read(&path)?;
    assert!(matches!(
        session.reader_token().open_reader(),
        Err(Error::InvalidFormatSpec(_))
    ));
    session.recover()?;
    assert_eq!(std::fs::read(&path)?, before);
    std::fs::remove_file(&path)?;
    std::fs::rename(&original, &path)?;
    let reader = session.reader_token().open_reader()?;
    let mut entries = Vec::new();
    reader.block_entries_into::<Number>(&mut entries)?;
    assert_eq!(entries.len(), 1);
    assert_eq!(
        crate::decode_from_slice::<Number>(
            reader.mmap_payload_window(&entries[0])?.as_bytes(),
            Endian::Little
        )?,
        Number(19)
    );
    Ok(())
}

#[test]
fn final_authority_cleanup_failure_is_observable_without_invalidating_views() -> Result<()> {
    for explicit in [false, true] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("session");
        let file = spec()
            .with_liveness_policy(LivenessPolicy::FooterFlags)
            .create(&path)?;
        // SAFETY: the private test session owns all backing-object mutation.
        let session = unsafe { AppendOnlySession::new(file)? };
        let mut writer = session.open_writer()?;
        writer.push_info(&Number(19))?;
        writer.commit_durable()?;
        writer.close()?;
        let token = session.reader_token();
        let reader = token.open_reader()?;
        let mapped = reader.mmap_payloads()?;
        // A real native read-only handle cannot write the clean liveness flags.
        // The session's authoritative native guard still pins the same object.
        session.authority.slot()?.file.as_mut().unwrap().file = RecordFile::new(File::open(&path)?);
        if explicit {
            assert!(matches!(session.close(), Err(Error::Io(_))));
        }
        drop(session);
        let status = token.session_status()?;
        assert_eq!(status.writer, AppendOnlyWriterState::Finished);
        assert!(status.recovery_required);
        let facts = status.last_error.unwrap();
        assert_eq!(
            facts.operation,
            if explicit {
                "session close"
            } else {
                "session drop"
            }
        );
        assert!(facts.io_kind.is_some());
        assert!(facts.raw_os_error.is_some());
        assert_eq!(
            crate::decode_from_slice::<Number>(
                mapped.block_payload_window::<Number>(0)?.unwrap(),
                Endian::Little
            )?,
            Number(19)
        );
        assert!(matches!(spec().open(&path), Err(Error::WriterLockHeld(_))));
    }
    Ok(())
}
