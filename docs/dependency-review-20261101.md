# Dependency review — 2026-11-01

Update implementation and measured results are recorded in the
[2026-11-02 qualification](performance/dependency-update-20261102/README.md).
The review below describes the pre-update baseline.

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

## Parent dependency and release checks

The follow-up inspects five workspace graphs (root, fuzz, public-API fixture,
rename fixture and isolated Loom models), and current version/dependency
metadata for 98 distinct packages, including the vendored redb release.
The latest published parent manifests were checked, not only the current
lockfile's constraints. All five blocked substitutions below were also attempted
with `cargo update -p PACKAGE --precise VERSION --dry-run`: Cargo rejected each
on the named requirement, without changing any lockfile.

| Candidate | Actual dependency chain and published requirement | Latest published parent support / development status |
| --- | --- | --- |
| quick-error 1.2.3 → 2.0.1 | varlit tests → proptest 1.12.0 → rusty-fork 0.3.1 → quick-error `^1.2` | proptest 1.12.0 and rusty-fork 0.3.1 are already latest stable. rusty-fork master still requests 1.2. No released parent upgrade enables 2.x. |
| r-efi 6.0.0 → 7.1.0 | tempfile 3.27.0 → getrandom 0.4.3 → r-efi `^6` | getrandom 0.4.3 is latest published stable. Its development branch already requests r-efi 7; that change is not in the registry release. This edge is specific to UEFI with the efi_rng backend. |
| windows-link 0.2.1 → 0.100.0 | varlit-core (Windows) → windows-sys 0.61.2 → windows-link `^0.2.1` | windows-sys 0.61.2 is latest published stable. Upstream master has windows-sys 0.100.0 and windows-link 0.100.0, but that windows-sys version is not currently available as a stable registry release. |
| syn 2.0.119 → 3.0.9 | zerocopy 0.8.63 → zerocopy-derive `=0.8.63` → syn `^2.0.46` | Latest published zerocopy/derive still require syn 2. Other macros in the same workspace already use syn 3; upgrading those does not remove this requirement. |
| windows-result 0.4.1 → 0.100.0 (new Loom workspace) | loom 0.7.2 → generator 0.8.11 → windows-result `>=0.3.1, <=0.4` | Loom and generator are already latest stable. windows-result 0.100.0 supports windows-link 0.100.0, but generator still excludes both: its own windows-link requirement is `>=0.1, <=0.2`. Both edges are Windows-specific. |

For these cases, updating already-current published parents cannot select the
new child version. A local fork or unreleased Git dependency would be a separate
source change requiring qualification, not an ordinary lockfile refresh. The
getrandom development change shows that an upstream transition exists; it must
not be described as absent simply because it has not shipped.

The 13 compatible registry candidates above need no parent requirement changes:

| Candidate group | Existing parent requirements that permit the update |
| --- | --- |
| rustix | tempfile: `^1.1.4` |
| cc / find-msvc-tools | zstd-sys: cc `^1.0.45`; libfuzzer-sys: cc `^1.0.83`; current cc: find-msvc-tools `^0.1.14` |
| equivalent | indexmap: `^1.0` |
| serde_json | varlit tests: `^1.0.151`; trybuild: `^1.0.110` |
| unicode-ident | proc-macro2 and syn: `^1.0` / `^1` |
| toml / toml_edit | trybuild: toml `^1`; proc-macro-crate: toml_edit `^0.25.0` |
| serde_spanned / toml_datetime / toml_parser / toml_writer | Existing toml/toml_edit dependencies already allow the proposed 1.x patch releases |
| arbitrary | libfuzzer-sys: `^1` |

Fresh Cargo dry runs select 12 updates in root, nine in fuzz, eight in the
public-API fixture, six in the rename fixture and zero in the Loom workspace.
These overlap: the union remains 13 registry packages. Vendored redb 4.4.0 is
an additional source-update candidate. Resolution succeeds within Rust 1.95;
the candidate versions have not yet been built/tested together.

Published requirements were read from the versioned crates.io dependency API
(`https://crates.io/api/v1/crates/NAME/VERSION/dependencies`), and compared to
`cargo metadata --locked --all-features` for each workspace. Development evidence:
[getrandom at 6a049007](https://github.com/rust-random/getrandom/blob/6a04900732207b537b2d3ea7bee61ab0a6c6ca9f/Cargo.toml),
[Windows workspace at be2831da](https://github.com/microsoft/windows-rs/blob/be2831da5cf6c3f7e6f88bf8b7222e46b5efd4cc/Cargo.toml),
and [rusty-fork master](https://github.com/altsysrq/rusty-fork/blob/master/Cargo.toml).
The inspected constraints and workspace membership are retained in
[parent-resolution evidence](dependency-parent-resolution-20261101.json).

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
