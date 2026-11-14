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
    ("keyed-spill-refusal", "varlit-core", None, "file::tests::keyed_tail_spill_allocation_failures_preserve_entries_and_retry", False, None),
    ("strict-lazy-choice", "varlit", "open_digest", "strict_lazy_failure_is_typed_and_scan_is_an_explicit_choice", False, None),
    ("strict-lazy-corruption", "varlit", "open_digest", "strict_lazy_bad_digest_returns_error_without_scanning", False, None),
    ("strict-lazy-header", "varlit", "header_tail_region", "strict_cold_header_returns_its_source_without_scanning", False, None),
    ("compound-read", "varlit-core", None, "disk_index::tree::corruption_guards::hostile_child_with_short_reads_io_errors_and_allocation_refusal_recovers", False, "COMPOUND_READ trials=160 retries=160 bounded=true strict_and_trusted=true"),
    ("failed-swmr", "varlit", "publication_failure_results", "enabled::failed_writer_preserves_concurrent_readers_and_retry_after_recovery", False, "FAILED_SWMR readers=8 probes=2048 retained_cursors=8 recovered=true"),
    ("snapshot-owner-release", "varlit-core", None, "disk_index::storage::failure_tests::failed_resume_releases_writer_lock_while_snapshot_owners_survive", False, None),
    ("portable-head-parser", "varlit-core", None, "disk_index::storage::failure_tests::head_parser_rejects_torn_and_hostile_words", False, None),
    ("ownership-deferred-box", "varlit-core", "fallible_ownership", "reserved_boxes_skip_initialization_on_refusal_and_release_failed_initializers", False, None),
    ("ownership-box-order", "varlit-core", "fallible_ownership", "boxed_clone_reserves_owner_before_cloning_payload", False, None),
    ("ownership-inplace-box", "varlit-core", "fallible_ownership", "uninitialized_large_box_can_be_filled_on_a_small_stack", False, None),
    ("ownership-deferred-arc", "varlit-core", None, "fallible::tests::shared_initialization_and_cow_reserve_before_calling_user_code", False, None),
    ("ownership-generated", "varlit", "fallible_publication", "generated_layout_fields_refuse_before_creating_the_file", False, None),
    ("ownership-path-error", "varlit-core", None, "fallible::tests::publication_path_and_source_refusals_keep_the_same_outcome", False, None),
    ("ownership-disk-error", "varlit-core", None, "fallible::tests::disk_error_wrapper_refusal_keeps_publication_facts", False, None),
    ("ownership-clone", "varlit-core", "fallible_ownership", "fallible_clones_release_partial_output_and_preserve_sources", False, "TRY_CLONE refusal_points=47 partial_cleanup=true source_unchanged=true"),
    ("ownership-box", "varlit-core", "fallible_ownership", "boxes_preserve_alignment_drop_once_and_clean_up_partial_clones", False, None),
    ("ownership-arc", "varlit-core", None, "fallible::tests::arc_refusal_drops_input_once_and_shared_cow_is_transactional", False, None),
    ("ownership-arc-threads", "varlit-core", None, "fallible::tests::shared_owners_across_threads_destroy_the_payload_once", False, None),
    ("ownership-token", "varlit-core", "fallible_ownership", "cancellation_token_refusal_and_retry", False, None),
    ("ownership-preflight", "varlit", "fallible_publication", "error_reservation_refusal_happens_before_matrix_mutation", False, None),
    ("ownership-published", "varlit", "fallible_publication", "failed_error_boxes_preserve_the_committed_matrix_event", False, None),
    ("ownership-mmap", "varlit", "fallible_publication", "mmap_layout_clone_refusal_preserves_the_reader_and_retries", False, "original_readable=true all_retried=true"),

    ("chunk-stream-window", "varlit-core", "chunk_stream", "enabled::legacy_windows_and_large_output_shortcuts_cannot_bypass_the_cap", False, None),
    ("chunk-stream-refusal", "varlit-core", "decompression_allocation", "range_streaming_never_requests_a_rust_output_allocation", False, None),
    ("chunk-stream-fallback", "varlit-core", "chunk_stream", "enabled::fallback_streams_requested_ranges_with_one_byte_scratch", False, None),
    ("chunk-stream-integrity", "varlit-core", "chunk_stream", "enabled::validates_entire_selected_chunk_including_suffix_outside_range", False, None),
    ("decoder-refusal", "varlit-core", "decoder_allocation_failures", "every_builtin_decoder_allocation_failure_returns_and_releases", False, "DECODER_OOM fixtures=20 refusal_points=122 all_retried=true"),
    ("chunk-refusal", "varlit-core", "decoder_allocation_failures", "chunk_output_allocation_failure_returns_and_retries", False, "CHUNK_OOM refusal_points=2 all_retried=true"),
    ("hashmap-rehash-budget", "varlit-core", "decoder_allocation_failures", "hash_map_rehash_peak_fits_charged_materialization", False, "HASH_REHASH entries=4096"),
    ("hostile-follow", "varlit", "index_corruption_guards", "failed_follow_of_checksums_valid_metadata_preserves_real_readers", False, "HOSTILE_FOLLOW valid_crc_fields=3 followers=8 pinned=1 retries=24"),
    ("hostile-codec", "varlit-core", "decompression_allocation", "hostile_frame_and_container_metadata_stays_bounded", False, "HOSTILE_METADATA frames=2576 containers=80 large_requests=0"),
    ("hostile-index", "varlit-core", None, "disk_index::tree::corruption_guards::checksummed_hostile_heights_and_counts_terminate", False, "HOSTILE_INDEX valid_crc_cases=1590 delivered=0"),
    ("spill-policy", "varlit-core", None, "spill_file::tests::isolated_policy_preserves_paths_limits_and_retry_state", False, "SPILL_POLICY isolated path, capacity and retry cases=8"),
    ("spill-byte-policy", "varlit-core", None, "file::external_merge::tests::spill_byte_policy_failure_keeps_the_existing_prefix", False, None),
    ("zstd-header-allocation", "varlit-core", "decompression_allocation", "untrusted_sizes_do_not_request_the_declared_maximum", False, None),
    ("chunk-header-allocation", "varlit-core", "decompression_allocation", "chunk_headers_do_not_authorize_container_allocation", False, None),
    ("publication-resume", "varlit-core", None, "disk_index::storage::failure_tests::divergent_clean_heads_require_sync_before_writer_resume", False, None),
    ("publication-clean-resume", "varlit-core", None, "disk_index::storage::failure_tests::matching_clean_heads_do_not_add_a_resume_sync", False, None),
    ("publication-head-pairs", "varlit-core", None, "disk_index::storage::failure_tests::independently_torn_heads_never_mix_publication_generations", False, "PUBLICATION_IMAGES independent head prefixes=8450"),
    ("publication-histories", "varlit", "scalable_crash_faults", "enabled::histories::bounded_and_generated_histories_preserve_committed_snapshots", False, "PUBLICATION_HISTORIES exhaustive=512 seeded=16 steps=2304"),
    ("publication-live-crashes", "varlit", "scalable_crash_faults", "enabled::histories::every_mixed_history_crash_boundary_preserves_live_readers_and_recovery", False, "PUBLICATION_CRASHES boundaries="),
    ("mutation-rewritten-boundaries", "varlit", "record_liveness", "rewritten_boundaries_preserve_exact_membership_across_small_pages", False, None),
    ("mutation-uniform-proof", "varlit-core", None, "file::resident_index::tests::uniform_prefix_never_authorizes_unobserved_or_truncated_offsets", False, None),
    ("mutation-amortized-work", "varlit", "record_liveness", "boundary_proof_reuses_reverse_random_repeated_and_failed_requests", False, None),
    ("recovery-linear-work", "varlit", "record_liveness", "marked_tail_recovery_frames_linearly_at_two_cardinalities", False, None),
    ("native-boundary-proof", "varlit-core", None, "file::tests::serialized_directory_cannot_authorize_marking_an_embedded_record", False, None),
    ("rewrite-source-integrity", "varlit", "record_liveness", "defragment_refuses_payload_damage_without_republishing", False, None),
    ("matrix-mmap-window", "varlit", "matrix", "matrix_mmap_payload_window_is_checked_and_snapshot_based", False, None),
    ("nonresident-record-budget", "varlit", "index_residency", "nonresident_records_still_count_toward_the_explicit_record_limit", False, None),
    ("nonresident-follow-sequence", "varlit", "index_residency", "follow_refuses_nonresident_sequences_older_than_its_snapshot", False, None),
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
