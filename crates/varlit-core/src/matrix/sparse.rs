//! sparse implementation, kept within the original parent privacy boundary.
use super::*;

#[cfg(windows)]
pub(super) fn query_allocated_extents(file: &File) -> Option<Vec<(u64, u64)>> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        FILE_ALLOCATED_RANGE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES,
    };

    const ENTRY_LEN: usize = std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>();

    let file_len = i64::try_from(file.raw().metadata().ok()?.len()).ok()?;
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    if file_len == 0 {
        return Some(ranges);
    }
    let mut output = vec![FILE_ALLOCATED_RANGE_BUFFER::default(); 512];
    let mut scan_from = 0i64;
    loop {
        let input = FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: scan_from,
            Length: file_len - scan_from,
        };
        let mut returned = 0u32;
        // SAFETY: the handle is borrowed from a live `File` opened for reading,
        // the input and output buffers are valid for the byte lengths passed,
        // and `returned` is a writable out pointer. The FSCTL only reads
        // allocation metadata.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle() as _,
                FSCTL_QUERY_ALLOCATED_RANGES,
                ptr::from_ref(&input).cast(),
                ENTRY_LEN as u32,
                output.as_mut_ptr().cast(),
                (output.len() * ENTRY_LEN) as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        let more = if ok == 0 {
            if std::io::Error::last_os_error().raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                return None;
            }
            true
        } else {
            false
        };
        let count = returned as usize / ENTRY_LEN;
        for entry in &output[..count] {
            let start = u64::try_from(entry.FileOffset).ok()?;
            let end = start.checked_add(u64::try_from(entry.Length).ok()?)?;
            ranges.push((start, end));
        }
        if !more || count == 0 {
            break;
        }
        scan_from = output[count - 1]
            .FileOffset
            .checked_add(output[count - 1].Length)?;
        if scan_from >= file_len || ranges.len() > MAX_TRACKED_EXTENTS {
            break;
        }
    }
    if ranges.len() > MAX_TRACKED_EXTENTS {
        return None;
    }
    Some(ranges)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn query_allocated_extents(file: &File) -> Option<Vec<(u64, u64)>> {
    use std::os::unix::io::AsRawFd;

    let fd = file.as_raw_fd();
    let file_len = file.raw().metadata().ok()?.len();
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut cursor = 0i64;
    while (cursor as u64) < file_len {
        // SAFETY: `fd` is borrowed from a live `File`; `lseek` only moves the
        // descriptor offset, which the caller restores.
        let data = unsafe { libc::lseek(fd, cursor, libc::SEEK_DATA) };
        if data < 0 {
            // `ENXIO` means there is no data at or after `cursor`, which is the
            // documented end of the scan. Anything else means the filesystem
            // cannot answer, so nothing may be skipped.
            return match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ENXIO) => Some(ranges),
                _ => None,
            };
        }
        // SAFETY: same invariants as the `SEEK_DATA` call above.
        let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        if hole < 0 {
            return None;
        }
        ranges.push((data as u64, hole as u64));
        cursor = hole;
        if ranges.len() > MAX_TRACKED_EXTENTS {
            return None;
        }
    }
    Some(ranges)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(super) fn query_allocated_extents(_file: &File) -> Option<Vec<(u64, u64)>> {
    None
}

/// Best-effort request that the filesystem represent unwritten regions of a
/// newly created matrix as holes.
///
/// On POSIX this is already the default. On Windows a file must carry the
/// sparse attribute *before* it is extended, otherwise `set_len` allocates the
/// clusters it reserves and `FSCTL_QUERY_ALLOCATED_RANGES` reports the whole
/// file as data. Failure is recorded in `MatrixStorageDiagnostics`: correctness is unchanged,
/// but a dense declaration may consume its full logical size.
#[cfg(windows)]
pub(super) fn mark_file_sparse(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

    let mut returned = 0u32;
    // SAFETY: the handle is borrowed from a live writable `File`,
    // `FSCTL_SET_SPARSE` accepts null input/output buffers, and `returned` is a
    // valid out pointer.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_SET_SPARSE,
            ptr::null(),
            0,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
pub(super) fn mark_file_sparse(_file: &File) -> std::io::Result<()> {
    Ok(())
}

/// Per-file storage observations captured at matrix create/open.
/// These qualify performance; they do not classify file corruption.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MatrixStorageDiagnostics {
    /// Some(true): sparse flag/holes observed. Some(false): disabled or known
    /// unsupported filesystem. None: sparseness could not be established.
    pub sparse_file: Option<bool>,
    /// The actual file answered a bounded allocation-map probe.
    pub allocation_map_available: bool,
    /// This build has a native hole-punch API. A filesystem may still refuse
    /// individual requests; the zero-range byte counters report that fallback.
    pub hole_punch_api_available: bool,
    /// Windows refused the create-time sparse-file request.
    pub sparse_mark_failed: bool,
}

pub(super) fn storage_diagnostics(
    file: &File,
    sparse_mark_failed: bool,
) -> MatrixStorageDiagnostics {
    // Miri does not expose native filesystem capability queries. Unknown
    // observations preserve correctness without adding an unsupported syscall
    // to otherwise portable matrix algorithms.
    if cfg!(miri) {
        return MatrixStorageDiagnostics {
            sparse_mark_failed,
            ..MatrixStorageDiagnostics::default()
        };
    }
    let (map, holes) = allocation_probe(file).unwrap_or((false, false));
    let sparse_file = match sparse_attribute(file) {
        Some(v) => Some(v),
        None if holes => Some(true),
        None => None,
    };
    MatrixStorageDiagnostics {
        sparse_file,
        allocation_map_available: map,
        hole_punch_api_available: cfg!(any(windows, target_os = "linux", target_os = "macos")),
        sparse_mark_failed,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn allocation_probe(file: &File) -> Option<(bool, bool)> {
    use std::os::unix::io::AsRawFd;
    if allocation_map_forced_unavailable() {
        return None;
    }
    let len = file.metadata().ok()?.len();
    let mut raw = file;
    let saved = std::io::Seek::stream_position(&mut raw).ok()?;
    // Only at create/open, before publishing this handle. No shared reader
    // or live writer cursor is probed, and the cursor is restored on errors.
    // SAFETY: live descriptor; the offset is restored before this private
    // create/open handle is exposed to callers.
    let hole = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_HOLE) };
    let restored = std::io::Seek::seek(&mut raw, SeekFrom::Start(saved)).is_ok();
    if !restored || hole < 0 {
        return None;
    }
    Some((true, (hole as u64) < len))
}
#[cfg(windows)]
pub(super) fn allocation_probe(file: &File) -> Option<(bool, bool)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::ERROR_MORE_DATA,
        System::{
            IO::DeviceIoControl,
            Ioctl::{FILE_ALLOCATED_RANGE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES},
        },
    };
    if allocation_map_forced_unavailable() {
        return None;
    }
    let len = i64::try_from(file.metadata().ok()?.len()).ok()?;
    if len == 0 {
        return Some((true, false));
    }
    let input = FILE_ALLOCATED_RANGE_BUFFER {
        FileOffset: 0,
        Length: len,
    };
    let mut output = FILE_ALLOCATED_RANGE_BUFFER::default();
    let mut returned = 0;
    // SAFETY: live read-capable handle, initialized fixed-size input/output
    // structures and valid length/result pointers. No file bytes are changed.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_QUERY_ALLOCATED_RANGES,
            (&input as *const FILE_ALLOCATED_RANGE_BUFFER).cast(),
            std::mem::size_of_val(&input) as u32,
            (&mut output as *mut FILE_ALLOCATED_RANGE_BUFFER).cast(),
            std::mem::size_of_val(&output) as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 && std::io::Error::last_os_error().raw_os_error() != Some(ERROR_MORE_DATA as i32) {
        return None;
    }
    Some((true, returned == 0 || output.FileOffset > 0))
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(super) fn allocation_probe(_file: &File) -> Option<(bool, bool)> {
    None
}

#[cfg(windows)]
pub(super) fn sparse_attribute(file: &File) -> Option<bool> {
    use std::os::windows::fs::MetadataExt;
    Some(
        file.metadata().ok()?.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SPARSE_FILE
            != 0,
    )
}
#[cfg(target_os = "linux")]
pub(super) fn sparse_attribute(file: &File) -> Option<bool> {
    use std::os::unix::io::AsRawFd;
    let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: valid descriptor and writable storage for fstatfs. Read only
    // after success. The query does not change contents or descriptor offset.
    if unsafe { libc::fstatfs(file.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
        return None;
    }
    let kind = unsafe { fs.assume_init() }.f_type as u64;
    if kind == libc::MSDOS_SUPER_MAGIC as u64 || kind == 0x2011bab0 {
        Some(false)
    } else {
        None
    }
}
#[cfg(target_os = "macos")]
pub(super) fn sparse_attribute(file: &File) -> Option<bool> {
    use std::os::unix::io::AsRawFd;
    let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: as for the Linux fstatfs query; the filesystem name is read
    // from the initialized fixed-size array only after a successful call.
    if unsafe { libc::fstatfs(file.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
        return None;
    }
    let fs = unsafe { fs.assume_init() };
    let name: Vec<u8> = fs
        .f_fstypename
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    if name == b"msdos" || name == b"exfat" {
        Some(false)
    } else {
        None
    }
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(super) fn sparse_attribute(_file: &File) -> Option<bool> {
    None
}

/// Zeroes `[offset, offset + len)`, deallocating the range where the platform
/// supports it.
///
/// Returns `true` when the range was zeroed by a hole punch, which is `O(1)` in
/// `len`; `false` means the caller must fall back to writing zeros.
pub(super) fn punch_zero_range(file: &mut impl MatrixIo, offset: u64, len: u64) -> Result<bool> {
    if len == 0 {
        return Ok(true);
    }
    if file.zero_pages(offset, len)? {
        return Ok(true);
    }
    Ok(punch_zero_range_native(file.raw(), offset, len))
}

#[cfg(windows)]
pub(super) fn punch_zero_range_native(file: &File, offset: u64, len: u64) -> bool {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{FILE_ZERO_DATA_INFORMATION, FSCTL_SET_ZERO_DATA};

    let (Some(file_offset), Some(end)) = (
        i64::try_from(offset).ok(),
        offset
            .checked_add(len)
            .and_then(|end| i64::try_from(end).ok()),
    ) else {
        return false;
    };
    let input = FILE_ZERO_DATA_INFORMATION {
        FileOffset: file_offset,
        BeyondFinalZero: end,
    };
    let mut returned = 0u32;
    // SAFETY: the handle is borrowed from a live writable `File`, the input
    // buffer is valid for the byte length passed, and `returned` is a writable
    // out pointer.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_SET_ZERO_DATA,
            ptr::from_ref(&input).cast(),
            std::mem::size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    ok != 0
}

#[cfg(target_os = "linux")]
pub(super) fn punch_zero_range_native(file: &File, offset: u64, len: u64) -> bool {
    use std::os::unix::io::AsRawFd;

    let (Ok(offset), Ok(len)) = (libc::off_t::try_from(offset), libc::off_t::try_from(len)) else {
        return false;
    };
    // SAFETY: `fd` is borrowed from a live writable `File`; `fallocate` only
    // affects the requested byte range and never changes the file length with
    // `FALLOC_FL_KEEP_SIZE`.
    let result = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset,
            len,
        )
    };
    result == 0
}

#[cfg(target_os = "macos")]
pub(super) fn punch_zero_range_native(file: &File, offset: u64, len: u64) -> bool {
    use std::os::unix::io::AsRawFd;
    let (Ok(offset), Ok(len)) = (libc::off_t::try_from(offset), libc::off_t::try_from(len)) else {
        return false;
    };
    let range = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset,
        fp_length: len,
    };
    // SAFETY: borrowed live writable descriptor and an initialized platform
    // punch-hole structure. This does not change the file's logical length.
    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &range) == 0 }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub(super) fn punch_zero_range_native(_file: &File, _offset: u64, _len: u64) -> bool {
    false
}

#[cfg(all(test, not(miri), any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    #[test]
    fn a_bounded_allocation_probe_preserves_the_private_handle_position() {
        let mut file = tempfile::tempfile().unwrap();
        file.set_len(8 * 1024 * 1024).unwrap();
        std::io::Seek::seek(&mut file, SeekFrom::Start(123)).unwrap();
        let _observations = storage_diagnostics(&file, false);
        assert_eq!(std::io::Seek::stream_position(&mut file).unwrap(), 123);
    }
}
