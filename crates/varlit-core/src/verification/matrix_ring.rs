//! Write-ahead ring arithmetic for tiled matrices: which stored headers are
//! believed, and that every offset computed from a believed header and every
//! frame a scan steps over stays inside the ring record. Checksums and I/O
//! are outside these harnesses.
use super::{
    ENTRY_HEADER, FRAME_HEADER, FRAMES_AT, FramePlan, HEADER_SLOT, MAX_RING_BYTES, MIN_RING, Ring,
    RingHeader, overlap, stream_entry,
};

fn any_ring() -> Ring {
    let base: u64 = kani::any();
    let len: u64 = kani::any();
    // What `Ring::within` admits: a format-bounded length, and an end that
    // is an offset in a file.
    kani::assume((MIN_RING..=MAX_RING_BYTES).contains(&len));
    kani::assume(base.checked_add(len).is_some());
    Ring { base, len }
}

/// A stored header is believed exactly when its generations are ordered as
/// a writer keeps them, and then the next generation and the next header
/// sequence are representable: nothing that adds one to them overflows.
#[kani::proof]
fn believed_ring_headers_have_ordered_generations_and_room_for_the_next() {
    let fields: [u64; 8] = kani::any();
    let ordered = fields[3] <= fields[1]
        && fields[1] <= fields[2]
        && fields[2] <= fields[7]
        && fields[7] != u64::MAX
        && fields[0] != u64::MAX;
    match RingHeader::from_fields(fields) {
        Some(header) => {
            assert!(ordered);
            assert!(header.reclaimed <= header.checkpointed);
            assert!(header.checkpointed <= header.mark);
            assert!(header.mark <= header.published);
            assert!(header.checkpointed.checked_add(1).is_some());
            assert!(header.published.checked_add(1).is_some());
            assert!(header.seq.checked_add(1).is_some());
            assert_eq!(header.seq, fields[0]);
            assert_eq!(header.tail, fields[4]);
            assert_eq!(header.tail_gen, fields[5]);
            assert_eq!(header.head, fields[6]);
        }
        None => assert!(!ordered),
    }
}

/// Offsets of a header that fits its ring address bytes of the frame area,
/// never the header page or anything past the record, and the space between
/// them is less than the area.
#[kani::proof]
fn offsets_of_a_fitting_header_stay_inside_the_ring_record() {
    let ring = any_ring();
    let header = RingHeader {
        tail: kani::any(),
        head: kani::any(),
        seq: kani::any(),
        ..RingHeader::default()
    };
    let area = ring.area();
    assert_eq!(
        u128::from(area),
        u128::from(ring.len) - u128::from(FRAMES_AT)
    );
    let end = u128::from(ring.base) + u128::from(ring.len);
    if header.fits(area) {
        for at in [header.tail, header.head] {
            assert_eq!(at % 8, 0);
            let logical = u128::from(ring.base) + u128::from(FRAMES_AT) + u128::from(at);
            assert_eq!(u128::from(ring.at(at)), logical);
            assert!(logical < end);
        }
        assert!(ring.used(header.tail, header.head) < area);
    } else {
        assert!(
            header.tail >= area
                || header.head >= area
                || header.tail % 8 != 0
                || header.head % 8 != 0
        );
    }
    // Both header slots lie in the header page, before the frames.
    let slot = u128::from(ring.slot_for(header.seq));
    assert!(
        slot == u128::from(ring.base) || slot == u128::from(ring.base) + u128::from(HEADER_SLOT)
    );
    assert!(slot + u128::from(HEADER_SLOT) <= u128::from(ring.base) + u128::from(FRAMES_AT));
}

/// A scan steps over a frame only when the whole frame lies inside the frame
/// area, and the offset it continues from is again inside the area and on a
/// word boundary. By induction from a fitting header, every frame a scan
/// reads is inside the ring record.
#[kani::proof]
fn a_stepped_frame_lies_inside_the_ring_and_the_next_offset_is_valid() {
    let ring = any_ring();
    let at: u64 = kani::any();
    let len: u64 = kani::any();
    let area = ring.area();
    let fits = at < area
        && len >= FRAME_HEADER + 4
        && len % 8 == 0
        && u128::from(at) + u128::from(len) <= u128::from(area);
    match ring.after_frame(at, len) {
        Some(next) => {
            assert!(fits);
            let last =
                u128::from(ring.base) + u128::from(FRAMES_AT) + u128::from(at) + u128::from(len);
            assert!(last <= u128::from(ring.base) + u128::from(ring.len));
            assert_eq!(u128::from(ring.at(at)) + u128::from(len), last);
            assert!(next < area);
            if at % 8 == 0 {
                assert_eq!(next % 8, 0);
            }
            assert!(next == 0 || u128::from(next) == u128::from(at) + u128::from(len));
        }
        None => assert!(!fits),
    }
}

/// Whether ring offset `at` is inside the frames kept from `tail` up to
/// `cursor`.
fn kept(tail: u64, cursor: u64, at: u64) -> bool {
    if tail <= cursor {
        tail <= at && at < cursor
    } else {
        at >= tail || at < cursor
    }
}

/// Wherever a writer decides to put its next frame, the frame lies inside
/// the frame area, covers no byte of a kept frame, and leaves the cursor
/// different from the tail, so a full ring is never mistaken for an empty
/// one. A frame is refused as too large only when an empty ring could not
/// hold it, and a checkpoint is asked for only when frames are kept.
#[kani::proof]
fn a_placed_frame_never_overwrites_a_kept_frame() {
    let ring = any_ring();
    let area = ring.area();
    let tail: u64 = kani::any();
    let cursor: u64 = kani::any();
    let len: u64 = kani::any();
    kani::assume(tail < area && cursor < area && len != 0);
    // Any byte of the placed frame.
    let offset: u64 = kani::any();
    kani::assume(offset < len);
    let placed_at = match ring.plan_frame(tail, cursor, len) {
        FramePlan::AtCursor => Some(cursor),
        FramePlan::AtStart => {
            // Only when frames are kept between a tail past the start and
            // the cursor: the start of the ring is then free.
            assert!(0 < tail && tail < cursor);
            Some(0)
        }
        FramePlan::Restart => {
            assert_eq!(tail, cursor);
            // From an empty ring at offset 0 the frame fits at once.
            assert!(matches!(ring.plan_frame(0, 0, len), FramePlan::AtCursor));
            None
        }
        FramePlan::Checkpoint => {
            assert!(tail != cursor);
            None
        }
        FramePlan::TooLarge => {
            assert_eq!(tail, cursor);
            assert!(len >= area);
            None
        }
    };
    if let Some(at) = placed_at {
        let end = u128::from(at) + u128::from(len);
        assert!(end <= u128::from(area));
        assert!(!kept(tail, cursor, at + offset));
        let next = if at + len == area { 0 } else { at + len };
        assert!(next < area);
        assert!(next != tail);
    }
}

/// The room a checkpoint plans for is the room placement then finds: once
/// the frames before `tail` are gone, a frame no longer than the larger of
/// the two rooms is placed without another checkpoint.
#[kani::proof]
fn room_promised_by_a_checkpoint_is_room_placement_finds() {
    let ring = any_ring();
    let area = ring.area();
    let tail: u64 = kani::any();
    let cursor: u64 = kani::any();
    let len: u64 = kani::any();
    kani::assume(tail < area && cursor < area && len != 0);
    let (at_cursor, at_start) = ring.room(tail, cursor);
    assert!(at_cursor < area && at_start < area);
    if len <= at_cursor.max(at_start) {
        assert!(matches!(
            ring.plan_frame(tail, cursor, len),
            FramePlan::AtCursor | FramePlan::AtStart | FramePlan::Restart
        ));
    }
}

/// An entry is taken from a frame body only when all its bytes lie inside
/// what is left of the body after its header: a scan streaming the entries
/// reads nothing outside the frame and always advances.
#[kani::proof]
fn a_streamed_entry_lies_inside_the_frame_body() {
    let header: [u8; ENTRY_HEADER as usize] = kani::any();
    let remaining: u64 = kani::any();
    let len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    match stream_entry(&header, remaining) {
        Some((target, n)) => {
            assert_eq!(n, len);
            assert!(u64::from(n) <= remaining);
            let stored = u64::from_le_bytes([
                header[0], header[1], header[2], header[3], header[4], header[5], header[6],
                header[7],
            ]);
            assert_eq!(target, stored);
        }
        None => assert!(u64::from(len) > remaining),
    }
}

/// The writer reads what it staged through the bytes it still buffers: for
/// any read and any buffered range, the overlap it copies lies inside both,
/// names the same file offsets in each, and is exactly their intersection,
/// so every byte the buffer holds for the read is served from it and no
/// other byte is.
#[kani::proof]
fn a_read_through_the_frame_buffer_copies_exactly_the_overlap() {
    let offset: u64 = kani::any();
    let len: usize = kani::any();
    let held_at: u64 = kani::any();
    let held: usize = kani::any();
    let fits =
        offset.checked_add(len as u64).is_some() && held_at.checked_add(held as u64).is_some();
    let byte: u64 = kani::any();
    let in_read = byte >= offset && byte - offset < len as u64;
    let in_held = byte >= held_at && byte - held_at < held as u64;
    match overlap(offset, len, held_at, held) {
        Some((dst, src, n)) => {
            assert!(fits && n > 0);
            assert!(dst + n <= len && src + n <= held);
            assert_eq!(offset + dst as u64, held_at + src as u64);
            let start = offset + dst as u64;
            let covered = byte >= start && byte - start < n as u64;
            assert_eq!(covered, in_read && in_held);
        }
        None => assert!(!fits || !(in_read && in_held)),
    }
}
