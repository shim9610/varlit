//! publication implementation, kept within the original parent privacy boundary.
use super::*;

/// Folds the native matrix file's OS-object identity and schema hash into a
/// 32-byte fingerprint.
///
/// Mirrors the scalable sidecar identity machinery
/// (`stream::primary_identity` / `opened_file_identity`). Matrix companions
/// retain their own fingerprint encoding. Uses the CRC helper so it honors the same
/// `integrity` gate the rest of the sidecar envelope requires.
pub(super) fn native_object_fingerprint(spec: FormatSpec, file: &File) -> Result<[u8; 32]> {
    let object_identity = opened_file_identity(file)?;
    let schema_hash = if spec.schema_hash == 0 {
        spec.computed_schema_hash()
    } else {
        spec.schema_hash
    };
    let mut fingerprint = [0u8; 32];
    for lane in 0..8u32 {
        let mut buffer = Vec::with_capacity(4 + 8 + object_identity.len());
        buffer.extend_from_slice(&lane.to_le_bytes());
        buffer.extend_from_slice(&schema_hash.to_le_bytes());
        buffer.extend_from_slice(&object_identity);
        let value = crc32_bytes(&buffer)?;
        fingerprint[(lane as usize) * 4..(lane as usize + 1) * 4]
            .copy_from_slice(&value.to_le_bytes());
    }
    Ok(fingerprint)
}

#[cfg(unix)]
pub(crate) fn opened_file_identity(file: &File) -> Result<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&metadata.dev().to_le_bytes());
    bytes.extend_from_slice(&metadata.ino().to_le_bytes());
    Ok(bytes)
}

#[cfg(windows)]
pub(crate) fn opened_file_identity(file: &File) -> Result<Vec<u8>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
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
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    let mut bytes = Vec::with_capacity(12);
    bytes.extend_from_slice(&information.dwVolumeSerialNumber.to_le_bytes());
    bytes.extend_from_slice(&file_index.to_le_bytes());
    Ok(bytes)
}

/// A file object held open, together with the OS identity read from that open
/// handle (F-05 follow-up).
///
/// [`opened_file_identity`] is `(st_dev, st_ino)` on Unix and
/// `(volume serial, file index)` on Windows. Neither is a durable name for a
/// file *object*: an inode number is a slot in an allocator, and a filesystem
/// hands a just-freed slot straight back to the next create. Identity bytes
/// copied out of a handle that is then closed therefore say nothing about a
/// later object bearing the same bytes — which is exactly the comparison every
/// identity-checked deletion in this crate was making. Measured on Linux
/// (overlayfs over ext4): unlinking a file and immediately creating another at
/// the same name reproduces the *same* `(dev, ino)`, so the check said "same
/// object" about a file it had never seen.
///
/// An open descriptor pins its inode: the number cannot be handed to another
/// object while any handle on it lives. Holding the handle for as long as the
/// identity is used therefore restores the property the comparison assumes.
/// The invariant this type exists to make structural is:
///
/// > identity bytes are only ever compared while a handle on the object they
/// > were read from is still open.
///
/// The handle is opened read-only and never lent out, so a pin can neither
/// write the object it holds nor be turned into something that can.
#[derive(Debug)]
pub(crate) struct PinnedObject {
    /// Never read through; held open purely so the identity below stays a name
    /// for this object. `_` because that is the whole contract.
    pub(super) _handle: File,
    pub(super) identity: Vec<u8>,
}

impl PinnedObject {
    /// Opens `path` read-only and keeps the handle, so the identity read from
    /// it names that object for as long as the returned value lives.
    ///
    /// This is the constructor for callers whose claim is "the object I am
    /// about to delete is the one I just opened at this name" — the lock
    /// marker and the stale sidecar. It cannot prove that the object at the
    /// name is the one the caller created earlier; use [`Self::open_verified`]
    /// when there is an earlier handle to check against.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let handle = open_pin_handle(path)?;
        let identity = opened_file_identity(&handle)?;
        Ok(Self {
            _handle: handle,
            identity,
        })
    }

    /// As [`Self::open`], but refuses unless the object opened is still the one
    /// `expected` names.
    ///
    /// Sound *only* while the caller holds the object `expected` was read from
    /// open — which pins it, so equal identity bytes prove the same object
    /// rather than a recycled slot. The self-test calls this with its writer
    /// still alive, which is what closes the window between creating the
    /// artifact and pinning it.
    pub(crate) fn open_verified(path: &Path, expected: &[u8]) -> Result<Self> {
        let pinned = Self::open(path)?;
        if pinned.identity() != expected {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the pathname stopped naming the expected file object",
            )));
        }
        Ok(pinned)
    }

    /// The identity of the pinned object. Valid as a name for that object only
    /// while `self` lives, which is why it borrows from `self`.
    pub(crate) fn identity(&self) -> &[u8] {
        &self.identity
    }
}

/// Opens the handle a [`PinnedObject`] holds: read-only, and on Unix without
/// following a symlink at the final component, so a pin can never end up
/// holding a file the name merely points at.
#[cfg(unix)]
pub(super) fn open_pin_handle(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?)
}

/// Windows counterpart. Deliberately *does* follow a reparse point, because the
/// Windows removal arm resolves the same pathname the same way: pinning the
/// link while deleting the target would compare two different objects.
///
/// The default `OpenOptions` share mode is read/write/delete, which is what
/// keeps the pin compatible with the removal that follows it: the removal opens
/// the same object for `DELETE`, which an existing handle only permits if that
/// handle shares deletion, and it opens with `FILE_SHARE_READ`, which only
/// admits existing handles whose granted access is read. A writable pin would
/// fail the second test and turn every cleanup into a sharing violation.
#[cfg(windows)]
pub(super) fn open_pin_handle(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().read(true).open(path)?)
}

/// RAII owner of a rewrite/publication temp file (STO4-P2).
///
/// [`create_rewrite_temp_file`] hands back a bare `(PathBuf, File)`, so every
/// fallible step between creation and the caller's first cleanup scope is a
/// path that can leak an unpublished temp. This owns the temp from creation
/// instead: any early return - including one from a `?` the author did not
/// think about - unlinks it on drop.
///
/// Cleanup is *given up*, never assumed, through [`RewriteTemp::retain`]:
/// after a successful rename the pathname is already consumed, and after
/// [`Error::ReplacePublicationIndeterminate`] the temp may be the only
/// surviving copy of the new generation and must be preserved for out-of-band
/// reconciliation (DUR2-01). [`RewriteTemp::cleanup_now`] deletes eagerly for
/// callers whose follow-up work (lock-marker removal) is only correct once the
/// temp is gone.
pub(super) struct RewriteTemp {
    pub(super) path: PathBuf,
    /// Closed by [`RewriteTemp::close`] before publication; Windows cannot
    /// rename or delete a file through a live handle.
    pub(super) file: Option<File>,
    pub(super) armed: bool,
}

impl RewriteTemp {
    pub(super) fn create(target: &Path) -> Result<Self> {
        let (path, file) = create_rewrite_temp_file(target)?;
        Ok(Self {
            path,
            file: Some(file),
            armed: true,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// The open handle. Panics only if called after [`RewriteTemp::close`],
    /// which is a caller bug rather than a runtime condition.
    pub(super) fn file(&self) -> &File {
        self.file
            .as_ref()
            .expect("rewrite temp handle used after close")
    }

    /// Releases the handle while keeping the file and the cleanup obligation.
    pub(super) fn close(&mut self) {
        self.file = None;
    }

    /// Closes and deletes the temp now, and gives up the obligation.
    pub(super) fn cleanup_now(&mut self) {
        self.file = None;
        if self.armed {
            self.armed = false;
            let _ = remove_file(&self.path);
        }
    }

    /// Gives up the cleanup obligation: the temp was published, or is
    /// deliberately preserved.
    pub(super) fn retain(&mut self) {
        self.armed = false;
    }
}

impl Drop for RewriteTemp {
    fn drop(&mut self) {
        self.file = None;
        if self.armed {
            let _ = remove_file(&self.path);
        }
    }
}

#[cfg(feature = "scalable-fault-injection")]
std::thread_local! {
    /// Armed post-creation rewrite-temp preparation failures (STO4-P2).
    pub(super) static INJECTED_REWRITE_TEMP_PREPARATION_FAILURES: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Consumes one armed rewrite-temp preparation failure (STO4-P2).
///
/// Models the permission copy that sits between the temp's creation and the
/// publication scope: a real `set_permissions` error cannot be produced on
/// demand on every host, and it was exactly the step whose `?` used to leave an
/// empty temp behind. Inert without the `scalable-fault-injection` feature.
#[inline]
pub(super) fn take_injected_rewrite_temp_preparation_failure() -> Result<()> {
    #[cfg(feature = "scalable-fault-injection")]
    {
        let armed = INJECTED_REWRITE_TEMP_PREPARATION_FAILURES.with(|count| {
            let current = count.get();
            if current != 0 {
                count.set(current - 1);
            }
            current != 0
        });
        if armed {
            return Err(std::io::Error::other("injected rewrite temp preparation failure").into());
        }
    }
    Ok(())
}

pub(crate) fn create_rewrite_temp_file(path: &Path) -> Result<(PathBuf, File)> {
    let base_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| OsString::from("varlit"));
    for attempt in 0..1000u32 {
        let mut file_name = OsString::from(".");
        file_name.push(&base_name);
        file_name.push(format!(".rewrite.{}.{}.tmp", std::process::id(), attempt));
        let temp_path = path
            .parent()
            .map(|parent| parent.join(&file_name))
            .unwrap_or_else(|| PathBuf::from(&file_name));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not create a unique varlit rewrite temp file",
    )
    .into())
}

/// Classification of a failed Windows `ReplaceFileW` call (DUR2-01).
///
/// Derived from the documented contract
/// (<https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew>):
/// `ERROR_UNABLE_TO_REMOVE_REPLACED` (1175) leaves the replaced file intact
/// under its original name, `ERROR_UNABLE_TO_MOVE_REPLACEMENT` (1176) and
/// `ERROR_UNABLE_TO_MOVE_REPLACEMENT_2` (1177) can leave the two files'
/// names, streams and attributes partially moved (the target pathname may
/// already have changed), and every other error occurs before any mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplacePublicationFailure {
    /// `ERROR_UNABLE_TO_REMOVE_REPLACED` (1175): the replaced file is intact
    /// under its original name; nothing was mutated.
    ReplacedFileIntact,
    /// Any other documented pre-publication error: both files retain their
    /// original names.
    PrePublication,
    /// 1176/1177: the pathname state is indeterminate. The replacement temp
    /// file must be preserved and a writer bound to the target must not
    /// blindly retry the publication.
    IndeterminatePublication,
}

pub(super) const ERROR_UNABLE_TO_REMOVE_REPLACED: i32 = 1175;
pub(super) const ERROR_UNABLE_TO_MOVE_REPLACEMENT: i32 = 1176;
pub(super) const ERROR_UNABLE_TO_MOVE_REPLACEMENT_2: i32 = 1177;

/// Classifies a raw `ReplaceFileW` OS error per the Microsoft contract.
///
/// Pure and platform-independent so the classification is unit-testable on
/// every host; only the Windows publication path feeds it live errors.
pub fn classify_replace_publication_error(raw_os_error: i32) -> ReplacePublicationFailure {
    match raw_os_error {
        ERROR_UNABLE_TO_REMOVE_REPLACED => ReplacePublicationFailure::ReplacedFileIntact,
        ERROR_UNABLE_TO_MOVE_REPLACEMENT | ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 => {
            ReplacePublicationFailure::IndeterminatePublication
        }
        _ => ReplacePublicationFailure::PrePublication,
    }
}

/// Outcome of a pathname publication whose rename step succeeded.
///
/// `Err` from [`replace_path_atomically`] means the publication itself failed.
/// With one exception the target pathname still resolves to the previous
/// generation and the caller may delete its temp file; the exception is
/// [`Error::ReplacePublicationIndeterminate`], after which the pathname state
/// is unknown, the replacement temp file must be preserved for reconciliation
/// and any writer bound to the target must be poisoned instead of retrying
/// (DUR2-01). Once the rename has happened the target is already the new
/// generation, so a later durability failure must not be reported as a plain
/// error: callers must keep operating on the published generation (rebind or
/// poison the writer) and surface [`Error::PublishedButParentSyncPending`]
/// instead.
#[derive(Debug)]
pub(crate) enum ReplaceDurability {
    /// The replacement is visible at the target path and the parent-directory
    /// entry was synced.
    Durable,
    /// The replacement is visible at the target path, but the parent-directory
    /// sync failed, so the rename is not yet guaranteed to survive power loss.
    ParentSyncPending(Error),
}

/// Publishes a [`tempfile::TempPath`]-guarded replacement at `target` via
/// [`replace_path_atomically`], taking ownership of the RAII guard so its
/// pathname deletion can be disarmed when the publication outcome is
/// indeterminate.
///
/// After [`Error::ReplacePublicationIndeterminate`] the replacement temp may
/// be the only surviving copy of the new generation and must be preserved for
/// out-of-band reconciliation, so the guard is disarmed before the error
/// propagates (DUR2-01). Every other `Err` is a documented pre-publication
/// failure ??the target pathname is untouched ??so dropping the guard deletes
/// the unpublished temp as usual; on `Ok` the rename already consumed the temp
/// pathname and the guard's drop is a no-op.
pub(crate) fn publish_temp_path_atomically(
    temporary: tempfile::TempPath,
    target: &Path,
) -> Result<ReplaceDurability> {
    publish_temp_path_with(temporary, target, replace_path_atomically)
}

pub(super) fn publish_temp_path_with(
    temporary: tempfile::TempPath,
    target: &Path,
    replace: fn(&Path, &Path) -> Result<ReplaceDurability>,
) -> Result<ReplaceDurability> {
    match replace(&temporary, target) {
        Err(error) => {
            if matches!(error, Error::ReplacePublicationIndeterminate { .. }) {
                // `keep` only forgets the guard; if it ever reports an error
                // it hands the guard back, so forget it manually rather than
                // letting the drop delete the replacement we must preserve.
                if let Err(persist_error) = temporary.keep() {
                    std::mem::forget(persist_error.path);
                }
            }
            Err(error)
        }
        published => published,
    }
}

/// Publishes an immutable companion generation with pinned old/new handles.
/// Windows needs a POSIX rename: `ReplaceFileW` exclusively opens the new file,
/// while ordinary `MoveFileExW` refuses an open target even with delete sharing.
/// Ordinary rewrites retain `ReplaceFileW` and its attribute/ACL inheritance.
pub(crate) fn publish_open_temp_path_atomically(
    temporary: tempfile::TempPath,
    target: &Path,
) -> Result<ReplaceDurability> {
    #[cfg(not(windows))]
    {
        publish_temp_path_atomically(temporary, target)
    }
    #[cfg(windows)]
    {
        publish_temp_path_with(temporary, target, replace_open_path_atomically)
    }
}

#[cfg(windows)]
pub(super) fn replace_open_path_atomically(
    replacement: &Path,
    target: &Path,
) -> Result<ReplaceDurability> {
    use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_NORMAL, FILE_RENAME_INFO, FileRenameInfoEx, SetFileAttributesW,
        SetFileInformationByHandle,
    };
    use windows_sys::Win32::System::WindowsProgramming::{
        FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
    };

    #[cfg(feature = "scalable-fault-injection")]
    take_injected_replace_indeterminate(replacement, target)?;

    let file = OpenOptions::new().access_mode(DELETE).open(replacement)?;
    let name: Vec<u16> = std::path::absolute(target)?
        .as_os_str()
        .encode_wide()
        .collect();
    let name_bytes = name.len().checked_mul(2).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement path too long",
        )
    })?;
    let name_bytes = u32::try_from(name_bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement path too long",
        )
    })?;
    let size = (std::mem::offset_of!(FILE_RENAME_INFO, FileName) + name_bytes as usize)
        .max(std::mem::size_of::<FILE_RENAME_INFO>());
    let size = u32::try_from(size).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "replacement path too long",
        )
    })?;
    // u64 storage provides the alignment and initialized flexible-tail capacity
    // required by FILE_RENAME_INFO on supported Windows targets.
    let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: the aligned allocation covers the header plus the UTF-16 name.
    unsafe {
        (*info).Anonymous.Flags =
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*info).FileNameLength = name_bytes;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
    }
    // Companion files inherit their new directory's permissions. Clear the
    // NamedTempFile temporary attribute before publishing the generation.
    let temporary_name: Vec<u16> = replacement
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: temporary_name is a live, NUL-terminated UTF-16 path.
    if unsafe { SetFileAttributesW(temporary_name.as_ptr(), FILE_ATTRIBUTE_NORMAL) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    crate::scalable_fault_point("replace.atomic");
    // SAFETY: file is a live DELETE-capable handle, and buffer contains a valid
    // FILE_RENAME_INFO followed by FileNameLength bytes of UTF-16 path data.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as _,
            FileRenameInfoEx,
            info.cast(),
            size,
        )
    };
    crate::scalable_fault_point("replace.atomic");
    if ok == 0 {
        // Do not fall back to a non-atomic remove/rename or wait for readers.
        return Err(std::io::Error::last_os_error().into());
    }
    match sync_parent_directory(target) {
        Ok(()) => Ok(ReplaceDurability::Durable),
        Err(error) => Ok(ReplaceDurability::ParentSyncPending(error)),
    }
}

#[cfg(not(windows))]
pub(crate) fn replace_path_atomically(
    replacement: &Path,
    target: &Path,
) -> Result<ReplaceDurability> {
    #[cfg(feature = "scalable-fault-injection")]
    take_injected_replace_indeterminate(replacement, target)?;
    crate::scalable_fault_point("replace.atomic");
    let replace = std::fs::rename(replacement, target);
    crate::scalable_fault_point("replace.atomic");
    replace?;
    match sync_parent_directory(target) {
        Ok(()) => Ok(ReplaceDurability::Durable),
        Err(error) => Ok(ReplaceDurability::ParentSyncPending(error)),
    }
}

#[cfg(windows)]
pub(crate) fn replace_path_atomically(
    replacement: &Path,
    target: &Path,
) -> Result<ReplaceDurability> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    #[cfg(feature = "scalable-fault-injection")]
    take_injected_replace_indeterminate(replacement, target)?;

    if !target.exists() {
        crate::scalable_fault_point("replace.atomic");
        let replace = std::fs::rename(replacement, target);
        crate::scalable_fault_point("replace.atomic");
        replace?;
        return match sync_parent_directory(target) {
            Ok(()) => Ok(ReplaceDurability::Durable),
            Err(error) => Ok(ReplaceDurability::ParentSyncPending(error)),
        };
    }

    // Captured before the call so an indeterminate 1176/1177 failure can be
    // reconciled cheaply afterwards: if the target pathname then resolves to
    // this exact OS object, the replacement de facto owns the target name.
    let replacement_identity = File::open(replacement)
        .ok()
        .and_then(|file| opened_file_identity(&file).ok());

    let replacement_wide: Vec<u16> = replacement
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let target_wide: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    crate::scalable_fault_point("replace.atomic");
    let ok = unsafe {
        ReplaceFileW(
            target_wide.as_ptr(),
            replacement_wide.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    crate::scalable_fault_point("replace.atomic");
    if ok == 0 {
        let os_error = std::io::Error::last_os_error();
        return match classify_replace_publication_error(os_error.raw_os_error().unwrap_or(0)) {
            // 1175 is a documented no-mutation outcome and every other code is
            // a documented pre-publication failure: both names are unchanged,
            // so the caller may treat this as "target untouched" and delete
            // its temp file.
            ReplacePublicationFailure::ReplacedFileIntact
            | ReplacePublicationFailure::PrePublication => Err(os_error.into()),
            ReplacePublicationFailure::IndeterminatePublication => {
                if replacement_identity
                    .as_deref()
                    .is_some_and(|identity| path_resolves_to_object(target, identity))
                {
                    // Reconciled: the replacement object now owns the target
                    // pathname, so the publication effectively happened.
                    match sync_parent_directory(target) {
                        Ok(()) => Ok(ReplaceDurability::Durable),
                        Err(error) => Ok(ReplaceDurability::ParentSyncPending(error)),
                    }
                } else {
                    // Unreconciled: names may be partially moved. The caller
                    // must preserve the replacement temp file and poison any
                    // writer bound to the target (DUR2-01).
                    Err(Error::ReplacePublicationIndeterminate {
                        path: target.display().to_string(),
                        replacement: replacement.display().to_string(),
                        source: os_error,
                    })
                }
            }
        };
    }
    match sync_parent_directory(target) {
        Ok(()) => Ok(ReplaceDurability::Durable),
        Err(error) => Ok(ReplaceDurability::ParentSyncPending(error)),
    }
}

/// Returns whether `path` currently resolves to the OS file object with the
/// given identity bytes. Any open or identity failure counts as "no".
///
/// Not Windows-only any more: [`VarlitFile::is_current`] is the public half of
/// exactly this question and needs the same answer on both platforms.
pub(super) fn path_resolves_to_object(path: &Path, identity: &[u8]) -> bool {
    File::open(path)
        .ok()
        .and_then(|file| opened_file_identity(&file).ok())
        .is_some_and(|current| current == identity)
}

#[cfg(unix)]
pub(crate) fn sync_parent_directory(path: &Path) -> Result<()> {
    #[cfg(test)]
    record_parent_directory_sync();
    #[cfg(test)]
    fail_parent_sync_if_requested()?;
    // `parent()` of a single-component relative path is an empty path, not
    // `None`, so the fallback below is unreachable without the filter and the
    // open lands on "" — a pathname with no directory component made every
    // `sync` report `PublishedButParentSyncPending`.
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    crate::scalable_fault_point("replace.parent_sync");
    #[cfg(feature = "scalable-fault-injection")]
    take_injected_parent_sync_failure()?;
    let sync = File::open(parent).and_then(|directory| directory.sync_all());
    crate::scalable_fault_point("replace.parent_sync");
    sync?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn sync_parent_directory(path: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    #[cfg(test)]
    record_parent_directory_sync();
    #[cfg(test)]
    fail_parent_sync_if_requested()?;
    // See the unix arm: an empty parent is what a bare filename yields, and it
    // is not `None`.
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // DUR2-02: FlushFileBuffers requires GENERIC_WRITE on the handle
    // (https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers),
    // so the directory must be opened with write access; a read-only directory
    // handle fails the flush with ERROR_ACCESS_DENIED on NTFS and the earlier
    // code silently promoted that refusal to `Durable`. Any open or flush
    // failure now propagates so the caller reports `ParentSyncPending`
    // instead of a false durability claim.
    let directory = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(parent)?;
    crate::scalable_fault_point("replace.parent_sync");
    #[cfg(feature = "scalable-fault-injection")]
    take_injected_parent_sync_failure()?;
    let sync = directory.sync_all();
    crate::scalable_fault_point("replace.parent_sync");
    sync?;
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
pub(super) static INJECTED_PARENT_SYNC_FAILURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "scalable-fault-injection")]
pub(super) fn take_injected_parent_sync_failure() -> Result<()> {
    use std::sync::atomic::Ordering;

    let mut current = INJECTED_PARENT_SYNC_FAILURES.load(Ordering::Acquire);
    while current != 0 {
        match INJECTED_PARENT_SYNC_FAILURES.compare_exchange(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                return Err(std::io::Error::other("injected parent-directory sync failure").into());
            }
            Err(observed) => current = observed,
        }
    }
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
pub(super) static INJECTED_REPLACE_INDETERMINATE_FAILURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "scalable-fault-injection")]
pub(super) fn take_injected_replace_indeterminate(replacement: &Path, target: &Path) -> Result<()> {
    use std::sync::atomic::Ordering;

    let mut current = INJECTED_REPLACE_INDETERMINATE_FAILURES.load(Ordering::Acquire);
    while current != 0 {
        match INJECTED_REPLACE_INDETERMINATE_FAILURES.compare_exchange(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // The publication is not attempted, so from the caller's
                // perspective this behaves exactly like an unreconciled
                // 1176/1177: the pathname state is unknown and the temp file
                // still exists.
                return Err(Error::ReplacePublicationIndeterminate {
                    path: target.display().to_string(),
                    replacement: replacement.display().to_string(),
                    source: std::io::Error::other("injected indeterminate replacement failure"),
                });
            }
            Err(observed) => current = observed,
        }
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static PARENT_DIRECTORY_SYNC_CALLS: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
pub(super) fn record_parent_directory_sync() {
    PARENT_DIRECTORY_SYNC_CALLS.set(PARENT_DIRECTORY_SYNC_CALLS.get() + 1);
}

#[cfg(test)]
pub(super) fn reset_parent_directory_sync_calls() {
    PARENT_DIRECTORY_SYNC_CALLS.set(0);
}

#[cfg(test)]
pub(super) fn parent_directory_sync_calls() -> u64 {
    PARENT_DIRECTORY_SYNC_CALLS.get()
}
