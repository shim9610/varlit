#!/bin/sh
# Runs every scenario's situation sweep in sequence (each resumes from its
# state.json). Start detached so it survives the terminal or app:
#   systemd-run --user --unit=fb-sweep-all --working-directory=$PWD sh sweep_all.sh OUT_ROOT
set -e
OUT=${1:?OUT_ROOT}
for s in cells grid analytics oltp mixed; do
  python3 sweep.py "$s" "$OUT/sweep-$s" --repeats 3
done
