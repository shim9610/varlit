//! locks implementation, kept within the original parent privacy boundary.
use super::*;

/// Opens the diagnostic `<target>.lock` marker for mutation, refusing any
/// object that is not a dedicated, unaliased regular file (F-07).
///
/// Acquiring the marker truncates it and writes pid/target metadata through
/// the handle, and dropping the lock truncates it again. Those writes must
/// never reach an object the marker path merely *points at*. Two aliasing
/// shapes are refused, on both platforms:
///
/// * a final-component symbolic link / reparse point, which would redirect the
///   writes to an arbitrary target. Unix refuses at `open` with `O_NOFOLLOW`;
///   Windows opens the reparse point itself with `FILE_FLAG_OPEN_REPARSE_POINT`
///   (so no write can reach the target) and then rejects it by attribute.
/// * a hard link, i.e. a marker object with more than one name. The extra name
///   is another path whose content this truncation would destroy.
///
/// Anything that is not a regular file (a directory, or on Unix a device or
/// FIFO) is refused for the same reason.
///
/// This is hardening of a *diagnostic* object only. Authoritative
/// single-writer exclusion is a native lock on the target file object
/// ([`probe_native_target_lock`]) and never depended on the marker, so a
/// refusal here removes no exclusion strength - it converts a silent foreign
/// write into [`Error::WriterLockMarkerNotDedicated`].
pub(super) fn open_dedicated_lock_marker(path: &Path) -> Result<File> {
    let file = open_lock_marker_without_following(path)?;
    verify_dedicated_lock_marker(path, &file)?;
    Ok(file)
}

#[cfg(unix)]
pub(super) fn open_lock_marker_without_following(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => Ok(file),
        // `O_NOFOLLOW` on a symlinked final component reports ELOOP on Linux
        // and EMLINK on some BSDs. Both mean the same thing here.
        Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP) | Some(libc::EMLINK)) => {
            Err(Error::WriterLockMarkerNotDedicated {
                path: path.display().to_string(),
                reason: "the final path component is a symbolic link",
            })
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
pub(super) fn open_lock_marker_without_following(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    // Opening the reparse point itself rather than its target guarantees that
    // no write performed through this handle can reach the target, whatever
    // `verify_dedicated_lock_marker` then decides. The flag is ignored for an
    // ordinary file and for the creating case.
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_lock_marker_without_following(path: &Path) -> Result<File> {
    // INVARIANT 5: refuse explicitly rather than silently offering weaker
    // protection on a platform where neither guarantee can be expressed.
    let _ = path;
    Err(Error::WriterLockMarkerNotDedicated {
        path: path.display().to_string(),
        reason: "this platform cannot open the marker without following links",
    })
}

#[cfg(unix)]
pub(crate) fn verify_dedicated_lock_marker(path: &Path, file: &File) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(Error::WriterLockMarkerNotDedicated {
            path: path.display().to_string(),
            reason: "the marker path does not name a regular file",
        });
    }
    if metadata.nlink() != 1 {
        return Err(Error::WriterLockMarkerNotDedicated {
            path: path.display().to_string(),
            reason: "the marker object has more than one hard link",
        });
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn verify_dedicated_lock_marker(path: &Path, file: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        GetFileInformationByHandle,
    };

    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // The handle belongs to `file`, the output points to initialized writable
    // storage, and the OS call does not outlive either value.
    let ok =
        unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, information.as_mut_ptr()) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // A successful call initializes every field of BY_HANDLE_FILE_INFORMATION.
    let information = unsafe { information.assume_init() };
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(Error::WriterLockMarkerNotDedicated {
            path: path.display().to_string(),
            reason: "the final path component is a reparse point",
        });
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(Error::WriterLockMarkerNotDedicated {
            path: path.display().to_string(),
            reason: "the marker path does not name a regular file",
        });
    }
    // `nNumberOfLinks` is supported by NTFS and ReFS; filesystems that cannot
    // report it (some network redirectors) return 1, which is the same answer
    // an unaliased file gives, so this check never becomes stricter than the
    // filesystem can substantiate.
    if information.nNumberOfLinks != 1 {
        return Err(Error::WriterLockMarkerNotDedicated {
            path: path.display().to_string(),
            reason: "the marker object has more than one hard link",
        });
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn verify_dedicated_lock_marker(path: &Path, _file: &File) -> Result<()> {
    Err(Error::WriterLockMarkerNotDedicated {
        path: path.display().to_string(),
        reason: "this platform cannot prove the marker is a dedicated regular file",
    })
}

#[derive(Debug)]
pub(crate) struct WriterLock {
    pub(super) file: File,
    /// Pathname of `file`, kept so a release can remove the marker object when
    /// truncating its content cannot be proved to have worked. See
    /// [`clear_and_verify_writer_lock_marker`].
    pub(super) marker_path: PathBuf,
    // Authoritative single-writer lock, held on the native file object itself.
    // Hard links and other path aliases all resolve to the same object, so an
    // object lock cannot be bypassed the way the path-derived ".lock" marker
    // can. The marker file above remains diagnostic metadata (pid, timestamps,
    // break-policy machinery) plus a fast same-path exclusion.
    pub(super) native_guard: Option<File>,
    /// Set by [`WriterLock::release`] so the drop that follows it does not
    /// repeat work that was already reported on.
    pub(super) released: bool,
}

// An acquisition can fail before a WriterLock exists. Explicitly unlock on
// every early return too: closing an fd does not release a flock inherited by
// another process between fork and exec.
pub(super) struct AcquiredWriterFile {
    pub(super) file: Option<File>,
    pub(super) native: bool,
}
impl AcquiredWriterFile {
    pub(super) fn new(file: File, native: bool) -> Self {
        Self {
            file: Some(file),
            native,
        }
    }
    pub(super) fn into_inner(mut self) -> File {
        self.file.take().expect("owned acquisition file")
    }
}
impl std::ops::Deref for AcquiredWriterFile {
    type Target = File;
    fn deref(&self) -> &File {
        self.file.as_ref().expect("owned acquisition file")
    }
}
impl std::ops::DerefMut for AcquiredWriterFile {
    fn deref_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("owned acquisition file")
    }
}
impl Drop for AcquiredWriterFile {
    fn drop(&mut self) {
        if let Some(file) = &self.file {
            if self.native {
                let _ = unlock_native_guard(file);
            } else {
                let _ = unlock_writer_guard(file);
            }
        }
    }
}

#[cfg(all(test, unix))]
#[test]
pub(super) fn failed_acquisition_guard_releases_a_fork_style_duplicate() -> Result<()> {
    for native in [false, true] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("guard");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        if native {
            try_lock_native_guard(&file)
                .map_err(|_| Error::WriterLockHeld(path.display().to_string()))?;
        } else {
            try_lock_writer_guard(&file)
                .map_err(|_| Error::WriterLockHeld(path.display().to_string()))?;
        }
        let admitted = AcquiredWriterFile::new(file, native);
        // dup and fork share the same open-file description on Unix. Keep
        // that description alive after the failed acquisition scope exits.
        let duplicate = admitted.try_clone()?;
        drop(admitted);
        let contender = OpenOptions::new().read(true).write(true).open(&path)?;
        assert!(try_lock_native_guard(&contender).is_ok());
        unlock_native_guard(&contender)?;
        drop(duplicate);
    }
    Ok(())
}

impl WriterLock {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        Self::acquire_with_policy(path, WriterLockBreakPolicy::Refuse)
    }

    pub(crate) fn acquire_with_policy(
        target_path: &Path,
        policy: WriterLockBreakPolicy,
    ) -> Result<Self> {
        let path = lock_path(target_path);
        // F-07: acquisition truncates and rewrites this object, so it must be
        // a dedicated, unaliased regular file before anything is mutated.
        let file = open_dedicated_lock_marker(&path)?;
        match try_lock_writer_guard(&file) {
            Ok(()) => {}
            Err(WriterGuardLockError::WouldBlock) => {
                return match policy {
                    WriterLockBreakPolicy::Refuse => {
                        Err(Error::WriterLockHeld(path.display().to_string()))
                    }
                    _ => Err(Error::WriterLockBreakRefused(path.display().to_string())),
                };
            }
            Err(WriterGuardLockError::Io(error)) => return Err(error.into()),
        }

        let mut file = AcquiredWriterFile::new(file, false);
        let marker_present = file.metadata()?.len() != 0;
        if marker_present && policy == WriterLockBreakPolicy::Refuse {
            return Err(Error::WriterLockHeld(path.display().to_string()));
        }
        if marker_present {
            let info = read_writer_lock_info_file(&path, &mut file)?
                .ok_or_else(|| Error::WriterLockMalformed(path.display().to_string()))?;
            if !should_break_writer_lock(&info, policy) {
                return Err(Error::WriterLockBreakRefused(path.display().to_string()));
            }
        }

        // An object lock held by a live writer can never be broken: the OS
        // releases it only when that writer's handles close, so a conflict here
        // always means an active writer, regardless of the marker break policy.
        let native_guard =
            probe_native_target_lock(target_path)?.map(|file| AcquiredWriterFile::new(file, true));

        let info = WriterLockInfo {
            path: path.clone(),
            target_path: absolute_target_path(target_path)?,
            process_id: std::process::id(),
            created_unix_ms: unix_time_ms(),
        };
        if let Err(error) = write_writer_lock_info(&mut file, &info) {
            let _ = clear_and_verify_writer_lock_marker(&mut file, &path);
            return Err(error);
        }
        Ok(Self {
            file: file.into_inner(),
            marker_path: path,
            native_guard: native_guard.map(AcquiredWriterFile::into_inner),
            released: false,
        })
    }

    /// Gives back both things an acquisition took, in that order, and reports a
    /// failure instead of discarding it.
    ///
    /// An acquisition takes the authoritative object lock on the target and the
    /// marker's content, and a *live* writer keeps both on purpose. Anything
    /// that acquired the lock and then failed - a refused open, a rejected
    /// header, a corrupt tail - never became a writer and must keep neither, or
    /// the next open is refused for a claim nobody holds.
    ///
    /// Two properties this has that [`Drop`] alone cannot:
    ///
    /// * it **reports**. `Drop` discarded `clear_writer_lock_info`'s error, so a
    ///   marker whose truncation failed became a silent `Error::WriterLockHeld`
    ///   on the next open, with nothing pointing at the cause.
    /// * it is **ordered**: the object lock goes first. Clearing the marker
    ///   first publishes "no writer here" while this process still holds the
    ///   object lock, so a competing writer that reads the marker inside that
    ///   window is refused by a lock that is already being given up.
    pub(crate) fn release(mut self) -> Result<()> {
        self.give_back_native_guard();
        let result = clear_and_verify_writer_lock_marker(&mut self.file, &self.marker_path);
        // Last, because clearing the marker is only safe while its own lock is
        // still held: see `clear_and_verify_writer_lock_marker`.
        let _ = unlock_writer_guard(&self.file);
        self.released = true;
        result
    }

    /// Releases the authoritative object lock, completely and now.
    ///
    /// `self.native_guard = None` was not enough: on Unix the guard is a `dup`
    /// of the writer's own handle, so dropping it leaves the lock held by the
    /// shared open file description, and any concurrently forked child holds
    /// that description too until it `exec`s. See [`unlock_writer_guard`].
    pub(super) fn give_back_native_guard(&mut self) {
        if let Some(guard) = self.native_guard.take() {
            let _ = unlock_native_guard(&guard);
        }
    }

    /// Moves the authoritative object lock onto the writer's own native handle.
    ///
    /// Writers must call this on the handle they keep open for their lifetime:
    /// once for a freshly created file (the acquire-time probe cannot lock a
    /// file that does not exist yet) and again whenever a publication rebinds
    /// the writer to a new file generation. Dropping the previous guard first
    /// is required because two exclusive range locks on the same file object
    /// conflict even within one process; a competing writer that wins the
    /// resulting microscopic window makes this call fail, which aborts the
    /// caller instead of ever admitting two writers.
    pub(crate) fn bind_native(&mut self, native: &File, target_path: &Path) -> Result<()> {
        // Unlock, not merely drop: a dropped duplicate keeps the lock alive on
        // Unix, which would make the re-lock below conflict with the guard it
        // is replacing rather than with a competing writer.
        self.give_back_native_guard();
        let guard = native.try_clone()?;
        match try_lock_native_guard(&guard) {
            Ok(()) => {
                self.native_guard = Some(guard);
                Ok(())
            }
            Err(WriterGuardLockError::WouldBlock) => {
                Err(Error::WriterLockHeld(target_path.display().to_string()))
            }
            Err(WriterGuardLockError::Io(error)) => Err(error.into()),
        }
    }
}

/// Locks the native file object behind `target_path` if the file exists.
///
/// A missing file has no aliases, so there is nothing to lock yet; creators
/// call [`WriterLock::bind_native`] on the handle they create instead.
pub(super) fn probe_native_target_lock(target_path: &Path) -> Result<Option<File>> {
    let native = match OpenOptions::new().read(true).open(target_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match try_lock_native_guard(&native) {
        Ok(()) => Ok(Some(native)),
        Err(WriterGuardLockError::WouldBlock) => {
            Err(Error::WriterLockHeld(target_path.display().to_string()))
        }
        Err(WriterGuardLockError::Io(error)) => Err(error.into()),
    }
}

pub(crate) enum WriterGuardLockError {
    WouldBlock,
    Io(std::io::Error),
}

#[cfg(not(windows))]
pub(super) fn try_lock_writer_guard(file: &File) -> std::result::Result<(), WriterGuardLockError> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(WriterGuardLockError::WouldBlock),
        Err(std::fs::TryLockError::Error(error)) => Err(WriterGuardLockError::Io(error)),
    }
}

#[cfg(windows)]
pub(super) fn try_lock_writer_guard(file: &File) -> std::result::Result<(), WriterGuardLockError> {
    try_lock_exclusive_range(file, WRITER_LOCK_MAX_LEN)
}

// Advisory whole-file lock on Unix; both native and marker guards share it.
#[cfg(not(windows))]
pub(crate) fn try_lock_native_guard(file: &File) -> std::result::Result<(), WriterGuardLockError> {
    try_lock_writer_guard(file)
}

// Windows range locks are mandatory, so the native guard must live at an
// offset no real data access can ever overlap. Record extents are bounded by
// the file length, which can never reach this reserved offset, so ordinary
// readers and the writer's own data I/O are unaffected.
#[cfg(windows)]
pub(super) const NATIVE_WRITER_GUARD_OFFSET: u64 = u64::MAX - 1;

#[cfg(windows)]
pub(crate) fn try_lock_native_guard(file: &File) -> std::result::Result<(), WriterGuardLockError> {
    try_lock_exclusive_range(file, NATIVE_WRITER_GUARD_OFFSET)
}

/// Gives a writer guard back *explicitly*, instead of letting the handle's close
/// do it.
///
/// Closing is not equivalent on Unix. `flock` binds the lock to the open file
/// description, not to the descriptor, and a description outlives the
/// descriptor that created it whenever any duplicate remains open - including
/// duplicates this process cannot see or reach:
///
/// * `File::try_clone` is `dup`, so the native guard shares one description
///   with the writer's own data handle. Dropping the guard alone releases
///   nothing while that handle lives.
/// * a `fork` anywhere in the process (every `std::process::Command::spawn` is
///   one) duplicates every open descriptor into the child. The child's copies
///   are `O_CLOEXEC` and so close at `exec`, but until they do, the child holds
///   descriptions this process has already dropped - and the lock on them.
///   Releasing by close therefore does not take effect when the release
///   happens, but at some unrelated later instant in another process. That is
///   how a released writer lock came back as [`Error::WriterLockHeld`] for a
///   claim nobody held: `flock` reported `EAGAIN` with no lock owner anywhere
///   in this process.
///
/// `LOCK_UN` releases the description's lock for every descriptor that refers
/// to it, in this process and in any pre-`exec` child, so it is the only
/// release that is complete at the moment it returns. Windows range locks are
/// per-handle and are dropped at close, but unlocking the exact range first is
/// the same statement made promptly, and `ERROR_NOT_LOCKED` on an
/// already-released range is ignorable either way.
#[cfg(not(windows))]
pub(super) fn unlock_writer_guard(file: &File) -> std::io::Result<()> {
    file.unlock()
}

#[cfg(windows)]
pub(super) fn unlock_writer_guard(file: &File) -> std::io::Result<()> {
    unlock_exclusive_range(file, WRITER_LOCK_MAX_LEN)
}

#[cfg(not(windows))]
pub(crate) fn unlock_native_guard(file: &File) -> std::io::Result<()> {
    file.unlock()
}

#[cfg(windows)]
pub(crate) fn unlock_native_guard(file: &File) -> std::io::Result<()> {
    unlock_exclusive_range(file, NATIVE_WRITER_GUARD_OFFSET)
}

#[cfg(windows)]
pub(super) fn unlock_exclusive_range(file: &File, offset: u64) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0_0};

    let mut overlapped = OVERLAPPED::default();
    overlapped.Anonymous.Anonymous = OVERLAPPED_0_0 {
        Offset: offset as u32,
        OffsetHigh: (offset >> 32) as u32,
    };
    // SAFETY: mirrors `try_lock_exclusive_range` - `file` is open for the
    // duration, the OVERLAPPED value is initialized for the same synchronous
    // one-byte range, and no pointer outlives the call.
    let unlocked =
        unsafe { UnlockFileEx(file.as_raw_handle() as HANDLE, 0, 1, 0, &mut overlapped) };
    if unlocked != 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

#[cfg(windows)]
pub(super) fn try_lock_exclusive_range(
    file: &File,
    offset: u64,
) -> std::result::Result<(), WriterGuardLockError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0_0};

    let mut overlapped = OVERLAPPED::default();
    overlapped.Anonymous.Anonymous = OVERLAPPED_0_0 {
        Offset: offset as u32,
        OffsetHigh: (offset >> 32) as u32,
    };
    // SAFETY: `file` remains open for the lifetime of the acquired lock, the
    // OVERLAPPED value is initialized for a synchronous one-byte range lock,
    // and the pointer is valid for the duration of this call.
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if locked != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Err(WriterGuardLockError::WouldBlock)
    } else {
        Err(WriterGuardLockError::Io(error))
    }
}

/// Clears stale writer-lock metadata without opening or scanning the target file.
///
/// The policy is always explicit. `Refuse` preserves the default single-writer
/// behavior, while process-aware policies only break a lock after their
/// predicate succeeds. An OS-level exclusive lock is acquired before metadata
/// is inspected or changed, so an active or concurrently recovering writer
/// cannot be displaced.
pub fn clear_stale_writer_lock(
    target_path: impl AsRef<Path>,
    policy: WriterLockBreakPolicy,
) -> Result<()> {
    // `release` is the reporting route: it clears the marker, proves it clear,
    // and drops the object lock first, so this cannot return `Ok(())` over a
    // marker that is still refusing.
    WriterLock::acquire_with_policy(target_path.as_ref(), policy)?.release()
}

/// Releases a writer lock that an open acquired and then could not use, and
/// returns the error to report.
///
/// An open that fails never became a writer, so the claim it took has to be
/// given back before the error leaves the function - not at some later scope
/// exit, and not silently. The original failure is what the caller asked about
/// and is what propagates; a release failure replaces it, because a stranded
/// claim is the more serious news and is the thing that would otherwise resurface
/// as `Error::WriterLockHeld` on a file nobody holds.
/// Runs `body` while holding `lock`, and gives the claim back explicitly if it
/// fails.
///
/// `WriterLock` releases on drop, so the OS-level guard comes back either way.
/// What drop cannot do is *report*, and the marker is the half that matters
/// here: `Drop` clears its contents on a best-effort basis and discards the
/// result, so a clear that failed left a non-empty marker behind and the next
/// acquisition refused with `WriterLockHeld` under the default
/// [`WriterLockBreakPolicy::Refuse`] — a live writer and a failed open became
/// indistinguishable. That is what made a failed open poison the pathname.
///
/// On success the lock is handed to the caller inside the value it built, whose
/// own drop owns it from then on. On failure it is released here, and a release
/// error replaces the original: a caller that is told the open failed can retry,
/// but a caller told only the *first* reason while the pathname is still claimed
/// would retry forever.
/// `body` builds the file while borrowing the lock and leaves `_lock` empty;
/// this installs the lock on success, so the claim lives exactly as long as the
/// handle. Deliberately not generic and deliberately not `mem::forget`: the
/// borrow means `body` cannot take ownership, and forgetting the lock here would
/// hold the OS guard and the marker for the life of the process.
pub(super) fn with_writer_lock(
    lock: WriterLock,
    body: impl FnOnce(&mut WriterLock) -> Result<StratavaFile>,
) -> Result<StratavaFile> {
    let (mut file, lock) = with_writer_lock_value(lock, body)?;
    debug_assert!(
        file._lock.is_none(),
        "the body under `with_writer_lock` must leave `_lock` empty; \
         installing it twice would drop the first claim early"
    );
    file._lock = Some(lock);
    Ok(file)
}

/// The same lifecycle for anything an acquisition builds that is not a
/// [`StratavaFile`].
///
/// [`with_writer_lock`] can install the claim itself because it knows the one
/// field to put it in. The layout writer and the recovering open build other
/// shapes - a different struct, a `(file, report)` pair - so they get the lock
/// handed back on success and install it themselves. What matters is identical:
/// `body` only *borrows* the lock, so it cannot take ownership and cannot leak
/// it, and a failure releases here with a report rather than at scope exit in
/// silence.
pub(crate) fn with_writer_lock_value<T>(
    mut lock: WriterLock,
    body: impl FnOnce(&mut WriterLock) -> Result<T>,
) -> Result<(T, WriterLock)> {
    match body(&mut lock) {
        Ok(value) => Ok((value, lock)),
        Err(error) => Err(release_writer_lock_after_failure(lock, error)),
    }
}

pub(super) fn release_writer_lock_after_failure(lock: WriterLock, error: Error) -> Error {
    match lock.release() {
        Ok(()) => error,
        Err(release_error) => release_error,
    }
}

pub(super) fn write_writer_lock_info(file: &mut File, info: &WriterLockInfo) -> Result<()> {
    let target = info.target_path.to_string_lossy();
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(
        format!(
            "{WRITER_LOCK_MAGIC}\npid={}\ncreated_unix_ms={}\ntarget={target}\n",
            info.process_id, info.created_unix_ms
        )
        .as_bytes(),
    )?;
    file.flush()?;
    Ok(())
}

pub(super) fn clear_writer_lock_info(file: &mut File) -> Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.flush()?;
    Ok(())
}

/// Clears the marker's content and *proves* it is gone, removing the marker
/// object if it is not.
///
/// The next acquisition decides from one observation only -
/// `file.metadata()?.len() != 0` in [`WriterLock::acquire_with_policy`] - and
/// under the default `WriterLockBreakPolicy::Refuse` a non-zero length is
/// `Error::WriterLockHeld` without the content ever being read. So a release
/// whose truncation silently did not take does not merely lose a diagnostic: it
/// leaves the file unopenable by the default policy until someone runs
/// `clear_stale_writer_lock`. Checking the length afterwards is what makes the
/// truncation's success a fact rather than an assumption.
///
/// When the length is still non-zero the marker object is removed instead.
/// That is safe precisely here and only here: the caller still holds the
/// marker's own OS lock, so no acquisition can be part-way through reading it,
/// and a caller that is releasing has no writer role left for the marker to
/// describe. Removal is a fallback, not the normal path - a marker file that
/// truncates correctly is left in place, zero length, exactly as before - so
/// nothing that depends on the marker surviving normal use changes.
pub(super) fn clear_and_verify_writer_lock_marker(file: &mut File, path: &Path) -> Result<()> {
    let cleared = clear_writer_lock_info(file);
    // `u64::MAX` for an unreadable length: unprovable is treated as non-empty,
    // never as empty.
    let observed = file
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or(u64::MAX);
    if observed == 0 {
        return Ok(());
    }
    remove_file(path)?;
    // The removal made the marker inert, which is what the next acquisition
    // reads; report the truncation failure only if it did not already succeed.
    cleared
}

pub(super) fn read_writer_lock_info(target_path: &Path) -> Result<Option<WriterLockInfo>> {
    let path = lock_path(target_path);
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Error::WriterLockMalformed(path.display().to_string())),
    };
    read_writer_lock_info_file(&path, &mut file)
}

pub(super) fn read_writer_lock_info_file(
    path: &Path,
    file: &mut File,
) -> Result<Option<WriterLockInfo>> {
    let len = file
        .metadata()
        .map_err(|_| Error::WriterLockMalformed(path.display().to_string()))?
        .len();
    if len == 0 {
        return Ok(None);
    }
    if len > WRITER_LOCK_MAX_LEN {
        return Err(Error::WriterLockMalformed(path.display().to_string()));
    }
    let capacity =
        usize::try_from(len).map_err(|_| Error::WriterLockMalformed(path.display().to_string()))?;
    let mut contents = String::new();
    contents
        .try_reserve_exact(capacity)
        .map_err(|_| Error::AllocationFailed {
            resource: "writer lock metadata",
            requested: len,
        })?;
    file.seek(SeekFrom::Start(0))
        .map_err(|_| Error::WriterLockMalformed(path.display().to_string()))?;
    file.take(WRITER_LOCK_MAX_LEN.saturating_add(1))
        .read_to_string(&mut contents)
        .map_err(|_| Error::WriterLockMalformed(path.display().to_string()))?;
    if contents.len() as u64 > WRITER_LOCK_MAX_LEN {
        return Err(Error::WriterLockMalformed(path.display().to_string()));
    }
    parse_writer_lock_info(path, &contents).map(Some)
}

pub(super) fn parse_writer_lock_info(path: &Path, contents: &str) -> Result<WriterLockInfo> {
    let mut lines = contents.lines();
    if lines.next() != Some(WRITER_LOCK_MAGIC) {
        return Err(Error::WriterLockMalformed(path.display().to_string()));
    }
    let mut process_id = None;
    let mut created_unix_ms = None;
    let mut target_path = None;
    for line in lines {
        let Some((key, value)) = line.split_once('=') else {
            return Err(Error::WriterLockMalformed(path.display().to_string()));
        };
        match key {
            "pid" => {
                process_id = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| Error::WriterLockMalformed(path.display().to_string()))?,
                );
            }
            "created_unix_ms" => {
                created_unix_ms = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| Error::WriterLockMalformed(path.display().to_string()))?,
                );
            }
            "target" => {
                if value.is_empty() {
                    return Err(Error::WriterLockMalformed(path.display().to_string()));
                }
                target_path = Some(PathBuf::from(value));
            }
            _ => return Err(Error::WriterLockMalformed(path.display().to_string())),
        }
    }
    Ok(WriterLockInfo {
        path: path.to_path_buf(),
        target_path: target_path
            .ok_or_else(|| Error::WriterLockMalformed(path.display().to_string()))?,
        process_id: process_id
            .ok_or_else(|| Error::WriterLockMalformed(path.display().to_string()))?,
        created_unix_ms: created_unix_ms
            .ok_or_else(|| Error::WriterLockMalformed(path.display().to_string()))?,
    })
}

pub(super) fn should_break_writer_lock(
    info: &WriterLockInfo,
    policy: WriterLockBreakPolicy,
) -> bool {
    match policy {
        WriterLockBreakPolicy::Refuse => false,
        WriterLockBreakPolicy::BreakIfProcessAbsent => process_is_absent(info.process_id),
        WriterLockBreakPolicy::BreakIfOlderThan(age) => lock_is_older_than(info, age),
        WriterLockBreakPolicy::BreakIfProcessAbsentAndOlderThan(age) => {
            process_is_absent(info.process_id) && lock_is_older_than(info, age)
        }
    }
}

pub(super) fn lock_is_older_than(info: &WriterLockInfo, age: Duration) -> bool {
    let now = unix_time_ms();
    now.saturating_sub(info.created_unix_ms) as u128 >= age.as_millis()
}

pub(super) fn unix_time_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

pub(super) fn absolute_target_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(windows)]
pub(super) fn process_is_absent(process_id: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    if process_id == std::process::id() {
        return false;
    }
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
            0,
            process_id,
        )
    };
    if handle.is_null() {
        return std::io::Error::last_os_error().raw_os_error()
            == Some(ERROR_INVALID_PARAMETER as i32);
    }
    // A terminated Windows process remains openable while any process still
    // holds a handle to its signaled kernel object. Query the wait state instead
    // of treating every successful OpenProcess call as a live writer.
    let wait = unsafe { WaitForSingleObject(handle, 0) };
    unsafe {
        CloseHandle(handle);
    }
    match wait {
        WAIT_OBJECT_0 => true,
        WAIT_TIMEOUT => false,
        _ => false,
    }
}

#[cfg(unix)]
pub(super) fn process_is_absent(process_id: u32) -> bool {
    if process_id == std::process::id() {
        return false;
    }
    let Ok(process_id) = libc::pid_t::try_from(process_id) else {
        return false;
    };
    if process_id <= 0 {
        return false;
    }

    // SAFETY: signal 0 performs an existence/permission check only. Positive
    // PIDs avoid process-group semantics, and no pointer crosses the FFI.
    if unsafe { libc::kill(process_id, 0) } == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn process_is_absent(process_id: u32) -> bool {
    let _ = process_id;
    false
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        // Same order and same guarantee as `WriterLock::release`, which is the
        // reporting route; this is the route a live writer's ordinary close
        // takes, and the backstop for any path that still relies on scope exit.
        // `Drop` cannot report, so the marker's emptiness is made *unconditional*
        // here rather than hoped for: see `clear_and_verify_writer_lock_marker`.
        // Both locks are given back by unlocking, never by letting the handle
        // close - a closing handle releases nothing that a duplicate still
        // holds, and `fork` makes duplicates this process cannot see. See
        // `unlock_writer_guard`.
        self.give_back_native_guard();
        let _ = clear_and_verify_writer_lock_marker(&mut self.file, &self.marker_path);
        let _ = unlock_writer_guard(&self.file);
    }
}

pub(super) fn lock_path(path: &Path) -> PathBuf {
    let mut lock_name: OsString = path.as_os_str().to_os_string();
    lock_name.push(".lock");
    PathBuf::from(lock_name)
}
