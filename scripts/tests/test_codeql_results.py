import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('codeql_gate', Path(__file__).parents[1]/'check_codeql_results.py')
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def report(severity='7.5', results=None):
    return {'version': '2.1.0', 'runs': [{'tool': {'driver': {'name': 'CodeQL', 'rules': [
        {'id': 'rust/cleartext-logging', 'properties': {'security-severity': severity}}]}},
        'results': [] if results is None else results}]}


class CodeQLGateTests(unittest.TestCase):
    def test_clean_analysis_passes(self):
        gate.validate(report())

    def test_successful_analysis_does_not_hide_high_findings(self):
        finding = {'ruleId': 'rust/cleartext-logging', 'level': 'warning'}
        for severity in ['7', '7.5', '9', '10']:
            with self.subTest(severity=severity), self.assertRaises(ValueError):
                gate.validate(report(severity, [finding]))
        gate.validate(report('6.9', [finding]))

    def test_suppression_does_not_silently_bypass_required_gate(self):
        with self.assertRaises(ValueError):
            gate.validate(report(results=[{'ruleId': 'rust/cleartext-logging',
                                          'suppressions': [{'status': 'accepted'}]}]))

    def test_absent_or_failed_analysis_is_rejected(self):
        broken = [{}, {'version': '2.1.0', 'runs': []}]
        for field in ['results', 'tool']:
            item = report()
            del item['runs'][0][field]
            broken.append(item)
        for invocation in [{'executionSuccessful': False},
                           {'toolExecutionNotifications': [{'level': 'error'}]}]:
            item = report()
            item['runs'][0]['invocations'] = [invocation]
            broken.append(item)
        for item in broken:
            with self.subTest(item=item), self.assertRaises((ValueError, KeyError)):
                gate.validate(item)

    def test_codeql_pack_rules_in_extensions_are_checked(self):
        # CodeQL 2.27.2 emits an empty driver inventory and per-pack extensions.
        value = report()
        driver = value['runs'][0]['tool']['driver']
        rules = driver.pop('rules')
        driver['rules'] = []
        value['runs'][0]['tool']['extensions'] = [
            {'name': 'codeql/rust-queries', 'rules': rules},
            {'name': 'codeql/rust-all', 'rules': []},
        ]
        gate.validate(value)
        value['runs'][0]['results'] = [{'ruleId': 'rust/cleartext-logging',
            'rule': {'id': 'rust/cleartext-logging', 'toolComponent': {'index': 0}}}]
        with self.assertRaisesRegex(ValueError, 'blocking CodeQL'):
            gate.validate(value)
        value['runs'][0]['results'][0]['rule']['toolComponent']['index'] = 1
        with self.assertRaisesRegex(ValueError, 'unknown or ambiguous'):
            gate.validate(value)
        value['runs'][0]['results'][0]['rule']['toolComponent']['index'] = -1
        with self.assertRaisesRegex(ValueError, 'invalid SARIF'):
            gate.validate(value)

    def test_invalid_metadata_and_error_results_fail_closed(self):
        for severity in ['NaN', 'inf', '-1', '11', 'invalid']:
            with self.subTest(severity=severity), self.assertRaises(ValueError):
                gate.validate(report(severity, [{'ruleId': 'rust/cleartext-logging'}]))
        with self.assertRaises(ValueError):
            gate.validate(report('0', [{'ruleId': 'rust/cleartext-logging', 'level': 'error'}]))
        with self.assertRaises(ValueError):
            gate.validate(report(results=[{'ruleId': 'missing'}]))
