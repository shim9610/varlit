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
        "#[cfg(any(kani, feature = \"mmap\"))] fn read() { spec.endian = Endian::Little; }",
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
        #[cfg(kani)] mod proofs { fn havoc() { decoder.endian = Endian::Big; } }
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

#[test]
fn included_methods_keep_identity_and_all_validation_checks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    std::fs::create_dir_all(root.join("file")).unwrap();
    std::fs::write(dir.path().join("outside.rs"), "pub fn hidden() {}").unwrap();
    std::fs::write(root.join("lib.rs"), "mod file;").unwrap();
    let method = "impl VarlitFile { pub fn read(&self, n: u64) -> u64 { n } }";
    let inspect = |inventory_only| {
        let mut audit = Audit {
            root: root.clone(),
            inventory_only,
            ..Audit::default()
        };
        audit.inspect_file(&root.join("lib.rs"));
        audit
    };
    std::fs::write(root.join("file.rs"), method).unwrap();
    let original = inspect(true).inventory;
    std::fs::write(root.join("file.rs"), "include!(\"file/methods.rs\");").unwrap();
    let child = root.join("file/methods.rs");
    std::fs::write(&child, method).unwrap();
    let included = inspect(true);
    assert_eq!(included.seen_files.len(), 3);
    assert_eq!(
        original.keys().collect::<Vec<_>>(),
        included.inventory.keys().collect::<Vec<_>>()
    );
    std::fs::write(&child, method.replace("n: u64", "n: u32")).unwrap();
    assert_ne!(
        original.keys().collect::<Vec<_>>(),
        inspect(true).inventory.keys().collect::<Vec<_>>()
    );
    std::fs::write(
        &child,
        "impl VarlitFile { fn bypass(&self) { read_native_record_header(file, 0); } }",
    )
    .unwrap();
    assert_eq!(inspect(false).errors.len(), 1);
    // A real child module owns its own source. It must not inherit a legacy
    // decoder allowance from the file that included its declaration.
    std::fs::write(&child, "#[path = \"../decoder.rs\"] mod decoder;").unwrap();
    std::fs::write(
        root.join("decoder.rs"),
        "fn fault_record_entry() { read_native_record_header(file, 0); }",
    )
    .unwrap();
    let nested = inspect(false);
    assert_eq!(nested.seen_files.len(), 4);
    assert_eq!(nested.errors.len(), 1);
    // Include paths are relative to the physical source, including inside an
    // inline module; the namespace must still be retained by the inventory.
    std::fs::write(&child, method).unwrap();
    std::fs::write(
        root.join("file.rs"),
        "mod nested { include!(\"file/methods.rs\"); }",
    )
    .unwrap();
    let inline = inspect(true);
    assert!(inline.errors.is_empty());
    assert_eq!(inline.seen_files.len(), 3);
    assert!(
        inline
            .inventory
            .keys()
            .any(|k| k.contains("file::nested::VarlitFile"))
    );
    for source in [
        "include!(concat!(\"file/\", \"methods.rs\"));",
        "include!(\"missing.rs\");",
        "include!(\"../outside.rs\");",
    ] {
        std::fs::write(root.join("file.rs"), source).unwrap();
        assert!(!inspect(true).errors.is_empty(), "accepted {source}");
    }
}
