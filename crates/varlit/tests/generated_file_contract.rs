//! Successful writer outputs remain readable across compatible read policies.
//! This is an end-to-end regression, not a proof over arbitrary files.
use varlit::{
    CommitPolicy, Endian, IntegrityPolicy, ReadLimits, ReadValidation, TransactionMarkerMode,
    VarlitBlock, VarlitFile, varlit_format,
};

#[derive(Clone, Debug, VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "fixed")]
struct Scalars {
    unsigned: u64,
    signed: i64,
    float: f64,
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 2, version = 1, kind = "variable")]
struct Capture {
    #[varlit(field_id = 1)]
    epoch: u64,
    #[varlit(field_id = 63)]
    words: Vec<u64>,
    #[varlit(field_id = 64)]
    label: String,
    #[varlit(field_id = 65)]
    optional: Option<(i64, Vec<u8>)>,
    #[varlit(field_id = 4294967295)]
    tail: [u8; 4],
}

varlit_format! {
    struct Format {
        magic: b"GENFILES";
        version: 1;
        endian: little;
        schema_hash: computed;
        blocks: [Scalars, Capture];
    }
}

fn values(epoch: usize) -> (Scalars, Capture) {
    const UNSIGNED: [u64; 6] = [0, 1, u32::MAX as u64, 1 << 63, u64::MAX - 1, u64::MAX];
    const SIGNED: [i64; 6] = [i64::MIN, -1, 0, 1, i32::MAX as i64, i64::MAX];
    // Include negative zero, a subnormal, infinity and a NaN with payload bits.
    const FLOAT: [u64; 6] = [
        0,
        1 << 63,
        1,
        0x7ff0_0000_0000_0000,
        0x7ff8_1234_5678_9abc,
        u64::MAX,
    ];
    let index = epoch % UNSIGNED.len();
    (
        Scalars {
            unsigned: UNSIGNED[index],
            signed: SIGNED[index],
            float: f64::from_bits(FLOAT[index]),
        },
        Capture {
            epoch: epoch as u64,
            words: (0..64)
                .map(|i| UNSIGNED[(i + epoch) % UNSIGNED.len()])
                .collect(),
            label: if index == 0 {
                String::new()
            } else {
                "\0measurements/\u{1f321}/\u{03bb}".repeat(index)
            },
            optional: (epoch % 2 == 1).then(|| (SIGNED[index], (0..=255).collect())),
            tail: [0, 127, 128, 255],
        },
    )
}

fn assert_values(reader: &VarlitFile, count: usize) -> varlit::Result<()> {
    let scalars = reader.blocks::<Scalars>()?;
    let captures = reader.blocks::<Capture>()?;
    assert_eq!(scalars.len(), count);
    assert_eq!(captures.len(), count);
    for epoch in 0..count {
        let (expected_scalar, expected_capture) = values(epoch);
        let actual = filled(|o| scalars.get(epoch, o))?.expect("published scalar");
        assert_eq!(actual.unsigned, expected_scalar.unsigned);
        assert_eq!(actual.signed, expected_scalar.signed);
        assert_eq!(actual.float.to_bits(), expected_scalar.float.to_bits());
        assert_eq!(filled(|o| captures.get(epoch, o))?, Some(expected_capture));
    }
    Ok(())
}

#[test]
fn successful_native_files_keep_values_across_reopen_and_read_policies() -> varlit::Result<()> {
    let mut integrities = vec![IntegrityPolicy::None];
    if cfg!(feature = "integrity") {
        integrities.extend([IntegrityPolicy::Crc32, IntegrityPolicy::Crc32WithHeader]);
    }
    for endian in [Endian::Little, Endian::Big] {
        for &integrity in &integrities {
            for commit in [
                CommitPolicy::None,
                CommitPolicy::RecordFooter,
                CommitPolicy::TransactionMarker(TransactionMarkerMode::OnFlush),
            ] {
                let directory = tempfile::tempdir()?;
                let path = directory.path().join("generated.varlit");
                let mut spec = Format::spec()
                    .with_integrity_policy(integrity)
                    .with_commit_policy(commit);
                spec.endian = endian;
                let spec = spec.with_computed_schema_hash();
                {
                    let mut writer = VarlitFile::create(spec, &path)?;
                    for epoch in 0..6 {
                        let (scalar, capture) = values(epoch);
                        writer.push(&scalar)?;
                        writer.push(&capture)?;
                    }
                    writer.flush()?;
                    writer.sync()?;
                }
                // Resume a completed file, then publish another bundle.
                {
                    let mut writer = VarlitFile::open(spec, &path)?;
                    let (scalar, capture) = values(6);
                    writer.push(&scalar)?;
                    writer.push(&capture)?;
                    writer.flush()?;
                    writer.sync()?;
                }
                let written = std::fs::read(&path)?;
                spec.inspect_layout_file(&path)?;
                let mut policies = vec![ReadValidation::TRUSTED, ReadValidation::STRUCTURE_ONLY];
                if integrity != IntegrityPolicy::None {
                    policies.extend([ReadValidation::CRC_ONLY, ReadValidation::FULL]);
                }
                for policy in policies {
                    let read_spec = spec
                        .with_resource_limits(ReadLimits::STANDARD.with_read_validation(policy));
                    assert_eq!(
                        spec.computed_schema_hash(),
                        read_spec.computed_schema_hash()
                    );
                    let reader = VarlitFile::open_readonly(read_spec, &path)?;
                    assert_values(&reader, 7)?;
                    let scanned = reader.scan().collect::<varlit::Result<Vec<_>>>()?;
                    assert!(scanned.len() >= 14);
                    let checksummed = if integrity == IntegrityPolicy::None {
                        0
                    } else {
                        reader.index_entries().iter().count()
                    };
                    assert_eq!(reader.verify_all()?, checksummed);
                }
                assert_eq!(std::fs::read(&path)?, written);
            }
        }
    }
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
