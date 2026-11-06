#![cfg(feature = "mmap")]
#[path = "support/append_only_cases.rs"]
#[allow(dead_code)]
mod cases;
use cases::*;
use std::time::Duration;
use varlit::{AppendOnlySession, AppendOnlyWriterState as State, Endian, Error, decode_from_slice};

fn session(directory: &tempfile::TempDir) -> varlit::Result<AppendOnlySession> {
    let file = spec().create(directory.path().join("session"))?;
    // SAFETY: only this private session modifies the original backing object.
    unsafe { AppendOnlySession::new(file) }
}

#[test]
fn reader_first_and_repeated_writer_reconnect_preserve_old_views() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let session = session(&directory)?;
    let token = session.reader_token();
    let mut reader = token.open_reader()?;
    let empty = reader.mmap_payloads()?;
    assert_eq!(reader.session_status()?.writer, State::NotStarted);
    assert!(empty.is_empty());
    let mut writer = session.open_writer()?;
    writer.push_info(&Value { value: 19 })?;
    writer.commit_durable()?;
    reader.follow()?;
    let original = reader.mmap_payload_window(&entries(&reader)?[0])?;
    writer.close()?;
    for epoch in 2..=8 {
        assert_eq!(token.session_status()?.writer, State::Closed);
        let mut writer = session.open_writer()?;
        assert_eq!(session.status()?.writer_epoch, epoch);
        writer.push_info(&Value { value: epoch })?;
        writer.commit_durable()?;
        drop(writer);
        assert!(reader.follow()? > 0);
        assert_eq!(entries(&reader)?.len(), epoch as usize);
        assert_eq!(
            decode_from_slice::<Value>(original.as_bytes(), Endian::Little)?,
            Value { value: 19 }
        );
        assert!(empty.is_empty());
    }
    assert_locked(&directory.path().join("session"));
    session.close()?;
    assert_eq!(reader.session_status()?.writer, State::Finished);
    assert!(matches!(
        session.open_writer(),
        Err(Error::AppendOnlySessionClosed)
    ));
    assert_eq!(entries(&token.open_reader()?)?.len(), 8);
    Ok(())
}

#[test]
fn reconnect_does_not_rescan_published_records() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let file = spec()
        .with_resource_limits(varlit::ReadLimits::MISSING.with_max_scan_bytes(0))
        .create(directory.path().join("session"))?;
    // SAFETY: exclusive private session, no external changes.
    let session = unsafe { AppendOnlySession::new(file)? };
    for value in 0..4 {
        let mut writer = session.open_writer()?;
        writer.push_info(&Value { value })?;
        writer.commit_durable()?;
        writer.close()?;
    }
    // The admitted zero scan budget would reject an actual reopen/scan of this
    // populated file. A writer reconnect must reuse its maintained native state.
    let reader = session.reader_token().open_reader_with_resource_limits(
        varlit::ReadLimits::MISSING.with_max_scan_bytes(1 << 20),
    )?;
    assert_eq!(entries(&reader)?.len(), 4);
    Ok(())
}

#[test]
fn uncommitted_close_requires_exclusive_recovery_but_keeps_read_capabilities() -> varlit::Result<()>
{
    let directory = tempfile::tempdir()?;
    let session = session(&directory)?;
    let token = session.reader_token();
    let mut writer = session.open_writer()?;
    writer.push_info(&Value { value: 19 })?;
    writer.commit_durable()?;
    let mut reader = token.open_reader()?;
    let old = reader.mmap_payload_window(&entries(&reader)?[0])?;
    writer.push_info(&Value { value: 23 })?;
    assert!(matches!(
        writer.close(),
        Err(Error::AppendOnlyRecoveryRequired)
    ));
    let status = reader.session_status()?;
    assert_eq!(status.writer, State::Closed);
    assert!(status.recovery_required);
    assert_eq!(status.last_error.unwrap().category, "uncommitted tail");
    assert_eq!(reader.follow()?, 0);
    assert!(matches!(
        session.open_writer(),
        Err(Error::AppendOnlyRecoveryRequired)
    ));
    assert!(matches!(
        session.recover(),
        Err(Error::AppendOnlyReadersActive)
    ));
    drop(reader);
    // The owned view retains its originating reader registration.
    assert!(matches!(
        session.recover(),
        Err(Error::AppendOnlyReadersActive)
    ));
    assert_eq!(
        decode_from_slice::<Value>(old.as_bytes(), Endian::Little)?,
        Value { value: 19 }
    );
    drop(old);
    session.recover()?;
    assert!(!token.session_status()?.recovery_required);
    let mut writer = session.open_writer()?;
    writer.push_info(&Value { value: 29 })?;
    writer.commit_durable()?;
    writer.close()?;
    let reader = token.open_reader()?;
    let rows = entries(&reader)?;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        decode_from_slice::<Value>(
            reader.mmap_payload_window(&rows[1])?.as_bytes(),
            Endian::Little
        )?,
        Value { value: 29 }
    );
    Ok(())
}

#[test]
fn writer_panic_is_observable_and_does_not_publish_its_tail() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let session = session(&directory)?;
    let mut writer = session.open_writer()?;
    writer.push_info(&Value { value: 19 })?;
    writer.commit_durable()?;
    writer.close()?;
    let token = session.reader_token();
    let reader = token.open_reader()?;
    let old = reader.mmap_payload_window(&entries(&reader)?[0])?;
    let authority = session.clone();
    let panic = std::thread::spawn(move || {
        let mut writer = authority.open_writer().unwrap();
        writer.push_info(&Value { value: 23 }).unwrap();
        panic!("abandon active writer during unwind");
    })
    .join();
    assert!(panic.is_err());
    let status = token.session_status()?;
    assert_eq!(status.writer, State::Closed);
    assert!(status.recovery_required);
    assert_eq!(status.last_error.unwrap().category, "writer panicked");
    assert_eq!(
        decode_from_slice::<Value>(old.as_bytes(), Endian::Little)?,
        Value { value: 19 }
    );
    drop(old);
    drop(reader);
    session.recover()?;
    assert_eq!(entries(&token.open_reader()?)?.len(), 1);
    Ok(())
}

#[test]
fn waiters_observe_start_close_and_final_write_authority_drop() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let session = session(&directory)?;
    let token = session.reader_token();
    let observed = token.session_status()?;
    std::thread::scope(|scope| -> varlit::Result<()> {
        let (ack, received) = std::sync::mpsc::channel();
        let mut commands = Vec::new();
        let mut joins = Vec::new();
        for _ in 0..8 {
            let (command, next) = std::sync::mpsc::channel();
            commands.push(command);
            let ack = ack.clone();
            let token = &token;
            joins.push(scope.spawn(move || -> varlit::Result<()> {
                let mut observed = observed;
                for expected in [State::Active, State::Closed, State::Finished] {
                    observed = token.wait_for_change(observed, Duration::from_secs(10))?;
                    assert_eq!(observed.writer, expected);
                    ack.send(()).unwrap();
                    next.recv_timeout(Duration::from_secs(10)).unwrap();
                }
                Ok(())
            }));
        }
        drop(ack);
        let writer = session.open_writer()?;
        for _ in 0..8 {
            received.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        for command in &commands {
            command.send(()).unwrap();
        }
        writer.close()?;
        for _ in 0..8 {
            received.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        for command in &commands {
            command.send(()).unwrap();
        }
        drop(session);
        for _ in 0..8 {
            received.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        for command in commands {
            command.send(()).unwrap();
        }
        for join in joins {
            join.join().unwrap()?;
        }
        Ok(())
    })?;
    assert_eq!(token.session_status()?.writer, State::Finished);
    Ok(())
}

#[test]
fn simultaneous_writer_requests_admit_exactly_one_handle() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let session = session(&directory)?;
    let barrier = std::sync::Barrier::new(9);
    std::thread::scope(|scope| -> varlit::Result<()> {
        let (send, receive) = std::sync::mpsc::channel();
        let mut releases = Vec::new();
        let mut joins = Vec::new();
        for _ in 0..8 {
            let (release, released) = std::sync::mpsc::channel();
            releases.push(release);
            let session = &session;
            let barrier = &barrier;
            let send = send.clone();
            joins.push(scope.spawn(move || -> varlit::Result<()> {
                barrier.wait();
                let writer = match session.open_writer() {
                    Ok(writer) => Some(writer),
                    Err(Error::AppendOnlyWriterActive) => None,
                    Err(error) => return Err(error),
                };
                send.send(writer.is_some()).unwrap();
                released.recv_timeout(Duration::from_secs(10)).unwrap();
                drop(writer);
                Ok(())
            }));
        }
        barrier.wait();
        let count = (0..8)
            .map(|_| receive.recv_timeout(Duration::from_secs(10)).unwrap() as usize)
            .sum::<usize>();
        assert_eq!(count, 1);
        for release in releases {
            release.send(()).unwrap();
        }
        for join in joins {
            join.join().unwrap()?;
        }
        Ok(())
    })?;
    assert_eq!(session.status()?.writer, State::Closed);
    session.open_writer()?.close()?;
    assert_eq!(session.status()?.writer_epoch, 2);
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn published_durability_failure_reaches_readers_before_retry() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let session = session(&directory)?;
    let token = session.reader_token();
    let mut reader = token.open_reader()?;
    let mut writer = session.open_writer()?;
    writer.push_info(&Value { value: 19 })?;
    varlit::VarlitFile::inject_commit_durability_failures(1);
    let sequence = match writer.commit_durable().unwrap_err() {
        Error::CommittedButDurabilityUnproven { sequence, .. } => sequence,
        error => panic!("unexpected error: {error:?}"),
    };
    let status = reader.session_status()?;
    assert_eq!(status.writer, State::Active);
    assert!(!status.recovery_required);
    assert_eq!(
        status.last_error.unwrap().committed_sequence,
        Some(sequence)
    );
    assert!(reader.follow()? > 0);
    assert_eq!(entries(&reader)?.len(), 1);
    assert_eq!(writer.commit_durable()?.sequence, sequence);
    writer.close()?;
    assert_eq!(entries(&reader)?.len(), 1);
    Ok(())
}

#[test]
fn failed_reader_construction_releases_recovery_registration() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let session = session(&directory)?;
    let mut writer = session.open_writer()?;
    writer.push_info(&Value { value: 19 })?;
    writer.commit_durable()?;
    writer.close()?;
    let token = session.reader_token();
    assert!(
        token
            .open_reader_with_resource_limits(varlit::ReadLimits::MISSING.with_max_records(0))
            .is_err()
    );
    session.recover()?;
    assert_eq!(entries(&token.open_reader()?)?.len(), 1);
    Ok(())
}

#[test]
fn close_order_and_revocation_do_not_invalidate_retained_readers() -> varlit::Result<()> {
    for drop_session_first in [false, true] {
        let directory = tempfile::tempdir()?;
        let session = session(&directory)?;
        let clone = session.clone();
        let mut writer = session.open_writer()?;
        let token = session.reader_token();
        let mut reader = token.open_reader()?;
        assert!(matches!(clone.close(), Err(Error::AppendOnlyWriterActive)));
        writer.push_info(&Value { value: 19 })?;
        writer.commit_durable()?;
        reader.follow()?;
        let old = reader.mmap_payload_window(&entries(&reader)?[0])?;
        if drop_session_first {
            drop(session);
            writer.close()?;
        } else {
            writer.close()?;
            drop(session);
        }
        clone.close()?;
        clone.close()?;
        assert!(matches!(
            clone.open_writer(),
            Err(Error::AppendOnlySessionClosed)
        ));
        assert!(matches!(
            clone.recover(),
            Err(Error::AppendOnlySessionClosed)
        ));
        drop(clone);
        drop(token);
        drop(reader);
        assert_locked(&directory.path().join("session"));
        assert_eq!(
            decode_from_slice::<Value>(old.as_bytes(), Endian::Little)?,
            Value { value: 19 }
        );
        drop(old);
        drop(spec().open(directory.path().join("session"))?);
    }
    Ok(())
}

#[test]
fn nonfatal_refusal_is_visible_and_does_not_poison_reconnect() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let file = spec()
        .with_read_limits(varlit::ReadLimits::STANDARD.with_max_records(2))
        .create(directory.path().join("session"))?;
    // SAFETY: all access uses this private session.
    let session = unsafe { AppendOnlySession::new(file)? };
    let token = session.reader_token();
    let mut reader = token.open_reader()?;
    let mut writer = session.open_writer()?;
    // The rejected block ID is a caller error; a valid block can still publish.
    #[derive(varlit::VarlitBlock)]
    #[varlit(id = 999, version = 1, kind = "fixed")]
    struct Unknown {
        value: u64,
    }
    assert!(writer.push_info(&Unknown { value: 0 }).is_err());
    let failed = reader.session_status()?;
    assert_eq!(failed.writer, State::Active);
    assert!(!failed.recovery_required);
    assert_eq!(failed.last_error.unwrap().operation, "push_info");
    writer.push_info(&Value { value: 19 })?;
    writer.commit_durable()?;
    writer.close()?;
    session.open_writer()?.close()?;
    assert!(reader.follow()? > 0);
    assert_eq!(entries(&reader)?.len(), 1);
    assert_eq!(token.session_status()?.last_error, failed.last_error);
    Ok(())
}

#[test]
fn every_read_policy_retains_protection_across_writer_handoff() -> varlit::Result<()> {
    let policies = if cfg!(feature = "integrity") {
        vec![
            varlit::ReadValidation::FULL,
            varlit::ReadValidation::CRC_ONLY,
            varlit::ReadValidation::STRUCTURE_ONLY,
            varlit::ReadValidation::TRUSTED,
        ]
    } else {
        vec![
            varlit::ReadValidation::STRUCTURE_ONLY,
            varlit::ReadValidation::TRUSTED,
        ]
    };
    for policy in policies {
        let directory = tempfile::tempdir()?;
        let session = session(&directory)?;
        let mut reader = session.reader_token().open_reader_with_resource_limits(
            varlit::ReadLimits::MISSING.with_read_validation(policy),
        )?;
        let mut writer = session.open_writer()?;
        writer.push_info(&Value { value: 19 })?;
        writer.commit_durable()?;
        writer.close()?;
        reader.follow()?;
        let old = reader.mmap_payloads()?;
        let mut writer = session.open_writer()?;
        writer.push_info(&Value { value: 23 })?;
        assert_eq!(reader.follow()?, 0);
        writer.commit_durable()?;
        writer.close()?;
        reader.follow()?;
        assert_eq!(entries(&reader)?.len(), 2);
        assert!(old.block_payload_window::<Value>(1)?.is_none());
        assert_eq!(
            decode_from_slice::<Value>(
                old.block_payload_window::<Value>(0)?.unwrap(),
                Endian::Little
            )?,
            Value { value: 19 }
        );
        assert!(matches!(
            session.recover(),
            Err(Error::AppendOnlyReadersActive)
        ));
    }
    Ok(())
}

#[test]
fn native_reader_registration_races_recovery_without_exposing_truncation() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let session = session(&directory)?;
    let mut writer = session.open_writer()?;
    writer.push_info(&Value { value: 19 })?;
    writer.commit_durable()?;
    writer.push_info(&Value { value: 23 })?;
    assert!(matches!(
        writer.close(),
        Err(Error::AppendOnlyRecoveryRequired)
    ));
    std::thread::scope(|scope| -> varlit::Result<()> {
        let mut joins = Vec::new();
        for _ in 0..8 {
            let token = session.reader_token();
            joins.push(scope.spawn(move || -> varlit::Result<()> {
                for _ in 0..16 {
                    let reader = match token.open_reader() {
                        Ok(reader) => reader,
                        Err(Error::AppendOnlySessionBusy) => {
                            std::thread::yield_now();
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let mapped = reader.mmap_payloads()?;
                    drop(reader);
                    std::thread::yield_now();
                    assert_eq!(
                        decode_from_slice::<Value>(
                            mapped.block_payload_window::<Value>(0)?.unwrap(),
                            Endian::Little
                        )?,
                        Value { value: 19 }
                    );
                    assert!(mapped.block_payload_window::<Value>(1)?.is_none());
                }
                Ok(())
            }));
        }
        for _ in 0..16 {
            match session.recover() {
                Ok(()) | Err(Error::AppendOnlyReadersActive) => {}
                Err(error) => return Err(error),
            }
            std::thread::yield_now();
        }
        for join in joins {
            join.join().unwrap()?;
        }
        Ok(())
    })?;
    session.recover()?;
    session.open_writer()?.close()?;
    assert_eq!(entries(&session.reader_token().open_reader()?)?.len(), 1);
    Ok(())
}
