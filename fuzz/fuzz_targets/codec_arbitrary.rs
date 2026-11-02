#![no_main]

use std::collections::{BTreeMap, HashMap};

use libfuzzer_sys::fuzz_target;
use varlit::{Endian, decode_from_slice};
use varlit_fuzz::{FuzzPoint, FuzzRecord};

fuzz_target!(|data: &[u8]| {
    let Some((&selector, payload)) = data.split_first() else {
        return;
    };

    match selector % 13 {
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
        _ => {
            if let Ok(chunks) = varlit::ChunkedBytes::from_encoded(payload.to_vec()) {
                drop(chunks.decode_to_vec_limited(4 * 1024 * 1024));
                // A length policy keeps selected-chunk work finite per input;
                // the output buffer remains tiny even for a large chunk.
                if chunks.decoded_len() <= 4 * 1024 * 1024 {
                    let end = chunks.decoded_len();
                    let start = end / 3;
                    let mut next = start;
                    let result = chunks.stream_range(start..end, &mut [0; 257], 4 * 1024 * 1024, |at, bytes| {
                        assert_eq!(at, next);
                        next += bytes.len() as u64;
                        assert!(next <= end);
                        Ok(())
                    });
                    if result.is_ok() { assert_eq!(next, end); }
                }
            }
        }
    }
});
