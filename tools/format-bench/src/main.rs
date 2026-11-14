mod analytics;
mod cells;
mod common;
mod grid;
mod mixed;
mod oltp;

use common::Result;
use std::path::PathBuf;

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        // bench <scenario> <format> <fresh empty dir>
        Some("bench") => {
            let dir = PathBuf::from(&a[4]);
            let fmt_full = a[3].clone();
            let fmt = fmt_full
                .strip_suffix("+trusted")
                .unwrap_or(&fmt_full)
                .to_string();
            if fmt != fmt_full {
                // SAFETY: single-threaded at this point; followers inherit it.
                unsafe { std::env::set_var("FB_TRUSTED", "1") };
            }
            let a = [a[0].clone(), a[1].clone(), a[2].clone(), fmt, a[4].clone()];
            std::fs::create_dir_all(&dir)?;
            let mut m = match a[2].as_str() {
                "grid" => grid::run(&a[3], &dir)?,
                "oltp" => oltp::run(&a[3], &dir)?,
                "analytics" => analytics::run(&a[3], &dir)?,
                "mixed" => mixed::run(&a[3], &dir)?,
                "cells" => cells::run(&a[3], &dir)?,
                s => return Err(format!("unknown scenario {s}").into()),
            };
            m.set("scenario", a[2].as_str());
            m.set("format", fmt_full.as_str());
            m.print();
        }
        // candidates <scenario> <engine>: storage configurations to tune over
        Some("candidates") => {
            let c: &[&str] = match a[2].as_str() {
                "cells" => cells::candidates(&a[3])?,
                "oltp" => oltp::candidates(&a[3])?,
                "analytics" => analytics::candidates(&a[3])?,
                "mixed" => mixed::candidates(&a[3])?,
                "grid" => grid::candidates(&a[3])?,
                s => return Err(format!("no candidates for {s}").into()),
            };
            println!("{}", serde_json::to_string(c)?);
        }
        // adapter <scenario> <engine> <candidate>: source file for effort accounting
        Some("adapter") => {
            let f = match a[2].as_str() {
                "cells" => cells::adapter_file(&a[3], &a[4]),
                "oltp" => oltp::adapter_files(&a[3], &a[4]),
                "analytics" => analytics::adapter_files(&a[3], &a[4]),
                "mixed" => mixed::adapter_files(&a[3], &a[4]),
                "grid" => grid::adapter_files(&a[3], &a[4]),
                s => return Err(format!("no adapters for {s}").into()),
            };
            println!("{f}");
        }
        Some("tile-bufsweep") => cells::tile_proto::buffer_sweep(&PathBuf::from(&a[2]))?,
        Some("probe") => println!("{}", common::probe(&PathBuf::from(&a[2]))?),
        Some("codec-probe") => mixed::codec_probe()?,
        Some("follow-cells") => cells::follower_main(&a[2], &PathBuf::from(&a[3]), a[4].parse()?)?,
        Some("follow-mixed") => {
            mixed::follower_main(&a[2], &PathBuf::from(&a[3]), &a[4], a[5].parse()?)?
        }
        _ => return Err("usage: bench <scenario> <format> <dir>".into()),
    }
    Ok(())
}
