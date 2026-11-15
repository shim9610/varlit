#!/usr/bin/env python3
"""Generate all-target default/all-feature inventories without changing resolution."""
import hashlib
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def main():
    output = ROOT / "target/sbom"
    output.mkdir(parents=True, exist_ok=True)
    lock = ROOT / "Cargo.lock"
    original = lock.read_bytes()
    subprocess.run(["cargo", "metadata", "--locked", "--format-version=1"],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
    # cyclonedx has no --locked switch. Resolve offline and reject any rewrite
    # instead of silently inventorying a different dependency graph.
    env = dict(os.environ, CARGO_NET_OFFLINE="true")
    env["SOURCE_DATE_EPOCH"] = subprocess.check_output(
        ["git", "show", "-s", "--format=%ct", "HEAD"], cwd=ROOT, text=True).strip()
    for profile, features in [("default", []), ("all-features", ["--all-features"])]:
        basename = f"varlit-sbom-{profile}"
        files = [ROOT / "crates" / crate / f"{basename}.json"
                 for crate in ("varlit", "varlit-core", "varlit-macros")]
        files.append(ROOT / "tools/varlit-test-runner" / f"{basename}.json")
        if any(path.exists() for path in files):
            raise SystemExit("Refusing to overwrite an existing SBOM")
        try:
            subprocess.run(["cargo", "cyclonedx", "--format", "json", "--all",
                            "--target", "all", "--spec-version", "1.5",
                            "--override-filename", basename, *features],
                           cwd=ROOT, env=env, check=True)
            if lock.read_bytes() != original:
                raise SystemExit("cyclonedx changed Cargo.lock")
            for path in files[:3]:
                data = json.loads(path.read_text())
                if data.get("bomFormat") != "CycloneDX" or not data.get("components"):
                    raise SystemExit(f"Empty or invalid SBOM: {path}")
                path.rename(output / f"{path.parent.name}-{profile}.cdx.json")
        finally:
            for path in files:
                path.unlink(missing_ok=True)
    (output / "provenance.json").write_text(json.dumps({
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "working_tree_dirty": bool(subprocess.check_output(
            ["git", "status", "--porcelain", "--untracked-files=normal"], cwd=ROOT, text=True).strip()),
        "cargo_lock_sha256": hashlib.sha256(original).hexdigest(),
        "target": "all", "profiles": ["default", "all-features"],
        "includes_build_dependencies": True, "includes_dev_dependencies": False,
        "note": "Dependency inventory; not a compiled binary attestation or vulnerability scan.",
    }, indent=2) + "\n")
    print(f"Six publishable-crate SBOMs written to {output}")


if __name__ == "__main__":
    main()
