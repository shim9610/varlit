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

    def test_docs_only_accepts_skipped_code_jobs(self):
        results = {"changes": {"result": "success"}, "fmt": {"result": "success"},
                   "rustdoc": {"result": "success"}, "test": {"result": "skipped"}}
        gate.validate(results, set(results), docs_only=True)

    def test_docs_only_still_requires_the_documentation_jobs(self):
        for job in sorted(gate.DOCS_JOBS):
            results = {name: {"result": "success"} for name in gate.DOCS_JOBS}
            results["test"] = {"result": "skipped"}
            results[job] = {"result": "skipped"}
            with self.subTest(job=job), self.assertRaises(ValueError):
                gate.validate(results, set(results), docs_only=True)

    def test_docs_only_still_rejects_failed_code_jobs(self):
        results = {name: {"result": "success"} for name in gate.DOCS_JOBS}
        results["test"] = {"result": "failure"}
        with self.assertRaises(ValueError):
            gate.validate(results, set(results), docs_only=True)

    def test_skipped_code_jobs_fail_a_code_change(self):
        results = {name: {"result": "success"} for name in gate.DOCS_JOBS}
        results["test"] = {"result": "skipped"}
        with self.assertRaises(ValueError):
            gate.validate(results, set(results), docs_only=False)

    def test_empty_or_malformed_results_are_rejected(self):
        for results in [{}, [], {"test": "success"}]:
            with self.subTest(results=results), self.assertRaises(ValueError):
                gate.validate(results, {"test"})
