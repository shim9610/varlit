#!/usr/bin/env python3
"""Run exact production helper bodies against deliberately broken variants."""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import subprocess
import tempfile
from check_publication_protocol import detected
from check_proof_contracts import MANIFEST, validate

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / 'crates/varlit-core/src'


def baseline_harnesses(manifest):
    """Rebase registered production proofs onto the extracted scratch modules."""
    registered = [h for contract in manifest['contracts'] for h in contract['harnesses']]
    prefixes = [
        ('file::resident_index::uniform_prefix_proofs::', 'uniform::uniform_prefix_proofs::'),
        ('compression_buffers::resource_steps::proofs::', 'compression_steps::proofs::'),
        ('spill_file::proofs::', 'spill_file::proofs::'),
        ('disk_index::tree::resource_proofs::', 'tree::resource_proofs::'),
        ('codec::resource_proofs::', 'decoder::resource_proofs::'),
    ]
    expected = []
    for production, scratch in prefixes:
        matches = [scratch + h[len(production):] for h in registered if h.startswith(production)]
        if not matches:
            raise ValueError('missing resource proof family: ' + production)
        expected.extend(matches)
    # The chunk fixture extracts only this production helper and its proof.
    chunk = 'chunks::resource_proofs::full_width_chunk_table_bounds_iteration_by_real_input'
    if registered.count(chunk) != 1:
        raise ValueError('missing or duplicate chunk-table proof')
    expected.append(chunk.replace('::resource_proofs::', '::proofs::'))
    if len(expected) != len(set(expected)):
        raise ValueError('duplicate resource proof contract')
    return expected


def function(source, name):
    start = source.index('fn ' + name + '(')
    opening = source.index('{', start)
    depth = 1
    end = opening + 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[start:end]



def decoder_fixture():
    source = (SRC/'codec.rs').read_text()
    def body(name):
        start = re.search(r'fn ' + name + r'[(<]', source).start()
        opening = source.index('{', start); depth = 1; end = opening + 1
        while depth:
            depth += (source[end] == '{') - (source[end] == '}'); end += 1
        return source[start:end]
    start = source.index('pub struct Decoder<')
    structure = source[start:source.index("impl<'a> Decoder", start)]
    methods = ['new', 'new_limited', 'decode_from_slice_accounted', 'remaining',
               'decode_nested', 'charge_materialization', 'decode_complete',
               'read_exact', 'read_u8', 'read_u64', 'preflight_count',
               'screen_map_wire_length', 'preflight_hash_map_count']
    # new/new_limited also name Encoder methods: search only the Decoder impl.
    encoder_end = source.index("impl<'a> Decoder")
    prefix = source[:encoder_end]; source = source[encoder_end:]
    methods_text = '\n'.join(body(name) for name in methods)
    source = prefix + source
    wire_start = source.index('pub enum WireType')
    wire = source[wire_start:source.index('impl WireType', wire_start)]
    standard = re.search(r'const STANDARD_MATERIALIZATION_LIMIT:[^;]+;', source).group()
    group = re.search(r'const HASH_TABLE_GROUP_BYTES:[^;]+;', source).group() + re.search(r'const MAP_PREALLOCATION_ENTRIES:[^;]+;', source).group()
    return ("use std::collections::HashSet; use std::mem::{size_of,align_of};\n"
            "#[derive(Clone, Copy)] enum Endian { Little, Big }\n"
            "enum Error { UnexpectedEof, InvalidUtf8, TrailingBytes { remaining: usize },"
            "InvalidCanonicalEncoding(&'static str), ResourceArithmeticOverflow { resource: &'static str },"
            "LimitExceeded { resource: &'static str, actual: u64, limit: u64 } }\n"
            "type Result<T> = std::result::Result<T,Error>;\n" + wire + structure +
            "trait VarlitDecode: Sized { const WIRE_TYPE: WireType; fn decode_varlit(d: &mut Decoder<'_>) -> Result<Self>; }\n"
            "impl VarlitDecode for () { const WIRE_TYPE: WireType = WireType::Unit; fn decode_varlit(_: &mut Decoder<'_>) -> Result<Self> { Ok(()) } }\n"
            "impl<'a> Decoder<'a> {" + standard + methods_text + "}\n" + group +
            body('minimum_wire_size') + body('hash_table_reservation_bytes') + body('hash_map_decode_reservation_bytes') +
            '\n#[cfg(kani)] #[path="verification/decoder.rs"] mod resource_proofs;\n')


def bindings():
    checks = [('compression_buffers.rs', 'resource_steps::output_capacity(', 1),
              ('compression_buffers.rs', 'resource_steps::block_extent(', 1),
              ('compression_buffers.rs', 'resource_steps::stream_progress(', 1),
              ('spill_file.rs', 'limited_write_all(&mut self.position', 1),
              ('disk_index/tree.rs', 'take_visit(remaining)', 1),
              ('disk_index/tree.rs', '!height_matches(', 2),
              ('chunks.rs', 'table_extent(header.chunk_count as usize, encoded.len())', 1)]
    for path, text, count in checks:
        if (SRC/path).read_text().count(text) != count:
            raise ValueError(f'production binding missing or ambiguous: {path}: {text}')
    return checks


MUTATIONS = [
    ('decoder-charge-omitted', 'decoder.rs', 'self.materialization_remaining -= bytes;', 'let _ = bytes;',
     'full_width_decoder_charge_preserves_budget_on_failure', 'failed decoder charge replenished or consumed budget'),
    ('zero-width-charge-omitted', 'decoder.rs', 'materialized_entry_bytes.max(1)', 'materialized_entry_bytes',
     'full_width_collection_preflight_bounds_zero_width_work', 'collection preflight differs from wide oracle'),
    ('nested-error-refunds-charge', 'decoder.rs', 'self.materialization_remaining = nested.materialization_remaining;', 'let _ = nested.materialization_remaining;',
     'nested_failure_cannot_refund_a_successful_charge', 'nested error refunded charge'),
    ('accounted-error-refunds-charge', 'decoder.rs', '*materialization_remaining = decoder.materialization_remaining;', 'let _ = decoder.materialization_remaining;',
     'retained_sibling_decodes_share_one_budget_even_after_errors', 'accounted decode reset the shared budget'),
    ('hashmap-overlap-omitted', 'decoder.rs', '?.checked_mul(copies)', '?.checked_mul(1)',
     'hashmap_preflight_accounts_for_overlapping_tables', 'rehash overlap is not charged'),
    ('stream-output-bound-omitted', 'compression_steps.rs', '&& next <= expected', '',
     'streaming_step_is_bounded_and_strictly_progresses', 'stream step acceptance differs from wide oracle'),
    ('stream-stall-accepted', 'compression_steps.rs', '&& (after > before || written != 0)', '',
     'streaming_step_is_bounded_and_strictly_progresses', 'stream step acceptance differs from wide oracle'),
    ('output-ignores-blocks', 'compression_steps.rs', 'actual > bound || ', '',
     'output_requests_respect_both_limits', 'output exceeds complete blocks'),
    ('output-rejects-valid', 'compression_steps.rs', 'Some(actual) => Some(actual as usize)', 'Some(_actual) => None',
     'output_requests_respect_both_limits', 'valid output reservation rejected'),
    ('block-does-not-progress', 'compression_steps.rs', 'offset.checked_add(3)?', 'offset.checked_add(0)?',
     'arbitrary_block_step_progresses_or_rejects', 'block acceptance differs from wide oracle'),
    ('block-size-cap-omitted', 'compression_steps.rs', 'if size > 128 * 1024 {', 'if false {',
     'arbitrary_block_step_progresses_or_rejects', 'block acceptance differs from wide oracle'),
    ('chunk-table-limit-omitted', 'chunks.rs', '(end <= input_len)', '(true)',
     'full_width_chunk_table_bounds_iteration_by_real_input', 'chunk table exceeds real input'),
    ('child-height-not-checked', 'tree.rs', 'expected.is_none_or(|height| height == actual)', 'true',
     'accepted_child_height_strictly_decreases', 'child height did not decrease'),
    ('prefix-truncation-count-lost', 'uniform.rs', 'self.count = count;', 'let _ = count;',
     'full_width_bounded_prefix_truncation_only_removes_rows', 'assertion failed: p.count == count.min'),
    ('visit-fuel-not-consumed', 'tree.rs', '*remaining = remaining.checked_sub(1)?;', 'let _ = remaining.checked_sub(1)?;',
     'every_node_visit_consumes_finite_fuel', 'traversal did not decrease fuel'),
    ('spill-cap-bypassed', 'spill_file.rs', 'check_extent(Some(maximum), end)?;', 'let _ = (maximum, end);',
     'limited_writes_preserve_progress_across_failure_and_retry', 'over-limit write reached backend'),
    ('spill-partial-progress-lost', 'spill_file.rs', '*position += n as u64;', 'let _ = n;',
     'limited_writes_preserve_progress_across_failure_and_retry', 'partial write position lost'),
]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output', type=Path, required=True)
    args = p.parse_args()
    out = args.output.resolve(); out.mkdir(parents=True, exist_ok=True)
    report = dict(bindings=bindings(), mutations=[], sources={})
    expected = baseline_harnesses(json.loads(MANIFEST.read_text()))
    spill = (SRC/'spill_file.rs').read_text()
    directory = (SRC/'native_directory.rs').read_text()
    prefix_start = directory.rfind('#[derive(Debug)]', 0, directory.index('pub(super) struct UniformPrefix'))
    prefix = directory[prefix_start:directory.index('fn invalid_boundary()',prefix_start)]
    originals = {
        'decoder.rs': decoder_fixture(),
        'compression_steps.rs': (SRC/'compression_steps.rs').read_text(),
        'spill_file.rs': 'use std::io;\n'+function(spill,'check_extent')+'\n'+function(spill,'limited_write_all')+'\n#[cfg(kani)]\n#[path="verification/spill.rs"]\nmod proofs;\n',
        'uniform.rs': prefix,
        'chunks.rs': 'const CHUNK_ENTRY_LEN: usize = 16;\nconst CHUNKED_HEADER_LEN: usize = 32;\n'+function((SRC/'chunks.rs').read_text(),'table_extent')+'\n#[cfg(kani)]\nmod proofs { use super::*; #[kani::proof]\n'+function((SRC/'verification/chunks.rs').read_text(),'full_width_chunk_table_bounds_iteration_by_real_input')+'\n}\n',
        'tree.rs': function((SRC/'disk_index/tree.rs').read_text(),'height_matches')+'\n'+function((SRC/'disk_index/tree.rs').read_text(),'take_visit')+'\n#[cfg(kani)]\n#[path="verification/traversal.rs"]\nmod resource_proofs;\n',
    }
    # Only the named full production bodies are extracted; no algorithm model.
    report['sources'] = {k: hashlib.sha256(v.encode()).hexdigest() for k,v in originals.items()}
    env = {**os.environ, 'CARGO_TARGET_DIR': str(ROOT/'target/resource-mutation-build')}
    with tempfile.TemporaryDirectory(prefix='varlit-resource-proofs-') as tmp:
        crate = Path(tmp); (crate/'src/verification').mkdir(parents=True)
        (crate/'Cargo.toml').write_text('[package]\nname="resource-negative-controls"\nversion="0.0.0"\nedition="2024"\n[workspace]\n[lints.rust]\nunexpected_cfgs={level="warn",check-cfg=["cfg(kani)"]}\n')
        (crate/'src/lib.rs').write_text('mod compression_steps;\nmod spill_file;\nmod tree;\nmod chunks;\nmod uniform;\nmod decoder;\n')
        for path, content in originals.items(): (crate/'src'/path).write_text(content)
        for name in ['compression','spill','traversal','uniform_prefix','decoder']:
            (crate/f'src/verification/{name}.rs').write_bytes((SRC/f'verification/{name}.rs').read_bytes())
        cmd = ['cargo','kani','--manifest-path',str(crate/'Cargo.toml'),'--default-unwind','5']
        def run(name, command):
            with (out/(name+'.log')).open('w') as log:
                code = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT, timeout=300).returncode
            return code, (out/(name+'.log')).read_text()
        code, log = run('baseline',cmd)
        if code:
            raise RuntimeError('unmodified resource proofs failed')
        report['baseline_harnesses'] = validate(log, expected)
        for name,path,before,after,harness,assertion in MUTATIONS:
            original = originals[path]
            if original.count(before) != 1: raise ValueError('stale mutation '+name)
            target = crate/'src'/path; target.write_text(original.replace(before,after,1))
            code, log = run(name,cmd+['--harness',harness])
            target.write_text(original)
            killed = detected(log,code,assertion)
            report['mutations'].append(dict(name=name,harness=harness,expected_assertion=assertion,detected=killed,exit_code=code))
            (out/'summary.json').write_text(json.dumps(report,indent=2)+'\n')
            print(f'RESOURCE_MUTATION {name}: {killed}',flush=True)
            if not killed: raise RuntimeError('negative control not detected: '+name)
    print(f'RESOURCE_MUTATIONS {len(MUTATIONS)}/{len(MUTATIONS)} detected')

if __name__ == '__main__': main()
