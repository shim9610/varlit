# Dependency review — 2026-11-01

Baseline: `e5e81a10201ef95834360ac2c2407f5701b12fbd`. Inspected the root, fuzz,
public-API fixture and rename fixture lockfiles, current crates.io metadata,
upstream changes and Cargo's dry-run resolution. Dependency versions were not
upgraded as part of the verification-tool integration. The new isolated Loom
workspace has its own dependency resolution.

## Update priority

| Dependency | Locked → candidate | Assessment |
| --- | --- | --- |
| rustix | 1.1.5 → 1.1.6 | Prioritize: upstream memory-safety and platform correctness fixes; review reachability below. |
| cc | 1.5.1 → 1.10.0 | Build-tool update worth testing separately: propagates AddressSanitizer to C compilation, fixes flag ordering/environment caching and adds parallel process polling. Affects zstd's native build and fuzz tooling. |
| find-msvc-tools | 0.1.14 → 0.1.15 | Regenerated Windows bindings; qualify together with cc on Windows. |
| serde_json | 1.0.151 → 1.0.154 | Test/tool dependency: UTF-8 error position, negative-zero/arbitrary-precision and float comparison fixes; no_std preserve_order support. |
| unicode-ident | 1.0.26 → 1.0.27 | Proc-macro dependency: identifier-trie layout optimization. |
| equivalent | 1.0.2 → 1.0.3 | Compatible patch candidate; source semantics not audited in this review. |
| TOML family | listed below | Compatible tooling/proc-macro candidates; no release-note-based safety conclusion. |
| arbitrary (fuzz workspace) | 1.4.2 → 1.5.0 | Compatible fuzz input-generator update; rerun the corpus and fuzz targets before adopting. |
| vendored redb (test oracle only) | 4.3.0 → 4.4.0 | Prioritize for the test oracle: upstream fixes unbounded page-cache growth, a compact/write-transaction wait and durability/recovery issues. This path dependency requires reviewing/replacing vendored source; Cargo's dry run does not update it. |

TOML candidates: `serde_spanned 1.1.1→1.1.2`, `toml 1.1.6→1.1.8`,
`toml_datetime 1.1.1→1.1.2`, `toml_edit 0.25.15→0.25.17`,
`toml_parser 1.1.3→1.1.5`, `toml_writer 1.1.2→1.1.3`.
The TOML version strings also carry `+spec-1.1.0` in the lockfiles.

Cargo's root dry run offers 12 compatible package updates; the fuzz workspace
offers nine, including `arbitrary`. None requires raising the project's Rust
1.95 minimum according to the current resolution and registry metadata.
These are resolution results, not test qualification of the proposed updates.

`quick-error 1→2`, `r-efi 6→7` and `windows-link 0.2→0.100` appear newer when
comparing registry maxima but cross dependency compatibility boundaries. Do not
force them into this graph. The simultaneously locked `syn 2` and `syn 3` serve
different dependencies; the presence of syn 2 is not itself a missed update.

## rustix: important upstream fixes, distinct from current reachability

[The 1.1.5…1.1.6 comparison](https://github.com/bytecodealliance/rustix/compare/v1.1.5...v1.1.6)
contains these relevant changes:

- Extended-attribute calls passed an OS size-query result to `assume_init` for
  zero-length buffers. The fix handles empty buffers separately and adds explicit
  size-query functions. The upstream commit names `GHSA-jh2m-fhpj-3vr7`.
- Truncated macOS socket ancillary messages could underflow buffer accounting,
  panic during destruction or read file descriptors from uninitialized storage.
  The fix clamps message lengths and rounds descriptor data to whole entries.
- Other fixes include 32-bit UID/GID syscall selection, Apple allocation behavior,
  inotify/statvfs constants and representation of uninitialized padding.

Varlit reaches rustix through `tempfile 3.27.0`, enabling `default`, `std`, `alloc`
and `fs`. Inspected tempfile call sites use temporary-file open/reopen, stat,
rename and unlink operations; they do not call the affected xattr functions.
The socket `net` feature is not enabled by this path. Varlit's sparse allocation
code calls libc directly, not rustix's fallocate wrapper. This inspection did
not identify a Varlit file-input route into those upstream faults. Feature
unification in a downstream application can change the enabled rustix surface.

The source diff contains added/changed unsafe syscall and buffer-initialization
blocks; this is not an unsafe-free update. Inspection of added production lines
found no added Mutex/RwLock or Arc/Rc/Box sites in this rustix version comparison.
That textual check is not a full semantic audit. In contrast, cc's intervening
1.7.0 release explicitly adds Arc-backed tool environment storage, and later
versions change build-process synchronization; those run during compilation.

## Current versions and advisory results

The current lockfiles already contain the latest stable versions returned by
crates.io for `crc32fast 1.5.3`, `libc 0.2.191`, `memmap2 0.9.12`,
`zerocopy 0.8.63`, `zstd 0.14.3`, `zstd-safe 8.2.0`,
`zstd-sys 2.1.2+zstd.1.5.7`, `tempfile 3.27.0`, `thiserror 2.0.21`,
`proptest 1.12.0` and `trybuild 1.0.122`. Lower manifest minimums do not mean
those older versions are currently selected by the lockfiles.

`cargo-audit 0.22.2` reported zero known advisories and no informational warnings
for the root and fuzz lockfiles, and for the new Loom lockfile. Its database had
1,395 advisories at commit `2295466c4e9df26647fd67d8fa1c705ac2fc7f81`, dated
2026-10-30. The rustix upstream fix above was identified independently; zero
audit findings is not a substitute for reviewing current upstream patches.

The repository had only PR #18 open at inspection; no bot update PR was waiting.
Dependabot already checks Cargo and Actions weekly, and now includes the isolated
Loom workspace. Installed cargo-fuzz 0.13.2 and cargo-audit 0.22.2 match the current
registry releases. No crate release, version bump or merge is included.

Pinned checkout 7.0.1, upload-artifact 7.0.2, rust-cache 2.9.3,
cargo-deny-action 2.1.1 and CodeQL Action 4.39.1 match the latest release
identities inspected. The pinned rust-toolchain commit also matches current
master. CodeQL's separate bundle release tag must not be mistaken for the
Action's version. These checks do not re-audit the Actions' source.

[redb 4.4.0](https://github.com/cberner/redb/releases/tag/v4.4.0) also adds
NonZero value/key support, optional-value niche encoding and stable cursors.
Its cache-growth and transaction fixes matter to differential testing and
long fuzz runs, but redb is excluded from Varlit's runtime graph. No vendored
source update or performance qualification of redb 4.4.0 was performed here.

Sources: [cc releases](https://github.com/rust-lang/cc-rs/releases),
[serde_json releases](https://github.com/serde-rs/json/releases),
[unicode-ident releases](https://github.com/dtolnay/unicode-ident/releases),
and [the version inventory](dependency-versions-20261101.json).
