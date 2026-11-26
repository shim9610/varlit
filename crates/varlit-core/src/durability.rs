//! How far a durable sync reaches: into the drive's own write cache, or only
//! to the drive.
//!
//! On Linux, FreeBSD and Windows a file sync (`fdatasync`/`fsync`,
//! `FlushFileBuffers`) already asks the drive to flush its write cache, so the
//! two settings behave alike there. On Apple platforms a plain `fsync(2)`
//! hands the data to the drive without flushing its cache; only
//! `fcntl(F_FULLFSYNC)` does, at several milliseconds per call on typical
//! hardware. Rust's `File::sync_data`/`sync_all` use `F_FULLFSYNC` there.
//! [`DeviceFlush`] chooses between the two.

#![allow(unsafe_code)]

use std::fs::File;

/// Whether a durable sync also flushes the drive's write cache.
///
/// This is runtime policy, like [`crate::ReadLimits`]: it is not stored in a
/// file, not part of the schema hash, and two handles of the same file may
/// differ. It affects only Apple platforms; see the module documentation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DeviceFlush {
    /// Flush the drive's write cache too (`F_FULLFSYNC` on Apple platforms):
    /// a completed sync survives power loss on drives that honour the flush.
    /// The default.
    #[default]
    Full,
    /// Stop at the operating system's plain sync (`fsync(2)` on Apple
    /// platforms): the data has left the OS, but may still sit in the
    /// drive's volatile cache, so a power loss can lose or reorder writes
    /// the sync reported complete. This is SQLite's default on macOS
    /// (`PRAGMA fullfsync=OFF`). Elsewhere it behaves as [`Self::Full`].
    Os,
}

impl DeviceFlush {
    /// Whether this setting changes anything on the platform this build
    /// targets.
    pub const fn differs_on_this_platform() -> bool {
        cfg!(any(target_os = "macos", target_os = "ios"))
    }
}

/// Makes `file`'s data durable under `flush`.
pub(crate) fn sync_data(file: &File, flush: DeviceFlush) -> std::io::Result<()> {
    match flush {
        DeviceFlush::Full => file.sync_data(),
        DeviceFlush::Os => os_sync(file, false),
    }
}

/// Makes `file`'s data and metadata durable under `flush`.
pub(crate) fn sync_all(file: &File, flush: DeviceFlush) -> std::io::Result<()> {
    match flush {
        DeviceFlush::Full => file.sync_all(),
        DeviceFlush::Os => os_sync(file, true),
    }
}

/// A plain `fsync(2)` on Apple platforms, which `std` replaces with
/// `F_FULLFSYNC`. Data and metadata are both synced: Apple has no separate
/// data-only sync that skips the drive flush.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn os_sync(file: &File, _metadata: bool) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: `fsync` takes a file descriptor and touches no memory; the
        // descriptor is valid for as long as `file` is borrowed.
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Elsewhere the standard sync already is the operating system's sync.
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn os_sync(file: &File, metadata: bool) -> std::io::Result<()> {
    if metadata {
        file.sync_all()
    } else {
        file.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_settings_sync_a_written_file() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!("varlit-durability-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("f");
        std::fs::write(&path, b"bytes")?;
        let file = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
        for flush in [DeviceFlush::Full, DeviceFlush::Os] {
            sync_data(&file, flush)?;
            sync_all(&file, flush)?;
        }
        drop(file);
        std::fs::remove_dir_all(&dir)
    }

    #[test]
    fn the_default_flushes_the_drive_cache() {
        assert_eq!(DeviceFlush::default(), DeviceFlush::Full);
    }
}
