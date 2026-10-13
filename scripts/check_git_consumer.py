#!/usr/bin/env python3
"""Build a standalone Git consumer and reject redb/Crossbeam runtime dependencies."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, default=ROOT)
    args = parser.parse_args()
    repository = args.repository.resolve()
    revision = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=repository, text=True
    ).strip()
    with tempfile.TemporaryDirectory(prefix="stratava-git-consumer-") as directory:
        consumer = Path(directory)
        (consumer / "src").mkdir()
        shutil.copyfile(ROOT / "tools/public-api-fixture/src/main.rs", consumer / "src/main.rs")
        (consumer / "Cargo.toml").write_text(
            '[package]\nname = "stratava-git-consumer"\nversion = "0.0.0"\n'
            'edition = "2024"\npublish = false\n\n[dependencies]\n'
            f'stratava = {{ git = {json.dumps(repository.as_uri())}, '
            f'rev = "{revision}", features = ["compression-zstd", "integrity"] }}\n'
            '\n[workspace]\n', encoding="utf-8"
        )
        # There is deliberately no root [patch] in this independent project.
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--format-version=1"], cwd=consumer
        ))
        for package in metadata["packages"]:
            name = package["name"]
            if name == "redb" or name == "crossbeam" or name.startswith("crossbeam-"):
                raise SystemExit(f"{name} entered the downstream runtime graph")
        print("Git consumer has no redb or Crossbeam runtime dependency", flush=True)
        subprocess.run(["cargo", "run", "--locked"], cwd=consumer, check=True)


if __name__ == "__main__":
    main()
