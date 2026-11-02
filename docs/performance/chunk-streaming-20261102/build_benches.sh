set -e
source /workspace/tooling/varlit/env.sh
export CARGO_TARGET_DIR=/workspace/varlit/target
mkdir -p /workspace/scratch/chunk-stream/before-bin /workspace/scratch/chunk-stream/after-bin
cargo build --manifest-path /workspace/varlit-stream-baseline/Cargo.toml --release -p varlit-core --all-features --example chunk_stream_load
cargo build --manifest-path /workspace/varlit-stream-baseline/Cargo.toml --release -p varlit --all-features --example record_mutation_load --example oom_maintenance_load --example scalable_load
for name in chunk_stream_load record_mutation_load oom_maintenance_load scalable_load; do
 cp "$CARGO_TARGET_DIR/release/examples/$name" "/workspace/scratch/chunk-stream/before-bin/$name"
done
cargo build --release -p varlit-core --all-features --example chunk_stream_load
cargo build --release -p varlit --all-features --example record_mutation_load --example oom_maintenance_load --example scalable_load
for name in chunk_stream_load record_mutation_load oom_maintenance_load scalable_load; do
 cp "$CARGO_TARGET_DIR/release/examples/$name" "/workspace/scratch/chunk-stream/after-bin/$name"
done
