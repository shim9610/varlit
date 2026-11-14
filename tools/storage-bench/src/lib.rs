pub mod hdf;
pub mod matrix_table;
pub mod model;
pub mod native;
pub mod packed_table;
pub mod sqlite;
use std::{ffi::c_int, path::Path, time::Instant};
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
#[repr(C)]
struct Timespec {
    sec: std::ffi::c_long,
    nsec: std::ffi::c_long,
}
unsafe extern "C" {
    fn clock_gettime(id: c_int, t: *mut Timespec) -> c_int;
}
pub fn thread_cpu() -> f64 {
    let mut t = Timespec { sec: 0, nsec: 0 };
    assert_eq!(unsafe { clock_gettime(3, &mut t) }, 0);
    t.sec as f64 + t.nsec as f64 / 1e9
}
pub fn monotonic_ns() -> u64 {
    let mut t = Timespec { sec: 0, nsec: 0 };
    assert_eq!(unsafe { clock_gettime(1, &mut t) }, 0);
    t.sec as u64 * 1_000_000_000 + t.nsec as u64
}
pub fn file_usage(root: &Path) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let (mut len, mut allocated) = (0, 0);
    for e in std::fs::read_dir(root)? {
        let e = e?;
        let m = e.metadata()?;
        if m.is_dir() {
            let (a, b) = file_usage(&e.path())?;
            len += a;
            allocated += b;
        } else if m.is_file() {
            len += m.len();
            allocated += m.blocks() * 512;
        }
    }
    Ok((len, allocated))
}
pub struct Stopwatch {
    wall: Instant,
    cpu: f64,
}
impl Stopwatch {
    pub fn start() -> Self {
        Self {
            wall: Instant::now(),
            cpu: thread_cpu(),
        }
    }
    pub fn stop(self) -> (f64, f64) {
        (self.wall.elapsed().as_secs_f64(), thread_cpu() - self.cpu)
    }
}
/// Optional conformance gate, disabled in timing runs. Readers must observe the
/// previous application prefix while a complete new transaction is still staged.
pub fn before_publish(previous_publications: u64) {
    if previous_publications != 0 {
        return;
    }
    let Some(root) = std::env::var_os("VARLIT_BENCH_VISIBILITY_GATE") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    std::fs::write(root.join("staged"), b"staged").unwrap();
    let n: usize = std::env::var("VARLIT_BENCH_GATE_READERS")
        .unwrap()
        .parse()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !(0..n).all(|i| root.join(format!("observed-{i}")).exists()) {
        assert!(
            std::time::Instant::now() < deadline,
            "unpublished-prefix conformance timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
