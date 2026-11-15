"""A noisy control or a zero-storage baseline cannot conceal an exceedance."""
import unittest
from analyze_read_validation import observed_gate


class ReadValidationComparisonContracts(unittest.TestCase):
    def test_stable_control_retains_time_regression(self):
        result = observed_gate(1.0, 1.01, 1.12)
        self.assertEqual(result["status"], "exceeds")
        self.assertTrue(result["observed_exceedance"])
        self.assertGreater(min(result["after_over_before"]), 1.05)

    def test_noisy_control_is_unqualified_and_still_retains_exceedance(self):
        result = observed_gate(1.0, 1.2, 1.3)
        self.assertEqual(result["status"], "unqualified")
        self.assertTrue(result["observed_exceedance"])

    def test_new_temporary_storage_cannot_pass_against_zero(self):
        self.assertEqual(observed_gate(0, 0, 0)["status"], "within")
        result = observed_gate(0, 0, 4096)
        self.assertEqual(result["status"], "exceeds")
        self.assertTrue(result["observed_exceedance"])
        self.assertEqual(result["after_over_before"], [None, None])

    def test_speedup_does_not_qualify_a_noisy_control(self):
        result = observed_gate(1.0, 1.2, .5)
        self.assertEqual(result["status"], "unqualified")
        self.assertFalse(result["observed_exceedance"])


if __name__ == "__main__":
    unittest.main()
