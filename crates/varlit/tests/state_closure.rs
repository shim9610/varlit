//! Field layout/privacy side of the compiler state-transition coverage gate.
use quote::ToTokens;
use serde_json::Value;
use std::{fs, path::Path};
#[test]
fn state_owner_schema_cannot_change_without_its_transition_contracts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest: Value = serde_json::from_slice(
        &fs::read(root.join("docs/verification/state-transitions.json")).unwrap(),
    )
    .unwrap();
    let proofs: Value = serde_json::from_slice(
        &fs::read(root.join("docs/verification/proof-contracts.json")).unwrap(),
    )
    .unwrap();
    let registered: std::collections::BTreeSet<_> = proofs["contracts"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["harnesses"].as_array().unwrap())
        .map(|h| h.as_str().unwrap())
        .collect();
    for owner in manifest["owners"].as_object().unwrap().values() {
        let text = fs::read_to_string(root.join(owner["source"].as_str().unwrap())).unwrap();
        let source = syn::parse_file(&text).unwrap();
        let name = owner["struct"].as_str().unwrap();
        let item = source
            .items
            .iter()
            .find_map(|i| match i {
                syn::Item::Struct(s) if s.ident == name => Some(s),
                _ => None,
            })
            .unwrap();
        let fields: Vec<_> = item
            .fields
            .iter()
            .map(|f| f.ident.as_ref().unwrap().to_string())
            .collect();
        assert_eq!(
            serde_json::to_value(fields).unwrap(),
            owner["fields"],
            "state field layout changed: {name}"
        );
        let types: Vec<_> = item
            .fields
            .iter()
            .map(|f| f.ty.to_token_stream().to_string().replace(' ', ""))
            .collect();
        assert_eq!(
            serde_json::to_value(types).unwrap(),
            owner["field_types"],
            "state field types changed: {name}"
        );
        for field in &item.fields {
            assert!(
                matches!(field.vis, syn::Visibility::Inherited),
                "state fields must remain private"
            );
        }
        if matches!(name, "Decoder" | "BuiltinCodec" | "DecodeBudget") {
            for attr in &item.attrs {
                if attr.path().is_ident("derive") {
                    let text = attr.to_token_stream().to_string();
                    assert!(
                        !["Clone", "Copy", "Default", "TryClone"]
                            .iter()
                            .any(|t| text.contains(t)),
                        "state authority may not be duplicated or fabricated"
                    );
                }
            }
        }
        for rule in owner["writers"].as_object().unwrap().values() {
            assert!(!rule["proofs"].as_array().unwrap().is_empty());
            for p in rule["proofs"].as_array().unwrap() {
                assert!(
                    registered.contains(p.as_str().unwrap()),
                    "unproved state writer"
                );
            }
        }
    }
}

#[test]
fn callback_adapters_are_private_and_exhaustively_sealed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source =
        syn::parse_file(&fs::read_to_string(root.join("crates/varlit-core/src/codec.rs")).unwrap())
            .unwrap();
    let mut adapters = std::collections::BTreeSet::new();
    for item in source.items {
        match item {
            syn::Item::Trait(t) if t.ident == "VarlitDecode" => {
                let evidence = t
                    .items
                    .iter()
                    .find_map(|i| match i {
                        syn::TraitItem::Const(c) if c.ident == "__VARLIT_BUILTIN" => Some(c),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(
                    evidence.ty.to_token_stream().to_string().replace(' ', ""),
                    "Option<BuiltinCodec<Self>>"
                );
                assert_eq!(
                    evidence
                        .default
                        .as_ref()
                        .unwrap()
                        .1
                        .to_token_stream()
                        .to_string(),
                    "None"
                );
            }
            syn::Item::Trait(t) if t.ident == "DecoderCallback" => {
                assert!(matches!(t.vis, syn::Visibility::Inherited))
            }
            syn::Item::Impl(i) => {
                if i.self_ty
                    .to_token_stream()
                    .to_string()
                    .starts_with("Decoder")
                    && let Some((path, _)) = &i.trait_
                {
                    assert!(
                        !matches!(
                            path.segments.last().unwrap().ident.to_string().as_str(),
                            "Clone" | "Copy"
                        ),
                        "decoder scope authority must not be duplicated"
                    );
                }
                if i.self_ty
                    .to_token_stream()
                    .to_string()
                    .starts_with("BuiltinCodec")
                {
                    panic!(
                        "builtin evidence must have no constructor/conversion/clone implementation"
                    );
                }
                if i.items
                    .iter()
                    .any(|m| matches!(m,syn::ImplItem::Fn(f) if f.sig.ident=="call"))
                {
                    assert_eq!(
                        i.trait_.as_ref().unwrap().0.segments.last().unwrap().ident,
                        "DecoderCallback"
                    );
                    adapters.insert(i.self_ty.to_token_stream().to_string().replace(' ', ""));
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        adapters,
        std::collections::BTreeSet::from([
            "ValueCallback<T>".into(),
            "ArrayCallback<T,N>".into(),
            "IntoCallback<'o,T>".into(),
        ])
    );
}
