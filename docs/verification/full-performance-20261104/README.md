# Full performance comparison — 2026-11-04

Compared main ba541cc3082cecdf7f385d06897a58a3889e1294 with PR #18 runtime 1e592397d34f4f576111f1107fb29303b9d472b0. The 64 primary conditions, two lazy first/repeated-validation conditions, and eight default-feature/checksum-disabled conditions total 74. All seven initial batch conditions were rerun after the common benchmark-source correction described below.

Status: 34 conditions meet the observed gates, 18 remain unqualified because control variation exceeds the limit, and 22 exceed an observed gate. Per-condition failures, variation, and absolute values are retained below. The 5% threshold is a regression allowance, not a definition of measurement error. There are 2,160 valid timing observations and 148 separate resource probes, with no execution failures or work mismatches in the final comparisons. Initial failures and interrupted attempts remain preserved. The 22 exceedances are not all elapsed-time regressions: some concern resident set size (RSS), temporary storage, or a few KiB of operating-system-accounted writes.

The scope is a Linux workload matrix covering the main operation families and eight default-feature conditions. It does not qualify every platform, input, or feature combination.

## Main findings

- Indexed batch with no readers, 4 MiB records and 4 GiB written: elapsed time +51.97%/+58.30%, process CPU +76.04%/+92.97%. Kernel CPU and minor page faults increased more prominently than bytes written. Median system CPU was about 1.05–1.13 s on main and 3.03 s on the PR; minor faults rose from about 256,000 to 2,130,000. Exact allocation call paths were not profiled. Write-time control variation was 3.99%, CPU variation was 8.77%, and other stages also exceeded calibration limits, so the condition remains unqualified. Both the control variation and the much larger observed increases are retained.
- Ordinary matrix updates: elapsed time +7.20%/+7.09%, process CPU +8.34%/+7.11%. For large-cell mmap workloads, distinguish the read timer from total CPU including preparation. The process CPU increase also includes initial matrix creation and writes.
- Fixed-map decoding: loop time +9.73%/+7.71%. Small fixed/variable value encode/decode stages also exceeded individual gates. Lower input-preparation CPU does not cancel the decoding-loop increase; the stages are reported separately.
- One million shared-owner construction/clone/cancel sequences: elapsed time +21.48%/+22.59% (0.013906/0.013781 → 0.016894 s). This control path has a different cost profile from large byte/nested clones.
- Nonresident marking after validation: 0.029251/0.028319 → 0.040209 s. First lazy mark: 0.000018/0.000018 → 0.071943 s; 4,096 marks after validation: 0.013022/0.013735 → 0.014767 s. Initial validation and repeated work are distinct. Warm lazy control variation is 5.47%, leaving that condition unqualified; the nonresident warm-mark increase remains an observed gate exceedance.
- Independent writer, eight readers, 4 GiB: the 1 MiB record condition meets the observed gates; 4 MiB records increase writer CPU by +5.61%/+6.25%. The latter condition remains unqualified because of time-control variation, but writer CPU control variation is only 0.60%, so the CPU increase is not dismissed as wall-time noise. Larger payload groups are not uniformly faster.
- Fixed two-million-record marking, pending follow, and several maintenance/clone conditions improved or remained within the observed gates. For example, small pending-follow time was 9.716026/9.874273 → 1.702707 s. Improvements do not offset other exceedances to produce an overall pass.

The measured runtime passes all [71 checks in CI #112](https://github.com/shim9610/varlit/actions/runs/41786692443) and [three checks in Sanitizers #87](https://github.com/shim9610/varlit/actions/runs/41786692417). This work adds benchmark tooling and reports without changing the product runtime.

The [searchable, sortable HTML table](report.html) supports time, CPU, RSS, and independent writer CPU metrics.

## Method

Budgets remained 4 CPU / 16 GiB / 32 GiB. Single-process workloads used the same one logical CPU; multithreaded workloads used the same four logical CPUs. Main A/B are the same binary. Six execution permutations balanced the initial six groups. If an observed metric or control exceeded 5%, one predetermined six-group extension was added and all observations were pooled. Remaining conditions continued after failures. No builds, checks, profiling, or compression ran concurrently with official timing.

Time, CPU, and output-block values use medians; peak RSS uses the maximum. A C parent forks the native executable and collects wait4 usage, avoiding Python's pre-exec memory high-water mark. Function timers and process CPU have different scopes. Map-decode CPU includes input encoding; mmap CPU includes matrix creation and initial writes. Batch write_cpu includes the writer and all readers. Only independent writer_cpu_seconds isolates the writer thread.

Cold/warm describes validation metadata, not forced eviction of the operating-system file cache. Marking measures boundary validation and deletion marking, not ordinary payload reads. Independent writers do not wait for reader acknowledgments. Batch indexed readers instead complete fixed work before acknowledgment. Indexed/matrix conditions with eight followers also include one pinned snapshot reader. Native marker publication and batch sync have different durability boundaries, so absolute throughput is not directly comparable between those families. Batch workloads reuse existing keys in the second half and delete some keys in indexed mode; they differ from logging that continually appends unique keys.

New TryClone, try_box, and shared-owner APIs are compared with main's existing Clone, Box, and construction APIs for successful results. Range streaming is compared with main's full decode followed by delivery of the same range. Equal results do not imply identical internal work or failure semantics.

Main's original scalable_load CLI lacked fixed-reader-work and writer-open options, so the initial batch comparisons could not execute. The first common-source retry also reused an old Cargo binary because the copied source retained an earlier modification time. Identical original/reused binary hashes, failures, and interruptions remain preserved. Updating the source timestamp forced a rebuild; different hashes and write/verify/open behavior were checked before rerunning all seven batch conditions. The batch results below are from the corrected comparison.

## All conditions

Time is the primary operation timer; CPU and RSS cover the entire workload process. Batch conditions use write time as the primary timer, with scan/point/open/compact stages assessed individually. Changes show the less favorable comparison against the two controls. Status combines all registered metrics and controls, so a condition can exceed a gate while its primary timer remains within the threshold.

| Condition | Groups | main A/B time (s) | PR time (s) | Time change | CPU change | RSS change | Status |
|---|---:|---:|---:|---:|---:|---:|---|
| native-fixed-2m | 6 | 0.275647/0.270712 | 0.146401 | -45.92% | -4.84% | +2.16% | Observed gates passed |
| native-variable-random | 6 | 0.042354/0.043760 | 0.043489 | +2.68% | -3.38% | +2.86% | Observed gates passed |
| native-epochs | 12 | 0.029644/0.027691 | 0.028072 | +1.38% | -0.11% | +4.27% | Unqualified: control variation |
| native-rewrite-variable | 12 | 0.039452/0.038173 | 0.039167 | +2.61% | -36.34% | -81.95% | Observed gates passed |
| native-rewrite-all-marks | 6 | 0.336539/0.337423 | 0.295580 | -12.17% | -31.52% | -78.65% | Observed gates passed |
| native-recovery | 6 | 0.539694/0.533478 | 0.329332 | -38.27% | -22.50% | -23.40% | Observed gates passed |
| native-large-4g | 6 | 0.014304/0.014350 | 0.011972 | -16.30% | -2.88% | -3.84% | Observed gates passed |
| native-lazy-4g | 12 | 0.012661/0.012702 | 0.083013 | +555.66% | +5.16% | -5.85% | Observed gate exceeded |
| native-mixed-4g | 12 | 0.035978/0.038360 | 0.036009 | +0.09% | -0.22% | -5.66% | Unqualified: control variation |
| nonresident-live | 12 | 0.043155/0.044344 | 0.054312 | +25.85% | +0.68% | +4.87% | Observed gate exceeded |
| nonresident-open-cold | 12 | 0.029554/0.029506 | 0.040698 | +37.93% | +0.67% | +7.38% | Observed gate exceeded |
| nonresident-open-warm | 12 | 0.029251/0.028319 | 0.040209 | +41.99% | +1.80% | +2.30% | Observed gate exceeded |
| maintenance-replace | 6 | 5.551626/5.752475 | 3.352498 | -39.61% | -29.61% | -86.58% | Observed gates passed |
| maintenance-defragment | 12 | 5.082675/4.984373 | 2.715357 | -45.52% | -32.81% | -90.98% | Observed gates passed |
| maintenance-complex | 6 | 0.562378/0.580768 | 0.413935 | -26.40% | -16.83% | -52.44% | Observed gates passed |
| maintenance-checkpoint | 6 | 1.754133/1.716920 | 1.258835 | -26.68% | -17.44% | -76.43% | Observed gates passed |
| maintenance-replace-4g | 6 | 4.435119/4.442255 | 3.972880 | -10.42% | -8.24% | -37.87% | Observed gates passed |
| map-encode-million | 6 | 6.628233/6.772711 | 3.586630 | -45.89% | -45.09% | -5.69% | Observed gates passed |
| chunk-compress-64m | 6 | 0.293604/0.295916 | 0.212960 | -27.47% | -20.27% | -8.96% | Observed gates passed |
| chunk-decode-64m | 6 | 0.668535/0.696741 | 0.378885 | -43.33% | -37.61% | +0.07% | Observed gates passed |
| decode-map-fixed | 12 | 0.989585/1.008166 | 1.085899 | +9.73% | -6.25% | -0.02% | Observed gate exceeded |
| decode-map-strings | 12 | 0.035841/0.033606 | 0.037272 | +10.91% | +10.04% | -1.42% | Unqualified: control variation |
| decode-map-nested | 6 | 0.069444/0.066383 | 0.058233 | -12.28% | -10.80% | -0.07% | Observed gates passed |
| owner-bytes-64m | 12 | 0.501810/0.519103 | 0.514949 | +2.62% | +2.70% | -0.11% | Observed gates passed |
| owner-bytes-1g | 6 | 0.973795/0.971090 | 0.962707 | -0.86% | +0.75% | -0.01% | Observed gates passed |
| owner-nested | 12 | 0.061892/0.061506 | 0.060544 | -1.57% | -1.67% | -1.09% | Observed gates passed |
| owner-box | 12 | 0.009273/0.009065 | 0.009577 | +5.65% | +4.89% | -10.93% | Observed gate exceeded |
| owner-shared-owner | 12 | 0.013906/0.013781 | 0.016894 | +22.59% | +22.41% | +0.33% | Observed gate exceeded |
| owner-boxed-nested | 6 | 0.051142/0.051235 | 0.051015 | -0.25% | -0.29% | -9.88% | Observed gates passed |
| native-zstd-write-r1 | 12 | 0.022335/0.021653 | 0.022434 | +3.61% | +2.68% | +0.00% | Observed gate exceeded |
| native-zstd-read-r1 | 12 | 0.030026/0.029137 | 0.027779 | -4.66% | -0.28% | +0.00% | Observed gate exceeded |
| native-zstd-read-r8 | 12 | 0.076295/0.075901 | 0.079382 | +4.59% | -0.80% | -3.13% | Observed gate exceeded |
| chunk-range-full-67108864-r1 | 6 | 0.137860/0.134452 | 0.072330 | -46.20% | -25.20% | +0.05% | Observed gates passed |
| chunk-range-stream-67108864-r1 | 6 | 0.130725/0.133476 | 0.040603 | -68.94% | -39.62% | -49.06% | Observed gates passed |
| chunk-range-stream-4k-67108864-r1 | 6 | 0.140297/0.138372 | 0.044574 | -67.79% | -39.68% | -49.03% | Observed gates passed |
| chunk-range-full-1048576-r8 | 12 | 0.357350/0.337037 | 0.193238 | -42.67% | -40.46% | +10.26% | Unqualified: control variation |
| chunk-range-stream-4k-1048576-r8 | 12 | 0.331904/0.338041 | 0.004104 | -98.76% | -96.91% | -85.50% | Observed gates passed |
| scan-open-false | 6 | 1.681199/1.674256 | 1.343104 | -19.78% | -15.97% | -20.83% | Observed gates passed |
| scan-open-true | 12 | 1.484968/1.469806 | 0.078928 | -94.63% | -93.39% | +16.85% | Observed gate exceeded |
| codec-merge-compact | 12 | 3.348148/3.361434 | 3.222706 | -3.75% | -4.90% | -10.94% | Observed gate exceeded |
| matrix-ordinary | 12 | 1.275921/1.277195 | 1.367747 | +7.20% | +8.34% | +0.04% | Observed gate exceeded |
| matrix-durable | 12 | 6.117033/6.170495 | 6.436833 | +5.23% | +8.44% | -0.29% | Observed gate exceeded |
| mmap-small-whole | 6 | 0.058229/0.057603 | 0.034021 | -40.94% | -7.78% | +1.95% | Observed gates passed |
| mmap-small-cell | 6 | 1.370311/1.330565 | 1.334760 | +0.32% | +0.26% | +0.50% | Observed gates passed |
| mmap-large-whole | 12 | 0.021156/0.020679 | 0.018533 | -10.38% | +59.69% | +0.18% | Observed gate exceeded |
| mmap-large-cell | 12 | 0.285362/0.281159 | 0.294345 | +4.69% | +35.62% | +0.33% | Observed gate exceeded |
| independent-1g-64k-r1 | 6 | 0.780190/0.792821 | 0.759812 | -2.61% | -1.27% | -2.30% | Observed gates passed |
| independent-1g-64k-r8 | 6 | 1.670692/1.720519 | 1.547675 | -7.36% | -10.61% | +3.46% | Observed gates passed |
| independent-4g-64k-r8 | 6 | 6.544542/6.482650 | 6.037066 | -6.87% | -7.08% | -3.78% | Observed gates passed |
| independent-1g-1024k-r8 | 12 | 1.429322/1.271107 | 1.337420 | +5.22% | +2.43% | +3.72% | Unqualified: control variation |
| independent-4g-1024k-r8 | 12 | 5.190846/5.022264 | 4.960879 | -1.22% | +1.31% | +2.05% | Observed gates passed |
| independent-1g-4096k-r8 | 12 | 1.652753/1.618574 | 1.623090 | +0.28% | -0.32% | +4.96% | Unqualified: control variation |
| independent-4g-4096k-r8 | 12 | 6.351363/6.867676 | 6.786216 | +6.85% | +7.30% | +0.08% | Unqualified: control variation |
| pending-small | 6 | 9.716026/9.874273 | 1.702707 | -82.48% | -84.63% | -3.44% | Observed gates passed |
| pending-large | 12 | 4.245919/4.455797 | 2.257971 | -46.82% | -53.09% | +3.67% | Unqualified: control variation |
| pending-complex | 12 | 5.695757/5.535034 | 1.490615 | -73.07% | -77.45% | -10.47% | Unqualified: control variation |
| pending-nonresident | 12 | 4.354647/4.375747 | 2.305895 | -47.05% | -52.29% | +3.31% | Observed gate exceeded |
| batch-stream-1g-64k-r0 | 12 | 0.817601/0.851170 | 0.839813 | +2.72% | +6.76% | +0.74% | Observed gate exceeded |
| batch-stream-4g-4096k-r0 | 12 | 4.082524/4.216046 | 4.398438 | +7.74% | +8.36% | +0.02% | Observed gate exceeded |
| batch-indexed-1g-64k-r0 | 12 | 0.890266/0.860364 | 0.859253 | -0.13% | -1.29% | -22.06% | Unqualified: control variation |
| batch-indexed-1g-1024k-r0 | 12 | 0.906686/0.895503 | 0.934896 | +4.40% | +5.38% | -1.26% | Unqualified: control variation |
| batch-indexed-4g-4096k-r0 | 12 | 4.134429/3.969303 | 6.283264 | +58.30% | +92.97% | +0.00% | Unqualified: control variation |
| batch-indexed-1g-64k-r8 | 12 | 0.913145/0.927977 | 0.896897 | -1.78% | -3.57% | -11.31% | Unqualified: control variation |
| batch-indexed-4g-4096k-r8 | 12 | 38.095546/38.935229 | 36.424150 | -4.39% | -5.27% | +0.01% | Unqualified: control variation |
| lazy-first-mark-4g | 12 | 0.000018/0.000018 | 0.071943 | +393407.80% | +5.92% | -0.72% | Observed gate exceeded |
| lazy-warm-marks-4g | 12 | 0.013022/0.013735 | 0.014767 | +13.40% | +4.49% | -4.20% | Unqualified: control variation |
| default-fixed-2m | 6 | 0.282942/0.280648 | 0.144546 | -48.50% | +0.85% | +1.93% | Observed gates passed |
| default-variable | 6 | 0.038832/0.039280 | 0.033980 | -12.49% | -2.59% | +3.26% | Observed gates passed |
| default-mixed-4g | 12 | 0.032720/0.038958 | 0.034022 | +3.98% | +1.62% | -6.20% | Unqualified: control variation |
| default-codec-merge | 12 | 3.549935/3.546182 | 3.408579 | -3.88% | -4.64% | -11.86% | Observed gate exceeded |
| default-map-decode | 12 | 1.033840/0.997349 | 1.087336 | +9.02% | -6.27% | -0.02% | Observed gate exceeded |
| default-owner-64m | 6 | 0.511551/0.491560 | 0.493088 | +0.31% | +0.26% | -0.09% | Observed gates passed |
| default-independent-4g-4m-r8 | 12 | 6.410026/6.896268 | 6.630141 | +3.43% | +4.65% | +0.18% | Unqualified: control variation |
| default-batch-indexed-1g-1m-r8 | 12 | 1.946340/1.889460 | 1.909761 | +1.07% | +0.37% | -0.08% | Unqualified: control variation |

## Observed exceedances by metric

Individual stages are assessed so that aggregate improvements cannot hide increases. Time and CPU are in seconds, RSS in KiB, and writes/temporary storage in bytes. A change from zero to a positive value has no finite percentage. Output blocks are operating-system-accounted writes and include preparation, validation, and logs. A few KiB of extra writes for small compressed files are not an elapsed-time increase.

| Condition | Metric | main A/B | PR | vs A/B |
|---|---|---:|---:|---:|
| native-lazy-4g | seconds | 0.012661/0.012702 | 0.083013 | +555.66%/+553.54% |
| native-lazy-4g | cpu | 1.713142/1.775616 | 1.801609 | +5.16%/+1.46% |
| native-lazy-4g | private_bytes | 0.000000/0.000000 | 523776.000000 | Added/Added |
| nonresident-live | seconds | 0.043155/0.044344 | 0.054312 | +25.85%/+22.48% |
| nonresident-live | private_bytes | 0.000000/0.000000 | 524288.000000 | Added/Added |
| nonresident-open-cold | seconds | 0.029554/0.029506 | 0.040698 | +37.71%/+37.93% |
| nonresident-open-cold | rss_kib | 2548.000000/2612.000000 | 2736.000000 | +7.38%/+4.75% |
| nonresident-open-cold | private_bytes | 0.000000/0.000000 | 524288.000000 | Added/Added |
| nonresident-open-warm | seconds | 0.029251/0.028319 | 0.040209 | +37.46%/+41.99% |
| nonresident-open-warm | private_bytes | 0.000000/0.000000 | 524288.000000 | Added/Added |
| decode-map-fixed | seconds | 0.989585/1.008166 | 1.085899 | +9.73%/+7.71% |
| decode-map-strings | seconds | 0.035841/0.033606 | 0.037272 | +3.99%/+10.91% |
| decode-map-strings | cpu | 0.045262/0.042136 | 0.046365 | +2.44%/+10.04% |
| owner-box | seconds | 0.009273/0.009065 | 0.009577 | +3.28%/+5.65% |
| owner-shared-owner | seconds | 0.013906/0.013781 | 0.016894 | +21.48%/+22.59% |
| owner-shared-owner | cpu | 0.014779/0.014721 | 0.018021 | +21.94%/+22.41% |
| native-zstd-write-r1 | write_bytes | 36864.000000/36864.000000 | 40960.000000 | +11.11%/+11.11% |
| native-zstd-read-r1 | write_bytes | 36864.000000/36864.000000 | 43008.000000 | +16.67%/+16.67% |
| native-zstd-read-r8 | write_bytes | 36864.000000/38912.000000 | 40960.000000 | +11.11%/+5.26% |
| chunk-range-full-1048576-r8 | rss_kib | 567740.000000/505736.000000 | 557640.000000 | -1.78%/+10.26% |
| scan-open-true | rss_kib | 3408.000000/3348.000000 | 3912.000000 | +14.79%/+16.85% |
| codec-merge-compact | encode/decode fixed_seconds | 0.005229/0.005439 | 0.005895 | +12.73%/+8.38% |
| codec-merge-compact | encode/decode variable_seconds | 0.051478/0.051985 | 0.056968 | +10.66%/+9.59% |
| matrix-ordinary | seconds | 1.275921/1.277195 | 1.367747 | +7.20%/+7.09% |
| matrix-ordinary | cpu | 2.820111/2.852616 | 3.055366 | +8.34%/+7.11% |
| matrix-durable | seconds | 6.117033/6.170495 | 6.436833 | +5.23%/+4.32% |
| matrix-durable | cpu | 3.591934/3.611562 | 3.895112 | +8.44%/+7.85% |
| mmap-large-whole | cpu | 0.259254/0.263433 | 0.414014 | +59.69%/+57.16% |
| mmap-large-cell | cpu | 0.506854/0.513836 | 0.687403 | +35.62%/+33.78% |
| independent-1g-1024k-r8 | seconds | 1.429322/1.271107 | 1.337420 | -6.43%/+5.22% |
| independent-1g-4096k-r8 | writer_seconds | 1.001859/0.892238 | 0.998015 | -0.38%/+11.86% |
| independent-1g-4096k-r8 | writer_cpu_seconds | 0.534951/0.444425 | 0.522181 | -2.39%/+17.50% |
| independent-4g-4096k-r8 | seconds | 6.351363/6.867676 | 6.786216 | +6.85%/-1.19% |
| independent-4g-4096k-r8 | cpu | 24.597430/26.599675 | 26.394239 | +7.30%/-0.77% |
| independent-4g-4096k-r8 | writer_seconds | 3.960280/3.702244 | 4.350410 | +9.85%/+17.51% |
| independent-4g-4096k-r8 | writer_cpu_seconds | 1.941319/1.929754 | 2.050295 | +5.61%/+6.25% |
| pending-nonresident | private_bytes | 0.000000/0.000000 | 131072.000000 | Added/Added |
| batch-stream-1g-64k-r0 | write_cpu | 0.403319/0.407197 | 0.430582 | +6.76%/+5.74% |
| batch-stream-4g-4096k-r0 | write_seconds | 4.082524/4.216046 | 4.398438 | +7.74%/+4.33% |
| batch-stream-4g-4096k-r0 | write_cpu | 2.691000/2.757665 | 2.915910 | +8.36%/+5.74% |
| batch-indexed-1g-64k-r0 | writer-open_seconds | 0.001932/0.001752 | 0.002218 | +14.78%/+26.58% |
| batch-indexed-1g-64k-r0 | writer-open_cpu | 0.006519/0.005859 | 0.006533 | +0.21%/+11.50% |
| batch-indexed-1g-64k-r0 | writer-open_write_bytes | 135168.000000/135168.000000 | 266240.000000 | +96.97%/+96.97% |
| batch-indexed-1g-1024k-r0 | write_cpu | 0.549669/0.550888 | 0.579248 | +5.38%/+5.15% |
| batch-indexed-1g-1024k-r0 | writer-open_seconds | 0.001899/0.001993 | 0.002162 | +13.84%/+8.45% |
| batch-indexed-1g-1024k-r0 | writer-open_write_bytes | 135168.000000/135168.000000 | 266240.000000 | +96.97%/+96.97% |
| batch-indexed-4g-4096k-r0 | write_seconds | 4.134429/3.969303 | 6.283264 | +51.97%/+58.30% |
| batch-indexed-4g-4096k-r0 | write_cpu | 2.795770/2.550543 | 4.921743 | +76.04%/+92.97% |
| batch-indexed-4g-4096k-r0 | writer-open_seconds | 0.001857/0.002121 | 0.002225 | +19.79%/+4.88% |
| batch-indexed-4g-4096k-r0 | writer-open_cpu | 0.185803/0.201425 | 0.200001 | +7.64%/-0.71% |
| batch-indexed-4g-4096k-r0 | writer-open_write_bytes | 135168.000000/135168.000000 | 266240.000000 | +96.97%/+96.97% |
| batch-indexed-4g-4096k-r0 | compact_cpu | 0.183390/0.192310 | 0.192935 | +5.21%/+0.33% |
| batch-indexed-1g-64k-r8 | writer-open_seconds | 0.001871/0.001888 | 0.002208 | +18.02%/+16.95% |
| batch-indexed-1g-64k-r8 | writer-open_cpu | 0.006183/0.005931 | 0.006382 | +3.21%/+7.61% |
| batch-indexed-1g-64k-r8 | writer-open_write_bytes | 135168.000000/135168.000000 | 266240.000000 | +96.97%/+96.97% |
| batch-indexed-4g-4096k-r8 | writer-open_seconds | 0.001854/0.001816 | 0.002135 | +15.17%/+17.53% |
| batch-indexed-4g-4096k-r8 | writer-open_write_bytes | 135168.000000/135168.000000 | 266240.000000 | +96.97%/+96.97% |
| batch-indexed-4g-4096k-r8 | compact_seconds | 0.002800/0.002769 | 0.003060 | +9.29%/+10.52% |
| batch-indexed-4g-4096k-r8 | compact_cpu | 0.196008/0.190927 | 0.202012 | +3.06%/+5.81% |
| lazy-first-mark-4g | seconds | 0.000018/0.000018 | 0.071943 | +391150.07%/+393407.80% |
| lazy-first-mark-4g | cpu | 1.714028/1.665049 | 1.763573 | +2.89%/+5.92% |
| lazy-first-mark-4g | private_bytes | 0.000000/0.000000 | 523776.000000 | Added/Added |
| lazy-warm-marks-4g | seconds | 0.013022/0.013735 | 0.014767 | +13.40%/+7.52% |
| lazy-warm-marks-4g | private_bytes | 0.000000/0.000000 | 523776.000000 | Added/Added |
| default-codec-merge | encode/decode fixed_seconds | 0.005267/0.005470 | 0.005821 | +10.51%/+6.41% |
| default-map-decode | seconds | 1.033840/0.997349 | 1.087336 | +5.17%/+9.02% |
| default-independent-4g-4m-r8 | writer_seconds | 3.614014/3.885680 | 3.885099 | +7.50%/-0.01% |
| default-batch-indexed-1g-1m-r8 | writer-open_seconds | 0.002020/0.001776 | 0.002327 | +15.24%/+31.06% |
| default-batch-indexed-1g-1m-r8 | writer-open_cpu | 0.052210/0.048876 | 0.053971 | +3.37%/+10.43% |
| default-batch-indexed-1g-1m-r8 | writer-open_write_bytes | 135168.000000/135168.000000 | 266240.000000 | +96.97%/+96.97% |
| default-batch-indexed-1g-1m-r8 | compact_seconds | 0.002660/0.002443 | 0.002565 | -3.54%/+5.03% |
| default-batch-indexed-1g-1m-r8 | compact_cpu | 0.049301/0.049792 | 0.053870 | +9.27%/+8.19% |

## Separate storage observations

Separate probes ran after official timings for every condition and version. Path/file-descriptor sampling at 5 ms intervals is non-atomic and may miss brief peaks. Fixed input fixtures and logs are excluded from workload data totals. Cgroup memory includes page cache, workspace, and the observer; it is not library heap usage.

| Condition | main/PR peak observed file allocation (MiB) | main/PR deleted open files (MiB) | Status |
|---|---:|---:|---|
| native-fixed-2m | 167.340/161.340 | 30.000/24.000 | Observed |
| native-variable-random | 44.543/44.543 | 2.000/2.000 | Observed |
| native-epochs | 15.777/15.777 | 2.000/2.000 | Observed |
| native-rewrite-variable | 87.078/88.777 | 2.000/3.699 | Observed |
| native-rewrite-all-marks | 87.078/88.777 | 44.535/45.730 | Observed |
| native-recovery | 27.738/27.738 | 14.000/14.000 | Observed |
| native-large-4g | 4101.512/4101.512 | 0.000/0.000 | Observed |
| native-lazy-4g | 4101.512/4102.012 | 0.000/0.500 | Observed |
| native-mixed-4g | 4108.949/4106.949 | 2.000/0.000 | Observed |
| nonresident-live | 2053.480/2053.980 | 0.000/0.500 | Observed |
| nonresident-open-cold | 2053.480/2053.980 | 0.000/0.500 | Observed |
| nonresident-open-warm | 2053.480/2053.980 | 0.000/0.500 | Observed |
| maintenance-replace | 299.781/297.781 | 173.891/171.891 | Observed |
| maintenance-defragment | 299.781/297.781 | 173.891/171.891 | Observed |
| maintenance-complex | 87.699/87.699 | 49.070/49.070 | Observed |
| maintenance-checkpoint | 209.172/207.168 | 120.016/118.012 | Observed |
| maintenance-replace-4g | 8213.027/8213.027 | 4108.508/4108.508 | Observed |
| map-encode-million | 0.000/0.000 | 0.000/0.000 | Observed |
| chunk-compress-64m | 0.000/0.000 | 0.000/0.000 | Observed |
| chunk-decode-64m | 0.000/0.000 | 0.000/0.000 | Observed |
| decode-map-fixed | 0.000/0.000 | 0.000/0.000 | Observed |
| decode-map-strings | 0.000/0.000 | 0.000/0.000 | Observed |
| decode-map-nested | 0.000/0.000 | 0.000/0.000 | Observed |
| owner-bytes-64m | 0.000/0.000 | 0.000/0.000 | Observed |
| owner-bytes-1g | 0.000/0.000 | 0.000/0.000 | Observed |
| owner-nested | 0.000/0.000 | 0.000/0.000 | Observed |
| owner-box | 0.000/0.000 | 0.000/0.000 | Observed |
| owner-shared-owner | 0.000/0.000 | 0.000/0.000 | Observed |
| owner-boxed-nested | 0.000/0.000 | 0.000/0.000 | Observed |
| native-zstd-write-r1 | 0.031/0.027 | 0.000/0.000 | Observed |
| native-zstd-read-r1 | 0.031/0.027 | 0.000/0.000 | Observed |
| native-zstd-read-r8 | 0.031/0.031 | 0.000/0.000 | Observed |
| chunk-range-full-67108864-r1 | 0.000/0.000 | 0.000/0.000 | Observed |
| chunk-range-stream-67108864-r1 | 0.000/0.000 | 0.000/0.000 | Observed |
| chunk-range-stream-4k-67108864-r1 | 0.000/0.000 | 0.000/0.000 | Observed |
| chunk-range-full-1048576-r8 | 0.000/0.000 | 0.000/0.000 | Observed |
| chunk-range-stream-4k-1048576-r8 | 0.000/0.000 | 0.000/0.000 | Observed |
| scan-open-false | 29.734/29.734 | 16.000/16.000 | Observed |
| scan-open-true | 0.668/0.668 | 0.000/0.000 | Observed |
| codec-merge-compact | 114.031/114.031 | 0.000/0.000 | Observed |
| matrix-ordinary | 106.070/106.066 | 73.754/73.754 | Observed |
| matrix-durable | 173.871/173.871 | 141.555/141.555 | Observed |
| mmap-small-whole | 0.551/0.551 | 0.000/0.000 | Observed |
| mmap-small-cell | 0.551/0.551 | 0.000/0.000 | Observed |
| mmap-large-whole | 32.316/32.316 | 0.000/0.000 | Observed |
| mmap-large-cell | 32.316/32.316 | 0.000/0.000 | Observed |
| independent-1g-64k-r1 | 1025.391/1025.391 | 0.000/0.000 | Observed |
| independent-1g-64k-r8 | 1025.391/1025.391 | 0.000/0.000 | Observed |
| independent-4g-64k-r8 | 4101.527/4101.527 | 0.000/0.000 | Observed |
| independent-1g-1024k-r8 | 1024.102/1024.102 | 0.000/0.000 | Observed |
| independent-4g-1024k-r8 | 4096.371/4096.371 | 0.000/0.000 | Observed |
| independent-1g-4096k-r8 | 1024.035/1024.035 | 0.000/0.000 | Observed |
| independent-4g-4096k-r8 | 4096.113/4096.113 | 0.000/0.000 | Observed |
| pending-small | 45.008/27.008 | 18.000/0.000 | Observed |
| pending-large | 1025.387/1025.387 | 0.000/0.000 | Observed |
| pending-complex | 261.512/261.512 | 0.000/0.000 | Observed |
| pending-nonresident | 1025.387/1025.512 | 0.000/0.125 | Observed |
| batch-stream-1g-64k-r0 | 1026.184/1026.180 | 0.000/0.000 | Observed |
| batch-stream-4g-4096k-r0 | 4096.258/4096.258 | 0.000/0.000 | Observed |
| batch-indexed-1g-64k-r0 | 1039.305/1039.961 | 0.000/0.000 | Observed |
| batch-indexed-1g-1024k-r0 | 1024.695/1024.621 | 0.000/0.000 | Observed |
| batch-indexed-4g-4096k-r0 | 4097.512/4097.590 | 0.000/0.000 | Observed |
| batch-indexed-1g-64k-r8 | 1039.305/1039.965 | 0.000/0.000 | Observed |
| batch-indexed-4g-4096k-r8 | 4097.516/4097.582 | 0.000/0.000 | Observed |
| lazy-first-mark-4g | 4101.512/4101.996 | 0.000/0.484 | Observed |
| lazy-warm-marks-4g | 4101.512/4102.012 | 0.000/0.500 | Observed |
| default-fixed-2m | 167.340/161.340 | 30.000/24.000 | Observed |
| default-variable | 44.543/44.543 | 2.000/2.000 | Observed |
| default-mixed-4g | 4108.949/4106.949 | 2.000/0.000 | Observed |
| default-codec-merge | 114.031/114.031 | 0.000/0.000 | Observed |
| default-map-decode | 0.000/0.000 | 0.000/0.000 | Observed |
| default-owner-64m | 0.000/0.000 | 0.000/0.000 | Observed |
| default-independent-4g-4m-r8 | 4096.113/4096.113 | 0.000/0.000 | Observed |
| default-batch-indexed-1g-1m-r8 | 1024.621/1024.625 | 0.000/0.000 | Observed |

## Interpretation and evidence

Observed regressions and control variation are distinguished, with exceedances retained. Validation and resource limits were not removed to obtain passes, and product code was not optimized during this comparison. Optimization follow-up remains in [issue #20](https://github.com/shim9610/varlit/issues/20).

Physical data-record counts differ from aggregate reader decodes. For native independent 4 GiB cases, 64 KiB payloads produce 65,536 records, 1 MiB produces 4,096, and 4 MiB produces 1,024; every reader consumes the whole file. BatchOptions coalesces I/O across logical records without merging the records themselves.

Detailed work counts, controls, and supplementary 95% matched-group bootstrap intervals are in [results.json](results.json). All attempts, sources, tools, and binary hashes are retained in the [raw evidence](evidence.tar.gz), [per-file hashes](evidence-manifest.json), and [archive checksum](evidence.sha256). Bootstrap intervals do not replace the observed 5% gates.
