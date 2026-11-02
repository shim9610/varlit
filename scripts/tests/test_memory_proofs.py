import importlib.util
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
spec = importlib.util.spec_from_file_location('memory_proofs', Path(__file__).parents[1]/'check_memory_proofs.py')
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class MemoryProofControls(unittest.TestCase):
    def test_only_the_expected_completed_counterexample_qualifies(self):
        valid = ('Checking harness bad...\nFailed Checks: invalid value\n'
                 'Complete - 0 successfully verified harnesses, 1 failures, 1 total.\n')
        gate.require_failure(valid, 1, 'bad', 'invalid value')
        for code, log in [
            (0, valid), (1, 'compiler panicked: invalid value'),
            (-9, valid), (124, valid), (2, valid),
            (1, valid + valid.splitlines()[-1] + '\n'),
            (1, valid + 'Complete - 1 successfully verified harnesses, 0 failures, 1 total.\n'),
            (1, valid.replace('invalid value', 'unsupported construct')),
            (1, valid.replace('bad...', 'wrong...')),
            (1, valid+'Failed Checks: another defect\n'),
            (1, valid.split('Complete')[0]),
        ]:
            with self.assertRaises(ValueError):
                gate.require_failure(log, code, 'bad', 'invalid value')


if __name__ == '__main__':
    unittest.main()
