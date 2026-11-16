use std::path::Path;

use varlit::{
    BatchAppendError, BatchAppendInfo, BatchOptions, Result as VarlitResult, ScanOptions,
    StreamBootstrapReport, StreamOptions, WriterLockBreakPolicy, varlit_format,
};

varlit_format! {
    pub format PetabytePlanBatchFormat {
        magic: b"PTPB";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            variable Frame(id = 8, key = [scan, frame]) {
                scan: u32,
                frame: u32,
                payload: Vec<u8>,
            }

            fixed Sample(id = 4) {
                value: u64,
            }

            variable Observation(id = 2, key = [id]) {
                id: u64,
                payload: Vec<u8>,
            }

            fixed Account(id = 6, key = [id]) {
                id: u64,
                active: bool,
            }
        }
    }
}

fn stream_borrowed_batch(
    writer: &mut PetabytePlanBatchFormatStreamWriter,
    samples: &[Sample],
) -> Result<BatchAppendInfo, BatchAppendError> {
    writer.push_samples(samples, BatchOptions::default())
}

fn bootstrap_stream(path: &Path) -> VarlitResult<StreamBootstrapReport> {
    PetabytePlanBatchFormat::bootstrap_stream_checkpoint(path, StreamOptions::default())
}

fn bootstrap_stream_with_progress(path: &Path) -> VarlitResult<StreamBootstrapReport> {
    PetabytePlanBatchFormat::bootstrap_stream_checkpoint_with_progress(
        path,
        StreamOptions::default(),
        ScanOptions::default(),
        |_| {},
    )
}

fn restore_stream(
    path: &Path,
) -> VarlitResult<PetabytePlanBatchFormatStreamWriter> {
    PetabytePlanBatchFormat::restore_stream_writer(path, StreamOptions::default())
}

fn clear_stale_lock(path: &Path) -> VarlitResult<()> {
    PetabytePlanBatchFormat::clear_stale_writer_lock(
        path,
        WriterLockBreakPolicy::BreakIfProcessAbsent,
    )
}

fn main() {
    let _ = (
        stream_borrowed_batch,
        bootstrap_stream,
        bootstrap_stream_with_progress,
        restore_stream,
        clear_stale_lock,
    );
}
