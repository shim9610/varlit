#!/usr/bin/env python3
"""Keep reproducible run artifacts and large blobs out of the tracked tree."""
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
MAX_BLOB_BYTES = 1024 * 1024
OUTPUT_ROOTS = ("formal-recovery/", "benchmark-results/", "performance-results/", "docs/performance/")
GENERATED_SUFFIXES = (".tar", ".tar.gz", ".tar.xz", ".tar.zst", ".tgz", ".zip", ".7z", ".ullbc", ".ullbc.gz", ".llbc", ".mir")


def violations(path, size):
    errors = []
    if path.startswith(OUTPUT_ROOTS) or re.match(r"^docs/verification/[^/]+-20\d{6}/", path):
        errors.append("generated run-output directory")
    if path.endswith(GENERATED_SUFFIXES):
        errors.append("archive or compiler extract")
    if size > MAX_BLOB_BYTES:
        errors.append(f"blob exceeds {MAX_BLOB_BYTES} bytes ({size} bytes)")
    return errors


def check(root=ROOT):
    entries = subprocess.check_output(["git", "ls-files", "--stage", "-z"], cwd=root)
    files = []
    for entry in entries.split(b"\0"):
        if entry:
            metadata, path = entry.split(b"\t", 1)
            _, oid, stage = metadata.split()
            if stage != b"0":
                raise ValueError("unmerged index cannot be checked")
            files.append((oid.decode("ascii"), path.decode("utf-8")))
    output = subprocess.check_output(
        ["git", "cat-file", "--batch-check=%(objectsize)"], cwd=root,
        input="".join(oid + "\n" for oid, _ in files).encode("ascii"),
    ).splitlines()
    if len(output) != len(files):
        raise ValueError("incomplete Git blob-size inventory")
    errors = [f"{path}: {reason}" for (_, path), size in zip(files, output)
              for reason in violations(path, int(size))]
    return len(files), errors


def main():
    count, errors = check()
    if errors:
        raise SystemExit("\n".join(errors))
    print(f"Repository hygiene: {count} tracked files; no run archives or blobs above 1 MiB")


if __name__ == "__main__":
    main()
