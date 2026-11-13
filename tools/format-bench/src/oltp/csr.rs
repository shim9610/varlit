//! Application-built secondary index: account ids grouped by branch, produced
//! with bounded memory (bucketed external sort through temporary files), for
//! engines that do not maintain secondary indexes themselves.
//! app-built: branch -> ids secondary index (external bucket sort)
use super::*;
use std::io::{BufReader, BufWriter, Read, Write};

const BUCKETS: u64 = 64;

/// Calls `each(branch, ids)` for every branch in order (ids ascending; empty
/// branches included). Memory: one bucket (~1/64 of the pairs).
pub fn by_branch(tmp: &Path, mut each: impl FnMut(u32, &[u64]) -> Result<()>) -> Result<()> {
    std::fs::create_dir_all(tmp)?;
    let nb = branches() as u64;
    let path = |k: u64| tmp.join(format!("csr-{k}.bin"));
    {
        let mut ws = (0..BUCKETS)
            .map(|k| {
                Ok(BufWriter::with_capacity(
                    1 << 16,
                    std::fs::File::create(path(k))?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        for id in 0..accounts() {
            let b = branch_of(id) as u64;
            let w = &mut ws[(b * BUCKETS / nb) as usize];
            w.write_all(&(b as u32).to_le_bytes())?;
            w.write_all(&id.to_le_bytes())?;
        }
        for w in ws.iter_mut() {
            w.flush()?;
        }
    }
    let mut next = 0u32;
    let mut ids = Vec::new();
    for k in 0..BUCKETS {
        let mut pairs: Vec<(u32, u64)> = Vec::new();
        let mut r = BufReader::with_capacity(1 << 16, std::fs::File::open(path(k))?);
        let mut rec = [0u8; 12];
        loop {
            match r.read_exact(&mut rec) {
                Ok(()) => pairs.push((
                    u32::from_le_bytes(rec[0..4].try_into().unwrap()),
                    u64::from_le_bytes(rec[4..12].try_into().unwrap()),
                )),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
        }
        std::fs::remove_file(path(k))?;
        pairs.sort_unstable();
        let mut i = 0;
        while i < pairs.len() {
            let b = pairs[i].0;
            while next < b {
                each(next, &[])?;
                next += 1;
            }
            ids.clear();
            while i < pairs.len() && pairs[i].0 == b {
                ids.push(pairs[i].1);
                i += 1;
            }
            each(b, &ids)?;
            next = b + 1;
        }
    }
    while (next as u64) < nb {
        each(next, &[])?;
        next += 1;
    }
    Ok(())
}
