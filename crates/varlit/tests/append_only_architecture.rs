//! Structural capability boundaries, including deliberate bypass controls.
use quote::ToTokens;
use std::collections::BTreeMap;
use syn::{Item, Visibility};

fn source() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../varlit-core/src/file/append_only.rs"
    ))
    .unwrap()
}
fn compact(tokens: impl ToTokens) -> String {
    tokens
        .to_token_stream()
        .to_string()
        .split_whitespace()
        .collect()
}

fn check(source: &str) -> Result<(), String> {
    let ast = syn::parse_file(source).map_err(|e| e.to_string())?;
    let shapes = BTreeMap::from([
        ("AppendOnlyWriter", vec!["file", "protection"]),
        ("AppendOnlyReaderToken", vec!["protection"]),
        ("AppendOnlyReader", vec!["file", "protection"]),
        ("GuardedMmapWindow", vec!["mapped", "_protection"]),
        ("GuardedMmapPayloads", vec!["mapped", "_protection"]),
    ]);
    let methods = BTreeMap::from([
        (
            "AppendOnlyWriter",
            vec![
                "new",
                "reader_token",
                "push_info",
                "push_keyed_info",
                "flush",
                "commit",
                "commit_durable",
            ],
        ),
        (
            "AppendOnlyReaderToken",
            vec!["open_reader", "open_reader_with_resource_limits"],
        ),
        (
            "AppendOnlyReader",
            vec![
                "follow",
                "block_entries_into",
                "block_chain",
                "mmap_payload_window",
                "mmap_payloads",
            ],
        ),
        ("GuardedMmapWindow", vec!["as_bytes"]),
        (
            "GuardedMmapPayloads",
            vec![
                "len",
                "is_empty",
                "index_entries",
                "payload_window",
                "block_payload_window",
            ],
        ),
    ]);
    let mut found = 0;
    for item in ast.items {
        match item {
            Item::Struct(s) if shapes.contains_key(s.ident.to_string().as_str()) => {
                let name = s.ident.to_string();
                found += 1;
                let fields: Vec<_> = s
                    .fields
                    .iter()
                    .map(|f| f.ident.as_ref().unwrap().to_string())
                    .collect();
                if fields != shapes[name.as_str()]
                    || s.fields
                        .iter()
                        .any(|f| !matches!(f.vis, Visibility::Inherited))
                {
                    return Err(format!(
                        "{name}: protection/drop order or field privacy changed"
                    ));
                }
            }
            Item::Impl(i) if methods.contains_key(compact(&i.self_ty).as_str()) => {
                let owner = compact(&i.self_ty);
                if i.trait_.is_some() {
                    return Err(format!("{owner}: unreviewed trait escape"));
                }
                for item in i.items {
                    let syn::ImplItem::Fn(f) = item else { continue };
                    let name = f.sig.ident.to_string();
                    if !methods[owner.as_str()].contains(&name.as_str()) {
                        return Err(format!("{owner}: unreviewed method {name}"));
                    }
                    if name == "open_reader_with_resource_limits" {
                        let body = compact(&f.block);
                        let guard = body
                            .find("ifsuper::publication::opened_file_identity_array")
                            .ok_or("missing identity guard")?;
                        let parse = body
                            .find("VarlitFile::open_readonly_file")
                            .ok_or("reader no longer uses the checked handle")?;
                        if guard >= parse
                            || !body.contains(
                                "opened_file_identity_array(&file)?!=self.protection.identity",
                            )
                        {
                            return Err(
                                "reader identity must be checked before parsing the same handle"
                                    .into(),
                            );
                        }
                    }
                    if owner == "AppendOnlyReader"
                        && name.starts_with("mmap_")
                        && !compact(&f.block).contains("_protection:Arc::clone(&self.protection)")
                    {
                        return Err("mapping detached from its originating protection".into());
                    }
                }
            }
            _ => {}
        }
    }
    if found != shapes.len() {
        return Err("missing guarded owner".into());
    }
    Ok(())
}

#[test]
fn facade_keeps_ownership_identity_and_mutation_boundaries() {
    check(&source()).unwrap();
}

#[test]
fn facade_checker_rejects_mutable_escapes_detached_leases_and_late_identity_checks() {
    let current = source();
    let defects = [
        current.replace(
            "mapped: MmapPayloadWindow,",
            "pub mapped: MmapPayloadWindow,",
        ),
        format!(
            "{current}\nimpl AppendOnlyWriter {{ pub fn file_mut(&mut self)->&mut VarlitFile {{ &mut self.file }} }}"
        ),
        format!(
            "{current}\nimpl std::ops::Deref for AppendOnlyWriter {{ type Target=VarlitFile; fn deref(&self)->&VarlitFile {{ &self.file }} }}"
        ),
        current.replace(
            "_protection: Arc::clone(&self.protection)",
            "_protection: fake_protection()",
        ),
        current.replace("!= self.protection.identity", "== self.protection.identity"),
    ];
    for defect in defects {
        assert_ne!(defect, current);
        assert!(check(&defect).is_err());
    }
}
