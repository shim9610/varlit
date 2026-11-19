//! Rustdoc-placement regression gate for `ReadLimits`' effective-policy
//! accessors.
//!
//! `effective_integrity_verification` once shipped carrying the doc comment of a
//! different method, and nothing failed, because `cargo doc` is happy to
//! publish a summary that is about another function.
//!
//! This test reads the source text, not the compiled crate, for that reason: a
//! "it compiles" or "cargo doc succeeds" check passes both before and after the
//! move and proves nothing.

use std::path::{Path, PathBuf};

#[path = "support/source_tree.rs"]
mod source_tree;

fn format_rs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/varlit has a workspace root two levels up")
        .join("crates/varlit-core/src/format.rs")
}

/// The contiguous run of `///` lines immediately preceding `signature`.
///
/// Returns the empty string when the line above the signature is not a doc
/// comment.
fn doc_block_above(text: &str, signature: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.trim_start().starts_with(signature))
        .unwrap_or_else(|| panic!("`{signature}` not found in format.rs"));
    let mut start = at;
    while start > 0 {
        let candidate = lines[start - 1].trim_start();
        if candidate.starts_with("///") {
            start -= 1;
        } else {
            break;
        }
    }
    lines[start..at].join("\n")
}

#[test]
fn the_effective_policy_accessors_carry_their_own_rustdoc() {
    let path = format_rs();
    // With the files `format.rs` includes, where its `impl` blocks live.
    let text = source_tree::expanded_source(&path);

    let integrity = doc_block_above(&text, "pub const fn effective_integrity_verification");
    assert!(
        integrity.contains("IntegrityVerification::DEFAULT"),
        "`effective_integrity_verification` lost its own summary:\n{integrity}"
    );
}
