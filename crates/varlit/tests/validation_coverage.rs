//! New source-level interfaces need executable valid/invalid contracts.
//! The frozen legacy inventory records the existing surface, not a claim of
//! complete historical coverage. New entries cannot join by refreshing it.
#![cfg(all(feature = "integrity", feature = "compression-zstd", feature = "mmap"))]

use quote::ToTokens;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use syn::visit::Visit;
#[allow(dead_code)]
#[path = "support/source_audit.rs"]
mod source_audit;
use source_audit::{Audit, Surface};
#[allow(dead_code)]
#[path = "support/validation_cases.rs"]
mod cases;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn inventory() -> BTreeMap<String, Surface> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args([
            "metadata",
            "--locked",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
        ])
        .current_dir(&root)
        .output()
        .expect("run Cargo metadata for coverage inventory");
    assert!(
        output.status.success(),
        "Cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let members: BTreeSet<_> = metadata["workspace_members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect();
    let mut surfaces = BTreeMap::new();
    for package in metadata["packages"].as_array().unwrap() {
        if !members.contains(package["id"].as_str().unwrap()) {
            continue;
        }
        let name = package["name"].as_str().unwrap();
        let Some(library) = package["targets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|target| {
                target["kind"].as_array().unwrap().iter().any(|kind| {
                    matches!(
                        kind.as_str(),
                        Some("lib" | "proc-macro" | "rlib" | "dylib" | "cdylib" | "staticlib")
                    )
                })
            })
        else {
            continue;
        };
        let entrypoint = Path::new(library["src_path"].as_str().unwrap());
        let prefix = match name {
            "varlit" => "facade",
            "varlit-core" => "core",
            "varlit-macros" => "macros",
            other => other,
        };
        let mut audit = Audit {
            root: entrypoint.parent().unwrap().into(),
            crate_name: prefix.into(),
            inventory_only: true,
            generator_crate: library["kind"]
                .as_array()
                .unwrap()
                .iter()
                .any(|kind| kind == "proc-macro"),
            ..Audit::default()
        };
        audit.inspect_file(entrypoint);
        assert!(audit.errors.is_empty(), "{}", audit.errors.join("\n"));
        // Empty libraries still own their Cargo features and will be inventoried
        // as soon as a declaration is added. No directory allowlist to maintain.
        surfaces.extend(audit.inventory);
        for (feature, dependencies) in package["features"].as_object().unwrap() {
            let mut dependencies: Vec<_> = dependencies
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            dependencies.sort_unstable();
            let signature =
                source_audit::fingerprint(&serde_json::to_string(&dependencies).unwrap());
            surfaces.insert(
                format!("feature/{name}/{feature}@{signature}"),
                Surface {
                    signature,
                    reference: feature.clone(),
                },
            );
        }
    }
    assert!(!surfaces.is_empty(), "empty workspace interface inventory");
    surfaces
}

struct Contract {
    symbol: &'static str,
    exercise: Option<&'static str>,
    valid_source: &'static str,
    invalid_source: &'static str,
    valid: fn() -> TestResult,
    invalid: fn() -> TestResult,
}

// A row contains runnable callbacks, not a test-name string. Both bodies are
// compiled and executed by the gate. The source check catches empty/stub rows;
// choosing meaningful assertions still requires review.
macro_rules! contract {
    (@exercise) => { None };
    (@exercise $name:literal) => { Some($name) };
    ($symbol:literal, $(generated $exercise:literal,)? valid $valid:block, invalid $invalid:block) => {
        Contract {
            symbol: $symbol,
            exercise: contract!(@exercise $($exercise)?),
            valid_source: stringify!($valid),
            invalid_source: stringify!($invalid),
            valid: || -> TestResult { $valid },
            invalid: || -> TestResult { $invalid },
        }
    };
}

#[derive(Debug, PartialEq, varlit::VarlitBlock)]
#[varlit(id = 1, version = 1, kind = "fixed")]
struct Probe {
    value: u64,
}

varlit::varlit_format! {
    struct ProbeFormat { magic: b"VCONTRACT"; version: 1; endian: little; integrity: crc32_with_header; blocks: [Probe]; }
}

fn probe(
    damaged: bool,
) -> Result<(tempfile::TempDir, varlit::VarlitFile, u64), Box<dyn std::error::Error>> {
    use std::io::{Seek, SeekFrom, Write};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("contract.varlit");
    let mut writer = ProbeFormat::spec().create(&path)?;
    let record = writer.push_info(&Probe { value: 19 })?;
    writer.sync()?;
    drop(writer);
    let reader = ProbeFormat::spec().open_readonly(&path)?;
    if damaged {
        let entry = reader.record_entry_at(record.record_offset)?;
        let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
        file.seek(SeekFrom::Start(entry.payload_offset))?;
        file.write_all(&[0x7f])?;
    }
    Ok((directory, reader, record.record_offset))
}

const CONTRACTS: &[Contract] = &[
    contract!("core/file.rs::file::VarlitFile::fn:read_block_at@8e571d1498d213bf",
        valid {
            let (_directory, reader, offset) = probe(false)?;
            assert_eq!(reader.read_block_at::<Probe>(offset)?, Probe { value: 19 });
            Ok(())
        },
        invalid {
            let (_directory, reader, offset) = probe(true)?;
            assert!(matches!(reader.read_block_at::<Probe>(offset), Err(varlit::Error::ChecksumMismatch { .. })));
            Ok(())
        }
    ),
];

#[derive(Default)]
struct Evidence {
    identifiers: BTreeSet<String>,
    calls: BTreeSet<String>,
    assertions: usize,
}

impl<'ast> Visit<'ast> for Evidence {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = &*call.func {
            self.calls
                .insert(path.path.segments.last().unwrap().ident.to_string());
        }
        syn::visit::visit_expr_call(self, call);
    }
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.calls.insert(call.method.to_string());
        syn::visit::visit_expr_method_call(self, call);
    }
    fn visit_member(&mut self, member: &'ast syn::Member) {
        if let syn::Member::Unnamed(index) = member {
            self.identifiers.insert(index.index.to_string());
        }
        syn::visit::visit_member(self, member);
    }
    fn visit_ident(&mut self, id: &'ast syn::Ident) {
        self.identifiers.insert(id.to_string());
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let name = mac.path.segments.last().unwrap().ident.to_string();
        self.identifiers.insert(name.clone());
        if matches!(
            name.as_str(),
            "assert" | "assert_eq" | "assert_ne" | "prop_assert" | "prop_assert_eq"
        ) {
            self.assertions += 1;
        }
        // Assertions contain the API expression itself. Parse normal macro
        // arguments where possible, retaining identifier tokens for DSLs.
        fn identifiers(tokens: impl ToTokens, out: &mut BTreeSet<String>) {
            for token in tokens.to_token_stream() {
                match token {
                    proc_macro2::TokenTree::Ident(id) => {
                        out.insert(id.to_string());
                    }
                    proc_macro2::TokenTree::Group(group) => identifiers(group.stream(), out),
                    _ => {}
                }
            }
        }
        identifiers(mac.tokens.clone(), &mut self.identifiers);
        if let Ok(expressions) = mac.parse_body_with(
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated,
        ) {
            for expression in expressions {
                self.visit_expr(&expression);
            }
        } else if name == "matches" {
            let first = mac.parse_body_with(|input: syn::parse::ParseStream| {
                let expression = input.parse::<syn::Expr>()?;
                let _rest = input.parse::<proc_macro2::TokenStream>()?;
                Ok(expression)
            });
            if let Ok(expression) = first {
                self.visit_expr(&expression);
            }
        }
    }
}

fn check_registration(surface: &Surface, contract: &Contract) -> Result<(), String> {
    for (name, source) in [
        ("valid", contract.valid_source),
        ("invalid", contract.invalid_source),
    ] {
        let block = syn::parse_str::<syn::Block>(source).map_err(|e| e.to_string())?;
        let mut evidence = Evidence::default();
        evidence.visit_block(&block);
        let generated =
            contract.symbol.contains("::generator:") || contract.symbol.contains("::macro:");
        let reference = contract.exercise.unwrap_or(&surface.reference);
        if generated != contract.exercise.is_some() {
            return Err(format!(
                "{}: only generator/macro contracts must declare the generated API they exercise",
                contract.symbol
            ));
        }
        let referenced = if contract.symbol.starts_with("feature/") {
            // Core-only features have no matching cfg in this facade test
            // crate. Exercise their observable API under workspace all-features.
            !evidence.calls.is_empty()
        } else if generated || contract.symbol.contains("::fn:") {
            evidence.calls.contains(reference)
        } else {
            evidence.identifiers.contains(reference)
        };
        if !referenced || evidence.assertions == 0 {
            return Err(format!(
                "{}: {name} contract must reference {} and assert its result",
                contract.symbol, surface.reference
            ));
        }
    }
    Ok(())
}

fn missing_contracts(
    current: &BTreeMap<String, Surface>,
    legacy: &BTreeSet<String>,
    contracts: &[Contract],
) -> Result<(), String> {
    let mut covered = legacy.clone();
    let mut seen = BTreeSet::new();
    for contract in contracts {
        if !seen.insert(contract.symbol) {
            return Err(format!("duplicate contract: {}", contract.symbol));
        }
        let Some(surface) = current.get(contract.symbol) else {
            return Err(format!("stale contract/signature: {}", contract.symbol));
        };
        check_registration(surface, contract)?;
        covered.insert(contract.symbol.into());
    }
    let missing: Vec<_> = current
        .keys()
        .filter(|key| !covered.contains(*key))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "new/changed interfaces require valid + invalid executable contracts (do not refresh the legacy inventory):\n{}",
            missing
                .into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

fn verify_policy_domains(current: &BTreeMap<String, Surface>) -> Result<(), String> {
    for domain in cases::DOMAINS {
        (domain.check_values)();
        let prefix = format!("core/format.rs::format::{}::variant:", domain.name);
        let actual: BTreeSet<_> = current
            .iter()
            .filter(|(id, _)| id.starts_with(&prefix))
            .map(|(_, surface)| surface.reference.as_str())
            .collect();
        let declared: BTreeSet<_> = domain.variants.iter().copied().collect();
        if actual != declared || actual.is_empty() || declared.len() != domain.variants.len() {
            return Err(format!(
                "{}: executable policy cases differ from source variants: source={actual:?}, cases={declared:?}",
                domain.name
            ));
        }
    }
    Ok(())
}

fn execute(contracts: &[Contract]) -> TestResult {
    for contract in contracts {
        (contract.valid)().map_err(|e| format!("{} valid: {e}", contract.symbol))?;
        (contract.invalid)().map_err(|e| format!("{} invalid: {e}", contract.symbol))?;
    }
    Ok(())
}

#[test]
fn every_new_interface_has_executed_validation_contracts() -> TestResult {
    let current = inventory();
    let snapshot = include_str!("support/validation_legacy.txt");
    let lines: Vec<_> = snapshot
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .collect();
    assert_eq!(
        source_audit::fingerprint(&(lines.join("\n") + "\n")),
        "ce0dd9b2f5f3ee70",
        "the legacy inventory is frozen; register executable contracts instead of refreshing it"
    );
    let legacy: BTreeSet<_> = lines.into_iter().map(str::to_owned).collect();
    verify_policy_domains(&current)?;
    missing_contracts(&current, &legacy, CONTRACTS)?;
    execute(CONTRACTS)
}

fn fixture(source: &str) -> BTreeMap<String, Surface> {
    let mut audit = Audit {
        file: "probe.rs".into(),
        crate_name: "core".into(),
        inventory_only: true,
        ..Audit::default()
    };
    audit.visit_file(&syn::parse_file(source).unwrap());
    audit.inventory
}

#[test]
fn new_methods_fields_variants_and_signature_changes_require_contracts() {
    let old = "pub struct Reader { pub policy: bool } pub enum Mode { A } impl Reader { pub fn get(&self) {} }";
    let legacy = fixture(old).into_keys().collect();
    for new in [
        format!("{old} impl Reader {{ pub fn fetch_anything(&self) {{}} }}"),
        old.replace("pub policy: bool", "pub policy: bool, pub new_policy: u8"),
        old.replace("Mode { A }", "Mode { A, B }"),
        old.replace("fn get(&self)", "fn get(&self, offset: u64)"),
        format!("{old} pub mod new_module {{ pub fn load() {{}} }}"),
        format!("{old} pub trait Extension {{ type Value; const LIMIT: u64; }}"),
    ] {
        assert!(
            missing_contracts(&fixture(&new), &legacy, &[]).is_err(),
            "accepted {new}"
        );
    }
    // Ordinary implementation edits keep their existing regression obligations;
    // this specific gate inventories interfaces, not arbitrary semantics.
    let comments = format!("// New words are not interfaces\n{old}");
    assert!(missing_contracts(&fixture(&comments), &legacy, &[]).is_ok());
}

#[test]
fn stub_or_unrelated_callbacks_cannot_count_as_api_coverage() {
    let surface = Surface {
        signature: String::new(),
        reference: "read_block_at".into(),
    };
    for stub in [
        "{ Ok(()) }",
        "{ assert!(true); Ok(()) }",
        "{ let f = Reader::read_block_at; assert!(true); Ok(()) }",
        "{ let text = \"reader.read_block_at(); assert!(true)\"; Ok(()) }",
    ] {
        let mut contract = Contract {
            invalid_source: stub,
            ..CONTRACTS[0]
        };
        assert!(check_registration(&surface, &contract).is_err());
        contract.invalid_source = CONTRACTS[0].invalid_source;
        assert!(check_registration(&surface, &contract).is_ok());
    }
}

#[test]
fn registered_callbacks_are_executed_and_their_failure_is_fatal() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let contracts = [Contract {
        symbol: "fixture",
        exercise: None,
        valid_source: "{}",
        invalid_source: "{}",
        valid: || {
            CALLS.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
        invalid: || {
            CALLS.fetch_add(1, Ordering::Relaxed);
            Err("intentional contract failure".into())
        },
    }];
    assert!(
        execute(&contracts)
            .unwrap_err()
            .to_string()
            .contains("intentional contract failure")
    );
    assert_eq!(CALLS.load(Ordering::Relaxed), 2);
}

#[test]
fn unregistered_policy_variants_fail_even_for_non_exhaustive_enums() {
    let mut current = inventory();
    assert!(verify_policy_domains(&current).is_ok());
    current.insert(
        "core/format.rs::format::IntegrityVerification::variant:Future@probe".into(),
        Surface {
            signature: String::new(),
            reference: "Future".into(),
        },
    );
    assert!(verify_policy_domains(&current).is_err());
}

#[test]
fn case_generators_retain_every_registered_shape_and_parameter() {
    use varlit::*;
    let commits = cases::commits();
    for shape in cases::COMMIT_SHAPES {
        assert!(
            commits
                .iter()
                .any(|value| std::mem::discriminant(value) == std::mem::discriminant(shape))
        );
    }
    for &mode in cases::TRANSACTIONS {
        assert!(commits.contains(&CommitPolicy::TransactionMarker(mode)));
    }
    let compressed = cases::compression();
    for shape in cases::COMPRESSION_SHAPES {
        assert!(
            compressed
                .iter()
                .any(|value| std::mem::discriminant(value) == std::mem::discriminant(shape))
        );
    }
    for &algorithm in cases::ALGORITHMS {
        for &header in cases::HEADERS {
            for &level in cases::LEVELS {
                assert!(compressed.iter().any(|policy| matches!(policy,
                    CompressionPolicy::VariableBlocks(value) if value.algorithm == algorithm && value.header_mode == header && value.level == level)));
            }
        }
    }
}
