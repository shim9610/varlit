set -e
source /workspace/tooling/varlit/env.sh
out=/workspace/scratch/ownership-contracts
cmp /workspace/varlit/crates/varlit-core/examples/ownership_isolated_load.rs /workspace/varlit-ownership-contracts-before/crates/varlit-core/examples/ownership_isolated_load.rs
for side in before after; do
 repo=/workspace/varlit
 if [ "$side" = before ]; then repo=/workspace/varlit-ownership-contracts-before; fi
 export CARGO_TARGET_DIR=/workspace/scratch/fallible-ownership/target-$side
 cargo build --manifest-path "$repo/Cargo.toml" --locked --release --all-features -p varlit-core --example ownership_isolated_load > "$out/matched-isolated-$side-build.log" 2>&1
 mkdir -p "$out/matched-isolated-$side-bin"
 cp "$CARGO_TARGET_DIR/release/examples/ownership_isolated_load" "$out/matched-isolated-$side-bin/ownership_boundary_load"
done
python3 "$out/owner_bench.py" matched-isolated-after matched-isolated-before > "$out/matched-isolated-owner.log" 2>&1
