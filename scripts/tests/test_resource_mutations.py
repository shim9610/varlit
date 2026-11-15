import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import check_resource_mutations as gate


class ResourceProofInventoryTests(unittest.TestCase):
    def setUp(self):
        self.manifest = json.loads(gate.MANIFEST.read_text())

    def test_new_candidate_proof_is_required_in_the_extracted_baseline(self):
        expected = gate.baseline_harnesses(self.manifest)
        candidate = ('uniform::uniform_prefix_proofs::'
                     'full_width_sequence_candidate_matches_wide_subtraction')
        self.assertIn(candidate, expected)
        count = len(expected)
        log = ''.join(f'Checking harness {name}...\n' for name in expected)
        complete = f'Complete - {count} successfully verified harnesses, 0 failures, {count} total.\n'
        self.assertEqual(gate.validate(log + complete, expected), expected)
        # Even an otherwise successful completion cannot omit the new proof.
        with self.assertRaises(ValueError):
            gate.validate(log.replace(f'Checking harness {candidate}...\n', '') + complete, expected)

    def test_missing_family_and_duplicate_registration_are_rejected(self):
        for prefix in ['codec::resource_proofs::', 'spill_file::proofs::']:
            manifest = json.loads(json.dumps(self.manifest))
            for contract in manifest['contracts']:
                contract['harnesses'] = [h for h in contract['harnesses'] if not h.startswith(prefix)]
            with self.subTest(prefix=prefix), self.assertRaises(ValueError):
                gate.baseline_harnesses(manifest)
        candidate = next(h for c in self.manifest['contracts'] for h in c['harnesses']
                         if h.endswith('full_width_sequence_candidate_matches_wide_subtraction'))
        self.manifest['contracts'].append({'harnesses': [candidate]})
        with self.assertRaises(ValueError):
            gate.baseline_harnesses(self.manifest)

    def test_extraction_keeps_real_contract_and_body(self):
        source = """// comment
#[cfg_attr(kani, kani::ensures(|r: &Option<usize>| { r.is_some() }))]
pub(super) fn decision(n: usize) -> Option<usize> {
    if n == 0 { None } else { Some(n) }
}

fn unrelated() {}"""
        extracted = gate.function(source, 'decision')
        self.assertTrue(extracted.startswith('#[cfg_attr(kani, kani::ensures('))
        self.assertIn('pub(super) fn decision', extracted)
        self.assertIn('if n == 0 { None } else { Some(n) }', extracted)
        self.assertNotIn('unrelated', extracted)

    def test_executable_contract_and_consumer_cannot_be_omitted(self):
        for prefix in ['chunks::contract_proofs::',
                       'compression_buffers::resource_steps::contract_proofs::',
                       'disk_index::tree::contract_proofs::',
                       'chunks::resource_proofs::incomplete_chunk_table_']:
            manifest = json.loads(json.dumps(self.manifest))
            for family in manifest['contracts']:
                family['harnesses'] = [h for h in family['harnesses'] if not h.startswith(prefix)]
            with self.subTest(prefix=prefix), self.assertRaises(ValueError):
                gate.baseline_harnesses(manifest)


if __name__ == '__main__':
    unittest.main()
