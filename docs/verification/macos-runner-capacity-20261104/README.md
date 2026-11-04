# macOS hosted-runner acquisition failures

CI run [#117](https://github.com/shim9610/varlit/actions/runs/41854157823),
at `1e770ce3a73d2f317e32f34d250bc44cd9c4a421`, failed because macOS jobs
were cancelled before acquiring a runner or executing any test step. The
required aggregate correctly rejected `test` / `platform-io` results marked
`abandoned`. Linux/Windows tests, both Kani configurations and CodeQL succeeded.
The same source's Sanitizers run completed successfully.

Public job annotations state:

> The job was not acquired by Runner of type hosted even after multiple attempts

> Due to capacity constraints, jobs targeting macOS arm64 runners may experience longer queue times.

The retry also reports an internal server error with a correlation ID. Exact
messages and source URLs are retained in [annotations.json](annotations.json).
The retry failed before the next benchmark commit was pushed; this cancellation
was not caused by that push. Separately, `cancel-in-progress: true` intentionally
supersedes older executions when a newer commit starts a run on the same PR.

`macos-latest` currently selects macOS 26 ARM64. The CI matrix now selects the
official `macos-15-intel` x64 image for both test feature sets and bounded-I/O
checks. The [runner image catalog](https://github.com/actions/runner-images#available-images)
lists that label for macOS 15. No test, failure condition, required aggregate,
timeout or correctness assertion is removed. This fallback changes OS version and architecture: these macOS jobs validate
macOS 15 x64, not macOS 26 ARM64. It is not a qualification of the unavailable
ARM64 pool or the latest macOS image. The label is recognized by the installed
workflow validator; no label validation exception is added.

This runner-selection fix does not alter the completed Linux performance
campaign's 4 CPU / 16 GiB / 32 GiB environment or its measurements. macOS CI
execution is a correctness check, not a performance comparison across machines.
Workflow lint and the 59 existing CI-helper tests are checked locally; remote
results must still be observed before claiming a successful CI run.
