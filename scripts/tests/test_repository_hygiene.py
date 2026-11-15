import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "repository_hygiene", Path(__file__).parents[1] / "check_repository_hygiene.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class RepositoryHygieneTests(unittest.TestCase):
    def test_source_and_reviewed_baselines_are_allowed(self):
        for path in ["formal/closure/Proof.lean", "crates/core/src/lib.rs",
                     "docs/spec.md", "docs/verification/caller-contracts.json",
                     "docs/verification/contracts/record.md",
                     "tools/storage-bench/report.html", "tests/golden.bin"]:
            self.assertEqual(gate.violations(path, 100), [])

    def test_generated_runs_are_rejected_even_when_small(self):
        for path in ["formal-recovery/current.json", "docs/performance/run/report.md",
                     "docs/verification/old-run-20261104/output.json", "backup/current.tar.gz",
                     "backup/current.zip",
                     "compiler/output.ullbc", "compiler/output.mir"]:
            self.assertTrue(gate.violations(path, 100), path)

    def test_size_boundary_is_independent_of_extension(self):
        self.assertEqual(gate.violations("sample.bin", gate.MAX_BLOB_BYTES), [])
        self.assertTrue(gate.violations("sample.bin", gate.MAX_BLOB_BYTES + 1))

    def test_check_uses_staged_blob_not_smaller_worktree_file(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", directory], check=True)
            path = root / "large.bin"
            path.write_bytes(b"x" * (gate.MAX_BLOB_BYTES + 1))
            subprocess.run(["git", "add", "large.bin"], cwd=root, check=True)
            path.write_bytes(b"small")
            count, errors = gate.check(root)
            self.assertEqual(count, 1)
            self.assertTrue(errors)
