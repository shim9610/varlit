//! `Vec<T>` of whole records as a block field (compound-array layout).
//!
//! An application can hand its records to a block as they are instead of
//! splitting them into one vector per field. Decoding must stay bounded: a
//! forged length is refused or bounded before any large reservation.

use varlit::__core::Decoder;
use varlit::{Endian, Error, VarlitBlock, decode_from_slice, encode_to_vec};

#[derive(Clone, Copy, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 50, version = 1, kind = "fixed")]
struct Sample {
    t: u64,
    a: [i8; 10],
    f: [bool; 3],
    x: [u16; 500],
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 51, version = 1, kind = "variable")]
struct Window {
    #[varlit(field_id = 1)]
    w: u64,
    #[varlit(field_id = 2)]
    records: Vec<Sample>,
}

fn sample(i: u64) -> Sample {
    Sample {
        t: i * 10_000,
        a: std::array::from_fn(|j| (i as i8).wrapping_add(j as i8)),
        f: std::array::from_fn(|j| (i + j as u64).is_multiple_of(2)),
        x: std::array::from_fn(|j| (i as u16).wrapping_mul(31).wrapping_add(j as u16)),
    }
}

#[test]
fn record_sequences_round_trip_on_both_endians() {
    for endian in [Endian::Little, Endian::Big] {
        for n in [0u64, 1, 3, 1000] {
            let window = Window {
                w: n,
                records: (0..n).map(sample).collect(),
            };
            let bytes = encode_to_vec(&window, endian).unwrap();
            let decoded: Window = decode_from_slice(&bytes, endian).unwrap();
            assert_eq!(decoded, window);
        }
    }
}

#[test]
fn a_record_is_stored_as_its_own_encoding_back_to_back() {
    let records: Vec<Sample> = (0..4).map(sample).collect();
    let bytes = encode_to_vec(&records, Endian::Little).unwrap();
    let mut expected = (records.len() as u64).to_le_bytes().to_vec();
    for record in &records {
        expected.extend(encode_to_vec(record, Endian::Little).unwrap());
    }
    assert_eq!(bytes, expected);
}

#[test]
fn forged_record_counts_are_refused_without_a_large_reservation() {
    let records: Vec<Sample> = (0..2).map(sample).collect();
    let mut bytes = encode_to_vec(&records, Endian::Little).unwrap();
    for forged in [u64::MAX, 1 << 40, 3] {
        bytes[..8].copy_from_slice(&forged.to_le_bytes());
        let result = decode_from_slice::<Vec<Sample>>(&bytes, Endian::Little);
        assert!(
            matches!(
                result,
                Err(Error::UnexpectedEof
                    | Error::LimitExceeded { .. }
                    | Error::LengthOverflow { .. }
                    | Error::InvalidCanonicalEncoding(_))
            ),
            "forged count {forged}: {result:?}"
        );
    }
}

#[test]
fn a_count_beyond_the_materialization_budget_is_refused_before_reserving() {
    let records: Vec<Sample> = (0..8).map(sample).collect();
    let bytes = encode_to_vec(&records, Endian::Little).unwrap();
    let need = (8 * size_of::<Sample>()) as u64;
    assert!(matches!(
        Decoder::decode_from_slice_limited::<Vec<Sample>>(&bytes, Endian::Little, need - 1),
        Err(Error::LimitExceeded { .. })
    ));
    let ok: Vec<Sample> =
        Decoder::decode_from_slice_limited(&bytes, Endian::Little, need + (1 << 20)).unwrap();
    assert_eq!(ok, records);
}
