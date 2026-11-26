//! Stream recovery under damage and arbitrary operation histories.
//!
//! A stream's durable boundary lives in the primary file: the end its last
//! sync published in the `VPUB` header slots, and the sync markers after it.
//! The damage target corrupts those slots and the unsynced tail and checks
//! that the confirmed prefix a reader sees never changes; the state machine
//! drives writers, readers and reopens through arbitrary histories.

use std::fs;
use std::path::Path;

use varlit::{BatchOptions, StreamOptions};

use crate::{FuzzStreamFormat, StreamEvent, StreamItem};

const MAX_BYTES: usize = 1_048_576;
const MAX_OPERATIONS: usize = 64;
// Verification admits 256 physical records, including the creation nonce and
// every sync marker. Keep generated state machines within that same ceiling.
const MAX_PHYSICAL_RECORDS: usize = 256;
/// The published-end block's two slots, after its framing and version word.
const SLOTS_AT: usize = 16;
const SLOTS_LEN: usize = 48;

#[cfg(test)]
mod tests {
    #[test]
    fn state_machine_counts_sync_markers_toward_the_record_ceiling() {
        let mut input = Vec::new();
        for _ in 0..32 {
            input.extend_from_slice(&[7, 0, 0, 7]);
            input.extend_from_slice(&[4, 0, 0, 0]);
        }
        super::run_state_machine(&input);
    }

    #[test]
    fn damaged_published_slots_never_change_the_confirmed_prefix() {
        for control in [0x01, 0x41, 0x81, 0xC1] {
            for seed in [0, 1, 3, 7] {
                super::run_damage(&[control, seed, 0, 0, 0xFF, 0, 24, 0x10]);
            }
        }
    }

    #[test]
    fn a_damaged_unsynced_tail_is_cut_and_never_read() {
        for seed in [1, 5, 9] {
            super::run_damage(&[0x02, seed, 0, 3, 0x55, 0, 40, 0xAA]);
        }
    }
}

#[derive(Clone, Default)]
struct Model {
    records: u64,
    /// Sync markers: a sync appends one when records were appended since
    /// the previous marker.
    markers: u64,
    unmarked: bool,
}

impl Model {
    fn appended(&mut self, records: u64) {
        self.records += records;
        self.unmarked |= records > 0;
    }

    fn synced(&mut self) {
        if self.unmarked {
            self.markers += 1;
            self.unmarked = false;
        }
    }

    /// What a full verification counts: the file's own leading record, every
    /// committed record and every sync marker.
    fn verified_records(&self) -> u64 {
        self.records + self.markers + 1
    }
}

/// Corrupts a synced stream file and checks what survives.
///
/// The first byte picks where the damage lands: `0` anywhere in the file
/// (only typed errors are required), `1` the published-end slots, `2` the
/// unsynced tail after the last sync. Bit `0x40` leaves an unsynced tail in
/// the fixture, bit `0x80` damages nothing. The second byte seeds the
/// fixture; the rest are `(offset high, offset low, xor)` triples.
pub fn run_damage(data: &[u8]) {
    let Ok(root) = tempfile::tempdir() else {
        return;
    };
    let control = data.first().copied().unwrap_or(0);
    let seed = data.get(1).copied().unwrap_or(0);
    let region = control % 4;
    let unsynced = region == 2 || control & 0x40 != 0;
    let mutations = data.get(2..).unwrap_or_default();

    let path = root.path().join("damage-stream.varlit");
    let (committed, synced_len) = create_stream_fixture(&path, seed, unsynced);
    let Some(synced_len) = synced_len else {
        return;
    };
    let Ok(mut bytes) = fs::read(&path) else {
        return;
    };
    bytes.truncate(MAX_BYTES);
    let range = match region {
        1 => published_slots(&bytes),
        2 => Some(
            usize::try_from(synced_len)
                .unwrap_or(usize::MAX)
                .min(bytes.len())..bytes.len(),
        ),
        _ => Some(0..bytes.len()),
    };
    let Some(range) = range.filter(|range| !range.is_empty()) else {
        return;
    };
    let before = bytes[range.clone()].to_vec();
    if control & 0x80 == 0 {
        damage(&mut bytes[range.clone()], mutations);
    }
    if fs::write(&path, &bytes).is_err() {
        return;
    }
    // Only damage confined to what the confirmed prefix does not depend on
    // must leave it unchanged; anywhere else a typed error is enough. Slot
    // damage is such damage while it only tears: a changed slot that still
    // passes its checksum is a forged end, and a file whose both slots are
    // gone and that has no marker to verify from keeps every record that
    // frames, which an unsynced tail is part of.
    let strict = match region {
        1 => !forged_slot(&before, &bytes[range]) && (committed.markers > 0 || !unsynced),
        2 => true,
        _ => false,
    };
    probe_stream(&path, &committed, !strict);
    let reopened = FuzzStreamFormat::open_stream_writer(&path, stream_options());
    assert!(
        !strict || reopened.is_ok(),
        "recovery refused a file whose confirmed prefix is intact"
    );
    drop(reopened);
    probe_stream(&path, &committed, !strict);
    if strict {
        // Recovery cut the unsynced tail, damaged or not.
        assert_eq!(fs::metadata(&path).map(|m| m.len()).ok(), Some(synced_len));
    }
}

pub fn run_state_machine(data: &[u8]) {
    let Ok(root) = tempfile::tempdir() else {
        return;
    };
    let operations: Vec<[u8; 4]> = data
        .chunks(4)
        .take(MAX_OPERATIONS)
        .map(|chunk| {
            let mut operation = [0; 4];
            operation[..chunk.len()].copy_from_slice(chunk);
            operation
        })
        .collect();

    run_stream_machine(&root.path().join("machine-stream.varlit"), &operations);
}

/// The slots of the file's published-end block, found by its magic in the
/// header.
fn published_slots(bytes: &[u8]) -> Option<std::ops::Range<usize>> {
    let header = &bytes[..bytes.len().min(4096)];
    let block = header.windows(4).position(|window| window == b"VPUB")?;
    let start = block + SLOTS_AT;
    (start + SLOTS_LEN <= bytes.len()).then_some(start..start + SLOTS_LEN)
}

/// Whether a slot the damage changed still passes its checksum.
fn forged_slot(before: &[u8], after: &[u8]) -> bool {
    before
        .chunks(SLOTS_LEN / 2)
        .zip(after.chunks(SLOTS_LEN / 2))
        .any(|(old, new)| old != new && slot_checks(new))
}

/// The published-end slot checksum: FNV-1a over all but its last four bytes.
fn slot_checks(slot: &[u8]) -> bool {
    let (body, sum) = slot.split_at(slot.len() - 4);
    let mut hash = 2_166_136_261u32;
    for byte in body {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    sum == hash.to_le_bytes()
}

fn damage(bytes: &mut [u8], mutations: &[u8]) {
    if mutations.is_empty() {
        bytes[0] ^= 1;
        return;
    }
    for mutation in mutations.chunks(3).take(MAX_OPERATIONS) {
        let high = usize::from(*mutation.first().unwrap_or(&0));
        let low = usize::from(*mutation.get(1).unwrap_or(&0));
        let index = ((high << 8) | low) % bytes.len();
        bytes[index] ^= mutation.get(2).copied().unwrap_or(1) | 1;
    }
}

/// Writes a synced fixture, plus an unsynced tail when asked, and returns
/// what was committed and the file's length at the last sync.
fn create_stream_fixture(path: &Path, seed: u8, unsynced: bool) -> (Model, Option<u64>) {
    let mut committed = Model::default();
    let Ok(mut writer) = FuzzStreamFormat::create_stream_writer(path, stream_options()) else {
        return (committed, None);
    };

    let populated = seed & 1 != 0;
    if populated {
        let count = usize::from(seed % 8) + 2;
        for index in 0..count {
            if writer
                .push_stream_event(&StreamEvent {
                    value: index as u64,
                })
                .is_ok()
            {
                committed.appended(1);
            }
            let key = if seed & 2 == 0 {
                index as u64
            } else {
                (index % 2) as u64
            };
            if writer
                .push_stream_item(&StreamItem {
                    key,
                    value: (index as u64) ^ u64::from(seed),
                    payload: vec![seed; index % 17],
                })
                .is_ok()
            {
                committed.appended(1);
            }
            // Several syncs, so the chain has links before the last one.
            if seed & 8 != 0 && index % 3 == 0 {
                if writer.sync().is_err() {
                    return (Model::default(), None);
                }
                committed.synced();
            }
        }
        if seed & 4 != 0 && writer.delete_stream_item(&0).is_ok() {
            committed.appended(1);
        }
    }
    if writer.sync().is_err() {
        return (Model::default(), None);
    }
    committed.synced();
    let synced_len = fs::metadata(path).map(|metadata| metadata.len()).ok();

    if unsynced {
        let _ = writer.push_stream_event(&StreamEvent { value: u64::MAX });
        let _ = writer.push_stream_item(&StreamItem {
            key: 0,
            value: u64::MAX,
            payload: vec![0xDD; 8],
        });
        let _ = writer.flush();
    }
    (committed, synced_len)
}

fn run_stream_machine(path: &Path, operations: &[[u8; 4]]) {
    let writer = FuzzStreamFormat::create_stream_writer(path, stream_options())
        .expect("valid stream state-machine fixture");
    let mut writer = Some(writer);
    let mut working = Model::default();
    let mut committed = Model::default();

    for [opcode, key, value, count] in operations.iter().copied() {
        let physical_records = usize::try_from(working.verified_records()).unwrap_or(usize::MAX);
        if physical_records >= MAX_PHYSICAL_RECORDS {
            break;
        }
        let remaining_records = MAX_PHYSICAL_RECORDS - physical_records;
        if writer.is_none() {
            // Opening recovers: every acknowledged sync is kept, the rest cut.
            writer = Some(
                FuzzStreamFormat::open_stream_writer(path, stream_options())
                    .expect("valid stream recovery"),
            );
            working = committed.clone();
        }
        let Some(current) = writer.as_mut() else {
            continue;
        };
        match opcode % 8 {
            0 => {
                if current
                    .push_stream_event(&StreamEvent {
                        value: u64::from(value),
                    })
                    .is_ok()
                {
                    working.appended(1);
                }
            }
            1 => {
                if current
                    .push_stream_item(&StreamItem {
                        key: u64::from(key % 16),
                        value: u64::from(value),
                        payload: vec![value; usize::from(count % 32)],
                    })
                    .is_ok()
                {
                    working.appended(1);
                }
            }
            2 => {
                if current.delete_stream_item(&u64::from(key % 16)).is_ok() {
                    working.appended(1);
                }
            }
            3 => {
                let _ = current.flush();
            }
            4 => {
                if current.sync().is_ok() {
                    working.synced();
                    committed = working.clone();
                }
            }
            5 => {
                // A reader sees the acknowledged prefix while the writer
                // still holds unsynced records.
                probe_stream(path, &committed, false);
            }
            6 => {
                writer.take();
            }
            _ => {
                let amount = (usize::from(count % 8) + 1).min(remaining_records);
                let values = (0..amount).map(|offset| StreamEvent {
                    value: u64::from(value) + offset as u64,
                });
                if let Ok(report) = current.push_stream_events(values, batch_options()) {
                    working.appended(report.records);
                }
            }
        }
    }
    drop(writer);
    probe_stream(path, &committed, false);
    drop(
        FuzzStreamFormat::open_stream_writer(path, stream_options())
            .expect("valid stream writer reopen"),
    );
    probe_stream(path, &committed, false);
}

fn probe_stream(path: &Path, committed: &Model, permit_rejection: bool) {
    match FuzzStreamFormat::open_stream_reader(path, stream_options()) {
        Ok(reader) => {
            let verified = reader.verify_all();
            assert!(
                permit_rejection || verified.as_ref().ok() == Some(&committed.verified_records()),
                "the confirmed prefix changed: {verified:?}, expected {}",
                committed.verified_records()
            );
        }
        Err(_typed_error) => assert!(
            permit_rejection,
            "valid committed stream snapshot was rejected"
        ),
    }
}

fn stream_options() -> StreamOptions {
    StreamOptions::default()
}

const fn batch_options() -> BatchOptions {
    BatchOptions {
        max_records: 2,
        max_bytes: 64 * 1024,
    }
}
