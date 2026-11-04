# Resource mutation proof inventory correction

The default Kani job in [CI #111](https://github.com/shim9610/varlit/actions/runs/41783412708), at `71ee3ee`, failed after its ordinary 55-harness verification had passed. The auxiliary resource mutation baseline also completed successfully: **19 verified harnesses, zero failures**. Its script still required the literal completion text for 18 harnesses, so it rejected the successful baseline before executing the mutations. The additional harness checks the new sequence candidate arithmetic.

This was a stale CI inventory check, not a failed Rust property. The failed run remains recorded; its default Kani and required aggregate checks are not relabeled as passes.

The corrected script rebases the registered production harness names onto its extracted scratch modules, requires every expected resource family and rejects duplicate registrations. It reuses the strict proof-log validator to require the exact unique harness set, consistent completed totals and no failed properties. Missing, partial, unexpected or duplicate proof output still fails. The chunk fixture deliberately includes only its table-extent helper and corresponding registered proof; the ordinary core verification covers the other chunk contracts.

## Validation

- The archived failing CI job's baseline log passes the corrected exact-set validation for all 19 harnesses.
- A fresh local run verifies all 19 baseline harnesses and detects **all 17 deliberate resource/traversal defects** at their required assertions. Compilation failures and timeouts do not count as detections.
- All **59 Python tooling regressions** pass, including omission of the newly added proof, missing resource families, duplicate registration and the existing strict completion/output checks.
- Python compilation and whitespace checks pass. The production `crates` Git tree equals runtime `5d8a852`; no Rust runtime, proof bound, assertion, resource allowance, dependency, public API or file format changed. A runtime performance rerun is therefore unnecessary for this tooling correction.

The following source push starts new exact-head CI checks; the preceding failed run is separate evidence. These scoped proofs do not establish universal OOM recovery or filesystem behavior.

See [results](results.json), [raw failed-baseline and repaired-run evidence](evidence.tar.gz), [file hashes](evidence-manifest.json), and [archive checksum](evidence.sha256).

The owner has prioritized stability verification for the next version. Unresolved streaming timing, nonresident storage cost, historical failures and uncompleted performance qualification remain in [issue #20](https://github.com/shim9610/varlit/issues/20); none is converted into a passing measurement by this priority change. The [streaming report](../streaming-follow-20261104/README.md) retains the original samples and workload distinctions.
