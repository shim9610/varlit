//! Finite keys identify declared value combinations; offsets identify records.
use varlit::{
    Decoder, Encoder, Endian, VarlitBlock, VarlitDecode, VarlitEncode, VarlitKeyedBlock,
    varlit_format,
};

varlit_format! {
    pub format Finite {
        magic: b"FINITE"; version: 1; schema_hash: computed;
        index: keyed_offset_chain;
        blocks {
            fixed Frame(id = 1, key = [scan, frame],
                key_values = [Scan1Frame2 = (1, 2), Scan2Frame4 = (2, 4), Scan9Frame8 = (9, 8)]) {
                scan: u32, frame: u32, value: u64,
            }
            variable Channel(id = 2, key = [name, active],
                key_domain = [name = ["red", "blue"], active = [false, true]]) {
                name: String, active: bool, value: u64,
            }
            fixed Sample(id = 3, key = [scan, frame],
                key_domain = [scan = -2..=1, frame = 0..4]) {
                scan: i8, frame: u16, value: u64,
            }
            fixed Mixed(id = 4, key = [second, first], key_values = [Pair = (9, 7)]) {
                prefix: u8, first: u16, middle: u32, second: u16, suffix: u8,
            }
        }
    }
}

#[test]
fn actual_combinations_have_fixed_codes_and_two_byte_encoding() -> varlit::Result<()> {
    assert_eq!(FrameKey::COUNT, 3);
    assert_eq!(std::mem::size_of::<FrameKey>(), 2);
    assert_eq!(FrameKey::from_values((1, 2))?, FrameKey::Scan1Frame2);
    assert_eq!(FrameKey::Scan2Frame4.values(), (2, 4));
    assert!(FrameKey::from_values((1, 4)).is_err());
    assert!(FrameKey::from_code(3).is_err());
    assert!(FrameKey::from_code(u16::MAX).is_err());
    for endian in [Endian::Little, Endian::Big] {
        for key in FrameKey::ALL {
            let mut encoder = Encoder::new(endian);
            key.encode_varlit(&mut encoder)?;
            let bytes = encoder.into_inner();
            assert_eq!(bytes.len(), 2);
            assert_eq!(
                FrameKey::decode_varlit(&mut Decoder::new(&bytes, endian))?,
                *key
            );
        }
        assert!(FrameKey::decode_varlit(&mut Decoder::new(&[255, 255], endian)).is_err());
        assert!(FrameKey::decode_varlit(&mut Decoder::new(&[0], endian)).is_err());
    }
    let frame = Frame {
        key: FrameKey::Scan1Frame2,
        value: 17,
    };
    assert_eq!(frame.key(), FrameKey::Scan1Frame2);
    let mut encoder = Encoder::new(Endian::Little);
    frame.encode_varlit(&mut encoder)?;
    assert_eq!(encoder.into_inner().len(), 10);
    Ok(())
}

#[test]
fn cartesian_domains_include_each_value_combination_once() -> varlit::Result<()> {
    assert_eq!(ChannelKey::COUNT, 4);
    for key in ChannelKey::ALL {
        assert_eq!(ChannelKey::from_values(key.values())?, *key);
    }
    assert!(ChannelKey::from_values(("green", true)).is_err());
    assert_eq!(SampleKey::COUNT, 16);
    for scan in -2..=1 {
        for frame in 0..4 {
            let key = SampleKey::from_values((scan, frame))?;
            assert_eq!(key.values(), (scan, frame));
            assert_eq!(key.code(), ((scan + 2) as u16) * 4 + frame);
        }
    }
    assert!(SampleKey::from_values((2, 0)).is_err());
    Ok(())
}

mod changed {
    use varlit::varlit_format;
    varlit_format! {
        pub format Changed {
            magic: b"FINITE"; version: 1; schema_hash: computed;
            blocks {
                fixed Frame(id = 1, key = [scan, frame],
                    key_values = [Scan1Frame2 = (1, 3), Scan2Frame4 = (2, 4), Scan9Frame8 = (9, 8)]) {
                    scan: u32, frame: u32, value: u64,
                }
            }
        }
    }
}

#[test]
fn changed_value_mapping_changes_schema_identity() {
    assert_ne!(
        <FrameKey as VarlitEncode>::SCHEMA_ID,
        <changed::FrameKey as VarlitEncode>::SCHEMA_ID
    );
    assert_ne!(
        Frame::SCHEMA_FINGERPRINT,
        changed::Frame::SCHEMA_FINGERPRINT
    );
}

#[test]
fn combined_field_preserves_non_key_field_order_and_ids() -> varlit::Result<()> {
    let key = MixedKey::from_values((9, 7))?;
    let value = Mixed {
        prefix: 1,
        key,
        middle: 0x04030201,
        suffix: 2,
    };
    let mut encoder = Encoder::new(Endian::Little);
    value.encode_varlit(&mut encoder)?;
    let bytes = encoder.into_inner();
    assert_eq!(bytes, [1, 0, 0, 1, 2, 3, 4, 2]);
    assert_eq!(
        Mixed::decode_varlit(&mut Decoder::new(&bytes, Endian::Little))?,
        value
    );
    assert_eq!(
        Mixed::FIELDS.iter().map(|f| f.id).collect::<Vec<_>>(),
        [1, 2, 3, 5]
    );
    Ok(())
}

/// Runs a caller-owned-output read that always fills `out`, returning the value.
#[allow(dead_code)]
fn filled_out<T: varlit::VarlitEmpty, R>(
    read: impl FnOnce(&mut T) -> varlit::Result<R>,
) -> varlit::Result<T> {
    let mut out = T::empty();
    read(&mut out)?;
    Ok(out)
}

/// Runs a caller-owned-output read that reports presence; `None` when absent.
#[allow(dead_code)]
fn filled<T: varlit::VarlitEmpty>(
    read: impl FnOnce(&mut T) -> varlit::Result<bool>,
) -> varlit::Result<Option<T>> {
    let mut out = T::empty();
    Ok(read(&mut out)?.then_some(out))
}

/// Drains a streaming walk into an owned `Vec`, reusing one decode target.
#[allow(dead_code)]
fn collect_stream<T: varlit::VarlitBlock + varlit::VarlitEmpty + Clone>(
    mut stream: varlit::StreamingBlocks<T>,
) -> varlit::Result<Vec<T>> {
    let mut out = T::empty();
    let mut all = Vec::new();
    while stream.next_into(&mut out)? {
        all.push(out.clone());
    }
    Ok(all)
}
