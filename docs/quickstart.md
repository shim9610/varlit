# Varlit quickstart

This example stores a stream of numeric samples in application blocks. It uses
one writer, a buffered append batch and a reader opened while the writer is
still alive. Install Varlit 0.13.0 with the `integrity` feature as shown in the
[README](../README.md).

## Write blocks, then read published data

```rust
use varlit::{BatchOptions, StreamOptions, varlit_format};

varlit_format! {
    pub format Capture {
        magic: b"CAPTURE";
        version: 1;
        endian: little;
        integrity: crc32_with_header;
        blocks {
            variable Frame(id = 1) {
                first_sample: u64,
                samples: Vec<u32>,
            }
        }
    }
}

fn main() -> varlit::Result<()> {
    // create_stream_writer creates a new capture and truncates an existing path.
    let path = "capture.varlit";
    let mut writer = Capture::create_stream_writer(path, StreamOptions::default())?;

    // Each physical Frame holds 4,096 samples (about 16 KiB).
    // The append batch contains 16 Frames, not 65,536 tiny records.
    let frames = (0..16u64).map(|block| Frame {
        first_sample: block * 4096,
        samples: (0..4096u32).collect(),
    });
    writer.push_frames(frames, BatchOptions::default())
        .map_err(|failure| failure.source)?;
    writer.sync()?;

    let reader = Capture::open_stream_reader(path, StreamOptions::default())?;
    let mut samples_read = 0;
    for (block, frame) in reader.frames()?.enumerate() {
        let frame = frame?;
        assert_eq!(frame.first_sample, block as u64 * 4096);
        assert_eq!(frame.samples.len(), 4096);
        assert!(frame.samples.iter().enumerate().all(|(i, &v)| v == i as u32));
        samples_read += frame.samples.len();
    }
    assert_eq!(samples_read, 65_536);
    Ok(())
}
```

Choose block size from acquisition and query needs. `push_frames` coalesces
writes but encodes one record per input `Frame`. `sync()` is the publication
boundary in this stream example. Keep publication frequency appropriate for
reader latency and durability; large batches delay visibility and retain more
staging memory. See [batch sizing](scalable-io.md#choose-the-physical-block-before-choosing-the-batch-size).

## Add readers and producers

Each reader captures a confirmed snapshot. Move a reader to its worker thread
before creating typed cursors, and use `follow` or `follow_blocks` to adopt new
publications. Matrix readers likewise hold one confirmed generation. A reader
that has not followed keeps its existing view. Readers own their buffers and
caches independently.

Feed multiple producers into one writer through a bounded queue. The library
keeps the single-writer contract; it does not start a writer service or impose
an application queue. A producer's sample count is separate from the number of
physical blocks and from the number of publications.

## Choose the next guide

- Need block-key lookup: use [indexed handles](scalable-io.md), with a group key
  and slot/range inside the returned block. Declared finite key domains avoid a
  general key tree where the domain is bounded.
- Need direct cell addressing: use [matrix declarations](format-author-guide.md)
  and review their generation and companion-file requirements.
- Need a small, directly indexed native file: see the [API reference](api-reference.md).
- Need checks or limits: start with the [read-policy table](api-reference.md#read-validation).
- Need failure handling: read the [durability model](durability-model.md) before
  retrying writes, and use [self-check diagnostics](self-check-guide.md).

Explicit verification, recovery, format migration and compaction remain caller
operations. Read the [known limitations](known-limitations.md) before deploying.
