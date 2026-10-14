//! format emit implementation, kept within the original parent privacy boundary.
use super::*;

pub(super) fn expand_format(input: FormatInput) -> TokenStream2 {
    let vis = input.vis;
    let name = input.name;
    let magic = input.magic;
    let version = input.version;
    let (schema_hash, schema_hash_step) = match input.schema_hash {
        SchemaHashChoice::Literal(value) => (value, quote!()),
        SchemaHashChoice::Computed => (0u64, quote!(.with_computed_schema_hash())),
    };
    let extension = input
        .extension
        .map(|extension| quote!(::core::option::Option::Some(#extension)))
        .unwrap_or_else(|| quote!(::core::option::Option::None));
    let endian = match input.endian {
        EndianChoice::Little => quote!(::stratava::__core::Endian::Little),
        EndianChoice::Big => quote!(::stratava::__core::Endian::Big),
    };
    let keyed_offset_chain = input.index.keyed_offset_chain;
    let index = index_tokens(input.index);
    let commit = commit_tokens(input.commit);
    let integrity = match input.integrity {
        IntegrityChoice::None => quote!(::stratava::__core::IntegrityPolicy::None),
        IntegrityChoice::Crc32 => quote!(::stratava::__core::IntegrityPolicy::Crc32),
        IntegrityChoice::Crc32WithHeader => {
            quote!(::stratava::__core::IntegrityPolicy::Crc32WithHeader)
        }
    };
    let liveness = match input.liveness {
        LivenessChoice::None => quote!(::stratava::__core::LivenessPolicy::None),
        LivenessChoice::FooterFlags => quote!(::stratava::__core::LivenessPolicy::FooterFlags),
    };
    let header_slots = match &input.header_slots {
        None => quote!(::stratava::__core::HeaderSlots::NONE),
        Some(slots) => {
            let capacity = slots.capacity;
            let integrity = match slots.integrity {
                HeaderSlotIntegrityChoice::None => {
                    quote!(::stratava::__core::HeaderSlotIntegrity::None)
                }
                HeaderSlotIntegrityChoice::Rolling => {
                    quote!(::stratava::__core::HeaderSlotIntegrity::Rolling)
                }
                HeaderSlotIntegrityChoice::Sealed => {
                    quote!(::stratava::__core::HeaderSlotIntegrity::Sealed)
                }
            };
            let ids = slots
                .blocks
                .iter()
                .map(|ty| quote!(<#ty as ::stratava::__core::StratavaBlock>::ID));
            // A named const rather than a bare `&[..]`: the slice must be
            // `&'static`, and relying on const promotion of an associated-const
            // read is a rule this does not need to depend on.
            quote!({
                const HEADER_SLOT_BLOCK_IDS: &[u32] = &[#(#ids,)*];
                ::stratava::__core::HeaderSlots::new(
                    #capacity,
                    #integrity,
                    HEADER_SLOT_BLOCK_IDS,
                )
            })
        }
    };
    let recovery = match input.recovery {
        RecoveryChoice::Strict => quote!(::stratava::__core::RecoveryPolicy::Strict),
        RecoveryChoice::TruncateTail => quote!(::stratava::__core::RecoveryPolicy::TruncateTail),
        RecoveryChoice::MarkTail => quote!(::stratava::__core::RecoveryPolicy::MarkTail),
    };
    let manifest = match input.manifest {
        ManifestChoice::None => quote!(::stratava::__core::ManifestPolicy::None),
        ManifestChoice::Embedded => quote!(::stratava::__core::ManifestPolicy::Embedded),
    };
    let compression = compression_tokens(input.compression);
    let read_limits = read_limits_tokens(input.limits);
    let dims = input.dims;
    let matrix_commit = input.matrix_commit;
    let matrix_aux = input.matrix_aux;
    let layout_file_header = input.layout_file_header;
    let layout_segments = input.layout_segments;
    let typed_api_enabled = input.typed_api;
    let layout = layout_tokens(
        input.layout_preset,
        layout_file_header.as_ref(),
        &layout_segments,
    );
    let registry_blocks = input.registry_blocks;
    let inline_blocks = input.inline_blocks;
    let inline_block_defs = inline_blocks
        .iter()
        .map(|block| inline_block_tokens(&vis, block, &dims));
    let matrix_type_defs = matrix_type_tokens(&vis, &name, &dims, &inline_blocks);
    let inline_block_types = inline_blocks.iter().map(|block| {
        let block = &block.name;
        syn::parse_quote!(#block)
    });
    let blocks: Vec<Type> = registry_blocks
        .into_iter()
        .chain(inline_block_types)
        .collect();

    let descriptors = blocks.iter().map(|block| {
        quote! {
            ::stratava::__core::BlockDescriptor {
                id: <#block as ::stratava::__core::StratavaBlock>::ID,
                name: stringify!(#block),
                version: <#block as ::stratava::__core::StratavaBlock>::VERSION,
                kind: <#block as ::stratava::__core::StratavaBlock>::KIND,
                fields: <#block as ::stratava::__core::StratavaBlock>::FIELDS,
            }
        }
    });
    // Per-block schema identities folded into the computed schema hash:
    // endian override, keyedness, and the generated codec fingerprint
    // (API2-01). `BlockDescriptor` alone does not carry these.
    let block_identities = blocks.iter().map(|block| {
        quote! {
            (
                <#block as ::stratava::__core::StratavaBlock>::ID,
                <#block as ::stratava::__core::StratavaBlock>::ENDIAN,
                <#block as ::stratava::__core::StratavaBlock>::IS_KEYED,
                <#block as ::stratava::__core::StratavaBlock>::SCHEMA_FINGERPRINT,
            )
        }
    });

    let duplicate_asserts = pairwise(&blocks).into_iter().map(|(left, right)| {
        quote! {
            const _: () = assert!(
                <#left as ::stratava::__core::StratavaBlock>::ID
                    != <#right as ::stratava::__core::StratavaBlock>::ID
            );
        }
    });
    let typed_api = if typed_api_enabled {
        typed_api_tokens(
            &vis,
            &name,
            &inline_blocks,
            matrix_commit.as_ref(),
            &matrix_aux,
        )
    } else {
        quote!()
    };
    let (high_cardinality_constructors, high_cardinality_api) = if typed_api_enabled {
        high_cardinality_api_tokens(&vis, &name, &inline_blocks, keyed_offset_chain)
    } else {
        (quote!(), quote!())
    };
    let layout_typed_api = if typed_api_enabled && !layout_segments.is_empty() {
        layout_typed_api_tokens(&vis, &name, layout_file_header.as_ref(), &layout_segments)
    } else {
        quote!()
    };
    let writer_return = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name)
    } else {
        quote!(::stratava::__core::StratavaWriter)
    };
    let reader_return = if typed_api_enabled {
        let reader_name = format_ident!("{}Reader", name);
        quote!(#reader_name)
    } else {
        quote!(::stratava::__core::StratavaReader)
    };
    let has_typed_layout_api = typed_api_enabled && !layout_segments.is_empty();
    let layout_writer_return = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(#writer_name)
    } else {
        quote!(::stratava::__core::LayoutWriter)
    };
    let layout_reader_return = if has_typed_layout_api {
        let reader_name = format_ident!("{}LayoutReader", name);
        quote!(#reader_name)
    } else {
        quote!(::stratava::__core::LayoutReader)
    };
    let create_layout_writer_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer(path)?)))
    } else {
        quote!(Self::spec().create_layout_writer(path))
    };
    let create_layout_writer_with_limits_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_with_limits(path, limits)?)))
    } else {
        quote!(Self::spec().create_layout_writer_with_limits(path, limits))
    };
    let create_layout_writer_with_resource_limits_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_with_resource_limits(path, limits)?)))
    } else {
        quote!(Self::spec().create_layout_writer_with_resource_limits(path, limits))
    };
    let create_layout_writer_trusted_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_trusted_unbounded(path)?)))
    } else {
        quote!(Self::spec().create_layout_writer_trusted_unbounded(path))
    };
    let create_layout_writer_with_header_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_with_header(path, fields)?)))
    } else {
        quote!(Self::spec().create_layout_writer_with_header(path, fields))
    };
    let create_layout_writer_with_header_and_limits_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_with_header_and_limits(path, fields, limits)?)))
    } else {
        quote!(Self::spec().create_layout_writer_with_header_and_limits(path, fields, limits))
    };
    let create_layout_writer_with_header_and_resource_limits_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_with_header_and_resource_limits(path, fields, limits)?)))
    } else {
        quote!(
            Self::spec().create_layout_writer_with_header_and_resource_limits(path, fields, limits)
        )
    };
    let create_layout_writer_with_header_trusted_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().create_layout_writer_with_header_trusted_unbounded(path, fields)?)))
    } else {
        quote!(Self::spec().create_layout_writer_with_header_trusted_unbounded(path, fields))
    };
    let open_layout_writer_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().open_layout_writer(path)?)))
    } else {
        quote!(Self::spec().open_layout_writer(path))
    };
    let open_layout_writer_with_limits_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().open_layout_writer_with_limits(path, limits)?)))
    } else {
        quote!(Self::spec().open_layout_writer_with_limits(path, limits))
    };
    let open_layout_writer_with_resource_limits_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().open_layout_writer_with_resource_limits(path, limits)?)))
    } else {
        quote!(Self::spec().open_layout_writer_with_resource_limits(path, limits))
    };
    let open_layout_writer_trusted_body = if has_typed_layout_api {
        let writer_name = format_ident!("{}LayoutWriter", name);
        quote!(::core::result::Result::Ok(#writer_name::from_inner(Self::spec().open_layout_writer_trusted_unbounded(path)?)))
    } else {
        quote!(Self::spec().open_layout_writer_trusted_unbounded(path))
    };
    let open_layout_reader_body = if has_typed_layout_api {
        let reader_name = format_ident!("{}LayoutReader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_layout_reader(path)?)))
    } else {
        quote!(Self::spec().open_layout_reader(path))
    };
    let open_layout_reader_with_limits_body = if has_typed_layout_api {
        let reader_name = format_ident!("{}LayoutReader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_layout_reader_with_limits(path, limits)?)))
    } else {
        quote!(Self::spec().open_layout_reader_with_limits(path, limits))
    };
    let open_layout_reader_with_resource_limits_body = if has_typed_layout_api {
        let reader_name = format_ident!("{}LayoutReader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_layout_reader_with_resource_limits(path, limits)?)))
    } else {
        quote!(Self::spec().open_layout_reader_with_resource_limits(path, limits))
    };
    let open_layout_reader_trusted_body = if has_typed_layout_api {
        let reader_name = format_ident!("{}LayoutReader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_layout_reader_trusted_unbounded(path)?)))
    } else {
        quote!(Self::spec().open_layout_reader_trusted_unbounded(path))
    };
    let create_writer_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().create_writer(path)?))
    } else {
        quote!(Self::spec().create_writer(path))
    };
    let create_writer_with_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().create_writer_with_limits(path, limits)?))
    } else {
        quote!(Self::spec().create_writer_with_limits(path, limits))
    };
    let create_writer_with_resource_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().create_writer_with_resource_limits(path, limits)?))
    } else {
        quote!(Self::spec().create_writer_with_resource_limits(path, limits))
    };
    let create_writer_trusted_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().create_writer_trusted_unbounded(path)?))
    } else {
        quote!(Self::spec().create_writer_trusted_unbounded(path))
    };
    let open_writer_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_writer(path)?))
    } else {
        quote!(Self::spec().open_writer(path))
    };
    let open_writer_with_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_writer_with_limits(path, limits)?))
    } else {
        quote!(Self::spec().open_writer_with_limits(path, limits))
    };
    let open_writer_with_resource_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_writer_with_resource_limits(path, limits)?))
    } else {
        quote!(Self::spec().open_writer_with_resource_limits(path, limits))
    };
    let open_writer_trusted_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_writer_trusted_unbounded(path)?))
    } else {
        quote!(Self::spec().open_writer_trusted_unbounded(path))
    };
    let open_recover_writer_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_recover_writer(path)?))
    } else {
        quote!(Self::spec().open_recover_writer(path))
    };
    let open_recover_writer_with_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_recover_writer_with_limits(path, limits)?))
    } else {
        quote!(Self::spec().open_recover_writer_with_limits(path, limits))
    };
    let open_recover_writer_with_resource_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_recover_writer_with_resource_limits(path, limits)?))
    } else {
        quote!(Self::spec().open_recover_writer_with_resource_limits(path, limits))
    };
    let open_recover_writer_trusted_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote!(#writer_name::from_inner(Self::spec().open_recover_writer_trusted_unbounded(path)?))
    } else {
        quote!(Self::spec().open_recover_writer_trusted_unbounded(path))
    };
    let open_recover_writer_report_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote! {
            {
                let (writer, report) = Self::spec().open_recover_writer_with_report(path)?;
                ::core::result::Result::Ok((#writer_name::from_inner(writer)?, report))
            }
        }
    } else {
        quote!(Self::spec().open_recover_writer_with_report(path))
    };
    let open_recover_writer_report_with_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote! {
            {
                let (writer, report) = Self::spec()
                    .open_recover_writer_with_report_and_limits(path, limits)?;
                ::core::result::Result::Ok((#writer_name::from_inner(writer)?, report))
            }
        }
    } else {
        quote!(Self::spec().open_recover_writer_with_report_and_limits(path, limits))
    };
    let open_recover_writer_report_with_resource_limits_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote! {
            {
                let (writer, report) = Self::spec()
                    .open_recover_writer_with_report_and_resource_limits(path, limits)?;
                ::core::result::Result::Ok((#writer_name::from_inner(writer)?, report))
            }
        }
    } else {
        quote!(Self::spec().open_recover_writer_with_report_and_resource_limits(path, limits))
    };
    let open_recover_writer_report_trusted_body = if typed_api_enabled {
        let writer_name = format_ident!("{}Writer", name);
        quote! {
            {
                let (writer, report) = Self::spec()
                    .open_recover_writer_with_report_trusted_unbounded(path)?;
                ::core::result::Result::Ok((#writer_name::from_inner(writer)?, report))
            }
        }
    } else {
        quote!(Self::spec().open_recover_writer_with_report_trusted_unbounded(path))
    };
    let open_reader_body = if typed_api_enabled {
        let reader_name = format_ident!("{}Reader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_reader(path)?)))
    } else {
        quote!(Self::spec().open_reader(path))
    };
    let open_reader_with_limits_body = if typed_api_enabled {
        let reader_name = format_ident!("{}Reader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_reader_with_limits(path, limits)?)))
    } else {
        quote!(Self::spec().open_reader_with_limits(path, limits))
    };
    let open_reader_with_resource_limits_body = if typed_api_enabled {
        let reader_name = format_ident!("{}Reader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_reader_with_resource_limits(path, limits)?)))
    } else {
        quote!(Self::spec().open_reader_with_resource_limits(path, limits))
    };
    let open_reader_trusted_body = if typed_api_enabled {
        let reader_name = format_ident!("{}Reader", name);
        quote!(::core::result::Result::Ok(#reader_name::from_inner(Self::spec().open_reader_trusted_unbounded(path)?)))
    } else {
        quote!(Self::spec().open_reader_trusted_unbounded(path))
    };
    let matrix_spec_step =
        matrix_spec_tokens(&dims, matrix_commit.as_ref(), &matrix_aux, &inline_blocks);
    let create_writer_with_dims_method = create_writer_with_dims_tokens(
        &name,
        &dims,
        matrix_commit.as_ref(),
        typed_api_enabled,
        &writer_return,
    );

    quote! {
        #matrix_type_defs
        #(#inline_block_defs)*

        #vis struct #name;

        impl #name {
            pub fn spec() -> ::stratava::__core::FormatSpec {
                const BLOCKS: &[::stratava::__core::BlockDescriptor] = &[
                    #(#descriptors,)*
                ];
                const BLOCK_IDENTITIES: &[(
                    u32,
                    ::core::option::Option<::stratava::__core::Endian>,
                    bool,
                    u64,
                )] = &[
                    #(#block_identities,)*
                ];
                ::stratava::__core::FormatSpec::new(
                    #magic,
                    #version,
                    #endian,
                    #schema_hash,
                    #index,
                    #integrity,
                    #recovery,
                    #manifest,
                    BLOCKS,
                )
                .with_extension(#extension)
                .with_liveness_policy(#liveness)
                .with_header_slots(#header_slots)
                .with_commit_policy(#commit)
                .with_compression_policy(#compression)
                .with_read_limits(#read_limits)
                #matrix_spec_step
                .with_block_identities(BLOCK_IDENTITIES)
                .with_layout(#layout)
                #schema_hash_step
            }

            pub fn clear_stale_writer_lock<P: AsRef<::std::path::Path>>(
                path: P,
                policy: ::stratava::__core::WriterLockBreakPolicy,
            ) -> ::stratava::__core::Result<()> {
                Self::spec().clear_stale_writer_lock(path, policy)
            }

            pub fn create<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().create(path)
            }

            pub fn create_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().create_with_limits(path, limits)
            }

            pub fn create_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().create_with_resource_limits(path, limits)
            }

            pub fn create_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().create_trusted_unbounded(path)
            }

            pub fn create_writer<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<#writer_return> {
                #create_writer_body
            }

            pub fn create_writer_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<#writer_return> {
                #create_writer_with_limits_body
            }

            pub fn create_writer_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<#writer_return> {
                #create_writer_with_resource_limits_body
            }

            pub fn create_writer_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<#writer_return> {
                #create_writer_trusted_body
            }

            pub fn create_layout_writer<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<#layout_writer_return> {
                #create_layout_writer_body
            }

            pub fn create_layout_writer_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<#layout_writer_return> {
                #create_layout_writer_with_limits_body
            }

            pub fn create_layout_writer_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<#layout_writer_return> {
                #create_layout_writer_with_resource_limits_body
            }

            pub fn create_layout_writer_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<#layout_writer_return> {
                #create_layout_writer_trusted_body
            }

            pub fn create_layout_writer_with_header<P: AsRef<::std::path::Path>>(
                path: P,
                fields: &[::stratava::__core::LayoutFieldValue],
            ) -> ::stratava::__core::Result<#layout_writer_return> {
                #create_layout_writer_with_header_body
            }

            pub fn create_layout_writer_with_header_and_limits<P: AsRef<::std::path::Path>>(
                path: P,
                fields: &[::stratava::__core::LayoutFieldValue],
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<#layout_writer_return> {
                #create_layout_writer_with_header_and_limits_body
            }

            pub fn create_layout_writer_with_header_and_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                fields: &[::stratava::__core::LayoutFieldValue],
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<#layout_writer_return> {
                #create_layout_writer_with_header_and_resource_limits_body
            }

            pub fn create_layout_writer_with_header_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
                fields: &[::stratava::__core::LayoutFieldValue],
            ) -> ::stratava::__core::Result<#layout_writer_return> {
                #create_layout_writer_with_header_trusted_body
            }

            #create_writer_with_dims_method

            pub fn open<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().open(path)
            }

            pub fn open_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().open_with_limits(path, limits)
            }

            pub fn open_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().open_with_resource_limits(path, limits)
            }

            pub fn open_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().open_trusted_unbounded(path)
            }

            pub fn open_writer<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<#writer_return> {
                #open_writer_body
            }

            pub fn open_writer_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<#writer_return> {
                #open_writer_with_limits_body
            }

            pub fn open_writer_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<#writer_return> {
                #open_writer_with_resource_limits_body
            }

            pub fn open_writer_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<#writer_return> {
                #open_writer_trusted_body
            }

            pub fn open_layout_writer<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<#layout_writer_return> {
                #open_layout_writer_body
            }

            pub fn open_layout_writer_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<#layout_writer_return> {
                #open_layout_writer_with_limits_body
            }

            pub fn open_layout_writer_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<#layout_writer_return> {
                #open_layout_writer_with_resource_limits_body
            }

            pub fn open_layout_writer_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<#layout_writer_return> {
                #open_layout_writer_trusted_body
            }

            pub fn open_readonly<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().open_readonly(path)
            }

            pub fn open_readonly_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().open_readonly_with_limits(path, limits)
            }

            pub fn open_readonly_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().open_readonly_with_resource_limits(path, limits)
            }

            pub fn open_readonly_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().open_readonly_trusted_unbounded(path)
            }

            pub fn open_reader<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<#reader_return> {
                #open_reader_body
            }

            pub fn open_reader_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<#reader_return> {
                #open_reader_with_limits_body
            }

            pub fn open_reader_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<#reader_return> {
                #open_reader_with_resource_limits_body
            }

            pub fn open_reader_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<#reader_return> {
                #open_reader_trusted_body
            }

            #high_cardinality_constructors

            pub fn open_layout_reader<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<#layout_reader_return> {
                #open_layout_reader_body
            }

            pub fn open_layout_reader_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<#layout_reader_return> {
                #open_layout_reader_with_limits_body
            }

            pub fn open_layout_reader_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<#layout_reader_return> {
                #open_layout_reader_with_resource_limits_body
            }

            pub fn open_layout_reader_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<#layout_reader_return> {
                #open_layout_reader_trusted_body
            }

            pub fn inspect_layout_file<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<::stratava::__core::LayoutFileInfo> {
                Self::spec().inspect_layout_file(path)
            }

            pub fn inspect_layout_file_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<::stratava::__core::LayoutFileInfo> {
                Self::spec().inspect_layout_file_with_limits(path, limits)
            }

            pub fn inspect_layout_file_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<::stratava::__core::LayoutFileInfo> {
                Self::spec().inspect_layout_file_with_resource_limits(path, limits)
            }

            pub fn inspect_layout_file_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<::stratava::__core::LayoutFileInfo> {
                Self::spec().inspect_layout_file_trusted_unbounded(path)
            }

            pub fn inspect_layout_file_report<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<::stratava::__core::LayoutScanReport> {
                Self::spec().inspect_layout_file_report(path)
            }

            pub fn inspect_layout_file_report_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<::stratava::__core::LayoutScanReport> {
                Self::spec().inspect_layout_file_report_with_limits(path, limits)
            }

            pub fn inspect_layout_file_report_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<::stratava::__core::LayoutScanReport> {
                Self::spec().inspect_layout_file_report_with_resource_limits(path, limits)
            }

            pub fn inspect_layout_file_report_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<::stratava::__core::LayoutScanReport> {
                Self::spec().inspect_layout_file_report_trusted_unbounded(path)
            }

            pub fn open_recover<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().open_recover(path)
            }

            pub fn open_recover_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().open_recover_with_limits(path, limits)
            }

            pub fn open_recover_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().open_recover_with_resource_limits(path, limits)
            }

            pub fn open_recover_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<::stratava::__core::StratavaFile> {
                Self::spec().open_recover_trusted_unbounded(path)
            }

            pub fn open_recover_writer<P: AsRef<::std::path::Path>>(path: P) -> ::stratava::__core::Result<#writer_return> {
                #open_recover_writer_body
            }

            pub fn open_recover_writer_with_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<#writer_return> {
                #open_recover_writer_with_limits_body
            }

            pub fn open_recover_writer_with_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<#writer_return> {
                #open_recover_writer_with_resource_limits_body
            }

            pub fn open_recover_writer_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<#writer_return> {
                #open_recover_writer_trusted_body
            }

            pub fn open_recover_with_report<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<(::stratava::__core::StratavaFile, ::stratava::__core::RecoveryReport)> {
                Self::spec().open_recover_with_report(path)
            }

            pub fn open_recover_with_report_and_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<(::stratava::__core::StratavaFile, ::stratava::__core::RecoveryReport)> {
                Self::spec().open_recover_with_report_and_limits(path, limits)
            }

            pub fn open_recover_with_report_and_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<(::stratava::__core::StratavaFile, ::stratava::__core::RecoveryReport)> {
                Self::spec().open_recover_with_report_and_resource_limits(path, limits)
            }

            pub fn open_recover_with_report_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<(::stratava::__core::StratavaFile, ::stratava::__core::RecoveryReport)> {
                Self::spec().open_recover_with_report_trusted_unbounded(path)
            }

            pub fn open_recover_writer_with_report<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<(#writer_return, ::stratava::__core::RecoveryReport)> {
                #open_recover_writer_report_body
            }

            pub fn open_recover_writer_with_report_and_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ReadLimits,
            ) -> ::stratava::__core::Result<(#writer_return, ::stratava::__core::RecoveryReport)> {
                #open_recover_writer_report_with_limits_body
            }

            pub fn open_recover_writer_with_report_and_resource_limits<P: AsRef<::std::path::Path>>(
                path: P,
                limits: ::stratava::__core::ResourceLimits,
            ) -> ::stratava::__core::Result<(#writer_return, ::stratava::__core::RecoveryReport)> {
                #open_recover_writer_report_with_resource_limits_body
            }

            pub fn open_recover_writer_with_report_trusted_unbounded<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::Result<(#writer_return, ::stratava::__core::RecoveryReport)> {
                #open_recover_writer_report_trusted_body
            }

            pub fn diagnostics() -> ::stratava::__core::FormatDiagnostics {
                Self::spec().diagnostics()
            }

            pub fn diagnose_file<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::FormatDiagnostics {
                Self::spec().diagnose_file(path)
            }

            pub fn self_test<P: AsRef<::std::path::Path>>(
                path: P,
            ) -> ::stratava::__core::FormatSelfTest {
                Self::spec().self_test(path)
            }
        }

        #(#duplicate_asserts)*
        #typed_api
        #high_cardinality_api
        #layout_typed_api
    }
}

pub(super) fn layout_tokens(
    preset: Option<LayoutPresetChoice>,
    file_header: Option<&LayoutFileHeader>,
    segments: &[LayoutSegment],
) -> TokenStream2 {
    let has_parts = file_header.is_some() || !segments.is_empty();
    let preset = preset.unwrap_or(if !has_parts {
        LayoutPresetChoice::StratavaNative
    } else {
        LayoutPresetChoice::None
    });
    let preset_tokens = layout_preset_tokens(preset);
    if !has_parts {
        return match preset {
            LayoutPresetChoice::StratavaNative => {
                quote!(::stratava::__core::LayoutSpec::stratava_native())
            }
            LayoutPresetChoice::None => quote!(::stratava::__core::LayoutSpec::none()),
            LayoutPresetChoice::Custom => {
                quote!(::stratava::__core::LayoutSpec::custom(&[]))
            }
        };
    }

    let header_fields_const = format_ident!("__STRATAVA_LAYOUT_FILE_HEADER_FIELDS");
    let header_field_const = file_header
        .map(|header| {
            let fields = header.fields.iter().map(layout_field_tokens);
            quote! {
                const #header_fields_const: &[::stratava::__core::LayoutFieldDescriptor] = &[
                    #(#fields,)*
                ];
            }
        })
        .unwrap_or_else(|| quote!());
    let header_part = file_header
        .map(|header| {
            let name = &header.name;
            quote! {
                ::stratava::__core::LayoutPartDescriptor {
                    name: stringify!(#name),
                    kind: ::stratava::__core::LayoutPartKind::FileHeader(
                        ::stratava::__core::FileHeaderDescriptor {
                            name: stringify!(#name),
                            fields: #header_fields_const,
                        }
                    ),
                },
            }
        })
        .unwrap_or_else(|| quote!());

    let lead_field_const_names: Vec<_> = (0..segments.len())
        .map(|index| format_ident!("__STRATAVA_LAYOUT_LEAD_IN_FIELDS_{index}"))
        .collect();
    let footer_field_const_names: Vec<_> = (0..segments.len())
        .map(|index| format_ident!("__STRATAVA_LAYOUT_FOOTER_FIELDS_{index}"))
        .collect();
    let lead_field_consts =
        segments
            .iter()
            .zip(lead_field_const_names.iter())
            .map(|(segment, const_name)| {
                let fields = segment.lead_in.fields.iter().map(layout_field_tokens);
                quote! {
                    const #const_name: &[::stratava::__core::LayoutFieldDescriptor] = &[
                        #(#fields,)*
                    ];
                }
            });
    let footer_field_consts = segments
        .iter()
        .zip(footer_field_const_names.iter())
        .filter_map(|(segment, const_name)| {
            segment.footer.as_ref().map(|footer| {
                let fields = footer.fields.iter().map(layout_field_tokens);
                quote! {
                    const #const_name: &[::stratava::__core::LayoutFieldDescriptor] = &[
                        #(#fields,)*
                    ];
                }
            })
        });
    let parts = segments
        .iter()
        .zip(lead_field_const_names.iter())
        .zip(footer_field_const_names.iter())
        .map(|((segment, fields), footer_fields)| {
            let name = &segment.name;
            let repeat = segment_repeat_tokens(segment.repeat);
            let lead_in = &segment.lead_in.name;
            let metadata = &segment.metadata;
            let raw_region = &segment.raw_region;
            let footer = segment
                .footer
                .as_ref()
                .map(|footer| {
                    let footer_name = &footer.name;
                    quote! {
                        ::core::option::Option::Some(::stratava::__core::FooterDescriptor {
                            name: stringify!(#footer_name),
                            fields: #footer_fields,
                        })
                    }
                })
                .unwrap_or_else(|| quote!(::core::option::Option::None));
            quote! {
                ::stratava::__core::LayoutPartDescriptor {
                    name: stringify!(#name),
                    kind: ::stratava::__core::LayoutPartKind::Segment(
                        ::stratava::__core::SegmentDescriptor {
                            name: stringify!(#name),
                            repeat: #repeat,
                            lead_in: ::stratava::__core::LeadInDescriptor {
                                name: stringify!(#lead_in),
                                fields: #fields,
                            },
                            metadata: ::stratava::__core::MetadataDescriptor {
                                name: stringify!(#metadata),
                                source: ::stratava::__core::LayoutBytesSource::Caller,
                            },
                            raw_region: ::stratava::__core::RawRegionDescriptor {
                                name: stringify!(#raw_region),
                                source: ::stratava::__core::LayoutBytesSource::Caller,
                            },
                            footer: #footer,
                        }
                    ),
                }
            }
        });

    quote! {
        {
            #header_field_const
            #(#lead_field_consts)*
            #(#footer_field_consts)*
            const __STRATAVA_LAYOUT_PARTS: &[::stratava::__core::LayoutPartDescriptor] = &[
                #header_part
                #(#parts,)*
            ];
            ::stratava::__core::LayoutSpec {
                preset: #preset_tokens,
                parts: __STRATAVA_LAYOUT_PARTS,
            }
        }
    }
}

pub(super) fn layout_field_tokens(field: &LayoutField) -> TokenStream2 {
    let name = &field.name;
    let ty = layout_field_type_tokens(&field.ty);
    let source = layout_field_source_tokens(&field.source);
    quote! {
        ::stratava::__core::LayoutFieldDescriptor {
            name: stringify!(#name),
            ty: #ty,
            source: #source,
            endian: None,
        }
    }
}

pub(super) fn layout_preset_tokens(preset: LayoutPresetChoice) -> TokenStream2 {
    match preset {
        LayoutPresetChoice::StratavaNative => {
            quote!(::stratava::__core::LayoutPreset::StratavaNative)
        }
        LayoutPresetChoice::None => quote!(::stratava::__core::LayoutPreset::None),
        LayoutPresetChoice::Custom => quote!(::stratava::__core::LayoutPreset::Custom),
    }
}

pub(super) fn segment_repeat_tokens(repeat: SegmentRepeatChoice) -> TokenStream2 {
    match repeat {
        SegmentRepeatChoice::Once => quote!(::stratava::__core::SegmentRepeat::Once),
        SegmentRepeatChoice::UntilEof => quote!(::stratava::__core::SegmentRepeat::UntilEof),
    }
}

pub(super) fn layout_field_type_tokens(ty: &LayoutFieldTypeChoice) -> TokenStream2 {
    match ty {
        LayoutFieldTypeChoice::Bytes(len) => {
            quote!(::stratava::__core::LayoutFieldType::Bytes { len: #len })
        }
        LayoutFieldTypeChoice::U8 => quote!(::stratava::__core::LayoutFieldType::U8),
        LayoutFieldTypeChoice::U16 => quote!(::stratava::__core::LayoutFieldType::U16),
        LayoutFieldTypeChoice::U32 => quote!(::stratava::__core::LayoutFieldType::U32),
        LayoutFieldTypeChoice::U64 => quote!(::stratava::__core::LayoutFieldType::U64),
        LayoutFieldTypeChoice::I64 => quote!(::stratava::__core::LayoutFieldType::I64),
    }
}

pub(super) fn layout_field_source_tokens(source: &LayoutFieldSourceChoice) -> TokenStream2 {
    match source {
        LayoutFieldSourceChoice::LiteralBytes(bytes) => {
            quote!(::stratava::__core::LayoutFieldSource::LiteralBytes(#bytes))
        }
        LayoutFieldSourceChoice::LiteralU64(value) => {
            quote!(::stratava::__core::LayoutFieldSource::LiteralU64(#value))
        }
        LayoutFieldSourceChoice::LiteralI64(value) => {
            quote!(::stratava::__core::LayoutFieldSource::LiteralI64(#value))
        }
        LayoutFieldSourceChoice::Caller => {
            quote!(::stratava::__core::LayoutFieldSource::Caller)
        }
        LayoutFieldSourceChoice::Finalize(finalize) => {
            let target = layout_anchor_tokens(finalize.target);
            let relative_to = layout_anchor_tokens(finalize.relative_to);
            quote! {
                ::stratava::__core::LayoutFieldSource::Finalize(
                    ::stratava::__core::LayoutFinalize {
                        target: #target,
                        relative_to: #relative_to,
                    }
                )
            }
        }
    }
}

pub(super) fn layout_anchor_tokens(anchor: LayoutAnchorChoice) -> TokenStream2 {
    match anchor {
        LayoutAnchorChoice::SegmentStart => quote!(::stratava::__core::LayoutAnchor::SegmentStart),
        LayoutAnchorChoice::AfterLeadIn => quote!(::stratava::__core::LayoutAnchor::AfterLeadIn),
        LayoutAnchorChoice::MetadataStart => {
            quote!(::stratava::__core::LayoutAnchor::MetadataStart)
        }
        LayoutAnchorChoice::RawRegionStart => {
            quote!(::stratava::__core::LayoutAnchor::RawRegionStart)
        }
        LayoutAnchorChoice::SegmentEnd => quote!(::stratava::__core::LayoutAnchor::SegmentEnd),
        LayoutAnchorChoice::FooterStart => quote!(::stratava::__core::LayoutAnchor::FooterStart),
        LayoutAnchorChoice::FooterEnd => quote!(::stratava::__core::LayoutAnchor::FooterEnd),
    }
}

pub(super) fn index_tokens(choice: IndexChoice) -> TokenStream2 {
    let scan_on_open = choice.scan_on_open;
    let checkpoint_on_flush = choice.checkpoint_on_flush;
    let block_offset_chain = choice.block_offset_chain;
    let keyed_offset_chain = choice.keyed_offset_chain;
    let header_tails = choice.header_tails;
    quote! {
        ::stratava::__core::IndexPolicy::new(
            #scan_on_open,
            #checkpoint_on_flush,
            #block_offset_chain,
            #keyed_offset_chain,
        )
        .with_header_tails(#header_tails)
    }
}

pub(super) fn commit_tokens(choice: CommitChoice) -> TokenStream2 {
    match choice {
        CommitChoice::None => quote!(::stratava::__core::CommitPolicy::None),
        CommitChoice::RecordFooter => quote!(::stratava::__core::CommitPolicy::RecordFooter),
        CommitChoice::TransactionMarkerOnFlush => quote! {
            ::stratava::__core::CommitPolicy::TransactionMarker(
                ::stratava::__core::TransactionMarkerMode::OnFlush
            )
        },
        CommitChoice::TransactionMarkerExplicit => quote! {
            ::stratava::__core::CommitPolicy::TransactionMarker(
                ::stratava::__core::TransactionMarkerMode::Explicit
            )
        },
    }
}

pub(super) fn matrix_type_tokens(
    vis: &Visibility,
    format_name: &Ident,
    dims: &[MatrixDim],
    blocks: &[InlineBlock],
) -> TokenStream2 {
    let dims_tokens = if dims.is_empty() {
        quote!()
    } else {
        let dims_name = format_ident!("{}Dims", format_name);
        let fields = dims.iter().map(|dim| {
            let name = &dim.name;
            let ty = &dim.ty;
            quote!(#vis #name: #ty,)
        });
        let pairs = dims.iter().map(|dim| {
            let name = dim.name.to_string();
            let field = &dim.name;
            quote!((#name, self.#field as u64))
        });
        quote! {
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #vis struct #dims_name {
                #(#fields)*
            }

            impl #dims_name {
                pub fn into_matrix_dims(self) -> ::stratava::__core::MatrixDimensions {
                    ::stratava::__core::MatrixDimensions::from_pairs([
                        #(#pairs,)*
                    ])
                }
            }
        }
    };

    let key_tokens = blocks.iter().filter_map(|block| {
        let InlineBlockKind::Matrix(meta) = &block.kind else {
            return None;
        };
        let key_name = format_ident!("{}Key", block.name);
        let fields = meta.dims.iter().map(|dim| quote!(#vis #dim: u64,));
        let first = &meta.dims[0];
        let second = &meta.dims[1];
        Some(quote! {
            #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
            #vis struct #key_name {
                #(#fields)*
            }

            impl From<#key_name> for ::stratava::__core::MatrixKey {
                fn from(key: #key_name) -> Self {
                    ::stratava::__core::MatrixKey::new(key.#first, key.#second)
                }
            }
        })
    });

    quote! {
        #dims_tokens
        #(#key_tokens)*
    }
}

pub(super) fn matrix_spec_tokens(
    dims: &[MatrixDim],
    commit: Option<&MatrixCommit>,
    aux: &[MatrixAux],
    blocks: &[InlineBlock],
) -> TokenStream2 {
    let matrix_blocks: Vec<_> = blocks
        .iter()
        .filter_map(|block| match &block.kind {
            InlineBlockKind::Matrix(meta) => Some((block, meta)),
            _ => None,
        })
        .collect();
    if matrix_blocks.is_empty() {
        return quote!();
    }
    let Some(commit) = commit else {
        return quote!();
    };
    let dimensions = dims.iter().map(|dim| {
        let name = dim.name.to_string();
        quote!(::stratava::__core::MatrixDimensionDescriptor { name: #name })
    });
    let cell_commits = commit.categories.iter().map(|ident| {
        let name = ident.to_string();
        quote! {
            ::stratava::__core::MatrixCommitDescriptor {
                name: #name,
                kind: ::stratava::__core::MatrixCommitKind::Cell,
            }
        }
    });
    let single_commits = commit.singles.iter().map(|ident| {
        let name = ident.to_string();
        quote! {
            ::stratava::__core::MatrixCommitDescriptor {
                name: #name,
                kind: ::stratava::__core::MatrixCommitKind::Single,
            }
        }
    });
    let channel_commits = commit.per_channel.iter().map(|ident| {
        let name = ident.to_string();
        quote! {
            ::stratava::__core::MatrixCommitDescriptor {
                name: #name,
                kind: ::stratava::__core::MatrixCommitKind::PerChannel,
            }
        }
    });
    let block_specs = matrix_blocks.iter().map(|(block, meta)| {
        let ty = &block.name;
        let category = meta.category.to_string();
        let first = matrix_storage_dimension_name(&meta.dims[0], dims)
            .unwrap_or_else(|| meta.dims[0].to_string());
        let second = matrix_storage_dimension_name(&meta.dims[1], dims)
            .unwrap_or_else(|| meta.dims[1].to_string());
        quote! {
            ::stratava::__core::MatrixBlockDescriptor {
                block_id: <#ty as ::stratava::__core::StratavaBlock>::ID,
                dimensions: [#first, #second],
                category: #category,
                slot_stride: <#ty as ::stratava::__core::StratavaMatrixBlock>::SLOT_STRIDE,
            }
        }
    });
    let aux_step = if aux.is_empty() {
        quote!()
    } else {
        let aux_specs = aux.iter().map(|aux| {
            let name = aux.name.to_string();
            let byte_len = aux.byte_len;
            quote! {
                ::stratava::__core::MatrixAuxDescriptor {
                    name: #name,
                    byte_len: #byte_len,
                }
            }
        });
        quote! {
            .with_matrix_aux(&[
                #(#aux_specs,)*
            ])
        }
    };
    quote! {
        .with_matrix_spec(
            &[
                #(#dimensions,)*
            ],
            &[
                #(#cell_commits,)*
                #(#single_commits,)*
                #(#channel_commits,)*
            ],
            &[
                #(#block_specs,)*
            ],
        )
        #aux_step
    }
}

pub(super) fn create_writer_with_dims_tokens(
    format_name: &Ident,
    dims: &[MatrixDim],
    commit: Option<&MatrixCommit>,
    typed_api: bool,
    writer_return: &TokenStream2,
) -> TokenStream2 {
    if dims.is_empty() || commit.is_none() {
        return quote!();
    }
    let dims_name = format_ident!("{}Dims", format_name);
    let body = if typed_api {
        let writer_name = format_ident!("{}Writer", format_name);
        quote!(#writer_name::from_inner(
            Self::spec().create_writer_with_dims(path, dims.into_matrix_dims())?
        ))
    } else {
        quote!(Self::spec().create_writer_with_dims(path, dims.into_matrix_dims()))
    };
    let with_limits_body = if typed_api {
        let writer_name = format_ident!("{}Writer", format_name);
        quote!(#writer_name::from_inner(
            Self::spec().create_writer_with_dims_and_limits(
                path,
                dims.into_matrix_dims(),
                limits,
            )?
        ))
    } else {
        quote!(Self::spec().create_writer_with_dims_and_limits(
            path,
            dims.into_matrix_dims(),
            limits,
        ))
    };
    let with_resource_limits_body = if typed_api {
        let writer_name = format_ident!("{}Writer", format_name);
        quote!(#writer_name::from_inner(
            Self::spec().create_writer_with_dims_and_resource_limits(
                path,
                dims.into_matrix_dims(),
                limits,
            )?
        ))
    } else {
        quote!(Self::spec().create_writer_with_dims_and_resource_limits(
            path,
            dims.into_matrix_dims(),
            limits,
        ))
    };
    let trusted_body = if typed_api {
        let writer_name = format_ident!("{}Writer", format_name);
        quote!(#writer_name::from_inner(
            Self::spec().create_writer_with_dims_trusted_unbounded(
                path,
                dims.into_matrix_dims(),
            )?
        ))
    } else {
        quote!(
            Self::spec().create_writer_with_dims_trusted_unbounded(path, dims.into_matrix_dims(),)
        )
    };
    quote! {
        pub fn create_writer_with_dims<P: AsRef<::std::path::Path>>(
            path: P,
            dims: #dims_name,
        ) -> ::stratava::__core::Result<#writer_return> {
            #body
        }

        pub fn create_writer_with_dims_and_limits<P: AsRef<::std::path::Path>>(
            path: P,
            dims: #dims_name,
            limits: ::stratava::__core::ReadLimits,
        ) -> ::stratava::__core::Result<#writer_return> {
            #with_limits_body
        }

        pub fn create_writer_with_dims_and_resource_limits<P: AsRef<::std::path::Path>>(
            path: P,
            dims: #dims_name,
            limits: ::stratava::__core::ResourceLimits,
        ) -> ::stratava::__core::Result<#writer_return> {
            #with_resource_limits_body
        }

        pub fn create_writer_with_dims_trusted_unbounded<P: AsRef<::std::path::Path>>(
            path: P,
            dims: #dims_name,
        ) -> ::stratava::__core::Result<#writer_return> {
            #trusted_body
        }
    }
}

pub(super) fn inline_block_tokens(
    vis: &Visibility,
    block: &InlineBlock,
    dims: &[MatrixDim],
) -> TokenStream2 {
    let name = &block.name;
    let finite_key = block
        .finite_key
        .as_ref()
        .map(|finite| finite.tokens(vis, name));
    let id = block.id;
    let version = block.version;
    let kind = match &block.kind {
        InlineBlockKind::Fixed => LitStr::new("fixed", name.span()),
        InlineBlockKind::Variable => LitStr::new("variable", name.span()),
        InlineBlockKind::Matrix(_) => LitStr::new("matrix", name.span()),
    };
    let durability_attr = block
        .durability
        .as_ref()
        .map(|value| quote!(#[stratava(durability = #value)]));
    let immediate_if_attr = block
        .immediate_if
        .as_ref()
        .map(|value| quote!(#[stratava(immediate_if = #value)]));
    let key_attr = if block.key_fields.is_empty() {
        quote!()
    } else {
        let key = block
            .key_fields
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let key = LitStr::new(&key, name.span());
        quote!(#[stratava(key = #key)])
    };
    let fields = block.fields.iter().map(|field| {
        let name = &field.name;
        let ty = &field.ty;
        let field_id = field.field_id;
        let default = if field.default {
            quote!(#[stratava(default)])
        } else {
            quote!()
        };
        quote! {
            #[stratava(field_id = #field_id)]
            #default
            #vis #name: #ty,
        }
    });
    let matrix_impl = if let InlineBlockKind::Matrix(meta) = &block.kind {
        let matrix_dims = meta
            .dims
            .iter()
            .map(|dim| matrix_storage_dimension_name(dim, dims).unwrap_or_else(|| dim.to_string()));
        let category = meta.category.to_string();
        let stride = matrix_slot_stride_tokens(&block.fields);
        quote! {
            impl ::stratava::__core::StratavaMatrixBlock for #name {
                const DIMENSIONS: [&'static str; 2] = [#(#matrix_dims,)*];
                const CATEGORY: &'static str = #category;
                const SLOT_STRIDE: u64 = #stride;
            }
        }
    } else {
        quote!()
    };
    quote! {
        #finite_key
        #[derive(Clone, Debug, PartialEq, ::stratava::StratavaBlock)]
        #[stratava(id = #id, version = #version, kind = #kind)]
        #key_attr
        #durability_attr
        #immediate_if_attr
        #vis struct #name {
            #(#fields)*
        }

        #matrix_impl
    }
}

pub(super) fn layout_typed_api_tokens(
    vis: &Visibility,
    format_name: &Ident,
    file_header: Option<&LayoutFileHeader>,
    segments: &[LayoutSegment],
) -> TokenStream2 {
    let reader_name = format_ident!("{}LayoutReader", format_name);
    let writer_name = format_ident!("{}LayoutWriter", format_name);
    let header_tokens = file_header
        .map(|header| layout_header_typed_tokens(vis, format_name, &writer_name, header))
        .unwrap_or_else(|| quote!());
    let reader_header_methods = file_header
        .map(|header| layout_reader_header_method_tokens(format_name, header))
        .unwrap_or_else(|| quote!());
    let segment_types = segments
        .iter()
        .map(|segment| layout_segment_typed_tokens(vis, format_name, segment));
    let writer_segment_methods = segments
        .iter()
        .map(|segment| layout_writer_segment_method_tokens(format_name, segment));
    let reader_segment_methods = segments
        .iter()
        .map(|segment| layout_reader_segment_methods_tokens(format_name, segment));

    quote! {
        #header_tokens
        #(#segment_types)*

        #[derive(Debug)]
        #vis struct #reader_name {
            inner: ::stratava::__core::LayoutReader,
            /// Per-name segment ordinal table. The OUTER vector holds one entry
            /// per segment kind actually present in the file — a handful, fixed
            /// by the format and independent of the segment count — and the
            /// inner vector maps that kind's ordinal to the absolute segment
            /// index. Without it every typed accessor re-derived the index by
            /// scanning `segments()` from the start, so an ordered pass over N
            /// segments cost O(N^2).
            ///
            /// Built on first typed access rather than at open: `LayoutReader`
            /// exposes no `&mut self` method, so `segments()` cannot change
            /// after open, and an open that never reaches a typed accessor pays
            /// nothing. Reader-local `OnceCell` needs no synchronization and
            /// keeps reads as `&self`. The wrapper is Send + !Sync: move it to
            /// another thread, or open a separate reader for that thread.
            segment_ordinals: ::std::cell::OnceCell<
                ::std::vec::Vec<(&'static str, ::std::vec::Vec<usize>)>,
            >,
        }

        impl #reader_name {
            pub fn from_inner(inner: ::stratava::__core::LayoutReader) -> Self {
                Self {
                    inner,
                    segment_ordinals: ::std::cell::OnceCell::new(),
                }
            }

            pub fn into_inner(self) -> ::stratava::__core::LayoutReader {
                self.inner
            }

            pub fn spec(&self) -> ::stratava::__core::FormatSpec {
                self.inner.spec()
            }

            pub fn path(&self) -> &::std::path::Path {
                self.inner.path()
            }

            pub fn file_header_len(&self) -> u64 {
                self.inner.file_header_len()
            }

            pub fn segment_count(&self) -> usize {
                self.inner.segment_count()
            }

            pub fn segment(
                &self,
                index: usize,
            ) -> ::stratava::__core::Result<
                ::core::option::Option<::stratava::__core::LayoutSegmentInfo>,
            > {
                self.inner.segment(index)
            }

            pub fn segments(
                &self,
            ) -> impl ::core::iter::Iterator<
                Item = ::stratava::__core::Result<::stratava::__core::LayoutSegmentInfo>,
            > + '_ {
                self.inner.segments()
            }

            pub fn segments_into(
                &self,
                out: &mut ::std::vec::Vec<::stratava::__core::LayoutSegmentInfo>,
            ) -> ::stratava::__core::Result<()> {
                self.inner.segments_into(out)
            }

            pub fn read_metadata(&self, index: usize) -> ::stratava::__core::Result<::std::vec::Vec<u8>> {
                self.inner.read_metadata(index)
            }

            pub fn read_metadata_range(
                &self,
                index: usize,
                offset: u64,
                len: u64,
            ) -> ::stratava::__core::Result<::std::vec::Vec<u8>> {
                self.inner.read_metadata_range(index, offset, len)
            }

            pub fn read_raw(&self, index: usize) -> ::stratava::__core::Result<::std::vec::Vec<u8>> {
                self.inner.read_raw(index)
            }

            pub fn read_raw_range(
                &self,
                index: usize,
                offset: u64,
                len: u64,
            ) -> ::stratava::__core::Result<::std::vec::Vec<u8>> {
                self.inner.read_raw_range(index, offset, len)
            }

            fn __stratava_layout_segment_ordinals(
                &self,
            ) -> &[(&'static str, ::std::vec::Vec<usize>)] {
                self.segment_ordinals.get_or_init(|| {
                    let mut table: ::std::vec::Vec<
                        (&'static str, ::std::vec::Vec<usize>),
                    > = ::std::vec::Vec::new();
                    for (index, segment) in self.inner.segments().enumerate() {
                        let ::core::result::Result::Ok(segment) = segment else {
                            // A segment that cannot be produced stops the table
                            // here. The per-segment accessors below then report
                            // `None` past this point rather than a wrong
                            // ordinal, which is the same answer they gave for a
                            // file with fewer segments.
                            break;
                        };
                        match table.iter_mut().find(|(name, _)| *name == segment.name) {
                            ::core::option::Option::Some((_, ordinals)) => {
                                ordinals.push(index);
                            }
                            ::core::option::Option::None => {
                                table.push((segment.name, ::std::vec![index]));
                            }
                        }
                    }
                    table
                })
            }

            fn __stratava_layout_segment_index(
                &self,
                name: &'static str,
                ordinal: usize,
            ) -> ::stratava::__core::Result<usize> {
                self.__stratava_layout_segment_ordinals()
                    .iter()
                    .find(|(entry, _)| *entry == name)
                    .and_then(|(_, ordinals)| ordinals.get(ordinal).copied())
                    .ok_or_else(|| ::stratava::__core::Error::LayoutSegmentIndexOutOfBounds {
                        segment: name.to_string(),
                        index: ordinal,
                    })
            }

            #reader_header_methods
            #(#reader_segment_methods)*
        }

        #[derive(Debug)]
        #vis struct #writer_name {
            inner: ::stratava::__core::LayoutWriter,
        }

        impl #writer_name {
            pub fn from_inner(inner: ::stratava::__core::LayoutWriter) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> ::stratava::__core::LayoutWriter {
                self.inner
            }

            pub fn spec(&self) -> ::stratava::__core::FormatSpec {
                self.inner.spec()
            }

            pub fn path(&self) -> &::std::path::Path {
                self.inner.path()
            }

            pub fn write_segment(
                &mut self,
                segment: ::stratava::__core::SegmentWrite<'_>,
            ) -> ::stratava::__core::Result<::stratava::__core::LayoutSegmentInfo> {
                self.inner.write_segment(segment)
            }

            pub fn write_segment_streamed<M, R>(
                &mut self,
                segment: ::stratava::__core::SegmentWriteStream<'_, M, R>,
            ) -> ::stratava::__core::Result<::stratava::__core::LayoutSegmentInfo>
            where
                M: FnOnce(&mut dyn ::std::io::Write) -> ::stratava::__core::Result<()>,
                R: FnOnce(&mut dyn ::std::io::Write) -> ::stratava::__core::Result<()>,
            {
                self.inner.write_segment_streamed(segment)
            }

            pub fn flush(&mut self) -> ::stratava::__core::Result<()> {
                self.inner.flush()
            }

            pub fn sync(&mut self) -> ::stratava::__core::Result<()> {
                self.inner.sync()
            }

            #(#writer_segment_methods)*
        }
    }
}

pub(super) fn layout_header_typed_tokens(
    vis: &Visibility,
    format_name: &Ident,
    writer_name: &Ident,
    header: &LayoutFileHeader,
) -> TokenStream2 {
    let fields_name = format_ident!("{}{}LayoutFields", format_name, header.name);
    let info_name = format_ident!("{}{}LayoutInfo", format_name, header.name);
    let field_struct = layout_field_struct_tokens(vis, &fields_name, &header.fields);
    let getters = header
        .fields
        .iter()
        .map(layout_file_header_info_field_getter);
    quote! {
        #field_struct

        #[derive(Clone, Debug, PartialEq, Eq)]
        #vis struct #info_name {
            fields: ::std::vec::Vec<::stratava::__core::LayoutFieldValue>,
        }

        impl #info_name {
            pub fn from_fields(fields: ::std::vec::Vec<::stratava::__core::LayoutFieldValue>) -> Self {
                Self { fields }
            }

            pub fn fields(&self) -> &[::stratava::__core::LayoutFieldValue] {
                &self.fields
            }

            pub fn field(&self, name: &str) -> ::core::option::Option<&::stratava::__core::LayoutValue> {
                self.fields
                    .iter()
                    .find(|field| field.name == name)
                    .map(|field| &field.value)
            }

            #(#getters)*
        }

        impl #format_name {
            pub fn create_layout_writer_with_typed_header<P: AsRef<::std::path::Path>>(
                path: P,
                header: #fields_name,
            ) -> ::stratava::__core::Result<#writer_name> {
                let fields = header.__stratava_layout_values();
                ::core::result::Result::Ok(#writer_name::from_inner(
                    Self::spec().create_layout_writer_with_header(path, &fields)?
                ))
            }
        }
    }
}

pub(super) fn layout_reader_header_method_tokens(
    format_name: &Ident,
    header: &LayoutFileHeader,
) -> TokenStream2 {
    let info_name = format_ident!("{}{}LayoutInfo", format_name, header.name);
    quote! {
        pub fn file_header(&self) -> #info_name {
            #info_name::from_fields(self.inner.file_header_fields().to_vec())
        }

        pub fn file_header_fields(&self) -> &[::stratava::__core::LayoutFieldValue] {
            self.inner.file_header_fields()
        }

        pub fn file_header_field(
            &self,
            name: &str,
        ) -> ::core::option::Option<&::stratava::__core::LayoutValue> {
            self.inner.file_header_field(name)
        }
    }
}

pub(super) fn layout_segment_typed_tokens(
    vis: &Visibility,
    format_name: &Ident,
    segment: &LayoutSegment,
) -> TokenStream2 {
    let field_type = format_ident!("{}{}LayoutFields", format_name, segment.name);
    let footer_field_type = format_ident!("{}{}LayoutFooterFields", format_name, segment.name);
    let write_type = format_ident!("{}{}LayoutWrite", format_name, segment.name);
    let info_type = format_ident!("{}{}LayoutInfo", format_name, segment.name);
    let field_struct = layout_field_struct_tokens(vis, &field_type, &segment.lead_in.fields);
    let footer_fields = segment
        .footer
        .as_ref()
        .map(|footer| footer.fields.as_slice())
        .unwrap_or(&[]);
    let footer_field_struct = layout_field_struct_tokens(vis, &footer_field_type, footer_fields);
    let lead_in_getters = segment.lead_in.fields.iter().map(layout_info_field_getter);
    let footer_getters = footer_fields.iter().map(layout_info_footer_field_getter);

    quote! {
        #field_struct
        #footer_field_struct

        #[derive(Clone, Debug)]
        #vis struct #write_type<'a> {
            pub fields: #field_type,
            pub footer_fields: #footer_field_type,
            pub metadata: &'a [u8],
            pub raw: &'a [u8],
        }

        #[derive(Clone, Debug, PartialEq, Eq)]
        #vis struct #info_type {
            inner: ::stratava::__core::LayoutSegmentInfo,
        }

        impl #info_type {
            pub fn from_inner(inner: ::stratava::__core::LayoutSegmentInfo) -> Self {
                Self { inner }
            }

            pub fn as_layout_segment_info(&self) -> &::stratava::__core::LayoutSegmentInfo {
                &self.inner
            }

            pub fn into_layout_segment_info(self) -> ::stratava::__core::LayoutSegmentInfo {
                self.inner
            }

            pub fn segment_start(&self) -> u64 {
                self.inner.segment_start
            }

            pub fn metadata_len(&self) -> u64 {
                self.inner.metadata_len
            }

            pub fn raw_len(&self) -> u64 {
                self.inner.raw_len
            }

            pub fn segment_end(&self) -> u64 {
                self.inner.segment_end
            }

            #(#lead_in_getters)*
            #(#footer_getters)*
        }
    }
}

pub(super) fn layout_writer_segment_method_tokens(
    format_name: &Ident,
    segment: &LayoutSegment,
) -> TokenStream2 {
    let method = format_ident!("write_{}", singular_method_name(&segment.name));
    let streamed_method = format_ident!("write_{}_streamed", singular_method_name(&segment.name));
    let write_type = format_ident!("{}{}LayoutWrite", format_name, segment.name);
    let field_type = format_ident!("{}{}LayoutFields", format_name, segment.name);
    let footer_field_type = format_ident!("{}{}LayoutFooterFields", format_name, segment.name);
    let info_type = format_ident!("{}{}LayoutInfo", format_name, segment.name);
    let segment_name = segment.name.to_string();
    quote! {
        pub fn #method(
            &mut self,
            segment: #write_type<'_>,
        ) -> ::stratava::__core::Result<#info_type> {
            let fields = segment.fields.__stratava_layout_values();
            let footer_fields = segment.footer_fields.__stratava_layout_values();
            let info = self.inner.write_segment(::stratava::__core::SegmentWrite {
                name: #segment_name,
                fields: &fields,
                footer_fields: &footer_fields,
                metadata: segment.metadata,
                raw: segment.raw,
            })?;
            ::core::result::Result::Ok(#info_type::from_inner(info))
        }

        pub fn #streamed_method<M, R>(
            &mut self,
            fields: #field_type,
            footer_fields: #footer_field_type,
            write_metadata: M,
            write_raw: R,
        ) -> ::stratava::__core::Result<#info_type>
        where
            M: FnOnce(&mut dyn ::std::io::Write) -> ::stratava::__core::Result<()>,
            R: FnOnce(&mut dyn ::std::io::Write) -> ::stratava::__core::Result<()>,
        {
            let fields = fields.__stratava_layout_values();
            let footer_fields = footer_fields.__stratava_layout_values();
            let info = self.inner.write_segment_streamed(::stratava::__core::SegmentWriteStream {
                name: #segment_name,
                fields: &fields,
                footer_fields: &footer_fields,
                write_metadata,
                write_raw,
            })?;
            ::core::result::Result::Ok(#info_type::from_inner(info))
        }
    }
}

pub(super) fn layout_reader_segment_methods_tokens(
    format_name: &Ident,
    segment: &LayoutSegment,
) -> TokenStream2 {
    let plural = plural_method_ident(&segment.name);
    let plural_into = format_ident!("{}_into", plural_method_ident(&segment.name));
    let singular = format_ident!("{}", singular_method_name(&segment.name));
    let read_metadata = format_ident!("read_{}_metadata", singular_method_name(&segment.name));
    let read_metadata_range = format_ident!(
        "read_{}_metadata_range",
        singular_method_name(&segment.name)
    );
    let read_raw = format_ident!("read_{}_raw", singular_method_name(&segment.name));
    let read_raw_range = format_ident!("read_{}_raw_range", singular_method_name(&segment.name));
    let info_type = format_ident!("{}{}LayoutInfo", format_name, segment.name);
    let segment_name = segment.name.to_string();
    quote! {
        pub fn #plural(&self) -> ::stratava::__core::Result<::std::vec::Vec<#info_type>> {
            let mut out = ::std::vec::Vec::new();
            self.#plural_into(&mut out)?;
            ::core::result::Result::Ok(out)
        }

        /// Fills the caller's buffer. `out` is cleared and then extended.
        ///
        /// This also stopped deep-cloning: the segment is produced once and
        /// moved, where the allocating form used to clone every matching
        /// segment on top of the reader's own copy of it.
        pub fn #plural_into(
            &self,
            out: &mut ::std::vec::Vec<#info_type>,
        ) -> ::stratava::__core::Result<()> {
            out.clear();
            for segment in self.inner.segments() {
                let segment = segment?;
                if segment.name == #segment_name {
                    out.push(#info_type::from_inner(segment));
                }
            }
            ::core::result::Result::Ok(())
        }

        pub fn #singular(&self, index: usize) -> ::stratava::__core::Result<::core::option::Option<#info_type>> {
            let ::core::result::Result::Ok(index) = self.__stratava_layout_segment_index(#segment_name, index) else {
                return ::core::result::Result::Ok(::core::option::Option::None);
            };
            let ::core::option::Option::Some(segment) = self.inner.segment(index)? else {
                return ::core::result::Result::Ok(::core::option::Option::None);
            };
            ::core::result::Result::Ok(::core::option::Option::Some(#info_type::from_inner(segment)))
        }

        pub fn #read_metadata(&self, index: usize) -> ::stratava::__core::Result<::std::vec::Vec<u8>> {
            let index = self.__stratava_layout_segment_index(#segment_name, index)?;
            self.inner.read_metadata(index)
        }

        pub fn #read_metadata_range(
            &self,
            index: usize,
            offset: u64,
            len: u64,
        ) -> ::stratava::__core::Result<::std::vec::Vec<u8>> {
            let index = self.__stratava_layout_segment_index(#segment_name, index)?;
            self.inner.read_metadata_range(index, offset, len)
        }

        pub fn #read_raw(&self, index: usize) -> ::stratava::__core::Result<::std::vec::Vec<u8>> {
            let index = self.__stratava_layout_segment_index(#segment_name, index)?;
            self.inner.read_raw(index)
        }

        pub fn #read_raw_range(
            &self,
            index: usize,
            offset: u64,
            len: u64,
        ) -> ::stratava::__core::Result<::std::vec::Vec<u8>> {
            let index = self.__stratava_layout_segment_index(#segment_name, index)?;
            self.inner.read_raw_range(index, offset, len)
        }
    }
}

pub(super) fn layout_field_struct_tokens(
    vis: &Visibility,
    name: &Ident,
    fields: &[LayoutField],
) -> TokenStream2 {
    let caller_fields: Vec<_> = fields
        .iter()
        .filter(|field| matches!(&field.source, LayoutFieldSourceChoice::Caller))
        .collect();
    if caller_fields.is_empty() {
        return quote! {
            #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
            #vis struct #name;

            impl #name {
                fn __stratava_layout_values(&self) -> ::std::vec::Vec<::stratava::__core::LayoutFieldValue> {
                    ::std::vec::Vec::new()
                }
            }
        };
    }

    let struct_fields = caller_fields.iter().map(|field| {
        let field_name = &field.name;
        let ty = layout_field_rust_type_tokens(&field.ty);
        quote!(pub #field_name: #ty,)
    });
    let value_entries = caller_fields.iter().map(|field| {
        let field_name = &field.name;
        let field_name_string = field.name.to_string();
        let value = layout_field_to_value_tokens(field, quote!(self.#field_name));
        quote! {
            ::stratava::__core::LayoutFieldValue {
                name: #field_name_string,
                value: #value,
            }
        }
    });

    quote! {
        #[derive(Clone, Debug, Default, PartialEq, Eq)]
        #vis struct #name {
            #(#struct_fields)*
        }

        impl #name {
            fn __stratava_layout_values(&self) -> ::std::vec::Vec<::stratava::__core::LayoutFieldValue> {
                vec![
                    #(#value_entries,)*
                ]
            }
        }
    }
}

pub(super) fn layout_info_field_getter(field: &LayoutField) -> TokenStream2 {
    let method = &field.name;
    let field_name = field.name.to_string();
    let ty = layout_field_rust_type_tokens(&field.ty);
    let conversion = layout_value_conversion_tokens(&field.ty, quote!(value), &field_name);
    quote! {
        pub fn #method(&self) -> ::stratava::__core::Result<#ty> {
            let value = self
                .inner
                .field(#field_name)
                .ok_or(::stratava::__core::Error::LayoutFieldMissing(#field_name))?;
            #conversion
        }
    }
}

pub(super) fn layout_file_header_info_field_getter(field: &LayoutField) -> TokenStream2 {
    let method = &field.name;
    let field_name = field.name.to_string();
    let ty = layout_field_rust_type_tokens(&field.ty);
    let conversion = layout_value_conversion_tokens(&field.ty, quote!(value), &field_name);
    quote! {
        pub fn #method(&self) -> ::stratava::__core::Result<#ty> {
            let value = self
                .field(#field_name)
                .ok_or(::stratava::__core::Error::LayoutFieldMissing(#field_name))?;
            #conversion
        }
    }
}

pub(super) fn layout_info_footer_field_getter(field: &LayoutField) -> TokenStream2 {
    let method = format_ident!("footer_{}", field.name);
    let field_name = field.name.to_string();
    let ty = layout_field_rust_type_tokens(&field.ty);
    let conversion = layout_value_conversion_tokens(&field.ty, quote!(value), &field_name);
    quote! {
        pub fn #method(&self) -> ::stratava::__core::Result<#ty> {
            let value = self
                .inner
                .footer_field(#field_name)
                .ok_or(::stratava::__core::Error::LayoutFieldMissing(#field_name))?;
            #conversion
        }
    }
}

pub(super) fn layout_field_rust_type_tokens(ty: &LayoutFieldTypeChoice) -> TokenStream2 {
    match ty {
        LayoutFieldTypeChoice::Bytes(_) => quote!(::std::vec::Vec<u8>),
        LayoutFieldTypeChoice::U8 => quote!(u8),
        LayoutFieldTypeChoice::U16 => quote!(u16),
        LayoutFieldTypeChoice::U32 => quote!(u32),
        LayoutFieldTypeChoice::U64 => quote!(u64),
        LayoutFieldTypeChoice::I64 => quote!(i64),
    }
}

pub(super) fn layout_field_to_value_tokens(
    field: &LayoutField,
    access: TokenStream2,
) -> TokenStream2 {
    match field.ty {
        LayoutFieldTypeChoice::Bytes(_) => {
            quote!(::stratava::__core::LayoutValue::Bytes(#access.clone()))
        }
        LayoutFieldTypeChoice::U8 => quote!(::stratava::__core::LayoutValue::U8(#access)),
        LayoutFieldTypeChoice::U16 => quote!(::stratava::__core::LayoutValue::U16(#access)),
        LayoutFieldTypeChoice::U32 => quote!(::stratava::__core::LayoutValue::U32(#access)),
        LayoutFieldTypeChoice::U64 => quote!(::stratava::__core::LayoutValue::U64(#access)),
        LayoutFieldTypeChoice::I64 => quote!(::stratava::__core::LayoutValue::I64(#access)),
    }
}

pub(super) fn layout_value_conversion_tokens(
    ty: &LayoutFieldTypeChoice,
    value: TokenStream2,
    field_name: &str,
) -> TokenStream2 {
    match ty {
        LayoutFieldTypeChoice::Bytes(_) => quote!(#value.to_bytes(#field_name)),
        LayoutFieldTypeChoice::U8 => quote!(#value.to_u8(#field_name)),
        LayoutFieldTypeChoice::U16 => quote!(#value.to_u16(#field_name)),
        LayoutFieldTypeChoice::U32 => quote!(#value.to_u32(#field_name)),
        LayoutFieldTypeChoice::U64 => quote!(#value.to_u64(#field_name)),
        LayoutFieldTypeChoice::I64 => quote!(#value.to_i64(#field_name)),
    }
}

pub(super) fn high_cardinality_api_tokens(
    vis: &Visibility,
    format_name: &Ident,
    blocks: &[InlineBlock],
    keyed_offset_chain: bool,
) -> (TokenStream2, TokenStream2) {
    let stream_reader_name = format_ident!("{}StreamReader", format_name);
    let stream_writer_name = format_ident!("{}StreamWriter", format_name);
    let indexed_reader_name = format_ident!("{}IndexedReader", format_name);
    let indexed_writer_name = format_ident!("{}IndexedWriter", format_name);
    let append_blocks: Vec<_> = blocks
        .iter()
        .filter(|block| !matches!(&block.kind, InlineBlockKind::Matrix(_)))
        .collect();
    let mut disk_blocks: Vec<_> = append_blocks
        .iter()
        .copied()
        .filter(|block| block.key_index == KeyIndexChoice::Disk)
        .collect();
    disk_blocks.sort_by_key(|block| block.id);

    let stream_reader_methods = append_blocks.iter().map(|block| {
        let ty = &block.name;
        let plural = plural_method_ident(ty);
        quote! {
            pub fn #plural(
                &self,
            ) -> ::stratava::__core::Result<::stratava::__core::StreamingBlocks<#ty>> {
                self.inner.blocks::<#ty>()
            }
        }
    });
    let stream_writer_methods = append_blocks.iter().flat_map(|block| {
        if keyed_offset_chain && !block.key_fields.is_empty() {
            return Vec::new();
        }
        let ty = &block.name;
        let push = format_ident!("push_{}", singular_method_name(ty));
        let push_many = format_ident!("push_{}", plural_method_name(ty));
        let mut methods = vec![
            quote! {
                pub fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                    self.inner.push_info(value)
                }
            },
            quote! {
                pub fn #push_many<I>(
                    &mut self,
                    values: I,
                    options: ::stratava::__core::BatchOptions,
                ) -> ::core::result::Result<
                    ::stratava::__core::BatchAppendInfo,
                    ::stratava::__core::BatchAppendError,
                >
                where
                    I: ::core::iter::IntoIterator,
                    I::Item: ::core::borrow::Borrow<#ty>,
                {
                    self.inner.push_iter::<#ty, _>(values, options)
                }
            },
        ];
        if !block.key_fields.is_empty() {
            let delete = format_ident!("delete_{}", singular_method_name(ty));
            methods.push(quote! {
                pub fn #delete(
                    &mut self,
                    key: &<#ty as ::stratava::__core::StratavaKeyedBlock>::Key,
                ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                    self.inner.delete_with_prev_key_info::<#ty>(
                        key,
                        ::core::option::Option::None,
                    )
                }
            });
        }
        methods
    });

    let stream_constructors = quote! {
        pub fn open_stream_reader<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::stratava::__core::StreamOptions,
        ) -> ::stratava::__core::Result<#stream_reader_name> {
            ::core::result::Result::Ok(#stream_reader_name::from_inner(
                ::stratava::__core::StratavaStreamReader::open(Self::spec(), path, options)?,
            ))
        }

        pub fn create_stream_writer<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::stratava::__core::StreamOptions,
        ) -> ::stratava::__core::Result<#stream_writer_name> {
            ::core::result::Result::Ok(#stream_writer_name::from_inner(
                ::stratava::__core::StratavaStreamWriter::create(Self::spec(), path, options)?,
            ))
        }

        pub fn open_stream_writer<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::stratava::__core::StreamOptions,
        ) -> ::stratava::__core::Result<#stream_writer_name> {
            ::core::result::Result::Ok(#stream_writer_name::from_inner(
                ::stratava::__core::StratavaStreamWriter::open(Self::spec(), path, options)?,
            ))
        }

        pub fn restore_stream_writer<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::stratava::__core::StreamOptions,
        ) -> ::stratava::__core::Result<#stream_writer_name> {
            ::core::result::Result::Ok(#stream_writer_name::from_inner(
                ::stratava::__core::StratavaStreamWriter::restore_checkpoint_and_open(
                    Self::spec(),
                    path,
                    options,
                )?,
            ))
        }

        pub fn bootstrap_stream_checkpoint<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::stratava::__core::StreamOptions,
        ) -> ::stratava::__core::Result<::stratava::__core::StreamBootstrapReport> {
            ::stratava::__core::bootstrap_stream_checkpoint(Self::spec(), path, options)
        }

        pub fn bootstrap_stream_checkpoint_with_progress<P, F>(
            path: P,
            options: ::stratava::__core::StreamOptions,
            scan: ::stratava::__core::ScanOptions<'_>,
            observer: F,
        ) -> ::stratava::__core::Result<::stratava::__core::StreamBootstrapReport>
        where
            P: AsRef<::std::path::Path>,
            F: ::core::ops::FnMut(::stratava::__core::ScanProgress),
        {
            ::stratava::__core::bootstrap_stream_checkpoint_with_progress(
                Self::spec(), path, options, scan, observer,
            )
        }
    };
    let stream_api = quote! {
        #vis struct #stream_reader_name {
            inner: ::stratava::__core::StratavaStreamReader,
        }

        impl #stream_reader_name {
            pub fn from_inner(inner: ::stratava::__core::StratavaStreamReader) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> ::stratava::__core::StratavaStreamReader {
                self.inner
            }

            pub fn follow(&mut self) -> ::stratava::__core::Result<u64> {
                self.inner.follow()
            }

            pub fn committed_len(&self) -> u64 { self.inner.committed_len() }

            pub fn release_snapshot(&mut self) -> bool { self.inner.release_snapshot() }

            pub fn snapshot_status(&self) -> ::stratava::__core::Result<::stratava::__core::SnapshotStatus> {
                self.inner.snapshot_status()
            }


            pub fn follow_events(&mut self, events: &mut ::stratava::__core::StreamEvents) -> ::stratava::__core::Result<u64> {
                self.inner.follow_events(events)
            }

            pub fn follow_blocks<T: ::stratava::__core::StratavaBlock>(&mut self, blocks: &mut ::stratava::__core::StreamingBlocks<T>) -> ::stratava::__core::Result<u64> {
                self.inner.follow_blocks(blocks)
            }


            pub fn events(&self) -> ::stratava::__core::Result<::stratava::__core::StreamEvents> {
                self.inner.events()
            }

            pub fn verify_all(&self) -> ::stratava::__core::Result<u64> {
                self.inner.verify_all()
            }

            pub fn verify_all_with_progress<F>(
                &self,
                scan: ::stratava::__core::ScanOptions<'_>,
                observer: F,
            ) -> ::stratava::__core::Result<u64>
            where
                F: ::core::ops::FnMut(::stratava::__core::ScanProgress),
            {
                self.inner.verify_all_with_progress(scan, observer)
            }


            pub fn resident_state(&self) -> ::stratava::__core::StreamResidentState {
                self.inner.resident_state()
            }

            #(#stream_reader_methods)*
        }

        #vis struct #stream_writer_name {
            inner: ::stratava::__core::StratavaStreamWriter,
        }

        impl #stream_writer_name {
            pub fn from_inner(inner: ::stratava::__core::StratavaStreamWriter) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> ::stratava::__core::StratavaStreamWriter {
                self.inner
            }

            pub fn flush(&mut self) -> ::stratava::__core::Result<()> {
                self.inner.flush()
            }

            pub fn sync(&mut self) -> ::stratava::__core::Result<()> {
                self.inner.sync()
            }

            pub fn immediate(&mut self) -> ::stratava::__core::Result<()> {
                self.inner.immediate()
            }

            pub fn set_immediate_policy(&mut self, policy: ::stratava::__core::ImmediatePolicy) -> ::stratava::__core::Result<()> {
                self.inner.set_immediate_policy(policy)
            }

            /// Reclaim sidecar pages after an explicit sync; readers do not block it.
            pub fn compact_index(&mut self) -> ::stratava::__core::Result<::stratava::__core::IndexCompaction> {
                self.inner.compact_index()
            }


            pub fn resident_state(&self) -> ::stratava::__core::StreamResidentState {
                self.inner.resident_state()
            }

            #(#stream_writer_methods)*
        }
    };

    if disk_blocks.is_empty() {
        return (stream_constructors, quote!(#stream_api));
    }

    let indexed_reader_methods = disk_blocks.iter().map(|block| {
        let ty = &block.name;
        let get = format_ident!("get_{}", singular_method_name(ty));
        quote! {
            pub fn #get(
                &self,
                key: &<#ty as ::stratava::__core::StratavaKeyedBlock>::Key,
            ) -> ::stratava::__core::Result<::core::option::Option<#ty>> {
                self.inner.get::<#ty>(key)
            }
        }
    });
    let indexed_scan_methods = append_blocks.iter().map(|block| {
        let ty = &block.name;
        let plural = plural_method_ident(ty);
        quote! {
            pub fn #plural(
                &self,
            ) -> ::stratava::__core::Result<::stratava::__core::StreamingBlocks<#ty>> {
                self.inner.blocks::<#ty>()
            }
        }
    });
    let indexed_writer_methods = append_blocks.iter().flat_map(|block| {
        if keyed_offset_chain
            && !block.key_fields.is_empty()
            && block.key_index != KeyIndexChoice::Disk
        {
            return Vec::new();
        }
        let ty = &block.name;
        let push = format_ident!("push_{}", singular_method_name(ty));
        let push_many = format_ident!("push_{}", plural_method_name(ty));
        let mut methods = if block.key_index == KeyIndexChoice::Disk {
            vec![
                quote! {
                    pub fn #push(
                        &mut self,
                        value: &#ty,
                    ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                        self.inner.push_info(value)
                    }
                },
                quote! {
                    pub fn #push_many<I>(
                        &mut self,
                        values: I,
                        options: ::stratava::__core::BatchOptions,
                    ) -> ::core::result::Result<
                        ::stratava::__core::BatchAppendInfo,
                        ::stratava::__core::BatchAppendError,
                    >
                    where
                        I: ::core::iter::IntoIterator,
                        I::Item: ::core::borrow::Borrow<#ty>,
                    {
                        self.inner.push_iter::<#ty, _>(values, options)
                    }
                },
            ]
        } else {
            vec![
                quote! {
                    pub fn #push(
                        &mut self,
                        value: &#ty,
                    ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                        self.inner.push_unindexed_info(value)
                    }
                },
                quote! {
                    pub fn #push_many<I>(
                        &mut self,
                        values: I,
                        options: ::stratava::__core::BatchOptions,
                    ) -> ::core::result::Result<
                        ::stratava::__core::BatchAppendInfo,
                        ::stratava::__core::BatchAppendError,
                    >
                    where
                        I: ::core::iter::IntoIterator,
                        I::Item: ::core::borrow::Borrow<#ty>,
                    {
                        self.inner
                            .push_unindexed_iter::<#ty, _>(values, options)
                    }
                },
            ]
        };
        if !block.key_fields.is_empty() {
            let delete = format_ident!("delete_{}", singular_method_name(ty));
            methods.push(if block.key_index == KeyIndexChoice::Disk {
                quote! {
                    pub fn #delete(
                        &mut self,
                        key: &<#ty as ::stratava::__core::StratavaKeyedBlock>::Key,
                    ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                        self.inner.delete_info::<#ty>(key)
                    }
                }
            } else {
                quote! {
                pub fn #delete(
                    &mut self,
                    key: &<#ty as ::stratava::__core::StratavaKeyedBlock>::Key,
                ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                    self.inner.delete_unindexed_info::<#ty>(key)
                }
                }
            });
        }
        methods
    });
    let indexed_block_descriptors = disk_blocks.iter().map(|block| {
        let ty = &block.name;
        quote!(::stratava::__core::DiskIndexedBlock::of::<#ty>())
    });
    let indexed_block_descriptors: Vec<_> = indexed_block_descriptors.collect();
    let indexed_constructors = quote! {
        pub fn disk_index_plan() -> ::stratava::__core::Result<::stratava::__core::DiskIndexPlan> {
            const BLOCKS: &[::stratava::__core::DiskIndexedBlock] = &[
                #(#indexed_block_descriptors),*
            ];
            ::stratava::__core::DiskIndexPlan::canonical(Self::spec(), BLOCKS)
                .map_err(|error| ::stratava::__core::Error::DiskIndex(::std::boxed::Box::new(error)))
        }

        pub fn open_indexed_reader<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::stratava::__core::DiskIndexOptions,
        ) -> ::stratava::__core::Result<#indexed_reader_name> {
            ::core::result::Result::Ok(#indexed_reader_name::from_inner(
                ::stratava::__core::StratavaIndexedReader::open(
                    Self::spec(),
                    path,
                    options,
                    Self::disk_index_plan()?,
                )?,
            ))
        }

        pub fn create_indexed_writer<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::stratava::__core::DiskIndexOptions,
        ) -> ::stratava::__core::Result<#indexed_writer_name> {
            ::core::result::Result::Ok(#indexed_writer_name::from_inner(
                ::stratava::__core::StratavaIndexedWriter::create(
                    Self::spec(),
                    path,
                    options,
                    Self::disk_index_plan()?,
                )?,
            ))
        }

        pub fn open_indexed_writer<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::stratava::__core::DiskIndexOptions,
        ) -> ::stratava::__core::Result<#indexed_writer_name> {
            ::core::result::Result::Ok(#indexed_writer_name::from_inner(
                ::stratava::__core::StratavaIndexedWriter::open(
                    Self::spec(),
                    path,
                    options,
                    Self::disk_index_plan()?,
                )?,
            ))
        }


        pub fn restore_indexed_writer<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::stratava::__core::DiskIndexOptions,
        ) -> ::stratava::__core::Result<#indexed_writer_name> {
            ::core::result::Result::Ok(#indexed_writer_name::from_inner(
                ::stratava::__core::StratavaIndexedWriter::restore_checkpoint_and_open(
                    Self::spec(),
                    path,
                    options,
                    Self::disk_index_plan()?,
                )?,
            ))
        }

        pub fn rebuild_disk_index<P: AsRef<::std::path::Path>>(
            path: P,
            options: ::stratava::__core::DiskIndexOptions,
        ) -> ::stratava::__core::Result<::stratava::__core::DiskIndexRebuildReport> {
            ::stratava::__core::rebuild_disk_index(
                Self::spec(),
                path,
                options,
                Self::disk_index_plan()?,
            )
        }

        pub fn rebuild_disk_index_with_progress<P, F>(
            path: P,
            options: ::stratava::__core::DiskIndexOptions,
            scan: ::stratava::__core::ScanOptions<'_>,
            observer: F,
        ) -> ::stratava::__core::Result<::stratava::__core::DiskIndexRebuildReport>
        where
            P: AsRef<::std::path::Path>,
            F: ::core::ops::FnMut(::stratava::__core::ScanProgress),
        {
            ::stratava::__core::rebuild_disk_index_with_progress(
                Self::spec(),
                path,
                options,
                Self::disk_index_plan()?,
                scan,
                observer,
            )
        }
    };
    let indexed_api = quote! {
        #vis struct #indexed_reader_name {
            inner: ::stratava::__core::StratavaIndexedReader,
        }

        impl #indexed_reader_name {
            pub fn from_inner(inner: ::stratava::__core::StratavaIndexedReader) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> ::stratava::__core::StratavaIndexedReader {
                self.inner
            }


            pub fn resident_state(&self) -> ::stratava::__core::StreamResidentState {
                self.inner.resident_state()
            }

            pub fn follow(&mut self) -> ::stratava::__core::Result<u64> {
                self.inner.follow()
            }

            pub fn committed_len(&self) -> u64 { self.inner.committed_len() }

            pub fn release_snapshot(&mut self) -> bool { self.inner.release_snapshot() }

            pub fn snapshot_status(&self) -> ::stratava::__core::Result<::stratava::__core::SnapshotStatus> {
                self.inner.snapshot_status()
            }


            pub fn follow_events(&mut self, events: &mut ::stratava::__core::StreamEvents) -> ::stratava::__core::Result<u64> {
                self.inner.follow_events(events)
            }

            pub fn follow_blocks<T: ::stratava::__core::StratavaBlock>(&mut self, blocks: &mut ::stratava::__core::StreamingBlocks<T>) -> ::stratava::__core::Result<u64> {
                self.inner.follow_blocks(blocks)
            }


            pub fn events(&self) -> ::stratava::__core::Result<::stratava::__core::StreamEvents> {
                self.inner.events()
            }

            pub fn verify_all(&self) -> ::stratava::__core::Result<u64> {
                self.inner.verify_all()
            }

            pub fn verify_all_with_progress<F>(
                &self,
                scan: ::stratava::__core::ScanOptions<'_>,
                observer: F,
            ) -> ::stratava::__core::Result<u64>
            where
                F: ::core::ops::FnMut(::stratava::__core::ScanProgress),
            {
                self.inner.verify_all_with_progress(scan, observer)
            }

            #(#indexed_scan_methods)*
            #(#indexed_reader_methods)*
        }

        #vis struct #indexed_writer_name {
            inner: ::stratava::__core::StratavaIndexedWriter,
        }

        impl #indexed_writer_name {
            pub fn from_inner(inner: ::stratava::__core::StratavaIndexedWriter) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> ::stratava::__core::StratavaIndexedWriter {
                self.inner
            }

            pub fn flush(&mut self) -> ::stratava::__core::Result<()> {
                self.inner.flush()
            }

            pub fn sync(&mut self) -> ::stratava::__core::Result<()> {
                self.inner.sync()
            }

            pub fn immediate(&mut self) -> ::stratava::__core::Result<()> {
                self.inner.immediate()
            }

            pub fn set_immediate_policy(&mut self, policy: ::stratava::__core::ImmediatePolicy) -> ::stratava::__core::Result<()> {
                self.inner.set_immediate_policy(policy)
            }

            /// Reclaim sidecar pages after an explicit sync; readers do not block it.
            pub fn compact_index(&mut self) -> ::stratava::__core::Result<::stratava::__core::IndexCompaction> {
                self.inner.compact_index()
            }


            pub fn resident_state(&self) -> ::stratava::__core::StreamResidentState {
                self.inner.resident_state()
            }

            #(#indexed_writer_methods)*
        }
    };

    (
        quote!(#stream_constructors #indexed_constructors),
        quote!(#stream_api #indexed_api),
    )
}

/// Rustdoc for a generated typed writer (P-01).
///
/// The resident writer is the convenient default, and for a format whose
/// distinct key count is large it is the wrong tool. The published pages are
/// where a caller decides that, so the cost model and the alternative are
/// stated on the type itself rather than only in `docs/known-limitations.md`
/// (§2.2) and `docs/api-reference.md`.
pub(super) fn generated_writer_doc(format_name: &Ident, keyed_blocks: &[&InlineBlock]) -> String {
    if keyed_blocks.is_empty() {
        return format!(
            "Typed resident writer for the `{format_name}` format.\n\
             \n\
             This format declares no keyed block, so construction primes no \
             keyed-tail cache. The writer holds the whole record index in \
             memory, which is the resident (non-petabyte) path: for a file \
             whose record count is chosen by an untrusted producer, bound it \
             with `ReadLimits` or use the streaming/indexed APIs."
        );
    }
    let names: Vec<String> = keyed_blocks
        .iter()
        .map(|block| format!("`{}`", block.name))
        .collect();
    let count = keyed_blocks.len();
    format!(
        "Typed resident writer for the `{format_name}` format.\n\
         \n\
         # Construction cost and cache size\n\
         \n\
         Constructing this writer primes one resident keyed-tail map per keyed \
         block type. This format declares {count} ({names}), and each priming \
         pass walks the resident record index, so construction costs \
         `Theta(M*N)` for `M` keyed block types and `N` resident index \
         entries.\n\
         \n\
         The retained cache is bounded per block id, not per file: \
         `ReadLimits::max_keyed_tail_bytes` is checked against each block's own \
         map, so the aggregate a single writer can retain is about `M` times \
         that ceiling. Both facts are the documented behaviour of the resident \
         path, not a limit violation.\n\
         \n\
         # High-cardinality formats should not use this writer\n\
         \n\
         If the number of distinct keys is large, unbounded, or chosen by an \
         untrusted producer, declare `key_index = disk` on the keyed blocks and \
         use the generated disk-indexed API (`{format_name}IndexedWriter` \
         through `create_indexed_writer` / `open_indexed_writer`, and \
         `{format_name}IndexedReader` through `open_indexed_reader`) instead. \
         Those keep key state in the native sidecar rather than in this process, \
         they perform no per-construction index walk, and they are the APIs the \
         petabyte-scale contract covers. The resident writer is explicitly not \
         that path.",
        names = names.join(", "),
    )
}

/// Rustdoc for the generated writer's `from_inner` (P-01).
pub(super) fn generated_writer_construction_doc(keyed_blocks: &[&InlineBlock]) -> String {
    if keyed_blocks.is_empty() {
        return "Wraps an open `StratavaWriter`. This format declares no keyed \
                block, so nothing is primed and this cannot fail on cache \
                budget."
            .to_string();
    }
    format!(
        "Wraps an open `StratavaWriter`, priming the resident keyed-tail map of \
         each of this format's {count} keyed block types.\n\
         \n\
         The priming is fallible and happens **before** any mutation: a file \
         whose distinct key count exceeds `ReadLimits::max_keyed_tail_bytes` \
         for one of those blocks is refused here, not at a later append. Each \
         pass walks the resident record index, so this is the `Theta(M*N)` step \
         described on the type; a format with high key cardinality should use \
         the disk-indexed API instead.",
        count = keyed_blocks.len(),
    )
}

pub(super) fn typed_api_tokens(
    vis: &Visibility,
    format_name: &Ident,
    blocks: &[InlineBlock],
    matrix_commit: Option<&MatrixCommit>,
    matrix_aux: &[MatrixAux],
) -> TokenStream2 {
    let reader_name = format_ident!("{}Reader", format_name);
    let writer_name = format_ident!("{}Writer", format_name);
    let read_trait = format_ident!("{}Read", format_name);
    let write_trait = format_ident!("{}Write", format_name);

    let append_blocks: Vec<_> = blocks
        .iter()
        .filter(|block| !matches!(&block.kind, InlineBlockKind::Matrix(_)))
        .collect();
    let matrix_blocks: Vec<_> = blocks
        .iter()
        .filter(|block| matches!(&block.kind, InlineBlockKind::Matrix(_)))
        .collect();

    let keyed_blocks: Vec<_> = append_blocks
        .iter()
        .filter(|block| !block.key_fields.is_empty())
        .copied()
        .collect();
    // F-01: the writer owns no keyed tail state of its own. Every keyed
    // mutation routes through the byte-keyed resident cache inside
    // `StratavaFile`, so the only step after an authoritative append is an
    // insert into a `HashMap<Vec<u8>, u64>` slot reserved before it - no
    // user-defined `Hash`, `Eq` or `Clone` can run after publication, and no
    // per-writer map has to be translated after a record replacement moves
    // offsets (the file invalidates its own cache there).
    //
    // Construction primes a bounded cache backed by a private scratch index.
    // A full cache spills before append; its budget does not cap key count.
    let writer_tail_primes = keyed_blocks.iter().map(|block| {
        let ty = &block.name;
        quote!(inner.prime_keyed_tails::<#ty>()?;)
    });
    // A format with no keyed block primes nothing, so the binding must not be
    // `mut` there.
    let writer_inner_binding = if keyed_blocks.is_empty() {
        quote!(inner)
    } else {
        quote!(mut inner)
    };
    // P-01, documentation only. Construction primes one keyed-tail map per
    // keyed block type and each priming pass walks the whole resident index,
    // so the cost is Theta(M*N) and the retained cache can reach roughly
    // M * `keyed_tail` because that ceiling is per block id, not per file.
    // That is the documented behaviour of the resident (non-PB) path rather
    // than a contract violation, so the generated pages say it out loud and
    // name the disk-indexed API a high-cardinality format should use instead.
    let writer_doc = generated_writer_doc(format_name, &keyed_blocks);
    let writer_construction_doc = generated_writer_construction_doc(&keyed_blocks);

    let reader_inherent_methods = append_blocks.iter().flat_map(|block| reader_methods(block));
    let reader_trait_methods = append_blocks
        .iter()
        .flat_map(|block| reader_trait_methods(block));
    let reader_trait_impl_methods = append_blocks
        .iter()
        .flat_map(|block| reader_trait_impl_methods(block));
    let writer_inherent_methods = append_blocks.iter().flat_map(|block| writer_methods(block));
    let writer_trait_methods = append_blocks
        .iter()
        .flat_map(|block| writer_trait_methods(block));
    let writer_trait_impl_methods = append_blocks
        .iter()
        .flat_map(|block| writer_trait_impl_methods(block));

    let matrix_reader_inherent_methods =
        matrix_reader_methods(&matrix_blocks, matrix_commit, MatrixMethodTarget::Inherent);
    let matrix_reader_trait_methods =
        matrix_reader_methods(&matrix_blocks, matrix_commit, MatrixMethodTarget::Trait);
    let matrix_reader_trait_impl_methods =
        matrix_reader_methods(&matrix_blocks, matrix_commit, MatrixMethodTarget::TraitImpl);
    let matrix_writer_inherent_methods =
        matrix_writer_methods(&matrix_blocks, matrix_commit, MatrixMethodTarget::Inherent);
    let matrix_writer_trait_methods =
        matrix_writer_methods(&matrix_blocks, matrix_commit, MatrixMethodTarget::Trait);
    let matrix_writer_trait_impl_methods =
        matrix_writer_methods(&matrix_blocks, matrix_commit, MatrixMethodTarget::TraitImpl);
    let matrix_reader_aux_inherent_methods =
        matrix_reader_aux_methods(matrix_aux, MatrixMethodTarget::Inherent);
    let matrix_reader_aux_trait_methods =
        matrix_reader_aux_methods(matrix_aux, MatrixMethodTarget::Trait);
    let matrix_reader_aux_trait_impl_methods =
        matrix_reader_aux_methods(matrix_aux, MatrixMethodTarget::TraitImpl);
    let matrix_writer_aux_inherent_methods =
        matrix_writer_aux_methods(matrix_aux, MatrixMethodTarget::Inherent);
    let matrix_writer_aux_trait_methods =
        matrix_writer_aux_methods(matrix_aux, MatrixMethodTarget::Trait);
    let matrix_writer_aux_trait_impl_methods =
        matrix_writer_aux_methods(matrix_aux, MatrixMethodTarget::TraitImpl);

    quote! {
        #vis trait #read_trait {
            #(#reader_trait_methods)*
            #matrix_reader_trait_methods
            #matrix_reader_aux_trait_methods
        }

        #vis trait #write_trait {
            #(#writer_trait_methods)*
            #matrix_writer_trait_methods
            #matrix_writer_aux_trait_methods

            fn commit(&mut self) -> ::stratava::__core::Result<::stratava::__core::AppendInfo>;
            fn commit_durable(&mut self) -> ::stratava::__core::Result<::stratava::__core::AppendInfo>;
            fn flush(&mut self) -> ::stratava::__core::Result<()>;
            fn sync(&mut self) -> ::stratava::__core::Result<()>;
        }

        #[derive(Debug)]
        #vis struct #reader_name {
            inner: ::stratava::__core::StratavaReader,
        }

        impl #reader_name {
            pub fn from_inner(inner: ::stratava::__core::StratavaReader) -> Self {
                Self { inner }
            }

            pub fn follow(&mut self) -> ::stratava::__core::Result<u64> { self.inner.follow() }
            pub fn matrix_generation(&self) -> Option<u64> { self.inner.matrix_generation() }
            pub fn matrix_generation_path(&self) -> Option<&::std::path::Path> {self.inner.matrix_generation_path()}

            pub fn into_inner(self) -> ::stratava::__core::StratavaReader {
                self.inner
            }

            pub fn spec(&self) -> ::stratava::__core::FormatSpec {
                self.inner.spec()
            }

            pub fn path(&self) -> &::std::path::Path {
                self.inner.path()
            }

            #(#reader_inherent_methods)*
            #matrix_reader_inherent_methods
            #matrix_reader_aux_inherent_methods
        }

        impl #read_trait for #reader_name {
            #(#reader_trait_impl_methods)*
            #matrix_reader_trait_impl_methods
            #matrix_reader_aux_trait_impl_methods
        }

        #[doc = #writer_doc]
        #[derive(Debug)]
        #vis struct #writer_name {
            inner: ::stratava::__core::StratavaWriter,
        }

        impl #writer_name {
            #[doc = #writer_construction_doc]
            pub fn from_inner(
                #writer_inner_binding: ::stratava::__core::StratavaWriter,
            ) -> ::stratava::__core::Result<Self> {
                #(#writer_tail_primes)*
                ::core::result::Result::Ok(Self { inner })
            }

            pub fn compact_matrix(&mut self) -> ::stratava::__core::Result<()> { self.inner.compact_matrix() }
            pub fn matrix_generation(&self) -> Option<u64> {self.inner.matrix_generation()}
            pub fn matrix_generation_path(&self) -> Option<&::std::path::Path> {self.inner.matrix_generation_path()}

            pub fn into_inner(self) -> ::stratava::__core::StratavaWriter {
                self.inner
            }

            pub fn spec(&self) -> ::stratava::__core::FormatSpec {
                self.inner.spec()
            }

            pub fn path(&self) -> &::std::path::Path {
                self.inner.path()
            }

            pub fn commit(&mut self) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                self.inner.commit()
            }

            pub fn commit_durable(&mut self) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                self.inner.commit_durable()
            }

            pub fn flush(&mut self) -> ::stratava::__core::Result<()> {
                self.inner.flush()
            }

            pub fn sync(&mut self) -> ::stratava::__core::Result<()> {
                self.inner.sync()
            }

            pub fn immediate(&mut self) -> ::stratava::__core::Result<()> {
                self.inner.immediate()
            }

            pub fn set_immediate_policy(&mut self, policy: ::stratava::__core::ImmediatePolicy) -> ::stratava::__core::Result<()> {
                self.inner.set_immediate_policy(policy)
            }


            /// The `StratavaWriter` this wrapper holds, borrowed rather than
            /// consumed. `into_inner` takes `self` and ends the wrapper, which
            /// makes every `&mut self` entry point below it unreachable from a
            /// writer still being written through.
            pub fn inner_mut(&mut self) -> &mut ::stratava::__core::StratavaWriter {
                &mut self.inner
            }

            /// The `StratavaWriter` this wrapper holds, borrowed for reading.
            pub fn inner(&self) -> &::stratava::__core::StratavaWriter {
                &self.inner
            }

            /// Reads a block out of the editable header region.
            pub fn read_header_block<T: ::stratava::__core::StratavaBlock>(
                &self,
            ) -> ::stratava::__core::Result<::core::option::Option<T>> {
                self.inner.read_header_block::<T>()
            }

            /// Writes a block into the editable header region, replacing any
            /// earlier value for the same block id.
            pub fn write_header_block<T: ::stratava::__core::StratavaBlock>(
                &mut self,
                block: &T,
            ) -> ::stratava::__core::Result<()> {
                self.inner.write_header_block(block)
            }

            /// Removes a block from the editable header region.
            pub fn remove_header_block<T: ::stratava::__core::StratavaBlock>(
                &mut self,
            ) -> ::stratava::__core::Result<()> {
                self.inner.remove_header_block::<T>()
            }

            /// Fixes the region's checksum and refuses every later write.
            pub fn seal_header_slots(&mut self) -> ::stratava::__core::Result<()> {
                self.inner.seal_header_slots()
            }

            /// The declared size of the editable header region; `0` when
            /// undeclared.
            pub fn header_slots_capacity(&self) -> usize {
                self.inner.header_slots_capacity()
            }

            /// How much of the region the current blocks occupy.
            pub fn header_slots_used(&self) -> ::stratava::__core::Result<usize> {
                self.inner.header_slots_used()
            }

            /// How much of the region is left.
            pub fn header_slots_free_bytes(&self) -> ::stratava::__core::Result<usize> {
                self.inner.header_slots_free_bytes()
            }

            /// Whether the region's seal is down.
            pub fn header_slots_sealed(&self) -> ::stratava::__core::Result<bool> {
                self.inner.header_slots_sealed()
            }

            #(#writer_inherent_methods)*
            #matrix_writer_inherent_methods
            #matrix_writer_aux_inherent_methods
        }

        impl #write_trait for #writer_name {
            #(#writer_trait_impl_methods)*
            #matrix_writer_trait_impl_methods
            #matrix_writer_aux_trait_impl_methods

            fn commit(&mut self) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                self.commit()
            }

            fn commit_durable(&mut self) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                self.commit_durable()
            }

            fn flush(&mut self) -> ::stratava::__core::Result<()> {
                self.flush()
            }

            fn sync(&mut self) -> ::stratava::__core::Result<()> {
                self.sync()
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum MatrixMethodTarget {
    Inherent,
    Trait,
    TraitImpl,
}

pub(super) fn matrix_reader_aux_methods(
    aux: &[MatrixAux],
    target: MatrixMethodTarget,
) -> TokenStream2 {
    let methods = aux.iter().map(|aux| {
        let name = aux.name.to_string();
        let len_method = format_ident!("{}_aux_len", aux.name);
        let read_method = format_ident!("read_{}_aux", aux.name);
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #len_method(&self) -> ::stratava::__core::Result<u64> {
                    self.inner.matrix_aux_len(#name)
                }

                pub fn #read_method(
                    &self,
                    offset: u64,
                    len: u64,
                ) -> ::stratava::__core::Result<::std::vec::Vec<u8>> {
                    self.inner.read_matrix_aux(#name, offset, len)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #len_method(&self) -> ::stratava::__core::Result<u64>;

                fn #read_method(
                    &self,
                    offset: u64,
                    len: u64,
                ) -> ::stratava::__core::Result<::std::vec::Vec<u8>>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #len_method(&self) -> ::stratava::__core::Result<u64> {
                    self.inner.matrix_aux_len(#name)
                }

                fn #read_method(
                    &self,
                    offset: u64,
                    len: u64,
                ) -> ::stratava::__core::Result<::std::vec::Vec<u8>> {
                    self.inner.read_matrix_aux(#name, offset, len)
                }
            },
        }
    });
    quote! {
        #(#methods)*
    }
}

pub(super) fn matrix_writer_aux_methods(
    aux: &[MatrixAux],
    target: MatrixMethodTarget,
) -> TokenStream2 {
    let methods = aux.iter().map(|aux| {
        let name = aux.name.to_string();
        let len_method = format_ident!("{}_aux_len", aux.name);
        let read_method = format_ident!("read_{}_aux", aux.name);
        let write_method = format_ident!("write_{}_aux", aux.name);
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #len_method(&self) -> ::stratava::__core::Result<u64> {
                    self.inner.matrix_aux_len(#name)
                }

                pub fn #read_method(
                    &self,
                    offset: u64,
                    len: u64,
                ) -> ::stratava::__core::Result<::std::vec::Vec<u8>> {
                    self.inner.read_matrix_aux(#name, offset, len)
                }

                pub fn #write_method(
                    &mut self,
                    offset: u64,
                    payload: &[u8],
                ) -> ::stratava::__core::Result<()> {
                    self.inner.write_matrix_aux(#name, offset, payload)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #len_method(&self) -> ::stratava::__core::Result<u64>;

                fn #read_method(
                    &self,
                    offset: u64,
                    len: u64,
                ) -> ::stratava::__core::Result<::std::vec::Vec<u8>>;

                fn #write_method(
                    &mut self,
                    offset: u64,
                    payload: &[u8],
                ) -> ::stratava::__core::Result<()>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #len_method(&self) -> ::stratava::__core::Result<u64> {
                    self.inner.matrix_aux_len(#name)
                }

                fn #read_method(
                    &self,
                    offset: u64,
                    len: u64,
                ) -> ::stratava::__core::Result<::std::vec::Vec<u8>> {
                    self.inner.read_matrix_aux(#name, offset, len)
                }

                fn #write_method(
                    &mut self,
                    offset: u64,
                    payload: &[u8],
                ) -> ::stratava::__core::Result<()> {
                    self.inner.write_matrix_aux(#name, offset, payload)
                }
            },
        }
    });
    quote! {
        #(#methods)*
    }
}

pub(super) fn matrix_reader_methods(
    blocks: &[&InlineBlock],
    commit: Option<&MatrixCommit>,
    target: MatrixMethodTarget,
) -> TokenStream2 {
    let cell_methods = blocks.iter().map(|block| {
        let ty = &block.name;
        let key_ty = format_ident!("{}Key", ty);
        let method = format_ident!("{}", singular_method_name(ty));
        let status = format_ident!("{}_status", singular_method_name(ty));
        let run = format_ident!("{}_run_into", singular_method_name(ty));
        let run_bytes = format_ident!("{}_run_bytes_into", singular_method_name(ty));
        let row = format_ident!("{}_row_into", singular_method_name(ty));
        let column = format_ident!("{}_column_into", singular_method_name(ty));
        let column_bytes = format_ident!("{}_column_bytes_into", singular_method_name(ty));
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #run(&self, start: #key_ty, out: &mut [#ty]) -> ::stratava::__core::Result<()> {
                    self.inner.read_matrix_run_into::<#ty>(start.into(), out)
                }
                pub fn #run_bytes(&self, start: #key_ty, out: &mut [u8]) -> ::stratava::__core::Result<()> {
                    self.inner.read_matrix_run_bytes_into::<#ty>(start.into(), out)
                }
                pub fn #column(&self, start: #key_ty, out: &mut [#ty]) -> ::stratava::__core::Result<()> {
                    self.inner.read_matrix_column_into::<#ty>(start.into(), out)
                }
                pub fn #column_bytes(&self, start: #key_ty, out: &mut [u8]) -> ::stratava::__core::Result<()> {
                    self.inner.read_matrix_column_bytes_into::<#ty>(start.into(), out)
                }
                pub fn #row(&self, first_axis: u64, out: &mut [#ty]) -> ::stratava::__core::Result<()> {
                    self.inner.read_matrix_row_into::<#ty>(first_axis, out)
                }

                pub fn #method(&self, key: #key_ty) -> ::stratava::__core::Result<#ty> {
                    self.inner.read_matrix_cell::<#ty>(key.into())
                }

                pub fn #status(
                    &self,
                    key: #key_ty,
                ) -> ::stratava::__core::Result<::stratava::__core::MatrixCellStatus> {
                    self.inner.matrix_cell_status::<#ty>(key.into())
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #run(&self, start: #key_ty, out: &mut [#ty]) -> ::stratava::__core::Result<()>;
                fn #run_bytes(&self, start: #key_ty, out: &mut [u8]) -> ::stratava::__core::Result<()>;
                fn #column(&self, start: #key_ty, out: &mut [#ty]) -> ::stratava::__core::Result<()>;
                fn #column_bytes(&self, start: #key_ty, out: &mut [u8]) -> ::stratava::__core::Result<()>;
                fn #row(&self, first_axis: u64, out: &mut [#ty]) -> ::stratava::__core::Result<()>;

                fn #method(&self, key: #key_ty) -> ::stratava::__core::Result<#ty>;

                fn #status(
                    &self,
                    key: #key_ty,
                ) -> ::stratava::__core::Result<::stratava::__core::MatrixCellStatus>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #run(&self, start: #key_ty, out: &mut [#ty]) -> ::stratava::__core::Result<()> {
                    self.inner.read_matrix_run_into::<#ty>(start.into(), out)
                }
                fn #run_bytes(&self, start: #key_ty, out: &mut [u8]) -> ::stratava::__core::Result<()> {
                    self.inner.read_matrix_run_bytes_into::<#ty>(start.into(), out)
                }
                fn #column(&self, start: #key_ty, out: &mut [#ty]) -> ::stratava::__core::Result<()> {
                    self.inner.read_matrix_column_into::<#ty>(start.into(), out)
                }
                fn #column_bytes(&self, start: #key_ty, out: &mut [u8]) -> ::stratava::__core::Result<()> {
                    self.inner.read_matrix_column_bytes_into::<#ty>(start.into(), out)
                }
                fn #row(&self, first_axis: u64, out: &mut [#ty]) -> ::stratava::__core::Result<()> {
                    self.inner.read_matrix_row_into::<#ty>(first_axis, out)
                }

                fn #method(&self, key: #key_ty) -> ::stratava::__core::Result<#ty> {
                    self.inner.read_matrix_cell::<#ty>(key.into())
                }

                fn #status(
                    &self,
                    key: #key_ty,
                ) -> ::stratava::__core::Result<::stratava::__core::MatrixCellStatus> {
                    self.inner.matrix_cell_status::<#ty>(key.into())
                }
            },
        }
    });
    let flag_methods = matrix_reader_flag_methods(commit, target);
    quote! {
        #(#cell_methods)*
        #flag_methods
    }
}

pub(super) fn matrix_writer_methods(
    blocks: &[&InlineBlock],
    commit: Option<&MatrixCommit>,
    target: MatrixMethodTarget,
) -> TokenStream2 {
    let cell_methods = blocks.iter().map(|block| {
        let ty = &block.name;
        let key_ty = format_ident!("{}Key", ty);
        let name = singular_method_name(ty);
        let write = format_ident!("write_{}", name);
        let commit_method = format_ident!("commit_{}", name);
        let clear = format_ident!("clear_{}", name);
        let rebuild = format_ident!("rebuild_{}_commit_from_crc", name);
        let status = format_ident!("{}_status", name);
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #write(
                    &mut self,
                    key: #key_ty,
                    value: &#ty,
                ) -> ::stratava::__core::Result<()> {
                    self.inner.write_matrix_cell::<#ty>(key.into(), value)
                }

                pub fn #commit_method(&mut self, key: #key_ty) -> ::stratava::__core::Result<()> {
                    self.inner.commit_matrix_cell::<#ty>(key.into())
                }

                pub fn #clear(&mut self, key: #key_ty) -> ::stratava::__core::Result<()> {
                    self.inner.clear_matrix_cell::<#ty>(key.into())
                }

                pub fn #rebuild(&mut self) -> ::stratava::__core::Result<u64> {
                    self.inner.rebuild_matrix_commit_from_crc::<#ty>()
                }

                pub fn #status(
                    &self,
                    key: #key_ty,
                ) -> ::stratava::__core::Result<::stratava::__core::MatrixCellStatus> {
                    self.inner.matrix_cell_status::<#ty>(key.into())
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #write(
                    &mut self,
                    key: #key_ty,
                    value: &#ty,
                ) -> ::stratava::__core::Result<()>;

                fn #commit_method(&mut self, key: #key_ty) -> ::stratava::__core::Result<()>;

                fn #clear(&mut self, key: #key_ty) -> ::stratava::__core::Result<()>;

                fn #rebuild(&mut self) -> ::stratava::__core::Result<u64>;

                fn #status(
                    &self,
                    key: #key_ty,
                ) -> ::stratava::__core::Result<::stratava::__core::MatrixCellStatus>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #write(
                    &mut self,
                    key: #key_ty,
                    value: &#ty,
                ) -> ::stratava::__core::Result<()> {
                    self.inner.write_matrix_cell::<#ty>(key.into(), value)
                }

                fn #commit_method(&mut self, key: #key_ty) -> ::stratava::__core::Result<()> {
                    self.inner.commit_matrix_cell::<#ty>(key.into())
                }

                fn #clear(&mut self, key: #key_ty) -> ::stratava::__core::Result<()> {
                    self.inner.clear_matrix_cell::<#ty>(key.into())
                }

                fn #rebuild(&mut self) -> ::stratava::__core::Result<u64> {
                    self.inner.rebuild_matrix_commit_from_crc::<#ty>()
                }

                fn #status(
                    &self,
                    key: #key_ty,
                ) -> ::stratava::__core::Result<::stratava::__core::MatrixCellStatus> {
                    self.inner.matrix_cell_status::<#ty>(key.into())
                }
            },
        }
    });
    let flag_methods = matrix_writer_flag_methods(commit, target);
    quote! {
        #(#cell_methods)*
        #flag_methods
    }
}

pub(super) fn matrix_reader_flag_methods(
    commit: Option<&MatrixCommit>,
    target: MatrixMethodTarget,
) -> TokenStream2 {
    let Some(commit) = commit else {
        return quote!();
    };
    let single_methods = commit.singles.iter().map(|flag| {
        let method = format_ident!("is_{}_committed", flag);
        let name = flag.to_string();
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #method(&self) -> ::stratava::__core::Result<bool> {
                    self.inner.is_matrix_single_committed(#name)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #method(&self) -> ::stratava::__core::Result<bool>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #method(&self) -> ::stratava::__core::Result<bool> {
                    self.inner.is_matrix_single_committed(#name)
                }
            },
        }
    });
    let channel_methods = commit.per_channel.iter().map(|flag| {
        let method = format_ident!("is_{}_committed", flag);
        let name = flag.to_string();
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #method(&self, channel: u64) -> ::stratava::__core::Result<bool> {
                    self.inner.is_matrix_channel_committed(#name, channel)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #method(&self, channel: u64) -> ::stratava::__core::Result<bool>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #method(&self, channel: u64) -> ::stratava::__core::Result<bool> {
                    self.inner.is_matrix_channel_committed(#name, channel)
                }
            },
        }
    });
    quote! {
        #(#single_methods)*
        #(#channel_methods)*
    }
}

pub(super) fn matrix_writer_flag_methods(
    commit: Option<&MatrixCommit>,
    target: MatrixMethodTarget,
) -> TokenStream2 {
    let Some(commit) = commit else {
        return quote!();
    };
    let category_methods = commit.categories.iter().map(|category| {
        let method = format_ident!("clear_{}_category", category);
        let name = category.to_string();
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #method(&mut self) -> ::stratava::__core::Result<u64> {
                    self.inner.clear_matrix_category(#name)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #method(&mut self) -> ::stratava::__core::Result<u64>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #method(&mut self) -> ::stratava::__core::Result<u64> {
                    self.inner.clear_matrix_category(#name)
                }
            },
        }
    });
    let single_methods = commit.singles.iter().map(|flag| {
        let get_method = format_ident!("is_{}_committed", flag);
        let set_method = format_ident!("set_{}_committed", flag);
        let name = flag.to_string();
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #get_method(&self) -> ::stratava::__core::Result<bool> {
                    self.inner.is_matrix_single_committed(#name)
                }

                pub fn #set_method(&mut self, value: bool) -> ::stratava::__core::Result<()> {
                    self.inner.set_matrix_single_committed(#name, value)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #get_method(&self) -> ::stratava::__core::Result<bool>;

                fn #set_method(&mut self, value: bool) -> ::stratava::__core::Result<()>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #get_method(&self) -> ::stratava::__core::Result<bool> {
                    self.inner.is_matrix_single_committed(#name)
                }

                fn #set_method(&mut self, value: bool) -> ::stratava::__core::Result<()> {
                    self.inner.set_matrix_single_committed(#name, value)
                }
            },
        }
    });
    let channel_methods = commit.per_channel.iter().map(|flag| {
        let get_method = format_ident!("is_{}_committed", flag);
        let set_method = format_ident!("set_{}_committed", flag);
        let name = flag.to_string();
        match target {
            MatrixMethodTarget::Inherent => quote! {
                pub fn #get_method(&self, channel: u64) -> ::stratava::__core::Result<bool> {
                    self.inner.is_matrix_channel_committed(#name, channel)
                }

                pub fn #set_method(
                    &mut self,
                    channel: u64,
                    value: bool,
                ) -> ::stratava::__core::Result<()> {
                    self.inner.set_matrix_channel_committed(#name, channel, value)
                }
            },
            MatrixMethodTarget::Trait => quote! {
                fn #get_method(&self, channel: u64) -> ::stratava::__core::Result<bool>;

                fn #set_method(
                    &mut self,
                    channel: u64,
                    value: bool,
                ) -> ::stratava::__core::Result<()>;
            },
            MatrixMethodTarget::TraitImpl => quote! {
                fn #get_method(&self, channel: u64) -> ::stratava::__core::Result<bool> {
                    self.inner.is_matrix_channel_committed(#name, channel)
                }

                fn #set_method(
                    &mut self,
                    channel: u64,
                    value: bool,
                ) -> ::stratava::__core::Result<()> {
                    self.inner.set_matrix_channel_committed(#name, channel, value)
                }
            },
        }
    });
    quote! {
        #(#category_methods)*
        #(#single_methods)*
        #(#channel_methods)*
    }
}

/// Which of the three places a reader method is emitted into.
///
/// The unkeyed/keyed accessor and both its `_into` twins are emitted three
/// times — the inherent impl, the trait declaration and the trait impl — and the
/// survey that prompted the `_into` work flagged exactly this as the way to
/// ship a half-change: miss the trait declaration and the trait route silently
/// lacks the method while the inherent route has it. One function emits all
/// three, so a method added here cannot appear in two of them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ReaderMethodTarget {
    Inherent,
    TraitDecl,
    TraitImpl,
}

pub(super) fn reader_block_methods(
    block: &InlineBlock,
    target: ReaderMethodTarget,
) -> Vec<TokenStream2> {
    let ty = &block.name;
    let plural = plural_method_ident(ty);
    let entries_into = format_ident!("{}_entries_into", plural_method_ident(ty));
    let decoded_into = format_ident!("{}_decoded_into", plural_method_ident(ty));
    let plural_into = format_ident!("{}_into", plural_method_ident(ty));
    let vis = match target {
        ReaderMethodTarget::Inherent => quote! { pub },
        ReaderMethodTarget::TraitDecl | ReaderMethodTarget::TraitImpl => quote! {},
    };

    // `(signature, body)`; a declaration takes the signature and a semicolon,
    // the other two take the signature and the body.
    let mut methods: Vec<(TokenStream2, TokenStream2)> = Vec::new();

    if block.key_fields.is_empty() {
        methods.push((
            quote! {
                #vis fn #plural(&self) -> ::stratava::__core::Result<::stratava::__core::BlockVec<#ty>>
            },
            quote! { { self.inner.blocks::<#ty>() } },
        ));
        methods.push((
            quote! {
                #vis fn #entries_into(
                    &self,
                    out: &mut ::std::vec::Vec<::stratava::__core::RecordIndexEntry>,
                ) -> ::stratava::__core::Result<()>
            },
            quote! { { self.inner.block_entries_into::<#ty>(out) } },
        ));
        methods.push((
            quote! {
                #vis fn #decoded_into(
                    &self,
                    out: &mut ::std::vec::Vec<#ty>,
                ) -> ::stratava::__core::Result<()>
            },
            quote! { { self.inner.decode_blocks_into::<#ty>(out) } },
        ));
    } else {
        methods.push((
            quote! {
                #vis fn #plural(
                    &self,
                ) -> ::stratava::__core::Result<
                    ::stratava::__core::KeyedBlockVec<
                        <#ty as ::stratava::__core::StratavaKeyedBlock>::Key,
                        #ty,
                    >
                >
            },
            quote! { { self.inner.keyed_blocks::<#ty>() } },
        ));
        methods.push((
            quote! {
                #vis fn #plural_into(
                    &self,
                    entries: &mut ::std::vec::Vec<::stratava::__core::RecordIndexEntry>,
                    by_key: &mut ::std::collections::HashMap<
                        <#ty as ::stratava::__core::StratavaKeyedBlock>::Key,
                        ::stratava::__core::RecordIndexEntry,
                    >,
                ) -> ::stratava::__core::Result<()>
            },
            quote! { { self.inner.keyed_blocks_into::<#ty>(entries, by_key) } },
        ));
        methods.push((
            quote! {
                #vis fn #entries_into(
                    &self,
                    out: &mut ::std::vec::Vec<::stratava::__core::RecordIndexEntry>,
                ) -> ::stratava::__core::Result<()>
            },
            quote! { { self.inner.block_entries_into::<#ty>(out) } },
        ));
        methods.push((
            quote! {
                #vis fn #decoded_into(
                    &self,
                    out: &mut ::std::vec::Vec<#ty>,
                ) -> ::stratava::__core::Result<()>
            },
            quote! { { self.inner.decode_blocks_into::<#ty>(out) } },
        ));
    }

    methods
        .into_iter()
        .map(|(signature, body)| match target {
            ReaderMethodTarget::TraitDecl => quote! { #signature; },
            ReaderMethodTarget::Inherent | ReaderMethodTarget::TraitImpl => {
                quote! { #signature #body }
            }
        })
        .collect()
}

pub(super) fn reader_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    reader_block_methods(block, ReaderMethodTarget::Inherent)
}

pub(super) fn reader_trait_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    reader_block_methods(block, ReaderMethodTarget::TraitDecl)
}

pub(super) fn reader_trait_impl_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    reader_block_methods(block, ReaderMethodTarget::TraitImpl)
}

pub(super) fn writer_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    let ty = &block.name;
    let push = format_ident!("push_{}", singular_method_name(ty));
    let replace = format_ident!("replace_{}", singular_method_name(ty));
    let replace_method = quote! {
        pub fn #replace(
            &mut self,
            index: usize,
            value: &#ty,
        ) -> ::stratava::__core::Result<::stratava::__core::ReplacementInfo> {
            // F-01: a successful replacement republishes the file generation
            // and moves record offsets, and the file invalidates its own
            // keyed-tail cache when it rebinds. There is nothing left to
            // translate here, so no fallible step follows the publication.
            self.inner.replace_block::<#ty>(index, value)
        }
    };
    if block.key_fields.is_empty() {
        vec![
            quote! {
                pub fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                    self.inner.push_info(value)
                }
            },
            replace_method,
        ]
    } else {
        let delete = format_ident!("delete_{}", singular_method_name(ty));
        vec![
            quote! {
                pub fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                    // F-01: the maintained keyed append. The predecessor comes
                    // from - and the new tail goes into - the byte-keyed
                    // resident cache, whose slot is charged and reserved
                    // before the append and filled afterwards by an insert
                    // that runs no user code and cannot allocate.
                    self.inner.push_keyed_info(value)
                }
            },
            quote! {
                pub fn #delete(
                    &mut self,
                    key: &<#ty as ::stratava::__core::StratavaKeyedBlock>::Key,
                ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                    // F-01: see the push path. The key is encoded once, before
                    // the tombstone, and those same canonical bytes are both
                    // the record payload and the cache key, so nothing owned
                    // by the caller's key type is cloned, hashed or compared
                    // after publication.
                    self.inner.delete_info::<#ty>(key)
                }
            },
            replace_method,
        ]
    }
}

pub(super) fn writer_trait_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    let ty = &block.name;
    let push = format_ident!("push_{}", singular_method_name(ty));
    let replace = format_ident!("replace_{}", singular_method_name(ty));
    let replace_method = quote! {
        fn #replace(
            &mut self,
            index: usize,
            value: &#ty,
        ) -> ::stratava::__core::Result<::stratava::__core::ReplacementInfo>;
    };
    if block.key_fields.is_empty() {
        vec![
            quote! {
                fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo>;
            },
            replace_method,
        ]
    } else {
        let delete = format_ident!("delete_{}", singular_method_name(ty));
        vec![
            quote! {
                fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo>;
            },
            quote! {
                fn #delete(
                    &mut self,
                    key: &<#ty as ::stratava::__core::StratavaKeyedBlock>::Key,
                ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo>;
            },
            replace_method,
        ]
    }
}

pub(super) fn writer_trait_impl_methods(block: &InlineBlock) -> Vec<TokenStream2> {
    let ty = &block.name;
    let push = format_ident!("push_{}", singular_method_name(ty));
    let replace = format_ident!("replace_{}", singular_method_name(ty));
    let replace_method = quote! {
        fn #replace(
            &mut self,
            index: usize,
            value: &#ty,
        ) -> ::stratava::__core::Result<::stratava::__core::ReplacementInfo> {
            self.#replace(index, value)
        }
    };
    if block.key_fields.is_empty() {
        vec![
            quote! {
                fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                    self.inner.push_info(value)
                }
            },
            replace_method,
        ]
    } else {
        let delete = format_ident!("delete_{}", singular_method_name(ty));
        vec![
            quote! {
                fn #push(
                    &mut self,
                    value: &#ty,
                ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                    // F-01: the trait route carries its own copy of the body,
                    // so it routes through the same single implementation as
                    // the inherent method.
                    self.inner.push_keyed_info(value)
                }
            },
            quote! {
                fn #delete(
                    &mut self,
                    key: &<#ty as ::stratava::__core::StratavaKeyedBlock>::Key,
                ) -> ::stratava::__core::Result<::stratava::__core::AppendInfo> {
                    // F-01: see the inherent `delete_<block>` method.
                    self.inner.delete_info::<#ty>(key)
                }
            },
            replace_method,
        ]
    }
}

pub(super) fn plural_method_ident(name: &Ident) -> Ident {
    format_ident!("{}", plural_method_name(name))
}

pub(super) fn plural_method_name(name: &Ident) -> String {
    let singular = singular_method_name(name);
    if let Some(prefix) = singular.strip_suffix('y') {
        format!("{prefix}ies")
    } else if singular.ends_with('s') {
        format!("{singular}es")
    } else {
        format!("{singular}s")
    }
}

pub(super) fn singular_method_name(name: &Ident) -> String {
    let raw = name.to_string();
    let mut out = String::new();
    for (index, ch) in raw.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

pub(super) fn compression_tokens(choice: CompressionChoice) -> TokenStream2 {
    match choice {
        CompressionChoice::None => quote!(::stratava::__core::CompressionPolicy::None),
        CompressionChoice::VariableBlocks(compression) => {
            let algorithm = match compression.algorithm {
                CompressionAlgorithmChoice::Zstd => {
                    quote!(::stratava::__core::CompressionAlgorithm::Zstd)
                }
            };
            let level = match compression.level {
                CompressionLevelChoice::Fast => quote!(::stratava::__core::CompressionLevel::Fast),
                CompressionLevelChoice::Default => {
                    quote!(::stratava::__core::CompressionLevel::Default)
                }
                CompressionLevelChoice::Best => quote!(::stratava::__core::CompressionLevel::Best),
                CompressionLevelChoice::Exact(level) => {
                    quote!(::stratava::__core::CompressionLevel::Exact(#level))
                }
            };
            let header = match compression.header {
                CompressionHeaderChoice::RecordExplicit => {
                    quote!(::stratava::__core::CompressionHeaderMode::RecordExplicit)
                }
                CompressionHeaderChoice::FileExplicit => {
                    quote!(::stratava::__core::CompressionHeaderMode::FileExplicit)
                }
                CompressionHeaderChoice::FormatContract => {
                    quote!(::stratava::__core::CompressionHeaderMode::FormatContract)
                }
            };
            let min_len = compression.min_len;
            let only_if_smaller = compression.only_if_smaller;
            let max_len = compression.max_len;
            quote! {
                ::stratava::__core::CompressionPolicy::VariableBlocks(
                    ::stratava::__core::VariableCompression {
                        algorithm: #algorithm,
                        level: #level,
                        header_mode: #header,
                        min_uncompressed_len: #min_len,
                        only_if_smaller: #only_if_smaller,
                        max_uncompressed_len: #max_len,
                    }
                )
            }
        }
    }
}

pub(super) fn read_limits_tokens(choice: LimitsChoice) -> TokenStream2 {
    match choice {
        LimitsChoice::TrustedUnbounded => {
            quote!(::stratava::__core::ReadLimits::trusted_unbounded())
        }
        LimitsChoice::Finite(entries) => {
            let setters = entries.into_iter().map(|entry| {
                let method = match entry.key.to_string().as_str() {
                    "file_len" => format_ident!("with_max_file_len"),
                    "records" => format_ident!("with_max_records"),
                    "index_bytes" => format_ident!("with_max_index_bytes"),
                    "scan_bytes" => format_ident!("with_max_scan_bytes"),
                    "record_payload" => format_ident!("with_max_record_payload_len"),
                    "logical_payload" => format_ident!("with_max_logical_payload_len"),
                    "materialized_bytes" => format_ident!("with_max_materialized_bytes"),
                    "segments" => format_ident!("with_max_segments"),
                    "matrix_dimension" => format_ident!("with_max_matrix_dimension"),
                    "matrix_cells" => format_ident!("with_max_matrix_cells"),
                    "matrix_bitmap" => format_ident!("with_max_matrix_bitmap_bytes"),
                    "matrix_crc" => format_ident!("with_max_matrix_crc_bytes"),
                    "matrix_metadata" => format_ident!("with_max_matrix_metadata_bytes"),
                    "matrix_slot_region" => format_ident!("with_max_matrix_slot_region_len"),
                    "sidecar" => format_ident!("with_max_sidecar_len"),
                    "mmap" => format_ident!("with_max_mmap_len"),
                    "keyed_tail" => format_ident!("with_max_keyed_tail_bytes"),
                    "header_extension" => {
                        format_ident!("with_max_file_header_extension_len")
                    }
                    _ => unreachable!("read limit keys are validated while parsing"),
                };
                let value = entry.value;
                quote!(.#method(#value))
            });
            quote! {
                ::stratava::__core::ReadLimits::missing()
                #(#setters)*
            }
        }
    }
}

pub(super) fn pairwise<T>(items: &[T]) -> Vec<(&T, &T)> {
    let mut pairs = Vec::new();
    for left in 0..items.len() {
        for right in (left + 1)..items.len() {
            pairs.push((&items[left], &items[right]));
        }
    }
    pairs
}
