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
            "keyed-spill-refusal", "strict-lazy-choice", "strict-lazy-corruption", "strict-lazy-header",
            "compound-read", "failed-swmr", "snapshot-owner-release", "portable-head-parser",
            "ownership-deferred-box", "ownership-box-order", "ownership-inplace-box", "ownership-deferred-arc", "ownership-clone", "ownership-box", "ownership-arc", "ownership-arc-threads",
            "ownership-token", "ownership-preflight", "ownership-published", "ownership-mmap",
            "ownership-generated", "ownership-path-error", "ownership-disk-error",
            "chunk-stream-window", "chunk-stream-refusal", "chunk-stream-integrity", "chunk-stream-fallback",
            "decoder-refusal", "chunk-refusal", "hashmap-rehash-budget", "hostile-codec", "hostile-index", "spill-policy", "spill-byte-policy", "zstd-header-allocation", "chunk-header-allocation", "publication-resume", "publication-clean-resume", "publication-head-pairs", "publication-live-crashes", "index-traversal", "kernel-short-write", "torn-publication", "consistent-frontiers",
            "native-boundary-proof", "rewrite-source-integrity", "matrix-mmap-window",
            "nonresident-record-budget", "nonresident-follow-sequence", "mutation-amortized-work", "recovery-linear-work", "mutation-uniform-proof", "mutation-rewritten-boundaries",
            "checked-chain-links", "decode-memory", "oom-recovery", "zstd-context-oom"})
        for case in gate.CASES:
            command = gate.command(case)
            self.assertIn("--exact", command)
            self.assertIn("--all-features", command)
            self.assertIn("--release", command)
            self.assertEqual("--ignored" in command, case[4])

    def test_feature_disabled_or_renamed_case_is_not_a_pass(self):
        output = "test result: ok. 0 passed; 0 failed;\nVarlit test artifact cleanup: verified empty\n"
        self.assertEqual(gate.classify(0, output, 1)[0], "failed")
