use std::{
    env, fs,
    path::{Path, PathBuf},
};
fn main() {
    assert_eq!(
        env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("linux"),
        "This diagnostic harness currently supports Linux native libraries."
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let mut dirs = Vec::new();
    if let Some(p) = env::var_os("VARLIT_BENCH_NATIVE_LIB_DIR") {
        dirs.push(PathBuf::from(p));
    }
    dirs.extend(
        [
            "/lib/x86_64-linux-gnu",
            "/usr/lib/x86_64-linux-gnu",
            "/lib/aarch64-linux-gnu",
            "/usr/lib/aarch64-linux-gnu",
            "/usr/lib64",
            "/usr/lib",
        ]
        .map(PathBuf::from),
    );
    for name in ["sqlite3", "hdf5_serial"] {
        let stem = format!("lib{name}.so");
        let found = dirs
            .iter()
            .find_map(|d| {
                let direct = d.join(&stem);
                if direct.exists() {
                    return Some(direct);
                }
                let mut files = fs::read_dir(d)
                    .ok()?
                    .filter_map(Result::ok)
                    .map(|x| x.path())
                    .filter(|p| {
                        p.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(&(stem.clone() + "."))
                    })
                    .collect::<Vec<_>>();
                files.sort();
                files.pop()
            })
            .unwrap_or_else(|| {
                panic!(
                    "Missing {stem}; install its system library or set VARLIT_BENCH_NATIVE_LIB_DIR"
                )
            });
        let target = out.join(&stem);
        if target.exists() {
            fs::remove_file(&target).unwrap();
        }
        std::os::unix::fs::symlink(Path::new(&found), target).unwrap();
    }
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rerun-if-env-changed=VARLIT_BENCH_NATIVE_LIB_DIR");
}
