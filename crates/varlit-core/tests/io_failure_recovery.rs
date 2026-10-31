//! Real kernel short writes, isolated in a child so file-size/signal limits
//! cannot affect other tests. This is not a simulated power failure.
#![cfg(target_os = "linux")]

use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use varlit_core::*;

#[derive(Debug, Clone, PartialEq)]
struct Record {
    id: u64,
    bytes: [u8; 8192],
}
impl VarlitEncode for Record {
    const WIRE_TYPE: WireType = WireType::Bytes;
    fn encode_varlit(&self, e: &mut Encoder) -> Result<()> {
        e.write_u64(self.id);
        e.write_all(&self.bytes);
        Ok(())
    }
}
impl VarlitDecode for Record {
    const WIRE_TYPE: WireType = WireType::Bytes;
    fn decode_varlit(d: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self {
            id: d.read_u64()?,
            bytes: d.read_exact(8192)?.try_into().unwrap(),
        })
    }
}
impl VarlitBlock for Record {
    const ID: u32 = 1;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 1;
}
fn spec(footer: bool) -> FormatSpec {
    FormatSpec::new(
        b"IOFAIL",
        1,
        Endian::Little,
        0,
        if footer {
            IndexPolicy::BlockOffsetChain
        } else {
            IndexPolicy::ScanOnOpen
        },
        if cfg!(feature = "integrity") {
            IntegrityPolicy::Crc32WithHeader
        } else {
            IntegrityPolicy::None
        },
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        &[BlockDescriptor {
            id: 1,
            name: "Record",
            version: 1,
            kind: BlockKind::Fixed,
            fields: &[],
        }],
    )
    .with_block_identities(&[Record::IDENTITY])
    .with_commit_policy(if footer {
        CommitPolicy::RecordFooter
    } else {
        CommitPolicy::None
    })
    .with_computed_schema_hash()
}

struct FileLimit(libc::rlimit);
impl FileLimit {
    fn set(bytes: u64) -> Self {
        let mut previous = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: valid pointers; only this explicitly isolated test child
        // changes its soft limit. The hard limit is never changed.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut previous) },
            0
        );
        #[allow(clippy::useless_conversion)] // rlim_t differs across Linux ABIs.
        let soft_limit = bytes.try_into().expect("test limit fits rlim_t");
        let next = libc::rlimit {
            rlim_cur: soft_limit,
            ..previous
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &next) }, 0);
        Self(previous)
    }
}
impl Drop for FileLimit {
    fn drop(&mut self) {
        // SAFETY: restore the child process's saved soft limit.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &self.0) }, 0);
    }
}

#[test]
fn partial_kernel_writes_preserve_retry_and_reopen() {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "kernel_failure_child",
            "--ignored",
            "--nocapture",
        ])
        .env("VARLIT_KERNEL_FAILURE_CHILD", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("kernel failure child timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "child failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("kernel short-write recovery trials=28")
    );
    println!("kernel short-write recovery trials=28");
}

#[test]
#[ignore = "isolated child; invoked by partial_kernel_writes_preserve_retry_and_reopen"]
fn kernel_failure_child() -> Result<()> {
    assert_eq!(std::env::var("VARLIT_KERNEL_FAILURE_CHILD").unwrap(), "1");
    // SAFETY: ignore SIGXFSZ only in the disposable child. write then returns
    // EFBIG, permitting verification of the library's ordinary error path.
    assert_ne!(
        unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) },
        libc::SIG_ERR
    );
    let dir = tempfile::tempdir()?;
    // Prove the mechanism really writes a prefix before returning an error.
    let probe = dir.path().join("probe");
    let mut file = fs::File::create(&probe)?;
    {
        let _limit = FileLimit::set(7);
        assert_eq!(
            file.write_all(&[0; 16]).unwrap_err().raw_os_error(),
            Some(libc::EFBIG)
        );
    }
    assert_eq!(fs::metadata(&probe)?.len(), 7);
    let mut trials = 0;
    for footer in [false, true] {
        for stream in [false, true] {
            let path = dir.path().join(format!("{footer}-{stream}.varlit"));
            let spec = spec(footer);
            let value = |id| Record {
                id,
                bytes: [id as u8; 8192],
            };
            let mut resident = (!stream)
                .then(|| VarlitFile::create(spec, &path))
                .transpose()?;
            let mut streaming = stream
                .then(|| VarlitStreamWriter::create(spec, &path, StreamOptions::default()))
                .transpose()?;
            let record_len;
            {
                let mut push = |record: &Record| -> Result<()> {
                    if let Some(w) = &mut resident {
                        w.push_info(record)?;
                    } else {
                        streaming
                            .as_mut()
                            .unwrap()
                            .push_iter::<Record, _>([record], BatchOptions::default())
                            .map_err(|e| e.source)?;
                    }
                    Ok(())
                };
                for id in 0..63 {
                    push(&value(id))?;
                }
                let before_last = fs::metadata(&path)?.len();
                push(&value(63))?;
                record_len = fs::metadata(&path)?.len() - before_last;
            }
            if let Some(w) = &mut resident {
                w.sync()?;
            } else {
                streaming.as_mut().unwrap().sync()?;
            }
            let mut expected: Vec<_> = (0..64).map(value).collect();
            // Boundaries inside header, payload and footer; each retry uses
            // the same writer, and a fresh reader checks all previous values.
            for cut in [0, 1, 31, 32, record_len / 2, record_len - 8, record_len - 1] {
                let before = fs::read(&path)?;
                let record = value(expected.len() as u64);
                let failed = {
                    let _limit = FileLimit::set(before.len() as u64 + cut);
                    if let Some(w) = &mut resident {
                        w.push_info(&record).map(|_| ())
                    } else {
                        streaming
                            .as_mut()
                            .unwrap()
                            .push_iter::<Record, _>([&record], BatchOptions::default())
                            .map(|_| ())
                            .map_err(|e| e.source)
                    }
                };
                assert!(
                    matches!(failed, Err(Error::Io(ref e)) if e.raw_os_error()==Some(libc::EFBIG)),
                    "{stream}/{footer}/{cut}: {failed:?}"
                );
                assert!(
                    fs::read(&path)? == before,
                    "failed write changed confirmed bytes"
                );
                if let Some(w) = &mut resident {
                    w.push_info(&record)?;
                    w.sync()?;
                } else {
                    let w = streaming.as_mut().unwrap();
                    w.push_info(&record)?;
                    w.sync()?;
                }
                expected.push(record);
                let reader = VarlitFile::open_readonly(spec, &path)?;
                let actual: Vec<_> = reader.blocks::<Record>()?.iter().collect::<Result<_>>()?;
                assert!(
                    actual == expected,
                    "reopen disagreed after {stream}/{footer}/{cut}"
                );
                trials += 1;
            }
        }
    }
    assert_eq!(trials, 28);
    println!("kernel short-write recovery trials={trials}");
    Ok(())
}
