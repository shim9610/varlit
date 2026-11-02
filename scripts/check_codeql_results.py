#!/usr/bin/env python3
"""Keep high-severity SARIF findings inside the required CI gate as well as GitHub's alert check."""
import argparse
import json
import math
from pathlib import Path


def validate(report):
    if report.get('version') != '2.1.0' or not report.get('runs'):
        raise ValueError('missing CodeQL SARIF runs')
    findings = []
    for run in report['runs']:
        driver = run['tool']['driver']
        if driver.get('name') != 'CodeQL' or not driver.get('rules'):
            raise ValueError('missing CodeQL rule inventory')
        rules = {rule['id']: rule for rule in driver['rules']}
        if len(rules) != len(driver['rules']):
            raise ValueError('duplicate CodeQL rule identity')
        for invocation in run.get('invocations', []):
            if invocation.get('executionSuccessful') is False:
                raise ValueError('CodeQL execution failed')
            for key in ['toolExecutionNotifications', 'toolConfigurationNotifications']:
                if any(note.get('level') == 'error' for note in invocation.get(key, [])):
                    raise ValueError('CodeQL reported an analysis error')
        if not isinstance(run.get('results'), list):
            raise ValueError('missing CodeQL result list')
        for result in run['results']:
            rule = rules[result['ruleId']]
            severity = float(rule.get('properties', {}).get('security-severity', 0))
            if not math.isfinite(severity) or not 0 <= severity <= 10:
                raise ValueError('invalid security severity')
            if severity >= 7 or result.get('level') == 'error':
                findings.append(result['ruleId'])
    if findings:
        raise ValueError(f'{len(findings)} blocking CodeQL findings: {sorted(set(findings))}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('sarif', type=Path)
    args = parser.parse_args()
    validate(json.loads(args.sarif.read_text()))
    print('CODEQL: no high/critical security findings or error-level results')


if __name__ == '__main__':
    main()
