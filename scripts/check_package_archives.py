#!/usr/bin/env python3
"""Inspect the three publishable archives after cargo package verifies them."""
import argparse
from pathlib import Path
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, default=ROOT / "target/package")
    args = parser.parse_args()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    for name in ("stratava-core", "stratava-macros", "stratava"):
        stem = f"{name}-{version}"
        with tarfile.open(args.directory / f"{stem}.crate") as archive:
            members = archive.getnames()
            for member in members:
                parts = Path(member).parts
                if any(part in (".internal-docs", "CLAUDE.md", "AGENTS.md") for part in parts):
                    raise SystemExit(f"Private process file in {name}: {member}")
            for required in ("README.md", "LICENSE-MIT", "LICENSE-APACHE"):
                if f"{stem}/{required}" not in members:
                    raise SystemExit(f"Missing {required} in {name}")
            manifest = tomllib.loads(archive.extractfile(f"{stem}/Cargo.toml").read().decode())
            package = manifest["package"]
            if package["name"] != name or package["version"] != version:
                raise SystemExit(f"Unexpected package identity in {name}")
            for table in [manifest, *manifest.get("target", {}).values()]:
                for section in ("dependencies", "build-dependencies"):
                    for dependency, spec in table.get(section, {}).items():
                        actual = spec.get("package", dependency)
                        if actual in ("varve", "varve-core", "varve-macros", "crossbeam") or actual.startswith("crossbeam-"):
                            raise SystemExit(f"Unexpected runtime dependency in {name}: {actual}")
                        if "git" in spec or "path" in spec:
                            raise SystemExit(f"Non-registry runtime dependency in {name}: {actual}")
        print(f"{name} {version}: package identity, notices and public contents verified")


if __name__ == "__main__":
    main()
