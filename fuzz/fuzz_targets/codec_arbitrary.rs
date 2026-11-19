#![no_main]

use std::collections::{BTreeMap, HashMap};

use libfuzzer_sys::fuzz_target;
use varlit::{Endian, decode_from_slice};
use varlit_fuzz::{FuzzPoint, FuzzRecord};

fuzz_target!(|data: &[u8]| {
    let Some((&selector, payload)) = data.split_first() else {
        return;
    };

    match selector % 16 {
        0 => drop(decode_from_slice::<FuzzPoint>(payload, Endian::Little)),
        1 => drop(decode_from_slice::<FuzzRecord>(payload, Endian::Little)),
        2 => drop(decode_from_slice::<Vec<u8>>(payload, Endian::Little)),
        3 => drop(decode_from_slice::<String>(payload, Endian::Little)),
        4 => drop(decode_from_slice::<Vec<i64>>(payload, Endian::Little)),
        5 => drop(decode_from_slice::<Vec<String>>(payload, Endian::Little)),
        6 => drop(decode_from_slice::<BTreeMap<u64, Vec<u8>>>(
            payload,
            Endian::Little,
        )),
        7 => drop(decode_from_slice::<HashMap<String, u64>>(
            payload,
            Endian::Little,
        )),
        8 => drop(decode_from_slice::<BTreeMap<(), ()>>(
            payload,
            Endian::Little,
        )),
        9 => drop(decode_from_slice::<HashMap<(), ()>>(
            payload,
            Endian::Little,
        )),
        10 => drop(decode_from_slice::<Option<(u64, String, Vec<u8>)>>(
            payload,
            Endian::Little,
        )),
        11 => drop(decode_from_slice::<(u128, i128, f64, bool)>(
            payload,
            Endian::Little,
        )),
        // `zerocopy::byteorder` numbers decode as the plain numbers they hold.
        12 => {
            use varlit::zerocopy::byteorder::{BigEndian, LittleEndian, U32};
            let wrapped =
                decode_from_slice::<(U32<LittleEndian>, U32<BigEndian>)>(payload, Endian::Little);
            let plain = decode_from_slice::<(u32, u32)>(payload, Endian::Little);
            match (wrapped, plain) {
                (Ok((a, b)), Ok((x, y))) => assert_eq!((a.get(), b.get()), (x, y)),
                (Err(_), Err(_)) => {}
                _ => panic!("byteorder and plain decodes disagree on acceptance"),
            }
        }
        13 => {
            use varlit::zerocopy::byteorder::{BigEndian, F64, I32, I64, LittleEndian, U16};
            let wrapped = decode_from_slice::<(
                I64<BigEndian>,
                F64<LittleEndian>,
                U16<LittleEndian>,
                I32<BigEndian>,
            )>(payload, Endian::Big);
            let plain = decode_from_slice::<(i64, f64, u16, i32)>(payload, Endian::Big);
            match (wrapped, plain) {
                (Ok((a, b, c, d)), Ok((w, x, y, z))) => {
                    assert_eq!((a.get(), c.get(), d.get()), (w, y, z));
                    assert_eq!(b.get().to_bits(), x.to_bits());
                }
                (Err(_), Err(_)) => {}
                _ => panic!("byteorder and plain decodes disagree on acceptance"),
            }
        }
        14 => {
            use varlit::zerocopy::byteorder::{LittleEndian, U32};
            let wrapped = decode_from_slice::<[U32<LittleEndian>; 4]>(payload, Endian::Little);
            let plain = decode_from_slice::<[u32; 4]>(payload, Endian::Little);
            match (wrapped, plain) {
                (Ok(a), Ok(b)) => assert!(a.iter().zip(&b).all(|(x, y)| x.get() == *y)),
                (Err(_), Err(_)) => {}
                _ => panic!("byteorder and plain decodes disagree on acceptance"),
            }
        }
        _ => {
            if let Ok(chunks) = varlit::ChunkedBytes::from_encoded(payload.to_vec()) {
                drop(chunks.decode_to_vec_limited(4 * 1024 * 1024));
                // A length policy keeps selected-chunk work finite per input;
                // the output buffer remains tiny even for a large chunk.
                if chunks.decoded_len() <= 4 * 1024 * 1024 {
                    let end = chunks.decoded_len();
                    let start = end / 3;
                    let mut next = start;
                    let result = chunks.stream_range(
                        start..end,
                        &mut [0; 257],
                        4 * 1024 * 1024,
                        |at, bytes| {
                            assert_eq!(at, next);
                            next += bytes.len() as u64;
                            assert!(next <= end);
                            Ok(())
                        },
                    );
                    if result.is_ok() {
                        assert_eq!(next, end);
                    }
                }
            }
        }
    }
});
