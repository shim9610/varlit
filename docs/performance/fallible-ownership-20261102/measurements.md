# Per-unit measurements

Positive changes mean more time, CPU or memory. Native confirmation samples are pooled with their initial samples.

## stage1

| Workload | Pairs | Operation | CPU | Max RSS before → after (MiB) |
| --- | ---: | ---: | ---: | ---: |
| bytes-67108864 | 5 | +0.61% | +0.67% | 129.19 → 129.12 |
| bytes-1073741824 | 5 | +0.37% | -1.86% | 2049.18 → 2049.19 |
| nested-16384 | 20 | +11.07% | +9.93% | 11.46 → 11.37 |
| 2M records / 200k marks | 5 | -5.39% | -0.13% | 5.76 → 5.76 |
| 4 GiB writer / reverse marks | 5 | -35.08% | -2.86% | 3.45 → 3.45 |
| Complex maintenance | 5 | +2.57% | +2.13% | 8.54 → 8.50 |
| 64 MiB chunk encode | 5 | +0.72% | -0.51% | 66.34 → 66.48 |
| 64 MiB full chunk decode | 5 | -1.34% | +0.66% | 131.07 → 131.20 |

| Indexed SWMR | Pairs | Wall | CPU | Max RSS change | Gate |
| --- | ---: | ---: | ---: | ---: | --- |
| 1 GiB / 8 followers + pin | 3 | +4.13% | +0.11% | -0.30% | passed |
| 4 GiB / 8 followers + pin | 3 | +0.06% | -0.16% | -0.14% | passed |

## stage2

| Workload | Pairs | Operation | CPU | Max RSS before → after (MiB) |
| --- | ---: | ---: | ---: | ---: |
| bytes-67108864 | 5 | +2.94% | +2.52% | 129.06 → 129.06 |
| bytes-1073741824 | 5 | -0.30% | +0.32% | 2049.12 → 2049.06 |
| nested-16384 | 5 | +0.10% | -0.23% | 11.38 → 11.31 |
| 2M records / 200k marks | 20 | -2.44% | -0.41% | 5.71 → 5.76 |
| 4 GiB writer / reverse marks | 5 | -3.20% | -0.66% | 3.46 → 3.50 |
| Complex maintenance | 5 | +1.53% | -1.09% | 8.50 → 8.50 |
| 64 MiB chunk encode | 20 | +2.04% | +1.79% | 66.53 → 66.40 |
| 64 MiB full chunk decode | 5 | +2.44% | +0.17% | 131.24 → 131.07 |
| Matrix: ordinary (disk) | 5 | +1.45% | -1.81% | 29.31 → 29.59 |
| Matrix: durable (disk) | 5 | +3.61% | +3.69% | 28.29 → 28.45 |

| Indexed SWMR | Pairs | Wall | CPU | Max RSS change | Gate |
| --- | ---: | ---: | ---: | ---: | --- |
| 1 GiB / 8 followers + pin | 3 | -2.19% | -2.15% | +0.47% | passed |
| 4 GiB / 8 followers + pin | 3 | -0.57% | -0.67% | +0.08% | passed |

## stage3

| Workload | Pairs | Operation | CPU | Max RSS before → after (MiB) |
| --- | ---: | ---: | ---: | ---: |
| bytes-67108864 | 5 | -2.14% | -1.43% | 129.06 → 129.07 |
| bytes-1073741824 | 5 | -8.59% | -6.65% | 2049.04 → 2049.06 |
| nested-16384 | 5 | +0.89% | -0.16% | 11.31 → 11.34 |
| 2M records / 200k marks | 20 | +1.87% | -2.08% | 5.67 → 5.70 |
| 4 GiB writer / reverse marks | 20 | +4.94% | -1.04% | 3.45 → 3.45 |
| Complex maintenance | 5 | -1.02% | +0.20% | 8.46 → 8.50 |
| 64 MiB chunk encode | 5 | +2.68% | +2.10% | 66.28 → 66.34 |
| 64 MiB full chunk decode | 5 | -1.75% | -2.58% | 131.10 → 131.01 |
| Matrix: ordinary (disk) | 5 | -7.42% | -0.45% | 29.09 → 29.25 |
| Matrix: durable (disk) | 20 | -1.89% | +1.70% | 28.57 → 28.54 |

| Indexed SWMR | Pairs | Wall | CPU | Max RSS change | Gate |
| --- | ---: | ---: | ---: | ---: | --- |
| 1 GiB / 8 followers + pin | 3 | -3.75% | -3.85% | +0.11% | passed |
| 4 GiB / 8 followers + pin | 3 | -3.27% | -3.22% | -0.02% | passed |
