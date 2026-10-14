# Matrix storage diagnostics

`VarlitFile`, `VarlitReader` and `VarlitWriter` expose
`matrix_storage_diagnostics() -> Option<MatrixStorageDiagnostics>`.
It returns `None` for non-matrix files. The observations are captured once
at create/open; querying them takes `&self` and performs no I/O.

| Field | Meaning |
| --- | --- |
| `sparse_file` | `Some(true)` observes holes or a Windows sparse attribute; `Some(false)` observes a disabled attribute or known unsupported filesystem; `None` is unknown |
| `allocation_map_available` | This file answered a bounded allocation-map query |
| `hole_punch_api_available` | The build includes a native range-removal API; the mounted filesystem can still reject requests |
| `sparse_mark_failed` | Windows refused the create-time sparse request; reopening cannot reconstruct that historical result |

`diagnose_file(spec, path)` adds environment warnings for a known dense file,
an unavailable allocation map or native hole-punch API. Creation-time request
failures are available directly on the creating handle. These are performance
qualifications, not corruption findings; they do not refuse open or block writers.
Linux and macOS use `SEEK_HOLE`/`SEEK_DATA`. Windows uses allocated-range queries.
macOS hole punching uses `F_PUNCHHOLE`, Linux uses `fallocate`, and Windows uses
`FSCTL_SET_ZERO_DATA`. Unsupported mounts retain the existing safe fallback.

Current matrix opens use persisted live-page indexes and do not scan the whole
declared bitmap when allocation-map inspection is unavailable. Large fixed
regions can nevertheless consume their full logical size on dense filesystems,
and a range clear that falls back to zero writes can cost proportional to the
range. Growing dimensions avoid predeclaring a large physical extent.
`MatrixRecoveryReport::matrix_last_zero_range_streamed_bytes()` and the cumulative
zero-range counters measure actual fallback writes on the calling thread;
API availability alone does not promise a cheap clear.

These observations qualify ordinary native matrix handles. Experimental page
reuse has separate handles and retains its existing experimental contract.
