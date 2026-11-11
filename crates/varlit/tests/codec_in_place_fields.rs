//! Variable-block fields are encoded in place (header, value, patched length).
//!
//! The derived encoder used to stage every field in its own `Vec` through
//! `Encoder::encode_nested_to_vec` and copy it in with `write_field`. These
//! tests pin the in-place path to the exact bytes of that construction and
//! check that an over-limit record is still refused with the typed limit.

use varlit::__core::{Encoder, VarlitEncode, write_field};
use varlit::{Endian, Error, VarlitBlock, decode_from_slice, encode_to_vec};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "variable")]
struct Mixed {
    #[varlit(field_id = 1)]
    w: u64,
    #[varlit(field_id = 2)]
    samples: Vec<u16>,
    #[varlit(field_id = 3)]
    flags: Vec<bool>,
    #[varlit(field_id = 4)]
    note: String,
    #[varlit(field_id = 9)]
    small: [i8; 10],
    #[varlit(field_id = 40)]
    maybe: Option<Vec<f32>>,
    #[varlit(field_id = 41)]
    empty: Vec<u64>,
}

fn sample() -> Mixed {
    Mixed {
        w: 0x0102_0304_0506_0708,
        samples: (0..1000u16).map(|v| v.wrapping_mul(7919)).collect(),
        flags: (0..33).map(|v| v % 3 == 0).collect(),
        note: "in-place".repeat(5),
        small: std::array::from_fn(|i| i as i8 - 5),
        maybe: Some(vec![1.5, f32::NAN, -0.0]),
        empty: Vec::new(),
    }
}

/// The construction the derive used before: one staged buffer per field.
fn staged(value: &Mixed, endian: Endian) -> Vec<u8> {
    fn field<T: VarlitEncode>(encoder: &mut Encoder, id: u32, value: &T) {
        let payload = encoder.encode_nested_to_vec(value).unwrap();
        write_field(encoder, id, T::WIRE_TYPE, &payload).unwrap();
    }
    let mut encoder = Encoder::new(endian);
    field(&mut encoder, 1, &value.w);
    field(&mut encoder, 2, &value.samples);
    field(&mut encoder, 3, &value.flags);
    field(&mut encoder, 4, &value.note);
    field(&mut encoder, 9, &value.small);
    field(&mut encoder, 40, &value.maybe);
    field(&mut encoder, 41, &value.empty);
    encoder.into_inner()
}

#[test]
fn in_place_fields_match_the_staged_field_bytes() {
    for endian in [Endian::Little, Endian::Big] {
        for value in [
            sample(),
            Mixed {
                maybe: None,
                ..sample()
            },
        ] {
            let bytes = encode_to_vec(&value, endian).unwrap();
            assert_eq!(bytes, staged(&value, endian), "{endian:?}");
            let decoded: Mixed = decode_from_slice(&bytes, endian).unwrap();
            assert_eq!(decoded.samples, value.samples);
            assert_eq!(decoded.note, value.note);
            assert_eq!(decoded.small, value.small);
            assert_eq!(
                decoded
                    .maybe
                    .map(|v| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>()),
                value
                    .maybe
                    .map(|v| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>())
            );
        }
    }
}

varlit::varlit_format! {
    struct LimitFormat {
        magic: b"INPLACE1";
        version: 1;
        endian: little;
        blocks: [Mixed];
    }
}

#[test]
fn an_over_limit_record_is_still_refused_with_the_typed_limit() {
    let value = sample();
    let full = encode_to_vec(&value, Endian::Little).unwrap().len() as u64;
    let dir = tempfile::tempdir().unwrap();
    for (n, limit) in [16, 40, full / 2, full - 1, full].into_iter().enumerate() {
        let spec = LimitFormat::spec().with_resource_limits(
            varlit::ResourceLimits::MISSING.with_max_logical_payload_len(limit),
        );
        let mut file = varlit::VarlitFile::create(spec, dir.path().join(format!("f{n}"))).unwrap();
        match file.push_info(&value) {
            Ok(_) => assert_eq!(limit, full, "only the exact size fits"),
            Err(Error::LimitExceeded {
                limit: reported,
                actual,
                ..
            }) => {
                assert!(limit < full);
                assert_eq!(reported, limit);
                assert!(actual > limit, "actual {actual} must exceed {limit}");
            }
            Err(other) => panic!("limit {limit}: expected LimitExceeded, got {other:?}"),
        }
    }
}
