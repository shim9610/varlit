//! Multi-gigabyte mixed read/write benchmark.
//!
//! One writer appends a mixed stream to a single file until it reaches the
//! target size, while a reader thread follows the file and reads back what has
//! just been committed. The file is then reopened cold and read in every way
//! the API offers. Usage:
//!
//! ```text
//! cargo run --release -p varlit --features integrity --example mixed_load_bench -- <GiB> [dir]
//! ```
//!
//! The stream per batch:
//! - 2048 fixed `Tick` records (16 bytes each),
//! - 64 keyed `Event` records of 64 B..4 KiB; every 8th rewrites an older key
//!   and every 32nd deletes one,
//! - one `Blob` of 64 KiB..8 MiB, large enough to be stored in chunks.
//!
//! Every phase reports wall time, throughput, the process peak RSS during that
//! phase (`VmHWM`, reset per phase through `/proc/self/clear_refs`) and the
//! number of allocator calls the whole process made during it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use varlit::{FormatSpec, VarlitBlock, VarlitEmpty, VarlitFile, varlit_format};

struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

// SAFETY: every call is forwarded unchanged to the system allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "fixed")]
struct Tick {
    at: u64,
    value: u64,
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 2, version = 1, kind = "variable", key = "id")]
struct Event {
    #[varlit(field_id = 1)]
    id: u64,
    #[varlit(field_id = 2)]
    round: u64,
    #[varlit(field_id = 3)]
    tag: String,
    #[varlit(field_id = 4)]
    body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 3, version = 1, kind = "variable")]
struct Blob {
    #[varlit(field_id = 1)]
    id: u64,
    #[varlit(field_id = 2)]
    bytes: Vec<u8>,
}

varlit_format! {
    pub struct MixedFormat {
        magic: b"VMIXLOAD";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        recovery: truncate_tail;
        index: [scan_on_open, block_offset_chain];
        commit: transaction_marker(on_flush);
        blocks: [Tick, Event, Blob];
    }
}

const TICKS_PER_BATCH: u64 = 2048;
const EVENTS_PER_BATCH: u64 = 64;
const SYNC_EVERY: u64 = 64;

/// The default spec is anchored every 4 MiB, so a multi-gigabyte file opens
/// from its last anchor instead of scanning the whole log.
fn spec() -> FormatSpec {
    MixedFormat::spec()
}

/// A cheap deterministic mixer, so every value can be recomputed by a reader.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}

fn blob_len(id: u64) -> usize {
    (64 << 10) + (mix(id) % ((8 << 20) - (64 << 10))) as usize
}

fn blob_byte(id: u64, at: usize) -> u8 {
    (at as u64 ^ id.wrapping_mul(0x9e37)) as u8
}

fn fill_blob(blob: &mut Blob, id: u64) {
    let len = blob_len(id);
    blob.id = id;
    blob.bytes.clear();
    blob.bytes.extend((0..len).map(|at| blob_byte(id, at)));
}

fn fill_event(event: &mut Event, id: u64, round: u64) {
    let len = 64 + (mix(id ^ (round << 40)) % 4032) as usize;
    event.id = id;
    event.round = round;
    event.tag.clear();
    // Formatted in place so the benchmark itself allocates nothing per record.
    let _ = write!(event.tag, "event-{id}");
    event.body.clear();
    event
        .body
        .extend((0..len).map(|at| (at as u64 ^ id ^ round) as u8));
}

fn check_blob(blob: &Blob) {
    assert_eq!(
        blob.bytes.len(),
        blob_len(blob.id),
        "blob {} length",
        blob.id
    );
    for at in [0, blob.bytes.len() / 2, blob.bytes.len() - 1] {
        assert_eq!(
            blob.bytes[at],
            blob_byte(blob.id, at),
            "blob {} byte",
            blob.id
        );
    }
}

fn status_kib(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with(field))
                .and_then(|line| {
                    line.split_whitespace()
                        .nth(1)
                        .and_then(|kib| kib.parse().ok())
                })
        })
        .unwrap_or(0)
}

fn reset_peak() {
    // `5` resets VmHWM to the current RSS (Linux 4.0+).
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

fn drop_page_cache() -> bool {
    let _ = std::process::Command::new("sync").status();
    std::fs::write("/proc/sys/vm/drop_caches", "3").is_ok()
}

struct Phase {
    name: &'static str,
    started: Instant,
    allocations: u64,
}

impl Phase {
    fn start(name: &'static str) -> Self {
        reset_peak();
        Self {
            name,
            started: Instant::now(),
            allocations: ALLOCATIONS.load(Ordering::Relaxed),
        }
    }

    fn finish(self, bytes: u64, detail: &str) -> Duration {
        let elapsed = self.started.elapsed();
        let allocations = ALLOCATIONS.load(Ordering::Relaxed) - self.allocations;
        let mib = bytes as f64 / (1 << 20) as f64;
        println!(
            "{:<28} {:>9.2} s | {:>9.1} MiB/s | peak RSS {:>8.1} MiB | allocs {:>9} | {}",
            self.name,
            elapsed.as_secs_f64(),
            mib / elapsed.as_secs_f64().max(1e-9),
            status_kib("VmHWM:") as f64 / 1024.0,
            allocations,
            detail,
        );
        elapsed
    }
}

fn percentiles(samples: &mut [Duration]) -> String {
    if samples.is_empty() {
        return "no samples".to_string();
    }
    samples.sort_unstable();
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize].as_secs_f64() * 1e3;
    format!(
        "p50 {:.3} ms, p99 {:.3} ms, max {:.3} ms",
        at(0.5),
        at(0.99),
        at(1.0)
    )
}

#[derive(Default)]
struct WriteTotals {
    batches: u64,
    ticks: u64,
    events: u64,
    rewrites: u64,
    deletes: u64,
    blobs: u64,
    payload_bytes: u64,
}

fn write_phase(path: &Path, target: u64, done: &AtomicBool, committed: &AtomicU64) -> WriteTotals {
    let mut file = spec().create(path).expect("create");
    let mut totals = WriteTotals::default();
    let mut event = Event::empty();
    let mut blob = Blob::empty();
    let mut next_event = 0u64;
    loop {
        let batch = totals.batches;
        for n in 0..TICKS_PER_BATCH {
            let at = batch * TICKS_PER_BATCH + n;
            file.push(&Tick { at, value: mix(at) }).expect("push tick");
        }
        totals.ticks += TICKS_PER_BATCH;
        totals.payload_bytes += TICKS_PER_BATCH * 16;
        for n in 0..EVENTS_PER_BATCH {
            let pick = mix(batch * EVENTS_PER_BATCH + n);
            if next_event > 0 && n % 32 == 31 {
                file.delete::<Event>(&(pick % next_event)).expect("delete");
                totals.deletes += 1;
                continue;
            }
            let (id, round) = if next_event > 0 && n % 8 == 7 {
                totals.rewrites += 1;
                (pick % next_event, batch)
            } else {
                next_event += 1;
                (next_event - 1, 0)
            };
            fill_event(&mut event, id, round);
            file.push(&event).expect("push event");
            totals.events += 1;
            totals.payload_bytes += event.body.len() as u64 + event.tag.len() as u64 + 16;
        }
        fill_blob(&mut blob, batch);
        file.push(&blob).expect("push blob");
        totals.blobs += 1;
        totals.payload_bytes += blob.bytes.len() as u64;
        if batch % SYNC_EVERY == SYNC_EVERY - 1 {
            file.sync().expect("sync");
        } else {
            file.flush().expect("flush");
        }
        totals.batches += 1;
        committed.store(totals.batches, Ordering::Release);
        if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) >= target {
            break;
        }
    }
    file.sync().expect("final sync");
    done.store(true, Ordering::Release);
    totals
}

#[derive(Default)]
struct FollowTotals {
    polls: u64,
    blobs_read: u64,
    events_read: u64,
    bytes_read: u64,
    latencies: Vec<Duration>,
}

/// Follows the writer and reads back the newest blob and some events each time
/// new data becomes visible.
fn follow_phase(path: &Path, done: &AtomicBool, committed: &AtomicU64) -> FollowTotals {
    while committed.load(Ordering::Acquire) == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut reader = VarlitFile::open_readonly(spec(), path).expect("reader open");
    let mut totals = FollowTotals {
        latencies: Vec::with_capacity(1 << 16),
        ..FollowTotals::default()
    };
    let mut blob_entries = Vec::new();
    let mut event_entries = Vec::new();
    let mut scratch = Vec::new();
    let mut blob = Blob::empty();
    let mut event = Event::empty();
    let mut seen_blobs = 0usize;
    loop {
        let finished = done.load(Ordering::Acquire);
        reader.follow().expect("follow");
        totals.polls += 1;
        reader
            .block_entries_into::<Blob>(&mut blob_entries)
            .expect("blob entries");
        if blob_entries.len() > seen_blobs {
            let started = Instant::now();
            let entry = &blob_entries[blob_entries.len() - 1];
            reader
                .decode_block_into(entry, &mut scratch, &mut blob)
                .expect("decode blob");
            check_blob(&blob);
            totals.blobs_read += 1;
            totals.bytes_read += blob.bytes.len() as u64;
            reader
                .block_entries_into::<Event>(&mut event_entries)
                .expect("event entries");
            for k in 0..8u64 {
                let at = (mix(totals.polls ^ k) % event_entries.len() as u64) as usize;
                reader
                    .decode_block_into(&event_entries[at], &mut scratch, &mut event)
                    .expect("decode event");
                totals.events_read += 1;
                totals.bytes_read += event.body.len() as u64;
            }
            if totals.latencies.len() < totals.latencies.capacity() {
                totals.latencies.push(started.elapsed());
            }
            seen_blobs = blob_entries.len();
        } else if !finished {
            std::thread::sleep(Duration::from_millis(2));
        }
        if finished && blob_entries.len() == seen_blobs {
            return totals;
        }
    }
}

fn main() -> varlit::Result<()> {
    let gib: f64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(5.5);
    let directory = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = directory.join(format!("mixed_load_bench_{}.varlit", std::process::id()));
    let target = (gib * (1u64 << 30) as f64) as u64;
    println!("mixed load bench: target {gib} GiB at {}", path.display());

    // Phase 1: one writer and one following reader at the same time.
    let done = AtomicBool::new(false);
    let committed = AtomicU64::new(0);
    let phase = Phase::start("write + concurrent follow");
    let (written, mut followed) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| follow_phase(&path, &done, &committed));
        let written = write_phase(&path, target, &done, &committed);
        (written, reader.join().expect("reader thread"))
    });
    let file_bytes = std::fs::metadata(&path)?.len();
    phase.finish(
        file_bytes,
        &format!(
            "{} batches, {} ticks, {} events ({} rewrites), {} deletes, {} blobs, {:.2} GiB payload",
            written.batches,
            written.ticks,
            written.events,
            written.rewrites,
            written.deletes,
            written.blobs,
            written.payload_bytes as f64 / (1u64 << 30) as f64,
        ),
    );
    println!(
        "{:<28} follow polls {}, read {} blobs + {} events ({:.1} MiB) while writing; per poll {}",
        "",
        followed.polls,
        followed.blobs_read,
        followed.events_read,
        followed.bytes_read as f64 / (1 << 20) as f64,
        percentiles(&mut followed.latencies),
    );
    println!(
        "file size {:.3} GiB",
        file_bytes as f64 / (1u64 << 30) as f64
    );

    let cold = drop_page_cache();
    println!(
        "page cache {}",
        if cold {
            "dropped before reopen (cold reads)"
        } else {
            "could not be dropped (warm reads)"
        }
    );

    // Phase 2: reopen.
    let phase = Phase::start("reopen");
    let reader = VarlitFile::open_readonly(spec(), &path)?;
    phase.finish(0, "open_readonly on the full file");

    let mut blob_entries = Vec::new();
    let mut event_entries = Vec::new();
    let phase = Phase::start("directory walks");
    reader.block_entries_into::<Blob>(&mut blob_entries)?;
    reader.block_entries_into::<Event>(&mut event_entries)?;
    phase.finish(
        0,
        &format!(
            "{} blob entries, {} event entries",
            blob_entries.len(),
            event_entries.len()
        ),
    );

    // Phase 3: random whole-record decodes into reused values.
    let mut scratch = Vec::new();
    let mut blob = Blob::empty();
    let mut samples = Vec::with_capacity(512);
    let mut bytes = 0u64;
    let phase = Phase::start("random blob decode x400");
    for k in 0..400u64 {
        let entry = &blob_entries[(mix(k) % blob_entries.len() as u64) as usize];
        let started = Instant::now();
        reader.decode_block_into(entry, &mut scratch, &mut blob)?;
        samples.push(started.elapsed());
        check_blob(&blob);
        bytes += blob.bytes.len() as u64;
    }
    phase.finish(bytes, &percentiles(&mut samples));

    // Phase 4: random streamed reads through one fixed caller buffer.
    let mut page_buffer = vec![0u8; 1 << 20];
    samples.clear();
    bytes = 0;
    let phase = Phase::start("random blob stream x400");
    for k in 0..400u64 {
        let entry = &blob_entries[(mix(k ^ 0xabc) % blob_entries.len() as u64) as usize];
        let started = Instant::now();
        let mut page = reader.read_record_into(entry, &mut page_buffer)?;
        loop {
            bytes += page.filled as u64;
            let Some(next) = page.next else { break };
            page = reader.read_record_from(next, &mut page_buffer)?;
        }
        samples.push(started.elapsed());
    }
    phase.finish(
        bytes,
        &format!("1 MiB buffer; {}", percentiles(&mut samples)),
    );

    // Phase 5: random small keyed reads.
    let mut event = Event::empty();
    let mut event_samples = Vec::with_capacity(20_000);
    bytes = 0;
    let phase = Phase::start("random event decode x20000");
    for k in 0..20_000u64 {
        let entry = &event_entries[(mix(k ^ 0x5eed) % event_entries.len() as u64) as usize];
        let started = Instant::now();
        reader.decode_block_into(entry, &mut scratch, &mut event)?;
        event_samples.push(started.elapsed());
        bytes += event.body.len() as u64;
    }
    phase.finish(bytes, &percentiles(&mut event_samples));

    // Phase 6: latest value per key, then point lookups through it.
    let mut tails: HashMap<u64, u64> = HashMap::new();
    let phase = Phase::start("key_tail_offsets_into");
    reader.key_tail_offsets_into::<Event>(&mut tails)?;
    phase.finish(0, &format!("{} keys (tombstones included)", tails.len()));
    let keys: Vec<u64> = tails.keys().copied().take(20_000).collect();
    event_samples.clear();
    bytes = 0;
    let mut deleted = 0u64;
    let phase = Phase::start("keyed point lookup x20000");
    for key in &keys {
        let started = Instant::now();
        let entry = reader.record_entry_at(tails[key])?;
        // A key's newest record is its tombstone once it has been deleted.
        if entry.block_id != 2 {
            deleted += 1;
            continue;
        }
        reader.decode_block_into(&entry, &mut scratch, &mut event)?;
        event_samples.push(started.elapsed());
        assert_eq!(event.id, *key);
        bytes += event.body.len() as u64;
    }
    phase.finish(
        bytes,
        &format!(
            "{deleted} keys end in a tombstone; {}",
            percentiles(&mut event_samples)
        ),
    );

    // Phase 7: every tick through a fixed page.
    let mut ticks = vec![Tick { at: 0, value: 0 }; 8192];
    let mut payload = Vec::new();
    let phase = Phase::start("paged tick pass");
    let mut page = reader.blocks::<Tick>(&mut payload, &mut ticks)?;
    let mut count = page.filled as u64;
    while let Some(next) = page.next {
        page = reader.blocks_from::<Tick>(next, &mut payload, &mut ticks)?;
        for tick in &ticks[..page.filled] {
            debug_assert_eq!(tick.value, mix(tick.at));
        }
        count += page.filled as u64;
    }
    assert_eq!(count, written.ticks);
    phase.finish(count * 16, &format!("{count} ticks, 8192 per page"));

    // Phase 8: a full scan and a full checksum pass.
    let phase = Phase::start("scan");
    let mut events = 0u64;
    for event in reader.scan() {
        event?;
        events += 1;
    }
    phase.finish(file_bytes, &format!("{events} records"));

    drop_page_cache();
    let phase = Phase::start("verify_all");
    let verified = reader.verify_all()?;
    phase.finish(file_bytes, &format!("{verified} records checksummed"));

    drop(reader);
    std::fs::remove_file(&path)?;
    let _ = std::fs::remove_file(path.with_extension("varlit.lock"));
    Ok(())
}
