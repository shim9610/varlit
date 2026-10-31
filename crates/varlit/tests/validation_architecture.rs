//! Syntax-tree guardrails for the validation boundaries that have regressed.
//!
//! This is not Rust type resolution or a proof about arbitrary macro expansions.
//! It catches new raw-decoder sites and ordinary policy rewrites, including
//! multiline expressions, qualified function references and renamed imports.
//! Differential tests exercise the actual behaviour; these checks keep new
//! paths from quietly escaping that coverage. No runtime dependency is added.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use syn::{
    Attribute, Expr, Item, Meta, Token,
    visit::{self, Visit},
};

const WIRE_FIELDS: &[&str] = &[
    "schema_hash",
    "endian",
    "index_policy",
    "commit_policy",
    "integrity_policy",
    "liveness_policy",
    "manifest_policy",
    "compression_policy",
    "block_compression",
];

// These are existing decoder owners, not an allowlist for new fast paths.
// Comparisons against held entries legitimately decode without constructing a
// new entry. The resident directory faults records whose offsets were scanned.
const RAW_SITES: &[(&str, &str, &str)] = &[
    (
        "file.rs",
        "check_directory_describes_this_file",
        "read_native_record_header",
    ),
    (
        "file.rs",
        "read_record_entry_positional",
        "read_native_record_header",
    ),
    ("file.rs", "read_record_header", "read_native_record_header"),
    ("file.rs", "fault_record_entry", "read_native_record_header"),
    (
        "file/scan.rs",
        "verify_record_bytes",
        "read_native_record_header",
    ),
    (
        "file.rs",
        "decode_record_footer",
        "decode_native_record_footer",
    ),
];
const REQUIRED_EDGES: &[(&str, &str, &str)] = &[
    (
        "file.rs",
        "read_record_entry_positional",
        "validate_record_entry",
    ),
    (
        "file/scan.rs",
        "read_record_entry_at",
        "validate_record_entry",
    ),
    (
        "indexed.rs",
        "read_stream_entry_frame",
        "read_record_entry_positional",
    ),
];

fn metas(list: &syn::MetaList) -> syn::punctuated::Punctuated<Meta, Token![,]> {
    list.parse_args_with(syn::punctuated::Punctuated::parse_terminated)
        .unwrap()
}

// Evaluate only the known fact `test = false`. Unknown features/targets remain
// included, so all platform and feature implementations are inspected.
fn production_cfg(meta: &Meta) -> Option<bool> {
    match meta {
        Meta::Path(path) if path.is_ident("test") => Some(false),
        Meta::List(list) if list.path.is_ident("not") => {
            let args = metas(list);
            assert_eq!(args.len(), 1);
            production_cfg(args.first().unwrap()).map(|value| !value)
        }
        Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
            let all = list.path.is_ident("all");
            let values: Vec<_> = metas(list).iter().map(production_cfg).collect();
            if values.contains(&Some(!all)) {
                Some(!all)
            } else if values.iter().all(|value| *value == Some(all)) {
                Some(all)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && production_cfg(&attr.parse_args::<Meta>().unwrap()) == Some(false)
    })
}

fn attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(v) => &v.attrs,
        Item::Enum(v) => &v.attrs,
        Item::ExternCrate(v) => &v.attrs,
        Item::Fn(v) => &v.attrs,
        Item::ForeignMod(v) => &v.attrs,
        Item::Impl(v) => &v.attrs,
        Item::Macro(v) => &v.attrs,
        Item::Mod(v) => &v.attrs,
        Item::Static(v) => &v.attrs,
        Item::Struct(v) => &v.attrs,
        Item::Trait(v) => &v.attrs,
        Item::TraitAlias(v) => &v.attrs,
        Item::Type(v) => &v.attrs,
        Item::Union(v) => &v.attrs,
        Item::Use(v) => &v.attrs,
        _ => panic!("unsupported Rust item in validation architecture checker"),
    }
}

fn wire_setter(name: &str) -> bool {
    name == "with_computed_schema_hash"
        || name
            .strip_prefix("with_")
            .is_some_and(|field| WIRE_FIELDS.contains(&field))
}

fn raw_decoder(name: &str) -> bool {
    matches!(
        name,
        "read_native_record_header" | "decode_native_record_footer"
    )
}

#[derive(Default)]
struct Audit {
    root: PathBuf,
    file: PathBuf,
    module_dir: PathBuf,
    attribute_dir: PathBuf,
    function: String,
    seen_files: BTreeSet<PathBuf>,
    edges: BTreeSet<(String, String, String)>,
    errors: Vec<String>,
}

impl Audit {
    fn error(&mut self, message: &str) {
        self.errors.push(format!(
            "{}::{}: {message}",
            self.file.display(),
            self.function
        ));
    }

    fn format_owner(&self) -> bool {
        self.file == Path::new("format.rs")
    }

    fn inspect_file(&mut self, path: &Path) {
        let source = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let parsed = syn::parse_file(&source).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let old_file = std::mem::replace(
            &mut self.file,
            path.strip_prefix(&self.root).unwrap().into(),
        );
        assert!(
            self.seen_files.insert(self.file.clone()),
            "duplicate module: {}",
            path.display()
        );
        let parent = path.parent().unwrap();
        let default_dir = if matches!(path.file_stem().unwrap().to_str().unwrap(), "lib" | "mod") {
            parent.to_owned()
        } else {
            parent.join(path.file_stem().unwrap())
        };
        let old_mod = std::mem::replace(&mut self.module_dir, default_dir);
        let old_attr = std::mem::replace(&mut self.attribute_dir, parent.into());
        self.visit_file(&parsed);
        self.file = old_file;
        self.module_dir = old_mod;
        self.attribute_dir = old_attr;
    }

    fn reference(&mut self, name: &str) {
        // Normalise only filesystem separators, not Rust identifiers.
        let file = self.file.to_string_lossy().replace('\\', "/");
        let edge = (file, self.function.clone(), name.to_owned());
        if raw_decoder(name)
            && !RAW_SITES
                .iter()
                .any(|(f, owner, decoder)| edge.0 == *f && edge.1 == *owner && edge.2 == *decoder)
        {
            self.error(&format!(
                "new raw decoder reference: {name}; use the shared validated reader"
            ));
        }
        if !self.format_owner() && wire_setter(name) {
            self.error(&format!("runtime wire-policy rewrite: {name}"));
        }
        self.edges.insert(edge);
    }

    fn write_target(&mut self, expr: &Expr) {
        match expr {
            Expr::Field(field) => {
                if let syn::Member::Named(name) = &field.member
                    && !self.format_owner()
                    && WIRE_FIELDS.contains(&name.to_string().as_str())
                {
                    self.error(&format!("runtime wire-policy write/borrow: {name}"));
                }
            }
            Expr::Paren(v) => self.write_target(&v.expr),
            Expr::Group(v) => self.write_target(&v.expr),
            Expr::Tuple(v) => {
                for value in &v.elems {
                    self.write_target(value);
                }
            }
            _ => {}
        }
    }
}

impl<'ast> Visit<'ast> for Audit {
    fn visit_item(&mut self, item: &'ast Item) {
        if !test_only(attrs(item)) {
            visit::visit_item(self, item);
        }
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if let Some((_, items)) = &item.content {
            let dir = self.module_dir.join(item.ident.to_string());
            let old_mod = std::mem::replace(&mut self.module_dir, dir.clone());
            let old_attr = std::mem::replace(&mut self.attribute_dir, dir);
            for item in items {
                self.visit_item(item);
            }
            self.module_dir = old_mod;
            self.attribute_dir = old_attr;
        } else {
            let override_path = item.attrs.iter().find(|a| a.path().is_ident("path"));
            let path = if let Some(attr) = override_path {
                let Meta::NameValue(value) = &attr.meta else {
                    panic!("invalid path attribute")
                };
                let Expr::Lit(value) = &value.value else {
                    panic!("invalid module path")
                };
                let syn::Lit::Str(value) = &value.lit else {
                    panic!("invalid module path")
                };
                self.attribute_dir.join(value.value())
            } else {
                let simple = self.module_dir.join(format!("{}.rs", item.ident));
                let nested = self.module_dir.join(item.ident.to_string()).join("mod.rs");
                assert_ne!(
                    simple.exists(),
                    nested.exists(),
                    "ambiguous/missing module {}",
                    item.ident
                );
                if simple.exists() { simple } else { nested }
            };
            self.inspect_file(&path);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let previous = std::mem::replace(&mut self.function, item.sig.ident.to_string());
        visit::visit_item_fn(self, item);
        self.function = previous;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if test_only(&item.attrs) {
            return;
        }
        let previous = std::mem::replace(&mut self.function, item.sig.ident.to_string());
        visit::visit_impl_item_fn(self, item);
        self.function = previous;
    }

    fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
        self.reference(&expr.path.segments.last().unwrap().ident.to_string());
        visit::visit_expr_path(self, expr);
    }

    fn visit_expr_method_call(&mut self, expr: &'ast syn::ExprMethodCall) {
        self.reference(&expr.method.to_string());
        visit::visit_expr_method_call(self, expr);
    }

    fn visit_use_rename(&mut self, item: &'ast syn::UseRename) {
        let name = item.ident.to_string();
        if raw_decoder(&name) || wire_setter(&name) {
            self.error("renamed validation-boundary import");
        }
        visit::visit_use_rename(self, item);
    }

    fn visit_expr_assign(&mut self, expr: &'ast syn::ExprAssign) {
        self.write_target(&expr.left);
        visit::visit_expr_assign(self, expr);
    }

    fn visit_expr_binary(&mut self, expr: &'ast syn::ExprBinary) {
        if matches!(
            expr.op,
            syn::BinOp::AddAssign(_)
                | syn::BinOp::SubAssign(_)
                | syn::BinOp::MulAssign(_)
                | syn::BinOp::DivAssign(_)
                | syn::BinOp::RemAssign(_)
                | syn::BinOp::BitXorAssign(_)
                | syn::BinOp::BitAndAssign(_)
                | syn::BinOp::BitOrAssign(_)
                | syn::BinOp::ShlAssign(_)
                | syn::BinOp::ShrAssign(_)
        ) {
            self.write_target(&expr.left);
        }
        visit::visit_expr_binary(self, expr);
    }

    fn visit_expr_reference(&mut self, expr: &'ast syn::ExprReference) {
        if expr.mutability.is_some() {
            self.write_target(&expr.expr);
        }
        visit::visit_expr_reference(self, expr);
    }

    fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
        if expr.path.segments.last().unwrap().ident == "FormatSpec" && !self.format_owner() {
            self.error("runtime FormatSpec reconstruction; retain the caller's wire policy");
        }
        visit::visit_expr_struct(self, expr);
    }
}

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
