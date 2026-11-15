//! Copy this pattern into tests/format_check.rs in an application project.
//! Run: cargo run --example check_format [-- existing-file.varlit]
use varlit::varlit_format;

varlit_format! {
    pub format AppFormat {
        magic: b"APP-CHECK"; version: 1; endian: little; schema_hash: computed;
        blocks {
            fixed Point(id = 1) { x: u64, y: i64 }
            variable Reading(id = 2, key = [station]) { station: u64, samples: Vec<u64> }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = std::env::args_os().nth(1) {
        let report = AppFormat::check_file(path)?;
        println!("{report:#?}");
        if !report.passed() {
            return Err("file check has violated or unverified conditions".into());
        }
        return Ok(());
    }
    let directory = tempfile::tempdir()?;
    let samples = AppFormat::self_test(directory.path().join("samples.varlit"))
        .with_block(Point {
            x: u64::MAX,
            y: i64::MIN,
        })
        .with_keyed_block(Reading {
            station: 42,
            samples: vec![0, 1, u64::MAX],
        })
        .cleanup(true)
        .run_strict();
    if !samples.passed() {
        eprintln!("{samples:#?}");
        return Err("format samples did not qualify".into());
    }
    let path = directory.path().join("output.varlit");
    {
        let mut writer = AppFormat::create(&path)?;
        writer.push(&Point { x: 7, y: -9 })?;
        writer.push_keyed(&Reading {
            station: 42,
            samples: vec![10, 20],
        })?;
        writer.sync()?;
    }
    let report = AppFormat::check_file(&path)?;
    println!("{report:#?}");
    if !report.passed() {
        return Err("file check has violated or unverified conditions".into());
    }
    Ok(())
}
