use std::{fs, path::Path};

// Reconstruct extracted child modules with their original scope/indentation.
// Privacy gates still inspect the same logical module; a split must not hide
// declarations or constructor calls from the checks.
pub(crate) fn expanded_source(path: &Path) -> String {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .replace("\r\n", "\n");
    let mut lines = source.lines().peekable();
    let mut out = String::new();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if let Some(relative) = trimmed
            .strip_prefix("#[path = \"")
            .and_then(|v| v.strip_suffix("\"]"))
            .filter(|v| v.contains('/'))
        {
            let declaration = lines.next().expect("a path attribute needs its module");
            assert!(
                declaration.trim().contains("mod ") && declaration.ends_with(';'),
                "{}: unsupported path declaration {declaration}",
                path.display()
            );
            out.push_str(declaration.trim_end_matches(';'));
            out.push_str(" {\n");
            let child = expanded_source(&path.parent().unwrap().join(relative));
            for child_line in child.lines() {
                out.push_str("    ");
                out.push_str(child_line);
                out.push('\n');
            }
            out.push_str("}\n");
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}
