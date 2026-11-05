# Mixed-block append and concurrent read comparison plan

Status: **the [canonical combined report](workload-storage-20261104/README.md) integrates the corrected grouped-item model with the existing mixed/array/table cases. The grouped cohort has 47 configurations, 141 successful timings and 47 qualified resource probes; the original 122-configuration cohort remains separately identified. Individual-record stress cases are historical controls, not representative grouped logging.**

## Operation comparisons and logical model

Evaluate writes, following new data, and key lookups separately using the same successful work. Compare SQLite and HDF5 using the same data and supported query semantics.

The API reference schema comes from [CardinalityFormat in the high-cardinality tests](../../crates/varlit/tests/high_cardinality.rs). It declares fixed Metadata(run) and variable Frame(scan, frame, payload) blocks together, with Frame's composite key stored in a disk index. The [declarative key tests](../../crates/varlit/tests/schema_key_kinds.rs) also distinguish finite and general key domains. That test schema exercises API/index behavior; it does not dictate one Frame per logical item. The `high-cardinality-dev` feature was removed in 0.10.0; high-cardinality storage is provided by indexed APIs for blocks declaring `key_index = disk`.

- One logical Frame is an application-defined data block. Do not split individual numeric samples inside it into separate frames.
- Append fixed metadata and variable data blocks into the same file. Distinguish versioned metadata, small independent objects, and large sample chunks.
- Record block counts, payload bytes per block, samples per payload, key cardinality, and commit counts separately.
- Choose physical blocks and a separately declared publication size for logical items. Batched per-item Frames are not equivalent to packing items into one block; individual-key stress is a separate control, not a representative grouped-capture model.

## Primary workloads

| Workload | Data and state | Queries and validation |
|---|---|---|
| Mixed sequential logging | Append fixed Metadata and variable 64 KiB / 1 MiB Frames together | Verify values, order within each type, and every payload byte |
| Mixed sizes and types | Interleave small independent metadata objects with Frames of different sizes | Type filtering, selected ranges, and newly committed suffixes |
| Concurrent sharing | One writer, one or eight readers, and a retained older snapshot | Follow new data, query keys, and verify that the older snapshot remains unchanged |
| Grouped logical items | 1,048,576 / 8,388,608 items in 512/8,192-item blocks; 16 MiB publications plus a 1 MiB contrast | Full follow, separately fetched group-local item queries, memory/storage and concurrent progress; unrelated general keys remain historical stress |
| Update history | Append new versions of existing keys as a separate workload | Latest values at the publication boundary and values in older snapshots; assess separately from pure appends |
| Long runs and idle polling | Add a small suffix to a long existing prefix, or add nothing | Repeated idle-follow cost and cost relative to newly added data |

Use `push_iter` and batch APIs for ordinary logging. Report workloads that genuinely require scalar pushes separately. Separate one-time open, bootstrap, and index construction from repeated queries. Readers retain handles and cursors instead of reopening or scanning the whole file for each operation.

The writer must not wait for reader completion to force its rate down to that of a slower reader. Every reader must consume the same record and byte counts. Record writer completion and complete reader consumption separately. Uncommitted data must not count as a query result; define a common logical publication boundary. Do not assert identical recovery or durability guarantees for engines with different contracts.

## Workload-appropriate storage by engine

### Varlit

Use declared fixed/variable blocks, generated stream/indexed handles, batch appends, persistent typed cursors, and explicit follow. Distinguish `key_index = disk`, finite keys, general high-cardinality keys, and resident/nonresident policies where relevant. Measure memory while checking the existing contract that increasing general key cardinality does not grow a Varlit HashMap.

Matrix and numeric-array measurements are auxiliary workloads for those data types. They do not replace mixed-block append tests. Preserve mmap immutability requirements and cyclic redundancy check (CRC) verification, and account for required compaction and initialization.

### SQLite

Define tables and types appropriate to the logical block kinds and fields. Permit suitable BLOB batching for Frames containing sample arrays, and place metadata fields in columns appropriate to the actual queries. Do not force each sample in a large array into a separate row.

Provide a sequence primary key for ordered logging and the composite indexes required by the queries. For example, use `(scan, frame, sequence DESC)` for the latest Frame version and sequence ranges for follow. Compare type-specific tables with a global log table where they satisfy the required ordering and query results. Do not add unnecessary indexes that penalize writes.

Record prepared statements, transactions matching the logical publication unit, write-ahead logging (WAL), FULL synchronization, and actual page, cache, and mmap settings. Permit partial BLOB APIs for partial reads. Freeze a **complete schema and configuration supporting all required queries**, then measure writes, reads, and storage together. Do not combine unindexed write performance with indexed query performance while omitting the index cost.

### HDF5

Use suitable compound or array datasets for each block kind. Use chunked arrays for fixed-size payloads and appropriate payload regions with offset/length information for variable-size payloads. Avoid pathological layouts that create a dataset or attribute for every logical item. Select chunk sizes, chunk caches, and bulk/partial hyperslab I/O appropriate to the data.

Use single-writer/multiple-reader (SWMR) mode for concurrent append/read. Specify commit information identifying the published logical prefix across datasets and the flush/refresh ordering. Verify data and metadata at the same publication boundary. File growth alone must not authorize reading an incomplete block.

HDF5 itself does not provide SQLite-style general composite-key SQL indexes. Use dataset axes for keys naturally represented by those axes. If arbitrary-key lookup needs a separate index, specify its implementation and include construction, update, open, cache, and storage costs. Without that index, do not force full scans as the only HDF5 key-lookup implementation or claim equivalent functionality. Distinguish common append/follow operations from general key lookups.

## Measurement and evidence

- Retain the existing 4 CPU / 16 GiB / 32 GiB budgets. Do not remove limits or validation to meet a speed target.
- Validate identical logical data, order, queries, publication boundaries, and final results. Different data types or physical layouts must not reduce the required work.
- Separate preliminary schema tuning from primary measurements, and freeze complete comparison configurations before the primary runs.
- Record writer and reader time/CPU, overall completion, accumulated lag, actual processed bytes, resident set size (RSS), proportional set size (PSS), storage, WAL, and deleted open files. Distinguish duplicate RSS accounting for mappings from physical memory use.
- State whether each metric includes initialization, the validation oracle, input generation, checkpointing, and compaction. Account for preparation and copying that the application actually performs.
- Preserve failures, limit refusals, interrupted attempts, original sources, settings, and binary hashes. Auxiliary results do not qualify the primary workloads.

## Runnable scope

The standalone harness includes mixed append/follow, grouped logical items and separately labeled individual-record stress controls, HDF5-favorable typed arrays and SQLite-favorable multi-producer typed tables. It includes bounded MPSC serialization for single-writer engines and independent SWMR reader processes. Its README distinguishes implemented insert/prefix checks from pending update-history, SQL-functionality and tuning studies. The measured report separates the fixed primary grid, optimized layout supplements, tuning and controls. Update history and broader SQL semantics remain outside the implemented contract. Smoke invocations are not performance samples.
