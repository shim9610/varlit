"""Compile deliberate caller defects; a hash mismatch is not the rejection oracle."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('proof_bindings', Path(__file__).parents[1] / 'check_proof_bindings.py')
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class CallerBindingTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.compiler = ['rustup', 'run', '1.95.0', 'rustc']
        version = subprocess.check_output(cls.compiler + ['--version'], text=True)
        if not version.startswith('rustc 1.95.0 '):
            raise RuntimeError('MIR controls require the pinned rustc 1.95.0: ' + version)

    def check(self, consumer, *, shadow='', rules=True):
        source = '''#![allow(dead_code, unused_must_use, unused_assignments)]
fn protected(x: bool) -> Result<(), ()> { if x { Ok(()) } else { Err(()) } }
#[inline(never)] fn effect() { std::hint::black_box(()); }
'''+shadow+'\nfn consumer(x: bool) -> Result<(), ()> { '+consumer+' }\n'
        with tempfile.TemporaryDirectory() as directory:
            rust = Path(directory) / 'fixture.rs'; mir = Path(directory) / 'fixture.mir'
            rust.write_text(source)
            subprocess.run(self.compiler + ['--edition=2024', '--crate-type=lib', '--emit=mir', str(rust), '-o', str(mir)], check=True, capture_output=True, text=True)
            manifest = dict(contracts={'fixture':dict(symbols=['protected'], result_policy={'protected':'checked-or-propagated'})}, non_contract_resolutions=[])
            observed, parsed = gate.observe(mir.read_text(), manifest)
            self.assertEqual(len(observed['fixture']), 1)
            if rules:
                gate.check_domination(parsed,[dict(caller='consumer(', guard='protected', effects=['effect'])])

    def test_checked_and_renamed_calls_pass(self):
        for code in ['protected(x)?; effect(); Ok(())',
                     'use protected as renamed; renamed(x)?; effect(); Ok(())',
                     'if protected(x).is_err() { return Err(()); } effect(); Ok(())',
                     'if !protected(x).is_ok() { return Err(()); } effect(); Ok(())',
                     'match protected(x) { Err(e) => return Err(e), Ok(()) => () } effect(); Ok(())']:
            with self.subTest(code=code): self.check(code)

    def test_returning_result_is_propagation(self):
        self.check('protected(x)', rules=False)

    def test_semantically_broken_compiled_callers_fail(self):
        mutants = {
            'ignored': ('let _ = protected(x); effect(); Ok(())', 'exits without a check'),
            'overwritten': ('let mut r = protected(x); r = Ok(()); r?; effect(); Ok(())', 'discarded/overwritten|exits without a check'),
            'alias-overwritten': ('let mut r = protected(x); let p = &mut r; *p = Ok(()); r?; effect(); Ok(())', 'mutated through an alias'),
            'raw-alias': ('let mut r = protected(x); let p = &raw mut r; unsafe { *p = Ok(()); } r?; effect(); Ok(())', 'unsupported protected result transformation'),
            'late': ('effect(); protected(x)?; Ok(())', 'effect can bypass verified guard'),
            'conditional-bypass': ('if x { protected(x)?; } effect(); Ok(())', 'effect can bypass verified guard'),
            'ignored-rejection': ('if protected(x).is_err() { std::hint::black_box(()); } effect(); Ok(())', 'rejected guard can still reach effect'),
        }
        for name, (code, reason) in mutants.items():
            with self.subTest(mutant=name), self.assertRaisesRegex(ValueError, reason): self.check(code)

    def test_shadow_resolution_fails(self):
        with self.assertRaisesRegex(ValueError, 'unregistered protected resolution'):
            self.check('shadow::protected(x)?; effect(); Ok(())', shadow='mod shadow { pub fn protected(_:bool)->Result<(),()>{Ok(())} }')

    def test_indirect_protected_call_fails(self):
        with self.assertRaisesRegex(ValueError, 'indirect protected reference'):
            self.check('let f:fn(bool)->Result<(),()> = protected; f(x)?; effect(); Ok(())')

    def test_generic_function_item_dispatch_fails(self):
        with self.assertRaisesRegex(ValueError, 'indirect protected reference'):
            self.check('std::iter::once(x).map(protected).next().unwrap()?; effect(); Ok(())')

    def test_empty_and_unrecognized_mir_fail(self):
        for text in ['', 'fn fixture() {\n}\n']:
            with self.assertRaises(ValueError): gate.functions(text)
        with self.assertRaises(ValueError): gate.successors('unsupported;')

    def test_contract_families_cannot_lose_harnesses(self):
        callers=json.loads(gate.CONTRACTS.read_text())
        proofs=json.loads((gate.ROOT/'docs/verification/proof-contracts.json').read_text())
        self.assertEqual(set(callers['contracts']),{c['id'] for c in proofs['contracts']})
        for c in proofs['contracts']:
            self.assertEqual(c['harnesses'],callers['contracts'][c['id']]['required_harnesses'])
