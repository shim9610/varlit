# Decoder state contract

This contract covers library-owned decoding through `Decoder::decode_value`,
complete/accounted/nested decoding, built-in containers and generated codecs.
It does not constrain a downstream caller that directly invokes a trait method.
The [machine-readable transition registry](state-transitions.json) and
[proof registry](proof-contracts.json) name the actual owners and harnesses.

## Invariant and frame

For a decoder with input `I`, cursor `p`, limit `L` and remaining budget `R`:

- `0 <= p <= I.len()` and `R <= L`.
- Within a frame, successful reads never move `p` backward; charges never increase `R`.
- Input identity, endianness and `L` remain fixed. A nested frame receives separate
  input and no more than the parent's remaining budget.
- Ordinary errors retain completed reads/charges. Overflow or insufficient input
  cannot commit a failed scalar transition.

`new_limited` establishes `p = 0`, `L = R` and no active callback scope. The pure
`charge` and `read_end` helpers have executable, exact full-width contracts.
Their callers must store that result using their actual state and request;
substituted inputs and disconnected stores are rejected by the compiler gate.
Collection-budget initialization and explicit `reset` start a new accounting
epoch. This contract does not prove that all users release live allocations
before resetting an internal budget.

## Extensible callbacks

A first custom callback receives a temporary frame carrying a private,
short-lived scope borrow. Its parent is exhausted before user code runs. Admission
requires original scope, input pointer/length, endian and limit, bounded forward
cursor movement and non-increasing budget. The Kani havoc harness considers all
numeric post-states, success/error and selected changed/absent scope and input
identities, using a four-byte backing input and full-width budgets/cursors.

Nested callbacks reuse the active scope. Every safe primitive transition preserves
the numeric invariant; whole replacement cannot recreate the private borrow.
A guard restores original frame fields and exhausts cursor/budget on rejection
or unwind. Its only normal-path suppression must follow successful scope
admission. The active-step harness selects an arbitrary full-width read, charge
or fresh replacement from an arbitrary valid state. Native tests also catch an
inner panic and attempt to continue using the frame. The scope-helper harness
uses two live byte objects with absent, identical and different-object reference
choices; it does not enumerate arbitrary pointer layouts.

Built-in implementations carry type-bound, invariant evidence with a private
constructor. Composite evidence propagates whether component codecs can invoke
user code, allowing one outer frame to serve nested callbacks. They preserve the
invariant through the same checked primitives and route extensible element callbacks through `decode_value`. Raw trait dispatch is
confined to two private static adapters; the compiler checks all other compiled
functions for bypasses. Compile-fail tests reject evidence construction and reuse
for a different output type. No runtime option disables the boundary.

## Mutation closure and trusted assumptions

The gate scans all unoptimized core MIR definitions for default/all Linux builds,
including generic implementations. It follows typed owners, field projections and
mutable aliases. Unregistered writes, mutable-field escapes, raw pointers,
unmodeled mutable calls, callback function items and aggregate captures fail.
Private field/type/layout checks retain the closed state representation. New
constructors, writers or mutable capabilities require a transition obligation;
changing a source hash alone cannot admit them. Deliberately broken compiled
fixtures test these rejection paths and guard dominance.

The induction is: initialization establishes the invariant, each admitted primitive
preserves it, and composed safe calls cannot acquire an unregistered mutation
capability. The semantic Kani proofs and structural compiler checks are separate
evidence; the Python MIR parser and their composition are not themselves formally
verified. This relies on Rust's safe memory, privacy, ownership and lifetime rules,
the pinned compiler/verifier, and a returning callback. Arbitrary callback
allocation/termination, malicious unsafe code, OS termination and foreign codec
internals are excluded. Existing publication/index/compression proofs keep their
own bounds; this is not whole-library OOM freedom or complete initialization proof.
