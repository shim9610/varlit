//! Temporary directories only this user can write, whatever the umask.
//!
//! Self-test cleanup refuses to unlink in a group- or world-writable parent
//! that is not sticky, and `tempfile::tempdir()` leaves the mode to the umask
//! (`0775` under the common `0002`).
#![allow(dead_code)]

pub fn private_tempdir() -> std::io::Result<tempfile::TempDir> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir()
}
