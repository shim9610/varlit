//! Decoding into a value the caller already owns.
//!
//! An in-place decode must leave exactly the value a fresh decode returns,
//! whatever the destination held before, must reuse the destination's heap
//! buffers where the shapes allow it, and must charge and refuse the same
//! materialization budget as a fresh decode.

use varlit::__core::Decoder;
use varlit::{
    Endian, Error, VarlitBlock, VarlitEmpty, decode_from_slice, decode_from_slice_into,
    encode_to_vec,
};

#[derive(Clone, Copy, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 70, version = 1, kind = "fixed")]
struct Sample {
    t: u64,
    a: [i8; 10],
    f: [bool; 3],
    x: [u16; 500],
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 71, version = 1, kind = "variable")]
struct Window {
    #[varlit(field_id = 1)]
    w: u64,
    #[varlit(field_id = 2)]
    samples: Vec<u16>,
    #[varlit(field_id = 3)]
    note: String,
    #[varlit(field_id = 4)]
    names: Vec<String>,
    #[varlit(field_id = 5)]
    maybe: Option<Vec<f32>>,
    #[varlit(field_id = 6)]
    records: Vec<Sample>,
    #[varlit(field_id = 7)]
    raw: Vec<u8>,
    #[varlit(field_id = 8, default)]
    added: Vec<u32>,
}

fn sample(i: u64) -> Sample {
    Sample {
        t: i * 10_000,
        a: std::array::from_fn(|j| (i as i8).wrapping_add(j as i8)),
        f: std::array::from_fn(|j| (i + j as u64).is_multiple_of(2)),
        x: std::array::from_fn(|j| (i as u16).wrapping_mul(31).wrapping_add(j as u16)),
    }
}

fn window(n: usize) -> Window {
    Window {
        w: n as u64,
        samples: (0..n as u16 * 7).collect(),
        note: "n".repeat(n),
        names: (0..n).map(|i| format!("ch{i}")).collect(),
        maybe: (n % 2 == 1).then(|| (0..n).map(|v| v as f32 * 0.5).collect()),
        records: (0..n as u64).map(sample).collect(),
        raw: vec![n as u8; n * 3],
        added: vec![n as u32; n],
    }
}

/// Prior destination contents: empty, shorter, longer and unrelated.
fn priors() -> Vec<Window> {
    vec![
        Window::empty(),
        window(0),
        window(1),
        window(4),
        window(40),
        Window {
            maybe: Some(vec![9.0; 100]),
            names: vec!["x".repeat(500); 3],
            ..window(2)
        },
    ]
}

#[test]
fn an_in_place_decode_equals_a_fresh_decode_whatever_the_destination_held() {
    for endian in [Endian::Little, Endian::Big] {
        for n in [0, 1, 3, 4, 17] {
            let value = window(n);
            let bytes = encode_to_vec(&value, endian).unwrap();
            let fresh: Window = decode_from_slice(&bytes, endian).unwrap();
            assert_eq!(fresh, value);
            for mut out in priors() {
                decode_from_slice_into(&bytes, endian, &mut out).unwrap();
                assert_eq!(out, fresh, "{endian:?} n={n}");
            }
        }
    }
}

#[test]
fn a_field_missing_from_the_bytes_is_reset_to_its_default() {
    // Encoded without field 8: a destination that held `added` must not keep it.
    #[derive(Clone, Debug, PartialEq, VarlitBlock)]
    #[varlit(id = 71, version = 1, kind = "variable")]
    struct OldWindow {
        #[varlit(field_id = 1)]
        w: u64,
        #[varlit(field_id = 2)]
        samples: Vec<u16>,
        #[varlit(field_id = 3)]
        note: String,
        #[varlit(field_id = 4)]
        names: Vec<String>,
        #[varlit(field_id = 5)]
        maybe: Option<Vec<f32>>,
        #[varlit(field_id = 6)]
        records: Vec<Sample>,
        #[varlit(field_id = 7)]
        raw: Vec<u8>,
    }
    let old = OldWindow {
        w: 3,
        samples: vec![1, 2],
        note: "old".into(),
        names: vec![],
        maybe: None,
        records: vec![],
        raw: vec![],
    };
    let bytes = encode_to_vec(&old, Endian::Little).unwrap();
    let fresh: Window = decode_from_slice(&bytes, Endian::Little).unwrap();
    assert!(fresh.added.is_empty());
    let mut out = window(9);
    decode_from_slice_into(&bytes, Endian::Little, &mut out).unwrap();
    assert_eq!(out, fresh);
}

#[test]
fn a_required_field_missing_from_the_bytes_is_still_refused() {
    #[derive(Clone, Debug, PartialEq, VarlitBlock)]
    #[varlit(id = 71, version = 1, kind = "variable")]
    struct OnlyW {
        #[varlit(field_id = 1)]
        w: u64,
    }
    let bytes = encode_to_vec(&OnlyW { w: 1 }, Endian::Little).unwrap();
    assert!(matches!(
        decode_from_slice::<Window>(&bytes, Endian::Little),
        Err(Error::MissingField { .. })
    ));
    let mut out = window(3);
    assert!(matches!(
        decode_from_slice_into(&bytes, Endian::Little, &mut out),
        Err(Error::MissingField { .. })
    ));
}

#[test]
fn heap_buffers_of_the_destination_are_reused() {
    let big = window(40);
    let small = window(7);
    let bytes = encode_to_vec(&small, Endian::Little).unwrap();
    let mut out = big.clone();
    let samples = out.samples.as_ptr();
    let note = out.note.as_ptr();
    let names = out.names.as_ptr();
    let name0 = out.names[0].as_ptr();
    let records = out.records.as_ptr();
    let raw = out.raw.as_ptr();
    decode_from_slice_into(&bytes, Endian::Little, &mut out).unwrap();
    assert_eq!(out, small);
    assert_eq!(out.samples.as_ptr(), samples);
    assert_eq!(out.note.as_ptr(), note);
    assert_eq!(out.names.as_ptr(), names);
    assert_eq!(out.names[0].as_ptr(), name0);
    assert_eq!(out.records.as_ptr(), records);
    assert_eq!(out.raw.as_ptr(), raw);

    // A steady stream of same-shaped records never reallocates.
    let mut out = Window::empty();
    decode_from_slice_into(&bytes, Endian::Little, &mut out).unwrap();
    let samples = out.samples.as_ptr();
    for _ in 0..10 {
        decode_from_slice_into(&bytes, Endian::Little, &mut out).unwrap();
        assert_eq!(out.samples.as_ptr(), samples);
    }
}

#[test]
fn fixed_blocks_decode_in_place() {
    for endian in [Endian::Little, Endian::Big] {
        let value = sample(5);
        let bytes = encode_to_vec(&value, endian).unwrap();
        let mut out = sample(99);
        decode_from_slice_into(&bytes, endian, &mut out).unwrap();
        assert_eq!(out, value);
        let mut out = Sample::empty();
        decode_from_slice_into(&bytes, endian, &mut out).unwrap();
        assert_eq!(out, value);
    }
}

#[test]
fn the_in_place_path_charges_the_same_materialization_budget() {
    let value = window(17);
    let bytes = encode_to_vec(&value, Endian::Little).unwrap();
    // Find the smallest budget a fresh decode accepts.
    let mut lo = 0u64;
    let mut hi = 1 << 24;
    while lo < hi {
        let mid = (lo + hi) / 2;
        if Decoder::decode_from_slice_limited::<Window>(&bytes, Endian::Little, mid).is_ok() {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let need = lo;
    assert!(need > 0);
    for prior in priors() {
        let mut out = prior.clone();
        let mut d = Decoder::new_limited(&bytes, Endian::Little, need - 1);
        assert!(
            matches!(
                d.decode_value_into(&mut out),
                Err(Error::LimitExceeded { .. })
            ),
            "in-place decode must refuse one byte under the fresh budget"
        );
        let mut out = prior;
        let mut d = Decoder::new_limited(&bytes, Endian::Little, need);
        d.decode_value_into(&mut out).unwrap();
        assert_eq!(out, value);
        assert_eq!(d.materialization_remaining(), 0);
    }
}

#[test]
fn malformed_input_is_refused_like_a_fresh_decode() {
    let bytes = encode_to_vec(&window(4), Endian::Little).unwrap();
    // Every truncation gives the same outcome in both paths: the same error,
    // or (a cut just before the defaulted last field) the same value.
    let mut refused = 0;
    for cut in 0..bytes.len() {
        let fresh = decode_from_slice::<Window>(&bytes[..cut], Endian::Little);
        let mut out = window(2);
        let into = decode_from_slice_into(&bytes[..cut], Endian::Little, &mut out);
        match (fresh, into) {
            (Ok(fresh), Ok(())) => assert_eq!(out, fresh, "cut {cut}"),
            (Err(fresh), Err(into)) => {
                refused += 1;
                assert_eq!(format!("{fresh:?}"), format!("{into:?}"), "cut {cut}")
            }
            (fresh, into) => panic!("cut {cut}: fresh {fresh:?} vs into {into:?}"),
        }
    }
    assert!(refused >= bytes.len() - 1);
    // Invalid UTF-8 in a String field.
    let s = encode_to_vec(&"abc".to_string(), Endian::Little).unwrap();
    let mut bad = s.clone();
    *bad.last_mut().unwrap() = 0xff;
    let mut out = String::from("keep me");
    assert!(matches!(
        decode_from_slice_into(&bad, Endian::Little, &mut out),
        Err(Error::InvalidUtf8)
    ));
    // Trailing bytes.
    let mut long = s;
    long.push(0);
    let mut out = String::new();
    assert!(matches!(
        decode_from_slice_into(&long, Endian::Little, &mut out),
        Err(Error::TrailingBytes { .. })
    ));
}

#[test]
fn record_sequences_and_options_decode_in_place() {
    for endian in [Endian::Little, Endian::Big] {
        let records: Vec<Sample> = (0..9).map(sample).collect();
        let bytes = encode_to_vec(&records, endian).unwrap();
        for mut out in [Vec::new(), vec![sample(1); 30], vec![sample(2)]] {
            decode_from_slice_into(&bytes, endian, &mut out).unwrap();
            assert_eq!(out, records);
        }
        for value in [None, Some(vec![1u64, 2, 3])] {
            let bytes = encode_to_vec(&value, endian).unwrap();
            for mut out in [None, Some(vec![7u64; 50])] {
                decode_from_slice_into(&bytes, endian, &mut out).unwrap();
                assert_eq!(out, value);
            }
        }
    }
}
