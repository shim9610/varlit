#!/usr/bin/env python3
"""Check documentation structure, links, evidence and the actual quickstart."""
import argparse
import hashlib
from html import unescape
import os
from pathlib import Path
import re
import subprocess
import tempfile
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
MAIN = "https://github.com/shim9610/varlit/blob/main/"


def prose(text):
    return re.sub(r"^```[^\n]*\n.*?^```\s*$", "", text, flags=re.M | re.S)


def anchors(text):
    text = prose(text)
    found = set(re.findall(r'<a\s+(?:id|name)=[\'"]([^\'"]+)', text))
    counts = {}
    for heading in re.findall(r"^#{1,6}\s+(.+?)\s*#*$", text, re.M):
        heading = unescape(re.sub(r"<[^>]*>", "", heading)).lower()
        slug = "".join(c for c in heading if c.isalnum() or c in "-_ ").replace(" ", "-")
        count = counts.get(slug, 0)
        counts[slug] = count + 1
        found.add(slug + (f"-{count}" if count else ""))
    return found


def duplicate_sections(raw):
    stack, seen, duplicates = [], set(), set()

    def finish(end):
        start, _, title = stack.pop()
        section = raw[start:end].strip()
        if len(section) >= 160 and section.count("\n") >= 4:
            if section in seen:
                duplicates.add(title)
            seen.add(section)

    fenced, offset = False, 0
    for line in raw.splitlines(keepends=True):
        if line.startswith("```"):
            fenced = not fenced
        elif not fenced and (match := re.match(r"^(#{1,6})\s+(.+?)\s*$", line)):
            depth = len(match[1])
            while stack and stack[-1][1] >= depth:
                finish(offset)
            stack.append((offset, depth, match[2]))
        offset += len(line)
    while stack:
        finish(len(raw))
    return sorted(duplicates)


def check_file(path, root=ROOT):
    raw = path.read_text()
    errors = []
    if len(re.findall(r"^```", raw, re.M)) % 2:
        errors.append("unclosed code fence")
    errors.extend(f"duplicated section: {title}" for title in duplicate_sections(raw))
    links = re.findall(r"!?\[[^\]\n]*\]\(([^\s)]+)\)", prose(raw))
    checked = 0
    for target in links:
        url = urlsplit(target)
        if target.startswith(MAIN):
            destination = root / unquote(url.path.split("/blob/main/", 1)[1])
        elif url.scheme:
            continue
        else:
            destination = (path.parent / unquote(url.path)).resolve() if url.path else path
        checked += 1
        if not destination.is_file():
            errors.append(f"missing file: {target}")
        elif url.fragment and destination.suffix == ".md":
            if unquote(url.fragment) not in anchors(destination.read_text()):
                errors.append(f"missing anchor: {target}")
    return checked, errors


def execute_quickstart():
    text = (ROOT / "docs/quickstart.md").read_text()
    blocks = re.findall(r"^```rust\n(.*?)^```", text, re.M | re.S)
    if len(blocks) != 1 or "fn main()" not in blocks[0]:
        raise ValueError("quickstart must contain one complete executable Rust example")
    with tempfile.TemporaryDirectory(prefix="varlit-doc-") as temp:
        crate = Path(temp)
        (crate / "src").mkdir()
        (crate / "src/main.rs").write_text(blocks[0])
        dependency = (ROOT / "crates/varlit").as_posix()
        (crate / "Cargo.toml").write_text(
            '[package]\nname="varlit-quickstart-contract"\nversion="0.0.0"\nedition="2024"\n'
            '[workspace]\n[dependencies]\n'
            f'varlit={{path="{dependency}",features=["integrity"]}}\n')
        env = dict(os.environ, CARGO_TARGET_DIR=str(ROOT / "target"))
        subprocess.run(["cargo", "run", "--offline", "--quiet"], cwd=crate, env=env, check=True)


def check_hashes(manifest):
    errors = []
    lines = manifest.read_text().splitlines()
    if not lines:
        errors.append("empty checksum manifest")
    for line in lines:
        parts = line.split()
        if len(parts) != 2 or not re.fullmatch(r"[0-9a-f]{64}", parts[0]):
            errors.append(f"malformed checksum: {line}")
            continue
        expected, name = parts
        path = manifest.parent / name
        if not path.is_file():
            errors.append(f"missing checksum target: {name}")
            continue
        with path.open("rb") as stream:
            if hashlib.file_digest(stream, "sha256").hexdigest() != expected:
                errors.append(f"checksum mismatch: {name}")
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--quickstart", action="store_true")
    args = parser.parse_args()
    files = [ROOT / p for p in subprocess.check_output(
        ["git", "ls-files", "*.md"], cwd=ROOT, text=True).splitlines()
        if (p.startswith("docs/") or p in ("README.md", "CHANGELOG.md", "CONTRIBUTING.md", "SECURITY.md"))
        and (ROOT / p).exists()]
    errors = []
    checked = 0
    for path in files:
        count, issues = check_file(path)
        checked += count
        errors.extend(f"{path.relative_to(ROOT)}: {issue}" for issue in issues)
    for path in (ROOT / "docs").rglob("*SHA256SUMS"):
        errors.extend(f"{path.relative_to(ROOT)}: {issue}" for issue in check_hashes(path))
    if errors:
        raise SystemExit("\n".join(errors))
    print(f"{len(files)} documents, {checked} links checked (external URLs excluded)")
    if args.quickstart:
        execute_quickstart()
        print("Published quickstart compiles, writes and reads its expected sample count")


if __name__ == "__main__":
    main()
