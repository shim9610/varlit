#!/usr/bin/env python3
"""Compare repeated equal-work load reports; reject saturation/unequal workloads."""
import argparse
import json
from pathlib import Path
import statistics


def samples(reports):
    result = []
    for report in reports:
        if report.get("status") != "passed":
            raise ValueError("load report did not pass its content/workload oracles")
        for case in report["cases"]:
            if case.get("status") != "passed" or case.get("write_workload") != "fixed-reader-work":
                raise ValueError("equal-work comparison requires fixed-reader-work reports")
            if not case["config"].get("checks_per_epoch"):
                raise ValueError("missing fixed reader count")
            writes = [stage for stage in case["stages"] if stage["phase"] == "write"]
            if len(writes) != 1 or writes[0]["status"] != "passed":
                raise ValueError("missing successful write stage")
            write = writes[0]
            readers = sorted((event for event in write["events"] if event["event"] == "reader"),
                             key=lambda event: event["id"])
            count = case["config"]["readers"] + case["config"].get("pinned", 1)
            batches = report["total_payload_bytes_per_case"] // report["epoch_bytes"] // 2
            if [row["id"] for row in readers] != list(range(count)):
                raise ValueError("reader set mismatch")
            if any(row["batches"] != batches or row["latency"]["count"] != batches * case["config"]["checks_per_epoch"]
                   for row in readers):
                raise ValueError("reader operation count mismatch")
            # Case dictionaries omit defaults. Compare actual arguments too,
            # so changing an implicit cache/batch limit cannot escape this gate.
            command = write["command"]
            arguments = command[command.index("--phase"):]
            if len(arguments) % 2:
                raise ValueError("malformed workload arguments")
            effective = dict(zip(arguments[::2], arguments[1::2]))
            effective.pop("--root", None)
            signature = {
                "config": case["config"],
                "effective_arguments": effective,
                "payload": report["total_payload_bytes_per_case"], "epoch": report["epoch_bytes"],
                "platform": report["platform"], "cpu_max": report["cpu_max"],
                "memory_max": report["memory_max"], "instrumentation": report["instrumentation"],
                "readers": [(row["id"], row["snapshots"], row["batches"], row["latency"]["count"], row["trace_hash"])
                            for row in readers],
            }
            metrics = {
                "wall_seconds": write["elapsed_seconds"],
                "cpu_seconds": write["user_seconds"] + write["system_seconds"],
                "observed_peak_rss_bytes": write["peak_rss_bytes"],
                "write_bytes": write["child_write_bytes"],
            }
            if any(value <= 0 for value in metrics.values()):
                raise ValueError("missing positive resource measurement")
            result.append((signature, metrics))
    return result


def compare(before, after, limit_percent=5):
    if not 0 <= limit_percent < 100:
        raise ValueError("regression limit must be between 0 and 100 percent")
    baseline, candidate = samples(before), samples(after)
    if len(baseline) != len(candidate) or len(baseline) < 3:
        raise ValueError("need equal sample counts, at least three per version")
    signature = baseline[0][0]
    if any(key != signature for key, _ in baseline + candidate):
        raise ValueError("workload, reader trace or environment differs between samples")
    metrics = {}
    for name in baseline[0][1]:
        left = [row[name] for _, row in baseline]
        right = [row[name] for _, row in candidate]
        # A memory peak must not be hidden by a median; timings/I/O use medians.
        aggregate = max if name == "observed_peak_rss_bytes" else statistics.median
        old, new = aggregate(left), aggregate(right)
        regression = (new / old - 1) * 100
        metrics[name] = {"before": old, "after": new, "change_percent": regression,
                         "before_range": [min(left), max(left)], "after_range": [min(right), max(right)],
                         "status": "passed" if regression <= limit_percent else "failed"}
    return {"status": "passed" if all(row["status"] == "passed" for row in metrics.values()) else "failed",
            "samples_per_version": len(baseline), "limit_percent": limit_percent,
            "workload": signature, "metrics": metrics,
            "scope": "local equal-work resource gate; not a statistical or terabyte-scale guarantee"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, action="append", required=True)
    parser.add_argument("--after", type=Path, action="append", required=True)
    parser.add_argument("--limit-percent", type=float, default=5)
    args = parser.parse_args()
    try:
        result = compare([json.loads(path.read_text()) for path in args.before],
                         [json.loads(path.read_text()) for path in args.after], args.limit_percent)
    except (KeyError, ValueError) as error:
        result = {"status": "failed", "error": str(error)}
    result["inputs"] = {"before": [str(path) for path in args.before],
                        "after": [str(path) for path in args.after]}
    print(json.dumps(result, indent=2))
    return int(result["status"] != "passed")


if __name__ == "__main__":
    raise SystemExit(main())
