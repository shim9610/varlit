//! Executable policy domains shared by the lifecycle and read-route runners.
//! validation_coverage compares these variant names with the Rust declarations,
//! including non_exhaustive enums. Values and coverage metadata have one owner.
use varlit::*;

pub struct Domain {
    pub name: &'static str,
    pub variants: &'static [&'static str],
    pub check_values: fn(),
}

macro_rules! domains {
    ($($ty:ident => $values:ident { $($variant:ident $(($payload:pat))? => $value:expr),+ $(,)? };)+) => {
        $(pub const $values: &[$ty] = &[$($value),+];)+
        pub const DOMAINS: &[Domain] = &[$(Domain {
            name: stringify!($ty), variants: &[$(stringify!($variant)),+],
            check_values: || {
                #[allow(unreachable_patterns)] // Also handles non_exhaustive public enums.
                let classify = |value: $ty| match value {
                    $($ty::$variant $(($payload))? => stringify!($variant),)+
                    _ => panic!("unregistered {} value", stringify!($ty)),
                };
                $(assert_eq!(classify($value), stringify!($variant), "case label must describe its actual value");)+
            },
        }),+];
    };
}

domains! {
    Endian => ENDIANS { Little => Endian::Little, Big => Endian::Big };
    IntegrityPolicy => INTEGRITY {
        None => IntegrityPolicy::None, Crc32 => IntegrityPolicy::Crc32,
        Crc32WithHeader => IntegrityPolicy::Crc32WithHeader,
    };
    IntegrityVerification => VERIFICATION {
        Missing => IntegrityVerification::Missing, AtOpen => IntegrityVerification::AtOpen,
        OnDemand => IntegrityVerification::OnDemand,
    };
    ManifestPolicy => MANIFESTS { None => ManifestPolicy::None, Embedded => ManifestPolicy::Embedded };
    CommitPolicy => COMMIT_SHAPES {
        None => CommitPolicy::None, RecordFooter => CommitPolicy::RecordFooter,
        TransactionMarker(_) => CommitPolicy::TransactionMarker(TransactionMarkerMode::OnFlush),
    };
    TransactionMarkerMode => TRANSACTIONS {
        OnFlush => TransactionMarkerMode::OnFlush, Explicit => TransactionMarkerMode::Explicit,
    };
    CompressionPolicy => COMPRESSION_SHAPES {
        None => CompressionPolicy::None,
        VariableBlocks(_) => CompressionPolicy::VariableBlocks(VariableCompression::zstd(CompressionHeaderMode::RecordExplicit)),
    };
    CompressionAlgorithm => ALGORITHMS { Zstd => CompressionAlgorithm::Zstd };
    CompressionHeaderMode => HEADERS {
        RecordExplicit => CompressionHeaderMode::RecordExplicit,
        FileExplicit => CompressionHeaderMode::FileExplicit,
        FormatContract => CompressionHeaderMode::FormatContract,
    };
    CompressionLevel => LEVELS {
        Fast => CompressionLevel::Fast, Default => CompressionLevel::Default,
        Best => CompressionLevel::Best, Exact(_) => CompressionLevel::Exact(2),
    };
}

pub fn commits() -> Vec<CommitPolicy> {
    COMMIT_SHAPES
        .iter()
        .flat_map(|&policy| match policy {
            CommitPolicy::None | CommitPolicy::RecordFooter => vec![policy],
            CommitPolicy::TransactionMarker(_) => TRANSACTIONS
                .iter()
                .copied()
                .map(CommitPolicy::TransactionMarker)
                .collect(),
        })
        .collect()
}

pub fn compression() -> Vec<CompressionPolicy> {
    let mut cases = Vec::new();
    for shape in COMPRESSION_SHAPES {
        match shape {
            CompressionPolicy::None => cases.push(CompressionPolicy::None),
            CompressionPolicy::VariableBlocks(_) => {
                for &algorithm in ALGORITHMS {
                    for &header_mode in HEADERS {
                        for &level in LEVELS {
                            cases.push(CompressionPolicy::VariableBlocks(VariableCompression {
                                algorithm,
                                header_mode,
                                level,
                                only_if_smaller: false,
                                ..VariableCompression::zstd(header_mode)
                            }));
                        }
                    }
                }
            }
        }
    }
    cases
}

pub fn checksum_policies() -> impl Iterator<Item = IntegrityPolicy> {
    INTEGRITY.iter().copied().filter(|policy| match policy {
        IntegrityPolicy::None => false, // Healthy/no-CRC behaviour is covered by the lifecycle runner.
        IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader => true,
    })
}
