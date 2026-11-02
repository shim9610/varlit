set -e
source /workspace/tooling/varlit/env.sh
stage=$1
root=/workspace/varlit
if [ "$stage" = before ]; then root=/workspace/varlit-ownership-baseline; fi
export CARGO_TARGET_DIR=/workspace/scratch/fallible-ownership/target-after
if [ "$stage" = before ]; then export CARGO_TARGET_DIR=/workspace/scratch/fallible-ownership/target-before; fi
mkdir -p /workspace/scratch/fallible-ownership/$stage-bin
cargo build --manifest-path "$root/Cargo.toml" --locked --release -p varlit-core --all-features --example ownership_load
cargo build --manifest-path "$root/Cargo.toml" --locked --release -p varlit --all-features --example record_mutation_load --example oom_maintenance_load --example scalable_load --example matrix_ownership_load
for name in ownership_load record_mutation_load oom_maintenance_load scalable_load matrix_ownership_load; do
 cp "$CARGO_TARGET_DIR/release/examples/$name" "/workspace/scratch/fallible-ownership/$stage-bin/$name"
done
