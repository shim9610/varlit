#!/usr/bin/env python3
"""Close registered state writers over compiler MIR; no path/hash allowlist.

This gate checks mutation coverage and callback routing. Kani separately proves
registered transition semantics. It is not a whole-Rust alias analysis: new raw
state pointers and unmodeled mutable-field calls fail rather than being ignored.
"""
import argparse
import json
from pathlib import Path
import re
import subprocess
from check_proof_bindings import functions, call, local_refs, guarded_result, reachable, normalize

ROOT=Path(__file__).resolve().parents[1]
MANIFEST=ROOT/'docs/verification/state-transitions.json'


def types(function):
    # rustc prints parameters in the signature and local declarations in body.
    result={}
    for local,typ in re.findall(r'(_\d+): ([^;\n]+);',function['body']):
        result[local]=typ
    for local,typ in re.findall(r'(_\d+): (.*?)(?=, _\d+: |\) -> )',function['signature']):
        result[local]=typ
    result['_0']=function['signature'].split(' -> ',1)[1]
    return result


def owner_type(typ,owners):
    plain=re.sub(r"&(?:'[\w_]+ )?(?:mut )?",'',typ).strip()
    for key,owner in owners.items():
        if any(plain==name or plain.startswith(name+'<') for name in [owner['type']]+owner.get('aliases',[])):
            return key
    return None


def method_name(name):
    return name  # already normalized by the pinned compiler-MIR parser


def check_transition_binding(f, owner, rule):
    """A write must use the exact proved helper result with the real inputs.

    Only copy/move/variant projection and standard Option/Result propagation
    preserve this fact. Arithmetic, overwrite and unknown transformations fail.
    """
    spec=rule.get('transition')
    if not spec:return
    env={local:{'param:'+local[1:]} for local in types(f)}
    stores=[];seen=0
    def origin(rhs):
        fields=re.findall(r'\(\(\*_1\)\.(\d+):',rhs)
        if fields and rhs.startswith(('copy ','move ')):
            return {'field:'+owner['fields'][int(fields[0])]}
        if rhs.startswith('PtrMetadata('):
            refs=local_refs(rhs)
            return {'length:'+tag for ref in refs for tag in env.get(ref,{'unknown'})}
        if rhs.startswith(('copy ','move ')):
            refs=local_refs(rhs)
            if len(refs)==1:return env.get(next(iter(refs)),{'unknown'})
        return {'unknown'}
    for body in f['blocks'].values():
        invocation=call(body)
        for line in body.splitlines():
            pair=re.match(r'\s*(.+?) = (.*);$',line)
            if not pair:continue
            lhs,rhs=pair.groups()
            if re.search(r' -> (?:\[return:|unwind )',rhs):
                if invocation['target'] in (spec['helper'],'decode_state::'+spec['helper']):
                    raw=invocation['arguments'].rsplit('(',1)[1][:-1]
                    actual=[origin(arg.strip()) for arg in raw.split(',')]
                    expected=[{tag} for tag in spec['arguments']]
                    if actual!=expected:raise ValueError('transition uses substituted inputs: '+f['name'])
                    env[lhs]={'proved-result'};seen+=1
                elif any(name in invocation['target'] for name in ('::ok_or','::branch','::from_output')):
                    refs=local_refs(invocation['arguments']);tags=set().union(*(env.get(r,{'unknown'}) for r in refs))
                    env[lhs]={'proved-result'} if 'proved-result' in tags else {'unknown'}
                else:env[lhs]={'unknown'}
            elif re.fullmatch(r'_\d+',lhs):env[lhs]=origin(rhs)
            elif re.search(r'\(\(\*_1\)\.'+str(owner['fields'].index(spec['field']))+r':',lhs):
                stores.append(origin(rhs))
    if seen!=1 or not stores or any(tags!={'proved-result'} for tags in stores):
        raise ValueError('state write is disconnected from its proved transition: '+f['name'])


def check_initializers(f, owners, owner_key, rule):
    spec=rule.get('initializes')
    if not spec:return
    owner=owners[owner_key];source=owners[spec.get('source_owner',owner_key)]
    definitions={}
    for bb,body in f['blocks'].items():
        for index,line in enumerate(body.splitlines()):
            pair=re.match(r'\s*(_\d+) = (.*);$',line)
            if pair:definitions.setdefault(pair[1],[]).append((bb,index,pair[2]))
    params=set(re.findall(r'(_\d+): ',f['signature']))
    def resolve(value,bb,index,seen=frozenset()):
        value=re.sub(r'^(?:copy|move) ','',value.strip())
        if re.fullmatch(r'_\d+',value):
            if value in params:return 'param:'+value[1:]
            if value in seen:return 'unknown'
            choices=[d for d in definitions.get(value,[]) if d[0]==bb and d[1]<index]
            if choices:chosen=max(choices,key=lambda d:d[1])
            elif len(definitions.get(value,[]))==1:chosen=definitions[value][0]
            else:return 'entry:'+str(bb)+':'+value
            return resolve(chosen[2],chosen[0],chosen[1],seen|{value})
        field=re.fullmatch(r'\(\(\*_1\)\.(\d+): .+\)',value)
        if field:return 'self:'+source['fields'][int(field[1])]
        if value.startswith('PtrMetadata('):return 'length:'+resolve(value[12:-1],bb,index,seen)
        if value.endswith('::None'):return 'none'
        some=re.fullmatch(r'Option::<.*>::Some\((.*)\)',value)
        if some:return 'some:'+resolve(some[1],bb,index,seen)
        borrow=re.fullmatch(r'&(_\d+)',value)
        if borrow and borrow[1] not in params:return 'borrow-local:'+types(f).get(borrow[1],'unknown')
        return value
    count=0
    for bb,body in f['blocks'].items():
        for index,line in enumerate(body.splitlines()):
            literal=re.search(r'= (?:'+re.escape(owner['type'])+'|'+re.escape(owner.get('aliases',[owner['type']])[0])+r')(?:::<.*>)? \{ (.*) \};',line)
            if not literal:continue
            fields=dict(part.split(': ',1) for part in re.split(r', (?=[A-Za-z_]\w*: )',literal[1]))
            values={name:resolve(value,bb,index) for name,value in fields.items()}
            for name,expected in spec.get('values',{}).items():
                if values.get(name)!=expected:raise ValueError('unproved state initializer: '+f['name']+' '+name)
            for first,second in spec.get('equal',[]):
                if values[first]!=values[second]:raise ValueError('constructor invents budget: '+f['name'])
            count+=1
    if count!=1:raise ValueError('missing/ambiguous state initializer: '+f['name'])


def check_callback_commits(f, rule):
    if not rule.get('guarded_commit'):return
    guards={bb:call(body) for bb,body in f['blocks'].items()
            if call(body) and call(body)['target'] in (rule.get('guard','callback_exit'),'decode_state::'+rule.get('guard','callback_exit'))}
    if len(guards)!=1:raise ValueError('callback commit requires one verified exit guard')
    for bb,invocation in guards.items():guarded_result(f,bb,invocation)
    definitions={}
    for body in f['blocks'].values():
        for line in body.splitlines():
            match=re.match(r'\s*(_\d+) = (.*);$',line)
            if match:definitions.setdefault(match[1],[]).append(match[2])
    def source(rhs,seen=frozenset()):
        rhs=re.sub(r'^(?:copy|move) ','',rhs)
        if re.fullmatch(r'_\d+',rhs) and rhs not in seen and len(definitions.get(rhs,[]))==1:
            return source(definitions[rhs][0],seen|{rhs})
        return rhs
    effects=set()
    for bb,body in f['blocks'].items():
        invocation=call(body)
        if rule['guarded_commit']=='forget' and invocation and invocation['target'].endswith('::forget'):effects.add(bb)
        for line in body.splitlines():
            pair=re.match(r'\s*(.+?) = (.*);$',line)
            if not pair:continue
            lhs,rhs=pair.groups()
            if rule['guarded_commit']=='forget':
                invocation=call(body)
                if invocation and invocation['target'].endswith('::forget'):effects.add(bb)
            elif re.search(r'\(\(\*_1\)\.(?:2|6):',lhs):
                if rhs=='const 0_u64':continue
                resolved=source(rhs)
                if resolved.startswith('PtrMetadata('):
                    refs=local_refs(resolved)
                    if len(refs)==1 and '((*_1).1:' in source(next(iter(refs))):continue
                    raise ValueError('callback unwind cursor is not the original input end')
                expected=2 if '.2:' in lhs else 6
                if not re.fullmatch(r'\(_\d+\.'+str(expected)+r': (?:usize|u64)\)',resolved):
                    raise ValueError('callback commit is disconnected from returned state')
                effects.add(bb)
    if not effects:raise ValueError('callback exit has no committed state')
    if reachable(f['blocks'],forbidden=set(guards))&effects:
        raise ValueError('callback state commit bypasses verified admission')
    failures=set().union(*(f.get('guard_failures',{}).get(bb,set()) for bb in guards))
    if not failures or any(reachable(f['blocks'],start=bb)&effects for bb in failures):
        raise ValueError('rejected callback can commit state')


def inspect(text,manifest,proofs):
    owners=manifest['owners'];known={h for c in proofs['contracts'] for h in c['harnesses']}
    for key,owner in owners.items():
        for name,rule in owner['writers'].items():
            if not rule['proofs'] or not set(rule['proofs'])<=known:
                raise ValueError('writer has missing transition proofs: '+name)
    observed={key:{} for key in owners};writer_defs={};dispatch=0;leases=0
    parsed=functions(text)
    evidence_constants=0
    if 'builtin_evidence' in manifest:
        # CTFE bodies are not functions. Evidence may only be constructed in a
        # codec's associated constant, never in a factory or unrelated constant.
        spans=[]
        for m in re.finditer(r'^const ([^\n]+) = \{\n',text,re.M):
            end=text.find('\n}',m.end())
            if end<0:raise ValueError('unterminated evidence constant')
            body=text[m.end():end]
            if re.search(r'BuiltinCodec(?:::<.*>)? \{',body):
                if not re.fullmatch(r'codec::<impl at crates/varlit-core/src/codec.rs>::__VARLIT_BUILTIN: Option<BuiltinCodec<.*>>',normalize(m[1])):
                    raise ValueError('builtin evidence constructed outside a codec associated constant')
                evidence_constants+=1
            spans.append((m.start(),end+2))
        # Every evidence literal must belong to one of those constants or to a
        # function below (where the ordinary state-writer closure rejects it).
        function_spans=[(m.start(),text.find('\n}',m.end())+2) for m in re.finditer(r'^fn [^\n]+ \{\n',text,re.M)]
        for m in re.finditer(r'BuiltinCodec(?:::<[^\n]*>)? \{',text):
            if not any(a<=m.start()<b for a,b in spans+function_spans):
                raise ValueError('unparsed builtin evidence construction')
        if not evidence_constants:raise ValueError('builtin evidence constants disappeared')
    mutable_targets=set()
    for f in parsed:
        ts=types(f)
        for local,typ in ts.items():
            own=owner_type(typ,owners)
            if local in re.findall(r'(_\d+): ',f['signature']) and own and '&mut ' in typ:
                mutable_targets.add(f['name'])
                if '<impl at ' in f['name']:
                    method=f['name'].rsplit('::',1)[-1]
                    for alias in [owners[own]['type']]+owners[own].get('aliases',[]):
                        mutable_targets.add(alias+'::'+method)
                    # A non-owner receiver may accept an owner as another
                    # argument (e.g. ManifestCursor::read_string(..., budget)).
                    receiver=re.sub(r"^&(?:'[\w_]+ )?(?:mut )?",'',ts.get('_1','')).split('<',1)[0]
                    if re.fullmatch(r'[A-Za-z_]\w*(?:::[A-Za-z_]\w*)*',receiver):
                        mutable_targets.add(receiver+'::'+method)
                        mutable_targets.add(receiver.rsplit('::',1)[-1]+'::'+method)
                elif '::' not in f['name']:
                    mutable_targets.add(f['name'])
    unique_names={}
    for f in parsed:
        unique_names.setdefault(f['name'].rsplit('::',1)[-1],set()).add(f['name'])
    for f in parsed:
        local_types=types(f);aliases={}
        for owner in owners.values():
            if owner.get('sealed_evidence') and any(re.search(r'\b'+re.escape(name.rsplit('::',1)[-1])+r'\b',f['signature']+' '+f['body']) for name in [owner['type']]+owner.get('aliases',[])):
                # ZST constructors can vanish even from unoptimized MIR. No
                # ordinary function may carry/create this compile-time token.
                raise ValueError('builtin evidence factory/use outside associated constants: '+f['name'])
        for owner in owners.values():
            if f['name'] in owner['writers'] and any(owner_type(t, {'current':owner}) for t in local_types.values()):
                check_transition_binding(f,owner,owner['writers'][f['name']])
                check_initializers(f,owners,next(k for k,o in owners.items() if o is owner),owner['writers'][f['name']])
                check_callback_commits(f,owner['writers'][f['name']])
        for local,typ in local_types.items():
            own=owner_type(typ,owners)
            if own:aliases[local]={(own,'*')}
            elif ('*mut ' in typ or '*const ' in typ) and any(name in typ for o in owners.values() for name in [o['type']]+o.get('aliases',[])):
                raise ValueError('raw state pointer is outside the verified closure: '+f['name'])
        def places(expression):
            found=set()
            # Captures and aggregate fields can hold an owner without a local
            # whose declared type is that owner. MIR prints the projection's
            # type; seed the same traversal there as for ordinary locals.
            anchors=[]
            for match in re.finditer(r': ([^()\n]+)\)',expression):
                own=owner_type(match[1],owners)
                if own:anchors.append((own,'*',expression[match.end():]))
            for local in local_refs(expression):
                for own,field in aliases.get(local,set()):
                    match=re.search(re.escape(local)+r'\b',expression)
                    anchors.append((own,field,expression[match.end():]))
            for own,field,tail in anchors:
                    if field=='*':
                        # The first projection belongs to the directly typed
                        # state local/reference. Numeric field layout is checked
                        # against syn's AST by the native state-schema test.
                        next_local=re.search(r'(?<![A-Za-z0-9])_\d+\b',tail)
                        if next_local:tail=tail[:next_local.start()]
                        projections=re.findall(r'\.(\d+):',tail)
                        for item in projections:
                            index=int(item);schema=owners[own]['fields']
                            if index>=len(schema):raise ValueError('unknown state projection')
                            field=schema[index]
                            nested=owners[own].get('nested',{}).get(field)
                            if nested:own=nested;field='*'
                            else:break
                    if field=='*' or field in owners[own]['protected']:
                        found.add((own,field))
            return found
        lines=f['body'].splitlines()
        # Conservative union of alias facts, independent of branch order.
        for _ in range(len(local_types)+1):
            changed=False
            for line in lines:
                pair=re.match(r'\s*(_\d+) = (.*);$',line)
                if not pair:continue
                dst,rhs=pair.groups()
                if re.search(r' -> (?:\[return:|unwind )',rhs):continue
                mutable='&mut ' in rhs or '&raw mut ' in rhs
                copied=rhs.startswith(('move _','copy _','(')) and ('&mut ' in local_types.get(dst,''))
                if mutable or copied:
                    facts=places(rhs)
                    if facts:
                        if '&raw ' in rhs:raise ValueError('raw mutable state alias: '+f['name'])
                        before=aliases.get(dst,set());after=before|facts
                        if after!=before:aliases[dst]=after;changed=True
            if not changed:break
        else:raise ValueError('alias closure did not converge')
        # A mutable scalar/owner reference may not escape as the return value.
        if '_0' in aliases and local_types['_0'].startswith('&mut '):
            raise ValueError('mutable state escape: '+f['name'])
        for number,body in f['blocks'].items():
            invocation=call(body)
            if 'DecoderCallback' in body and '::call' in body:
                if f['name'] not in manifest.get('callback_boundaries',[]):
                    raise ValueError('private callback adapter bypasses its guard: '+f['name'])
            for line in body.splitlines():
                stripped=line.strip()
                # Raw trait calls and function-item dispatch must reach the
                # checked boundary. Implementations themselves are not calls.
                if re.search(r'\b(?:decode_varlit|decode_varlit_into|decode_array)\b',stripped):
                    adapter=f['name'] in manifest.get('callback_adapters',[])
                    if not (adapter and invocation and stripped==body.strip().splitlines()[-1].strip()):
                        raise ValueError('unchecked codec callback dispatch: '+f['name'])
                    dispatch+=1
                pair=re.match(r'(.+?) = (.*);$',stripped)
                if not pair:continue
                lhs,rhs=pair.groups()
                if re.search(r' -> (?:\[return:|unwind )',rhs):continue
                aggregate=(' {' in rhs or rhs.startswith(('(', '[')) or bool(re.search(r'::[A-Z]\w*\(',rhs)))
                if aggregate and places(rhs) and owner_type(local_types.get(lhs,''),owners) is None:
                    # Capturing just `&mut decoder.position` erases the owner
                    # type from the closure body. Refuse such aggregate loans
                    # rather than silently losing the interprocedural alias.
                    raise ValueError('state borrow escapes into unmodeled aggregate: '+f['name'])
                # Borrow/copy declarations do not mutate their source. A direct
                # state literal, field store or write through an alias does.
                destinations=places(lhs)
                if re.fullmatch(r'_\d+',lhs) and not any(
                        any(name in rhs for name in [owners[own]['type']]+owners[own].get('aliases',[])) and '{' in rhs for own,_ in destinations):
                    continue
                for own,field in destinations:
                    rule=owners[own]['writers'].get(method_name(f['name']))
                    if rule is None or field not in rule['fields']:
                        raise ValueError(f'unregistered state write: {f["name"]} {own}.{field}')
                    key=(own,f['name'])
                    if key in writer_defs and writer_defs[key]!=f['id']:
                        raise ValueError('ambiguous state writer definition: '+f['name'])
                    writer_defs[key]=f['id']
                    observed[own].setdefault(f['name'],set()).add(field)
            if invocation:
                facts=places(invocation['arguments'])
                if any(field=='*' and owners[own].get('requires_admission_before_forget') for own,field in facts):
                    if invocation['target'] not in ('std::mem::forget','std::mem::drop'):
                        raise ValueError('callback rollback owner escapes into an unmodeled call: '+f['name'])
                if invocation['target'].endswith('::forget'):
                    for own,_ in facts:
                        if owners[own].get('requires_admission_before_forget'):
                            r=owners[own]['writers'].get(f['name'],{})
                            if r.get('guarded_commit')!='forget':
                                raise ValueError('callback rollback suppressed outside admitted commit')
                mutable_whole=any(field=='*' and any(
                    (own,'*') in aliases.get(local,set()) and '&mut ' in local_types.get(local,'')
                    for local in local_refs(invocation['arguments'])) for own,field in facts)
                mutable_whole=mutable_whole or any(field=='*' for _,field in facts) and bool(re.search(r': &mut [^()]+\)',invocation['arguments']))
                sealed=any(owner_type(typ,owners) in {k for k,o in owners.items() if o.get('sealed_lease')} for typ in local_types.values())
                if sealed and mutable_whole:
                    allowed={name for o in owners.values() if o.get('sealed_lease') for name in o['allowed_calls']}
                    if invocation['target'] not in allowed:
                        raise ValueError('sealed budget lease permits only monotone operations: '+f['name']+' '+invocation['target'])
                if mutable_whole and invocation['target'] not in mutable_targets and not (
                        re.fullmatch(r'[A-Za-z_]\w*(?:::[A-Za-z_]\w*)+',invocation['target'])
                        and len(unique_names.get(invocation['target'].rsplit('::',1)[-1],set()))==1
                        and invocation['target'].rsplit('::',1)[-1] in mutable_targets) and not (
                        # A crate trait call on a concrete type (`<Vec<u16> as
                        # FillFixedVec>::fill_fixed`) whose every same-named definition is
                        # itself a checked owner writer. A bare Self name may be a generic
                        # parameter dispatching to an unchecked impl, so it fails closed.
                        re.fullmatch(r'<[^\n]+ as [A-Za-z_]\w*>::[A-Za-z_]\w*',invocation['target'])
                        and not re.match(r'<[A-Za-z_]\w* as ',invocation['target'])
                        and unique_names.get(invocation['target'].rsplit('::',1)[-1])
                        and unique_names[invocation['target'].rsplit('::',1)[-1]]<=mutable_targets):
                    callback=((f['name'] in manifest.get('callback_boundaries',[])
                              and 'DecoderCallback' in invocation['target'] and invocation['target'].endswith('::call'))
                              or (f['name'] in manifest.get('callback_adapters',[]) and re.search(r'::decode_(?:varlit|varlit_into|array)$',invocation['target'])))
                    if not callback:
                        raise ValueError('unmodeled mutable-owner call: '+f['name']+' '+invocation['target'])
                # Mutable projections can only enter a registered semantic
                # transition. Whole-owner method calls are checked by the
                # complete writer scan, and callbacks by the boundary rule.
                for own,field in facts:
                    if field=='*':continue
                    if not any(own2==own and fld=='*' for own2,fld in facts):
                        matches=[r for r in manifest['field_leases']
                                 if r['caller']==f['name'] and r['target']==invocation['target']
                                 and r['owner']==own and r['field']==field]
                        if not matches:raise ValueError('unregistered mutable-field call: '+f['name']+' '+invocation['target'])
                        leases+=1
    for own,owner in owners.items():
        if set(observed[own])!=set(owner['writers']):
            raise ValueError('registered state writer disappeared: '+own+' '+str(set(owner['writers'])-set(observed[own])))
    if 'adapter_count' in manifest and dispatch!=manifest['adapter_count']:
        raise ValueError('sealed callback adapter set changed')
    return dict(writers={k:{n:sorted(v) for n,v in d.items()} for k,d in observed.items()},
                checked_callback_dispatches=dispatch,mutable_field_leases=leases,
                builtin_evidence_constants=evidence_constants)


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--mir',type=Path,required=True)
    p.add_argument('--features',choices=['default','all'],required=True);p.add_argument('--output',type=Path,required=True)
    a=p.parse_args();version=subprocess.check_output(['rustc','--version'],text=True)
    if not version.startswith('rustc 1.95.0 '):raise ValueError('state closure requires pinned rustc 1.95.0')
    report=inspect(a.mir.read_text(),json.loads(MANIFEST.read_text()),json.loads((ROOT/'docs/verification/proof-contracts.json').read_text()))
    a.output.parent.mkdir(parents=True,exist_ok=True);a.output.write_text(json.dumps(dict(features=a.features,**report),indent=2)+'\n')
    print('STATE_TRANSITIONS '+a.features+' '+str(sum(len(v) for v in report['writers'].values()))+' writers closed')
if __name__=='__main__':main()
