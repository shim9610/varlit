//! Source gates for private construction, publication and writer-poison boundaries.
//! Compile-fail tests cover externally nameable types; these checks cover private
//! module invariants and reject changes that bypass their enforcement.

use std::fs;
use std::path::{Path, PathBuf};

#[path = "support/source_tree.rs"]
mod source_tree;
use source_tree::expanded_source;

fn crate_source(name: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../varlit-core/src");
    expanded_source(&root.join(name))
}

/// The source with every comment line removed.
///
/// Round 15 added in-crate *bypass catalogues* — `#[cfg(test)]` modules whose
/// documentation quotes each forbidden spelling next to the diagnostic rustc
/// emits for it. Those quotations are the point: they are what a future round
/// re-runs. But a gate that counts occurrences of a spelling must not count
/// them, or documenting a bypass would break the gate that forbids it.
fn crate_code(name: &str) -> String {
    crate_source(name)
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("//")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An attribute that compiles its item only under `cfg(test)`.
///
/// `#[cfg(test)]` is the common spelling, but `matrix.rs` and `file.rs` also
/// carry `#[cfg(all(test, feature = "..."))]` items. Round 16's stripper knew
/// only the first, so those counted as shipped code — conservative for the
/// gates, but it made "what ships" mean two different things in one helper.
fn is_test_gate_attribute(trimmed: &str) -> bool {
    trimmed == "#[cfg(test)]"
        || trimmed.starts_with("#[cfg(all(test,")
        || trimmed.starts_with("#[cfg(all(test)")
}

/// The source with every comment line and every test-only item removed: what
/// actually ships.
///
/// The bypass catalogues exercise the legitimate constructors on purpose, so a
/// gate that says "this constructor is called exactly once" has to mean once in
/// the library. Counting the tests would make proving the property break the
/// gate that states it.
///
/// # This is a textual stripper, and it fails loudly rather than quietly
///
/// Re-verification of round 16 named the honest risk: the skip is driven by
/// text, from a line that is exactly `#[cfg(test)]` to the next line that is
/// exactly the same indentation followed by `}`. Anything else — a
/// `#[cfg(test)] use ...;`, a `#[cfg(test)] fn` nested one level in, a
/// `#[cfg(all(test, feature = "..."))]` item, a `let` whose initialiser wraps —
/// either escaped the strip or would have swallowed arbitrary shipped code up
/// to the next same-indent closing brace, silently *narrowing every gate built
/// on it*. Silent narrowing is the one failure mode a gate must not have.
///
/// So every assumption is now checked, and a violation panics naming the file,
/// the line number and the offending text instead of returning a quietly
/// shortened string:
///
/// 1. the item introduced by a test-gate attribute must either open a brace at
///    the attribute's own indentation or end in `;` within
///    `MAX_ITEM_HEADER_LINES` (further attributes and doc comments in between
///    are allowed, because that is how the catalogues are written);
/// 2. every skip that starts must end before the file does.
///
/// `the_shipped_code_stripper_removes_test_modules_and_nothing_else` pins the
/// result: only whole lines are ever dropped, in order, and nothing bearing
/// `#[test]` or a test-gate attribute survives.
fn crate_shipped_code(name: &str) -> String {
    let source = crate_source(name);
    let mut kept: Vec<&str> = Vec::new();
    // The indentation of the `#[cfg(test)]` being skipped, so that a test
    // module *nested inside* an enforcement module is stripped too. Round 16
    // moved `fatal_access`'s own derivation test inside the module (its
    // constructor is now private to it), and a stripper that only knew about
    // column-zero `#[cfg(test)]` would have counted those calls against the
    // gate that says the constructor is called once in the library.
    let mut skipping: Option<String> = None;
    // Set between the `#[cfg(test)]` line and the item it introduces, carrying
    // the attribute's indentation and how many lines of that item have been
    // seen. The counter is what keeps a mis-parse loud: a well-formed item
    // resolves within a couple of lines, so running past the bound means the
    // stripper no longer understands the source and must say so.
    let mut awaiting_item: Option<(String, usize)> = None;
    const MAX_ITEM_HEADER_LINES: usize = 8;
    for (number, line) in source.lines().enumerate() {
        let number = number + 1;
        if let Some(indent) = skipping.as_deref() {
            if line == format!("{indent}}}") {
                skipping = None;
            }
            continue;
        }
        if let Some((indent, seen)) = awaiting_item.clone() {
            let trimmed = line.trim_start();
            // Further attributes and doc comments still belong to the item.
            if trimmed.starts_with("#[") || trimmed.starts_with("//") || trimmed.is_empty() {
                continue;
            }
            // A braced item — `mod`, `fn`, `impl` — opens its scope at the
            // attribute's own indentation and closes it at `{indent}}`.
            if line.ends_with('{') && line.starts_with(&indent) {
                awaiting_item = None;
                skipping = Some(indent);
                continue;
            }
            // A statement — `use ...;`, or a `let` whose initialiser wraps
            // onto following lines — ends at the first line ending in `;`.
            if line.ends_with(';') {
                awaiting_item = None;
                continue;
            }
            assert!(
                seen < MAX_ITEM_HEADER_LINES,
                "{name}:{number}: a `#[cfg(test)]` item is still unresolved {seen} lines in, at \
                 `{}`. This stripper understands a braced item opening at the attribute's own \
                 indentation and a statement ending in `;`. Teach `crate_shipped_code` about \
                 the new shape — do not leave it, because every gate built on this helper \
                 would silently narrow.",
                line.trim()
            );
            awaiting_item = Some((indent, seen + 1));
            continue;
        }
        if is_test_gate_attribute(line.trim_start()) {
            let indent = &line[..line.len() - line.trim_start().len()];
            awaiting_item = Some((indent.to_string(), 0));
            continue;
        }
        if line.trim_start().starts_with("//") {
            continue;
        }
        kept.push(line);
    }
    assert!(
        awaiting_item.is_none(),
        "{name}: a trailing `#[cfg(test)]` introduces no item"
    );
    assert!(
        skipping.is_none(),
        "{name}: a `#[cfg(test)]` module was never closed by a line that is exactly its \
         indentation plus `}}` — the stripper ran to end of file and would have hidden \
         everything after it from every gate that uses it"
    );
    kept.join("\n")
}

/// The stripper's own contract, stated as a test rather than as a comment.
///
/// Re-verification called the `#[cfg(test)]` skip "a real narrowing of those
/// gates" whose heuristic "will silently mis-scope if anyone reformats". It no
/// longer can be silent — but the property worth pinning is the one the gates
/// actually depend on: the stripper removes test modules and *only* test
/// modules, leaving shipped code byte-identical.
#[test]
fn the_shipped_code_stripper_removes_test_modules_and_nothing_else() {
    for name in ["format.rs", "file.rs", "writer_permit.rs"] {
        let code = crate_code(name);
        let shipped = crate_shipped_code(name);
        assert!(
            shipped.len() < code.len(),
            "{name}: stripping `#[cfg(test)]` removed nothing; the catalogues live in one"
        );
        // Nothing survives that only a test module could have contained...
        assert!(
            !shipped.contains("#[cfg(test)]"),
            "{name}: a `#[cfg(test)]` attribute survived stripping"
        );
        assert!(
            !shipped.contains("#[test]"),
            "{name}: a `#[test]` function survived stripping"
        );
        // ..and everything that survives is a line of the comment-stripped
        // source, in the same order: the stripper only ever drops lines.
        let mut shipped_lines = shipped.lines();
        let mut dropped = 0usize;
        for line in code.lines() {
            match shipped_lines.clone().next() {
                Some(next) if next == line => {
                    shipped_lines.next();
                }
                _ => dropped += 1,
            }
        }
        assert_eq!(
            shipped_lines.next(),
            None,
            "{name}: stripping produced a line the source does not have"
        );
        assert!(dropped > 0, "{name}: nothing was dropped");
    }
}

/// Every `.rs` file that ships in `varlit-core`. Widened deliberately: a gate
/// that names its modules cannot see the module somebody adds next.
fn crate_modules() -> Vec<String> {
    let dir: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("varlit-core")
        .join("src");
    fn collect(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).expect("source directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                collect(&path, root, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let mut modules = Vec::new();
    collect(&dir, &dir, &mut modules);
    modules.sort();
    assert!(
        modules.len() > 15,
        "the module sweep found {} files, which cannot be right",
        modules.len()
    );
    modules
}

/// SHAPE B, crate-wide. `crate::writer_permit::PoisonFlag` is the only holder
/// of a writer poison boolean. Round 12 found three independent hand-written
/// flags (`stream.rs`, `file.rs`, `layout.rs`), each with its own guard and its
/// own convention about which methods must consult it;  was two of them
/// disagreeing. A fourth would recreate the class, so a bare `poisoned: bool`
/// field anywhere in the crate fails here.
#[test]
fn no_writer_keeps_its_own_poison_boolean() {
    for module in [
        "file.rs",
        "layout.rs",
        "stream.rs",
        "indexed.rs",
        "matrix.rs",
        "disk_index.rs",
    ] {
        let source = crate_source(module);
        assert!(
            !source.contains("poisoned: bool"),
            "{module} declares its own poison boolean; use `crate::writer_permit::PoisonFlag`, \
             so that the guarded operations can demand a `MutationPermit` and a mutating \
             method written later cannot skip the check"
        );
    }
    let permit = crate_source("writer_permit.rs");
    assert!(
        permit.contains("poisoned: bool") && permit.contains("in_flight: bool"),
        "`writer_permit` must remain the one holder of the state"
    );
    assert!(
        permit.contains("pub struct MutationPermit<'a, W: ?Sized> {\n    writer: &'a mut W,\n    context: &'static str,\n}"),
        "the permit's field must stay private: a public field makes the witness forgeable, \
         which is what tests/ui/fail_fabricated_mutation_permit.rs asserts"
    );
}

/// Writer-bound mutation permits. An unbound permit proved that *a* poison flag
/// had been checked, not that *the writer being mutated* had been:
/// `PoisonFlag::healthy()` is a `const fn` and the permit constructor was
/// `pub(crate)`, so re-verification compiled a three-line bypass —
/// `PoisonFlag::healthy().permit("stream")` handed straight to
/// `append_prepared_chunk` on a poisoned writer. The witness was launderable
/// from a decoy.
///
/// The constructor is now **private to `writer_permit`**, so no other module can
/// mint a permit from any flag, and the only route out of the module is
/// `GuardedWriter::writer_permit`, which takes `&mut self` of the writer and reads
/// that writer's own flag. Restoring any visibility on the constructor is the
/// single edit that reopens the bypass, and it fails here.
///
/// This one cannot be a `tests/ui` fixture: the bypass was *in-crate*, and every
/// spelling of it is already unreachable from a downstream crate, so a
/// compile-fail fixture would pass for the wrong reason. The cross-writer half
/// of the property — that a permit for one writer is not a permit for another —
/// *is* expressible downstream and is asserted by
/// `tests/ui/fail_cross_writer_mutation_permit.rs`.
#[test]
fn a_permit_can_only_be_minted_from_the_writers_own_poison_flag() {
    let permit = crate_source("writer_permit.rs");
    assert!(permit.contains("    fn issue<'a, W: GuardedWriter>("));
    assert!(permit.contains("        writer: &'a mut W,"));
    for visibility in ["pub fn issue", "pub(crate) fn issue", "pub(super) fn issue"] {
        assert!(
            !permit.contains(visibility),
            "the sole permit constructor must be private"
        );
    }
    assert_eq!(
        crate_shipped_code("writer_permit.rs")
            .matches("Ok(MutationPermit { writer, context })")
            .count(),
        1
    );
    assert!(permit.contains("    pub fn healthy() -> Self {"));
    assert!(permit.contains("fn writer_permit(&mut self, context: &'static str) -> Result<MutationPermit<'_, Self>> {\n        PoisonFlag::issue(self, context)\n    }"),
        "only an exclusive borrow of the authoritative writer may mint a handle");
    assert!(
        permit.contains("self.writer.check_mutation(self.context)"),
        "a later guarded step must recheck poison raised after the handle was issued"
    );
    for (module, owner) in [("file.rs", "File"), ("stream.rs", "Stream")] {
        let source = crate_shipped_code(module);
        assert!(source.contains(&format!("impl {owner}MutationPermit<'_> {{")));
        assert!(
            !source.contains("permit: &"),
            "{module}: the operation receiver must be the bound handle, not a second writer"
        );
    }
    for module in ["file.rs", "stream.rs", "layout.rs"] {
        let source = crate_code(module);
        assert!(
            !source.contains("fn check_mutation("),
            "{module}: no override of the authoritative check"
        );
        assert!(
            !source.contains("fn writer_permit("),
            "{module}: no alternate permit minting route"
        );
        assert!(
            source.contains("fn poison_flag(&self) -> &PoisonFlag {\n        &self.poison\n    }")
        );
        assert!(source.contains(
            "fn poison_flag_mut(&mut self) -> &mut PoisonFlag {\n        &mut self.poison\n    }"
        ));
    }

    // Belt and braces: no module may hold a `PoisonFlag` anywhere but in a
    // writer's own field, so a decoy flag does not even come into being.
    for module in ["file.rs", "layout.rs", "stream.rs", "indexed.rs"] {
        let source = crate_code(module);
        for (number, line) in source.lines().enumerate() {
            if !line.contains("PoisonFlag::healthy()") {
                continue;
            }
            assert!(
                line.trim_start()
                    .starts_with("poison: PoisonFlag::healthy()"),
                "{module}:{}: a `PoisonFlag` may only be created as a writer's own field; a local \
                 one is a decoy, and the permit constructor's privacy is what stops it speaking \
                 for a real writer",
                number + 1
            );
        }
    }
}

/// SHAPE A/B, `file.rs`. Round 12 claimed that a replacement path added later
/// "cannot reach `self.index[..]` for a caller-chosen ordinal without going
/// through" the version check, and that `overwrite_record_bytes_in_place` was
/// "the only function in the crate that rewrites the bytes of an already-indexed
/// record". Re-verification compiled both bypasses: a new function that resolved
/// its own ordinal and called the writer, and a new `self.file.seek(..)` /
/// `self.file.write_all(..)` pair that skipped the writer entirely. Both claims
/// were about today's code, not about what the compiler permits.
///
/// The compiler now refuses both, because the primary handle is a `RecordFile`
/// whose `File` is unnameable outside `mod record_file` and which exposes no
/// general-purpose write. This gate stands for the part a compile-fail fixture
/// cannot express from outside the crate: that the handle stays wrapped and
/// that no second write pair is introduced against the raw field.
/// Every reserved block id has to be in `INTERNAL_BLOCK_IDS`.
///
/// The list is what `HEADER_TAILS_RESERVED_ENTRIES` counts, and the region's
/// capacity is fixed at create: a table that outgrows it is dropped whole, so
/// an id missing from the list would make `index: header_tails` silently do
/// nothing for the life of every file written after it was added.
///
/// The compiler cannot catch an omission — a new `const FOO_BLOCK_ID` compiles
/// perfectly well without being listed — so this reads the source, which is the
/// same reason every other gate in this file does.
#[test]
fn every_reserved_block_id_is_in_the_internal_list() {
    let source = crate_source("file.rs");
    let list_start = source
        .find("pub(crate) const INTERNAL_BLOCK_IDS: &[u32] = &[")
        .expect("the list exists");
    let list_end = source[list_start..]
        .find("];")
        .map(|at| list_start + at)
        .expect("the list is terminated");
    let list = &source[list_start..list_end];

    let mut missing: Vec<&str> = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("pub const ")
            .or_else(|| line.strip_prefix("pub(crate) const "))
            .or_else(|| line.strip_prefix("const "))
        else {
            continue;
        };
        let Some(name) = rest.split(':').next() else {
            continue;
        };
        // `RESERVED_BLOCK_ID_START` is the range bound, not a member of it.
        if !name.ends_with("_BLOCK_ID") {
            continue;
        }
        if !list.contains(name) {
            missing.push(line);
        }
    }
    assert!(
        missing.is_empty(),
        "these reserved block ids are not in `INTERNAL_BLOCK_IDS`, so the header tail \
         region does not reserve a slot for them and a table naming one would be dropped \
         whole: {missing:?}"
    );
}

#[test]
fn the_primary_record_handle_is_only_written_through_its_four_gated_operations() {
    let source = crate_source("file.rs");
    assert!(
        source.contains("    file: RecordFile,"),
        "`VarlitFile` must hold the primary handle as a `RecordFile`; a bare `File` field puts \
         `seek`/`write_all` back in reach of every one of this file's ~12,000 lines, which is \
         how F-01 and F-02 were both reproducible against round 12's fixes"
    );
    let gate_start = source
        .find("\nmod record_file {")
        .expect("file.rs must still hold the private `mod record_file` gate");
    let gate_end = source[gate_start + 1..]
        .find("\n}\n")
        .map(|offset| gate_start + 1 + offset)
        .expect("`mod record_file` must be closed");
    let gate = &source[gate_start..gate_end];
    // Everything outside the gate. The gate's own body is where the two
    // permitted writes are implemented; anywhere else, a `self.file` write or
    // seek is a second route to record bytes.
    let outside: String = format!("{}{}", &source[..gate_start], &source[gate_end..])
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for bypass in [
        "self.file.write_all(",
        "self.file.write(",
        "self.file.seek(",
        "self.file.write_vectored(",
        "self.file.write_all_at(",
        "self.file.write_at(",
        "self.file.seek_write(",
    ] {
        assert!(
            !outside.contains(bypass),
            "`{bypass}` writes or positions the primary handle directly; record bytes may only \
             be placed by `RecordFile::append_record_at_end` (which seeks to the end itself) or \
             by `RecordFile::overwrite_indexed_record` (which consumes a `RecordOverwrite`), \
             header bytes only by `RecordFile::overwrite_header_region`, and a footer's mutable \
             word only by `RecordFile::overwrite_record_mutable_flags`"
        );
    }
    // The gate above forbids a bypass; it does not notice a *new gated method*,
    // and adding one is exactly how the module's guarantee widens without
    // anyone deciding to widen it. So count them: five today, and a sixth has
    // to be argued for here before it can exist.
    //
    // The fifth is `overwrite_matrix_bytes`. Matrix storage records (tiles,
    // their directory pages, the metadata record and the write-ahead ring)
    // are appended through `append_record_at_end` like every record, with
    // `RECORD_FLAG_IN_PLACE`, which keeps their payload out of the record
    // checksum; their framing never changes afterwards, so anchors and their
    // summaries stay valid. The tile engine is the only caller, and its ring
    // is what makes those in-place writes crash-atomic.
    //
    // The fourth is `overwrite_record_mutable_flags`, and the argument for it
    // is that it is the narrowest write in the module. It takes `[u8; 4]`
    // rather than a slice, so no run of record bytes fits through it; the four
    // bytes it writes are the footer's trailing word, which
    // `LivenessPolicy::FooterFlags` excludes from the record checksum; and it
    // is the only way varlit mutates a record without rewriting it. Routing it
    // through `overwrite_header_region` instead would have been a lie about
    // what that method touches, and reusing `overwrite_indexed_record` would
    // have meant re-encoding a header and payload to change four bytes.
    let gated: Vec<&str> = gate
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pub(super) fn "))
        .filter_map(|rest| rest.split(['(', '<']).next())
        .filter(|name| {
            matches!(
                *name,
                "append_record_at_end"
                    | "overwrite_indexed_record"
                    | "overwrite_header_region"
                    | "overwrite_record_mutable_flags"
                    | "overwrite_matrix_bytes"
            )
        })
        .collect();
    assert_eq!(
        gated.len(),
        5,
        "`mod record_file` must expose exactly the five writes this gate names, and no other: \
         found {gated:?}"
    );
    // Comment-stripped, because the fields carry doc comments and a gate that
    // breaks when someone rewords a comment teaches people to relax the gate.
    let gate_code: String = gate
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        gate_code.contains("    pub struct RecordFile {\n        file: PrimaryFile,\n    }"),
        "the wrapped handle must stay the only, private field of `RecordFile`; any further one \
         has to be argued for here. \
         \
         A cached file offset is the field this gate is now specifically here to keep out. There \
         was one — `cursor: Option<u64>`, to spare the append path an `lseek` per record — and it \
         could not be made sound: `VarlitFile::snapshot` is a `try_clone` of this same handle, so \
         it shares the open file description and therefore the offset, and it seeks that offset \
         (`SnapshotFile::cursor_at`, every `replace_*` through `validate_generation_index`, and \
         on Windows every single positional read, `read_exact_at` being `seek_read` there). The \
         Windows case moves the offset from another thread under `&self` while this handle is \
         exclusively borrowed, which no invalidate-on-handout scheme reaches. Making the append \
         path free of the syscall did not need the cache anyway: the offset's only consumer was \
         `AppendSnapshot`'s rollback cursor, and the rollback restores `eof` instead. \
         \
         The handle is a `PrimaryFile` because anchors sit inside the log and only that type \
         translates logical offsets around them; see the gate on `PrimaryFile` below for why \
         its `position` is not the cached offset this paragraph excludes"
    );
    // `PrimaryFile` implements `Write`, so lending one is lending a write.
    assert!(
        !gate.contains("-> &PrimaryFile") && !gate.contains("-> &mut PrimaryFile"),
        "`RecordFile` must not lend out its `PrimaryFile`: it implements `Write`"
    );
    // `PrimaryFile::position` is this handle's own logical cursor, not the
    // kernel's offset: every byte moves by a positional call (`pread`/`pwrite`,
    // `seek_read`/`seek_write`), and `try_clone` starts the clone at zero. So a
    // snapshot clone shares the open file description but never a cursor, and
    // a Windows `seek_read` that moves the kernel offset moves nothing anyone
    // reads. That holds only while the type never seeks or streams its handle.
    let primary = crate_code("primary_file.rs");
    for bypass in [
        "self.file.seek(",
        "self.file.read(",
        "self.file.read_exact(",
        "self.file.write(",
        "self.file.write_all(",
        "self.file.stream_position(",
    ] {
        assert!(
            !primary.contains(bypass),
            "`{bypass}` uses the kernel offset `PrimaryFile` shares with its clones; every \
             transfer must be positional"
        );
    }
    let clone = &primary[primary
        .find("fn try_clone(")
        .expect("`PrimaryFile::try_clone` exists")..];
    let clone = &clone[..clone.find("\n    }\n").expect("`try_clone` is closed")];
    assert!(
        clone.contains("position: 0,"),
        "`PrimaryFile::try_clone` must start the clone's cursor at zero, not copy it"
    );
    assert!(
        !gate.contains("impl Write for RecordFile")
            && !gate.contains("impl Seek for RecordFile")
            && !gate.contains("fn as_file")
            && !gate.contains("-> &File"),
        "`RecordFile` must not implement a general write, nor lend out a `&File`: `&File` \
         implements `Write`, so lending one is equivalent to lending a mutable handle"
    );
    // The single deliberate exception, named in the module doc rather than left
    // to be discovered.
    assert_eq!(
        source.matches("fn barrier_file(").count(),
        1,
        "there must be exactly one escape hatch to the raw handle"
    );
}

/// The named exception to the gate above. The public `MatrixDurabilityBarrier`
/// trait takes `&mut File`, so the handle has to be lent to a caller's barrier.
/// This gate keeps the exception from silently widening: every use of the
/// accessor must be an argument to a matrix durability sync.
#[test]
fn the_primary_handle_escape_is_only_for_the_matrix_region() {
    let source = crate_source("file.rs");
    for (number, line) in source.lines().enumerate() {
        if !line.contains(".barrier_file()") {
            continue;
        }
        let context: String = source
            .lines()
            .skip(number.saturating_sub(6))
            .take(7)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            context.contains("sync_matrix_data")
                || context.contains("sync_matrix_commit")
                || line.contains("pub(super) fn barrier_file"),
            "file.rs:{}: `barrier_file()` is the one escape from the record-write gate and may \
             only hand the handle to a matrix durability barrier; this use is something \
             else:\n{context}",
            number + 1
        );
    }
}

/// Wiring in `file.rs`. The two enforcement types keep their private fields and
/// their single constructors, and the in-place writer keeps taking the
/// version-checked token rather than a caller-supplied offset. The compile-fail
/// fixtures assert the fields; this asserts the wiring they are pointless
/// without.
#[test]
fn an_in_place_record_write_still_demands_a_version_checked_target() {
    let source = crate_source("file.rs");
    assert!(
        source.contains(
            "pub struct ReplacementTarget {\n        position: usize,\n        entry: super::RecordIndexEntry,\n    }"
        ),
        "the target token's field must stay private, which is what \
         tests/ui/fail_fabricated_replacement_target.rs asserts"
    );
    // Two constructors, and the count is on the *body* rather than on the
    // count of `pub(super) fn`s, because what has to stay unique is the place
    // the stored-version refusal happens. `resolve<T>` substitutes `T::ID` and
    // `T::VERSION` into `resolve_internal` and does nothing else, so a third
    // constructor that skipped the refusal would have to add a second `if
    // actual != ` — which this catches.
    assert_eq!(
        source.matches("fn resolve<T: VarlitBlock>").count(),
        1,
        "`ReplacementTarget::resolve` must remain the only *generic* constructor"
    );
    assert_eq!(
        source.matches("fn resolve_internal(").count(),
        1,
        "`ReplacementTarget::resolve_internal` must remain the only non-generic constructor. It \
         exists because a matrix chunk record is a block varlit writes itself and no `VarlitBlock` \
         type names, and a written chunk has to be rewritable in place — see \
         `VarlitFile::rewrite_chunk_record`. It performs the same refusal against an explicit \
         `(block_id, version)` pair"
    );
    assert_eq!(
        source.matches("if actual != block_version {").count(),
        1,
        "the stored-version refusal must live in exactly one place, whichever constructor was \
         called: that is the whole reason a second constructor delegates rather than repeating"
    );
    assert_eq!(
        source.matches("fn prepare(").count(),
        1,
        "`RecordOverwrite::prepare` must remain the only constructor, because it is where the \
         keyed-tail invalidation lives"
    );
    assert!(
        source.contains("keyed_tails.invalidate(entry.block_id);"),
        "producing the write permission must drop the target block's keyed-tail map, before the \
         permission exists and therefore before any byte can reach disk (F-02)"
    );
    assert!(
        source.contains("target: ReplacementTarget,"),
        "the in-place writer must take the version-checked token, not a caller-chosen offset"
    );
    assert!(
        source.contains("write: RecordOverwrite,"),
        "`RecordFile::overwrite_indexed_record` must consume the permission by value"
    );
}

/// SHAPE B, the inventory itself. The predecessor of this gate matched the
/// literal string `poisoned: bool` in six named files, which by construction
/// could not find a validity flag with another name — and re-verification found
/// two: `matrix.rs`'s `fatal_access_blocked` (a `Fatal` recovery finding
/// blocking safe access to matrix state, consulted by
/// `ensure_fatal_access_allowed` at eleven sites) and, added by the round that
/// was supposed to be closing this class, `crc_valid_complete` ('s
/// evidence-completeness fact, consulted by one `ensure_` call). Both were
/// guards a later function simply had to not call.
///
/// The rule is therefore stated by *shape* rather than by name: no struct in
/// the crate may declare a `bool` field that an `fn ensure_*` guard consults.
/// A boolean that gates an operation belongs inside a type whose only output is
/// a witness, so that the operations demand the check instead of remembering
/// it — `writer_permit::PoisonFlag`, `matrix::fatal_access::FatalAccessGate`
/// and `matrix::crc_valid_evidence::CrcValidEvidence` are the three that exist,
/// and each keeps its boolean unnameable outside its module.
///
/// The scan is deliberately conservative about what an "`ensure_` guard" is: it
/// reads the forty lines following each `fn ensure_` signature. Guards are
/// short, and a guard that is not would be its own finding.
#[test]
fn no_guard_consults_a_bare_boolean_field() {
    for module in crate_modules() {
        let source = crate_source(&module);
        let fields: Vec<String> = source
            .lines()
            .map(str::trim)
            .filter(|line| line.ends_with(": bool,") && !line.starts_with("//"))
            .filter_map(|line| {
                line.split(':').next().map(|name| {
                    // Extraction promotes parent-private helper fields to
                    // pub(super); that must not remove them from the old sweep.
                    name.trim()
                        .strip_prefix("pub(super) ")
                        .unwrap_or(name.trim())
                        .to_string()
                })
            })
            .filter(|name| !name.is_empty() && !name.contains(' '))
            .collect();
        if fields.is_empty() {
            continue;
        }
        let lines: Vec<&str> = source.lines().collect();
        for (number, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let is_guard = [
                "fn ensure_",
                "pub fn ensure_",
                "pub(crate) fn ensure_",
                "pub(super) fn ensure_",
            ]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix));
            if !is_guard {
                continue;
            }
            // Bounded by the guard's own closing brace rather than by a line
            // count: a window that overruns reads the *next* function's
            // bookkeeping and reports it as a gate, which is a false finding
            // and would train a reader to ignore this test.
            let indent = &line[..line.len() - trimmed.len()];
            let closing = format!("{indent}}}");
            let body_end = lines[number + 1..]
                .iter()
                .position(|candidate| *candidate == closing)
                .map(|offset| number + 1 + offset)
                .unwrap_or(lines.len() - 1);
            for field in &fields {
                // What makes a boolean a *gate* is that the guard refuses
                // because of it. A guard that reads a bookkeeping flag on its
                // way to doing something — `indexed.rs::ensure_batch` and its
                // `dirty` — is not this class, and reporting it would be a
                // false finding that trains a reader to ignore this test.
                let refuses =
                    lines[number..=body_end]
                        .iter()
                        .enumerate()
                        .any(|(offset, candidate)| {
                            let position = number + offset;
                            candidate.contains(&format!(".{field}"))
                                && candidate.trim_start().starts_with("if ")
                                && lines[position..lines.len().min(position + 5)]
                                    .iter()
                                    .any(|following| following.contains("return Err("))
                        });
                assert!(
                    !refuses,
                    "{module}:{}: the guard `{}` refuses on the bare boolean field `{field}`. A \
                     boolean that gates an operation is a guard a later function can fail to \
                     call, which is how `fatal_access_blocked` and `crc_valid_complete` \
                     survived the round that closed this class elsewhere. Put it in a type \
                     whose only output is a witness the guarded operation demands.",
                    number + 1,
                    line.trim()
                );
            }
        }
    }
}

/// SHAPE A, `file.rs`, the ordering half. Re-verification compiled 's exact
/// shape against round 12's token — an authoritative `write_all`, then the
/// fallible `ReservedIndexSlot::reserve`, then the install — because the token
/// proved a reservation *existed*, not that it *preceded* the write. It also
/// compiled a mirror grown around the token entirely, via `core::mem::take` on
/// the `Vec` field.
///
/// Both are now refused: the mirror is a `ResidentIndex` whose `Vec` is
/// unnameable outside `mod resident_index`, and the append demands the
/// reservation token as an argument, so the write cannot be reached before the
/// fallible half has succeeded. This gate stands for the declarations that make
/// that true; the downstream half is
/// `tests/ui/fail_fabricated_index_reservation.rs`.
#[test]
fn the_resident_record_index_cannot_be_grown_or_outrun() {
    let source = crate_source("file.rs");
    let directory = crate_source("native_directory.rs");
    assert!(
        source.contains("    index: ResidentIndex,"),
        "`VarlitFile::index` must stay a `ResidentIndex`; a bare `Vec` field puts `push`, \
         `insert`, `extend` and `mem::take` back in reach of every one of this file's ~12,000 \
         lines, and the reservation token then binds nothing"
    );
    assert!(
        directory.contains("pub struct ResidentIndex {\n    slots: SpillRows<RESIDENT_ROW_BYTES>,"),
        "the bounded directory rows must stay private to ResidentIndex"
    );
    assert!(
        directory.contains("    retained: bool,\n}"),
        "`ResidentIndex` must keep the `retained` flag as its last field: \"no directory\" \
         and \"empty file\" are different answers and must not collapse into `slots.is_empty()`"
    );
    assert!(
        !directory.contains("impl DerefMut for ResidentIndex")
            && !directory.contains("fn slots_mut")
            && !directory.contains("fn entries_mut")
            && !directory.contains("-> &mut SpillRows<")
            && !directory.contains("-> &mut Vec<RecordIndexEntry>"),
        "`ResidentIndex` must not lend out its `Vec`: a `&mut Vec` is every growth operation \
         at once"
    );
    assert!(
        !directory.contains("Default)]\npub struct ResidentIndex"),
        "`ResidentIndex` must not be `Default`: `core::mem::take` on the field is the bypass \
         that re-verification compiled"
    );
    assert!(
        directory.contains("pub struct ReservedIndexSlot(());"),
        "the reservation token's field must stay private, which is what \
         tests/ui/fail_fabricated_index_reservation.rs asserts"
    );
    assert_eq!(
        directory.matches("self.slots.push_reserved(").count(),
        1,
        "there must be exactly one growth of the record directory in the crate, and it must be \
         `ResidentIndex::install`, which consumes the token"
    );
    assert!(
        source.contains("            _reserved: &ReservedIndexSlot,"),
        "`RecordFile::append_record_at_end` must demand the reservation token: that is what \
         makes the fallible mirror half a precondition of the authoritative write, rather than \
         a comment above it"
    );
    assert!(
        source.contains("self.index.install(index_slot, &entry);"),
        "the single post-append directory install must stay routed through the token"
    );
    // Carried over from the round-12 gate this supersedes: the spelling that
    // grew the mirror directly must not come back, even though the type now
    // refuses it.
    assert!(
        !source.contains("self.index.push("),
        "the append path must install through `ResidentIndex::install`, whose token is \
         produced only by the reservation that runs before the write"
    );
}

/// No file-length ceiling is enforced, and none is required to be declared.
///
/// A `max_file_len` refused a whole file for its size while bounding no
/// allocation the reader actually makes — those are bounded by `Records`,
/// `IndexBytes`, `ScanBytes`, `RecordPayloadLen` and `LogicalPayloadLen`, each
/// with a check that consults it. All twenty-two enforcement sites were
/// removed. The variant and the field remain only so that existing
/// `limits { file_len: .. }` declarations keep parsing.
///
/// This is a source gate rather than a behavioural one because the property is
/// "the code does not do X". A behavioural test would have to guess which
/// operation a reintroduced ceiling was attached to; this cannot miss one.
#[test]
fn no_file_length_ceiling_is_enforced() {
    for module in [
        "file.rs",
        "layout.rs",
        "matrix.rs",
        "stream.rs",
        "indexed.rs",
    ] {
        let source = crate_source(module);
        assert!(
            !source.contains("ReadLimitKey::FileLen"),
            "{module} names `ReadLimitKey::FileLen`. A file-length ceiling bounds no \
             allocation the reader makes; the limits that do are `Records`, `IndexBytes`, \
             `ScanBytes`, `RecordPayloadLen` and `LogicalPayloadLen`."
        );
    }
    let format = crate_source("format.rs");
    assert!(
        format.contains("dead_code,")
            && format.contains("kept so existing `file_len:` declarations parse; inert"),
        "`ReadLimitKey::FileLen` must stay marked as deliberately unconstructed; if that \
         `allow` was removed because the variant became live again, a ceiling came back"
    );
    // And it must not be back on either required-declaration list.
    let file = crate_source("file.rs");
    let layout = crate_source("layout.rs");
    assert!(
        !file.contains("ensure_native_write_limits")
            || !file.contains("        ReadLimitKey::FileLen,"),
        "`ensure_native_write_limits` must not require a `file_len` declaration for a \
         ceiling nothing consults"
    );
    assert!(
        !layout.contains("        ReadLimitKey::FileLen,"),
        "`ensure_layout_open_limits` must not require a `file_len` declaration"
    );
}
