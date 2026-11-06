#!/usr/bin/env python3
"""Qualify narrow memory checks; report full ownership checks separately as blocked."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from check_proof_contracts import fingerprint, validate, MANIFEST

ROOT = Path(__file__).resolve().parents[1]
REVISION = '947fcec757d9b6a2d07283a0952aaa088e1d8df1'
PREFIX = 'fallible::memory_proofs::'
SUCCESS = PREFIX + 'initialized_scalar_box_and_clone'
REFUSAL = PREFIX + 'refused_scalar_box_does_not_initialize'


def require_failure(log, code, harness, expected):
    """A compiler error, timeout, unsupported check or unrelated UB is not a kill."""
    checks = re.findall(r'^Failed Checks: (.*)$', log, re.M)
    checked = re.findall(r'^Checking harness (\S+)\.\.\.$', log, re.M)
    summary = 'Complete - 0 successfully verified harnesses, 1 failures, 1 total.'
    summaries = re.findall(r'^Complete - .*$', log, re.M)
    if code != 1 or checked != [harness] or summaries != [summary] or checks != [expected]:
        raise ValueError(f'control did not fail for exactly the expected property: {harness}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kani-checkout', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--features', choices=['default', 'all'], default='default')
    parser.add_argument('--diagnose-ownership', action='store_true')
    args = parser.parse_args()
    checkout, out = args.kani_checkout.resolve(), args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    revision = subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], text=True).strip()
    if revision != REVISION:
        raise ValueError('unexpected Kani source revision')
    subprocess.run(['git', '-C', str(checkout), 'diff', '--exit-code', 'HEAD'], check=True)
    env = {**os.environ, 'PATH': str(checkout/'scripts') + os.pathsep + os.environ['PATH'],
           'CARGO_BUILD_JOBS': '4', 'CARGO_INCREMENTAL': '0', 'CARGO_TERM_COLOR': 'never'}
    before = fingerprint(json.loads(MANIFEST.read_text()))
    report = dict(kani_revision=revision, features=args.features, checks=[],
                  whole_ownership_initialization='not-qualified', source_before=before)
    for name, path in [('driver', checkout/'target/kani/bin/kani-driver'),
                       ('compiler', checkout/'target/kani/bin/kani-compiler')]:
        report[name+'_sha256'] = hashlib.sha256(path.read_bytes()).hexdigest()
    report['cbmc'] = subprocess.check_output(['cbmc', '--version'], env=env, text=True).strip()

    def run(name, cmd):
        path = out/(name+'.log')
        with path.open('w') as log:
            code = subprocess.run(cmd, cwd=ROOT, env=env, stdout=log,
                                  stderr=subprocess.STDOUT, timeout=600).returncode
        contents = path.read_text()
        report['checks'].append(dict(name=name, command=cmd, exit_code=code,
                                     log_sha256=hashlib.sha256(path.read_bytes()).hexdigest()))
        (out/'report.json').write_text(json.dumps(report, indent=2)+'\n')
        return code, contents

    fixture = ROOT/'scripts/fixtures/kani_memory_controls.rs'
    report['control_sha256'] = hashlib.sha256(fixture.read_bytes()).hexdigest()
    control_copy = out/'kani_memory_controls.rs'
    control_copy.write_bytes(fixture.read_bytes())
    control = ['kani', str(control_copy),
               '-Z', 'unstable-options', '--harness-timeout', '300', '--exact']
    cases = [
        ('allocated_value', 'valid-value-checks', None),
        ('null_wide_pointer', 'valid-value-checks',
         'Undefined Behavior: Invalid value of type `std::ptr::NonNull<[u8]>`'),
        ('initialized_reference', 'uninit-checks', None),
        ('uninitialized_reference', 'uninit-checks',
         'Undefined Behavior: Reading from an uninitialized pointer of type `*const u8`'),
        ('slice_index_with_unreachable_indirect_panic', 'uninit-checks', None),
        ('reachable_indirect_call', 'uninit-checks',
         'Kani does not support memory initialization checks across calls through function pointers. '
         'For more information about planned support, see https://github.com/model-checking/kani/issues/3300.'),
    ]
    for harness, flag, expected in cases:
        code, log = run('control-'+harness, control+['--harness', harness, '-Z', flag])
        if expected:
            require_failure(log, code, harness, expected)
        else:
            if code:
                raise ValueError('positive control failed: '+harness)
            validate(log, [harness])
    cargo = ['cargo', 'kani', '-p', 'varlit-core', '-Z', 'stubbing', '-Z', 'unstable-options', '-Z', 'function-contracts',
             '--harness-timeout', '300', '--exact']
    if args.features == 'all':
        cargo += ['--all-features']
    for label, flags, harnesses in [
        ('scalar-validity', ['-Z', 'valid-value-checks'], [SUCCESS, REFUSAL]),
        ('refusal-initialization', ['-Z', 'valid-value-checks', '-Z', 'uninit-checks'], [REFUSAL]),
    ]:
        selection = [item for h in harnesses for item in ['--harness', h]]
        code, log = run(label, cargo+flags+selection)
        if code:
            raise ValueError('memory proof failed: '+label)
        validate(log, harnesses)
    report['qualified'] = dict(validity=[SUCCESS, REFUSAL], initialization=[REFUSAL])
    if args.diagnose_ownership:
        # These are the original full production harnesses, with the same
        # assertions and allocator model. An unsupported property is NEVER
        # reclassified as a successful proof. Save every failure for review.
        harnesses = ['box_value_refusal_or_unique_ownership',
                     'byte_clone_preserves_source_on_every_refusal',
                     'shared_initializer_refusal_and_last_owner_drop',
                     'copy_on_write_refusal_preserves_shared_source']
        selection = [item for h in harnesses for item in ['--harness', 'fallible::proofs::'+h]]
        code, log = run('ownership-diagnostic', cargo+['-Z', 'valid-value-checks', '-Z', 'uninit-checks']+selection)
        completed = re.findall(r'^Complete - (\d+) successfully verified harnesses, (\d+) failures, (\d+) total\.$', log, re.M)
        if len(completed) != 1 or int(completed[0][2]) != 4:
            raise ValueError('ownership diagnostic did not complete (crash or timeout)')
        selected = re.findall(r'^Checking harness (\S+)\.\.\.$', log, re.M)
        if sorted(selected) != sorted('fallible::proofs::'+h for h in harnesses):
            raise ValueError('ownership diagnostic selection changed')
        report['ownership_diagnostic'] = dict(exit_code=code, completion=completed[0],
            failed_checks=re.findall(r'^Failed Checks: (.*)$', log, re.M))
        report['whole_ownership_initialization'] = 'blocked' if code else 'selected-four-passed'
    if fingerprint(json.loads(MANIFEST.read_text())) != before:
        raise ValueError('production/proof sources changed during memory checks')
    report['source_after_matches'] = True
    (out/'report.json').write_text(json.dumps(report, indent=2)+'\n')
    print('MEMORY_PROOFS: 2 scalar validity, 1 refusal initialization; 6 controls qualified')
    print('FULL OWNERSHIP INITIALIZATION: '+report['whole_ownership_initialization'])
    summary = os.environ.get('GITHUB_STEP_SUMMARY')
    if summary:
        with open(summary, 'a') as output:
            output.write('\nExtra memory instrumentation: two scalar validity harnesses and one '
                         'complete-refusal initialization harness passed. Full Box/TryClone/Arc '
                         'initialization is **'+report['whole_ownership_initialization']+'**; '
                         'this job does not certify it. See memory-checks/report.json.\n')
    # A requested full diagnostic that remains blocked must be visibly red,
    # even though the separately qualified scalar/control suite passed.
    return 1 if report['whole_ownership_initialization'] == 'blocked' else 0


if __name__ == '__main__':
    sys.exit(main())
