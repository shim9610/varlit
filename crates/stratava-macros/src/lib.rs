use proc_macro::TokenStream;
use proc_macro2::{Group, Span, TokenStream as TokenStream2, TokenTree};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Data, DeriveInput, Fields, Ident, LitBool, LitByteStr, LitInt, LitStr, Result, Token, Type,
    Visibility, braced, bracketed, parenthesized, parse_macro_input,
};

mod finite_key;

/// Derives [`StratavaBlock`] — and the codec impls it needs — for one stored
/// record type.
///
/// Re-exported by the `stratava` facade as `stratava::StratavaBlock`; depend on `stratava`,
/// not on this crate directly.
///
/// # Attributes
///
/// The block itself is configured with `#[stratava(...)]` on the struct:
///
/// | Key | Meaning |
/// | --- | --- |
/// | `id = <u32>` | block id, below `0xFFFF_FF00` (ids at or above that are reserved for internal records) |
/// | `version = <u16>` | block version; defaults to `1` |
/// | `kind = "fixed" \| "variable" \| "matrix"` | encoding shape |
/// | `key = "<field>"` | marks the block keyed and names the key field |
/// | `endian = "little" \| "big"` | per-block byte-order override; otherwise the format's byte order applies |
/// | `durability = "immediate" \| "deferred"` | append/delete durability boundary; defaults to deferred |
/// | `immediate_if = "path::predicate"` | pure `fn(&Self) -> bool` condition on appended values |
///
/// Fields of a `variable` block take `#[stratava(field_id = <u32>)]`, which is the
/// wire identity that lets readers skip unknown fields. `fixed` blocks are
/// positional and take no field ids.
///
/// ```no_run
/// #[derive(Clone, Debug, PartialEq, stratava::StratavaBlock)]
/// #[stratava(id = 2, version = 1, kind = "variable", key = "id")]
/// struct User {
///     #[stratava(field_id = 1)]
///     id: u64,
///     #[stratava(field_id = 2)]
///     name: String,
/// }
/// ```
///
/// # Generated schema fingerprint
///
/// The derive computes `StratavaBlock::SCHEMA_FINGERPRINT` deterministically
/// (FNV-1a 64 over the canonical schema: id, version, kind, endian, keyedness,
/// ordered field name/type identities, and each field's resolved codec
/// `SCHEMA_ID`). A manual block that mirrors a derived one must reuse that
/// block's fingerprint constant.
///
/// # Field codec identity
///
/// Every field type must declare a **non-zero** `StratavaEncode::SCHEMA_ID` and
/// `StratavaDecode::SCHEMA_ID`. A field whose codec leaves the trait default `0`
/// fails to compile with a message naming the field. Wrapping the codec in
/// `Option`, `Vec`, an array, a map, or a tuple does not satisfy the rule:
/// container identities fold their elements, so a missing element identity
/// propagates outward. Every built-in codec — including `ChunkedBytes` and
/// `PackedBitmap` — declares one. See `docs/custom-codec-guide.md`.
///
/// # Matrix blocks
///
/// `kind = "matrix"` additionally requires every field to have a width fixed by
/// its type, because a matrix slot needs a compile-time stride. Scalars and
/// fixed arrays of them qualify; anything that owns heap storage does not, and
/// is rejected during const evaluation of the generated `SLOT_STRIDE`.
///
/// [`StratavaBlock`]: https://docs.rs/stratava/latest/stratava/trait.StratavaBlock.html
#[proc_macro_derive(StratavaBlock, attributes(stratava))]
pub fn derive_stratava_block(input: TokenStream) -> TokenStream {
    match expand_stratava_block(parse_macro_input!(input as DeriveInput)) {
        Ok(tokens) => rebrand_facade(tokens).into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Declares a whole binary format: its header identity, resource limits,
/// policies, block set, and a typed API generated from them.
///
/// Re-exported by the `stratava` facade as `stratava::stratava_format!`; depend on
/// `stratava`, not on this crate directly. This is the format-first entry point —
/// the declaration is the single source of truth for the on-disk contract and
/// for the Rust API that reads and writes it.
///
/// ```no_run
/// stratava::stratava_format! {
///     pub format AppFormat {
///         magic: b"APPF";
///         version: 1;
///         limits {
///             file_len: 8_589_934_592;
///             records: 4_000_000;
///             index_bytes: 536_870_912;
///             scan_bytes: 8_589_934_592;
///             record_payload: 67_108_864;
///             logical_payload: 268_435_456;
///             materialized_bytes: 1_073_741_824;
///             segments: 4_000_000;
///             sidecar: 268_435_456;
///             mmap: 8_589_934_592;
///         }
///         endian: little;
///         schema_hash: computed;
///         extension: "appf";
///         blocks {
///             fixed Point(id = 1, version = 1) { x: u32, y: u32 }
///             variable User(id = 2, version = 1, key = [id]) { id: u64, name: String }
///         }
///     }
/// }
///
/// # fn main() {
/// let spec = AppFormat::spec();
/// assert_eq!(spec.magic, *b"APPF");
/// # }
/// ```
///
/// # Sections
///
/// | Section | Meaning |
/// | --- | --- |
/// | `magic`, `version`, `extension` | file header identity |
/// | `limits { … }` | the resource ceilings every read is checked against |
/// | `endian` | format byte order; a block may override it |
/// | `schema_hash` | `computed` (derived from the declaration) or a pinned literal |
/// | `index`, `commit`, `manifest`, `integrity`, `compression` | policies |
/// | `dims`, `commit: cell_bitmap`, `aux` | preallocated-matrix declarations |
/// | `layout { … }` | custom physical layout, for non-Stratava-native byte shapes |
/// | `blocks { … }` | `fixed` / `variable` / `matrix` block declarations |
///
/// # What is generated
///
/// A `FormatSpec` constructor (`AppFormat::spec()`), one block type per
/// declaration with its `StratavaBlock` impl, typed reader/writer handles
/// (`AppFormat::open_reader`, `create_writer_with_dims`, …), and per-block
/// accessors: `push_point`, `points()`, `delete_user`, and for matrix blocks
/// `write_cell`/`commit_cell`/`cell`/`cell_status` plus a key struct such as
/// `CellKey { scan, ch }`.
///
/// Generated code refers to the facade as `::stratava::…` and rewrites that path
/// when the dependency is renamed (`vv = { package = "stratava", … }`), so no
/// `extern crate` alias is needed downstream.
///
/// See `docs/spec.md` ("Macro Contract") for the normative grammar and
/// `docs/format-author-guide.md` for a clause-by-clause walkthrough of it.
#[proc_macro]
pub fn stratava_format(input: TokenStream) -> TokenStream {
    match syn::parse::<FormatInput>(input) {
        Ok(input) => rebrand_facade(expand_format(input)).into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Name under which the calling crate depends on the `stratava` facade, when it
/// differs from the literal `stratava` (API2-05).
///
/// Generated code references the facade as `::stratava::__core::…`, which fails
/// with E0433 when the dependency is renamed (`vv = { package = "stratava", … }`).
/// `proc-macro-crate` resolves the rename from the calling crate's manifest.
/// `FoundCrate::Itself` (the facade's own integration tests, where `::stratava`
/// resolves normally) and resolution errors both fall back to the literal
/// `stratava` name.
fn renamed_facade_ident() -> Option<Ident> {
    match proc_macro_crate::crate_name("stratava") {
        Ok(proc_macro_crate::FoundCrate::Name(name)) if name != "stratava" => {
            Some(Ident::new(&name, proc_macro2::Span::call_site()))
        }
        Ok(proc_macro_crate::FoundCrate::Name(_) | proc_macro_crate::FoundCrate::Itself)
        | Err(_) => None,
    }
}

/// Rewrites path-leading `::stratava` segments in generated tokens to the
/// resolved facade name. No-op in the common non-renamed case.
fn rebrand_facade(tokens: TokenStream2) -> TokenStream2 {
    match renamed_facade_ident() {
        Some(facade) => rebrand_facade_tokens(tokens, &facade),
        None => tokens,
    }
}

/// Whether `ident` can be a path segment immediately preceding a `::`
/// separator, i.e. whether `ident::…` continues a path rather than beginning
/// an absolute one. Path-prefix keywords (`crate`, `super`, `self`, `Self`)
/// and every non-keyword ident qualify; other keywords (`impl`, `as`, `dyn`,
/// `for`, …) cannot precede a path segment, so a `::` after them is
/// path-leading.
fn ident_can_end_path_prefix(ident: &Ident) -> bool {
    let name = ident.to_string();
    matches!(name.as_str(), "crate" | "super" | "self" | "Self")
        || !matches!(
            name.as_str(),
            "as" | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "dyn"
                | "else"
                | "enum"
                | "extern"
                | "false"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "static"
                | "struct"
                | "trait"
                | "true"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
        )
}

/// Replaces every ident spelled `stratava` that begins an absolute path
/// (`::stratava`) with `facade`, recursing into groups.
///
/// Only generated code produces absolute `::stratava` paths: a user crate that
/// renamed the dependency cannot name `::stratava` in the field types or
/// attributes the macros embed, so the rewrite never touches user tokens.
/// Idents preceded by a path prefix (`crate::stratava`, `<T as Tr>::stratava`) or
/// with no leading `::` (`self.stratava`) are left alone.
///
/// A leading `::` is recognized in two shapes:
/// - a 2-colon run whose first colon does not follow a path prefix
///   (statement/expression position: `= ::stratava::…`, `(::stratava::…)`), and
/// - the tail of a 3-or-more-colon run: Rust has no `:::` token, so a run of
///   three or more colons is always a lone `:` (type ascription, struct
///   field, trait bound, …) followed by an absolute-path `::` — the pervasive
///   generated shape `IDENT: ::stratava::__core::…`. `run_is_leading` is latched
///   at the run's first colon (the ascription colon, which follows an ident),
///   so this shape must be accepted by run length rather than by the latch.
fn rebrand_facade_tokens(tokens: TokenStream2, facade: &Ident) -> TokenStream2 {
    let mut output: Vec<TokenTree> = Vec::new();
    // Consecutive `:` puncts seen immediately before the current token, and
    // whether that colon run begins a path rather than continuing one.
    let mut colon_run = 0usize;
    let mut run_is_leading = false;
    // Whether the previous non-colon token can end a path prefix (an ident
    // or the closing `>` of a qualified path).
    let mut prev_ends_path = false;
    // Char of the immediately preceding punct token, to tell the `>` of a
    // qualified path apart from the second half of `->` / `=>`.
    let mut prev_punct: Option<char> = None;
    for tree in tokens {
        match tree {
            TokenTree::Punct(punct) if punct.as_char() == ':' => {
                if colon_run == 0 {
                    run_is_leading = !prev_ends_path;
                }
                colon_run += 1;
                prev_punct = Some(':');
                output.push(TokenTree::Punct(punct));
            }
            TokenTree::Ident(ident) => {
                prev_ends_path = ident_can_end_path_prefix(&ident);
                if colon_run >= 2 && (run_is_leading || colon_run >= 3) && ident == "stratava" {
                    let mut renamed = facade.clone();
                    renamed.set_span(ident.span());
                    output.push(TokenTree::Ident(renamed));
                } else {
                    output.push(TokenTree::Ident(ident));
                }
                colon_run = 0;
                prev_punct = None;
            }
            TokenTree::Group(group) => {
                let rebranded = rebrand_facade_tokens(group.stream(), facade);
                let mut rebuilt = Group::new(group.delimiter(), rebranded);
                rebuilt.set_span(group.span());
                output.push(TokenTree::Group(rebuilt));
                colon_run = 0;
                prev_ends_path = false;
                prev_punct = None;
            }
            TokenTree::Punct(punct) => {
                let char = punct.as_char();
                prev_ends_path = char == '>' && !matches!(prev_punct, Some('-') | Some('='));
                prev_punct = Some(char);
                colon_run = 0;
                output.push(TokenTree::Punct(punct));
            }
            literal @ TokenTree::Literal(_) => {
                colon_run = 0;
                prev_ends_path = false;
                prev_punct = None;
                output.push(literal);
            }
        }
    }
    output.into_iter().collect()
}

fn parse_key_fields(value: &LitStr) -> Result<Vec<Ident>> {
    let raw = value.value();
    if raw.trim().is_empty() {
        return Err(syn::Error::new(
            value.span(),
            "stratava key must name at least one field",
        ));
    }
    let mut fields = Vec::new();
    for part in raw.split(',') {
        let name = part.trim();
        if name.is_empty() {
            return Err(syn::Error::new(
                value.span(),
                "stratava key contains an empty field name",
            ));
        }
        let mut ident = syn::parse_str::<Ident>(name).map_err(|_| {
            syn::Error::new(
                value.span(),
                format!("invalid stratava key field {name:?}; expected a Rust identifier"),
            )
        })?;
        // Re-span in place rather than rebuilding through `Ident::new`, which
        // panics on a raw identifier ("r#type" is not a valid identifier).
        // `set_span` preserves the raw flag and emits a bit-identical token for
        // an ordinary identifier.
        ident.set_span(value.span());
        fields.push(ident);
    }
    Ok(fields)
}

struct FieldDescriptor {
    name: Ident,
    ty: Type,
    field_id: u32,
    default: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    fn parse_inline_test_block(block: TokenStream2) -> Result<InlineBlock> {
        let parser = |input: ParseStream<'_>| parse_inline_block(input);
        syn::parse::Parser::parse2(parser, block)
    }

    #[test]
    fn immediate_declarations_reject_invalid_or_ambiguous_policies() {
        for tokens in [
            quote! { fixed B(id = 1, durability = eventually) { value: u64 } },
            quote! { fixed B(id = 1, durability = immediate, durability = deferred) { value: u64 } },
            quote! { fixed B(id = 1, immediate_if = first, immediate_if = second) { value: u64 } },
        ] {
            assert!(parse_inline_test_block(tokens).is_err());
        }
        for tokens in [
            quote! { #[stratava(id = 1, durability = "eventually")] struct B { value: u64 } },
            quote! { #[stratava(id = 1, durability = "immediate", durability = "deferred")] struct B { value: u64 } },
            quote! { #[stratava(id = 1, kind = "matrix", durability = "immediate")] struct B { value: u64 } },
        ] {
            assert!(expand_stratava_block(syn::parse2(tokens).unwrap()).is_err());
        }
    }

    fn expand_format_source(tokens: TokenStream2) -> String {
        let input: FormatInput = syn::parse2(tokens).expect("format input parses");
        expand_format(input).to_string()
    }

    /// F36: the reader, the writer and BOTH generated traits carry the
    /// visibility the format was declared with. Emitting `pub` unconditionally
    /// made a non-`pub` format's handles reachable from outside the module the
    /// author put them in.
    #[test]
    fn declared_visibility_reaches_the_generated_record_api() {
        let rendered = expand_format_source(quote! {
            pub(crate) format Scoped {
                magic: b"SCOP";
                version: 1;
                endian: little;
                blocks {
                    fixed Point(id = 1) {
                        x: u32,
                    }
                }
            }
        });

        for item in [
            "pub (crate) struct Scoped",
            "pub (crate) trait ScopedRead",
            "pub (crate) trait ScopedWrite",
            "pub (crate) struct ScopedReader",
            "pub (crate) struct ScopedWriter",
        ] {
            assert!(
                rendered.contains(item),
                "expected `{item}` in the expansion"
            );
        }
        // The two traits are the ones a partial fix leaves behind: relaxing the
        // structs alone still exports `ScopedRead`/`ScopedWrite`.
        for leaked in [
            "pub trait ScopedRead",
            "pub trait ScopedWrite",
            "pub struct ScopedReader",
            "pub struct ScopedWriter",
        ] {
            assert!(
                !rendered.contains(leaked),
                "`{leaked}` ignores the declared visibility"
            );
        }
    }

    /// F36, layout surface: the typed layout reader/writer, the per-segment
    /// info and write types, and the caller-field structs they expose.
    #[test]
    fn declared_visibility_reaches_the_generated_layout_api() {
        let rendered = expand_format_source(quote! {
            pub(crate) format ScopedLayout {
                magic: b"SCPL";
                version: 1;
                endian: little;
                preset: none;

                layout {
                    file_header ScopedHeader {
                        bytes signature = b"SCP!";
                        u16 header_version = 1;
                    }

                    segment Chunk repeat until_eof {
                        lead_in ChunkLeadIn {
                            bytes tag = b"CHNK";
                            u32 kind;
                            i64 next_segment_offset =
                                finalize(target = segment_end, relative_to = after_lead_in);
                            i64 raw_data_offset =
                                finalize(target = raw_region_start, relative_to = after_lead_in);
                        }

                        metadata ChunkMetadata;
                        raw_region ChunkRaw;
                    }
                }
            }
        });

        for item in [
            "pub (crate) struct ScopedLayoutLayoutReader",
            "pub (crate) struct ScopedLayoutLayoutWriter",
            "pub (crate) struct ScopedLayoutScopedHeaderLayoutInfo",
            "pub (crate) struct ScopedLayoutChunkLayoutInfo",
            "pub (crate) struct ScopedLayoutChunkLayoutWrite",
            "pub (crate) struct ScopedLayoutChunkLayoutFields",
        ] {
            assert!(
                rendered.contains(item),
                "expected `{item}` in the expansion"
            );
        }
        for leaked in [
            "pub struct ScopedLayoutLayoutReader",
            "pub struct ScopedLayoutLayoutWriter",
            "pub struct ScopedLayoutChunkLayoutInfo",
            "pub struct ScopedLayoutChunkLayoutWrite",
        ] {
            assert!(
                !rendered.contains(leaked),
                "`{leaked}` ignores the declared visibility"
            );
        }
    }

    /// F36 control: a format declared `pub` still generates `pub` items, so the
    /// substitution threads the declaration through rather than narrowing
    /// everything.
    #[test]
    fn public_format_still_generates_public_handles() {
        let rendered = expand_format_source(quote! {
            pub format Open {
                magic: b"OPEN";
                version: 1;
                endian: little;
                blocks {
                    fixed Point(id = 1) {
                        x: u32,
                    }
                }
            }
        });

        for item in [
            "pub trait OpenRead",
            "pub trait OpenWrite",
            "pub struct OpenReader",
            "pub struct OpenWriter",
        ] {
            assert!(
                rendered.contains(item),
                "expected `{item}` in the expansion"
            );
        }
    }

    /// API2-05: absolute facade paths are rewritten to the renamed dependency.
    #[test]
    fn rebrand_rewrites_absolute_facade_paths() {
        let facade = Ident::new("vv", proc_macro2::Span::call_site());
        let tokens = quote! {
            impl ::stratava::__core::StratavaBlock for Foo {
                const ID: u32 = <Bar as ::stratava::__core::StratavaBlock>::ID;
            }
        };
        let rebranded = rebrand_facade_tokens(tokens, &facade).to_string();
        assert!(rebranded.contains(":: vv :: __core :: StratavaBlock"));
        assert!(!rebranded.contains("stratava"));
    }

    /// API2-05 regression: `IDENT: ::stratava::…` merges the type-ascription
    /// colon with the leading `::` into a 3-colon run, whose leading-ness the
    /// first-colon latch misjudges (it follows an ident). Rust has no `:::`
    /// token, so the final `::` of a 3+ colon run always begins an absolute
    /// path and must be rewritten. This is the pervasive generated shape
    /// (`const WIRE_TYPE: ::stratava::__core::WireType`, typed fn params, trait
    /// bounds, struct-literal fields) that the original fix missed.
    #[test]
    fn rebrand_rewrites_absolute_paths_after_type_ascription_colon() {
        let facade = Ident::new("vv", proc_macro2::Span::call_site());
        let tokens = quote! {
            const WIRE_TYPE: ::stratava::__core::WireType =
                ::stratava::__core::WireType::Fixed;
            static SPEC: ::stratava::__core::FormatSpec = make();
            fn probe<T: ::stratava::__core::StratavaBlock>(
                spec: ::stratava::__core::FormatSpec,
            ) -> u32
            where
                T: ::stratava::__core::StratavaKeyedBlock,
            {
                let descriptor = Descriptor {
                    kind: ::stratava::__core::BlockKind::Fixed,
                };
                let bound: ::stratava::__core::WireType = descriptor.kind.wire();
                <T as ::stratava::__core::StratavaBlock>::ID
            }
        };
        let rebranded = rebrand_facade_tokens(tokens, &facade).to_string();
        assert!(
            !rebranded.contains("stratava"),
            "unrewritten facade path survived: {rebranded}"
        );
        assert!(rebranded.contains("WIRE_TYPE : :: vv :: __core :: WireType"));
        assert!(rebranded.contains("spec : :: vv :: __core :: FormatSpec"));
        assert!(rebranded.contains("T : :: vv :: __core :: StratavaKeyedBlock"));
        assert!(rebranded.contains("kind : :: vv :: __core :: BlockKind"));
        assert!(rebranded.contains("< T as :: vv :: __core :: StratavaBlock > :: ID"));
    }

    /// API2-05: user tokens that merely contain the ident `stratava` without a
    /// leading `::` (field access, relative paths, qualified-path members,
    /// strings) survive the rewrite unchanged.
    #[test]
    fn rebrand_leaves_non_facade_tokens_alone() {
        let facade = Ident::new("vv", proc_macro2::Span::call_site());
        let tokens = quote! {
            fn probe(value: crate::stratava::Local, other: some::stratava::Path) {
                let _ = value.stratava;
                let _ = <T as Trait>::stratava;
                let _ = "::stratava::__core";
            }
        };
        let rebranded = rebrand_facade_tokens(tokens.clone(), &facade).to_string();
        assert_eq!(rebranded, tokens.to_string());
    }

    /// API2-05: the rewrite recurses into delimited groups.
    #[test]
    fn rebrand_recurses_into_groups() {
        let facade = Ident::new("renamed_facade", proc_macro2::Span::call_site());
        let tokens = quote! {
            fn body() -> ::stratava::__core::Result<()> {
                ::core::result::Result::Ok(::stratava::__core::noop())
            }
        };
        let rebranded = rebrand_facade_tokens(tokens, &facade).to_string();
        assert!(rebranded.contains(":: renamed_facade :: __core :: Result"));
        assert!(rebranded.contains(":: renamed_facade :: __core :: noop"));
        assert!(rebranded.contains(":: core :: result :: Result :: Ok"));
        assert!(!rebranded.contains(":: stratava"));
    }

    #[test]
    fn inline_key_index_defaults_to_memory() {
        let block = parse_inline_test_block(quote! {
            variable Item(id = 1, key = [id]) { id: u64 }
        })
        .expect("keyed inline block should parse");

        assert!(block.key_index == KeyIndexChoice::Memory);
    }

    #[test]
    fn inline_key_index_rejects_unkeyed_blocks() {
        let error = parse_inline_test_block(quote! {
            fixed Item(id = 1, key_index = memory) { id: u64 }
        })
        .err()
        .expect("unkeyed key_index should fail");

        assert!(error.to_string().contains("key_index requires a keyed"));
    }

    #[test]
    fn inline_key_index_rejects_matrix_blocks() {
        let error = parse_inline_test_block(quote! {
            matrix Cell(
                id = 1,
                dims = [row, column],
                category = data,
                key_index = disk,
            ) { value: u64 }
        })
        .err()
        .expect("matrix key_index should fail");

        assert!(error.to_string().contains("matrix blocks"));
    }

    #[test]
    fn inline_key_index_rejects_unknown_value() {
        let error = parse_inline_test_block(quote! {
            variable Item(id = 1, key = [id], key_index = cached) { id: u64 }
        })
        .err()
        .expect("unknown key_index should fail");

        assert_eq!(error.to_string(), "expected memory or disk");
    }

    #[test]
    fn disk_key_index_generates_indexed_api() {
        let block = parse_inline_test_block(quote! {
            variable Item(id = 1, key = [id], key_index = disk) { id: u64 }
        })
        .expect("disk key_index should parse without a feature");
        let (constructors, api) = high_cardinality_api_tokens(
            &syn::parse_quote!(pub),
            &format_ident!("Test"),
            &[block],
            false,
        );
        let tokens = format!("{constructors} {api}");

        assert!(tokens.contains("TestStreamReader"));
        assert!(tokens.contains("TestIndexedReader"));
        assert!(tokens.contains("get_item"));
        assert!(tokens.contains("push_item"));
        assert!(tokens.contains("push_items"));
        assert!(tokens.contains("delete_item"));
        assert!(tokens.contains("DiskIndexPlan"));
        assert!(tokens.contains("disk_index_plan"));
        assert!(tokens.contains("BatchAppendInfo"));
        assert!(tokens.contains("BatchAppendError"));
        assert!(
            !tokens.contains("high-cardinality-dev"),
            "dependency features must not become downstream cfg predicates"
        );
    }

    #[test]
    fn disk_index_plan_is_sorted_by_block_id() {
        let later = parse_inline_test_block(quote! {
            variable Later(id = 9, key = [id], key_index = disk) { id: u64 }
        })
        .expect("later disk block should parse");
        let earlier = parse_inline_test_block(quote! {
            variable Earlier(id = 2, key = [id], key_index = disk) { id: u64 }
        })
        .expect("earlier disk block should parse");
        let (constructors, _) = high_cardinality_api_tokens(
            &syn::parse_quote!(pub),
            &format_ident!("Test"),
            &[later, earlier],
            false,
        );
        let tokens = constructors.to_string();
        let plan_start = tokens
            .find("pub fn disk_index_plan")
            .expect("generated plan function");
        let plan_end = tokens[plan_start..]
            .find("pub fn open_indexed_reader")
            .expect("reader constructor after plan")
            + plan_start;
        let plan = &tokens[plan_start..plan_end];

        assert!(plan.contains("DiskIndexPlan :: canonical"));
        assert!(
            plan.find("Earlier").expect("earlier descriptor")
                < plan.find("Later").expect("later descriptor")
        );
        assert!(tokens.contains("Self :: disk_index_plan ()"));
    }

    #[test]
    fn generated_batch_method_uses_plural_and_borrowed_iterator() {
        let block = parse_inline_test_block(quote! {
            variable Frame(id = 1) { payload: Vec<u8> }
        })
        .expect("variable block should parse");
        let (_, api) = high_cardinality_api_tokens(
            &syn::parse_quote!(pub),
            &format_ident!("Test"),
            &[block],
            false,
        );
        let tokens = api.to_string();

        assert!(tokens.contains("push_frames"));
        assert!(tokens.contains("BatchOptions"));
        assert!(tokens.contains("BatchAppendInfo"));
        assert!(tokens.contains("BatchAppendError"));
        assert!(tokens.contains("Borrow < Frame >"));
        assert!(tokens.contains("push_iter :: < Frame , _ >"));
    }

    #[test]
    fn keyed_chain_exposes_only_plan_backed_keyed_mutations() {
        let account = parse_inline_test_block(quote! {
            variable Account(id = 1, key = [id]) { id: u64 }
        })
        .expect("memory-keyed block should parse");
        let frame = parse_inline_test_block(quote! {
            variable Frame(id = 2, key = [id], key_index = disk) { id: u64 }
        })
        .expect("disk-keyed block should parse");
        let metadata = parse_inline_test_block(quote! {
            fixed Metadata(id = 3) { value: u64 }
        })
        .expect("unkeyed block should parse");
        let (_, api) = high_cardinality_api_tokens(
            &syn::parse_quote!(pub),
            &format_ident!("Test"),
            &[account, frame, metadata],
            true,
        );
        let tokens = api.to_string();
        let stream_start = tokens
            .find("impl TestStreamWriter")
            .expect("stream writer impl");
        let stream_end = tokens[stream_start..]
            .find("pub struct TestIndexedReader")
            .expect("indexed reader after stream writer")
            + stream_start;
        let stream_writer = &tokens[stream_start..stream_end];
        let indexed_start = tokens
            .find("impl TestIndexedWriter")
            .expect("indexed writer impl");
        let indexed_writer = &tokens[indexed_start..];

        assert!(!stream_writer.contains("push_account"));
        assert!(!stream_writer.contains("delete_account"));
        assert!(!stream_writer.contains("push_frame"));
        assert!(!stream_writer.contains("delete_frame"));
        assert!(stream_writer.contains("push_metadata"));
        assert!(indexed_writer.contains("push_frame"));
        assert!(indexed_writer.contains("push_frames"));
        assert!(indexed_writer.contains("delete_frame"));
        assert!(!indexed_writer.contains("push_account"));
        assert!(!indexed_writer.contains("delete_account"));
        assert!(indexed_writer.contains("push_metadata"));
    }

    #[test]
    fn memory_key_index_generates_only_stream_api() {
        let block = parse_inline_test_block(quote! {
            variable Item(id = 1, key = [id]) { id: u64 }
        })
        .expect("default key index should parse");
        let (constructors, api) = high_cardinality_api_tokens(
            &syn::parse_quote!(pub),
            &format_ident!("Test"),
            &[block],
            false,
        );
        let tokens = format!("{constructors} {api}");

        assert!(tokens.contains("TestStreamReader"));
        assert!(!tokens.contains("TestIndexedReader"));
    }

    #[test]
    fn parses_matrix_format_syntax() {
        let input = syn::parse2::<FormatInput>(quote! {
            pub format AnalysisFormat {
                magic: b"ANALYSIS";
                version: 1;
                limits: trusted_unbounded;
                endian: little;
                schema_hash: computed;
                extension: "vrv";

                dims {
                    n_scans: u32,
                    n_channels: u32,
                }

                commit: cell_bitmap {
                    keyspace = [scan, ch];
                    categories = [analysis];
                    singles = [master_grid];
                    per_channel = [threshold];
                };

                aux {
                    thumbnail: 64,
                }

                blocks {
                    matrix AnalysisCell(id = 10, dims = [scan, ch], category = analysis) {
                        value: u32,
                    }
                }
            }
        })
        .expect("matrix format syntax should parse");

        assert_eq!(input.dims.len(), 2);
        assert!(input.matrix_commit.is_some());
        assert_eq!(input.matrix_aux.len(), 1);
        assert_eq!(input.inline_blocks.len(), 1);
        assert!(matches!(
            input.inline_blocks[0].kind,
            InlineBlockKind::Matrix(_)
        ));
    }
}

#[path = "modules/block_derive.rs"]
mod block_derive;
use block_derive::*;

#[path = "modules/format_parse.rs"]
mod format_parse;
use format_parse::*;

#[path = "modules/format_emit.rs"]
mod format_emit;
use format_emit::*;
