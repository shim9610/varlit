//! Reader-local fixed-width temporary directories. The memory budget controls
//! buffering, never the number of rows that can be addressed.
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};

#[derive(Debug)]
pub(crate) struct SpillRows<const N: usize> {
    file: Option<File>,
    spilled: usize,
    buffer: Vec<u8>,
    capacity: usize,
}
impl<const N: usize> SpillRows<N> {
    pub(crate) fn new(bytes: usize) -> Self {
        assert!(N != 0);
        Self {
            file: None,
            spilled: 0,
            buffer: Vec::new(),
            capacity: bytes.max(N) / N * N,
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.spilled + self.buffer.len() / N
    }
    pub(crate) fn push(&mut self, row: [u8; N]) -> io::Result<()> {
        if self.buffer.len() >= self.capacity {
            if self.file.is_none() {
                self.file = Some(tempfile::tempfile()?);
            }
            let offset = (self.spilled as u64)
                .checked_mul(N as u64)
                .ok_or(io::ErrorKind::InvalidInput)?;
            let file = self.file.as_mut().unwrap();
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&self.buffer)?;
            self.spilled += self.buffer.len() / N;
            self.buffer.clear();
        }
        if self.buffer.capacity() - self.buffer.len() < N {
            let target = self
                .buffer
                .capacity()
                .saturating_mul(2)
                .max(N)
                .min(self.capacity);
            self.buffer
                .try_reserve_exact(target - self.buffer.len())
                .map_err(io::Error::other)?;
        }
        self.buffer.extend_from_slice(&row);
        Ok(())
    }
    pub(crate) fn get(&self, index: usize) -> io::Result<Option<[u8; N]>> {
        if index >= self.len() {
            return Ok(None);
        }
        let mut row = [0; N];
        if index >= self.spilled {
            let at = (index - self.spilled) * N;
            row.copy_from_slice(&self.buffer[at..at + N]);
        } else {
            let mut at = (index as u64)
                .checked_mul(N as u64)
                .ok_or(io::ErrorKind::InvalidInput)?;
            let mut remaining = row.as_mut_slice();
            while !remaining.is_empty() {
                let n = match crate::snapshot::read_at(self.file.as_ref().unwrap(), remaining, at) {
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    r => r?,
                };
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                at += n as u64;
                remaining = &mut remaining[n..];
            }
        }
        Ok(Some(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_between_spills_do_not_move_the_append_position() {
        let mut rows = SpillRows::<8>::new(16);
        for i in 0..100u64 {
            rows.push(i.to_le_bytes()).unwrap();
            for j in [0, i / 2, i] {
                assert_eq!(rows.get(j as usize).unwrap(), Some(j.to_le_bytes()));
            }
            assert!(rows.buffer.len() <= 16);
        }
        assert_eq!(rows.get(100).unwrap(), None);
    }
}
