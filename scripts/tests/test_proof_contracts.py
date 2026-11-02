import importlib.util
from pathlib import Path
import unittest
spec = importlib.util.spec_from_file_location('proof_contracts', Path(__file__).parents[1] / 'check_proof_contracts.py')
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)

class ProofContractTests(unittest.TestCase):
    def test_missing_duplicate_and_partial_proofs_fail(self):
        valid = 'Checking harness a...\nChecking harness b...\nComplete - 2 successfully verified harnesses, 0 failures, 2 total.\n'
        self.assertEqual(gate.validate(valid, ['a','b']), ['a','b'])
        for invalid in [valid.replace('Checking harness b...\n',''), valid.replace('b...', 'a...'), valid.replace('0 failures','1 failures'), valid.split('Complete')[0], valid+'Status: FAILURE\n']:
            with self.assertRaises(ValueError):
                gate.validate(invalid, ['a','b'])

    def test_empty_manifest_is_not_a_proof(self):
        with self.assertRaises(ValueError):
            gate.validate('Complete - 0 successfully verified harnesses, 0 failures, 0 total.\n', [])

    def test_manifest_keeps_the_required_contract_families(self):
        import json
        manifest = json.loads(gate.MANIFEST.read_text())
        contracts = manifest['contracts']
        self.assertTrue({'record-extent', 'observed-prefix', 'publication-metadata',
                         'publication-protocol', 'compression-steps', 'limited-spill',
                         'chunk-parser', 'materialization', 'index-visits', 'fallible-ownership'} <= {c['id'] for c in contracts})
        harnesses = [h for c in contracts for h in c['harnesses']]
        self.assertEqual(len(harnesses), len(set(harnesses)))
        for c in contracts:
            self.assertTrue(c['harnesses'])
            self.assertTrue(c['scope'])
            self.assertTrue(c['limits'])
            for source in c['sources']:
                self.assertTrue((gate.ROOT/source).is_file(), source)

    def test_colored_ci_output_keeps_the_same_selection_checks(self):
        log = '\x1b[32mChecking harness a...\x1b[0m\n\x1b[32mComplete - 1 successfully verified harnesses, 0 failures, 1 total.\x1b[0m\n'
        self.assertEqual(gate.validate(log, ['a']), ['a'])
