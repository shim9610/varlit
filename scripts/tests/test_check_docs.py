import importlib.util
import hashlib
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("docs", Path(__file__).parents[1] / "check_docs.py")
docs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(docs)


class DocumentationContracts(unittest.TestCase):
    def test_fences_do_not_invent_links_or_anchors(self):
        text = '# Real\n```rust\n# Fake\n[bad](missing.md)\n```\n# Real\n<a id="stable"></a>\n'
        self.assertEqual(docs.anchors(text), {"real", "real-1", "stable"})
        self.assertNotIn("[bad]", docs.prose(text))

    def test_missing_file_and_anchor_both_fail(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            path = root / "README.md"
            (root / "guide.md").write_text('# Working\n')
            path.write_text('[a](missing.md) [b](guide.md#missing) [c](guide.md#working)')
            count, errors = docs.check_file(path, root)
            self.assertEqual(count, 3)
            self.assertEqual(len(errors), 2)
            self.assertTrue(any("missing file" in e for e in errors))
            self.assertTrue(any("missing anchor" in e for e in errors))

    def test_main_branch_links_check_local_checkout(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            path = root / "README.md"
            path.write_text(f'[a]({docs.MAIN}guide.md#working)')
            self.assertTrue(docs.check_file(path, root)[1])
            (root / "guide.md").write_text('# Working\n')
            self.assertFalse(docs.check_file(path, root)[1])
            path.write_text('[external](https://example.invalid/guide)')
            self.assertEqual(docs.check_file(path, root), (0, []))

    def test_unclosed_fence_fails(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "README.md"
            path.write_text('```rust\nfn main() {}\n')
            self.assertEqual(docs.check_file(path)[1], ["unclosed code fence"])

    def test_changed_or_missing_evidence_fails_its_checksum(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            payload = root / "evidence.json"
            payload.write_bytes(b"original")
            manifest = root / "SHA256SUMS"
            manifest.write_text(hashlib.sha256(b"original").hexdigest() + "  evidence.json\n")
            self.assertFalse(docs.check_hashes(manifest))
            payload.write_bytes(b"changed")
            self.assertEqual(docs.check_hashes(manifest), ["checksum mismatch: evidence.json"])
            payload.unlink()
            self.assertEqual(docs.check_hashes(manifest), ["missing checksum target: evidence.json"])

    def test_malformed_checksum_fails(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "SHA256SUMS"
            path.write_text("not-a-checksum\n")
            self.assertTrue(docs.check_hashes(path))
            path.write_text("")
            self.assertEqual(docs.check_hashes(path), ["empty checksum manifest"])
