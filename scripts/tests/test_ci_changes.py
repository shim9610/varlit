import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "ci_changes", Path(__file__).resolve().parents[1] / "ci_changes.py")
changes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(changes)


class DocsOnlyTests(unittest.TestCase):
    def test_markdown_anywhere_is_documentation(self):
        self.assertTrue(changes.docs_only(
            ["README.md", "docs/how-it-works.md", "tools/format-bench/README.md", "CHANGELOG.md"]))

    def test_any_other_file_runs_everything(self):
        for other in ["crates/varlit/src/lib.rs", ".github/workflows/ci.yml",
                      "scripts/check_docs.py", "Cargo.toml", "docs/verification/caller-contracts.json"]:
            with self.subTest(other=other):
                self.assertFalse(changes.docs_only(["README.md", other]))

    def test_verification_records_are_not_documentation(self):
        self.assertFalse(changes.is_documentation("docs/verification/notes.md"))

    def test_doc_images_are_documentation(self):
        self.assertTrue(changes.docs_only(["docs/diagram.svg", "docs/a.md"]))
        self.assertFalse(changes.docs_only(["tools/diagram.svg"]))

    def test_an_empty_change_set_runs_everything(self):
        self.assertFalse(changes.docs_only([]))
        self.assertFalse(changes.docs_only([""]))


if __name__ == "__main__":
    unittest.main()
