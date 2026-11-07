//! Current release, wire and publication documentation contracts.
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/varlit has a workspace root two levels up")
        .to_path_buf()
}

fn normative_files() -> Vec<(String, String)> {
    let root = repo_root();
    let mut files = Vec::new();
    for name in ["README.md", "CHANGELOG.md"] {
        let path = root.join(name);
        let text = fs::read_to_string(&path).unwrap_or_else(|err| panic!("{name}: {err}"));
        files.push((name.to_string(), text));
    }
    collect(&root.join("docs"), "md", &root, &mut files);
    collect(&root.join("crates"), "rs", &root, &mut files);
    assert!(
        files.len() > 20,
        "the normative file set collapsed to {} entries; the walk is broken and \
         this gate would pass vacuously",
        files.len()
    );
    files
}

fn collect(dir: &Path, extension: &str, root: &Path, out: &mut Vec<(String, String)>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => panic!("read_dir {}: {err}", dir.display()),
    };
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            collect(&path, extension, root, out);
            continue;
        }
        if path.extension().is_none_or(|ext| ext != extension) {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if relative.starts_with("docs/") && relative.contains("review") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap_or_else(|err| panic!("{relative}: {err}"));
        out.push((relative, text));
    }
}

fn shipped_vmat_version() -> u16 {
    let source = fs::read_to_string(repo_root().join("crates/varlit-core/src/matrix.rs"))
        .expect("crates/varlit-core/src/matrix.rs is readable");
    let marker = "const VMAT_VERSION: u16 = ";
    let start = source
        .find(marker)
        .expect("matrix.rs declares const VMAT_VERSION")
        + marker.len();
    let rest = &source[start..];
    let end = rest.find(';').expect("VMAT_VERSION declaration terminates");
    rest[..end]
        .trim()
        .parse()
        .expect("VMAT_VERSION is a u16 literal")
}

fn published_version_claims(
    claims: &[(String, String)],
    prefix: &str,
    suffix: &str,
) -> Vec<(String, usize, u16)> {
    let mut found = Vec::new();
    for (path, text) in claims {
        for (number, line) in text.lines().enumerate() {
            let mut rest = line;
            while let Some(at) = rest.find(prefix) {
                let after = &rest[at + prefix.len()..];
                let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
                let tail = &after[digits.len()..];
                if !digits.is_empty() && tail.starts_with(suffix) {
                    let value = digits.parse().unwrap_or_else(|_| {
                        panic!("{path}:{}: {digits:?} is not a u16", number + 1)
                    });
                    found.push((path.clone(), number + 1, value));
                }
                rest = &rest[at + prefix.len()..];
            }
        }
    }
    found
}

#[test]
fn release_docs_and_installation_match_the_package_version() {
    let root = repo_root();
    let version = env!("CARGO_PKG_VERSION");
    let readme = fs::read_to_string(root.join("README.md")).unwrap();
    assert!(readme.contains(&format!("Version: **{version}**")));
    assert!(readme.contains(&format!("version = \"{version}\"")));
    for (path, heading) in [
        ("CHANGELOG.md", format!("## {version} - ")),
        ("docs/api-changes.md", format!("## {version}\n")),
    ] {
        assert!(
            fs::read_to_string(root.join(path))
                .unwrap()
                .contains(&heading),
            "{path}"
        );
    }
}

#[test]
fn current_wire_claims_match_the_written_vmat_version() {
    let files = normative_files();
    let expected = shipped_vmat_version();
    let mut checked = std::collections::BTreeSet::new();
    for (prefix, suffix) in [
        ("## VMAT Version ", " Header"),
        ("layout_version        u16 = ", ""),
        ("little-endian in layout version ", ""),
        ("`VMAT` layout version ", " stores"),
        ("matrix layout is `VMAT` version ", ""),
        ("| Matrix layout | `VMAT_VERSION` | **", "**"),
    ] {
        for (path, line, value) in published_version_claims(&files, prefix, suffix) {
            assert_eq!(value, expected, "{path}:{line}");
            checked.insert(path);
        }
    }
    for path in ["docs/spec.md", "docs/migration-guide.md"] {
        assert!(checked.contains(path), "missing wire version in {path}");
    }
}

#[test]
fn wire_claim_parser_checks_both_copies_and_accepts_no_missing_numeral() {
    let files = vec![(
        "fixture".into(),
        "field = 4; field = 5; field = missing".into(),
    )];
    let claims = published_version_claims(&files, "field = ", ";");
    assert_eq!(claims.iter().map(|c| c.2).collect::<Vec<_>>(), [4, 5]);
}

#[test]
fn digest_wire_spec_preserves_full_width_sequence_and_presence_flag() {
    let spec = fs::read_to_string(repo_root().join("docs/spec.md")).unwrap();
    assert!(spec.contains("bit 0 (`0x0001`) says the sequence field carries a mark"));
    assert!(spec.contains("**No value is reserved**"));
    let code = fs::read_to_string(repo_root().join("crates/varlit-core/src/file.rs")).unwrap();
    assert!(code.contains("const DIGEST_FLAG_SEQUENCE: u16 = 0x0001;"));
}

#[test]
fn publication_guides_describe_confirmed_generations_and_explicit_follow() {
    for path in [
        "docs/quickstart.md",
        "docs/format-author-guide.md",
        "docs/durability-model.md",
    ] {
        let text = fs::read_to_string(repo_root().join(path)).unwrap();
        assert!(text.contains("confirmed generation"), "{path}");
        assert!(text.contains("follow"), "{path}");
    }
}
