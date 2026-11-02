# Small-buffer ChunkedBytes range streaming

Baseline: `b837577cca7a2436a1da0074e94149c003503680` (PR #18).
The preceding PR history and its conditional-memory, hostile-metadata and
compression audits were reviewed before implementation. This is one runtime
unit: a new explicit streaming route. Existing decode methods, VCHK format,
dependencies and default resource budgets remain unchanged.

`ChunkedBytes::stream_range(range, buffer, max_window_bytes, callback)` delivers
absolute decoded offsets through an external slice without resizing it or
materializing a decoded chunk. `decoded_len()` exposes the declared total.
A caller can select this route after whole-output limit/allocation refusal.
Tests cover complete traversal and ranges crossing chunk boundaries with
1-, 7- and 4096-byte buffers, empty ranges, invalid ranges, explicit window
refusal, callback failure and same-object retry. See the
[API contract and usage](../../chunk-streaming.md).

Selected chunks are decoded completely and CRC checked, even beyond the
requested range. Callback delivery remains provisional until final success;
nonselected payloads are not checked. CRC is corruption detection, not
cryptographic authentication. Encoded input remains resident. zstd context,
history and block workspace remain native allocations; this is not a fully
external-workspace decoder or a global OOM-freedom claim.

## Executed checks

- Workspace/unit/integration/doc tests: **1,338 passed, 0 failed**, 30 existing
  ignored. The native allocation-refusal test is explicitly executed separately
  by the release contract runner; its ignored workspace status is not a pass.
- Required release storage/resource contracts: **40/40**. Four new public API
  symbols have executed valid/invalid contracts; the frozen legacy inventory
  was not regenerated.
- Python tooling tests: **46 passed**. Warnings-denied workspace/all-targets/
  all-features Clippy passed. The final benchmark-only changes also passed
  warnings-denied Clippy. Formatting passed.
- New streaming suite: **8 tests** with all features. No-default, integrity-only
  and zstd-only feature configurations each pass their two applicable tests.
- **35 Kani harnesses default + 35 all-feature**, with matching before/after
  source fingerprints and exact proof-set checks. **17/17 resource mutants**
  are detected; the isolated unmodified baseline's 18 selected proofs pass.
- ASan `codec_arbitrary`: **3,278,836 executions in 121 seconds**, with
  `-max_total_time=120 -rss_limit_mb=4096`, pinned nightly 2026-10-12.
  This finite fuzz run is not exhaustive hostile-input verification.
- An 8 MiB streaming test denies every Rust allocation on its current thread:
  **zero allocation requests** occur during streaming. Separate native malloc
  refusal sweeps exercise decoder context/workspace failures and successful
  retry. These are deliberate allocator-refusal tests, not OOM-kill reproductions.

## What the new proof establishes

The production `stream_progress` helper is called after each native decode
step and before callback delivery. Full-width symbolic inputs prove that every
accepted step advances the input cursor or produces output, never reverses or
passes the input extent, and never overflows/exceeds the declared output extent.
The helper is also checked against an independent wider-integer oracle.
Removing the output ceiling or accepting a stalled step fails the proof.

With finite input/output extents, these accepted transitions give the wrapper a
finite progress measure. This assumes each backend call and callback returns.
It does **not** prove C decoder termination, OS/allocator behavior, the complete
parser, arbitrary callbacks, all library allocations or whole-process OOM
freedom. The existing BTreeMap deferral and other conditional-memory obligations
remain in force.

## Window-policy defect found during implementation

The native legacy streaming adapters bypass modern `WindowLogMax`; a modern
one-shot shortcut can also bypass that streaming-only check. The new path
therefore preflights every selected frame before native decode, including
concatenated legacy v0.4-v0.7 frames. A regression fixture rejects a legacy
2048-byte window under a 1024-byte policy before any callback, and rejects a
modern oversized window even when the output buffer holds the complete frame.
The upstream concatenated legacy fixture succeeds with a sufficient policy.
The native backend does not support v0.1-v0.3 streaming; existing whole-output
compatibility is unchanged. The explicit window ceiling is rounded down to a
power of two, with minimum 1024 bytes. It is not a total heap/RSS ceiling.

## Evidence and unsuccessful setup attempts

`evidence.tar.gz` retains raw validation logs, command/exit records, proof
fingerprints and all mutation logs. `evidence-members.sha256` hashes each member.
Initial failed runs are retained separately by their original names. They include
an initially global allocation-denial flag colliding with another test thread
(fixed to thread-local for the all-allocation guard), missing new API registration,
a tooling category-set assertion, an initial Clippy lint and a benchmark harness
extraction error. They are not counted as successful qualification. The final
command list retains the initial Python failure and subsequent successful rerun.

Remote platform/CI checks are separate from these local results.
[Equal-work performance and resource measurements](../../performance/chunk-streaming-20261102/README.md)
include the measured 64-byte-buffer slowdown.
