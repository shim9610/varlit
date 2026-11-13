#!/usr/bin/env python3
"""Small-budget native I/O regressions; no endurance/performance claims."""

import argparse
import datetime
import json
import os
from pathlib import Path
import platform
import sys

from qualify_scalable import capture, classify, run_command, test_command


# Exact names and pass counts ensure a renamed or feature-disabled test cannot
# silently turn this platform gate into a successful zero-test run.
CASES = [
    ("matrix-tile-overwrite", "matrix_tiles",
     "overwrites_keep_the_file_size_and_new_tiles_add_one_tile"),
    ("matrix-tile-damage", "matrix_tiles",
     "damage_in_place_is_reported_and_a_category_clear_recovers"),
    ("native-directory-spill", "native_budgeted_directory",
     "tiny_directory_budget_covers_append_dirty_open_follow_and_reopen"),
    ("native-directory-memory", "record_index_residency",
     "spilled_open_peak_and_retained_memory_do_not_scale_with_record_count"),
    ("segment-link-buffer", "record_index_residency",
     "segment_link_staging_obeys_the_buffer_before_falling_back"),
    ("external-merge", "roundtrip",
     "external_merge_spills_and_preserves_shard_ops_deletes_and_atomic_output"),
    ("matrix-directory-follow", "matrix_generations",
     "tiny_bitmap_budget_follows_growing_and_shrinking_indexes"),
    ("large-logical-matrix", "matrix_generations",
     "default_matrix_admission_is_independent_of_the_working_set"),
    ("temp-directory", None,
     "spill_rows::tests::reads_between_spills_do_not_move_the_append_position"),
    ("private-matrix-spill", None,
     "matrix_tiles::tests::a_publication_larger_than_a_frame_spills_and_commits_whole"),
    ("abandoned-matrix-spill", None,
     "matrix_tiles::tests::discarded_changes_never_reach_a_reader_or_the_writer"),
    ("layout-directory", "layout_writer_open_residency",
     "tiny_directory_budget_spills_and_does_not_limit_append_or_lookup"),
    ("keyed-tail-spill", "resident_contracts",
     "generated_keyed_writer_spills_tails_for_puts_and_tombstones"),
    ("long-key-spill", "high_cardinality",
     "a_generated_keyed_writer_spills_long_keys_and_keeps_tombstone_predecessors"),
    ("record-window", "mmap_zero_copy",
     "individual_mapping_works_without_a_resident_directory_or_whole_file_budget"),
    ("matrix-window", "matrix_generations",
     "matrix_cell_mapping_charges_only_its_window_and_cow_pages"),
    ("diagnostic-budget", "native_diagnostics_security",
     "diagnostics_bound_each_live_payload_instead_of_the_entire_scan"),
    ("matrix-follow", "matrix_generations",
     "readers_keep_one_generation_across_concurrent_overwrites_and_clear"),
    ("matrix-checkpoint", None,
     "matrix_tiles::tests::a_reader_keeps_its_snapshot_across_publications_and_checkpoints"),
    ("torn-matrix-publication", "matrix_tiles",
     "unpublished_and_torn_publications_never_reach_a_reopen"),
    ("indexed-follow", "scalable_reader_follow",
     "eight_followers_observe_atomic_generations_during_single_writer_updates"),
    ("process-compaction", "scalable_reader_management",
     "independent_process_reads_dirty_writer_and_follows_compaction"),
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path("target/platform-io"))
    parser.add_argument("--timeout", type=int, default=900,
                        help="seconds per test command, including first compilation")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    args.output.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "CARGO_BUILD_JOBS": "4", "RUST_TEST_THREADS": "4",
           "CARGO_INCREMENTAL": "0", "CARGO_TERM_COLOR": "never"}
    report = {
        "started_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "head": capture(["git", "rev-parse", "HEAD"]),
        "dirty": bool(capture(["git", "status", "--porcelain"])),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "rustc": capture(["rustc", "-vV"]),
        "scope": "native small-budget I/O correctness; not endurance or throughput",
        "cases": [],
    }
    summary = args.output / "summary.json"
    for name, target, test in CASES:
        command = test_command(target, test, core=target is None)
        log = args.output / f"{name}.log"
        print(f"RUN {name}", flush=True)
        code, timed_out, seconds = run_command(command, env, log, args.timeout)
        status, reason = classify(code, log.read_text(encoding="utf-8", errors="replace"), 1,
                                  timed_out=timed_out)
        report["cases"].append({"name": name, "test": test, "command": command,
                                "status": status, "reason": reason, "exit_code": code,
                                "seconds": seconds, "log": log.name})
        summary.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(f"{status.upper()} {name} ({seconds}s): {reason or '1 test, cleanup verified'}",
              flush=True)
        if status != "passed":
            # Retain the full log but also put enough failure context in Actions.
            print(log.read_text(encoding="utf-8", errors="replace")[-12000:], flush=True)
    return int(any(case["status"] != "passed" for case in report["cases"]))


if __name__ == "__main__":
    sys.exit(main())
