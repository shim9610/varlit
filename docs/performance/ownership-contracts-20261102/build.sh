set -e
source /workspace/tooling/varlit/env.sh
stage=$1
repo=/workspace/varlit
export CARGO_TARGET_DIR=/workspace/scratch/fallible-ownership/target-after
if [ "$stage" = before ]; then
 repo=/workspace/varlit-ownership-contracts-before
 export CARGO_TARGET_DIR=/workspace/scratch/fallible-ownership/target-before
fi
# Reproduce Box from its isolated snapshot, not from the later Arc checkout.
if [ "$stage" = box ]; then
 repo=/workspace/varlit-ownership-contracts-box
fi
# Optional explicit source checkout for reproductions on another host.
repo=${2:-$repo}
out=/workspace/scratch/ownership-contracts/$stage-bin
mkdir -p "$out"
cargo build --manifest-path "$repo/Cargo.toml" --locked --release -p varlit-core --all-features --example ownership_load --example ownership_boundary_load
cargo build --manifest-path "$repo/Cargo.toml" --locked --release -p varlit --all-features --example record_mutation_load --example oom_maintenance_load --example scalable_load --example matrix_ownership_load
for name in ownership_load ownership_boundary_load record_mutation_load oom_maintenance_load scalable_load matrix_ownership_load; do
 cp "$CARGO_TARGET_DIR/release/examples/$name" "$out/$name"
done
