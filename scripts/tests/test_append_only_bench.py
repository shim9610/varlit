import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest

BENCH = Path(__file__).resolve().parents[2] / "tools/storage-bench"
sys.path.insert(0, str(BENCH))
try:
    spec = importlib.util.spec_from_file_location("append_only_comparison", BENCH / "compare_append_only.py")
    comparison = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(comparison)
finally:
    sys.path.pop(0)


class AppendOnlyBenchmarkContracts(unittest.TestCase):
    def test_all_variants_preserve_bytes_grouping_concurrency_and_read_policy(self):
        with tempfile.TemporaryDirectory() as temp:
            profiles = comparison.profiles(Path(temp) / 'before', Path(temp) / 'after')
        self.assertEqual(len(profiles), 48)
        workloads = {}
        for p in profiles:
            work = tuple(p[k] for k in ['total', 'group', 'readers', 'producers', 'complex', 'read_validation']) + tuple(p['command_tail'])
            workloads.setdefault(p['workload'], set()).add(work)
            self.assertGreaterEqual(p['group'], 65536)
            self.assertEqual(p['total'] % (16 << 20), 0)
            self.assertEqual(p['read_validation'], 'full')
            self.assertEqual(p['total'] // 128 * 128, p['total'])
        self.assertTrue(all(len(v) == 1 for v in workloads.values()))
        self.assertEqual(len(workloads), 12)

    def test_identical_binary_controls_and_guarded_mode_are_explicit(self):
        profiles = comparison.profiles(Path('before'), Path('after'), smoke=True)
        for workload in {p['workload'] for p in profiles}:
            variants = {p['variant']: p for p in profiles if p['workload'] == workload}
            self.assertEqual(variants['before-a']['binary_override'], variants['before-b']['binary_override'])
            self.assertEqual(variants['before-a']['mode'], 'session-raw')
            self.assertEqual(variants['after-raw']['mode'], 'session-raw')
            self.assertEqual(variants['after-guarded']['mode'], 'session-guarded')
            self.assertEqual(variants['after-raw']['binary_override'], variants['after-guarded']['binary_override'])

    def test_complex_geometry_preserves_publication_bytes(self):
        for group in [65536, 1 << 20]:
            lengths = [group // 2 * n for n in [1, 2, 2, 3]]
            self.assertEqual(sum(lengths), 4 * group)
            self.assertEqual(sum(lengths) * ((16 << 20) // (group * 4)), 16 << 20)
            self.assertTrue(all(n % 128 == 0 and n <= 4 << 20 for n in lengths))

    def test_phase_calibration_keeps_same_binary_and_three_original_workloads(self):
        profiles = comparison.calibration_profiles(Path('candidate'))
        self.assertEqual(len(profiles), 9)
        self.assertEqual(len({p['binary_override'] for p in profiles}), 1)
        workloads = {p['workload'] for p in profiles}
        self.assertEqual(len(workloads), 3)
        for workload in workloads:
            variants = [p for p in profiles if p['workload'] == workload]
            self.assertEqual({p['variant'] for p in variants}, {'raw-a', 'raw-b', 'guarded'})
            self.assertEqual({p['readers'] for p in variants}, {8})
            self.assertEqual({p['read_validation'] for p in variants}, {'full'})

    def test_supplemental_calibration_selects_only_two_new_alerts(self):
        profiles = comparison.calibration_profiles(Path('candidate'), new_alerts=True)
        self.assertEqual(len(profiles), 6)
        self.assertEqual(len({p['binary_override'] for p in profiles}), 1)
        self.assertEqual(len({p['workload'] for p in profiles}), 2)
        for p in profiles:
            self.assertEqual(p['readers'], 8)
            self.assertEqual(p['producers'], 1)
            self.assertIn((p['command_tail'][0], p['total'], p['group']), [
                ('session-live', 1 << 30, 1 << 20),
                ('session-warm', 128 << 20, 65536),
            ])
