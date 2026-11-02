import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("vendor_audit", Path(__file__).resolve().parents[1] / "check_vendored_advisories.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class VendoredAdvisoryTests(unittest.TestCase):
    def test_version_is_read_from_vendor_not_hard_coded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / "vendor/redb/Cargo.toml"
            manifest.parent.mkdir(parents=True)
            for version in ["4.4.0", "4.4.1"]:
                manifest.write_text('[package]\nname="redb"\nversion="' + version + '"\n')
                packages = gate.inventory(root)
                self.assertEqual(packages[0]["version"], version)
                lock = gate.tomllib.loads(gate.lockfile(packages))
                self.assertTrue(lock["package"][0]["source"].startswith("registry+"))
                self.assertEqual(lock["package"][0]["version"], version)

    def test_missing_inventory_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ValueError):
                gate.inventory(Path(directory))

    def test_tool_failure_or_incomplete_report_cannot_pass(self):
        for code, output in [(2, ""), (0, "{}"), (0, '{"vulnerabilities":{"found":false,"count":1,"list":[]}}')]:
            result = subprocess.CompletedProcess([], code, output, "synthetic failure")
            with patch.object(gate.subprocess, "run", return_value=result):
                with self.assertRaises(RuntimeError):
                    gate.audit([{"name": "redb", "version": "4.4.0"}])

    def test_informational_warning_is_not_silently_accepted(self):
        output = json.dumps({"vulnerabilities": {"found": False, "count": 0, "list": []}, "warnings": {"unsound": [{"fixture": True}]}})
        result = subprocess.CompletedProcess([], 1, output, "")
        with patch.object(gate.subprocess, "run", return_value=result):
            self.assertEqual(gate.audit([{"name": "redb", "version": "4.4.0"}])["status"], "failed")
