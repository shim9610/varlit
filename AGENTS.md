# Working rules for agents in this repository

## Heap allocation

- A new `Vec` in library code (`crates/varlit-core/src`, `crates/varlit/src`)
  needs the maintainer's approval before it is written. Ask first, saying
  where it is, why a caller-provided buffer or a buffer allocated once at open
  cannot do, and what bounds its size. The same applies to other growing heap
  containers (`String`, `Box<[T]>`, maps, sets) used in its place. Tests,
  benches, fuzz targets and tools are exempt.
- Data paths (staging and writing, publication, follow, checkpoint, reads)
  work from the caller's buffers and from fixed buffers allocated once when the
  handle is opened, sized by the file and declared limits. After open they
  allocate nothing: they cannot fail for lack of memory unless the host takes
  memory away, and they flush to the file without allocating.
- Work larger than those buffers streams through them. It is never refused for
  its size, never limited by a fixed structure such as the ring, and never
  grows memory with it.

## Testing

- While developing, run only the checks related to the code you changed
  (its unit and integration tests, clippy, the proof checks that cover it).
- Run the full test suite only with the maintainer's approval; ask with the
  reason it is needed.
