//! Native rollback differences must not weaken captured-byte or poison checks.
use super::tests::{Number, spec};
use super::*;

pub(super) fn assert_rollback_failure(error: &Error, injected: bool) {
    let Error::WriteRollbackFailed { operation, source } = error else {
        panic!("expected typed append rollback failure, got {error:?}");
    };
    assert_eq!(*operation, "append record");
    assert!(injected || cfg!(windows));
    if !injected {
        // Windows ERROR_USER_MAPPED_FILE: SetEndOfFile requires every mapped
        // view to be released, even when shrinking only an unpublished tail.
        assert_eq!(source.raw_os_error(), Some(1224), "{error:?}");
    }
}

#[test]
fn append_rollback_with_only_reader_leases_recovers_and_retries() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let file = spec().create(directory.path().join("unmapped"))?;
    // SAFETY: only the facade writes this private backing object.
    let mut writer = unsafe { AppendOnlyWriter::new(file)? };
    writer.push_info(&Number(19))?;
    writer.commit_durable()?;
    let mut reader = writer.reader_token().open_reader()?;
    inject_write_fault(WriteFault::AppendAfterHeader);
    let error = writer.push_info(&Number(23)).unwrap_err();
    assert!(matches!(error, Error::Io(_)), "{error:?}");
    assert_eq!(reader.follow()?, 0);
    writer.push_info(&Number(29))?;
    writer.commit_durable()?;
    assert!(reader.follow()? > 0);
    let mapped = reader.mmap_payloads()?;
    assert_eq!(
        crate::decode_from_slice::<Number>(
            mapped.block_payload_window::<Number>(1)?.unwrap(),
            Endian::Little
        )?,
        Number(29)
    );
    assert!(mapped.block_payload_window::<Number>(2)?.is_none());
    Ok(())
}

#[test]
fn failed_mapped_rollback_recovers_only_after_protection_is_released() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("recovery");
    let file = spec().create(&path)?;
    // SAFETY: the private object's only writer is this facade.
    let mut writer = unsafe { AppendOnlyWriter::new(file)? };
    writer.push_info(&Number(19))?;
    writer.commit_durable()?;
    let token = writer.reader_token();
    let reader = token.open_reader()?;
    let mut entries = Vec::new();
    reader.block_entries_into::<Number>(&mut entries)?;
    let mapped = reader.mmap_payload_window(&entries[0])?;
    // Windows supplies the real mapped-file truncate refusal. Elsewhere inject
    // that rollback failure to exercise the same recovery/ownership sequence.
    let injected = !cfg!(windows);
    inject_write_fault(if injected {
        WriteFault::AppendAfterHeaderWithRollbackFailure
    } else {
        WriteFault::AppendAfterHeader
    });
    let error = writer.push_info(&Number(23)).unwrap_err();
    assert_rollback_failure(&error, injected);
    assert_eq!(
        crate::decode_from_slice::<Number>(mapped.as_bytes(), Endian::Little)?,
        Number(19)
    );
    assert!(matches!(
        writer.push_info(&Number(31)),
        Err(Error::WriterPoisoned(_))
    ));
    drop(mapped);
    // Unmapping makes the OS operation possible; it must not clear poison.
    assert!(matches!(writer.commit(), Err(Error::WriterPoisoned(_))));
    drop(writer);
    drop(reader);
    assert!(matches!(spec().open(&path), Err(Error::WriterLockHeld(_))));
    drop(token);
    let recovered = spec().open(&path)?;
    assert_eq!(recovered.blocks::<Number>()?.get(0)?, Some(Number(19)));
    assert_eq!(recovered.blocks::<Number>()?.get(1)?, None);
    // SAFETY: all previous protections ended; exclusive private ownership resumes.
    let mut writer = unsafe { AppendOnlyWriter::new(recovered)? };
    writer.push_info(&Number(29))?;
    writer.commit_durable()?;
    let reader = writer.reader_token().open_reader()?;
    let mapped = reader.mmap_payloads()?;
    assert_eq!(
        crate::decode_from_slice::<Number>(
            mapped.block_payload_window::<Number>(1)?.unwrap(),
            Endian::Little
        )?,
        Number(29)
    );
    assert!(mapped.block_payload_window::<Number>(2)?.is_none());
    Ok(())
}
