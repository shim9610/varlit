//! A path that names a FIFO or a device is refused, and promptly.
//!
//! Opening a FIFO with no writer blocks inside `open` itself, so a reader
//! handed such a path would wait forever before any check could run. Each open
//! here runs on its own thread with a deadline: a hang fails the test instead
//! of the test hanging.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use varlit::{VarlitBlock, varlit_format};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "fixed")]
struct Sample {
    value: u64,
}

varlit_format! {
    pub struct Specials {
        magic: b"TSPECL";
        version: 1;
        endian: little;
        blocks: [Sample];
    }
}

fn promptly<T: Send + 'static>(what: &str, open: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(open());
    });
    receiver
        .recv_timeout(Duration::from_secs(20))
        .unwrap_or_else(|_| panic!("{what} blocked instead of returning"))
}

fn fifo(directory: &Path) -> PathBuf {
    let path = directory.join("pipe.vlt");
    let status = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .expect("mkfifo");
    assert!(status.success());
    path
}

fn assert_refused(what: &str, result: varlit::Result<impl Sized>) {
    match result {
        Ok(_) => panic!("{what} accepted a special file"),
        Err(varlit::Error::Io(error)) => {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::InvalidInput | std::io::ErrorKind::WouldBlock
                ) || error.raw_os_error() == Some(libc_enxio()),
                "{what}: unexpected error {error}"
            );
        }
        Err(error) => panic!("{what}: unexpected error {error}"),
    }
}

/// `ENXIO`: opening a FIFO for writing with `O_NONBLOCK` and no reader.
fn libc_enxio() -> i32 {
    6
}

#[test]
fn a_fifo_is_refused_by_readers_and_writers_without_waiting() {
    let directory = tempfile::tempdir().unwrap();
    let path = fifo(directory.path());
    let p = path.clone();
    assert_refused(
        "open_readonly",
        promptly("open_readonly", move || Specials::open_readonly(&p)),
    );
    let p = path.clone();
    assert_refused(
        "open_readonly_scan",
        promptly("open_readonly_scan", move || {
            varlit::VarlitFile::open_readonly_scan(Specials::spec(), &p)
        }),
    );
    let p = path.clone();
    assert_refused("open", promptly("open", move || Specials::open(&p)));
    let p = path.clone();
    assert_refused("create", promptly("create", move || Specials::create(&p)));
    // Nothing replaced the FIFO.
    use std::os::unix::fs::FileTypeExt;
    assert!(std::fs::metadata(&path).unwrap().file_type().is_fifo());
}

#[cfg(target_os = "linux")]
#[test]
fn a_device_is_refused() {
    assert_refused(
        "open_readonly /dev/zero",
        promptly("open_readonly /dev/zero", || {
            Specials::open_readonly("/dev/zero")
        }),
    );
}

#[test]
fn a_regular_file_still_opens() -> varlit::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("regular.vlt");
    let mut writer = Specials::create(&path)?;
    writer.push(&Sample { value: 7 })?;
    writer.flush()?;
    drop(writer);
    let reader = Specials::open_readonly(&path)?;
    assert!(reader.record_count()? >= 1);
    let mut writer = Specials::open(&path)?;
    writer.push(&Sample { value: 8 })?;
    writer.flush()?;
    Ok(())
}
