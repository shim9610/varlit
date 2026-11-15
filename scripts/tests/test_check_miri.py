"""The Miri gate follows the unsafe boundary: neither may move without the other."""
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("check_miri", Path(__file__).resolve().parents[1] / "check_miri.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)

DENIED = {"lib.rs": "#![deny(unsafe_code)]\n"}


def scope(interpreted=("a.rs",), uninterpreted=("b.rs",)):
    return {
        "interpreted": [{"source": s, "tests": ["a::"], "why": "raw memory"} for s in interpreted],
        "not_interpreted": [{"source": s, "reason": "a system call", "covered_by": "native tests"}
                            for s in uninterpreted],
        "exclusions": [],
    }


class BoundaryTests(unittest.TestCase):
    def test_the_repository_boundary_matches_its_manifest(self):
        import json
        manifest = json.loads(MODULE.SCOPE.read_text())
        roots = {name: (MODULE.ROOT / name).read_text(encoding="utf-8") for name in MODULE.CRATE_ROOTS}
        MODULE.validate_boundary(manifest, MODULE.unsafe_sources(), roots)

    def test_new_unsafe_outside_the_manifest_refuses_the_gate(self):
        MODULE.validate_boundary(scope(), ["a.rs", "b.rs"], DENIED)
        with self.assertRaisesRegex(ValueError, r"undeclared=\['c.rs'\]"):
            MODULE.validate_boundary(scope(), ["a.rs", "b.rs", "c.rs"], DENIED)

    def test_a_manifest_entry_for_code_that_is_gone_refuses_the_gate(self):
        with self.assertRaisesRegex(ValueError, r"stale=\['b.rs'\]"):
            MODULE.validate_boundary(scope(), ["a.rs"], DENIED)

    def test_a_crate_that_stops_denying_unsafe_refuses_the_gate(self):
        for text in ["", "// #![deny(unsafe_code)]\n", "#![warn(unsafe_code)]\n"]:
            with self.assertRaisesRegex(ValueError, "does not deny"):
                MODULE.validate_boundary(scope(), ["a.rs", "b.rs"], {"lib.rs": text})
        MODULE.validate_boundary(scope(), ["a.rs", "b.rs"], {"lib.rs": "#![forbid(unsafe_code)]\n"})

    def test_every_entry_says_what_checks_it(self):
        for broken in [lambda s: s["interpreted"][0].update(tests=[]),
                       lambda s: s["interpreted"][0].update(why=""),
                       lambda s: s["not_interpreted"][0].update(reason=""),
                       lambda s: s["not_interpreted"][0].update(covered_by="")]:
            manifest = scope()
            broken(manifest)
            with self.assertRaises(ValueError):
                MODULE.validate_boundary(manifest, ["a.rs", "b.rs"], DENIED)
        with self.assertRaisesRegex(ValueError, "declared twice"):
            MODULE.validate_boundary(scope(("a.rs",), ("a.rs",)), ["a.rs"], DENIED)

    def test_split_attributes_are_still_found(self):
        self.assertTrue(MODULE.ALLOW.search('#[allow(\n    unsafe_code,\n    reason = "x"\n)]'))
        self.assertTrue(MODULE.ALLOW.search("#![allow(unsafe_code)]"))
        self.assertFalse(MODULE.ALLOW.search("#![deny(unsafe_code)]"))


class SelectionTests(unittest.TestCase):
    def test_ignored_or_duplicate_completion_cannot_count_as_execution(self):
        valid = "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;\n"
        MODULE.validate_completion(valid, ["a", "b"])
        for log in [valid.replace("2 passed", "1 passed").replace("0 ignored", "1 ignored"),
                    valid + valid, "", valid.replace("0 failed", "1 failed")]:
            with self.assertRaises(ValueError):
                MODULE.validate_completion(log, ["a", "b"])
        with self.assertRaises(ValueError):
            MODULE.selected_tests(["a", "a"], ["a"], [])

    def test_only_tests_of_the_unsafe_modules_are_selected(self):
        tests = ["codec::tests::decode", "codec::raw_fill::tests::size", "file::tests::sync",
                 "fallible::tests::clone"]
        self.assertEqual(MODULE.selected_tests(tests, ["codec::", "fallible::"], []),
                         ["codec::tests::decode", "codec::raw_fill::tests::size", "fallible::tests::clone"])

    def test_a_prefix_that_selects_nothing_refuses_the_gate(self):
        with self.assertRaisesRegex(ValueError, "selects no test: renamed::"):
            MODULE.selected_tests(["codec::tests::decode"], ["codec::", "renamed::"], [])

    def test_exclusions_are_exact_documented_and_inside_the_selection(self):
        tests = ["codec::tests::slow", "codec::tests::slower", "file::tests::sync"]
        self.assertEqual(MODULE.selected_tests(tests, ["codec::"], [{"test": "codec::tests::slow", "reason": "cost"}]),
                         ["codec::tests::slower"])
        for exclusions, message in [([{"test": "codec::tests::slow"}], "missing exclusion reason"),
                                    ([{"test": "codec::tests::gone", "reason": "x"}], "stale"),
                                    ([{"test": "file::tests::sync", "reason": "x"}], "stale")]:
            with self.assertRaisesRegex(ValueError, message):
                MODULE.selected_tests(tests, ["codec::"], exclusions)
        with self.assertRaisesRegex(ValueError, "no tests"):
            MODULE.selected_tests(["codec::a"], ["codec::"], [{"test": "codec::a", "reason": "x"}])


if __name__ == "__main__":
    unittest.main()
