#!/usr/bin/env python3
"""Required failure/resource contracts; exact selections reject empty suites."""
import argparse
import json
import os
from pathlib import Path
import platform
import sys

from qualify_scalable import ROOT, classify, run_command, source_identity

CASES = [
    ("index-cache-shape", "varlit", "index_corruption_guards", "aliased_finite_extents_return_errors_through_public_reads", False, None),
    ("index-traversal", "varlit-core", None, "disk_index::tree::corruption_guards::duplicated_subtrees_stop_before_repeated_leaf_delivery", False, None),
    ("index-oom", "varlit", "index_oom_guards", "public_index_reads_return_oom_and_remain_usable_after_each_refusal", False, "INDEX_OOM every cold lookup allocation refused and retried"),
    ("kernel-short-write", "varlit-core", "io_failure_recovery", "partial_kernel_writes_preserve_retry_and_reopen", False, "kernel short-write recovery trials=28"),
    ("torn-publication", "varlit-core", None, "disk_index::storage::failure_tests::torn_publication_heads_never_invent_a_confirmed_generation", False, "torn head prefixes=130"),
    ("consistent-frontiers", "varlit-core", None, "disk_index::storage::failure_tests::valid_checksums_do_not_admit_inconsistent_checkpoint_frontiers", False, None),
    ("checked-chain-links", "varlit-core", None, "file::validation_tests::checksummed_chain_links_must_decrease_and_stay_in_their_block", False, None),
    ("failed-history", "varlit", "stateful_failure_guards", "failed_batches_recover_through_delete_reinsert_follow_and_compaction", False, "stateful failure epochs=24"),
    ("decode-memory", "varlit", "resource_amplification_guards", "decoded_memory_is_bounded_and_released_across_sizes_and_failures", False, "DECODE_RSS peak_kib="),
    ("oom-recovery", "varlit", "oom_recovery_guards", "allocations_refuse_cleanly_and_rewrite_scratch_stays_bounded", False, "OOM_GUARDS allocation refusal"),
    ("zstd-context-oom", "varlit", "oom_recovery_guards", "zstd_context_allocation_failures_return_errors", True, "C_CONTEXT_REFUSAL compression and decompression passed"),
    ("cardinality-growth", "varlit", "high_cardinality", "cardinality_growth_respects_memory_budget", True, 'QUAL_MEMORY {"keys":1000000,'),
]


def command(case):
    _, package, target, test, ignored, _ = case
    result = ["cargo", "run", "--locked", "-p", "varlit-test-runner", "--",
              "test", "--locked", "--release", "-p", package, "--all-features"]
    result += ["--test", target] if target else ["--lib"]
    result += [test, "--", "--exact", "--nocapture", "--test-threads=1"]
    if ignored:
        result.append("--ignored")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/storage-contracts")
    parser.add_argument("--timeout", type=int, default=1200)
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("kernel short-write qualification requires Linux")
    if args.timeout <= 0:
        parser.error("positive timeout required")
    args.output.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "CARGO_INCREMENTAL": "0", "VARLIT_SKIP_UI_SNAPSHOTS": "1"}
    report = {"source": source_identity(), "platform": platform.platform(), "cases": []}
    for case in CASES:
        name, _, _, _, _, marker = case
        log = args.output / f"{name}.log"
        print(f"RUN {name}", flush=True)
        case_env = {**env, **({"VARLIT_ENFORCE_DECODE_RSS": "1"} if name == "decode-memory" else {})}
        code, timed_out, seconds = run_command(command(case), case_env, log, args.timeout)
        output = log.read_text(errors="replace")
        status, reason = classify(code, output, 1, marker, timed_out)
        report["cases"].append({"name": name, "command": command(case), "status": status,
                                "reason": reason, "seconds": seconds, "log": log.name})
        (args.output / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"{status.upper()} {name}: {reason or 'contract executed'}", flush=True)
        if status != "passed":
            print(output[-10000:], flush=True)
    return int(any(row["status"] != "passed" for row in report["cases"]))


if __name__ == "__main__":
    sys.exit(main())
