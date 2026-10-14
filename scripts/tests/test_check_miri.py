"""Selection regressions: renames and broad filters must never remove coverage silently."""
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("check_miri", Path(__file__).resolve().parents[1] / "check_miri.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class SelectionTests(unittest.TestCase):
    def test_renamed_exclusion_refuses_the_gate(self):
        with self.assertRaisesRegex(ValueError, "stale or ambiguous"):
            MODULE.selected_tests(["file::renamed"], [{"test": "file::old", "reason": "unsupported"}])

    def test_filter_cannot_also_exclude_a_second_test(self):
        with self.assertRaisesRegex(ValueError, "stale or ambiguous"):
            MODULE.selected_tests(["file::sync", "file::sync_error"], [{"test": "file::sync", "reason": "unsupported"}])

    def test_only_documented_exact_test_is_removed(self):
        self.assertEqual(MODULE.selected_tests(["file::sync", "matrix::page", "codec::decode"],
                         [{"test": "file::sync", "reason": "directory fsync unsupported"}]),
                         ["matrix::page", "codec::decode"])


if __name__ == "__main__":
    unittest.main()
