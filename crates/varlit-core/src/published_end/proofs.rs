//! Kani harnesses for the published end; see the module documentation.
//!
//! What is proved here is the pure part: slot encoding, the choice between
//! the two slots, and the length a reader is told. The choice is proved over
//! decoded slots, so the checksum's arithmetic stays out of the solver; that a
//! torn write fails its checksum is the checksum's assumption, not a proof.
//! That a writer moves the end only after the barrier of a durable
//! publication is a property of its call sites, held by
//! `a_follower_does_not_adopt_a_commit_whose_barrier_did_not_return`.
use super::*;

fn any_slot() -> Slot {
    Slot {
        end: kani::any(),
        seq: kani::any(),
        flags: kani::any(),
    }
}

/// Every slot decodes to what was encoded.
#[kani::proof]
#[kani::unwind(25)]
fn a_slot_round_trips() {
    let slot = any_slot();
    assert_eq!(decode_slot(&encode_slot(slot)), Some(slot));
}

/// A reader is never told more than the file holds, never more than the
/// published end (or the remembered bound, when the block is unreadable),
/// and never less than the start of the append log the file reaches.
#[kani::proof]
fn the_visible_length_is_bounded_by_the_file_and_the_published_end() {
    let file_len: u64 = kani::any();
    let floor: u64 = kani::any();
    let last: u64 = kani::any();
    let end: u64 = kani::any();
    let published = match kani::any::<u8>() % 3 {
        0 => Published::Unbounded,
        1 => Published::Bounded(end),
        _ => Published::Unreadable,
    };
    let (len, next) = visible_len(file_len, floor, published, last);
    assert!(len <= file_len);
    assert!(len >= file_len.min(floor));
    match published {
        Published::Unbounded => {
            assert_eq!(len, file_len);
            assert_eq!(next, last);
        }
        Published::Bounded(end) => {
            assert!(len <= end.max(floor));
            assert_eq!(next, end.max(floor));
        }
        Published::Unreadable => {
            assert!(len <= last.max(floor));
            assert_eq!(next, last.max(floor));
        }
    }
}

/// The writer's protocol: slot `seq % 2` gets each new end, with a larger
/// sequence and an end no smaller than the one before. Written whole, the
/// new end is chosen; torn (its checksum fails), the previous end is, which
/// is never more than the new one. A torn write can hide, never reveal.
#[kani::proof]
fn a_torn_slot_write_falls_back_to_the_previous_end() {
    let previous = Slot {
        end: kani::any(),
        seq: kani::any(),
        flags: ACTIVE,
    };
    let new_end: u64 = kani::any();
    kani::assume(previous.seq < u64::MAX);
    kani::assume(previous.end <= new_end);
    let written = Slot {
        end: new_end,
        seq: previous.seq + 1,
        flags: ACTIVE,
    };
    let new_first = slot_index(written.seq) == 0;
    let (whole, torn) = if new_first {
        (
            choose(Some(written), Some(previous), false),
            choose(None, Some(previous), false),
        )
    } else {
        (
            choose(Some(previous), Some(written), false),
            choose(Some(previous), None, false),
        )
    };
    assert_eq!(whole, Published::Bounded(new_end));
    assert_eq!(torn, Published::Bounded(previous.end));
}

/// Of two valid slots the one with the larger sequence decides; with
/// neither valid, blank slots do not bound readers and damaged ones are
/// unreadable, never a bound.
#[kani::proof]
fn the_newest_valid_slot_decides() {
    let first = any_slot();
    let second = any_slot();
    let newest = if second.seq > first.seq {
        second
    } else {
        first
    };
    let expected = if newest.flags & ACTIVE != 0 {
        Published::Bounded(newest.end)
    } else {
        Published::Unbounded
    };
    let blank: bool = kani::any();
    assert_eq!(choose(Some(first), Some(second), blank), expected);
    assert_eq!(choose(None, None, true), Published::Unbounded);
    assert_eq!(choose(None, None, false), Published::Unreadable);
}

/// Skipping the measurement changes nothing while the file has not shrunk:
/// whenever [`known_len`] answers, the file is at least as long as it was
/// when measured, and the answer is what [`visible_len`] gives for it.
#[kani::proof]
fn a_known_length_is_what_measuring_would_give() {
    let floor: u64 = kani::any();
    let seen: u64 = kani::any();
    let file_len: u64 = kani::any();
    let last: u64 = kani::any();
    let end: u64 = kani::any();
    kani::assume(file_len >= seen);
    let published = match kani::any::<u8>() % 3 {
        0 => Published::Unbounded,
        1 => Published::Bounded(end),
        _ => Published::Unreadable,
    };
    if let Some(len) = known_len(floor, published, seen) {
        assert!(len <= seen);
        assert_eq!(
            Some(len),
            Some(visible_len(file_len, floor, published, last).0)
        );
    }
}
