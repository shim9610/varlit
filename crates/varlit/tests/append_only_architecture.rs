//! Structural capability boundaries, including deliberate bypass controls.
use quote::ToTokens;
use std::collections::BTreeMap;
use syn::{Item, Visibility};

fn source() -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../varlit-core/src/file");
    ["append_only.rs", "append_only/session.rs"]
        .map(|p| {
            std::fs::read_to_string(root.join(p))
                .unwrap()
                .lines()
                .filter(|l| !l.starts_with("//! "))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .join("\n")
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
        (
            "AppendOnlyWriter",
            vec![
                ("file", "Option<VarlitFile>"),
                ("authority", "Arc<WriteAuthority>"),
            ],
        ),
        (
            "AppendOnlySession",
            vec![("authority", "Arc<WriteAuthority>")],
        ),
        (
            "AppendOnlyReaderToken",
            vec![("protection", "Arc<Protection>")],
        ),
        (
            "AppendOnlyReader",
            vec![
                ("file", "VarlitFile"),
                ("protection", "Arc<ReaderProtection>"),
            ],
        ),
        (
            "GuardedMmapWindow",
            vec![
                ("mapped", "MmapPayloadWindow"),
                ("_protection", "Arc<ReaderProtection>"),
            ],
        ),
        (
            "GuardedMmapPayloads",
            vec![
                ("mapped", "MmapPayloads"),
                ("_protection", "Arc<ReaderProtection>"),
            ],
        ),
    ]);
    let methods = BTreeMap::from([
        (
            "AppendOnlyWriter",
            vec![
                "new",
                "session",
                "close",
                "reader_token",
                "push_info",
                "push_keyed_info",
                "flush",
                "commit",
                "commit_durable",
            ],
        ),
        (
            "AppendOnlySession",
            vec![
                "new",
                "reader_token",
                "open_writer",
                "status",
                "wait_for_change",
                "recover",
                "close",
            ],
        ),
        (
            "AppendOnlyReaderToken",
            vec![
                "open_reader",
                "open_reader_with_resource_limits",
                "session_status",
                "wait_for_change",
            ],
        ),
        (
            "AppendOnlyReader",
            vec![
                "session_status",
                "wait_for_change",
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
    let mut checked_recovery = false;
    for item in ast.items {
        match item {
            Item::Struct(s) if shapes.contains_key(s.ident.to_string().as_str()) => {
                let name = s.ident.to_string();
                found += 1;
                let fields: Vec<_> = s
                    .fields
                    .iter()
                    .map(|f| (f.ident.as_ref().unwrap().to_string(), compact(&f.ty)))
                    .collect();
                let expected: Vec<_> = shapes[name.as_str()]
                    .iter()
                    .map(|(n, t)| (n.to_string(), t.to_string()))
                    .collect();
                if fields != expected
                    || s.fields
                        .iter()
                        .any(|f| !matches!(f.vis, Visibility::Inherited))
                {
                    return Err(format!(
                        "{name}: authority, protection/drop order or field privacy changed"
                    ));
                }
            }
            Item::Impl(i) if methods.contains_key(compact(&i.self_ty).as_str()) => {
                let owner = compact(&i.self_ty);
                if let Some((path, _)) = &i.trait_
                    && (owner != "AppendOnlyWriter" || compact(path) != "Drop")
                {
                    return Err(format!("{owner}: unreviewed trait escape"));
                }
                for item in i.items {
                    let syn::ImplItem::Fn(f) = item else { continue };
                    let name = f.sig.ident.to_string();
                    if matches!(f.vis, Visibility::Public(_))
                        && !methods[owner.as_str()].contains(&name.as_str())
                    {
                        return Err(format!("{owner}: unreviewed public method {name}"));
                    }
                    let body = compact(&f.block);
                    if name == "open_reader_with_resource_limits" {
                        let register = body
                            .find("Protection::reader(&self.protection)?")
                            .ok_or("missing reader registration")?;
                        let open = body
                            .find("OpenOptions::new()")
                            .ok_or("missing reader handle")?;
                        let guard = body
                            .find("ifsuper::publication::opened_file_identity_array")
                            .ok_or("missing identity guard")?;
                        let parse = body
                            .find("VarlitFile::open_readonly_file")
                            .ok_or("missing checked handle parse")?;
                        if !(register < open
                            && open < guard
                            && guard < parse
                            && body.contains(
                                "opened_file_identity_array(&file)?!=self.protection.identity",
                            ))
                        {
                            return Err("reader must register, open, then check identity before parsing the same handle".into());
                        }
                    }
                    if owner == "AppendOnlyReader"
                        && name.starts_with("mmap_")
                        && !body.contains("_protection:Arc::clone(&self.protection)")
                    {
                        return Err("mapping detached from its originating protection".into());
                    }
                    if owner == "AppendOnlySession" && name == "recover_using" {
                        checked_recovery = true;
                        let gate = body
                            .find("protection.gate.try_recovery()")
                            .ok_or("missing exclusive recovery admission")?;
                        let clone = body
                            .find("native.try_clone()?")
                            .ok_or("recovery must clone the pinned native handle")?;
                        let rebuild = body
                            .find("rebuild(protection.spec,path,file)")
                            .ok_or("missing native rebuild")?;
                        if !(gate < clone && clone < rebuild)
                            || body.contains("OpenOptions")
                            || body.contains("give_back_native_guard")
                        {
                            return Err("recovery must exclude registered readers before using the pinned handle".into());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if found != shapes.len() || !checked_recovery {
        return Err("missing guarded owner/recovery".into());
    }
    Ok(())
}
#[test]
fn facade_keeps_ownership_identity_and_mutation_boundaries() {
    check(&source()).unwrap();
}
#[test]
fn facade_checker_rejects_capability_and_recovery_bypasses() {
    let current = source();
    let defects = [
        current.replace(
            "mapped: MmapPayloadWindow,",
            "pub mapped: MmapPayloadWindow,",
        ),
        format!(
            "{current}\nimpl AppendOnlyWriter {{ pub fn file_mut(&mut self)->&mut VarlitFile {{ self.active_file() }} }}"
        ),
        format!(
            "{current}\nimpl std::ops::Deref for AppendOnlyWriter {{ type Target=VarlitFile; fn deref(&self)->&VarlitFile {{ self.file.as_ref().unwrap() }} }}"
        ),
        format!(
            "{current}\nimpl AppendOnlyReaderToken {{ pub fn session(&self)->AppendOnlySession {{ todo!() }} }}"
        ),
        current.replace(
            "_protection: Arc::clone(&self.protection)",
            "_protection: fake_protection()",
        ),
        current.replace("!= self.protection.identity", "== self.protection.identity"),
        current.replace(
            "Protection::reader(&self.protection)?",
            "fake_registration()?",
        ),
        current.replace("protection.gate.try_recovery()", "fake_exclusion()"),
        current.replace("native.try_clone()?", "File::open(&protection.path)?"),
        current.replace(
            "protection: Arc<ReaderProtection>,",
            "protection: Arc<Protection>,",
        ),
    ];
    for defect in defects {
        assert_ne!(defect, current);
        assert!(check(&defect).is_err());
    }
}
