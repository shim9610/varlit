//! Matrix sidecar files: a caller's resume state stored beside a matrix
//! file, written atomically and read back only while its manifest still
//! matches that file's identity, layout generation and schema.
use super::*;

pub(super) fn write_matrix_sidecar_file<P: AsRef<Path>>(
    spec: FormatSpec,
    category: &str,
    sidecar_path: P,
    generation: u64,
    payload: &[u8],
    identity: MatrixNativeIdentity,
) -> Result<MatrixSidecarManifest> {
    let payload_len =
        u64::try_from(payload.len()).map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
    let format_magic_len =
        u64::try_from(spec.magic.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "matrix sidecar",
        })?;
    let category_len_u64 =
        u64::try_from(category.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "matrix sidecar",
        })?;
    crate::matrix::matrix_sidecar_write_len(
        spec,
        MATRIX_SIDECAR_FIXED_LEN as u64,
        format_magic_len,
        category_len_u64,
        payload_len,
    )?;
    let manifest = matrix_sidecar_manifest_for(spec, category, generation, payload, identity)?;
    let magic_len =
        u16::try_from(manifest.format_magic.len()).map_err(|_| Error::InvalidMatrixSidecar)?;
    let category_len =
        u16::try_from(manifest.category.len()).map_err(|_| Error::InvalidMatrixSidecar)?;
    let header_len = MATRIX_SIDECAR_FIXED_LEN
        .checked_add(magic_len as usize)
        .and_then(|len| len.checked_add(category_len as usize))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: "sidecar header length",
        })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(header_len)
        .map_err(|_| Error::AllocationFailed {
            resource: "sidecar header",
            requested: header_len as u64,
        })?;
    bytes.extend_from_slice(MATRIX_SIDECAR_MAGIC);
    bytes.extend_from_slice(&MATRIX_SIDECAR_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&manifest.format_version.to_le_bytes());
    bytes.extend_from_slice(&magic_len.to_le_bytes());
    bytes.extend_from_slice(&category_len.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&manifest.schema_hash.to_le_bytes());
    bytes.extend_from_slice(&manifest.generation.to_le_bytes());
    bytes.extend_from_slice(&manifest.payload_len.to_le_bytes());
    bytes.extend_from_slice(&manifest.payload_crc32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&manifest.native_fingerprint);
    bytes.extend_from_slice(&manifest.matrix_layout_generation.to_le_bytes());
    bytes.extend_from_slice(&manifest.matrix_creation_nonce);
    debug_assert_eq!(bytes.len(), MATRIX_SIDECAR_FIXED_LEN);
    bytes.extend_from_slice(&manifest.format_magic);
    bytes.extend_from_slice(manifest.category.as_bytes());

    // Publish atomically via a same-directory RAII temp: write the full sidecar
    // into a private temp, fsync it, then atomically replace the destination and
    // sync the parent directory. A crash before the replace leaves the previous
    // sidecar untouched; the old in-place `File::create` truncated it eagerly
    // and could expose a partial sidecar.
    let sidecar_path = sidecar_path.as_ref();
    let (temp_path, mut temp_file) = create_rewrite_temp_file(sidecar_path)?;
    let write_result = (|| -> Result<()> {
        temp_file.write_all(&bytes)?;
        temp_file.write_all(payload)?;
        temp_file.flush()?;
        temp_file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        drop(temp_file);
        let _ = remove_file(&temp_path);
        return Err(error);
    }
    drop(temp_file);

    match replace_path_atomically(&temp_path, sidecar_path) {
        Ok(ReplaceDurability::Durable) => Ok(manifest),
        Ok(ReplaceDurability::ParentSyncPending(sync_error)) => {
            // The replacement is already visible at `sidecar_path`; only the
            // parent-directory entry's durability is pending. Do not delete the
            // now-published file, and surface the durability gap so a caller
            // that needs a hard guarantee can react (a regenerable sidecar may
            // simply be rewritten).
            Err(crate::fallible::boxed_path_error(
                sync_error,
                sidecar_path,
                Error::ErrorContextAllocationFailed {
                    context: crate::AllocationFailureContext::ParentSyncPending,
                },
                |path, source| Error::PublishedButParentSyncPending { path, source },
            ))
        }
        Err(error) => Err(preserve_temp_on_indeterminate(&temp_path, error)),
    }
}

pub(super) fn read_matrix_sidecar_file<P: AsRef<Path>>(
    spec: FormatSpec,
    category: &str,
    sidecar_path: P,
    expected_generation: Option<u64>,
    identity: MatrixNativeIdentity,
) -> Result<(MatrixSidecarManifest, Vec<u8>)> {
    let mut file = File::open(sidecar_path)?;
    let file_len = file.metadata()?.len();
    crate::matrix::check_matrix_sidecar_file_len(spec, file_len)?;
    if file_len < MATRIX_SIDECAR_FIXED_LEN as u64 {
        return Err(Error::InvalidMatrixSidecar);
    }
    let mut fixed = [0; MATRIX_SIDECAR_FIXED_LEN];
    file.read_exact(&mut fixed)?;
    if &fixed[0..4] != MATRIX_SIDECAR_MAGIC {
        return Err(Error::InvalidMatrixSidecar);
    }
    let version = u16::from_le_bytes(fixed[4..6].try_into().expect("slice"));
    if version != MATRIX_SIDECAR_VERSION {
        // A different envelope version (legacy v1 lacked native identity,
        // legacy v2 lacked the matrix creation nonce) is refused as stale and
        // regenerable rather than trusted.
        return Err(Error::MatrixSidecarMismatch("sidecar version"));
    }
    let format_version = u16::from_le_bytes(fixed[8..10].try_into().expect("slice"));
    let flags = u16::from_le_bytes(fixed[6..8].try_into().expect("slice"));
    let magic_len = u64::from(u16::from_le_bytes(fixed[10..12].try_into().expect("slice")));
    let category_len = u64::from(u16::from_le_bytes(fixed[12..14].try_into().expect("slice")));
    let reserved = u16::from_le_bytes(fixed[14..16].try_into().expect("slice"));
    let schema_hash = u64::from_le_bytes(fixed[16..24].try_into().expect("slice"));
    let generation = u64::from_le_bytes(fixed[24..32].try_into().expect("slice"));
    let payload_len = u64::from_le_bytes(fixed[32..40].try_into().expect("slice"));
    let payload_crc32 = u32::from_le_bytes(fixed[40..44].try_into().expect("slice"));
    let trailing_reserved = u32::from_le_bytes(fixed[44..48].try_into().expect("slice"));
    let mut native_fingerprint = [0u8; 32];
    native_fingerprint.copy_from_slice(
        &fixed[MATRIX_SIDECAR_FINGERPRINT_OFFSET..MATRIX_SIDECAR_LAYOUT_GENERATION_OFFSET],
    );
    let matrix_layout_generation = u64::from_le_bytes(
        fixed[MATRIX_SIDECAR_LAYOUT_GENERATION_OFFSET..MATRIX_SIDECAR_CREATION_NONCE_OFFSET]
            .try_into()
            .expect("slice"),
    );
    let mut matrix_creation_nonce = [0u8; MATRIX_CREATION_NONCE_LEN];
    matrix_creation_nonce
        .copy_from_slice(&fixed[MATRIX_SIDECAR_CREATION_NONCE_OFFSET..MATRIX_SIDECAR_FIXED_LEN]);
    let plan = crate::matrix::matrix_sidecar_read_plan(
        spec,
        file_len,
        MATRIX_SIDECAR_FIXED_LEN as u64,
        magic_len,
        category_len,
        payload_len,
        flags,
        reserved,
        trailing_reserved,
    )?;
    // only the small fixed header and the bounded magic/category
    // prefix are read before identity validation. A sidecar that belongs to a
    // different native file, schema, category or generation is rejected here,
    // before the payload is ever allocated, read or hashed.
    file.seek(SeekFrom::Start(plan.format_magic_offset))?;
    let mut format_magic = try_alloc_bytes(magic_len, "sidecar format magic")?;
    file.read_exact(&mut format_magic)?;
    file.seek(SeekFrom::Start(plan.category_offset))?;
    let mut category_bytes = try_alloc_bytes(category_len, "sidecar category")?;
    file.read_exact(&mut category_bytes)?;
    let category_text =
        String::from_utf8(category_bytes).map_err(|_| Error::InvalidMatrixSidecar)?;
    let manifest = MatrixSidecarManifest {
        format_magic,
        format_version,
        schema_hash,
        category: category_text,
        generation,
        payload_len,
        payload_crc32,
        native_fingerprint,
        matrix_layout_generation,
        matrix_creation_nonce,
    };
    validate_matrix_sidecar_manifest(spec, category, expected_generation, identity, &manifest)?;
    file.seek(SeekFrom::Start(plan.payload_offset))?;
    let mut payload = try_alloc_bytes(plan.payload_len, "sidecar payload")?;
    file.read_exact(&mut payload)?;
    let actual = crc32_bytes(&payload)?;
    if actual != payload_crc32 {
        return Err(Error::MatrixSidecarChecksumMismatch {
            expected: payload_crc32,
            actual,
        });
    }
    Ok((manifest, payload))
}

pub(super) fn matrix_sidecar_manifest_for(
    spec: FormatSpec,
    category: &str,
    generation: u64,
    payload: &[u8],
    identity: MatrixNativeIdentity,
) -> Result<MatrixSidecarManifest> {
    if category.is_empty() {
        return Err(Error::InvalidMatrixSidecar);
    }
    let mut format_magic = try_alloc_bytes(
        u64::try_from(spec.magic.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "matrix sidecar",
        })?,
        "sidecar format magic",
    )?;
    format_magic.copy_from_slice(spec.magic);
    let category_len =
        u64::try_from(category.len()).map_err(|_| Error::ResourceArithmeticOverflow {
            resource: "matrix sidecar",
        })?;
    let mut category_text = String::new();
    category_text
        .try_reserve_exact(category.len())
        .map_err(|_| Error::AllocationFailed {
            resource: "sidecar category",
            requested: category_len,
        })?;
    category_text.push_str(category);
    Ok(MatrixSidecarManifest {
        format_magic,
        format_version: spec.version,
        schema_hash: spec.computed_schema_hash(),
        category: category_text,
        generation,
        payload_len: payload
            .len()
            .try_into()
            .map_err(|_| Error::InvalidMatrixSidecar)?,
        payload_crc32: crc32_bytes(payload)?,
        native_fingerprint: identity.fingerprint,
        matrix_layout_generation: identity.layout_generation,
        matrix_creation_nonce: identity.creation_nonce,
    })
}

pub(super) fn validate_matrix_sidecar_manifest(
    spec: FormatSpec,
    category: &str,
    expected_generation: Option<u64>,
    identity: MatrixNativeIdentity,
    manifest: &MatrixSidecarManifest,
) -> Result<()> {
    if manifest.format_magic != spec.magic {
        return Err(Error::MatrixSidecarMismatch("format magic"));
    }
    if manifest.format_version != spec.version {
        return Err(Error::MatrixSidecarMismatch("format version"));
    }
    if manifest.schema_hash != spec.computed_schema_hash() {
        return Err(Error::MatrixSidecarMismatch("schema hash"));
    }
    if manifest.category != category {
        return Err(Error::MatrixSidecarMismatch("category"));
    }
    // Bind to a specific native file and its matrix layout: a same-spec,
    // same-generation sibling produces a different OS-object fingerprint and is
    // rejected.
    if manifest.native_fingerprint != identity.fingerprint {
        return Err(Error::MatrixSidecarMismatch("native identity"));
    }
    if manifest.matrix_layout_generation != identity.layout_generation {
        return Err(Error::MatrixSidecarMismatch("matrix layout generation"));
    }
    // Same OS object, schema and layout offsets are not enough: a matrix
    // recreated in the same file object gets a fresh creation nonce, so every
    // sidecar published against the previous logical matrix is refused as
    // stale and regenerable.
    if manifest.matrix_creation_nonce != identity.creation_nonce {
        return Err(Error::MatrixSidecarMismatch("creation nonce"));
    }
    if let Some(expected_generation) = expected_generation
        && manifest.generation != expected_generation
    {
        return Err(Error::MatrixSidecarMismatch("generation"));
    }
    Ok(())
}
