"""Guard workload equivalence and conservative concurrency accounting."""
import unittest
import analyze_grouped
import run


class GroupedContracts(unittest.TestCase):
    def test_grouping_preserves_item_bytes_and_publication_work(self):
        shapes = {}
        for p in run.profiles("grouped"):
            c = p["config"]
            items = c["records"] * c["group_items"]
            shape = (items, c["producers"], c["readers"], c["lookups"], c["per_tx"]*c["group_items"])
            work = (c["records"] * c["payload"], c["per_tx"] * c["group_items"],
                    c["records"] // c["per_tx"])
            self.assertEqual(work, shapes.setdefault(shape, work))
            self.assertIn(work[1], [8192,131072])
            self.assertEqual(c["records"] % (c["producers"] * c["per_tx"]), 0)
        for p in run.profiles("grouped-control"):
            c = p["config"]
            self.assertEqual(c["records"] * c["payload"], 128 << 20)
            self.assertEqual(c["per_tx"], 131072)

    def test_overlap_does_not_count_crossing_or_after_writer_bytes(self):
        r = dict(writer_active_start_ns=100, writer_active_end_ns=300,
                 publication_spans=[[1, 100, 130], [2, 150, 180], [3, 200, 250]],
                 config=dict(records=3, payload=128, per_tx=1),
                 readers=[dict(consume_intervals=[[140, 170, 0, 1, 128],
                                                 [250, 350, 1, 2, 128],
                                                 [400, 450, 2, 3, 128]])])
        t = analyze_grouped.telemetry(r)
        self.assertEqual(t["min_reader_during_fraction"], 1 / 3)
        self.assertEqual(t["max_reader_crossing_bytes"], 128)
        self.assertEqual(t["max_reader_after_bytes"], 128)
        self.assertLessEqual(t["delay_p99_lower_ms"], t["delay_p99_upper_ms"])
        # A refresh that starts before the writer window is not fully inside it.
        r["readers"][0]["consume_intervals"][0][0] = 99
        self.assertEqual(analyze_grouped.telemetry(r)["min_reader_during_fraction"], 0)

    def test_missing_publication_evidence_is_rejected(self):
        r = dict(writer_active_start_ns=100, writer_active_end_ns=300,
                 publication_spans=[], config=dict(records=1, payload=128, per_tx=1),
                 readers=[])
        with self.assertRaises(AssertionError):
            analyze_grouped.telemetry(r)


if __name__ == "__main__":
    unittest.main()
