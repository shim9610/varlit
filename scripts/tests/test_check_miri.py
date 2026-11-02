"""Selection regressions: renames and broad filters must never remove coverage silently."""
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("check_miri", Path(__file__).resolve().parents[1] / "check_miri.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class SelectionTests(unittest.TestCase):
    def test_ignored_or_duplicate_completion_cannot_count_as_execution(self):
        valid = "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;\n"
        MODULE.validate_completion(valid, ["a", "b"])
        for log in [valid.replace("2 passed", "1 passed").replace("0 ignored", "1 ignored"),
                    valid + valid, "", valid.replace("0 failed", "1 failed")]:
            with self.assertRaises(ValueError):
                MODULE.validate_completion(log, ["a", "b"])
        with self.assertRaises(ValueError):
            MODULE.selected_tests(["a", "a"], [])

    def test_exclusion_alternatives_cannot_be_missing_or_silently_skipped(self):
        row = dict(test="native", category="workload-cost", alternative="native-and-asan",
                   portable_tests=["portable"], remaining_gap="native I/O remains outside Miri")
        exclusions = [{"test": "native"}]
        MODULE.validate_exclusion_audit(["portable"], exclusions, [row])
        for audit in [[], [row, row], [{**row, "portable_tests": ["excluded"]}],
                      [{**row, "remaining_gap": ""}], [{**row, "category": "opt-in-probe"}]]:
            with self.assertRaises(ValueError):
                MODULE.validate_exclusion_audit(["portable"], exclusions, audit)

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
