#![no_main]

use libfuzzer_sys::fuzz_target;
use varlit_fuzz::{FuzzNativeFormat, FuzzPoint, FuzzRecord, write_fuzz_file};

fuzz_target!(|data: &[u8]| {
    let Some(path) = write_fuzz_file("native", data) else {
        return;
    };

    {
        if let Ok(reader) = FuzzNativeFormat::open_reader(&path) {
            let reader = reader.into_inner();
            // One page of each block type, decoded into caller-owned slots.
            let mut payload = Vec::new();
            let mut points: Vec<FuzzPoint> = (0..8).map(|_| varlit::VarlitEmpty::empty()).collect();
            let _ = reader.blocks::<FuzzPoint>(&mut payload, &mut points);
            let mut records: Vec<FuzzRecord> =
                (0..8).map(|_| varlit::VarlitEmpty::empty()).collect();
            let _ = reader.blocks::<FuzzRecord>(&mut payload, &mut records);

            // SAFETY: this target owns the file and does not mutate it while
            // the mapping or any returned window is alive.
            if let Ok(mapped) = unsafe { reader.mmap_payloads() } {
                let mut first = vec![varlit::RecordIndexEntry::default(); 16];
                let filled = reader
                    .index_entries(&mut first)
                    .map_or(0, |page| page.filled);
                for entry in &first[..filled] {
                    let mut owned = Vec::new();
                    let result = reader.read_payload_into(entry, &mut owned);
                    // The fixture's per-record limit fits its materialisation
                    // budget. Once mapping succeeds, these stored-byte reads
                    // must agree on acceptance and bytes, including CRC errors.
                    // Repeat to exercise the successful-verification cache.
                    for _ in 0..2 {
                        let window = mapped.payload_window(entry);
                        assert_eq!(
                            result.is_ok(),
                            window.is_ok(),
                            "owned and mapped payload reads disagree on acceptance"
                        );
                        if let Ok(window) = window {
                            assert!(owned == window, "owned and mapped payload bytes differ");
                        }
                    }
                }
            }
        }
    }

    let _ = FuzzNativeFormat::diagnose_file(&path);
    let _ = FuzzNativeFormat::spec().open_recover_with_report(&path);
});
