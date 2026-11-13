//! Policy for disposable private scratch, independent of database placement.
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    ops::Deref,
};

#[derive(Debug)]
pub(crate) struct SpillFile {
    file: File,
    maximum: Option<u64>,
    position: u64,
}

/// The largest one scratch file may grow when `VARLIT_SPILL_MAX_BYTES` is not
/// set. Scratch on a tmpfs `TMPDIR` is memory, so it cannot be unbounded by
/// default; a workload that needs more says so through the variable.
pub(crate) const DEFAULT_SPILL_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub(crate) fn check_extent(maximum: Option<u64>, end: u64) -> io::Result<()> {
    if maximum.is_some_and(|maximum| end > maximum) {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "scratch file exceeded its maximum (VARLIT_SPILL_MAX_BYTES, 4 GiB by default)",
        ));
    }
    Ok(())
}

impl SpillFile {
    pub(crate) fn new() -> io::Result<Self> {
        let maximum = std::env::var_os("VARLIT_SPILL_MAX_BYTES")
            .map(|value| {
                value
                    .to_str()
                    .filter(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "VARLIT_SPILL_MAX_BYTES must be an unsigned decimal byte count",
                        )
                    })
            })
            .transpose()?
            .or(Some(DEFAULT_SPILL_MAX_BYTES));
        let file = match std::env::var_os("VARLIT_SPILL_DIR") {
            Some(directory) if directory.is_empty() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "VARLIT_SPILL_DIR is empty",
                ));
            }
            Some(directory) => tempfile::tempfile_in(directory)?,
            None => tempfile::tempfile()?,
        };
        Ok(Self {
            file,
            maximum,
            position: 0,
        })
    }
    pub(crate) fn into_parts(self) -> (File, Option<u64>) {
        (self.file, self.maximum)
    }
}

// Shared positional reads need the underlying file; mutations use Write below.
impl Deref for SpillFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.file
    }
}
impl Read for SpillFile {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = self.file.read(bytes)?;
        self.position += n as u64;
        Ok(n)
    }
}
impl Seek for SpillFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.position = self.file.seek(position)?;
        Ok(self.position)
    }
}
impl Write for SpillFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.maximum.is_none() {
            return self.file.write(bytes);
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let end = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or(io::ErrorKind::InvalidInput)?;
        check_extent(self.maximum, end)?;
        let n = self.file.write(bytes)?;
        self.position += n as u64;
        Ok(n)
    }
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        // Preserve File's specialized complete-write path for the default
        // unlimited policy. Position is relevant only to a captured limit.
        if self.maximum.is_none() {
            return self.file.write_all(bytes);
        }
        limited_write_all(&mut self.position, bytes, self.maximum.unwrap(), |bytes| {
            self.file.write(bytes)
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

// The real limited path and symbolic I/O harness execute this same loop.
fn limited_write_all(
    position: &mut u64,
    mut bytes: &[u8],
    maximum: u64,
    mut write: impl FnMut(&[u8]) -> io::Result<usize>,
) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let end = (*position)
        .checked_add(bytes.len() as u64)
        .ok_or(io::ErrorKind::InvalidInput)?;
    check_extent(Some(maximum), end)?;
    // Keep exact progress through interrupted/partial limited writes so a
    // retry cannot authorize an extent from a stale stream position.
    while !bytes.is_empty() {
        match write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                *position += n as u64;
                bytes = &bytes[n..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(kani)]
#[path = "verification/spill.rs"]
mod proofs;

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::{disk_index::ScratchIndex, packed_rows::PackedRows, spill_rows::SpillRows};
    use std::{collections::BTreeMap, process::Command};

    #[test]
    fn isolated_policy_preserves_paths_limits_and_retry_state() -> crate::Result<()> {
        let Some(case) = std::env::var_os("VARLIT_SPILL_TEST_CASE") else {
            let directory = tempfile::tempdir()?;
            for (case, limit) in [
                ("unset", None),
                ("fallback", None),
                ("zero", Some("0")),
                ("limited", Some("32768")),
                ("empty", Some("")),
                ("negative", Some("-1")),
                ("overflow", Some("18446744073709551616")),
                ("missing", None),
            ] {
                let path = if case == "missing" {
                    directory.path().join("missing")
                } else {
                    directory.path().to_path_buf()
                };
                let mut command = Command::new(std::env::current_exe()?);
                command
                    .args([
                        "spill_file::tests::isolated_policy_preserves_paths_limits_and_retry_state",
                        "--exact",
                        "--nocapture",
                    ])
                    .env("VARLIT_SPILL_TEST_CASE", case)
                    .env("VARLIT_SPILL_DIR", path)
                    .env_remove("VARLIT_SPILL_MAX_BYTES");
                if case == "fallback" {
                    command
                        .env_remove("VARLIT_SPILL_DIR")
                        .env("TMPDIR", directory.path());
                }
                if let Some(limit) = limit {
                    command.env("VARLIT_SPILL_MAX_BYTES", limit);
                }
                let result = command.output()?;
                assert!(
                    result.status.success(),
                    "spill policy case {case}: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
            println!("SPILL_POLICY isolated path, capacity and retry cases=8");
            return Ok(());
        };
        let case = case.to_str().unwrap();
        if matches!(case, "empty" | "negative" | "overflow" | "missing") {
            assert!(SpillFile::new().is_err());
            return Ok(());
        }
        let mut file = SpillFile::new()?;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let path = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            let selected = std::env::var_os("VARLIT_SPILL_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            assert!(path.starts_with(selected));
        }
        if matches!(case, "unset" | "fallback") {
            // Unset is not unbounded: the default ceiling applies.
            assert_eq!(file.maximum, Some(DEFAULT_SPILL_MAX_BYTES));
            file.write_all(&vec![7; 1024 * 1024])?;
        } else if case == "zero" {
            assert_eq!(
                file.write(&[1]).unwrap_err().kind(),
                io::ErrorKind::StorageFull
            );
            assert_eq!(file.metadata()?.len(), 0);
            assert!(ScratchIndex::new(0).is_err());
            let mut rows = SpillRows::<8>::unbuffered();
            assert!(rows.push([1; 8]).is_err());
            assert_eq!(rows.len(), 0);
        } else {
            file.write_all(&[7; 16])?;
            assert_eq!(
                file.write(&[8; 32768]).unwrap_err().kind(),
                io::ErrorKind::StorageFull
            );
            assert_eq!(file.metadata()?.len(), 16);
            file.seek(SeekFrom::Start(8))?;
            file.write_all(&[9; 8])?;
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = [0; 16];
            file.read_exact(&mut bytes)?;
            assert_eq!(bytes, [7, 7, 7, 7, 7, 7, 7, 7, 9, 9, 9, 9, 9, 9, 9, 9]);
            let mut rows = SpillRows::<8>::new(8);
            while rows.push((rows.len() as u64).to_le_bytes()).is_ok() {}
            let retained = rows.len();
            assert_eq!(rows.get(0)?, Some(0u64.to_le_bytes()));
            assert!(rows.push([99; 8]).is_err());
            assert_eq!(rows.len(), retained);
            assert_eq!(
                rows.get(retained - 1)?,
                Some(((retained - 1) as u64).to_le_bytes())
            );
            let mut packed = PackedRows::<8>::new(0);
            while packed.push((packed.len() as u64).to_le_bytes()).is_ok() {}
            let retained = packed.len();
            assert!(packed.push([99; 8]).is_err());
            assert_eq!(packed.len(), retained);
            assert_eq!(packed.get(0)?, Some(0u64.to_le_bytes()));
            assert_eq!(
                packed.get(retained - 1)?,
                Some(((retained - 1) as u64).to_le_bytes())
            );
            let mut index = ScratchIndex::new(4096)?;
            let mut values = BTreeMap::from([(vec![1], [7; 52])]);
            index.apply(&values)?;
            let held = index.clone();
            values = (0..10000u64)
                .map(|key| (key.to_le_bytes().to_vec(), [8; 52]))
                .collect();
            assert!(index.apply(&values).is_err());
            assert_eq!(index.get(&[1])?, Some([7; 52]));
            assert_eq!(held.get(&[1])?, Some([7; 52]));
            index.apply(&BTreeMap::from([(vec![2], [9; 52])]))?;
            assert_eq!(index.get(&[2])?, Some([9; 52]));
            #[cfg(unix)]
            limited_partial_write_tracks_progress()?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn limited_partial_write_tracks_progress() -> io::Result<()> {
        let mut file = SpillFile::new()?;
        assert_eq!(file.maximum, Some(32768));
        let mut prior = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: this exact test runs alone in an isolated child process;
        // pointers are valid and its limit/signal handler are restored before
        // inspecting the deliberately partial write result.
        let result = unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut prior), 0);
            let signal = libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            assert_ne!(signal, libc::SIG_ERR);
            let limit = libc::rlimit {
                rlim_cur: 16,
                rlim_max: prior.rlim_max,
            };
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
            let result = file.write_all(&[7; 32]);
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &prior), 0);
            libc::signal(libc::SIGXFSZ, signal);
            result
        };
        assert!(result.is_err());
        assert_eq!(file.position, 16);
        assert_eq!(file.metadata()?.len(), 16);
        assert_eq!(
            file.write_all(&[8; 32760]).unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        assert_eq!(file.metadata()?.len(), 16);
        file.write_all(&[9; 8])?;
        assert_eq!(file.metadata()?.len(), 24);
        Ok(())
    }
}
