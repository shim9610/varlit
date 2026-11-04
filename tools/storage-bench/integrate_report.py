#!/usr/bin/env python3
"""Build the existing comparison dashboard from separate, non-pooled cohorts."""
import argparse
import copy
import hashlib
import json
from pathlib import Path


def build_current(original, grouped):
    data = copy.deepcopy(original)
    for name, row in data["profiles"].items():
        row["presentation"] = "historical" if name.startswith("objects-") else "adverse" if name.startswith("table-") else "primary"
        row["origin_report"] = "initial cohort (original metrics retained)"
    for name, source in grouped["profiles"].items():
        assert name not in data["profiles"]
        row = copy.deepcopy(source)
        row["metrics"]["allocated_mib"] = row["metrics"]["final_mib"]
        row["observations"] = len(row["observation_files"])
        row["presentation"] = "historical" if name.startswith("individual-") else "primary"
        row["origin_report"] = "corrected grouped cohort"
        data["profiles"][name] = row
    for name, row in grouped["profiles"].items():
        if row["profile"]["engine"] not in ["varlit-stream", "varlit-indexed"]:
            continue
        if not name.startswith("grouped-"):
            continue
        target = name.removesuffix(row["profile"]["engine"]) + "sqlite"
        sql = grouped["profiles"].get(target)
        if sql is None:
            continue
        for metric in ["write", "read_active", "completion"]:
            ratio = sql["metrics"][metric] / row["metrics"][metric]
            data["comparisons"].append(dict(profile=name, sqlite=target, metric=metric,
                                            ratio=ratio, observed_twofold=ratio >= 2,
                                            repetitions=3))
    data["cohorts"] = [
        dict(name="Initial mixed/array/table cohort", configurations=122, timing_observations=360,
             resource_probes=120, refused_attempts=8),
        dict(name="Corrected grouped/control cohort", configurations=len(grouped["profiles"]),
             timing_observations=3 * len(grouped["profiles"]), resource_probes=len(grouped["profiles"]),
             refused_attempts=0),
    ]
    active = [r for r in data["comparisons"]
              if data["profiles"][r["profile"]]["presentation"] != "historical"]
    data["timing_observations"] = sum(c["timing_observations"] for c in data["cohorts"])
    data["resource_probes"] = sum(c["resource_probes"] for c in data["cohorts"])
    data["compared_operations"] = len(active)
    data["observed_twofold"] = sum(r["ratio"] >= 2 for r in active)
    data["ratio_count_scope"] = "Nonhistorical operation observations only; not a qualification."
    data["presentation_policy"] = "Individual-record stress/control rows are historical and excluded from the current view. Source medians remain separate; no pooled timing or universal speed qualification."
    return data


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--original", type=Path, required=True)
    ap.add_argument("--grouped", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    current = build_current(json.loads(args.original.read_text()), json.loads(args.grouped.read_text()))
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "current-comparison.json").write_text(json.dumps(current, indent=2) + "\n")
    template = Path(__file__).with_name("report.html").read_text()
    template = template.replace('href="comparison.json"', 'href="current-comparison.json"')
    (args.output / "index.html").write_text(template.replace("__DATA__", json.dumps(current).replace("</", "<\\/")))
    (args.output / "CURRENT_SHA256SUMS").write_text("".join(
        f"{hashlib.sha256((args.output/name).read_bytes()).hexdigest()}  {name}\n"
        for name in ["README.md", "current-comparison.json", "index.html"]))
    print("Integrated", len(current["profiles"]), "measured configurations; historical controls excluded by default")


if __name__ == "__main__":
    main()
