import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("protocol_gate", Path(__file__).parents[1] / "check_publication_protocol.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class ProtocolGateTests(unittest.TestCase):
    def test_compiler_errors_and_timeouts_are_not_detections(self):
        self.assertFalse(gate.detected("compiler error: broken invariant", 1, "broken invariant"))
        self.assertFalse(gate.detected("VERIFICATION:- FAILED\nbroken invariant", 124, "broken invariant"))

    def test_unrelated_assertions_cannot_kill_the_mutant(self):
        log = "Check 1: x\n - Status: SUCCESS\n - Description: broken invariant\nCheck 2: y\n - Status: FAILURE\n - Description: unwinding assertion\nVERIFICATION:- FAILED\nManual Harness Summary:"
        self.assertFalse(gate.detected(log, 1, "broken invariant"))
        self.assertTrue(gate.detected(log.replace("Description: unwinding assertion", "Description: broken invariant"), 1, "broken invariant"))

    def test_each_required_family_has_a_negative_control(self):
        self.assertEqual(len(gate.MUTATIONS), 11)
        self.assertEqual(len({m[0] for m in gate.MUTATIONS}), 11)
        self.assertEqual(len(gate.check_bindings()), 5)
