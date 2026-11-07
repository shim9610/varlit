#!/usr/bin/env python3
"""Audit vendored package identities, which lockfile audits omit for path sources.

The synthetic registry identity is for advisory matching only: no dependency
resolution, registry download, or change to the vendored source is performed.
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def inventory(root=ROOT):
    packages = []
    for manifest in sorted((root / "vendor").glob("*/Cargo.toml")):
        package = tomllib.loads(manifest.read_text())["package"]
        name, version = package["name"], package["version"]
        if not isinstance(name, str) or not isinstance(version, str):
            raise ValueError(f"explicit package name/version required: {manifest}")
        packages.append({"name": name, "version": version,
                         "manifest": str(manifest.relative_to(root))})
    if not packages:
        raise ValueError("no vendored package manifests found")
    return packages


def lockfile(packages):
    return "version = 4\n" + "".join(
        "\n[[package]]\nname = " + json.dumps(p["name"]) +
        "\nversion = " + json.dumps(p["version"]) +
        '\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n'
        for p in packages)


def audit(packages, database=None):
    with tempfile.TemporaryDirectory(prefix="varlit-vendor-audit-") as directory:
        path = Path(directory) / "Cargo.lock"
        path.write_text(lockfile(packages))
        command = ["cargo", "audit", "--file", str(path), "--json", "--deny", "warnings"]
        if database is not None:
            command += ["--db", str(database.resolve()), "--no-fetch"]
        # Isolate repository suppression configuration from this check.
        result = subprocess.run(command, cwd=directory, capture_output=True, text=True, check=False)
        try:
            report = json.loads(result.stdout)
            vulnerabilities = report["vulnerabilities"]
            found = vulnerabilities["found"]
            count = vulnerabilities["count"]
            if not isinstance(found, bool) or not isinstance(count, int):
                raise ValueError("unexpected audit result types")
            if found != bool(count) or count != len(vulnerabilities["list"]):
                raise ValueError("inconsistent audit result")
        except (ValueError, KeyError, TypeError) as error:
            raise RuntimeError(f"cargo audit did not return a complete report (exit {result.returncode}): {result.stderr}") from error
        findings = found or any(report.get("warnings", {}).values())
        if result.returncode not in (0, 1) or bool(result.returncode) != findings:
            raise RuntimeError(f"cargo audit failed without an advisory result: {result.stderr}")
        return {"packages": packages, "status": "failed" if findings else "passed", "audit": report}


def self_test():
    # Exercise cargo-audit's real RustSec/semver matching, not a mocked command.
    with tempfile.TemporaryDirectory(prefix="varlit-advisory-fixture-") as directory:
        database = Path(directory)
        subprocess.run(["git", "init", "-q", str(database)], check=True)
        advisory = database / "crates/redb/RUSTSEC-2026-9999.md"
        advisory.parent.mkdir(parents=True)
        advisory.write_text('''```toml
[advisory]
id = "RUSTSEC-2026-9999"
package = "redb"
date = "2026-11-01"
url = "https://example.invalid/varlit-synthetic-advisory"

[versions]
patched = [">=99.0.0"]
```
# Synthetic advisory for the vendored package negative control

Fixture only; this is not a published redb security advisory.
''')
        bad = audit([{"name": "redb", "version": "4.4.0"}], database)
        good = audit([{"name": "redb", "version": "99.0.0"}], database)
        assert bad["status"] == "failed", "vendored identity was omitted"
        assert bad["audit"]["vulnerabilities"]["list"][0]["advisory"]["id"] == "RUSTSEC-2026-9999"
        assert good["status"] == "passed", "patched version was rejected"
        return {"vulnerable_path_identity_detected": True, "patched_identity_accepted": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", type=Path, help="existing local RustSec database; no fetch")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    report = {"negative_control": self_test()} if args.self_test else audit(inventory(), args.database)
    text = json.dumps(report, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(text)
    print(text, end="")
    return int(report.get("status") == "failed")


if __name__ == "__main__":
    raise SystemExit(main())
