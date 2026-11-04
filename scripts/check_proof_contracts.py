#!/usr/bin/env python3
"""Require the declared proof set, completed verification and source provenance."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / 'docs/verification/proof-contracts.json'


def validate(log, expected):
    log = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", log)
    if not expected or len(expected) != len(set(expected)):
        raise ValueError("empty or duplicate proof contract manifest")
    # Parallel Kani prefixes each harness with its worker ID. Keep the same
    # exact names, uniqueness and final-completion requirements in both modes.
    checked = re.findall(r'^(?:Thread \d+: )?Checking harness (\S+)\.\.\.$', log, re.M)
    if len(checked) != len(set(checked)) or set(checked) != set(expected):
        raise ValueError(f'proof selection mismatch: missing={sorted(set(expected)-set(checked))}, unexpected={sorted(set(checked)-set(expected))}')
    summaries = re.findall(r'^Complete - (\d+) successfully verified harnesses, (\d+) failures, (\d+) total\.$', log, re.M)
    if summaries != [(str(len(expected)), '0', str(len(expected)))]:
        raise ValueError('missing or inconsistent successful completion')
    if ('Status: FAILURE' in log or 'VERIFICATION:- FAILED' in log
            or re.search(r'^(?:Thread \d+: )?Failed Checks:', log, re.M)):
        raise ValueError('failed property in allegedly successful log')
    return checked


def fingerprint(manifest):
    paths = {p for c in manifest['contracts'] for p in c['sources']}
    paths.update(str(p.relative_to(ROOT)) for p in (ROOT/'crates').glob('*/src/**/*.rs'))
    paths.update(str(p.relative_to(ROOT)) for p in (ROOT/'crates').glob('*/Cargo.toml'))
    paths.update(['Cargo.toml', 'Cargo.lock'])
    paths = sorted(paths)
    return dict(source_sha256={p: hashlib.sha256((ROOT/p).read_bytes()).hexdigest() for p in paths},
                manifest_sha256=hashlib.sha256(MANIFEST.read_bytes()).hexdigest())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--log', type=Path)
    parser.add_argument('--capture', type=Path)
    parser.add_argument('--source-before', type=Path)
    parser.add_argument('--features', choices=['default', 'all'])
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    manifest = json.loads(MANIFEST.read_text())
    identity = fingerprint(manifest)
    if args.capture:
        args.capture.write_text(json.dumps(identity, indent=2)+'\n')
        return
    if not all([args.log, args.features, args.output]):
        parser.error('--log, --features and --output are required for validation')
    if args.source_before and json.loads(args.source_before.read_text()) != identity:
        raise ValueError('proof source or contract manifest changed during verification')
    expected = [h for c in manifest['contracts'] for h in c['harnesses']]
    checked = validate(args.log.read_text(), expected)
    def git(*cmd):
        return subprocess.check_output(['git', *cmd], cwd=ROOT, text=True).strip()
    report = dict(head=git('rev-parse', 'HEAD'), tree=git('rev-parse', 'HEAD^{tree}'),
                  dirty=git('status', '--porcelain'), features=args.features,
                  kani=subprocess.check_output(['cargo','kani','--version'], text=True).strip(),
                  harnesses=checked, **identity,
                  source_snapshot='before-and-after' if args.source_before else 'after-only',
                  log_sha256=hashlib.sha256(args.log.read_bytes()).hexdigest())
    args.output.write_text(json.dumps(report, indent=2)+'\n')
    print(f'PROOF_CONTRACTS {len(checked)} executed; features={args.features}')


if __name__ == '__main__':
    main()
