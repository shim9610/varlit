#!/usr/bin/env python3
"""Derive CI feature configurations from workspace manifests, never a fixed list."""
import json
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def feature_matrix(manifests):
    singletons = []
    facade = manifests.get("varlit", {}).get("features", {})
    for package, manifest in sorted(manifests.items()):
        if not re.fullmatch(r"[A-Za-z0-9_-]+", package):
            raise ValueError(f"unsafe package name in CI arguments: {package!r}")
        for feature in sorted(manifest.get("features", {})):
            if feature == "default":
                continue
            if not re.fullmatch(r"[A-Za-z0-9_-]+", feature):
                raise ValueError(f"unsafe feature name in CI arguments: {feature!r}")
            # A direct facade forward already exercises that dependency feature.
            # Core-only and macro-only features get their own job automatically.
            if package != "varlit" and f"{package}/{feature}" in facade.get(feature, []):
                continue
            singletons.append({
                "name": f"{package}-{feature}",
                "flags": f"-p {package} --no-default-features --features {feature}",
            })
    return {
        "checks": [
            {"name": "no-default", "flags": "--workspace --no-default-features"},
            {"name": "default", "flags": "--workspace"},
            *singletons,
            {"name": "all-features", "flags": "--workspace --all-features"},
        ],
        "singletons": singletons,
    }


def main():
    # Cargo includes implicit optional-dependency features, workspace globs and
    # renamed/inherited dependencies. Parsing only [features] would miss them.
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--offline", "--no-deps", "--format-version", "1"],
        cwd=ROOT, text=True))
    members = set(metadata["workspace_members"])
    manifests = {package["name"]: package for package in metadata["packages"]
                 if package["id"] in members}
    matrices = feature_matrix(manifests)
    if not matrices["singletons"]:
        raise ValueError("feature discovery unexpectedly found no singletons")
    if destination := os.environ.get("GITHUB_OUTPUT"):
        with open(destination, "a", encoding="utf-8") as output:
            for name, value in matrices.items():
                output.write(f"{name}={json.dumps(value, separators=(',', ':'))}\n")
    print(json.dumps(matrices, indent=2))


if __name__ == "__main__":
    main()
