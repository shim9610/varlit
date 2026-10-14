//! block derive implementation, kept within the original parent privacy boundary.
use super::*;

pub(super) fn expand_stratava_block(input: DeriveInput) -> Result<TokenStream2> {
    let ident = input.ident.clone();
    if !input.generics.params.is_empty() || input.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(
            input.generics,
            "StratavaBlock derive does not support generic parameters yet; declare a concrete block type or implement StratavaBlock manually",
        ));
    }
    let mut block_id = None;
    let mut version = quote!(1u16);
    let mut version_value: u16 = 1;
    let mut kind = quote!(::stratava::__core::BlockKind::Fixed);
    let mut kind_name = "fixed";
    let mut variable_block = false;
    let mut endian = quote!(::core::option::Option::None);
    let mut endian_name = "default";
    let mut immediate = false;
    let mut immediate_seen = false;
    let mut immediate_if: Option<syn::Path> = None;
    let mut key_fields: Vec<Ident> = Vec::new();

    for attr in &input.attrs {
        if !attr.path().is_ident("stratava") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("id") {
                let value: LitInt = meta.value()?.parse()?;
                block_id = Some(value.base10_parse::<u32>()?);
                Ok(())
            } else if meta.path.is_ident("version") {
                let value: LitInt = meta.value()?.parse()?;
                let parsed = value.base10_parse::<u16>()?;
                version = quote!(#parsed);
                version_value = parsed;
                Ok(())
            } else if meta.path.is_ident("kind") {
                let value: LitStr = meta.value()?.parse()?;
                kind = match value.value().as_str() {
                    "fixed" => {
                        variable_block = false;
                        kind_name = "fixed";
                        quote!(::stratava::__core::BlockKind::Fixed)
                    }
                    "matrix" => {
                        variable_block = false;
                        kind_name = "matrix";
                        quote!(::stratava::__core::BlockKind::Matrix)
                    }
                    "variable" => {
                        variable_block = true;
                        kind_name = "variable";
                        quote!(::stratava::__core::BlockKind::Variable)
                    }
                    other => {
                        return Err(meta.error(format!(
                            "unsupported stratava kind {other:?}; use \"fixed\", \"variable\", or \"matrix\""
                        )));
                    }
                };
                Ok(())
            } else if meta.path.is_ident("endian") {
                let value: LitStr = meta.value()?.parse()?;
                endian = match value.value().as_str() {
                    "little" => {
                        endian_name = "little";
                        quote!(::core::option::Option::Some(::stratava::__core::Endian::Little))
                    }
                    "big" => {
                        endian_name = "big";
                        quote!(::core::option::Option::Some(::stratava::__core::Endian::Big))
                    }
                    other => {
                        return Err(meta.error(format!(
                            "unsupported endian {other:?}; use \"little\" or \"big\""
                        )));
                    }
                };
                Ok(())
            } else if meta.path.is_ident("durability") {
                if immediate_seen { return Err(meta.error("duplicate durability")); }
                immediate_seen = true;
                let value: LitStr = meta.value()?.parse()?;
                immediate = match value.value().as_str() {
                    "immediate" => true,
                    "deferred" => false,
                    _ => return Err(meta.error("expected immediate or deferred")),
                };
                Ok(())
            } else if meta.path.is_ident("immediate_if") {
                if immediate_if.is_some() { return Err(meta.error("duplicate immediate_if")); }
                let value: LitStr = meta.value()?.parse()?;
                immediate_if = Some(value.parse()?);
                Ok(())
            } else if meta.path.is_ident("key") {
                let value: LitStr = meta.value()?.parse()?;
                key_fields = parse_key_fields(&value)?;
                Ok(())
            } else {
                Err(meta.error("unsupported stratava block attribute"))
            }
        })?;
    }

    if kind_name == "matrix" && (immediate_seen || immediate_if.is_some()) {
        return Err(syn::Error::new_spanned(
            &ident,
            "block Immediate policies require an append block; use the ordered matrix durability API",
        ));
    }
    let immediate_method = immediate_if.map(|condition| {
        quote! {
            fn immediate_if(&self) -> bool { #condition(self) }
        }
    });

    let block_id =
        block_id.ok_or_else(|| syn::Error::new_spanned(&ident, "missing #[stratava(id = ...)]"))?;
    if block_id >= 0xFFFF_FF00 {
        return Err(syn::Error::new_spanned(
            &ident,
            "stratava block id is reserved; use an id below 0xFFFF_FF00",
        ));
    }
    for (index, key) in key_fields.iter().enumerate() {
        if key_fields[(index + 1)..].iter().any(|other| other == key) {
            return Err(syn::Error::new_spanned(key, "duplicate stratava key field"));
        }
    }
    let fields = match input.data {
        Data::Struct(data) => match data.fields {
            Fields::Named(fields) => fields.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    ident,
                    "StratavaBlock only supports structs with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                ident,
                "StratavaBlock only supports structs",
            ));
        }
    };

    let mut descriptors = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        let name = field.ident.clone().expect("named field");
        let ty = field.ty.clone();
        let mut field_id = (index as u32) + 1;
        let mut default = false;
        for attr in &field.attrs {
            if !attr.path().is_ident("stratava") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("field_id") {
                    let value: LitInt = meta.value()?.parse()?;
                    field_id = value.base10_parse::<u32>()?;
                    Ok(())
                } else if meta.path.is_ident("default") {
                    default = true;
                    Ok(())
                } else {
                    Err(meta.error("unsupported stratava field attribute"))
                }
            })?;
        }
        if default && !variable_block {
            return Err(syn::Error::new_spanned(
                &name,
                "#[stratava(default)] is only supported on variable blocks",
            ));
        }
        descriptors.push(FieldDescriptor {
            name,
            ty,
            field_id,
            default,
        });
    }
    for (index, field) in descriptors.iter().enumerate() {
        if field.field_id == 0 {
            return Err(syn::Error::new_spanned(
                &field.name,
                "stratava field_id must be non-zero",
            ));
        }
        if descriptors[(index + 1)..]
            .iter()
            .any(|other| other.field_id == field.field_id)
        {
            return Err(syn::Error::new_spanned(
                &field.name,
                "duplicate stratava field_id",
            ));
        }
    }

    let fixed_encode = descriptors.iter().map(|field| {
        let name = &field.name;
        quote!(::stratava::__core::StratavaEncode::encode_stratava(&self.#name, encoder)?;)
    });
    let field_descriptors = descriptors.iter().map(|field| {
        let name = field.name.to_string();
        let ty = &field.ty;
        let field_id = field.field_id;
        let presence = if field.default {
            quote!(::stratava::__core::FieldPresence::Defaulted)
        } else {
            quote!(::stratava::__core::FieldPresence::Required)
        };
        quote! {
            ::stratava::__core::FieldDescriptor {
                id: #field_id,
                name: #name,
                wire_type: <#ty as ::stratava::__core::StratavaEncode>::WIRE_TYPE,
                presence: #presence,
            }
        }
    });
    let fixed_decode = descriptors.iter().map(|field| {
        let name = &field.name;
        let ty = &field.ty;
        quote!(#name: <#ty as ::stratava::__core::StratavaDecode>::decode_stratava(decoder)?)
    });

    let variable_encode = descriptors.iter().map(|field| {
        let name = &field.name;
        let ty = &field.ty;
        let field_id = field.field_id;
        quote! {
            // DEF-02: the nested field encoder inherits the parent encoder's
            // remaining output budget, so a limit-bounded writer entry point
            // surfaces its typed limit error before an oversized field is
            // ever fully buffered, instead of after staging the whole child
            // encoding.
            let payload = encoder.encode_nested_to_vec(&self.#name)?;
            ::stratava::__core::write_field(
                encoder,
                #field_id,
                <#ty as ::stratava::__core::StratavaEncode>::WIRE_TYPE,
                &payload,
            )?;
        }
    });

    let option_vars = descriptors.iter().map(|field| {
        let var = option_ident(&field.name);
        let ty = &field.ty;
        quote!(let mut #var: ::core::option::Option<#ty> = ::core::option::Option::None;)
    });
    let match_arms = descriptors.iter().map(|field| {
        let var = option_ident(&field.name);
        let ty = &field.ty;
        let field_id = field.field_id;
        let field_name = field.name.to_string();
        quote! {
            #field_id => {
                if header.wire_type != <#ty as ::stratava::__core::StratavaDecode>::WIRE_TYPE {
                    return Err(::stratava::__core::Error::WireTypeMismatch {
                        field: #field_name,
                        expected: <#ty as ::stratava::__core::StratavaDecode>::WIRE_TYPE,
                        actual: header.wire_type,
                    });
                }
                #var = ::core::option::Option::Some(
                    decoder.decode_nested::<#ty>(payload)?
                );
            }
        }
    });
    let build_fields = descriptors.iter().map(|field| {
        let name = &field.name;
        let var = option_ident(name);
        let field_id = field.field_id;
        let field_name = name.to_string();
        if field.default {
            quote!(#name: #var.unwrap_or_default())
        } else {
            quote! {
                #name: #var.ok_or(::stratava::__core::Error::MissingField {
                    field: #field_name,
                    field_id: #field_id,
                })?
            }
        }
    });

    let encode_body = quote! {
        match <Self as ::stratava::__core::StratavaBlock>::KIND {
            ::stratava::__core::BlockKind::Fixed | ::stratava::__core::BlockKind::Matrix => {
                #(#fixed_encode)*
            }
            ::stratava::__core::BlockKind::Variable => {
                #(#variable_encode)*
            }
            ::stratava::__core::BlockKind::Internal => {}
        }
        ::core::result::Result::Ok(())
    };

    let decode_body = quote! {
        match <Self as ::stratava::__core::StratavaBlock>::KIND {
            ::stratava::__core::BlockKind::Fixed | ::stratava::__core::BlockKind::Matrix => {
                ::core::result::Result::Ok(Self { #(#fixed_decode,)* })
            }
            ::stratava::__core::BlockKind::Variable => {
                #(#option_vars)*
                while decoder.remaining() > 0 {
                    let header = ::stratava::__core::read_field_header(decoder)?;
                    let payload = decoder.read_exact(header.payload_len as usize)?;
                    match header.field_id {
                        #(#match_arms)*
                        _ => {}
                    }
                }
                ::core::result::Result::Ok(Self { #(#build_fields,)* })
            }
            ::stratava::__core::BlockKind::Internal => unreachable!("user blocks cannot be internal"),
        }
    };

    let keyed_impl = if key_fields.is_empty() {
        quote!()
    } else {
        let mut key_types = Vec::new();
        for key in &key_fields {
            let ty = descriptors
                .iter()
                .find(|field| field.name == *key)
                .map(|field| field.ty.clone())
                .ok_or_else(|| syn::Error::new_spanned(key, "key field does not exist"))?;
            key_types.push(ty);
        }
        let key_type = if key_types.len() == 1 {
            let ty = &key_types[0];
            quote!(#ty)
        } else {
            quote!((#(#key_types,)*))
        };
        let key_value = if key_fields.len() == 1 {
            let key = &key_fields[0];
            quote!(self.#key.clone())
        } else {
            quote!((#(self.#key_fields.clone(),)*))
        };
        // The borrowing counterpart of `key_value`: the same fields, compared
        // in place. `key()` clones every key field, so a caller that only wants
        // to know whether the key matches allocated once per heap-typed field
        // and dropped the answer. Generated field by field rather than as
        // `&self.key() == other`, which is exactly the clone being avoided.
        let key_eq_value = if key_fields.len() == 1 {
            let key = &key_fields[0];
            quote!(self.#key == *other)
        } else {
            let comparisons = key_fields.iter().enumerate().map(|(position, key)| {
                let index = syn::Index::from(position);
                quote!(self.#key == other.#index)
            });
            quote!(#(#comparisons)&&*)
        };
        quote! {
            impl ::stratava::__core::StratavaKeyedBlock for #ident {
                type Key = #key_type;

                fn key(&self) -> Self::Key {
                    #key_value
                }

                fn key_eq(&self, other: &Self::Key) -> bool {
                    #key_eq_value
                }
            }
        }
    };
    let is_keyed = !key_fields.is_empty();
    let codec_identity_asserts = field_codec_identity_asserts(&descriptors);
    let schema_fingerprint = schema_fingerprint(
        block_id,
        version_value,
        kind_name,
        endian_name,
        is_keyed,
        &descriptors,
    );
    let replace_impl = if key_fields.is_empty() {
        quote! {
            impl ::stratava::__core::StratavaReplaceBlock for #ident {
                fn validate_replacement(
                    _old: &Self,
                    _new: &Self,
                ) -> ::stratava::__core::Result<()> {
                    ::core::result::Result::Ok(())
                }
            }
        }
    } else {
        quote! {
            impl ::stratava::__core::StratavaReplaceBlock for #ident {
                fn validate_replacement(
                    old: &Self,
                    new: &Self,
                ) -> ::stratava::__core::Result<()> {
                    if <Self as ::stratava::__core::StratavaKeyedBlock>::key(old)
                        != <Self as ::stratava::__core::StratavaKeyedBlock>::key(new)
                    {
                        return ::core::result::Result::Err(
                            ::stratava::__core::Error::ReplacementKeyMismatch,
                        );
                    }
                    ::core::result::Result::Ok(())
                }
            }
        }
    };

    Ok(quote! {
        #(#codec_identity_asserts)*

        impl ::stratava::__core::StratavaEncode for #ident {
            const WIRE_TYPE: ::stratava::__core::WireType = ::stratava::__core::WireType::Nested;
            const SCHEMA_ID: u64 = <Self as ::stratava::__core::StratavaBlock>::SCHEMA_FINGERPRINT;

            fn encode_stratava(&self, encoder: &mut ::stratava::__core::Encoder) -> ::stratava::__core::Result<()> {
                #encode_body
            }
        }

        impl ::stratava::__core::StratavaDecode for #ident {
            const WIRE_TYPE: ::stratava::__core::WireType = ::stratava::__core::WireType::Nested;
            const SCHEMA_ID: u64 = <Self as ::stratava::__core::StratavaBlock>::SCHEMA_FINGERPRINT;

            fn decode_stratava(decoder: &mut ::stratava::__core::Decoder<'_>) -> ::stratava::__core::Result<Self> {
                #decode_body
            }
        }

        impl ::stratava::__core::StratavaBlock for #ident {
            const ID: u32 = #block_id;
            const VERSION: u16 = #version;
            const KIND: ::stratava::__core::BlockKind = #kind;
            const ENDIAN: ::core::option::Option<::stratava::__core::Endian> = #endian;
            const IS_KEYED: bool = #is_keyed;
            const IMMEDIATE: bool = #immediate;
            #immediate_method
            const SCHEMA_FINGERPRINT: u64 = #schema_fingerprint;
            const FIELDS: &'static [::stratava::__core::FieldDescriptor] = &[
                #(#field_descriptors,)*
            ];
        }

        #keyed_impl
        #replace_impl
    })
}

pub(super) fn option_ident(name: &Ident) -> Ident {
    format_ident!("__stratava_field_{}", name)
}

/// Deterministic fingerprint of the canonical block schema, emitted as a const
/// expression the compiler folds in the caller's crate.
///
/// FNV-1a 64 over a canonical byte string is expanded inline because the
/// standard `DefaultHasher` output is not stability-guaranteed. The value is a
/// process-local schema identity, never part of the wire format.
///
/// API-04: each field contributes `<FieldTy as StratavaEncode>::SCHEMA_ID` and
/// its decode counterpart — resolved codec identities — alongside the source
/// spelling, so two custom nested codecs that spell the field type identically
/// but emit different bytes cannot share a fingerprint. That is why the
/// fingerprint can no longer be a literal: the field identities are known only
/// after type resolution.
pub(super) fn schema_fingerprint(
    block_id: u32,
    version: u16,
    kind: &str,
    endian: &str,
    keyed: bool,
    fields: &[FieldDescriptor],
) -> TokenStream2 {
    // Versioned schema domain: preserve this across project/package renames.
    let header = LitByteStr::new(
        format!(
            "varve:block-schema:v2|id={block_id}|version={version}|kind={kind}|endian={endian}|keyed={keyed}"
        )
        .as_bytes(),
        Span::call_site(),
    );
    let field_steps = fields.iter().map(|field| {
        let name = &field.name;
        let ty = &field.ty;
        let type_identity = quote!(#ty).to_string();
        let field_id = field.field_id;
        let presence = if field.default {
            "defaulted"
        } else {
            "required"
        };
        let literal = LitByteStr::new(
            format!(
                "|field:id={field_id},name={name},type={type_identity},presence={presence},codec="
            )
            .as_bytes(),
            Span::call_site(),
        );
        quote! {
            __stratava_acc = __stratava_schema_bytes(__stratava_acc, #literal);
            __stratava_acc = __stratava_schema_u64(
                __stratava_acc,
                <#ty as ::stratava::__core::StratavaEncode>::SCHEMA_ID,
            );
            __stratava_acc = __stratava_schema_u64(
                __stratava_acc,
                <#ty as ::stratava::__core::StratavaDecode>::SCHEMA_ID,
            );
            __stratava_acc = __stratava_schema_u64(
                __stratava_acc,
                <#ty as ::stratava::__core::StratavaEncode>::WIRE_TYPE as u16 as u64,
            );
        }
    });
    quote! {
        {
            const fn __stratava_schema_bytes(acc: u64, bytes: &[u8]) -> u64 {
                let mut acc = acc;
                let mut index = 0;
                while index < bytes.len() {
                    acc ^= bytes[index] as u64;
                    acc = acc.wrapping_mul(0x0000_0100_0000_01b3u64);
                    index += 1;
                }
                acc
            }
            const fn __stratava_schema_u64(acc: u64, value: u64) -> u64 {
                __stratava_schema_bytes(acc, &value.to_le_bytes())
            }
            let mut __stratava_acc = __stratava_schema_bytes(0xcbf2_9ce4_8422_2325u64, #header);
            #(#field_steps)*
            __stratava_acc
        }
    }
}

/// Compile-time enforcement of the `StratavaEncode::SCHEMA_ID` trust boundary for
/// every field codec: a field type whose codec declares no identity would
/// fingerprint by source spelling alone, which is exactly the API-04 collision.
///
/// The rule is deliberately wire-type agnostic. Restricting it to
/// `WireType::Nested` left the same collision reachable through any other wire
/// type - two `Packed` codecs spelled identically, both `WireType::U64`, both
/// identity-less, emitting different bytes - so the assertion demands an
/// identity from every field type. Built-in scalars, built-in containers and
/// derived blocks all declare one, so this only fires on hand-written codecs,
/// including ones reached through a container (`container_schema_id`
/// propagates the missing identity outward as zero).
pub(super) fn field_codec_identity_asserts(fields: &[FieldDescriptor]) -> Vec<TokenStream2> {
    fields
        .iter()
        .map(|field| {
            let ty = &field.ty;
            let name = field.name.to_string();
            let message = format!(
                "stratava field `{name}` uses a custom codec that declares no SCHEMA_ID; a hand-written codec must declare a schema identity that changes whenever its encoded bytes change (wrapping it in Option/Vec/array/map/tuple does not supply one)"
            );
            quote! {
                const _: () = ::core::assert!(
                    <#ty as ::stratava::__core::StratavaEncode>::SCHEMA_ID != 0,
                    #message
                );
                const _: () = ::core::assert!(
                    <#ty as ::stratava::__core::StratavaDecode>::SCHEMA_ID != 0,
                    #message
                );
            }
        })
        .collect()
}
