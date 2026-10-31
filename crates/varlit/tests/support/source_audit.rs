//! Syntax-tree guardrails for the validation boundaries that have regressed.
//!
//! This is not Rust type resolution or a proof about arbitrary macro expansions.
//! It catches new raw-decoder sites and ordinary policy rewrites, including
//! multiline expressions, qualified function references and renamed imports.
//! Differential tests exercise the actual behaviour; these checks keep new
//! paths from quietly escaping that coverage. No runtime dependency is added.

use quote::ToTokens;
use std::{
    collections::{BTreeMap, BTreeSet},
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
pub(crate) const RAW_SITES: &[(&str, &str, &str)] = &[
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
pub(crate) const REQUIRED_EDGES: &[(&str, &str, &str)] = &[
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

fn conditional_module_path(meta: &Meta) -> bool {
    match meta {
        Meta::NameValue(value) => value.path.is_ident("path"),
        Meta::List(list) if list.path.is_ident("cfg_attr") => {
            metas(list).iter().any(conditional_module_path)
        }
        _ => false,
    }
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

#[derive(Clone, Debug)]
pub(crate) struct Surface {
    pub(crate) signature: String,
    pub(crate) reference: String,
}

// Change detector, not an authentication primitive. Canonical Rust tokens
// exclude whitespace and comments; no dependency/toolchain-specific Debug output.
pub(crate) fn fingerprint(text: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[derive(Default)]
pub(crate) struct Audit {
    pub(crate) root: PathBuf,
    pub(crate) file: PathBuf,
    pub(crate) module_dir: PathBuf,
    pub(crate) attribute_dir: PathBuf,
    pub(crate) function: String,
    pub(crate) scope: Vec<String>,
    pub(crate) crate_name: String,
    pub(crate) generator_crate: bool,
    pub(crate) inventory_only: bool,
    pub(crate) inventory: BTreeMap<String, Surface>,
    pub(crate) seen_files: BTreeSet<PathBuf>,
    pub(crate) edges: BTreeSet<(String, String, String)>,
    pub(crate) errors: Vec<String>,
}

impl Audit {
    fn surface(&mut self, kind: &str, name: &str, tokens: impl ToTokens) {
        let scope = self.scope.join("::");
        let id = format!(
            "{}/{}::{scope}::{kind}:{name}",
            self.crate_name,
            self.file.to_string_lossy().replace('\\', "/")
        );
        let signature = fingerprint(&tokens.to_token_stream().to_string());
        // The same method can be declared separately for different platforms.
        let key = format!("{id}@{signature}");
        self.inventory.insert(
            key,
            Surface {
                signature,
                reference: name.rsplit("::").next().unwrap().into(),
            },
        );
    }

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

    pub(crate) fn inspect_file(&mut self, path: &Path) {
        let source = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let parsed = syn::parse_file(&source).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let old_file = std::mem::replace(
            &mut self.file,
            path.strip_prefix(&self.root).unwrap().into(),
        );
        let entrypoint = self.seen_files.is_empty();
        assert!(
            self.seen_files.insert(self.file.clone()),
            "duplicate module: {}",
            path.display()
        );
        let parent = path.parent().unwrap();
        let default_dir =
            if entrypoint || matches!(path.file_stem().unwrap().to_str().unwrap(), "lib" | "mod") {
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
        if !self.inventory_only
            && raw_decoder(name)
            && !RAW_SITES
                .iter()
                .any(|(f, owner, decoder)| edge.0 == *f && edge.1 == *owner && edge.2 == *decoder)
        {
            self.error(&format!(
                "new raw decoder reference: {name}; use the shared validated reader"
            ));
        }
        if !self.inventory_only && !self.format_owner() && wire_setter(name) {
            self.error(&format!("runtime wire-policy rewrite: {name}"));
        }
        self.edges.insert(edge);
    }

    fn write_target(&mut self, expr: &Expr) {
        match expr {
            Expr::Field(field) => {
                if let syn::Member::Named(name) = &field.member
                    && !self.inventory_only
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
        if item
            .attrs
            .iter()
            .any(|attr| attr.path().is_ident("cfg_attr") && conditional_module_path(&attr.meta))
        {
            self.error("conditional module paths need explicit traversal before entering the validation inventory");
            return;
        }
        self.scope.push(item.ident.to_string());
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
        self.scope.pop();
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if self.generator_crate || self.crate_name == "macros" {
            // Proc-macro output is generated inside quote! and cannot be
            // inventoried as ordinary Rust declarations before expansion.
            // Require executable generated-code contracts on generator changes.
            let sig = &item.sig;
            let block = &item.block;
            self.surface(
                "generator",
                &item.sig.ident.to_string(),
                quote::quote!(#sig #block),
            );
        } else if matches!(item.vis, syn::Visibility::Public(_)) {
            self.surface("fn", &item.sig.ident.to_string(), &item.sig);
        }
        let previous = std::mem::replace(&mut self.function, item.sig.ident.to_string());
        visit::visit_item_fn(self, item);
        self.function = previous;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if test_only(&item.attrs) {
            return;
        }
        if matches!(item.vis, syn::Visibility::Public(_)) {
            self.surface("fn", &item.sig.ident.to_string(), &item.sig);
        }
        let previous = std::mem::replace(&mut self.function, item.sig.ident.to_string());
        visit::visit_impl_item_fn(self, item);
        self.function = previous;
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let owner = item.self_ty.to_token_stream().to_string();
        self.scope.push(owner);
        if let Some((path, _)) = &item.trait_ {
            self.scope.push(format!("as {}", path.to_token_stream()));
        }
        visit::visit_item_impl(self, item);
        if item.trait_.is_some() {
            self.scope.pop();
        }
        self.scope.pop();
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        if matches!(item.vis, syn::Visibility::Public(_)) {
            self.surface("struct", &item.ident.to_string(), &item.generics);
            self.scope.push(item.ident.to_string());
            for (index, field) in item.fields.iter().enumerate() {
                if !test_only(&field.attrs) && matches!(field.vis, syn::Visibility::Public(_)) {
                    let name = field
                        .ident
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or(index.to_string());
                    self.surface("field", &name, &field.ty);
                }
            }
            self.scope.pop();
        }
        visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        if matches!(item.vis, syn::Visibility::Public(_)) {
            self.surface("enum", &item.ident.to_string(), &item.generics);
            self.scope.push(item.ident.to_string());
            for variant in &item.variants {
                if test_only(&variant.attrs) {
                    continue;
                }
                let mut variant = variant.clone();
                variant.attrs.clear();
                self.surface("variant", &variant.ident.to_string(), &variant);
            }
            self.scope.pop();
        }
        visit::visit_item_enum(self, item);
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        if matches!(item.vis, syn::Visibility::Public(_)) {
            self.surface("trait", &item.ident.to_string(), &item.generics);
            self.scope.push(item.ident.to_string());
            for member in &item.items {
                match member {
                    syn::TraitItem::Fn(method) if !test_only(&method.attrs) => {
                        self.surface("fn", &method.sig.ident.to_string(), &method.sig);
                    }
                    syn::TraitItem::Type(ty) if !test_only(&ty.attrs) => {
                        let mut declaration = ty.clone();
                        declaration.attrs.clear();
                        self.surface("type", &ty.ident.to_string(), declaration);
                    }
                    syn::TraitItem::Const(value) if !test_only(&value.attrs) => {
                        let mut declaration = value.clone();
                        declaration.attrs.clear();
                        self.surface("const", &value.ident.to_string(), declaration);
                    }
                    _ => {}
                }
            }
            self.scope.pop();
        }
        visit::visit_item_trait(self, item);
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        if matches!(item.vis, syn::Visibility::Public(_)) {
            self.surface("type", &item.ident.to_string(), &item.ty);
        }
        visit::visit_item_type(self, item);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        fn exports(audit: &mut Audit, tree: &syn::UseTree, prefix: &str) {
            match tree {
                syn::UseTree::Path(path) => {
                    exports(audit, &path.tree, &format!("{prefix}{}::", path.ident))
                }
                syn::UseTree::Group(group) => {
                    for tree in &group.items {
                        exports(audit, tree, prefix);
                    }
                }
                syn::UseTree::Name(name) => audit.surface(
                    "export",
                    &name.ident.to_string(),
                    quote::quote!(#prefix #tree),
                ),
                syn::UseTree::Rename(name) => audit.surface(
                    "export",
                    &name.rename.to_string(),
                    quote::quote!(#prefix #tree),
                ),
                syn::UseTree::Glob(_) => {
                    let name = prefix.trim_end_matches(':');
                    audit.surface("export", name, quote::quote!(#prefix #tree));
                }
            }
        }
        if matches!(item.vis, syn::Visibility::Public(_)) {
            exports(self, &item.tree, "");
        }
        visit::visit_item_use(self, item);
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        if matches!(item.vis, syn::Visibility::Public(_)) {
            let mut declaration = item.clone();
            declaration.attrs.clear();
            self.surface("const", &item.ident.to_string(), declaration);
        }
        visit::visit_item_const(self, item);
    }

    fn visit_impl_item_const(&mut self, item: &'ast syn::ImplItemConst) {
        if !test_only(&item.attrs) && matches!(item.vis, syn::Visibility::Public(_)) {
            let mut declaration = item.clone();
            declaration.attrs.clear();
            self.surface("const", &item.ident.to_string(), declaration);
        }
        visit::visit_impl_item_const(self, item);
    }

    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        if matches!(item.vis, syn::Visibility::Public(_)) {
            let mut declaration = item.clone();
            declaration.attrs.clear();
            self.surface("static", &item.ident.to_string(), declaration);
        }
        visit::visit_item_static(self, item);
    }

    fn visit_item_union(&mut self, item: &'ast syn::ItemUnion) {
        if matches!(item.vis, syn::Visibility::Public(_)) {
            self.surface("union", &item.ident.to_string(), &item.generics);
            self.scope.push(item.ident.to_string());
            for field in &item.fields.named {
                if !test_only(&field.attrs) && matches!(field.vis, syn::Visibility::Public(_)) {
                    self.surface(
                        "field",
                        &field.ident.as_ref().unwrap().to_string(),
                        &field.ty,
                    );
                }
            }
            self.scope.pop();
        }
        visit::visit_item_union(self, item);
    }

    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        let name = item
            .ident
            .as_ref()
            .unwrap_or(&item.mac.path.segments.last().unwrap().ident)
            .to_string();
        self.surface("macro", &name, &item.mac);
        visit::visit_item_macro(self, item);
    }

    fn visit_impl_item_macro(&mut self, item: &'ast syn::ImplItemMacro) {
        if !test_only(&item.attrs) {
            let name = item.mac.path.segments.last().unwrap().ident.to_string();
            self.surface("macro", &name, &item.mac);
        }
        visit::visit_impl_item_macro(self, item);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac.path.is_ident("include") {
            self.error("include! needs explicit source traversal before it can enter the validation inventory");
        }
        visit::visit_macro(self, mac);
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
        if !self.inventory_only
            && expr.path.segments.last().unwrap().ident == "FormatSpec"
            && !self.format_owner()
        {
            self.error("runtime FormatSpec reconstruction; retain the caller's wire policy");
        }
        visit::visit_expr_struct(self, expr);
    }
}
