//! Write-ahead ring arithmetic for tiled matrices: which stored headers are
//! believed, and that every offset computed from a believed header and every
//! frame a scan steps over stays inside the ring record. Checksums and I/O
//! are outside these harnesses.
use super::{FRAME_HEADER, FRAMES_AT, HEADER_SLOT, MAX_RING_BYTES, MIN_RING, Ring, RingHeader};

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
