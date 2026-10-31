import importlib.util
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
spec = importlib.util.spec_from_file_location("storage_contracts", Path(__file__).resolve().parents[1] / "check_storage_contracts.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class StorageSelectionTests(unittest.TestCase):
    def test_failure_and_resource_categories_are_required(self):
        self.assertEqual({row[0] for row in gate.CASES}, {
            "kernel-short-write", "torn-publication", "consistent-frontiers",
            "checked-chain-links", "failed-history", "decode-memory", "cardinality-growth"})
        for case in gate.CASES:
            command = gate.command(case)
            self.assertIn("--exact", command)
            self.assertIn("--all-features", command)
            self.assertIn("--release", command)
            self.assertEqual("--ignored" in command, case[4])
        self.assertIn('"keys":1000000', next(row[5] for row in gate.CASES if row[0] == "cardinality-growth"))

    def test_feature_disabled_or_renamed_case_is_not_a_pass(self):
        output = "test result: ok. 0 passed; 0 failed;\nVarlit test artifact cleanup: verified empty\n"
        self.assertEqual(gate.classify(0, output, 1)[0], "failed")
