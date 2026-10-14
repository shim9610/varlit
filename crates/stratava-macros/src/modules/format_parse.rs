//! format parse implementation, kept within the original parent privacy boundary.
use super::*;

pub(super) struct FormatInput {
    pub(super) vis: Visibility,
    pub(super) name: Ident,
    pub(super) magic: LitByteStr,
    pub(super) version: u16,
    pub(super) endian: EndianChoice,
    pub(super) schema_hash: SchemaHashChoice,
    pub(super) extension: Option<LitStr>,
    pub(super) index: IndexChoice,
    pub(super) commit: CommitChoice,
    pub(super) integrity: IntegrityChoice,
    pub(super) liveness: LivenessChoice,
    pub(super) header_slots: Option<HeaderSlotsChoice>,
    pub(super) recovery: RecoveryChoice,
    pub(super) manifest: ManifestChoice,
    pub(super) compression: CompressionChoice,
    pub(super) limits: LimitsChoice,
    pub(super) dims: Vec<MatrixDim>,
    pub(super) matrix_commit: Option<MatrixCommit>,
    pub(super) matrix_aux: Vec<MatrixAux>,
    pub(super) registry_blocks: Vec<Type>,
    pub(super) inline_blocks: Vec<InlineBlock>,
    pub(super) layout_preset: Option<LayoutPresetChoice>,
    pub(super) layout_file_header: Option<LayoutFileHeader>,
    pub(super) layout_segments: Vec<LayoutSegment>,
    pub(super) typed_api: bool,
}

pub(super) enum EndianChoice {
    Little,
    Big,
}

pub(super) enum SchemaHashChoice {
    Literal(u64),
    Computed,
}

#[derive(Clone)]
pub(super) struct IndexChoice {
    pub(super) scan_on_open: bool,
    pub(super) checkpoint_on_flush: bool,
    pub(super) block_offset_chain: bool,
    pub(super) keyed_offset_chain: bool,
    pub(super) header_tails: bool,
}

impl IndexChoice {
    pub(super) fn empty() -> Self {
        Self {
            scan_on_open: false,
            checkpoint_on_flush: false,
            block_offset_chain: false,
            keyed_offset_chain: false,
            header_tails: false,
        }
    }

    pub(super) fn scan_on_open() -> Self {
        Self {
            scan_on_open: true,
            ..Self::empty()
        }
    }

    pub(super) fn with_block_offset_chain(mut self) -> Self {
        self.scan_on_open = true;
        self.block_offset_chain = true;
        self
    }

    pub(super) fn with_keyed_offset_chain(mut self) -> Self {
        self.scan_on_open = true;
        self.block_offset_chain = true;
        self.keyed_offset_chain = true;
        self
    }

    /// The region hands out a block's tail offset, and the only way to use one
    /// is to walk back through `prev_same_block_offset`, so the chain comes
    /// with it.
    pub(super) fn with_header_tails(mut self) -> Self {
        self.scan_on_open = true;
        self.block_offset_chain = true;
        self.header_tails = true;
        self
    }
}

pub(super) enum CommitChoice {
    None,
    RecordFooter,
    TransactionMarkerOnFlush,
    TransactionMarkerExplicit,
}

pub(super) enum ParsedCommitChoice {
    Append(CommitChoice),
    Matrix(MatrixCommit),
}

pub(super) enum IntegrityChoice {
    None,
    Crc32,
    Crc32WithHeader,
}

pub(super) enum LivenessChoice {
    None,
    FooterFlags,
}

/// `header_slots { capacity: N; integrity: ...; blocks: [..]; }`, parsed.
///
/// Held as the block *types* rather than ids: the DSL names blocks the way the
/// rest of the format does, and the id is read off the type in the generated
/// spec, so a renamed or renumbered block cannot drift out of this list.
pub(super) struct HeaderSlotsChoice {
    pub(super) capacity: u32,
    pub(super) integrity: HeaderSlotIntegrityChoice,
    pub(super) blocks: Vec<Type>,
}

pub(super) enum HeaderSlotIntegrityChoice {
    None,
    Rolling,
    Sealed,
}

pub(super) enum RecoveryChoice {
    Strict,
    TruncateTail,
    MarkTail,
}

pub(super) enum ManifestChoice {
    None,
    Embedded,
}

pub(super) enum CompressionChoice {
    None,
    VariableBlocks(VariableCompressionChoice),
}

pub(super) enum LimitsChoice {
    Finite(Vec<LimitEntry>),
    TrustedUnbounded,
}

pub(super) struct LimitEntry {
    pub(super) key: Ident,
    pub(super) value: u64,
}

pub(super) struct VariableCompressionChoice {
    pub(super) algorithm: CompressionAlgorithmChoice,
    pub(super) level: CompressionLevelChoice,
    pub(super) header: CompressionHeaderChoice,
    pub(super) min_len: u64,
    pub(super) only_if_smaller: bool,
    pub(super) max_len: u64,
}

pub(super) enum CompressionAlgorithmChoice {
    Zstd,
}

pub(super) enum CompressionLevelChoice {
    Fast,
    Default,
    Best,
    Exact(i32),
}

pub(super) enum CompressionHeaderChoice {
    RecordExplicit,
    FileExplicit,
    FormatContract,
}

#[derive(Clone, Copy)]
pub(super) enum LayoutPresetChoice {
    StratavaNative,
    None,
    Custom,
}

pub(super) struct LayoutFileHeader {
    pub(super) name: Ident,
    pub(super) fields: Vec<LayoutField>,
}

pub(super) struct LayoutSegment {
    pub(super) name: Ident,
    pub(super) repeat: SegmentRepeatChoice,
    pub(super) lead_in: LayoutLeadIn,
    pub(super) metadata: Ident,
    pub(super) raw_region: Ident,
    pub(super) footer: Option<LayoutFooter>,
}

#[derive(Clone, Copy)]
pub(super) enum SegmentRepeatChoice {
    Once,
    UntilEof,
}

pub(super) struct LayoutLeadIn {
    pub(super) name: Ident,
    pub(super) fields: Vec<LayoutField>,
}

pub(super) struct LayoutFooter {
    pub(super) name: Ident,
    pub(super) fields: Vec<LayoutField>,
}

pub(super) struct LayoutField {
    pub(super) name: Ident,
    pub(super) ty: LayoutFieldTypeChoice,
    pub(super) source: LayoutFieldSourceChoice,
}

pub(super) enum LayoutFieldTypeChoice {
    Bytes(u64),
    U8,
    U16,
    U32,
    U64,
    I64,
}

pub(super) enum LayoutFieldSourceChoice {
    LiteralBytes(LitByteStr),
    LiteralU64(u64),
    LiteralI64(i64),
    Caller,
    Finalize(LayoutFinalizeChoice),
}

pub(super) struct LayoutFinalizeChoice {
    pub(super) target: LayoutAnchorChoice,
    pub(super) relative_to: LayoutAnchorChoice,
}

#[derive(Clone, Copy)]
pub(super) enum LayoutAnchorChoice {
    SegmentStart,
    AfterLeadIn,
    MetadataStart,
    RawRegionStart,
    SegmentEnd,
    FooterStart,
    FooterEnd,
}

pub(super) enum InlineBlockKind {
    Fixed,
    Variable,
    Matrix(MatrixBlockMeta),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyIndexChoice {
    Memory,
    Disk,
}

pub(super) struct InlineBlock {
    pub(super) kind: InlineBlockKind,
    pub(super) name: Ident,
    pub(super) id: u32,
    pub(super) version: u16,
    pub(super) key_fields: Vec<Ident>,
    pub(super) finite_key: Option<finite_key::FiniteKey>,
    pub(super) key_index: KeyIndexChoice,
    pub(super) durability: Option<LitStr>,
    pub(super) immediate_if: Option<LitStr>,
    pub(super) fields: Vec<InlineField>,
}

pub(super) struct MatrixDim {
    pub(super) name: Ident,
    pub(super) ty: Type,
}

pub(super) struct MatrixCommit {
    pub(super) keyspace: Vec<Ident>,
    pub(super) categories: Vec<Ident>,
    pub(super) singles: Vec<Ident>,
    pub(super) per_channel: Vec<Ident>,
}

pub(super) struct MatrixAux {
    pub(super) name: Ident,
    pub(super) byte_len: u64,
}

pub(super) struct MatrixBlockMeta {
    pub(super) dims: Vec<Ident>,
    pub(super) category: Ident,
}

pub(super) struct InlineField {
    pub(super) name: Ident,
    pub(super) ty: Type,
    pub(super) field_id: u32,
    pub(super) default: bool,
}

impl Parse for FormatInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let vis: Visibility = input.parse()?;
        let typed_api = if input.peek(Token![struct]) {
            input.parse::<Token![struct]>()?;
            false
        } else {
            let keyword: Ident = input.parse()?;
            if keyword != "format" {
                return Err(syn::Error::new_spanned(
                    keyword,
                    "expected struct or format",
                ));
            }
            true
        };
        let name: Ident = input.parse()?;
        let content;
        braced!(content in input);

        let mut magic = None;
        let mut version = None;
        let mut endian = EndianChoice::Little;
        let mut schema_hash = SchemaHashChoice::Literal(0);
        let mut extension = None;
        let mut index = IndexChoice::scan_on_open();
        let mut commit = CommitChoice::None;
        let mut integrity = IntegrityChoice::None;
        let mut liveness = LivenessChoice::None;
        let mut header_slots: Option<HeaderSlotsChoice> = None;
        let mut recovery = RecoveryChoice::Strict;
        let mut manifest = ManifestChoice::None;
        let mut compression = CompressionChoice::None;
        let mut limits = None;
        let mut dims: Option<Vec<MatrixDim>> = None;
        let mut matrix_commit = None;
        let mut matrix_aux: Option<Vec<MatrixAux>> = None;
        let mut registry_blocks: Option<Vec<Type>> = None;
        let mut inline_blocks: Option<Vec<InlineBlock>> = None;
        let mut layout_preset = None;
        let mut layout_file_header = None;
        let mut layout_segments: Option<Vec<LayoutSegment>> = None;
        let mut seen_keys = Vec::new();

        while !content.is_empty() {
            let key: Ident = content.parse()?;
            if key == "limits" && content.peek(syn::token::Brace) {
                note_format_key(&mut seen_keys, &key)?;
                let inner;
                braced!(inner in content);
                limits = Some(LimitsChoice::Finite(parse_read_limits(&inner)?));
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            if key == "dims" && content.peek(syn::token::Brace) {
                note_format_key(&mut seen_keys, &key)?;
                let inner;
                braced!(inner in content);
                dims = Some(parse_matrix_dims(&inner)?);
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            if key == "aux" && content.peek(syn::token::Brace) {
                note_format_key(&mut seen_keys, &key)?;
                let inner;
                braced!(inner in content);
                matrix_aux = Some(parse_matrix_aux(&inner)?);
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            if key == "blocks" && content.peek(syn::token::Brace) {
                note_format_key(&mut seen_keys, &key)?;
                let inner;
                braced!(inner in content);
                inline_blocks = Some(parse_inline_blocks(&inner)?);
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            if key == "header_slots" && content.peek(syn::token::Brace) {
                note_format_key(&mut seen_keys, &key)?;
                let inner;
                braced!(inner in content);
                header_slots = Some(parse_header_slots(&inner)?);
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            if key == "layout" && content.peek(syn::token::Brace) {
                note_format_key(&mut seen_keys, &key)?;
                let inner;
                braced!(inner in content);
                let layout = parse_layout(&inner)?;
                layout_file_header = layout.file_header;
                layout_segments = Some(layout.segments);
                if content.peek(Token![;]) {
                    content.parse::<Token![;]>()?;
                }
                continue;
            }
            content.parse::<Token![:]>()?;
            note_format_key(&mut seen_keys, &key)?;
            if key == "magic" {
                magic = Some(content.parse()?);
            } else if key == "version" {
                let value: LitInt = content.parse()?;
                version = Some(value.base10_parse::<u16>()?);
            } else if key == "endian" {
                let value: Ident = content.parse()?;
                endian = match value.to_string().as_str() {
                    "little" => EndianChoice::Little,
                    "big" => EndianChoice::Big,
                    _ => return Err(syn::Error::new_spanned(value, "expected little or big")),
                };
            } else if key == "schema_hash" {
                if content.peek(LitInt) {
                    let value: LitInt = content.parse()?;
                    schema_hash = SchemaHashChoice::Literal(value.base10_parse::<u64>()?);
                } else {
                    let value: Ident = content.parse()?;
                    if value == "computed" {
                        schema_hash = SchemaHashChoice::Computed;
                    } else {
                        return Err(syn::Error::new_spanned(
                            value,
                            "expected computed or a u64 schema hash",
                        ));
                    }
                }
            } else if key == "extension" {
                extension = Some(content.parse()?);
            } else if key == "index" {
                index = parse_index_choice(&content)?;
            } else if key == "commit" {
                match parse_commit_choice(&content)? {
                    ParsedCommitChoice::Append(choice) => commit = choice,
                    ParsedCommitChoice::Matrix(choice) => matrix_commit = Some(choice),
                }
            } else if key == "integrity" {
                let value: Ident = content.parse()?;
                integrity = match value.to_string().as_str() {
                    "none" => IntegrityChoice::None,
                    "crc32" => IntegrityChoice::Crc32,
                    "crc32_with_header" => IntegrityChoice::Crc32WithHeader,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "expected none, crc32, or crc32_with_header",
                        ));
                    }
                };
            } else if key == "liveness" {
                let value: Ident = content.parse()?;
                liveness = match value.to_string().as_str() {
                    "none" => LivenessChoice::None,
                    "footer_flags" => LivenessChoice::FooterFlags,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "expected none or footer_flags",
                        ));
                    }
                };
            } else if key == "recovery" {
                let value: Ident = content.parse()?;
                recovery = match value.to_string().as_str() {
                    "strict" => RecoveryChoice::Strict,
                    "truncate_tail" => RecoveryChoice::TruncateTail,
                    "mark_tail" => RecoveryChoice::MarkTail,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "expected strict, truncate_tail, or mark_tail",
                        ));
                    }
                };
            } else if key == "manifest" {
                let value: Ident = content.parse()?;
                manifest = match value.to_string().as_str() {
                    "none" => ManifestChoice::None,
                    "embedded" => ManifestChoice::Embedded,
                    _ => return Err(syn::Error::new_spanned(value, "expected none or embedded")),
                };
            } else if key == "compression" {
                compression = parse_compression_choice(&content)?;
            } else if key == "limits" {
                let value: Ident = content.parse()?;
                if value != "trusted_unbounded" {
                    return Err(syn::Error::new_spanned(
                        value,
                        "expected trusted_unbounded or limits { ... }",
                    ));
                }
                limits = Some(LimitsChoice::TrustedUnbounded);
            } else if key == "preset" {
                let value: Ident = content.parse()?;
                layout_preset = Some(match value.to_string().as_str() {
                    "stratava_native" => LayoutPresetChoice::StratavaNative,
                    "none" => LayoutPresetChoice::None,
                    "custom" => LayoutPresetChoice::Custom,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "expected stratava_native, none, or custom",
                        ));
                    }
                });
            } else if key == "blocks" {
                let inner;
                bracketed!(inner in content);
                let parsed = Punctuated::<Type, Token![,]>::parse_terminated(&inner)?;
                registry_blocks = Some(parsed.into_iter().collect());
            } else if key == "dims" {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected dims { name: type, ... }",
                ));
            } else if key == "aux" {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected aux { name: byte_len, ... }",
                ));
            } else if key == "layout" {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected layout { file_header ... segment ... }",
                ));
            } else {
                return Err(syn::Error::new_spanned(
                    key,
                    "unsupported stratava_format key",
                ));
            }
            content.parse::<Token![;]>()?;
        }

        let dims = dims.unwrap_or_default();
        let matrix_aux = matrix_aux.unwrap_or_default();
        let registry_blocks = registry_blocks.unwrap_or_default();
        let inline_blocks = inline_blocks.unwrap_or_default();
        if inline_blocks.iter().any(|block| block.finite_key.is_some())
            && !matches!(schema_hash, SchemaHashChoice::Computed)
        {
            return Err(content.error("finite keys require schema_hash: computed so value/code changes cannot silently reinterpret a file"));
        }
        let layout_segments = layout_segments.unwrap_or_default();
        let limits = limits.unwrap_or_else(|| LimitsChoice::Finite(Vec::new()));
        if typed_api && inline_blocks.is_empty() && layout_segments.is_empty() {
            return Err(content.error("format syntax requires inline blocks"));
        }
        if !typed_api && registry_blocks.is_empty() {
            return Err(content.error("missing blocks"));
        }
        validate_matrix_format(&dims, matrix_commit.as_ref(), &matrix_aux, &inline_blocks)?;

        Ok(Self {
            vis,
            name,
            magic: magic.ok_or_else(|| content.error("missing magic"))?,
            version: version.ok_or_else(|| content.error("missing version"))?,
            endian,
            schema_hash,
            extension,
            index,
            commit,
            integrity,
            liveness,
            header_slots,
            recovery,
            manifest,
            compression,
            limits,
            dims,
            matrix_commit,
            matrix_aux,
            registry_blocks,
            inline_blocks,
            layout_preset,
            layout_file_header,
            layout_segments,
            typed_api,
        })
    }
}

pub(super) const READ_LIMIT_KEYS: &[&str] = &[
    "file_len",
    "records",
    "index_bytes",
    "scan_bytes",
    "record_payload",
    "logical_payload",
    "materialized_bytes",
    "segments",
    "matrix_dimension",
    "matrix_cells",
    "matrix_bitmap",
    "matrix_crc",
    "matrix_metadata",
    "matrix_slot_region",
    "sidecar",
    "mmap",
    "keyed_tail",
    "header_extension",
];

pub(super) fn parse_read_limits(input: ParseStream<'_>) -> Result<Vec<LimitEntry>> {
    let mut entries = Vec::new();
    while !input.is_empty() {
        let key: Ident = input.parse()?;
        let key_text = key.to_string();
        if !READ_LIMIT_KEYS.contains(&key_text.as_str()) {
            return Err(syn::Error::new_spanned(
                key,
                "unknown Stratava read limit key",
            ));
        }
        if entries.iter().any(|entry: &LimitEntry| entry.key == key) {
            return Err(syn::Error::new_spanned(
                key,
                format!("duplicate Stratava read limit key {key_text:?}"),
            ));
        }
        input.parse::<Token![:]>()?;
        let value: LitInt = input.parse()?;
        entries.push(LimitEntry {
            key,
            value: value.base10_parse::<u64>()?,
        });
        input.parse::<Token![;]>()?;
    }

    Ok(entries)
}

pub(super) fn note_format_key(seen: &mut Vec<String>, key: &Ident) -> Result<()> {
    let key_text = key.to_string();
    if seen.iter().any(|seen| seen == &key_text) {
        return Err(syn::Error::new_spanned(
            key,
            format!("duplicate stratava_format key {key_text:?}"),
        ));
    }
    seen.push(key_text);
    Ok(())
}

pub(super) fn parse_index_choice(input: ParseStream<'_>) -> Result<IndexChoice> {
    if input.peek(syn::token::Bracket) {
        let inner;
        bracketed!(inner in input);
        let mut choice = IndexChoice::empty();
        let mut seen_any = false;
        while !inner.is_empty() {
            choice = parse_index_entry(&inner, choice)?;
            seen_any = true;
            if inner.is_empty() {
                break;
            }
            inner.parse::<Token![,]>()?;
        }
        if !seen_any {
            return Err(inner.error("index list must not be empty"));
        }
        return Ok(choice);
    }
    parse_index_entry(input, IndexChoice::empty())
}

/// One entry of the `index: [...]` list.
///
/// Entries are bare idents (`scan_on_open`) or call-shaped
/// (`offset_sidecar(per_block)`); the list previously parsed as
/// `Punctuated<Ident, Comma>`, which cannot express the second form.
pub(super) fn parse_index_entry(
    input: ParseStream<'_>,
    choice: IndexChoice,
) -> Result<IndexChoice> {
    let value: Ident = input.parse()?;
    apply_index_ident(choice, value)
}

pub(super) fn apply_index_ident(mut choice: IndexChoice, value: Ident) -> Result<IndexChoice> {
    match value.to_string().as_str() {
        "scan_on_open" => {
            choice.scan_on_open = true;
            Ok(choice)
        }
        "checkpoint_on_flush" => {
            choice.scan_on_open = true;
            choice.checkpoint_on_flush = true;
            Ok(choice)
        }
        "block_offset_chain" => Ok(choice.with_block_offset_chain()),
        "keyed_offset_chain" => Ok(choice.with_keyed_offset_chain()),
        "header_tails" => Ok(choice.with_header_tails()),
        _ => Err(syn::Error::new_spanned(
            value,
            "expected scan_on_open, checkpoint_on_flush, block_offset_chain, keyed_offset_chain, or header_tails",
        )),
    }
}

pub(super) fn parse_commit_choice(input: ParseStream<'_>) -> Result<ParsedCommitChoice> {
    let value: Ident = input.parse()?;
    match value.to_string().as_str() {
        "none" => Ok(ParsedCommitChoice::Append(CommitChoice::None)),
        "record_footer" => Ok(ParsedCommitChoice::Append(CommitChoice::RecordFooter)),
        "transaction_marker" => {
            let args;
            parenthesized!(args in input);
            let mode: Ident = args.parse()?;
            if !args.is_empty() {
                return Err(args.error("transaction_marker accepts exactly one mode"));
            }
            match mode.to_string().as_str() {
                "on_flush" => Ok(ParsedCommitChoice::Append(
                    CommitChoice::TransactionMarkerOnFlush,
                )),
                "explicit" => Ok(ParsedCommitChoice::Append(
                    CommitChoice::TransactionMarkerExplicit,
                )),
                _ => Err(syn::Error::new_spanned(
                    mode,
                    "expected on_flush or explicit",
                )),
            }
        }
        "cell_bitmap" => {
            let body;
            braced!(body in input);
            parse_matrix_commit(&body).map(ParsedCommitChoice::Matrix)
        }
        _ => Err(syn::Error::new_spanned(
            value,
            "expected none, record_footer, transaction_marker(...), or cell_bitmap { ... }",
        )),
    }
}

pub(super) struct LayoutDecl {
    pub(super) file_header: Option<LayoutFileHeader>,
    pub(super) segments: Vec<LayoutSegment>,
}

/// `header_slots { capacity: N; integrity: none|rolling|sealed; blocks: [..]; }`
///
/// `capacity` and `blocks` are both required, and neither has a defensible
/// default: a region with no declared blocks can hold nothing, and a capacity
/// picked by the macro would be a size the caller has to discover from a
/// failure rather than choose. `integrity` defaults to `none`, which is the
/// setting that costs nothing.
pub(super) fn parse_header_slots(input: ParseStream<'_>) -> Result<HeaderSlotsChoice> {
    let mut capacity: Option<u32> = None;
    let mut integrity = HeaderSlotIntegrityChoice::None;
    let mut blocks: Option<Vec<Type>> = None;

    while !input.is_empty() {
        let key: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        if key == "capacity" {
            let value: LitInt = input.parse()?;
            capacity = Some(value.base10_parse()?);
        } else if key == "integrity" {
            let value: Ident = input.parse()?;
            integrity = match value.to_string().as_str() {
                "none" => HeaderSlotIntegrityChoice::None,
                "rolling" => HeaderSlotIntegrityChoice::Rolling,
                "sealed" => HeaderSlotIntegrityChoice::Sealed,
                _ => {
                    return Err(syn::Error::new_spanned(
                        value,
                        "expected none, rolling, or sealed",
                    ));
                }
            };
        } else if key == "blocks" {
            let inner;
            bracketed!(inner in input);
            let parsed = Punctuated::<Type, Token![,]>::parse_terminated(&inner)?;
            blocks = Some(parsed.into_iter().collect());
        } else {
            return Err(syn::Error::new_spanned(
                key,
                "expected capacity, integrity, or blocks",
            ));
        }
        if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
        }
    }

    let capacity = capacity
        .ok_or_else(|| syn::Error::new(input.span(), "header_slots requires a capacity"))?;
    let blocks = blocks
        .ok_or_else(|| syn::Error::new(input.span(), "header_slots requires a blocks list"))?;
    Ok(HeaderSlotsChoice {
        capacity,
        integrity,
        blocks,
    })
}

pub(super) fn parse_layout(input: ParseStream<'_>) -> Result<LayoutDecl> {
    let mut file_header = None;
    let mut segments = Vec::new();
    while !input.is_empty() {
        let keyword: Ident = input.parse()?;
        match keyword.to_string().as_str() {
            "file_header" => {
                if file_header.is_some() {
                    return Err(syn::Error::new_spanned(
                        keyword,
                        "layout can declare at most one file_header",
                    ));
                }
                let name: Ident = input.parse()?;
                let inner;
                braced!(inner in input);
                file_header = Some(LayoutFileHeader {
                    name,
                    fields: parse_layout_fields(&inner)?,
                });
            }
            "segment" => {
                let name: Ident = input.parse()?;
                let mut repeat = SegmentRepeatChoice::Once;
                if input.peek(Ident) {
                    let repeat_keyword: Ident = input.parse()?;
                    if repeat_keyword != "repeat" {
                        return Err(syn::Error::new_spanned(repeat_keyword, "expected repeat"));
                    }
                    let repeat_value: Ident = input.parse()?;
                    repeat = match repeat_value.to_string().as_str() {
                        "once" => SegmentRepeatChoice::Once,
                        "until_eof" => SegmentRepeatChoice::UntilEof,
                        _ => {
                            return Err(syn::Error::new_spanned(
                                repeat_value,
                                "expected once or until_eof",
                            ));
                        }
                    };
                }
                let inner;
                braced!(inner in input);
                segments.push(parse_layout_segment_body(name, repeat, &inner)?);
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    keyword,
                    "expected file_header or segment",
                ));
            }
        }
        if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
        }
    }
    Ok(LayoutDecl {
        file_header,
        segments,
    })
}

pub(super) fn parse_layout_segment_body(
    name: Ident,
    repeat: SegmentRepeatChoice,
    input: ParseStream<'_>,
) -> Result<LayoutSegment> {
    let mut lead_in = None;
    let mut metadata = None;
    let mut raw_region = None;
    let mut footer = None;
    while !input.is_empty() {
        let keyword: Ident = input.parse()?;
        match keyword.to_string().as_str() {
            "lead_in" => {
                let lead_name: Ident = input.parse()?;
                let inner;
                braced!(inner in input);
                lead_in = Some(LayoutLeadIn {
                    name: lead_name,
                    fields: parse_layout_fields(&inner)?,
                });
            }
            "metadata" => {
                metadata = Some(input.parse()?);
                input.parse::<Token![;]>()?;
            }
            "raw_region" => {
                raw_region = Some(input.parse()?);
                input.parse::<Token![;]>()?;
            }
            "footer" => {
                let footer_name: Ident = input.parse()?;
                let inner;
                braced!(inner in input);
                footer = Some(LayoutFooter {
                    name: footer_name,
                    fields: parse_layout_fields(&inner)?,
                });
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    keyword,
                    "expected lead_in, metadata, raw_region, or footer",
                ));
            }
        }
    }
    Ok(LayoutSegment {
        name,
        repeat,
        lead_in: lead_in.ok_or_else(|| input.error("layout segment requires lead_in"))?,
        metadata: metadata.ok_or_else(|| input.error("layout segment requires metadata"))?,
        raw_region: raw_region.ok_or_else(|| input.error("layout segment requires raw_region"))?,
        footer,
    })
}

pub(super) fn parse_layout_fields(input: ParseStream<'_>) -> Result<Vec<LayoutField>> {
    let mut fields = Vec::new();
    while !input.is_empty() {
        let ty_ident: Ident = input.parse()?;
        let name: Ident = input.parse()?;
        let mut source = LayoutFieldSourceChoice::Caller;
        let ty = match ty_ident.to_string().as_str() {
            "bytes" => {
                input.parse::<Token![=]>()?;
                let bytes: LitByteStr = input.parse()?;
                let len = bytes.value().len() as u64;
                source = LayoutFieldSourceChoice::LiteralBytes(bytes);
                LayoutFieldTypeChoice::Bytes(len)
            }
            "u8" => LayoutFieldTypeChoice::U8,
            "u16" => LayoutFieldTypeChoice::U16,
            "u32" => LayoutFieldTypeChoice::U32,
            "u64" => LayoutFieldTypeChoice::U64,
            "i64" => LayoutFieldTypeChoice::I64,
            _ => {
                return Err(syn::Error::new_spanned(
                    ty_ident,
                    "expected bytes, u8, u16, u32, u64, or i64",
                ));
            }
        };
        if !matches!(ty, LayoutFieldTypeChoice::Bytes(_)) && input.peek(Token![=]) {
            input.parse::<Token![=]>()?;
            if input.peek(Ident) {
                source = LayoutFieldSourceChoice::Finalize(parse_layout_finalize(input)?);
            } else {
                let value: LitInt = input.parse()?;
                source = match ty {
                    LayoutFieldTypeChoice::I64 => {
                        LayoutFieldSourceChoice::LiteralI64(value.base10_parse::<i64>()?)
                    }
                    _ => LayoutFieldSourceChoice::LiteralU64(value.base10_parse::<u64>()?),
                };
            }
        }
        input.parse::<Token![;]>()?;
        fields.push(LayoutField { name, ty, source });
    }
    Ok(fields)
}

pub(super) fn parse_layout_finalize(input: ParseStream<'_>) -> Result<LayoutFinalizeChoice> {
    let function: Ident = input.parse()?;
    if function != "finalize" {
        return Err(syn::Error::new_spanned(function, "expected finalize"));
    }
    let args;
    parenthesized!(args in input);
    let mut target = None;
    let mut relative_to = None;
    while !args.is_empty() {
        let key: Ident = args.parse()?;
        args.parse::<Token![=]>()?;
        let value: Ident = args.parse()?;
        match key.to_string().as_str() {
            "target" => target = Some(parse_layout_anchor(value)?),
            "relative_to" => relative_to = Some(parse_layout_anchor(value)?),
            _ => {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected target or relative_to",
                ));
            }
        }
        if args.peek(Token![,]) {
            args.parse::<Token![,]>()?;
        }
    }
    Ok(LayoutFinalizeChoice {
        target: target.ok_or_else(|| args.error("finalize requires target"))?,
        relative_to: relative_to.ok_or_else(|| args.error("finalize requires relative_to"))?,
    })
}

pub(super) fn parse_layout_anchor(value: Ident) -> Result<LayoutAnchorChoice> {
    Ok(match value.to_string().as_str() {
        "segment_start" => LayoutAnchorChoice::SegmentStart,
        "after_lead_in" => LayoutAnchorChoice::AfterLeadIn,
        "metadata_start" => LayoutAnchorChoice::MetadataStart,
        "raw_region_start" => LayoutAnchorChoice::RawRegionStart,
        "segment_end" => LayoutAnchorChoice::SegmentEnd,
        "footer_start" => LayoutAnchorChoice::FooterStart,
        "footer_end" => LayoutAnchorChoice::FooterEnd,
        _ => {
            return Err(syn::Error::new_spanned(
                value,
                "expected a known layout anchor",
            ));
        }
    })
}

pub(super) fn parse_matrix_dims(input: ParseStream<'_>) -> Result<Vec<MatrixDim>> {
    let mut dims = Vec::new();
    while !input.is_empty() {
        let name: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let ty: Type = input.parse()?;
        dims.push(MatrixDim { name, ty });
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
    }
    Ok(dims)
}

pub(super) fn parse_matrix_commit(input: ParseStream<'_>) -> Result<MatrixCommit> {
    let mut keyspace = None;
    let mut categories = None;
    let mut singles = None;
    let mut per_channel = None;
    while !input.is_empty() {
        let key: Ident = input.parse()?;
        input.parse::<Token![=]>()?;
        let inner;
        bracketed!(inner in input);
        let parsed = Punctuated::<Ident, Token![,]>::parse_terminated(&inner)?;
        let values: Vec<Ident> = parsed.into_iter().collect();
        match key.to_string().as_str() {
            "keyspace" => keyspace = Some(values),
            "categories" => categories = Some(values),
            "singles" => singles = Some(values),
            "per_channel" => per_channel = Some(values),
            _ => {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected keyspace, categories, singles, or per_channel",
                ));
            }
        }
        if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
        } else if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
    }
    Ok(MatrixCommit {
        keyspace: keyspace.unwrap_or_default(),
        categories: categories.unwrap_or_default(),
        singles: singles.unwrap_or_default(),
        per_channel: per_channel.unwrap_or_default(),
    })
}

pub(super) fn parse_matrix_aux(input: ParseStream<'_>) -> Result<Vec<MatrixAux>> {
    let mut aux = Vec::new();
    while !input.is_empty() {
        let name: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let len: LitInt = input.parse()?;
        aux.push(MatrixAux {
            name,
            byte_len: len.base10_parse::<u64>()?,
        });
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
    }
    Ok(aux)
}

pub(super) fn parse_inline_blocks(input: ParseStream<'_>) -> Result<Vec<InlineBlock>> {
    let mut blocks = Vec::new();
    while !input.is_empty() {
        blocks.push(parse_inline_block(input)?);
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        } else if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
        }
    }
    Ok(blocks)
}

pub(super) fn parse_inline_block(input: ParseStream<'_>) -> Result<InlineBlock> {
    let kind_ident: Ident = input.parse()?;
    let kind_name = kind_ident.to_string();
    let mut matrix_dims = Vec::new();
    let mut matrix_category = None;
    let kind = match kind_name.as_str() {
        "fixed" => InlineBlockKind::Fixed,
        "variable" => InlineBlockKind::Variable,
        "matrix" => InlineBlockKind::Matrix(MatrixBlockMeta {
            dims: Vec::new(),
            category: Ident::new("__stratava_missing_category", kind_ident.span()),
        }),
        _ => {
            return Err(syn::Error::new_spanned(
                kind_ident,
                "expected fixed, variable, or matrix",
            ));
        }
    };
    let name: Ident = input.parse()?;
    let meta;
    parenthesized!(meta in input);
    let mut id = None;
    let mut version = 1u16;
    let mut key_fields = Vec::new();
    let mut key_index = KeyIndexChoice::Memory;
    let mut key_index_span = None;
    let mut finite_key_decl = None;
    let mut durability = None;
    let mut immediate_if = None;
    while !meta.is_empty() {
        let key: Ident = meta.parse()?;
        meta.parse::<Token![=]>()?;
        match key.to_string().as_str() {
            "id" => {
                let value: LitInt = meta.parse()?;
                id = Some(value.base10_parse::<u32>()?);
            }
            "version" => {
                let value: LitInt = meta.parse()?;
                version = value.base10_parse::<u16>()?;
            }
            "key" => {
                let inner;
                bracketed!(inner in meta);
                let parsed = Punctuated::<Ident, Token![,]>::parse_terminated(&inner)?;
                if parsed.is_empty() {
                    return Err(syn::Error::new_spanned(key, "key list must not be empty"));
                }
                key_fields = parsed.into_iter().collect();
            }
            "key_values" | "key_domain" => {
                if finite_key_decl.is_some() {
                    return Err(syn::Error::new_spanned(
                        key,
                        "declare key_values or key_domain exactly once",
                    ));
                }
                finite_key_decl = Some(if key == "key_values" {
                    finite_key::parse_values(&meta)?
                } else {
                    finite_key::parse_domain(&meta)?
                });
            }
            "durability" => {
                if durability.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate durability"));
                }
                let value: Ident = meta.parse()?;
                if value != "immediate" && value != "deferred" {
                    return Err(syn::Error::new_spanned(
                        value,
                        "expected immediate or deferred",
                    ));
                }
                durability = Some(LitStr::new(&value.to_string(), value.span()));
            }
            "immediate_if" => {
                if immediate_if.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate immediate_if"));
                }
                let value: syn::Path = meta.parse()?;
                immediate_if = Some(LitStr::new(&quote!(#value).to_string(), key.span()));
            }
            "key_index" => {
                if key_index_span.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate key_index"));
                }
                let value: Ident = meta.parse()?;
                key_index = match value.to_string().as_str() {
                    "memory" => KeyIndexChoice::Memory,
                    "disk" => KeyIndexChoice::Disk,
                    _ => {
                        return Err(syn::Error::new_spanned(value, "expected memory or disk"));
                    }
                };
                key_index_span = Some(key.span());
            }
            "dims" => {
                let inner;
                bracketed!(inner in meta);
                let parsed = Punctuated::<Ident, Token![,]>::parse_terminated(&inner)?;
                if parsed.is_empty() {
                    return Err(syn::Error::new_spanned(key, "dims list must not be empty"));
                }
                matrix_dims = parsed.into_iter().collect();
            }
            "category" => {
                matrix_category = Some(meta.parse()?);
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected id, version, key, key_values, key_domain, key_index, durability, immediate_if, dims, or category",
                ));
            }
        }
        if meta.peek(Token![,]) {
            meta.parse::<Token![,]>()?;
        }
    }
    let kind = if matches!(&kind, InlineBlockKind::Matrix(_)) {
        if let Some(span) = key_index_span {
            return Err(syn::Error::new(
                span,
                "key_index is not supported on matrix blocks",
            ));
        }
        if !key_fields.is_empty() {
            return Err(syn::Error::new_spanned(
                &name,
                "matrix blocks use dims for their keyspace, not key",
            ));
        }
        InlineBlockKind::Matrix(MatrixBlockMeta {
            dims: matrix_dims,
            category: matrix_category
                .ok_or_else(|| syn::Error::new_spanned(&name, "missing matrix category"))?,
        })
    } else {
        if !matrix_dims.is_empty() {
            return Err(syn::Error::new_spanned(
                &name,
                "dims is only supported on matrix blocks",
            ));
        }
        if matrix_category.is_some() {
            return Err(syn::Error::new_spanned(
                &name,
                "category is only supported on matrix blocks",
            ));
        }
        kind
    };
    if let Some(span) = key_index_span
        && key_fields.is_empty()
    {
        return Err(syn::Error::new(
            span,
            "key_index requires a keyed fixed or variable block",
        ));
    }
    let body;
    braced!(body in input);
    let mut fields = Vec::new();
    while !body.is_empty() {
        let name: Ident = body.parse()?;
        body.parse::<Token![:]>()?;
        let ty: Type = body.parse()?;
        let mut default = false;
        if body.peek(Token![=]) {
            body.parse::<Token![=]>()?;
            let value: Ident = body.parse()?;
            if value != "default" {
                return Err(syn::Error::new_spanned(value, "expected default"));
            }
            default = true;
        }
        if matches!(&kind, InlineBlockKind::Fixed | InlineBlockKind::Matrix(_)) && default {
            return Err(syn::Error::new_spanned(
                &name,
                "field defaults are only supported on variable blocks",
            ));
        }
        let field_id = fields.len() as u32 + 1;
        fields.push(InlineField {
            name,
            ty,
            field_id,
            default,
        });
        if body.peek(Token![,]) {
            body.parse::<Token![,]>()?;
        }
    }
    let id = id.ok_or_else(|| syn::Error::new_spanned(&name, "missing block id"))?;
    if id >= 0xFFFF_FF00 {
        return Err(syn::Error::new_spanned(
            &name,
            "stratava block id is reserved; use an id below 0xFFFF_FF00",
        ));
    }
    if version == 0 {
        return Err(syn::Error::new_spanned(
            &name,
            "block version must be non-zero",
        ));
    }
    for (index, key) in key_fields.iter().enumerate() {
        if key_fields[(index + 1)..].iter().any(|other| other == key) {
            return Err(syn::Error::new_spanned(key, "duplicate key field"));
        }
        if !fields.iter().any(|field| field.name == *key) {
            return Err(syn::Error::new_spanned(key, "key field does not exist"));
        }
    }
    if let InlineBlockKind::Matrix(meta) = &kind {
        for (index, dim) in meta.dims.iter().enumerate() {
            if meta.dims[(index + 1)..].iter().any(|other| other == dim) {
                return Err(syn::Error::new_spanned(dim, "duplicate matrix dimension"));
            }
        }
    }
    let finite_key = if let Some(decl) = finite_key_decl {
        let finite = finite_key::FiniteKey::build(decl, &key_fields, &fields, name.span())?;
        if fields
            .iter()
            .any(|field| field.name == "key" && !key_fields.contains(&field.name))
        {
            return Err(syn::Error::new_spanned(
                &name,
                "finite keys reserve the generated field name `key`",
            ));
        }
        let position = fields
            .iter()
            .position(|field| key_fields.contains(&field.name))
            .unwrap();
        let field_id = fields[position].field_id;
        fields.retain(|field| !key_fields.contains(&field.name));
        let enum_name = format_ident!("{name}Key");
        let key_name = Ident::new("key", name.span());
        fields.insert(
            position,
            InlineField {
                name: key_name.clone(),
                ty: syn::parse_quote!(#enum_name),
                field_id,
                default: false,
            },
        );
        key_fields = vec![key_name];
        Some(finite)
    } else {
        None
    };
    Ok(InlineBlock {
        kind,
        name,
        id,
        version,
        key_fields,
        finite_key,
        key_index,
        durability,
        immediate_if,
        fields,
    })
}

pub(super) fn parse_compression_choice(input: ParseStream<'_>) -> Result<CompressionChoice> {
    let value: Ident = input.parse()?;
    match value.to_string().as_str() {
        "none" => Ok(CompressionChoice::None),
        "variable_blocks" => {
            let args;
            parenthesized!(args in input);
            parse_variable_compression_choice(&args).map(CompressionChoice::VariableBlocks)
        }
        _ => Err(syn::Error::new_spanned(
            value,
            "expected none or variable_blocks(...)",
        )),
    }
}

pub(super) fn validate_matrix_format(
    dims: &[MatrixDim],
    commit: Option<&MatrixCommit>,
    aux: &[MatrixAux],
    blocks: &[InlineBlock],
) -> Result<()> {
    for (index, dim) in dims.iter().enumerate() {
        if dims[(index + 1)..]
            .iter()
            .any(|other| other.name == dim.name)
        {
            return Err(syn::Error::new_spanned(
                &dim.name,
                "duplicate matrix dimension",
            ));
        }
    }

    let matrix_blocks: Vec<_> = blocks
        .iter()
        .filter_map(|block| match &block.kind {
            InlineBlockKind::Matrix(meta) => Some((block, meta)),
            _ => None,
        })
        .collect();
    if matrix_blocks.is_empty() {
        if !dims.is_empty() || commit.is_some() || !aux.is_empty() {
            return Err(syn::Error::new_spanned(
                dims.first()
                    .map(|dim| dim.name.clone())
                    .or_else(|| commit.and_then(|commit| commit.keyspace.first().cloned()))
                    .or_else(|| aux.first().map(|aux| aux.name.clone()))
                    .unwrap_or_else(|| Ident::new("commit", proc_macro2::Span::call_site())),
                "matrix dims/commit/aux require at least one matrix block",
            ));
        }
        return Ok(());
    }

    for (index, item) in aux.iter().enumerate() {
        if item.byte_len == 0 {
            return Err(syn::Error::new_spanned(
                &item.name,
                "matrix aux byte_len must be non-zero",
            ));
        }
        if aux[(index + 1)..]
            .iter()
            .any(|other| other.name == item.name)
        {
            return Err(syn::Error::new_spanned(&item.name, "duplicate matrix aux"));
        }
    }

    let commit = commit.ok_or_else(|| {
        syn::Error::new_spanned(
            &matrix_blocks[0].0.name,
            "matrix blocks require commit: cell_bitmap { ... }",
        )
    })?;
    if dims.is_empty() {
        return Err(syn::Error::new_spanned(
            &matrix_blocks[0].0.name,
            "matrix blocks require dims { ... }",
        ));
    }
    if commit.keyspace.is_empty() {
        return Err(syn::Error::new_spanned(
            &matrix_blocks[0].0.name,
            "matrix commit keyspace must not be empty",
        ));
    }
    if commit.keyspace.len() != 2 {
        return Err(syn::Error::new_spanned(
            &matrix_blocks[0].0.name,
            "P0 matrix commit keyspace must contain exactly two dimensions",
        ));
    }
    validate_ident_list("matrix commit keyspace", &commit.keyspace)?;
    validate_ident_list("matrix commit categories", &commit.categories)?;
    validate_ident_list("matrix commit singles", &commit.singles)?;
    validate_ident_list("matrix commit per_channel", &commit.per_channel)?;

    for key_dim in &commit.keyspace {
        if !dims
            .iter()
            .any(|dim| matrix_dimension_matches_key(&dim.name, key_dim))
        {
            return Err(syn::Error::new_spanned(
                key_dim,
                "matrix commit keyspace dimension is not declared in dims",
            ));
        }
    }
    for (index, (_, meta)) in matrix_blocks.iter().enumerate() {
        if meta.dims != commit.keyspace {
            return Err(syn::Error::new_spanned(
                &meta.category,
                "P0 matrix block dims must match commit keyspace order",
            ));
        }
        if meta.dims.len() != 2 {
            return Err(syn::Error::new_spanned(
                &meta.category,
                "P0 matrix block dims must contain exactly two dimensions",
            ));
        }
        if !commit
            .categories
            .iter()
            .any(|category| category == &meta.category)
        {
            return Err(syn::Error::new_spanned(
                &meta.category,
                "matrix block category is not declared in commit categories",
            ));
        }
        for (_, other) in &matrix_blocks[(index + 1)..] {
            if meta.category == other.category {
                return Err(syn::Error::new_spanned(
                    &other.category,
                    "matrix cell commit category must be unique per block",
                ));
            }
        }
    }
    for (block, _) in &matrix_blocks {
        for field in &block.fields {
            if !matrix_field_is_fixed_width(&field.ty) {
                return Err(syn::Error::new_spanned(
                    &field.name,
                    "P0 matrix fields must be a named type (or an array of one) whose \
                     codec has a fixed encoded width; references, raw pointers, slices, \
                     tuples, trait objects, `impl Trait` and function pointers can never \
                     occupy a matrix slot",
                ));
            }
        }
    }
    Ok(())
}

/// Permissive syntactic shape check for matrix field types (API-04, F-10).
///
/// A matrix slot needs a stride the compiler knows. Deciding *which* types have
/// one is a question about types, and a proc macro sees only spellings, so this
/// function deliberately does **not** answer it. It rejects exactly the source
/// shapes that can never denote a `StratavaEncode` type with a fixed encoded width
/// no matter what they resolve to — references, raw pointers, slices, tuples,
/// trait objects, `impl Trait`, function pointers, `dyn`/`!`/`_` and macro
/// invocations — and admits every named path plus arrays of an admitted
/// element.
///
/// The authority on the classification is the generated `SLOT_STRIDE`, which
/// resolves each element type's `StratavaEncode::WIRE_TYPE` and fails to compile
/// for anything without a fixed encoded width — see
/// [`matrix_slot_stride_tokens`].
///
/// The previous revision whitelisted the literal primitive spellings
/// (`"u32"`, `"f64"`, …). That made a *spelling* authoritative over a *type*
/// in both directions: `type Word = u32;` was rejected even though the alias
/// resolves to a perfectly good 4-byte slot (contradicting
/// `docs/api-reference.md`), while a user struct named `u32` in scope would
/// have been waved through to the const check anyway. Only the const check can
/// tell those apart, so only the const check decides. `PackedBitmap` — built-in
/// or user-defined — now fails there rather than here: the built-in owns a
/// `Vec<u8>` and encodes a `bit_len` plus a variable byte string, so it has no
/// fixed encoded width, and a same-named user type has no `StratavaEncode` impl at
/// all. It remains usable as an ordinary variable field.
pub(super) fn matrix_field_is_fixed_width(ty: &Type) -> bool {
    match ty {
        // A named path may be a primitive, an alias for one, or a user type
        // with a fixed-width codec. `SLOT_STRIDE` decides.
        Type::Path(_) => true,
        Type::Array(array) => matrix_field_is_fixed_width(&array.elem),
        Type::Group(group) => matrix_field_is_fixed_width(&group.elem),
        Type::Paren(paren) => matrix_field_is_fixed_width(&paren.elem),
        _ => false,
    }
}

/// The compile-time slot stride of an inline matrix block (API-04).
///
/// Every field contributes the encoded width of its own codec, resolved through
/// `StratavaEncode::WIRE_TYPE` rather than taken from `size_of`. The two disagree
/// for any type that owns heap storage — `size_of::<PackedBitmap>()` is the
/// width of a `Vec` plus a `u64`, not the width of the bytes it emits — so
/// deriving the stride from the Rust object size could publish a matrix whose
/// slots do not match the values written into them. A wire type with no fixed
/// width has no stride, and the const evaluation of this expression is what
/// rejects it, whatever the field type is spelled.
pub(super) fn matrix_slot_stride_tokens(fields: &[InlineField]) -> TokenStream2 {
    let terms = fields.iter().map(|field| {
        let mut ty = &field.ty;
        let mut count = quote!(1u64);
        while let Type::Array(array) = ty {
            let len = &array.len;
            count = quote!(#count * ((#len) as u64));
            ty = &array.elem;
        }
        quote!(#count * __stratava_matrix_encoded_width(
            <#ty as ::stratava::__core::StratavaEncode>::WIRE_TYPE
        ))
    });
    quote! {
        {
            const fn __stratava_matrix_encoded_width(wire: ::stratava::__core::WireType) -> u64 {
                match wire {
                    ::stratava::__core::WireType::Bool
                    | ::stratava::__core::WireType::U8
                    | ::stratava::__core::WireType::I8 => 1,
                    ::stratava::__core::WireType::U16 | ::stratava::__core::WireType::I16 => 2,
                    ::stratava::__core::WireType::U32
                    | ::stratava::__core::WireType::I32
                    | ::stratava::__core::WireType::F32 => 4,
                    ::stratava::__core::WireType::U64
                    | ::stratava::__core::WireType::I64
                    | ::stratava::__core::WireType::F64 => 8,
                    ::stratava::__core::WireType::U128 | ::stratava::__core::WireType::I128 => 16,
                    _ => panic!(
                        "matrix fields must have a width fixed by their type; this codec does not"
                    ),
                }
            }
            0u64 #( + #terms )*
        }
    }
}

pub(super) fn matrix_dimension_matches_key(dim: &Ident, key: &Ident) -> bool {
    matrix_storage_dimension_name_from_str(key.to_string().as_str(), &[dim.to_string()]).is_some()
}

pub(super) fn matrix_storage_dimension_name(key: &Ident, dims: &[MatrixDim]) -> Option<String> {
    let names = dims
        .iter()
        .map(|dim| dim.name.to_string())
        .collect::<Vec<_>>();
    matrix_storage_dimension_name_from_str(&key.to_string(), &names)
}

pub(super) fn matrix_storage_dimension_name_from_str(key: &str, dims: &[String]) -> Option<String> {
    let candidates = [
        key.to_string(),
        format!("n_{key}"),
        format!("n_{key}s"),
        if key == "ch" {
            "n_channels".to_string()
        } else {
            String::new()
        },
    ];
    candidates
        .into_iter()
        .filter(|candidate| !candidate.is_empty())
        .find(|candidate| dims.iter().any(|dim| dim == candidate))
}

pub(super) fn validate_ident_list(label: &str, values: &[Ident]) -> Result<()> {
    for (index, value) in values.iter().enumerate() {
        if values[(index + 1)..].iter().any(|other| other == value) {
            return Err(syn::Error::new_spanned(
                value,
                format!("duplicate {label} entry"),
            ));
        }
    }
    Ok(())
}

pub(super) fn parse_variable_compression_choice(
    input: ParseStream<'_>,
) -> Result<VariableCompressionChoice> {
    let algorithm: Ident = input.parse()?;
    let algorithm = match algorithm.to_string().as_str() {
        "zstd" => CompressionAlgorithmChoice::Zstd,
        _ => return Err(syn::Error::new_spanned(algorithm, "expected zstd")),
    };
    let mut level = CompressionLevelChoice::Default;
    let mut header = CompressionHeaderChoice::RecordExplicit;
    let mut min_len = 0u64;
    let mut only_if_smaller = true;
    let mut max_len = 64 * 1024 * 1024;

    while !input.is_empty() {
        input.parse::<Token![,]>()?;
        if input.is_empty() {
            break;
        }
        let key: Ident = input.parse()?;
        input.parse::<Token![=]>()?;
        match key.to_string().as_str() {
            "level" => {
                level = if input.peek(LitInt) {
                    let value: LitInt = input.parse()?;
                    CompressionLevelChoice::Exact(value.base10_parse::<i32>()?)
                } else {
                    let value: Ident = input.parse()?;
                    match value.to_string().as_str() {
                        "fast" => CompressionLevelChoice::Fast,
                        "default" => CompressionLevelChoice::Default,
                        "best" => CompressionLevelChoice::Best,
                        _ => {
                            return Err(syn::Error::new_spanned(
                                value,
                                "expected fast, default, best, or an integer level",
                            ));
                        }
                    }
                };
            }
            "header" => {
                let value: Ident = input.parse()?;
                header = match value.to_string().as_str() {
                    "record_explicit" => CompressionHeaderChoice::RecordExplicit,
                    "file_explicit" => CompressionHeaderChoice::FileExplicit,
                    "format_contract" => CompressionHeaderChoice::FormatContract,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "expected record_explicit, file_explicit, or format_contract",
                        ));
                    }
                };
            }
            "min_len" => {
                let value: LitInt = input.parse()?;
                min_len = value.base10_parse::<u64>()?;
            }
            "only_if_smaller" => {
                let value: LitBool = input.parse()?;
                only_if_smaller = value.value;
            }
            "max_len" => {
                let value: LitInt = input.parse()?;
                max_len = value.base10_parse::<u64>()?;
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    key,
                    "expected level, header, min_len, only_if_smaller, or max_len",
                ));
            }
        }
    }

    Ok(VariableCompressionChoice {
        algorithm,
        level,
        header,
        min_len,
        only_if_smaller,
        max_len,
    })
}
