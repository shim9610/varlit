#!/usr/bin/env python3
"""Require ownership harnesses to reject deliberately broken production bodies."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
from check_publication_protocol import detected

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / 'crates/varlit-core/src/fallible.rs'
PROOFS = SRC.with_suffix('') / 'proofs.rs'
MUTATIONS = [
    ('boxed-clone-calls-before-reserving',
     'try_box_with(|| (**self).try_clone())', 'try_box((**self).try_clone()?)',
     'boxed_clone_reserves_before_payload_callback', 'boxed clone callback ran before reservation'),
    ('box-rejects-success', 'if pointer.is_null() {', 'if true {',
     'box_value_refusal_or_unique_ownership', 'box allocation refusal lost'),
    ('box-initializes-before-reserving',
     'let storage = try_box_uninit()?;\n    Ok(Box::write(storage, initialize()?))',
     'let value = initialize()?;\n    let storage = try_box_uninit()?;\n    Ok(Box::write(storage, value))',
     'box_initializer_runs_only_after_reservation', 'initializer ran without reserved storage'),
    ('box-leaks-failed-initializer',
     'Ok(Box::write(storage, initialize()?))',
     'match initialize() { Ok(value) => Ok(Box::write(storage, value)), Err(error) => { std::mem::forget(storage); Err(error) } }',
     'box_initializer_runs_only_after_reservation', 'failed initializer leaked storage'),
    ('arc-initializes-before-reserving',
     'fn try_arc_with<T>(initialize: impl FnOnce() -> Result<T>) -> Result<Arc<T>> {',
     'fn try_arc_with<T>(initialize: impl FnOnce() -> Result<T>) -> Result<Arc<T>> {\n    let initialized = initialize()?;\n    let initialize = || -> Result<T> { Ok(initialized) };',
     'shared_initializer_refusal_and_last_owner_drop', 'shared initializer ran without reserved storage'),
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    # Use exact production functions and harnesses. Only the Error definition
    # is reduced to the three heap-free variants exercised by these controls;
    # registered full-crate proofs separately use the production Error type.
    original = SRC.read_text().split('#[cfg(test)]', 1)[0] + '\n#[cfg(kani)] mod proofs;\n'
    report = dict(sources={str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest()
                           for p in (SRC, PROOFS, Path(__file__).resolve())}, mutations=[])
    env = {**os.environ, 'CARGO_TARGET_DIR': str(out / 'build')}
    with tempfile.TemporaryDirectory(prefix='varlit-ownership-controls-') as tmp:
        crate = Path(tmp)
        (crate / 'src/fallible').mkdir(parents=True)
        (crate / 'Cargo.toml').write_text('''[package]
name="ownership-negative-controls"
version="0.0.0"
edition="2024"
[workspace]
[dependencies]
triomphe={version="=0.1.17",default-features=false,features=["std"]}
[lints.rust]
unexpected_cfgs={level="warn",check-cfg=["cfg(kani)"]}
''')
        (crate / 'src/lib.rs').write_text('''#![cfg_attr(kani, feature(allocator_api))]
#[derive(Debug)] pub enum Error { AllocationFailed { resource: &'static str, requested: u64 }, InvalidMatrixLayout, Io(std::io::Error) }
impl From<std::io::Error> for Error { fn from(e: std::io::Error) -> Self { Self::Io(e) } }
pub type Result<T> = std::result::Result<T, Error>;
mod fallible;
''')
        target = crate / 'src/fallible.rs'
        (crate / 'src/fallible/proofs.rs').write_bytes(PROOFS.read_bytes())
        cmd = ['cargo', 'kani', '--manifest-path', str(crate / 'Cargo.toml'),
               '-Z', 'stubbing', '-Z', 'unstable-options', '--harness-timeout', '300']
        def run(name, harness):
            path = out / (name + '.log')
            with path.open('w') as log:
                code = subprocess.run(cmd + ['--harness', harness], env=env,
                                      stdout=log, stderr=subprocess.STDOUT, timeout=360).returncode
            return code, path.read_text()
        for name, before, after, harness, assertion in MUTATIONS:
            if original.count(before) != 1:
                raise ValueError('stale or ambiguous mutation: ' + name)
            target.write_text(original)
            code, log = run(name + '-baseline', harness)
            if code or 'Complete - 1 successfully verified harnesses, 0 failures, 1 total.' not in log:
                raise RuntimeError('unmodified ownership proof failed: ' + name)
            target.write_text(original.replace(before, after, 1))
            code, log = run(name, harness)
            killed = detected(log, code, assertion)
            report['mutations'].append(dict(name=name, harness=harness, expected_assertion=assertion,
                                            detected=killed, exit_code=code))
            (out / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
            if not killed:
                raise RuntimeError('undetected ownership mutation: ' + name)
    print(f"OWNERSHIP_MUTATIONS {len(report['mutations'])} rejected")


if __name__ == '__main__':
    main()
