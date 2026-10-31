"""A failed load binary must produce a failed report with the original stage."""
from pathlib import Path
import json
import platform
import subprocess
import sys
import tempfile
import unittest


@unittest.skipUnless(platform.system() == "Linux", "Linux load monitor")
class LoadFailureTests(unittest.TestCase):
    def test_failure_before_file_creation_is_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "fail-load"
            binary.write_text("#!/bin/sh\nexit 7\n")
            binary.chmod(0o700)
            result = subprocess.run([
                sys.executable, str(Path(__file__).resolve().parents[1] / "load_scalable.py"),
                "--suite", "smoke", "--only", "raw-smoke", "--total-gib", "0.0625",
                "--epoch-mib", "8", "--binary", str(binary), "--read-seconds", "1",
                "--data-root", str(root / "data"), "--output", str(root / "report"),
            ], capture_output=True, text=True, timeout=20)
            self.assertNotEqual(result.returncode, 0)
            report = json.loads((root / "report/report.json").read_text())
            self.assertEqual(report["status"], "failed")
            self.assertIn("write failed", report["error"])
            self.assertEqual(report["cases"][0]["stages"][0]["returncode"], 7)
