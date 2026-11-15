import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
sys.path.insert(0,str(Path(__file__).parents[1]))
import check_state_transitions as gate
from check_proof_bindings import functions

class StateClosureTests(unittest.TestCase):
    def compile(self,source):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory);rs=path/'fixture.rs';mir=path/'fixture.mir';rs.write_text(source)
            subprocess.run(['rustup','run','1.95.0','rustc','--edition=2024','--crate-type=lib','--emit=mir',str(rs),'-o',str(mir)],check=True,capture_output=True,text=True)
            return mir.read_text()
    def inspect(self, extra):
        source='''#![allow(dead_code)]
struct State { remaining:u64 }
impl State { fn new(n:u64)->Self {Self{remaining:n}} fn set(&mut self,n:u64){self.remaining=n;} }
'''+extra
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory);rs=path/'fixture.rs';mir=path/'fixture.mir';rs.write_text(source)
            subprocess.run(['rustup','run','1.95.0','rustc','--edition=2024','--crate-type=lib','--emit=mir',str(rs),'-o',str(mir)],check=True,capture_output=True,text=True)
            text=mir.read_text();fs=functions(text)
            writers={f['name']:dict(fields=['*'] if f['name'].endswith('::new') else ['remaining'],proofs=['p']) for f in fs if f['name'].endswith(('::new','::set'))}
            manifest=dict(owners={'state':dict(type='qualified::State',aliases=['State'],fields=['remaining'],protected=['remaining'],writers=writers)},field_leases=[])
            return gate.inspect(text,manifest,dict(contracts=[dict(harnesses=['p'])]))
    def test_registered_transitions_and_borrowed_calls_pass(self):
        self.inspect('fn consumer(s:&mut State){s.set(3);}')
    def test_new_mutation_and_alias_writes_fail(self):
        for extra in ['fn bad(s:&mut State){s.remaining=3;}',
                      'fn bad(s:&mut State){let p=&mut s.remaining;*p=3;}',
                      'fn bad(s:&mut State){let mut f=||{s.remaining=3;};f();}',
                      'fn bad(s:&mut State){let mut f=move ||{let p=&mut s.remaining;*p=3;};f();}',
                      'fn bad(s:&mut State){*s=State::new(3);}']:
            with self.subTest(extra=extra),self.assertRaisesRegex(ValueError,'unregistered state write|unmodeled aggregate'):
                self.inspect(extra)
    def test_mutable_escapes_and_unknown_callbacks_fail(self):
        for extra,why in [('fn bad(s:&mut State)->&mut u64 {&mut s.remaining}','mutable state escape'),
                          ('fn bad(s:&mut State, f:impl FnOnce(&mut State)){f(s);}','unmodeled mutable-owner call|unmodeled aggregate'),
                          ('fn helper(n:&mut u64){*n=0;} fn bad(s:&mut State){helper(&mut s.remaining);}','unregistered mutable-field call'),
                          ('fn bad(s:&mut State){let p=&raw mut s.remaining;unsafe{*p=0;}}','raw mutable state alias')]:
            with self.subTest(extra=extra),self.assertRaisesRegex(ValueError,why):self.inspect(extra)
        with self.assertRaisesRegex(ValueError,'raw state pointer'):
            self.inspect('unsafe fn bad(s:*mut State){unsafe{(*s).remaining=3;}}')
    def test_raw_trait_dispatch_and_function_items_fail(self):
        prefix='trait Codec {fn decode_varlit(s:&mut State);}'
        for extra in ['fn bad<T:Codec>(s:&mut State){T::decode_varlit(s);}',
                      'fn bad<T:Codec>(s:&mut State){let f=T::decode_varlit;f(s);}']:
            with self.subTest(extra=extra),self.assertRaisesRegex(ValueError,'unchecked codec callback dispatch'):self.inspect(prefix+extra)

    def test_same_named_trait_callback_is_not_a_known_owner_method(self):
        with self.assertRaisesRegex(ValueError,'unmodeled mutable-owner call'):
            self.inspect('trait Callback {fn set(s:&mut State);} fn bad<T:Callback>(s:&mut State){T::set(s);}')

    def test_state_store_must_use_proved_result_and_real_inputs(self):
        for body,why in [
            ('let next=charge(self.remaining,n).ok_or(())?; self.remaining=next; Ok(())',None),
            ('let next=charge(self.remaining,n).ok_or(())?; self.remaining=0; Ok(())','disconnected'),
            ('let next=charge(self.remaining,0).ok_or(())?; self.remaining=next; Ok(())','substituted inputs'),
            ('let next=charge(n,self.remaining).ok_or(())?; self.remaining=next; Ok(())','substituted inputs'),
        ]:
            source='struct State {remaining:u64} fn charge(a:u64,b:u64)->Option<u64>{a.checked_sub(b)} impl State {fn consume(&mut self,n:u64)->Result<(),()>{'+body+'}}'
            f=next(f for f in functions(self.compile(source)) if f['name'].endswith('::consume'))
            owner=dict(fields=['remaining'])
            rule=dict(transition=dict(helper='charge',arguments=['field:remaining','param:2'],field='remaining'))
            with self.subTest(body=body):
                if why:
                    with self.assertRaisesRegex(ValueError,why):gate.check_transition_binding(f,owner,rule)
                else:gate.check_transition_binding(f,owner,rule)

    def test_constructor_cannot_invent_remaining_budget(self):
        for initializer,why in [('n',None),('n.wrapping_add(1)','invents budget'),('0','invents budget')]:
            text=self.compile('struct State {limit:u64,remaining:u64} impl State {fn new(n:u64)->Self{Self{limit:n,remaining:'+initializer+'}}}')
            f=next(f for f in functions(text) if f['name'].endswith('::new'))
            owner=dict(type='State',fields=['limit','remaining'])
            rule=dict(initializes=dict(equal=[['limit','remaining']]))
            with self.subTest(initializer=initializer):
                if why:
                    with self.assertRaisesRegex(ValueError,why):gate.check_initializers(f,{'state':owner},'state',rule)
                else:gate.check_initializers(f,{'state':owner},'state',rule)

    def test_rollback_suppression_requires_successful_admission(self):
        for body,why in [
            ('if !same_scope(x){return;} std::mem::forget(guard);',None),
            ('std::mem::forget(guard); let _=same_scope(x);','discarded|exits without|bypasses'),
            ('if !same_scope(x){std::hint::black_box(());} std::mem::forget(guard);','rejected callback'),
            ('if x {if !same_scope(x){return;}} std::mem::forget(guard);','bypasses'),
        ]:
            text=self.compile('struct Guard; fn same_scope(x:bool)->bool{x} fn consumer(x:bool){let guard=Guard;'+body+'}')
            f=next(f for f in functions(text) if f['name']=='consumer')
            with self.subTest(body=body):
                if why:
                    with self.assertRaisesRegex(ValueError,why):gate.check_callback_commits(f,dict(guarded_commit='forget',guard='same_scope'))
                else:gate.check_callback_commits(f,dict(guarded_commit='forget',guard='same_scope'))

    def test_builtin_evidence_cannot_gain_a_factory_or_unrelated_constant(self):
        prefix='struct BuiltinCodec<T>{invariant:std::marker::PhantomData<fn(T)->T>} '
        owner=dict(type='BuiltinCodec',fields=['invariant'],protected=['invariant'],writers={},sealed_evidence=True)
        proofs=dict(contracts=[dict(harnesses=['p'])])
        for extra,is_constant,why in [
            ('fn bad<T>()->BuiltinCodec<T>{BuiltinCodec{invariant:std::marker::PhantomData}}',False,'evidence factory/use'),
            ('const FAKE:BuiltinCodec<u8>=BuiltinCodec{invariant:std::marker::PhantomData}; fn dummy(){}',True,'outside a codec associated constant'),
        ]:
            manifest=dict(owners={'evidence':owner},field_leases=[])
            if is_constant:manifest['builtin_evidence']=True
            with self.subTest(extra=extra),self.assertRaisesRegex(ValueError,why):
                gate.inspect(self.compile(prefix+extra),manifest,proofs)

    def test_rollback_owner_cannot_be_hidden_in_manually_drop(self):
        text=self.compile("struct Guard<'a>{v:&'a mut u64} impl Drop for Guard<'_>{fn drop(&mut self){*self.v=0;}} fn bad(g:Guard<'_>){let _hidden=std::mem::ManuallyDrop::new(g);}")
        owner=dict(type='Guard',fields=['v'],protected=[],writers={},requires_admission_before_forget=True)
        with self.assertRaisesRegex(ValueError,'rollback owner escapes'):
            gate.inspect(text,dict(owners={'guard':owner},field_leases=[]),dict(contracts=[dict(harnesses=['p'])]))

if __name__=='__main__':unittest.main()
