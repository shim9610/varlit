# Per-unit measurements

Positive changes mean more time, CPU or memory. Native confirmation samples are pooled with their initial samples.

## box

| Workload | Pairs | Operation | CPU | Max RSS before → after (MiB) |
| --- | ---: | ---: | ---: | ---: |
| Owner: box | 20 | +4.82% | +4.07% | 1.20 → 1.14 |
| Owner: nested-box | 5 | -17.04% | -16.97% | 1.33 → 1.27 |
| Owner: shared | 20 | +1.55% | +1.56% | 1.20 → 1.14 |
| bytes-67108864 | 5 | -1.92% | -3.41% | 129.07 → 129.07 |
| bytes-1073741824 | 5 | -2.78% | -1.80% | 2049.07 → 2049.07 |
| nested-16384 | 5 | +2.40% | +2.77% | 11.33 → 11.33 |
| 2M records / 200k marks | 5 | -2.54% | +3.13% | 5.63 → 5.64 |
| 4 GiB writer / reverse marks | 20 | +1.95% | -1.94% | 3.38 → 3.39 |
| Complex maintenance | 5 | -4.58% | -0.83% | 8.42 → 8.38 |
| 64 MiB chunk encode | 5 | -0.99% | -1.19% | 66.07 → 66.11 |
| 64 MiB full chunk decode | 5 | -0.77% | -0.28% | 130.91 → 131.00 |
| Matrix: ordinary (disk) | 5 | +2.22% | +3.97% | 29.51 → 29.38 |
| Matrix: durable (disk) | 5 | +3.65% | +3.15% | 29.15 → 29.18 |

| Indexed SWMR | Pairs | Wall | CPU | Max RSS change | Gate |
| --- | ---: | ---: | ---: | ---: | --- |
| 1 GiB / 8 followers + pin | 3 | -0.79% | -1.17% | -0.22% | passed |
| 4 GiB / 8 followers + pin | 3 | -3.01% | -3.11% | +0.05% | passed |

## arc

| Workload | Pairs | Operation | CPU | Max RSS before → after (MiB) |
| --- | ---: | ---: | ---: | ---: |
| Owner: box | 20 | +3.65% | +3.87% | 1.14 → 1.14 |
| Owner: nested-box | 5 | -4.76% | -4.76% | 1.26 → 1.27 |
| Owner: shared | 5 | -1.56% | -1.47% | 1.07 → 1.07 |
| bytes-67108864 | 5 | -0.46% | +0.04% | 129.07 → 129.08 |
| bytes-1073741824 | 5 | -4.11% | -4.84% | 2049.08 → 2049.05 |
| nested-16384 | 20 | -1.21% | -1.76% | 11.33 → 11.33 |
| 2M records / 200k marks | 5 | +3.34% | -0.54% | 5.64 → 5.64 |
| 4 GiB writer / reverse marks | 5 | +2.97% | -1.61% | 3.39 → 3.39 |
| Complex maintenance | 5 | -3.79% | -1.61% | 8.43 → 8.43 |
| 64 MiB chunk encode | 5 | +1.02% | -0.69% | 66.11 → 66.10 |
| 64 MiB full chunk decode | 5 | +1.93% | +2.38% | 130.92 → 130.87 |
| Matrix: ordinary (disk) | 5 | -6.06% | -9.68% | 29.38 → 28.99 |
| Matrix: durable (disk) | 5 | -4.13% | -4.80% | 28.97 → 28.96 |

| Indexed SWMR | Pairs | Wall | CPU | Max RSS change | Gate |
| --- | ---: | ---: | ---: | ---: | --- |
| 1 GiB / 8 followers + pin | 3 | +4.61% | +3.72% | -0.17% | passed |
| 4 GiB / 8 followers + pin | 3 | -5.45% | -4.53% | +0.02% | passed |

## Final versus baseline: original owner benchmark

| Workload | Pairs | Operation | CPU | Max RSS before → after (MiB) |
| --- | ---: | ---: | ---: | ---: |
| box | 20 | +8.77% | +9.11% | 1.20 → 1.07 |
| nested-box | 5 | -18.52% | -18.50% | 1.32 → 1.19 |
| shared | 20 | +0.86% | +0.98% | 1.20 → 1.14 |

## Diagnostic: dispatch outside the timed loop, matching all features

| Workload | Pairs | Operation | CPU | Max RSS before → after (MiB) |
| --- | ---: | ---: | ---: | ---: |
| box | 5 | -1.09% | -1.14% | 1.18 → 1.13 |
| nested-box | 5 | -12.86% | -12.84% | 1.33 → 1.18 |
| shared | 20 | -0.25% | -0.13% | 1.20 → 1.12 |

## Sampled resource peaks

Each row is a separate resource-probe process. Cgroup memory includes filesystem cache and other resident host work; these are observations, not upper bounds.

| Unit / workload | RSS MiB before → after | Deleted temporary allocation MiB | Open regular allocation MiB | Whole-cgroup MiB |
| --- | ---: | ---: | ---: | ---: |
| box / complex | 8.37 → 8.32 | 4.00 → 4.00 | 87.70 → 87.70 | 2776.48 → 2776.12 |
| box / clone-large | 2049.04 → 2049.05 | 0.00 → 0.00 | 0.00 → 0.00 | 4731.67 → 4724.28 |
| box / matrix | 27.87 → 27.82 | 0.00 → 0.00 | 173.87 → 173.87 | 2917.71 → 2917.53 |
| arc / complex | 8.26 → 8.32 | 4.00 → 4.00 | 87.70 → 87.70 | 2791.47 → 2791.93 |
| arc / clone-large | 2049.03 → 2049.04 | 0.00 → 0.00 | 0.00 → 0.00 | 4743.58 → 4749.33 |
| arc / matrix | 28.51 → 27.84 | 0.00 → 0.00 | 173.86 → 173.87 | 2933.04 → 2932.42 |
