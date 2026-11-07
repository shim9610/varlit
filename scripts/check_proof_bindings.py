#!/usr/bin/env python3
"""Compiler-MIR caller inventory and control-flow checks, never a whole-library proof.

Rust 1.95 is pinned because textual MIR is not a stable compiler API. Unknown
syntax, unresolved protected-function references and new caller shapes fail
closed. The source-wide inventory separately covers inactive cfg branches,
macro generators, private bodies and generic definitions.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
CONTRACTS = ROOT / 'docs/verification/caller-contracts.json'


def normalize(text):
    text = re.sub(r'(?:[A-Za-z]:)?/[^\s{}<>]*?/crates/', 'crates/', text)
    text = re.sub(r'(\.rs):\d+:\d+(?:: \d+:\d+)?', r'\1', text)
    return ' '.join(text.split())


def without_generics(text):
    """Remove only ::<...> argument lists, preserving UFCS type/trait owners."""
    while '::<' in text:
        start = text.index('::<'); depth = 1; end = start + 3
        while depth and end < len(text):
            depth += (text[end] == '<') - (text[end] == '>' and text[end-1] != '-')
            end += 1
        if depth:
            raise ValueError('unbalanced generic call target')
        text = text[:start] + text[end:]
    return normalize(text)


def functions(text):
    result = []
    for match in re.finditer(r'^fn ([^\n]+) \{\n', text, re.M):
        end = text.find('\n}', match.end())
        if end == -1:
            raise ValueError('unterminated MIR function')
        signature = match[1]
        body = text[match.end():end]
        blocks = {}
        for bb in re.finditer(r'^    bb(\d+)(?: \(cleanup\))?: \{\n(.*?)^    \}', body, re.M | re.S):
            number = int(bb[1])
            if number in blocks:
                raise ValueError('duplicate basic block')
            blocks[number] = bb[2]
        if not blocks or 0 not in blocks:
            raise ValueError('missing MIR entry block')
        ctfe=text[max(0,match.start()-30):match.start()].rstrip().endswith('// MIR FOR CTFE')
        digest=hashlib.sha256(normalize(body).encode()).hexdigest()[:16]
        identity=normalize(signature)+(' [CTFE]' if ctfe else '')+' [body:'+digest+']'
        # Macro-generated trait impls can print identical source/signatures.
        # Retain every compiler definition, including identical expansions.
        duplicate=sum(f['id'].startswith(identity) for f in result)
        if duplicate: identity+=' [definition:'+str(duplicate+1)+']'
        result.append(dict(id=identity, name=normalize(signature.split('(',1)[0]),
                           signature=signature, body=body, blocks=blocks))
    if not result:
        raise ValueError('empty compiler MIR')
    if len(result)!=len(re.findall(r'^fn ',text,re.M)):
        raise ValueError('unparsed compiler function')
    return result


def successors(body):
    last = body.strip().splitlines()[-1].strip()
    if last in ('return;', 'unreachable;', 'resume;', 'terminate(abi);') or last.startswith('unwind '):
        return set()
    if re.search(r' -> unwind (?:continue|terminate\([^)]*\));$',last):
        return set()  # A diverging call has no normal successor.
    # Success-flow only. Cleanup/unwind edges are outside these domination
    # properties and remain exercised by native failure tests and Kani models.
    flow = re.sub(r'unwind: bb\d+', '', last)
    targets = {int(x) for x in re.findall(r'bb(\d+)', flow)}
    if not targets:
        raise ValueError('unsupported MIR terminator: ' + last)
    return targets


def reachable(blocks, start=0, forbidden=frozenset()):
    seen = set(); pending = [start]
    while pending:
        node = pending.pop()
        if node in seen or node in forbidden:
            continue
        if node not in blocks:
            raise ValueError('MIR edge targets an absent block')
        seen.add(node); pending.extend(successors(blocks[node]))
    return seen


def call(body):
    statement = body.strip().splitlines()[-1].strip()
    if ' -> [return: bb' not in statement or ' = ' not in statement:
        return None
    lhs, rhs = statement.split(' = ',1)
    raw, edge = rhs.rsplit(' -> [return: bb',1)
    # Match the outer call parentheses, including generic tuple/Fn arguments.
    masked = re.sub(r'"(?:\\.|[^"\\])*"', '""', raw)
    if not masked.endswith(')'):
        raise ValueError('unsupported MIR call syntax')
    depth = 1; at = len(masked)-2
    while at >= 0 and depth:
        depth += (masked[at] == ')') - (masked[at] == '(')
        at -= 1
    if depth:
        raise ValueError('unbalanced MIR call')
    opening = at+1
    target = masked[:opening]
    # Recover argument text from the original expression; quoted text is not
    # used for name resolution, only copy/move local dependencies are tracked.
    return dict(target=without_generics(target), destination=lhs,
                arguments=raw, successor=int(re.match(r'\d+',edge)[0]))


def local_refs(text):
    return set(re.findall(r'(?<![A-Za-z0-9])_\d+\b', text))


def guarded_result(function, node, invocation):
    """Require a real checked/propagated use, not a disconnected helper call.

    Track the helper result through standard Result/Option adapters and Try.
    A returned result is a propagated obligation; checked results must reach a
    discriminant switch. The caller inventory and explicit domination rules
    remain separate from the helper's Kani proof.
    """
    destination = invocation['destination']
    if not re.fullmatch(r'_\d+', destination):
        raise ValueError('unsupported guard result place')
    declaration=re.search(r'let (?:mut )?'+re.escape(destination)+r': ([^;]+);',function['body'])
    if not declaration:raise ValueError('guard result type missing')
    typ=declaration[1]
    tag=0 if 'Option<' in typ or typ=='bool' else 1 if 'Result<' in typ else None
    if tag is None:raise ValueError('unknown protected result type '+typ)
    pending=[(invocation['successor'],((destination,tag),),frozenset())]
    visited=set(); checked=0; propagated=0
    while pending:
        bb,initial,ancestors=pending.pop();state=(bb,initial)
        if state in ancestors:
            raise ValueError('guard result cycles before a check: '+function['id'])
        if state in visited:continue
        visited.add(state);tainted=dict(initial);body=function['blocks'][bb]
        nested=call(body)
        for line in body.splitlines():
            if ' = ' not in line or ' -> [return: bb' in line:continue
            lhs,rhs=line.strip().split(' = ',1)
            if not re.fullmatch(r'_\d+',lhs):
                if local_refs(lhs)&tainted.keys():
                    raise ValueError('protected result mutated through an alias: '+function['id'])
                continue
            depends=local_refs(rhs)&tainted.keys();value=tainted[next(iter(depends))] if depends else None
            tainted.pop(lhs,None)
            if depends and (rhs.startswith(('move ','copy ','discriminant('))
                            or re.fullmatch(r'&(?:mut )?_\d+;',rhs)):
                tainted[lhs]=value
            elif depends and rhs.startswith('Not('):tainted[lhs]=1-value
            elif depends:raise ValueError('unsupported protected result transformation: '+rhs)
        if nested:
            depends=local_refs(nested['arguments'])&tainted.keys()
            value=tainted[next(iter(depends))] if depends else None
            tainted.pop(nested['destination'],None)
            if depends:
                target=nested['target']
                if re.search(r' as (?:[A-Za-z_:]+::)?Try>::branch$',target):value=1
                elif re.search(r'Option::(?:ok_or|ok_or_else)$',target):value=1
                elif re.search(r'(?:Option|Result)::(?:map_err|map|and_then)$',target):pass
                elif re.search(r'(?:Option|Result)::(?:is_some|is_ok)$',target):value=0
                elif re.search(r'(?:Option|Result)::(?:is_none|is_err)$',target):value=1
                else:raise ValueError('protected result passed to an unmodeled adapter: '+target)
                tainted[nested['destination']]=value
        if not tainted:
            raise ValueError('protected result discarded/overwritten: '+function['id'])
        switch=re.search(r'switchInt\((?:move|copy) (_\d+)\) -> \[(.*?)\]',body)
        if switch and switch[1] in tainted:
            choices=dict(re.findall(r'(\d+|otherwise): bb(\d+)',switch[2]))
            failure=int(choices.get(str(tainted[switch[1]]),choices.get('otherwise','-1')))
            if failure<0:raise ValueError('guard failure edge missing')
            function.setdefault('guard_failures',{}).setdefault(node,set()).add(failure)
            checked+=1;continue
        edges=successors(body)
        if not edges:
            if body.strip().endswith('return;') and '_0' in tainted:
                propagated+=1;continue
            raise ValueError('protected result exits without a check: '+function['id'])
        pending.extend((edge,tuple(sorted(tainted.items())),ancestors|{state}) for edge in edges)
    return 'checked' if checked else 'propagated'



def observe(text, manifest):
    roots = {symbol: family for family, c in manifest['contracts'].items() for symbol in c['symbols']}
    if not roots:
        raise ValueError('empty protected function set')
    names = {symbol.rsplit('::',1)[-1] for symbol in roots}
    observed = {family:[] for family in manifest['contracts']}
    parsed = functions(text)
    ids = [f['id'] for f in parsed]
    if len(ids) != len(set(ids)):
        raise ValueError('ambiguous compiler function identity')
    for function in parsed:
        live = None
        for bb, body in function['blocks'].items():
            invocation = call(body)
            if not invocation:
                continue
            target = manifest.get('aliases',{}).get(invocation['target'],invocation['target'])
            if target not in roots:
                # A new/shadowed resolution with a protected basename must be
                # classified explicitly rather than mistaken for the helper.
                if target.rsplit('::',1)[-1] in names and target not in manifest['non_contract_resolutions']:
                    raise ValueError('unregistered protected resolution: '+target)
                continue
            if live is None:
                live = reachable(function['blocks'])
            if bb not in live:
                raise ValueError('protected call exists only in unreachable code')
            family = roots[target]
            policy = manifest['contracts'][family].get('result_policy',{}).get(target)
            checked = guarded_result(function,bb,invocation) if policy == 'checked-or-propagated' else 'model/caller-specific'
            observed[family].append(dict(caller=function['id'],target=target,result=checked))
        # Any taking of a protected function address is a separate contract,
        # not a direct call hidden behind a function pointer/dispatch table.
        indirect=set()
        for line in function['body'].splitlines():
            pointer = 'PointerCoercion(ReifyFnPointer' in line
            function_item = False
            for item in re.findall(r'\{([^{}\n]+)\}',line):
                resolved=manifest.get('aliases',{}).get(item,item)
                if resolved.rsplit('::',1)[-1] in names:
                    if resolved not in manifest['non_contract_resolutions']:
                        registered={'caller':function['id'],'target':resolved}
                        if resolved in roots and registered in manifest.get('indirect_contracts',[]):
                            indirect.add(resolved)
                        else: function_item=True
            if function_item or (pointer and any(re.search(r'\b'+re.escape(n)+r'\b',line) for n in names)):
                raise ValueError('indirect protected reference requires a contract: '+function['id'])
        for target in indirect:
            observed[roots[target]].append(dict(caller=function['id'],target=target,result='registered-function-item'))
    for family in observed:
        observed[family].sort(key=lambda x:(x['caller'],x['target'],x['result']))
    return observed,parsed


def check_domination(parsed, rules):
    checked=[]
    for rule in rules:
        matches=[f for f in parsed if rule['caller'] in f['id']]
        if len(matches)!=1:
            raise ValueError('missing/ambiguous domination caller: '+rule['caller'])
        f=matches[0]; calls={bb:call(body) for bb,body in f['blocks'].items()}
        guards={bb for bb,c in calls.items() if c and c['target']==rule['guard']}
        effects={bb for bb,c in calls.items() if c and any(s in c['target'] for s in rule['effects'] if s!='@return-ok')}
        if '@return-ok' in rule['effects']:
            effects.update(bb for bb,body in f['blocks'].items() if re.search(r'_0 = .*::Ok\(',body))
        if not guards or not effects:
            raise ValueError('missing guard/effect in '+f['id'])
        if reachable(f['blocks'],forbidden=guards)&effects:
            raise ValueError('effect can bypass verified guard in '+f['id'])
        failures=set().union(*(f.get('guard_failures',{}).get(g,set()) for g in guards))
        if rule.get('failure_must_not_reach_effect',True):
            if not failures:raise ValueError('guard failure outcome is not modeled in '+f['id'])
            if any(reachable(f['blocks'],start=failed)&effects for failed in failures):
                raise ValueError('rejected guard can still reach effect in '+f['id'])
        checked.append(dict(caller=f['id'],guard=rule['guard'],effects=len(effects),failure_outcomes=len(failures)))
    return checked


def validate(text, manifest, features):
    observed,parsed=observe(text,manifest)
    for family,c in manifest['contracts'].items():
        expected=c['callers'][features]
        if observed[family]!=expected:
            raise ValueError('unreviewed compiler callers for '+family+'; record a caller contract and rerun its proofs, not just the source hash')
    return dict(compiler_functions=len(parsed),protected_calls=sum(map(len,observed.values())),
                callers=observed,domination=check_domination(parsed,manifest['domination'][features]))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mir',type=Path,required=True)
    parser.add_argument('--features',choices=['default','all'],required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--observe',action='store_true',help='Export candidate evidence; never modify the accepted contracts')
    args=parser.parse_args()
    compiler=subprocess.check_output(['rustc','--version'],text=True).strip()
    manifest=json.loads(CONTRACTS.read_text());text=args.mir.read_text()
    if not compiler.startswith('rustc '+manifest['compiler']+' '):
        raise ValueError('textual MIR requires pinned rustc '+manifest['compiler']+': '+compiler)
    result=observe(text,manifest)[0] if args.observe else validate(text,manifest,args.features)
    report=dict(compiler=compiler,features=args.features,mir_sha256=hashlib.sha256(args.mir.read_bytes()).hexdigest(),
                contracts_sha256=hashlib.sha256(CONTRACTS.read_bytes()).hexdigest(),result=result,
                scope='Compiler-resolved caller inventory and selected control-flow obligations; not whole-program semantic refinement.')
    args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps(report,indent=2)+'\n')
    print('PROOF_BINDINGS '+args.features+' '+str(result.get('protected_calls','candidate evidence')))


if __name__=='__main__':main()
