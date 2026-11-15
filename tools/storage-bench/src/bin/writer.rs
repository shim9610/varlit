//! Writer-only phase followed by a checked sequential readback (outside phase).
use std::{path::Path, sync::mpsc, time::Instant};
use varlit_storage_bench::{
    Result, file_usage,
    model::{Config, Head, Oracle},
    native, thread_cpu,
};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args[1], "run");
    assert_eq!(args[2], "varlit-indexed");
    let root = Path::new(&args[3]);
    let c: Config = serde_json::from_slice(&std::fs::read(&args[4])?)?;
    // This diagnostic deliberately has no concurrent readers. Config::validate
    // owns the mixed-reader CLI contract and requires one; validate this role's
    // producer shape explicitly instead of changing the primary harness.
    assert_eq!(c.readers, 0);
    assert!(c.records > 0 && c.payload >= 8 && c.per_tx > 0);
    assert!((1..=64).contains(&c.producers) && c.records.is_multiple_of(c.producers));
    assert!(c.group_items > 0 && c.group_items * 128 == c.payload && !c.variable);
    std::fs::create_dir_all(root.join("data"))?;
    let path = root.join("data/store");
    let all = Instant::now();
    let cpu = thread_cpu();
    let init = Instant::now();
    let mut writer = native::Writer::create(&path, &c, true)?;
    writer.initial()?;
    let initialization = init.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut head = Head::default();
    let mut producer_cpu = 0.0;
    std::thread::scope(|scope| -> Result<()> {
        let (tx, rx) = mpsc::sync_channel(2);
        let mut workers = Vec::new();
        for source in 0..c.producers {
            let tx = tx.clone();
            let c = &c;
            workers.push(scope.spawn(move || -> Result<f64> {
                let cpu = thread_cpu();
                for local in 0..c.txns_per_producer() {
                    tx.send(c.transaction(source, local))?;
                }
                Ok(thread_cpu() - cpu)
            }));
        }
        drop(tx);
        for transaction in rx {
            writer.append(&transaction, &mut head, &c)?;
        }
        for worker in workers {
            producer_cpu += worker.join().map_err(|_| "producer panicked")??;
        }
        Ok(())
    })?;
    let write = t.elapsed().as_secs_f64();
    let t = Instant::now();
    drop(writer);
    let finalize = t.elapsed().as_secs_f64();
    let writer_cpu = thread_cpu() - cpu;
    assert_eq!(head.frames, c.records as u64);
    assert_eq!(head.txns, c.transactions() as u64);
    let readback = Instant::now();
    let cpu = thread_cpu();
    let mut reader = native::Reader::open(&path, &c, true)?;
    let h = reader.refresh()?;
    let mut oracle = Oracle::new(&c);
    reader.consume(&h, &mut oracle, &c)?;
    oracle.finish();
    let readback_cpu = thread_cpu() - cpu;
    let readback_seconds = readback.elapsed().as_secs_f64();
    let (logical, allocated) = file_usage(&root.join("data"))?;
    println!(
        "{}",
        serde_json::json!({"verified":true,"readers":[],"concurrent_readers":0,
        "records":c.records,"config":c,"initialization_seconds":initialization,
        "writer_seconds":write,"finalize_seconds":finalize,"writer_thread_cpu_seconds":writer_cpu,
        "producer_thread_cpu_seconds":producer_cpu,"readback_cpu_seconds":readback_cpu,
        "readback_seconds":readback_seconds,"seconds":all.elapsed().as_secs_f64(),
        "file_bytes":logical,"allocated_bytes":allocated})
    );
    Ok(())
}
