"""A failed load binary must produce a failed report with the original stage."""
from pathlib import Path
from copy import deepcopy
import importlib.util
import json
import platform
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("load_scalable", Path(__file__).resolve().parents[1] / "load_scalable.py")
load = importlib.util.module_from_spec(spec)
spec.loader.exec_module(load)


class FixedReaderWorkTests(unittest.TestCase):
    def setUp(self):
        self.case = {"mode": "indexed", "readers": 1, "pinned": 1, "checks_per_epoch": 32}
        # Captured from the native 64 MiB / 8 MiB-epoch smoke, independently
        # checked by the driver's deterministic key/snapshot oracle.
        self.events = [
            {"event": "reader", "id": 0, "batches": 4, "snapshots": 1,
             "trace_hash": 381263429217691012, "latency": {"count": 128}},
            {"event": "reader", "id": 1, "batches": 4, "snapshots": 4,
             "trace_hash": 6295446422500442997, "latency": {"count": 128}},
        ]

    def validate(self, events):
        load.validate_fixed_readers(self.case, 64 * 1024**2, 8 * 1024**2, events)

    def test_native_pinned_and_following_traces_match(self):
        self.validate(self.events)

    def test_incomplete_or_different_work_is_rejected(self):
        mutants = [self.events[:1], [self.events[0], self.events[0]]]
        for field in ["batches", "snapshots", "trace_hash"]:
            events = deepcopy(self.events)
            events[1][field] -= 1
            mutants.append(events)
        events = deepcopy(self.events)
        events[1]["latency"]["count"] -= 1
        mutants.append(events)
        for events in mutants:
            with self.subTest(events=events), self.assertRaises(ValueError):
                self.validate(events)

    def test_only_write_phase_receives_fixed_work_option(self):
        for phase in ["write", "verify", "point", "random"]:
            command = load.case_command("binary", self.case, "data", phase, 64 * 1024**2, 8 * 1024**2, 1)
            self.assertEqual("--reader-checks-per-epoch" in command, phase == "write")


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
