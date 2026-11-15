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
    let mut contracts = json!({});
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
        if name == "core" {
            contracts = json!({"definitions": audit.contract_definitions,
                "proofs": audit.contract_harnesses, "verified": audit.verified_stubs,
                "ordinary": audit.ordinary_stubs,"replacements":audit.stub_replacements});
        }
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
    json!({"schema":1,"functions":{"count":functions.len(),"catalogue":source_audit::fingerprint(&serde_json::to_string(&functions).unwrap())},"sources":sources,"contracts":contracts})
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
    for key in ["functions", "sources", "contracts"] {
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
    check_contract_links(&current["contracts"], &manifest).unwrap();
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

fn check_contract_links(facts: &Value, manifest: &Value) -> Result<(), String> {
    let registered: BTreeSet<_> = manifest["contracts"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| c["harnesses"].as_array().unwrap())
        .map(|h| h.as_str().unwrap())
        .collect();
    let mut definitions = BTreeMap::new();
    let mut proofs = BTreeMap::new();
    let mut references = BTreeMap::new();
    for c in manifest["executable_contracts"]
        .as_array()
        .ok_or("missing executable contracts")?
    {
        let target = c["target"].as_str().unwrap();
        let proof = c["proof"].as_str().unwrap();
        let reference = c["reference"].as_str().unwrap();
        if !registered.contains(proof) {
            return Err(format!("unregistered contract proof: {proof}"));
        }
        if references.insert(target, reference).is_some()
            || proofs.insert(proof, reference).is_some()
        {
            return Err("duplicate executable contract".into());
        }
        if !c["annotations"]
            .as_array()
            .unwrap()
            .contains(&json!("ensures"))
        {
            return Err("contract has no postcondition".into());
        }
        definitions.insert(target, c["annotations"].clone());
    }
    if definitions.is_empty() {
        return Err("empty executable contracts".into());
    }
    if serde_json::to_value(definitions).unwrap() != facts["definitions"] {
        return Err("contract definition/annotation mismatch".into());
    }
    if serde_json::to_value(proofs).unwrap() != facts["proofs"] {
        return Err("proof_for_contract target mismatch".into());
    }
    let mut expected = BTreeMap::new();
    for (caller, dependencies) in manifest["contract_uses"]
        .as_object()
        .ok_or("missing contract dependencies")?
    {
        if !registered.contains(caller.as_str()) {
            return Err(format!("unregistered contract consumer: {caller}"));
        }
        let mut targets = BTreeSet::new();
        for target in dependencies.as_array().unwrap() {
            let name = target.as_str().unwrap();
            targets.insert(
                *references
                    .get(name)
                    .ok_or_else(|| format!("unproved dependency: {name}"))?,
            );
        }
        expected.insert(caller, targets);
    }
    if serde_json::to_value(expected).unwrap() != facts["verified"] {
        return Err("verified-stub dependency mismatch".into());
    }
    let protected: BTreeSet<_> = references.values().copied().collect();
    for targets in facts["ordinary"].as_object().unwrap().values() {
        for target in targets.as_array().unwrap() {
            let last = target.as_str().unwrap().rsplit("::").next().unwrap().trim();
            if protected.contains(last) {
                return Err("unverified stub replaces a contracted function".into());
            }
        }
    }
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (consumer, targets) in manifest["contract_uses"].as_object().unwrap() {
        for target in targets.as_array().unwrap() {
            let entry = manifest["executable_contracts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["target"] == *target)
                .unwrap();
            graph
                .entry(consumer.clone())
                .or_default()
                .insert(entry["proof"].as_str().unwrap().into());
        }
    }
    if let Some(callers) = facts["replacements"].as_object() {
        for (consumer, targets) in callers {
            for (target, replacement) in targets.as_object().unwrap() {
                let target = target.replace(' ', "");
                let replacement = replacement.as_str().unwrap().replace(' ', "");
                // Existing allocator/empty-Vec assumptions remain explicit in
                // the family specifications. A new semantic stub needs proofs.
                let allocation = target.starts_with("std::alloc::")
                    || target.starts_with("<std::alloc::Globalas");
                let empty_vec = target.starts_with("std::vec::Vec") && target.ends_with("::new");
                let sentinel = target == "parse_entry" && replacement == "forbidden_entry_read";
                if !(allocation || empty_vec || sentinel) {
                    return Err(format!(
                        "semantic stub has no verified function contract: {consumer} {target} {replacement}"
                    ));
                }
            }
        }
    }
    fn acyclic(
        node: &str,
        graph: &BTreeMap<String, BTreeSet<String>>,
        active: &mut BTreeSet<String>,
        done: &mut BTreeSet<String>,
    ) -> bool {
        if done.contains(node) {
            return true;
        }
        if !active.insert(node.into()) {
            return false;
        }
        if graph
            .get(node)
            .is_some_and(|edges| edges.iter().any(|next| !acyclic(next, graph, active, done)))
        {
            return false;
        }
        active.remove(node);
        done.insert(node.into());
        true
    }
    let mut done = BTreeSet::new();
    for node in graph.keys() {
        if !acyclic(node, &graph, &mut BTreeSet::new(), &mut done) {
            return Err("cyclic proof/model dependency".into());
        }
    }
    for contract in manifest["contracts"].as_array().unwrap() {
        for key in ["requires", "ensures", "on_error", "frame", "progress"] {
            if contract["specification"][key]
                .as_str()
                .is_none_or(|s| s.trim().is_empty())
            {
                return Err(format!(
                    "missing specification clause: {} {key}",
                    contract["id"]
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn contract_links_require_the_real_definition_proof_and_verified_use() {
    let facts = json!({"definitions":{"f":["ensures"]},"proofs":{"proof":"f"},
        "verified":{"consumer":["f"]},"ordinary":{}});
    let manifest = json!({"contracts":[{"id":"fixture","harnesses":["proof","consumer"],
        "specification":{"requires":"valid typed inputs","ensures":"r == x","on_error":"none","frame":"pure","progress":"one step"}}],
        "executable_contracts":[{"target":"f","reference":"f","proof":"proof","annotations":["ensures"]}],
        "contract_uses":{"consumer":["f"]}});
    check_contract_links(&facts, &manifest).unwrap();
    for (kind, expected) in [
        (0, "definition/annotation"),
        (1, "proof_for_contract"),
        (2, "verified-stub"),
        (3, "unverified stub"),
        (4, "unproved dependency"),
        (5, "specification clause"),
        (6, "cyclic"),
        (7, "semantic stub"),
    ] {
        let mut f = facts.clone();
        let mut m = manifest.clone();
        match kind {
            0 => {
                f["definitions"] = json!({});
            }
            1 => {
                f["proofs"]["proof"] = json!("different");
            }
            2 => {
                f["verified"] = json!({});
            }
            3 => {
                f["ordinary"] = json!({"consumer":["module :: f"]});
            }
            4 => {
                m["contract_uses"]["consumer"] = json!(["unknown"]);
            }
            5 => {
                m["contracts"][0]["specification"]["on_error"] = Value::Null;
            }
            6 => {
                m["contract_uses"] = json!({"proof":["f"]});
                f["verified"] = json!({"proof":["f"]});
            }
            _ => {
                f["replacements"] = json!({"consumer":{"semantic::g":"unchecked_model"}});
            }
        }
        assert!(check_contract_links(&f, &m).unwrap_err().contains(expected));
    }
}
