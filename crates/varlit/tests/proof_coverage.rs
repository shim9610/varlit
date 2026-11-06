//! Source-wide change/impact gate. This is not a claim that every body is proved.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};
#[allow(dead_code)]
#[path = "support/source_audit.rs"]
mod source_audit;
use serde_json::{Value, json};
use source_audit::Audit;

fn observed(root: &Path) -> Value {
    let mut functions = BTreeMap::new();
    let mut sources = BTreeMap::new();
    for (name, directory, generator) in [
        ("core", "varlit-core", false),
        ("facade", "varlit", false),
        ("macros", "varlit-macros", true),
    ] {
        let source_root = root.join("crates").join(directory).join("src");
        let mut audit = Audit {
            root: source_root.clone(),
            crate_name: name.into(),
            generator_crate: generator,
            inventory_only: true,
            ..Audit::default()
        };
        audit.inspect_file(&source_root.join("lib.rs"));
        assert!(audit.errors.is_empty(), "{}", audit.errors.join("\n"));
        for (id, body) in audit.proof_bodies {
            assert!(
                functions
                    .insert(
                        id,
                        json!({"body":body.fingerprint,"references":body.references})
                    )
                    .is_none()
            );
        }
        // Include private constants/types/imports/macro generators as well as
        // function bodies. Text hashes intentionally invalidate on any source
        // edit, including cfg branches inactive on the running target.
        fn files(path: &Path, out: &mut BTreeSet<std::path::PathBuf>) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                assert!(!path.is_symlink(), "source symlink needs an explicit audit");
                if path.is_dir() {
                    files(&path, out);
                } else if path.extension().is_some_and(|s| s == "rs") {
                    out.insert(path);
                }
            }
        }
        let mut paths = BTreeSet::new();
        files(&source_root, &mut paths);
        for path in paths {
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .replace('\\', "/");
            sources.insert(
                relative,
                source_audit::fingerprint(&fs::read_to_string(path).unwrap().replace("\r\n", "\n")),
            );
        }
    }
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "crates/varlit/Cargo.toml",
        "crates/varlit-core/Cargo.toml",
        "crates/varlit-macros/Cargo.toml",
    ] {
        sources.insert(
            path.into(),
            source_audit::fingerprint(
                &fs::read_to_string(root.join(path))
                    .unwrap()
                    .replace("\r\n", "\n"),
            ),
        );
    }
    assert!(
        functions.len() > 1000,
        "production body discovery unexpectedly narrowed"
    );
    // Keep the complete sorted AST catalogue in the digest without checking a
    // megabyte of duplicated source metadata into the repository. Every source
    // edit conservatively requires all contract families, not an inferred call
    // graph that could miss dynamic dispatch or generated code.
    json!({"schema":1,"functions":{"count":functions.len(),"catalogue":source_audit::fingerprint(&serde_json::to_string(&functions).unwrap())},"sources":sources})
}

#[test]
fn every_source_change_has_current_proof_obligations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let current = observed(&root);
    if let Some(output) = std::env::var_os("VARLIT_PROOF_INVENTORY_OUT") {
        fs::write(output, serde_json::to_vec_pretty(&current).unwrap()).unwrap();
    }
    let expected: Value = serde_json::from_slice(
        &fs::read(root.join("docs/verification/production-inventory.json"))
            .expect("missing production proof inventory"),
    )
    .unwrap();
    for key in ["functions", "sources"] {
        let before = expected[key].as_object().unwrap();
        let after = current[key].as_object().unwrap();
        let changed: BTreeSet<_> = before
            .keys()
            .chain(after.keys())
            .filter(|k| before.get(*k) != after.get(*k))
            .collect();
        assert!(
            changed.is_empty(),
            "unreviewed {key}; update the impacted proof/caller obligations and rerun verification before recording a new inventory: {changed:?}"
        );
    }
    let manifest: Value = serde_json::from_slice(
        &fs::read(root.join("docs/verification/proof-contracts.json")).unwrap(),
    )
    .unwrap();
    let families: BTreeSet<_> = manifest["contracts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    let obligations: Value = serde_json::from_slice(
        &fs::read(root.join("docs/verification/caller-contracts.json"))
            .expect("missing caller contracts"),
    )
    .unwrap();
    let caller_families: BTreeSet<_> = obligations["contracts"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        families, caller_families,
        "proof and caller obligations must cover exactly the same families"
    );
    for contract in manifest["contracts"].as_array().unwrap() {
        let family = contract["id"].as_str().unwrap();
        assert_eq!(
            contract["harnesses"], obligations["contracts"][family]["required_harnesses"],
            "caller obligation silently lost a proof: {family}"
        );
    }
}

#[test]
fn body_inventory_covers_private_trait_nested_and_cfg_definitions() {
    use syn::visit::Visit;
    let mut audit = Audit {
        crate_name: "fixture".into(),
        file: "fixture.rs".into(),
        inventory_only: true,
        ..Audit::default()
    };
    audit.visit_file(
        &syn::parse_file(
            r#"
        fn outer(){ fn nested(){ protected(); } nested(); }
        fn other(){ fn nested(){ protected(); } nested(); }
        trait T { fn required(&self); fn defaulted(&self){ protected(); } }
        impl T for Owner { fn required(&self){ protected(); } }
        impl Owner { fn private(&self){ protected(); } }
        #[cfg(unix)] fn platform(){ protected(); }
        #[cfg(windows)] fn platform(){ protected(); }
        #[cfg(test)] fn excluded(){ protected(); }
        fn pointer(){ let indirect=protected; indirect(); }
        fn generated(){ invoke!(protected()); }
    "#,
        )
        .unwrap(),
    );
    assert_eq!(audit.proof_bodies.len(), 12);
    assert!(
        audit
            .proof_bodies
            .values()
            .filter(|b| b.references.contains("protected"))
            .count()
            >= 9
    );
    assert!(!audit.proof_bodies.keys().any(|k| k.contains("excluded")));
}

#[test]
fn private_edits_and_new_paths_invalidate_the_body_catalogue() {
    use syn::visit::Visit;
    fn catalogue(source: &str) -> Vec<(String, String)> {
        let mut audit = Audit {
            inventory_only: true,
            ..Audit::default()
        };
        audit.visit_file(&syn::parse_file(source).unwrap());
        audit
            .proof_bodies
            .into_iter()
            .map(|(id, body)| (id, body.fingerprint))
            .collect()
    }
    let baseline = catalogue("fn private(){ protected(); }");
    for changed in [
        "fn private(){ unprotected(); }",
        "fn private(){ protected(); } fn added(){ unprotected(); }",
        "#[cfg(windows)] fn private(){ protected(); }",
        "fn private(){ alias(); }",
    ] {
        assert_ne!(
            baseline,
            catalogue(changed),
            "missed private/cfg/caller change"
        );
    }
    assert_ne!(
        catalogue("trait T { fn f(){ protected(); } }"),
        catalogue("trait T { fn f(){ unprotected(); } }")
    );
}
