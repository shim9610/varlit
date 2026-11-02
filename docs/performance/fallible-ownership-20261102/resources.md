# Sampled resources

Peaks sampled every 5 ms. RSS excludes some filesystem cache and tmpfs backing. Deleted regular files include retained generations; zero private spill does not mean zero temporary disk use. Whole-cgroup memory is retained separately in the raw JSON and includes other cache/workspace memory.

| Probe | RSS MiB before → after | Private spill allocated bytes | Deleted regular logical bytes | All open regular allocated bytes |
| --- | ---: | ---: | ---: | ---: |
| stage1 / complex (default tmpfs) | 8.38 → 8.44 | 4,194,304 → 4,194,304 | 51,447,742 → 51,447,742 | 91,955,200 → 91,955,200 |
| stage1 / clone-large (default tmpfs) | 2049.17 → 2049.06 | 0 → 0 | 0 → 0 | 0 → 0 |
| stage2 / complex (default tmpfs) | 8.44 → 8.39 | 4,194,304 → 4,194,304 | 51,447,742 → 51,447,742 | 91,955,200 → 91,955,200 |
| stage2 / clone-large (default tmpfs) | 2048.97 → 2049.04 | 0 → 0 | 0 → 0 | 0 → 0 |
| stage3 / complex (default tmpfs) | 8.44 → 8.40 | 4,194,304 → 4,194,304 | 51,447,742 → 51,447,742 | 91,955,200 → 91,955,200 |
| stage3 / clone-large (default tmpfs) | 2049.12 → 2049.06 | 0 → 0 | 0 → 0 | 0 → 0 |
| stage2 / durable matrix (disk) | 28.02 → 28.01 | 0 → 0 | 148,422,656 → 148,422,656 | 182,312,960 → 182,308,864 |
| stage3 / durable matrix (disk) | 28.57 → 28.14 | 0 → 0 | 148,422,656 → 148,422,656 | 182,317,056 → 182,312,960 |
