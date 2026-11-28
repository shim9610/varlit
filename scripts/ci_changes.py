#!/usr/bin/env python3
"""Decide whether a pull request changes documentation only.

A documentation-only change skips the build, test, verification and
sanitizer jobs: they compile and run code the change cannot affect. The jobs
that read documentation still run: `rustfmt` (helper regressions), `rustdoc`
(links, the executable quickstart and README examples, and the tests that
read documents). Any other path, or an unknown change set, runs everything.
"""
import argparse
import os
from pathlib import PurePosixPath
import subprocess
import sys

# Documents only. `docs/verification/` holds machine-read contracts that tests
# consume, so it is code for this purpose whatever the extension.
DOC_SUFFIXES = {".md"}
DOC_ASSET_SUFFIXES = {".svg", ".png"}


def is_documentation(path):
    p = PurePosixPath(path)
    if p.parts[:2] == ("docs", "verification"):
        return False
    if p.suffix.lower() in DOC_SUFFIXES:
        return True
    return p.parts[:1] == ("docs",) and p.suffix.lower() in DOC_ASSET_SUFFIXES


def docs_only(paths):
    paths = [p for p in paths if p]
    return bool(paths) and all(is_documentation(p) for p in paths)


def changed_paths(base, head):
    subprocess.run(["git", "fetch", "--no-tags", "--depth=1", "origin", base],
                   check=True, capture_output=True)
    out = subprocess.run(["git", "diff", "--name-only", base, head],
                         check=True, capture_output=True, text=True).stdout
    return out.splitlines()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="")
    parser.add_argument("--head", default="HEAD")
    args = parser.parse_args()
    result = False
    if args.base:
        try:
            paths = changed_paths(args.base, args.head)
        except subprocess.CalledProcessError as error:
            print(f"cannot list changed files, running everything: {error}", file=sys.stderr)
            paths = []
        result = docs_only(paths)
        print(f"{len(paths)} changed file(s); documentation only: {result}")
        for path in paths:
            print(f"  {'doc ' if is_documentation(path) else 'code'} {path}")
    else:
        print("no base revision (not a pull request): running everything")
    value = "true" if result else "false"
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write(f"docs_only={value}\n")
    else:
        print(f"docs_only={value}")


if __name__ == "__main__":
    main()
