from copy import deepcopy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("compare_load", Path(__file__).resolve().parents[1] / "compare_load.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def report():
    return {"status": "passed", "platform": "test", "cpu_max": "400000 100000", "memory_max": "max",
            "instrumentation": "none", "total_payload_bytes_per_case": 64, "epoch_bytes": 8,
            "cases": [{"status": "passed", "write_workload": "fixed-reader-work",
                       "config": {"mode": "indexed", "readers": 1, "pinned": 0, "checks_per_epoch": 32},
                       "stages": [{"phase": "write", "status": "passed", "elapsed_seconds": 10,
                                   "command": ["binary", "--phase", "write", "--root", "data", "--cache-bytes", "8388608"],
                                   "user_seconds": 20, "system_seconds": 5, "peak_rss_bytes": 100,
                                   "child_write_bytes": 1000,
                                   "events": [{"event": "reader", "id": 0, "batches": 4, "snapshots": 4,
                                               "trace_hash": 12, "latency": {"count": 128}}]}]}]}


class EqualWorkComparisonTests(unittest.TestCase):
    def test_equal_reports_pass(self):
        self.assertEqual(gate.compare([report()] * 3, [report()] * 3)["status"], "passed")

    def test_32_percent_slowdown_fails(self):
        changed = report()
        changed["cases"][0]["stages"][0]["elapsed_seconds"] *= 1.32
        self.assertEqual(gate.compare([report()] * 3, [changed] * 3)["status"], "failed")

    def test_memory_or_cpu_cost_cannot_hide_behind_equal_wall_time(self):
        for metric in ["peak_rss_bytes", "user_seconds", "child_write_bytes"]:
            changed = report()
            changed["cases"][0]["stages"][0][metric] *= 2
            with self.subTest(metric=metric):
                self.assertEqual(gate.compare([report()] * 3, [changed] * 3)["status"], "failed")
        spikes = [report() for _ in range(3)]
        spikes[-1]["cases"][0]["stages"][0]["peak_rss_bytes"] *= 2
        self.assertEqual(gate.compare([report()] * 3, spikes)["status"], "failed")

    def test_different_work_or_environment_is_refused(self):
        changed = report()
        changed["cases"][0]["write_workload"] = "read-until-writer-finishes"
        trace = report()
        trace["cases"][0]["stages"][0]["events"][0]["trace_hash"] += 1
        count = report()
        count["cases"][0]["stages"][0]["events"][0]["latency"]["count"] -= 1
        quota = report()
        quota["cpu_max"] = "800000 100000"
        cache = report()
        cache["cases"][0]["stages"][0]["command"][-1] = "16777216"
        for candidate in [changed, trace, count, quota, cache]:
            with self.subTest(candidate=candidate), self.assertRaises(ValueError):
                gate.compare([report()] * 3, [candidate] * 3)

    def test_single_or_missing_measurements_are_refused(self):
        with self.assertRaises(ValueError):
            gate.compare([report()], [report()])
        changed = deepcopy(report())
        changed["cases"][0]["stages"][0]["peak_rss_bytes"] = 0
        with self.assertRaises(ValueError):
            gate.compare([report()] * 3, [changed] * 3)
