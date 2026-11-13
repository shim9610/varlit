#!/usr/bin/env python3
"""Require production delegation and prove that known broken protocols are rejected.

Mutations run in a dependency-free scratch crate containing exact copies of the
production driver and its proofs. Compilation errors/timeouts are NOT detections.
The working source tree is never mutated. Full core proofs run separately in CI.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
DRIVER = ROOT / "crates/varlit-core/src/publication_protocol.rs"
PROOFS = DRIVER.with_suffix("") / "proofs.rs"
COMMIT_PROOF = "commit_survives_every_io_failure_effect"
MUTATIONS = [
    ("volatile-head-resume", "    if first.is_none() || first != second {", "    if false {",
     "resumed_writer_cannot_acknowledge_a_volatile_head", "resumed writer acknowledged volatile head"),
    ("newest-head-overwritten-first", "    usize::from(first > second)", "    usize::from(first < second)",
     "commit_survives_every_io_failure_effect", "newest prior anchor was destroyed"),
    ("readiness-check-omitted", "    io(CommitStep::Prepare)?;", "",
     "poisoned_writer_never_reaches_publication", "poisoned writer accepted a commit"),
    ("primary-sync-omitted", "    io(CommitStep::PrimarySync)?;", "", COMMIT_PROOF,
     "generation adopted before primary durability"),
    ("head-sync-omitted", "        io(CheckpointStep::HeadSync)?;", "", COMMIT_PROOF,
     "acknowledged an undurable commit"),
    ("body-sync-omitted", "        io(CheckpointStep::BodySync)?;", "",
     "every_two_barrier_publication_syncs_its_body_before_its_head",
     "assertion failed: body && (body_synced || !confirmed)"),
    ("poison-omitted", "        let _ = io(CommitStep::Poison);", "", COMMIT_PROOF,
     "uncertain publication left writer usable"),
    ("publication-error-swallowed", "        return Err(error);", "        return Ok(());", COMMIT_PROOF,
     "assertion failed: result.is_ok() == medium.finished"),
    ("recovery-published-before-sync", "    sync()?;\n    publish()",
     "    let value = publish()?;\n    sync()?;\n    Ok(value)",
     "recovery_never_publishes_before_truncate_and_sync", "recovery published before native sync"),
    ("follow-error-panics", "    let next = prepare(current)?;",
     '    let next = match prepare(current) { Ok(next) => next, Err(_) => panic!("failed follow panicked") };',
     "follow_error_preserves_the_entire_reader", "failed follow panicked"),
    ("success-without-publication", "    io(CommitStep::PrimarySync)?;", "    return Ok(());",
     "successful_commit_is_reachable_and_durable", "assertion failed: medium.finished"),
]


def check_bindings():
    # This wiring guard prevents accidentally testing an orphaned copy. It is
    # not a Rust semantic/refinement proof of callbacks or arbitrary future APIs.
    bindings = [
        ("indexed.rs", "fn sync_with(&mut self, force_confirm: bool)", "commit"),
        ("indexed.rs", "fn follow_records(&mut self)", "follow"),
        ("stream.rs", "pub(crate) fn restore_checkpointed(", "restore"),
        ("disk_index/storage.rs", "pub fn publish(", "checkpoint"),
        ("disk_index/storage.rs", "pub(super) fn reconcile_confirmed_heads(", "resume"),
    ]
    for path, signature, driver in bindings:
        source = (ROOT / "crates/varlit-core/src" / path).read_text()
        start = source.index(signature)
        next_method = re.search(r"\n    (?:pub[^\n]* )?fn ", source[start + len(signature):])
        end = start + len(signature) + next_method.start() if next_method else len(source)
        body = source[start:end]
        if body.count(f"crate::publication_protocol::{driver}(") != 1:
            raise ValueError(f"production binding missing/ambiguous: {path}: {signature}")
        if driver == "checkpoint" and body.count("crate::publication_protocol::older_head(") != 1:
            raise ValueError("production head selection bypasses the verified older-head helper")
    # Public entry points must reach the private drivers bound above.
    source = (ROOT / "crates/varlit-core/src/indexed.rs").read_text()
    for public, helper in [("pub fn sync(&mut self)", "sync_with"), ("pub fn follow(&mut self)", "follow_records")]:
        start = source.index(public)
        end = source.index(f"fn {helper}(", start)
        if source[start:end].count(f"self.{helper}(") != 1:
            raise ValueError(f"public indexed entry point bypasses its verified driver: {public}")
    source = (ROOT / "crates/varlit-core/src/disk_index.rs").read_text()
    start = source.index("pub(crate) fn validate_clean_writer_for_spec(")
    end = source.index("pub(crate) fn compact(", start)
    if source[start:end].count("self.storage.reconcile_confirmed_heads()?") != 1:
        raise ValueError("clean writer resume bypasses durability reconciliation")
    return [dict(path=p, method=m, driver=d) for p, m, d in bindings]


def detected(log, code, expected):
    # Require a failed assertion in a completed verification, not a matching
    # string in compiler diagnostics or an unrelated unwinding failure.
    if code != 1 or "VERIFICATION:- FAILED" not in log or "Manual Harness Summary:" not in log:
        return False
    checks = re.split(r"Check \d+: ", log)
    if any("Status: FAILURE" in check and (
        "unwinding assertion" in check.lower() or "unsupported" in check.lower()
    ) for check in checks):
        return False
    return any("Status: FAILURE" in check and expected in check for check in checks)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/protocol-mutations")
    parser.add_argument("--bindings-only", action="store_true")
    args = parser.parse_args()
    bindings = check_bindings()
    if args.bindings_only:
        print(f"PROTOCOL_BINDINGS {len(bindings)} production delegates")
        return
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    original = DRIVER.read_text()
    report = {"driver_sha256": hashlib.sha256(DRIVER.read_bytes()).hexdigest(),
              "proofs_sha256": hashlib.sha256(PROOFS.read_bytes()).hexdigest(),
              "bindings": bindings, "mutations": []}
    env = {**os.environ, "CARGO_TARGET_DIR": str(ROOT / "target/protocol-mutation-build")}
    with tempfile.TemporaryDirectory(prefix="varlit-protocol-") as directory:
        crate = Path(directory)
        (crate / "src/publication_protocol").mkdir(parents=True)
        (crate / "Cargo.toml").write_text('[package]\nname = "protocol-negative-controls"\nversion = "0.0.0"\nedition = "2024"\n[workspace]\n[lints.rust]\nunexpected_cfgs = { level = "warn", check-cfg = ["cfg(kani)"] }\n')
        (crate / "src/lib.rs").write_text("mod publication_protocol;\n")
        driver = crate / "src/publication_protocol.rs"
        driver.write_text(original)
        (crate / "src/publication_protocol/proofs.rs").write_bytes(PROOFS.read_bytes())
        command = ["cargo", "kani", "--manifest-path", str(crate / "Cargo.toml"), "--default-unwind", "5"]
        with (out / "baseline.log").open("w") as log:
            code = subprocess.run(command, cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT, timeout=900).returncode
        baseline = (out / "baseline.log").read_text()
        summary = re.search(r"Complete - (\d+) successfully verified harnesses, 0 failures, (\d+) total\.", baseline)
        if code or not summary or int(summary[1]) < 9 or summary[1] != summary[2]:
            raise RuntimeError("unmodified protocol proofs failed; see baseline.log")
        for name, before, after, harness, expected in MUTATIONS:
            if before not in original:
                raise ValueError(f"stale mutation: {name}")
            driver.write_text(original.replace(before, after, 1))
            path = out / f"{name}.log"
            with path.open("w") as log:
                code = subprocess.run(command + ["--harness", harness], cwd=ROOT, env=env,
                                      stdout=log, stderr=subprocess.STDOUT, timeout=300).returncode
            killed = detected(path.read_text(), code, expected)
            report["mutations"].append({"name": name, "harness": harness, "expected_assertion": expected,
                                        "detected": killed, "exit_code": code, "log": path.name})
            (out / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
            print(f"PROTOCOL_MUTATION {name}: {'detected' if killed else 'NOT DETECTED'}", flush=True)
            if not killed:
                raise RuntimeError(f"negative control survived or did not verify: {name}; see {path}")
    print(f"PROTOCOL_MUTATIONS {len(MUTATIONS)}/{len(MUTATIONS)} detected", flush=True)


if __name__ == "__main__":
    main()
