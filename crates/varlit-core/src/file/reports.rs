// The values file operations report: block events, append and replacement
// info, recovery and defragmentation reports, writer lock information.
// Included by `file.rs`, whose module it shares.

#[derive(Clone, Debug)]
pub struct BlockEvent {
    pub block_id: u32,
    pub block_version: u16,
    pub sequence: u64,
    pub record_offset: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendInfo {
    pub sequence: u64,
    pub record_offset: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub footer_offset: Option<u64>,
    pub prev_same_block_offset: Option<u64>,
    pub prev_same_key_offset: Option<u64>,
    pub committed: bool,
}

/// What a [`VarlitFile::defragment`] actually did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefragmentReport {
    /// Records marked dead that the new generation does not contain.
    pub records_dropped: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplacementInfo {
    pub sequence: u64,
    pub record_offset: u64,
    pub old_payload_len: u64,
    pub new_payload_len: u64,
    pub old_physical_len: u64,
    pub new_physical_len: u64,
}

impl ReplacementInfo {
    pub fn translate_record_offset(&self, old: u64) -> Result<u64> {
        if old <= self.record_offset {
            return Ok(old);
        }
        if self.new_physical_len >= self.old_physical_len {
            old.checked_add(self.new_physical_len - self.old_physical_len)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "replacement record offset",
                })
        } else {
            old.checked_sub(self.old_physical_len - self.new_physical_len)
                .ok_or(Error::ResourceArithmeticOverflow {
                    resource: "replacement record offset",
                })
        }
    }
}

impl From<&RecordIndexEntry> for BlockEvent {
    fn from(entry: &RecordIndexEntry) -> Self {
        Self {
            block_id: entry.block_id,
            block_version: entry.block_version,
            sequence: entry.sequence,
            record_offset: entry.record_offset,
            payload_offset: entry.payload_offset,
            payload_len: entry.payload_len,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    pub original_len: u64,
    pub recovered_len: u64,
    pub records_preserved: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriterLockInfo {
    pub path: PathBuf,
    pub target_path: PathBuf,
    pub process_id: u32,
    pub created_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterLockBreakPolicy {
    Refuse,
    BreakIfProcessAbsent,
    BreakIfOlderThan(Duration),
    BreakIfProcessAbsentAndOlderThan(Duration),
}
