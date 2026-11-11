mod common;
mod s2_grid;
mod s3_oltp;
mod s4_analytics;
mod s5_mixed;
mod s6_cells;

use common::Result;
use std::path::PathBuf;

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        // bench <scenario> <format> <fresh empty dir>
        Some("bench") => {
            let dir = PathBuf::from(&a[4]);
            let fmt_full = a[3].clone();
            let fmt = fmt_full.strip_suffix("+trusted").unwrap_or(&fmt_full).to_string();
            if fmt != fmt_full {
                // SAFETY: single-threaded at this point; followers inherit it.
                unsafe { std::env::set_var("FB_TRUSTED", "1") };
            }
            let a = [a[0].clone(), a[1].clone(), a[2].clone(), fmt, a[4].clone()];
            std::fs::create_dir_all(&dir)?;
            let mut m = match a[2].as_str() {
                "grid" => s2_grid::run(&a[3], &dir)?,
                "oltp" => s3_oltp::run(&a[3], &dir)?,
                "analytics" => s4_analytics::run(&a[3], &dir)?,
                "mixed" => s5_mixed::run(&a[3], &dir)?,
                "cells" => s6_cells::run(&a[3], &dir)?,
                s => return Err(format!("unknown scenario {s}").into()),
            };
            m.set("scenario", a[2].as_str());
            m.set("format", fmt_full.as_str());
            m.print();
        }
        Some("codec-probe") => s5_mixed::codec_probe()?,
        Some("reuse-probe") => s5_mixed::reuse_probe()?,
        Some("follow-cells") => s6_cells::follower_main(&a[2], &PathBuf::from(&a[3]), a[4].parse()?)?,
        Some("follow-mixed") => s5_mixed::follower_main(&a[2], &PathBuf::from(&a[3]), &a[4], a[5].parse()?)?,
        _ => return Err("usage: bench <scenario> <format> <dir>".into()),
    }
    Ok(())
}
