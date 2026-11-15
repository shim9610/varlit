import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest

BENCH = Path(__file__).resolve().parents[2] / "tools/storage-bench"
spec = importlib.util.spec_from_file_location("storage_mmap_runner", BENCH / "run.py")
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)
sys.path.insert(0, str(BENCH))
try:
    spec = importlib.util.spec_from_file_location("sqlite_mmap_comparison", BENCH / "compare_sqlite_mmap.py")
    comparison = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(comparison)
finally:
    sys.path.pop(0)


class SqliteMmapComparisonContracts(unittest.TestCase):
    def test_primary_maps_exclude_wal_shm_libraries_and_neighboring_roots(self):
        root = (Path(tempfile.gettempdir()) / 'mmap study').resolve()
        lines = [
            f'1000-3000 r--s 00001000 00:12 19 {root}/samples.bin',
            f'4000-5000 r--s 00003000 00:12 19 {root}/samples.bin (deleted)',
            f'6000-7000 rw-s 00000000 00:12 20 {root}/samples.bin-shm',
            f'8000-9000 r--s 00000000 00:12 21 {root}/samples.bin-wal',
            f'a000-b000 r--s 00000000 00:12 22 {root}-other/samples.bin',
            'c000-d000 r-xp 00000000 00:12 23 /usr/lib/libsqlite3.so',
            'e000-f000 rw-p 00000000 00:00 0',
        ]
        self.assertEqual(list(runner.primary_mapping_extents(lines, root)), [
            (('00:12', '19'), 4096, 12288), (('00:12', '19'), 12288, 16384)])

    def test_grouping_preserves_equal_logical_work_and_large_physical_blocks(self):
        cases = comparison.profiles()
        families = {}
        for p in cases:
            if p['kind'] == 'mixed':
                c = p['config']
                self.assertEqual(c['records'] * c['payload'], 1 << 30)
                self.assertEqual(c['payload'], c['group_items'] * 128)
                self.assertEqual(c['per_tx'] * c['payload'], 16 << 20)
                self.assertGreaterEqual(c['group_items'], 512)
                work = (c['records'], c['payload'], c['group_items'], c['per_tx'],
                        c['readers'], c['producers'], c['lookups'])
            else:
                self.assertGreaterEqual(p['group'], 65536)
                self.assertEqual(p['total'] % p['group'], 0)
                self.assertEqual(p['warm_scan'], not p['live'])
                self.assertEqual(p['points'], 0 if p['live'] else 4096)
                work = tuple(p[k] for k in ('total', 'group', 'readers', 'points', 'ranges', 'live', 'warm_scan'))
            families.setdefault(p['workload'], set()).add(work)
        self.assertTrue(families)
        self.assertTrue(all(len(work) == 1 for work in families.values()))

    def test_live_summary_uses_live_writer_measurement(self):
        row = dict(profile=dict(name='live', kind='array', live=True), probe=False,
                   visibility_gate=False, result=dict(writer_seconds=2, allocated_bytes=4096),
                   usage=dict(elapsed_seconds=3, cpu=4, rss_kib=1024))
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            runner.summarize([row], root)
            self.assertIn('| live | 1 | 2.000000 | 3.000000 | 4.000000 |', (root / 'summary.md').read_text())
