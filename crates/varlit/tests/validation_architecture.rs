//! Existing validation-boundary rules and adversarial checker fixtures.
use std::{fs, path::Path};
use syn::visit::Visit;
#[allow(dead_code)] // Shared with the interface-coverage test target.
#[path = "support/source_audit.rs"]
mod source_audit;
use source_audit::{Audit, RAW_SITES, REQUIRED_EDGES};

#[test]
fn production_readers_keep_the_validation_boundaries() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../varlit-core/src");
    let mut audit = Audit {
        root: root.clone(),
        ..Audit::default()
    };
    audit.inspect_file(&root.join("lib.rs"));
    assert!(
        audit.seen_files.len() >= 30,
        "module traversal unexpectedly narrowed"
    );
    for &(file, function, callee) in RAW_SITES.iter().chain(REQUIRED_EDGES) {
        assert!(
            audit
                .edges
                .contains(&(file.into(), function.into(), callee.into())),
            "missing validation edge {file}::{function} -> {callee}"
        );
    }
    assert!(audit.errors.is_empty(), "{}", audit.errors.join("\n"));
}

fn fixture(source: &str) -> Audit {
    let mut audit = Audit {
        file: "probe.rs".into(),
        ..Audit::default()
    };
    audit.visit_file(&syn::parse_file(source).unwrap());
    audit
}

#[test]
fn checker_rejects_reintroduced_bypasses_independent_of_formatting() {
    for source in [
        "fn read() { spec\n .with_integrity_policy(IntegrityPolicy::None); }",
        "fn read() { let f = FormatSpec::with_integrity_policy; f(spec, None); }",
        "fn read() { spec.integrity_policy = None; }",
        "fn read() { spec.schema_hash &= 0; }",
        "fn read() { let policy = &mut (spec.integrity_policy); }",
        "fn read() { let spec = FormatSpec { integrity_policy: None, ..spec }; }",
        "fn read() { crate::native_layout::read_native_record_header(file, 0); }",
        "fn read() { let decode = read_native_record_header; decode(file, 0); }",
        "use crate::native_layout::read_native_record_header as decode;",
        "#[cfg(any(test, feature = \"mmap\"))] fn read() { read_native_record_header(file, 0); }",
        "#[cfg(not(test))] fn read() { read_native_record_header(file, 0); }",
        "impl Reader { fn read(&self) { spec.with_integrity_policy(None); } }",
    ] {
        assert!(
            !fixture(source).errors.is_empty(),
            "checker accepted: {source}"
        );
    }
    let accepted = fixture(
        r#"
        #[cfg(all(test, feature = "integrity"))]
        mod tests { fn fixture() { spec.with_integrity_policy(None); } }
        fn read() {
            // read_native_record_header(file, 0)
            let text = "spec.with_integrity_policy(None)";
            spec.with_resource_limits(limits);
        }
    "#,
    );
    assert!(accepted.errors.is_empty(), "{:?}", accepted.errors);
}

#[test]
fn checker_follows_external_modules_and_their_parent_cfg() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("lib.rs"),
        r#"
        #[cfg(test)] mod absent_tests;
        #[path = "decoder.rs"] mod renamed;
    "#,
    )
    .unwrap();
    fs::write(
        dir.path().join("decoder.rs"),
        "fn read() { read_native_record_header(file, 0); }",
    )
    .unwrap();
    let mut audit = Audit {
        root: dir.path().into(),
        ..Audit::default()
    };
    audit.inspect_file(&dir.path().join("lib.rs"));
    assert_eq!(audit.seen_files.len(), 2);
    assert_eq!(audit.errors.len(), 1);
    assert!(audit.errors[0].contains("decoder.rs::read"));
}

#[test]
fn untraversed_source_inclusion_cannot_silently_hide_interfaces() {
    for source in [
        "include!(\"generated.rs\");",
        "impl Reader { include!(\"generated_methods.rs\"); }",
        "#[cfg_attr(windows, path = \"windows.rs\")] mod reader;",
    ] {
        assert!(
            !fixture(source).errors.is_empty(),
            "accepted hidden source: {source}"
        );
    }
}
