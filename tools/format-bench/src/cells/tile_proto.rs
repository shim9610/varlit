//! PROTOTYPE (not Varlit): a matrix stored as user-shaped tiles appended to
//! the file, with cells overwritten in place.
//!
//! What a `tile = [TR, TC]` declaration would make the macro emit is modelled
//! with const generics: the tile shape, slot stride and tile size are
//! compile-time constants, so locating a cell is constant arithmetic.
//!
//! Layout of the main file:
//! - page 0: header (magic written last at create, shape, directory version);
//! - page 1: directory root. The directory is a 4-level radix table of
//!   512-entry pages keyed by the Morton code of the tile coordinates, so
//!   any dimension can grow without renumbering existing tiles;
//! - then tiles and directory pages, appended as they are first needed. A
//!   tile is a 64-byte header followed by `TR * TC` slots of `CELL + 4` bytes
//!   (the cell, then its CRC32). A tile never written does not exist.
//!
//! Updates are written in place. Two things in-place writing alone does not
//! give, and which this scenario requires, are added:
//! - atomicity across a crash: a redo journal (`<file>.tj`). A publish
//!   appends its byte ranges plus a checksummed trailer, syncs the journal
//!   (the one barrier per publish), then writes the ranges in place. The main
//!   file is synced only at checkpoints (journal over `TILE_CHECKPOINT`
//!   bytes), after which the journal is reset. Recovery replays every whole
//!   frame of the current epoch; redo is idempotent.
//! - isolation from readers: a lock file (`<file>.tl`). Readers hold it
//!   shared for one observation, the writer exclusively while applying.
//!
//! `-nojournal` candidates skip the journal: an upper bound on speed that does
//! NOT survive a crash mid-publish.
//!
//! Every write goes through one coalescing buffer of `TILE_BUF` bytes
//! (default 64 KiB): adjacent ranges become one positional write.
use super::*;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;

const PAGE: u64 = 4096;
const MAGIC: &[u8; 8] = b"TILEPRO1";
const DIR_VERSION_AT: u64 = 40;
const ROOT: u64 = PAGE;
const DATA_START: u64 = 2 * PAGE;
const FANOUT: u64 = 512;
const LEVELS: u32 = 4;
const TILE_HEADER: u64 = 64;
const SLOT: u64 = CELL as u64 + 4;
const COLS: u64 = 1024;
const JOURNAL_HEADER: u64 = 16;
const ENTRY: u8 = 1;
const TRAILER: u8 = 2;

pub const CANDIDATES: &[&str] = &["t8x8", "t1x64", "t8x8-nojournal"];

fn buf_len() -> usize {
    std::env::var("TILE_BUF")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64 * 1024)
}
fn checkpoint_len() -> u64 {
    std::env::var("TILE_CHECKPOINT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64 << 20)
}

fn morton(r: u32, c: u32) -> u64 {
    fn spread(x: u32) -> u64 {
        let mut x = x as u64;
        x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
        x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
        x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
        x = (x | (x << 2)) & 0x3333_3333_3333_3333;
        x = (x | (x << 1)) & 0x5555_5555_5555_5555;
        x
    }
    spread(r) | (spread(c) << 1)
}
fn level_index(key: u64, level: u32) -> u64 {
    (key >> (9 * (LEVELS - 1 - level))) & (FANOUT - 1)
}

/// The constants a `tile = [TR, TC]` declaration fixes at compile time.
pub struct Shape<const TR: u64, const TC: u64>;
impl<const TR: u64, const TC: u64> Shape<TR, TC> {
    const CELLS: u64 = TR * TC;
    const TILE_BYTES: u64 = TILE_HEADER + Self::CELLS * SLOT;
    /// (tile key, byte offset of the slot inside its tile).
    #[inline(always)]
    fn locate(id: u32) -> (u64, u64) {
        let (r, c) = (id as u64 / COLS, id as u64 % COLS);
        let key = morton((r / TR) as u32, (c / TC) as u32);
        let slot = (r % TR) * TC + (c % TC);
        (key, TILE_HEADER + slot * SLOT)
    }
}

fn read_u64(file: &File, at: u64) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    file.read_exact_at(&mut b, at)?;
    Ok(u64::from_le_bytes(b))
}

/// Writes byte ranges through one bounded buffer, merging adjacent ranges
/// into one positional write.
struct Coalescer {
    buf: Vec<u8>,
    at: u64,
    cap: usize,
    pub writes: u64,
}
impl Coalescer {
    fn new(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            at: 0,
            cap,
            writes: 0,
        }
    }
    fn push(&mut self, file: &File, at: u64, mut bytes: &[u8]) -> std::io::Result<()> {
        if self.buf.is_empty() || at != self.at + self.buf.len() as u64 {
            self.flush(file)?;
            self.at = at;
        }
        while !bytes.is_empty() {
            let room = self.cap - self.buf.len();
            let n = room.min(bytes.len());
            self.buf.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if self.buf.len() == self.cap {
                let next = self.at + self.buf.len() as u64;
                self.flush(file)?;
                self.at = next;
            }
        }
        Ok(())
    }
    fn flush(&mut self, file: &File) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            file.write_all_at(&self.buf, self.at)?;
            self.writes += 1;
            self.buf.clear();
        }
        Ok(())
    }
}

/// Pending in-place writes of one publish.
#[derive(Default)]
struct Pending {
    bytes: Vec<u8>,
    ranges: Vec<(u64, u32, u32)>,
}
impl Pending {
    fn push(&mut self, at: u64, bytes: &[u8]) {
        self.ranges
            .push((at, self.bytes.len() as u32, bytes.len() as u32));
        self.bytes.extend_from_slice(bytes);
    }
    fn clear(&mut self) {
        self.bytes.clear();
        self.ranges.clear();
    }
    /// Applies in file order (a later write of the same range wins).
    fn apply(&mut self, file: &File, out: &mut Coalescer) -> std::io::Result<()> {
        let mut order: Vec<usize> = (0..self.ranges.len()).collect();
        order.sort_by_key(|&i| (self.ranges[i].0, i));
        for i in order {
            let (at, start, len) = self.ranges[i];
            out.push(file, at, &self.bytes[start as usize..(start + len) as usize])?;
        }
        out.flush(file)
    }
}

pub struct TileWriter<const TR: u64, const TC: u64> {
    main: File,
    journal: Option<File>,
    lock: File,
    end: u64,
    jlen: u64,
    epoch: u64,
    dir: HashMap<u64, Box<[u64; FANOUT as usize]>>,
    dir_version: u64,
    dir_version_written: u64,
    pending: Pending,
    out: Coalescer,
    loading: bool,
    pub barriers: u64,
}

fn paths(p: &Path) -> (PathBuf, PathBuf) {
    let mut j = p.as_os_str().to_owned();
    j.push(".tj");
    let mut l = p.as_os_str().to_owned();
    l.push(".tl");
    (j.into(), l.into())
}

impl<const TR: u64, const TC: u64> TileWriter<TR, TC> {
    pub fn create(p: &Path, journaled: bool) -> Result<Self> {
        const { assert!(Shape::<TR, TC>::TILE_BYTES <= 64 << 20, "tile exceeds a record") };
        let (jp, lp) = paths(p);
        let main = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(p)?;
        main.set_len(DATA_START)?;
        let mut header = [0u8; 40];
        header[8..16].copy_from_slice(&TR.to_le_bytes());
        header[16..24].copy_from_slice(&TC.to_le_bytes());
        header[24..32].copy_from_slice(&(CELL as u64).to_le_bytes());
        header[32..40].copy_from_slice(&COLS.to_le_bytes());
        main.write_all_at(&header, 0)?;
        let journal = if journaled {
            let j = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&jp)?;
            write_journal_header(&j, 1)?;
            Some(j)
        } else {
            None
        };
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lp)?;
        Ok(Self {
            main,
            journal,
            lock,
            end: DATA_START,
            jlen: JOURNAL_HEADER,
            epoch: 1,
            dir: HashMap::new(),
            dir_version: 0,
            dir_version_written: 0,
            pending: Pending::default(),
            out: Coalescer::new(buf_len()),
            loading: true,
            barriers: 0,
        })
    }

    /// Reopens after a crash: replays every whole journal frame of the
    /// current epoch into the main file, makes it durable, resets the journal.
    pub fn open(p: &Path) -> Result<Self> {
        let (jp, lp) = paths(p);
        let main = OpenOptions::new().read(true).write(true).open(p)?;
        let mut magic = [0u8; 8];
        main.read_exact_at(&mut magic, 0)?;
        if &magic != MAGIC {
            return Err("tile file was never completed".into());
        }
        if read_u64(&main, 8)? != TR || read_u64(&main, 16)? != TC {
            return Err("tile shape differs from the declaration".into());
        }
        let journal = OpenOptions::new().read(true).write(true).open(&jp)?;
        let epoch = recover(&main, &journal)?;
        main.sync_data()?;
        write_journal_header(&journal, epoch + 1)?;
        journal.set_len(JOURNAL_HEADER)?;
        journal.sync_data()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lp)?;
        let end = main.metadata()?.len().next_multiple_of(PAGE);
        let dir_version = read_u64(&main, DIR_VERSION_AT)?;
        Ok(Self {
            dir_version,
            dir_version_written: dir_version,
            main,
            journal: Some(journal),
            lock,
            end,
            jlen: JOURNAL_HEADER,
            epoch: epoch + 1,
            dir: HashMap::new(),
            pending: Pending::default(),
            out: Coalescer::new(buf_len()),
            loading: false,
            barriers: 2,
        })
    }

    fn dir_page(&mut self, page: u64) -> Result<&mut [u64; FANOUT as usize]> {
        if !self.dir.contains_key(&page) {
            let mut raw = [0u8; PAGE as usize];
            // A page past the end of the file has never been written.
            let _ = self.main.read_at(&mut raw, page)?;
            let mut entries = Box::new([0u64; FANOUT as usize]);
            for (e, b) in entries.iter_mut().zip(raw.chunks_exact(8)) {
                *e = u64::from_le_bytes(b.try_into().unwrap());
            }
            self.dir.insert(page, entries);
        }
        Ok(self.dir.get_mut(&page).unwrap())
    }

    /// The tile for `key`, allocating it (and directory pages) when absent.
    fn tile(&mut self, key: u64) -> Result<u64> {
        let mut page = ROOT;
        for level in 0..LEVELS {
            let index = level_index(key, level);
            let entry = self.dir_page(page)?[index as usize];
            let next = if entry != 0 {
                entry
            } else {
                let last = level + 1 == LEVELS;
                let at = self.end.next_multiple_of(if last { 64 } else { PAGE });
                self.end = at + if last { Shape::<TR, TC>::TILE_BYTES } else { PAGE };
                self.dir_page(page)?[index as usize] = at;
                self.pending.push(page + index * 8, &at.to_le_bytes());
                if last {
                    let mut header = [0u8; 24];
                    header[..8].copy_from_slice(b"TILEHDR1");
                    header[8..16].copy_from_slice(&key.to_le_bytes());
                    self.pending.push(at, &header);
                }
                self.dir_version += 1;
                at
            };
            if level + 1 == LEVELS {
                return Ok(next);
            }
            page = next;
        }
        unreachable!()
    }

    pub fn write_cell(&mut self, id: u32, cell: &[u8]) -> Result<()> {
        let (key, within) = Shape::<TR, TC>::locate(id);
        let tile = self.tile(key)?;
        let mut slot = [0u8; SLOT as usize];
        slot[..CELL].copy_from_slice(cell);
        slot[CELL..].copy_from_slice(&crc32fast::hash(cell).to_le_bytes());
        self.pending.push(tile + within, &slot);
        Ok(())
    }

    pub fn commit(&mut self, durable: bool) -> Result<()> {
        if self.pending.ranges.is_empty() {
            return Ok(());
        }
        if self.dir_version != self.dir_version_written {
            let v = self.dir_version;
            self.pending.push(DIR_VERSION_AT, &v.to_le_bytes());
            self.dir_version_written = v;
        }
        if self.loading {
            // Initial load: no readers, no journal; made durable by `loaded`.
            self.pending.apply(&self.main, &mut self.out)?;
            self.pending.clear();
            return Ok(());
        }
        if let Some(journal) = &self.journal {
            let mut crc = crc32fast::Hasher::new();
            let mut head = [0u8; 13];
            for &(at, start, len) in &self.pending.ranges {
                head[0] = ENTRY;
                head[1..9].copy_from_slice(&at.to_le_bytes());
                head[9..13].copy_from_slice(&len.to_le_bytes());
                let body = &self.pending.bytes[start as usize..(start + len) as usize];
                crc.update(&head);
                crc.update(body);
                self.out.push(journal, self.jlen, &head)?;
                self.jlen += 13;
                self.out.push(journal, self.jlen, body)?;
                self.jlen += len as u64;
            }
            let count = self.pending.ranges.len() as u32;
            crc.update(&self.epoch.to_le_bytes());
            crc.update(&count.to_le_bytes());
            let mut trailer = [0u8; 17];
            trailer[0] = TRAILER;
            trailer[1..9].copy_from_slice(&self.epoch.to_le_bytes());
            trailer[9..13].copy_from_slice(&count.to_le_bytes());
            trailer[13..17].copy_from_slice(&crc.finalize().to_le_bytes());
            self.out.push(journal, self.jlen, &trailer)?;
            self.jlen += 17;
            self.out.flush(journal)?;
            // Visibility waits for durability: a page the OS writes back
            // before its journal frame is durable could not be undone.
            journal.sync_data()?;
            self.barriers += 1;
        }
        self.lock.lock()?;
        let applied = self.pending.apply(&self.main, &mut self.out);
        self.lock.unlock()?;
        applied?;
        self.pending.clear();
        if self.journal.is_none() {
            if durable {
                self.main.sync_data()?;
                self.barriers += 1;
            }
        } else if self.jlen > checkpoint_len() {
            self.checkpoint()?;
        }
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<()> {
        self.main.sync_data()?;
        let journal = self.journal.as_ref().unwrap();
        self.epoch += 1;
        write_journal_header(journal, self.epoch)?;
        journal.set_len(JOURNAL_HEADER)?;
        journal.sync_data()?;
        self.barriers += 2;
        self.jlen = JOURNAL_HEADER;
        Ok(())
    }

    /// Ends the initial load: everything durable, then the magic that makes
    /// the file valid.
    pub fn finish_load(&mut self) -> Result<()> {
        self.commit(true)?;
        self.loading = false;
        self.main.sync_data()?;
        self.main.write_all_at(MAGIC, 0)?;
        self.main.sync_data()?;
        if let Some(j) = &self.journal {
            j.sync_data()?;
        }
        Ok(())
    }

    /// For tests: writes the journal frame of the pending publish and stops,
    /// as a crash between the journal barrier and the in-place writes would.
    #[cfg(test)]
    fn crash_after_journal(mut self) -> Result<()> {
        let main = self.main.try_clone()?;
        // Point the in-place writes at a scratch file that is thrown away.
        self.main = scratch_file()?;
        self.commit(true)?;
        drop(main);
        Ok(())
    }
}

fn write_journal_header(j: &File, epoch: u64) -> std::io::Result<()> {
    let mut h = [0u8; JOURNAL_HEADER as usize];
    h[..4].copy_from_slice(b"TJNL");
    h[8..16].copy_from_slice(&epoch.to_le_bytes());
    j.write_all_at(&h, 0)
}

/// Replays whole frames of the journal's epoch; stops at the first frame that
/// is torn, foreign or missing its trailer. Returns the epoch.
fn recover(main: &File, journal: &File) -> Result<u64> {
    let epoch = read_u64(journal, 8)?;
    let len = journal.metadata()?.len();
    let mut pos = JOURNAL_HEADER;
    let mut frame = Pending::default();
    let mut crc = crc32fast::Hasher::new();
    let mut out = Coalescer::new(buf_len());
    loop {
        let mut tag = [0u8; 1];
        if pos >= len || journal.read_exact_at(&mut tag, pos).is_err() {
            break;
        }
        match tag[0] {
            ENTRY => {
                let mut head = [0u8; 13];
                if journal.read_exact_at(&mut head, pos).is_err() {
                    break;
                }
                let at = u64::from_le_bytes(head[1..9].try_into().unwrap());
                let n = u32::from_le_bytes(head[9..13].try_into().unwrap()) as usize;
                if pos + 13 + n as u64 > len || n > 1 << 20 {
                    break;
                }
                let mut body = vec![0u8; n];
                journal.read_exact_at(&mut body, pos + 13)?;
                crc.update(&head);
                crc.update(&body);
                frame.push(at, &body);
                pos += 13 + n as u64;
            }
            TRAILER => {
                let mut t = [0u8; 17];
                if journal.read_exact_at(&mut t, pos).is_err() {
                    break;
                }
                let e = u64::from_le_bytes(t[1..9].try_into().unwrap());
                let count = u32::from_le_bytes(t[9..13].try_into().unwrap());
                let stored = u32::from_le_bytes(t[13..17].try_into().unwrap());
                crc.update(&e.to_le_bytes());
                crc.update(&count.to_le_bytes());
                let done = std::mem::replace(&mut crc, crc32fast::Hasher::new()).finalize();
                if e != epoch || count as usize != frame.ranges.len() || done != stored {
                    break;
                }
                frame.apply(main, &mut out)?;
                frame.clear();
                pos += 17;
            }
            _ => break,
        }
    }
    Ok(epoch)
}

pub struct TileReader<const TR: u64, const TC: u64> {
    main: File,
    lock: File,
    locked: bool,
    dir: HashMap<u64, Box<[u64; FANOUT as usize]>>,
    dir_version: u64,
}

impl<const TR: u64, const TC: u64> TileReader<TR, TC> {
    pub fn open(p: &Path) -> Result<Self> {
        let (_, lp) = paths(p);
        let main = File::open(p)?;
        let mut magic = [0u8; 8];
        main.read_exact_at(&mut magic, 0)?;
        if &magic != MAGIC || read_u64(&main, 8)? != TR || read_u64(&main, 16)? != TC {
            return Err("not a tile file of this shape".into());
        }
        Ok(Self {
            main,
            lock: File::open(lp)?,
            locked: false,
            dir: HashMap::new(),
            dir_version: u64::MAX,
        })
    }
    pub fn begin(&mut self) -> Result<()> {
        if !self.locked {
            self.lock.lock_shared()?;
            self.locked = true;
        }
        let v = read_u64(&self.main, DIR_VERSION_AT)?;
        if v != self.dir_version {
            self.dir.clear();
            self.dir_version = v;
        }
        Ok(())
    }
    pub fn end(&mut self) -> Result<()> {
        if self.locked {
            self.lock.unlock()?;
            self.locked = false;
        }
        Ok(())
    }
    fn entry(&mut self, page: u64, index: u64) -> Result<u64> {
        if !self.dir.contains_key(&page) {
            let mut raw = [0u8; PAGE as usize];
            self.main.read_exact_at(&mut raw, page)?;
            let mut entries = Box::new([0u64; FANOUT as usize]);
            for (e, b) in entries.iter_mut().zip(raw.chunks_exact(8)) {
                *e = u64::from_le_bytes(b.try_into().unwrap());
            }
            self.dir.insert(page, entries);
        }
        Ok(self.dir[&page][index as usize])
    }
    pub fn read_cell(&mut self, id: u32, out: &mut [u8]) -> Result<()> {
        let (key, within) = Shape::<TR, TC>::locate(id);
        let mut page = ROOT;
        for level in 0..LEVELS {
            page = self.entry(page, level_index(key, level))?;
            if page == 0 {
                return Err(format!("cell {id}: tile absent").into());
            }
        }
        let mut slot = [0u8; SLOT as usize];
        self.main.read_exact_at(&mut slot, page + within)?;
        let stored = u32::from_le_bytes(slot[CELL..].try_into().unwrap());
        if stored == 0 && slot[..CELL].iter().all(|b| *b == 0) {
            return Err(format!("cell {id}: never written").into());
        }
        if crc32fast::hash(&slot[..CELL]) != stored {
            return Err(format!("cell {id}: checksum mismatch").into());
        }
        out.copy_from_slice(&slot[..CELL]);
        Ok(())
    }
}

// ---- harness adapters ----

struct W<const TR: u64, const TC: u64>(TileWriter<TR, TC>);
impl<const TR: u64, const TC: u64> CellWriter for W<TR, TC> {
    fn load(&mut self, first: u32, data: &[u8]) -> Result<()> {
        for (i, c) in data.chunks_exact(CELL).enumerate() {
            self.0.write_cell(first + i as u32, c)?;
        }
        self.0.commit(false)
    }
    fn loaded(&mut self) -> Result<()> {
        self.0.finish_load()?;
        eprintln!("tile: load done");
        Ok(())
    }
    fn write(&mut self, id: u32, cell: &[u8]) -> Result<()> {
        self.0.write_cell(id, cell)
    }
    fn publish(&mut self, durable: bool) -> Result<()> {
        self.0.commit(durable)
    }
}
impl<const TR: u64, const TC: u64> Drop for W<TR, TC> {
    fn drop(&mut self) {
        eprintln!(
            "tile writer: {} positional writes, {} barriers",
            self.0.out.writes, self.0.barriers
        );
    }
}
struct R<const TR: u64, const TC: u64>(TileReader<TR, TC>);
impl<const TR: u64, const TC: u64> CellReader for R<TR, TC> {
    fn refresh(&mut self) -> Result<()> {
        self.0.begin()
    }
    fn read(&mut self, id: u32, out: &mut [u8]) -> Result<()> {
        self.0.read_cell(id, out)
    }
    fn end(&mut self) -> Result<()> {
        self.0.end()
    }
}

pub fn create(p: &Path, cand: &str) -> Result<Box<dyn CellWriter>> {
    let journaled = !cand.ends_with("-nojournal");
    Ok(match cand.trim_end_matches("-nojournal") {
        "t8x8" => Box::new(W(TileWriter::<8, 8>::create(p, journaled)?)),
        "t1x64" => Box::new(W(TileWriter::<1, 64>::create(p, journaled)?)),
        c => return Err(format!("unknown tile candidate {c}").into()),
    })
}
pub fn open(p: &Path, cand: &str) -> Result<Box<dyn CellReader>> {
    Ok(match cand.trim_end_matches("-nojournal") {
        "t8x8" => Box::new(R(TileReader::<8, 8>::open(p)?)),
        "t1x64" => Box::new(R(TileReader::<1, 64>::open(p)?)),
        c => return Err(format!("unknown tile candidate {c}").into()),
    })
}

/// `tile-bufsweep <dir>`: one publish of `n` adjacent dirty cells per buffer
/// size, journaled; reports wall time and positional writes.
pub fn buffer_sweep(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let n: u32 = std::env::var("TILE_SWEEP_CELLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let mut cell = vec![0u8; CELL];
    for cap in [4096usize, 16 << 10, 64 << 10, 256 << 10, 1 << 20] {
        // SAFETY: single-threaded; read by `buf_len` below.
        unsafe { std::env::set_var("TILE_BUF", cap.to_string()) };
        let p = dir.join(format!("sweep-{cap}.tile"));
        let mut w = TileWriter::<1, 64>::create(&p, true)?;
        for id in 0..n {
            fill(id, 0, 0, &mut cell);
            w.write_cell(id, &cell)?;
        }
        w.commit(false)?;
        w.finish_load()?;
        let mut best = f64::MAX;
        let mut writes = 0;
        for round in 1..=5u64 {
            for id in 0..n {
                fill(id, round, 0, &mut cell);
                w.write_cell(id, &cell)?;
            }
            let before = w.out.writes;
            let t = Timer::start();
            w.commit(true)?;
            best = best.min(t.secs());
            writes = w.out.writes - before;
        }
        println!(
            "{{\"buf\":{cap},\"cells\":{n},\"publish_ms\":{:.3},\"positional_writes\":{writes}}}",
            best * 1e3
        );
        drop(w);
        let (jp, lp) = paths(&p);
        for f in [p, jp, lp] {
            let _ = std::fs::remove_file(f);
        }
    }
    Ok(())
}

#[cfg(test)]
fn scratch_file() -> std::io::Result<File> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "tile-scratch-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let f = OpenOptions::new().read(true).write(true).create_new(true).open(&p)?;
    std::fs::remove_file(&p)?;
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "tile-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cell(id: u32, v: u64) -> Vec<u8> {
        let mut c = vec![0u8; CELL];
        fill(id, v, 0, &mut c);
        c
    }

    fn grid(p: &Path, n: u32) -> TileWriter<8, 8> {
        let mut w = TileWriter::<8, 8>::create(p, true).unwrap();
        for id in 0..n {
            w.write_cell(id, &cell(id, 0)).unwrap();
        }
        w.commit(false).unwrap();
        w.finish_load().unwrap();
        w
    }

    #[test]
    fn cells_round_trip_and_grow_in_any_direction() {
        let d = TestDir::new();
        let p = d.path().join("g.tile");
        let mut w = grid(&p, 4096);
        // Far outside the loaded rows and in a new tile column.
        for id in [1_000_000u32, 5_000_000, 3 * 1024 + 1000] {
            w.write_cell(id, &cell(id, 7)).unwrap();
        }
        w.commit(true).unwrap();
        let mut r = TileReader::<8, 8>::open(&p).unwrap();
        r.begin().unwrap();
        let mut out = vec![0u8; CELL];
        for id in [0u32, 1, 4095] {
            r.read_cell(id, &mut out).unwrap();
            assert_eq!(out, cell(id, 0));
        }
        for id in [1_000_000u32, 5_000_000, 3 * 1024 + 1000] {
            r.read_cell(id, &mut out).unwrap();
            assert_eq!(out, cell(id, 7));
        }
        assert!(r.read_cell(9_000_000, &mut out).is_err());
        r.end().unwrap();
    }

    #[test]
    fn a_crash_after_the_journal_barrier_is_replayed_whole() {
        let d = TestDir::new();
        let p = d.path().join("c.tile");
        let mut w = grid(&p, 4096);
        for id in [5u32, 900, 4000, 2_000_000] {
            w.write_cell(id, &cell(id, 3)).unwrap();
        }
        w.crash_after_journal().unwrap();
        // The main file still holds the old values.
        let mut r = TileReader::<8, 8>::open(&p).unwrap();
        r.begin().unwrap();
        let mut out = vec![0u8; CELL];
        r.read_cell(5, &mut out).unwrap();
        assert_eq!(out, cell(5, 0));
        r.end().unwrap();
        drop(r);
        let _w = TileWriter::<8, 8>::open(&p).unwrap();
        let mut r = TileReader::<8, 8>::open(&p).unwrap();
        r.begin().unwrap();
        for id in [5u32, 900, 4000, 2_000_000] {
            r.read_cell(id, &mut out).unwrap();
            assert_eq!(out, cell(id, 3), "cell {id}");
        }
    }

    #[test]
    fn a_torn_journal_frame_is_not_replayed() {
        let d = TestDir::new();
        let p = d.path().join("t.tile");
        let mut w = grid(&p, 4096);
        for id in [5u32, 900] {
            w.write_cell(id, &cell(id, 3)).unwrap();
        }
        w.crash_after_journal().unwrap();
        let (jp, _) = paths(&p);
        let j = OpenOptions::new().write(true).open(&jp).unwrap();
        let len = j.metadata().unwrap().len();
        j.set_len(len - 3).unwrap();
        let _w = TileWriter::<8, 8>::open(&p).unwrap();
        let mut r = TileReader::<8, 8>::open(&p).unwrap();
        r.begin().unwrap();
        let mut out = vec![0u8; CELL];
        for id in [5u32, 900] {
            r.read_cell(id, &mut out).unwrap();
            assert_eq!(out, cell(id, 0), "a partial frame changed cell {id}");
        }
    }

    #[test]
    fn a_checkpoint_resets_the_journal_and_keeps_every_value() {
        let d = TestDir::new();
        let p = d.path().join("k.tile");
        // SAFETY: tests touching this variable only read it at construction.
        unsafe { std::env::set_var("TILE_CHECKPOINT", "200000") };
        let mut w = grid(&p, 4096);
        for round in 1..=10u64 {
            for id in (0..4096).step_by(31) {
                w.write_cell(id, &cell(id, round)).unwrap();
            }
            w.commit(true).unwrap();
        }
        assert!(w.barriers > 10, "no checkpoint ran");
        drop(w);
        let _w = TileWriter::<8, 8>::open(&p).unwrap();
        let mut r = TileReader::<8, 8>::open(&p).unwrap();
        r.begin().unwrap();
        let mut out = vec![0u8; CELL];
        for id in (0..4096).step_by(31) {
            r.read_cell(id, &mut out).unwrap();
            assert_eq!(out, cell(id, 10));
        }
    }

    #[test]
    fn the_coalescer_merges_adjacent_ranges_within_its_buffer() {
        let f = scratch_file().unwrap();
        let mut c = Coalescer::new(4096);
        for i in 0..8u64 {
            c.push(&f, i * 1024, &[i as u8; 1024]).unwrap();
        }
        c.push(&f, 100_000, &[9; 10]).unwrap();
        c.flush(&f).unwrap();
        assert_eq!(c.writes, 3);
        let mut b = [0u8; 1024];
        f.read_exact_at(&mut b, 5 * 1024).unwrap();
        assert_eq!(b, [5u8; 1024]);
    }
}
