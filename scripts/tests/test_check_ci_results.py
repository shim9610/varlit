import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "check_ci_results", Path(__file__).resolve().parents[1] / "check_ci_results.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class RequiredGateTests(unittest.TestCase):
    def test_all_success_is_accepted(self):
        gate.validate({"test": {"result": "success"}}, {"test"})

    def test_non_success_results_cannot_turn_green(self):
        for result in ["failure", "cancelled", "skipped", None]:
            with self.subTest(result=result), self.assertRaises(ValueError):
                gate.validate({"test": {"result": result}}, {"test"})

    def test_missing_dependency_is_rejected(self):
        with self.assertRaises(ValueError):
            gate.validate({"test": {"result": "success"}}, {"test", "codeql"})

    def test_empty_or_malformed_results_are_rejected(self):
        for results in [{}, [], {"test": "success"}]:
            with self.subTest(results=results), self.assertRaises(ValueError):
                gate.validate(results, {"test"})
