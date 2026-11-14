//! Scenario `grid`: a 6-D f32 array larger than memory whose time axis grows,
//! read by hyperslabs in every direction.
//!
//! Situation (the only thing this module decides):
//! - Model output [time 64, ensemble 8, variable 4, level 16, lat 256,
//!   lon 512] (17.2 GB; `FB_GRID=small`: [16, 8, 4, 8, 128, 128]); time steps
//!   arrive one at a time and the file must hold every step written so far.
//! - Then, from a cold cache: lines along each of the six axes, planes over
//!   each of the 15 axis pairs, random sub-cubes and ensemble means. Every
//!   selection is compared element-wise to the oracle.
//! - Objective: total read time. Write time and size are reported. Runs under
//!   the harness memory cap (data ~4x the memory available).
//!
//! Chunk shape (from the candidate shapes below, all 32 KiB), how chunks are
//! keyed, stored and found is each adapter's choice (`candidates`). A shape
//! deeper than one time step must be completed across several arriving
//! steps (read-modify-write or buffering); engines that cannot do that fail
//! that candidate.
use crate::common::*;
use std::path::{Path, PathBuf};

mod h5;
mod pq;
mod sq;
mod vl_keyed;
mod vl_matrix;

pub const CE: usize = 8192; // elements per chunk, every candidate shape
pub const CB: usize = CE * 4;

/// Candidate chunk shapes (8192 elements each).
pub fn chunk_shape(name: &str) -> Result<Ix> {
    Ok(match name {
        "bal" => [1, 2, 1, 4, 32, 32],
        "plane" => [1, 1, 1, 1, 64, 128],
        "cube" => [1, 2, 2, 2, 32, 32],
        "time" => [8, 1, 1, 1, 32, 32],
        _ => return Err(format!("unknown chunk shape {name}").into()),
    })
}
pub const SHAPES: &[&str] = &["bal", "plane", "cube", "time"];

pub fn shape() -> Ix {
    match std::env::var("FB_GRID").as_deref() {
        Ok("small") => [16, 8, 4, 8, 128, 128],
        _ => [64, 8, 4, 16, 256, 512],
    }
}
/// Elements per time step.
pub fn step_len() -> usize {
    shape()[1..].iter().product()
}

pub type Ix = [usize; 6];

/// A chunking of the array: chunk shape and chunks per axis.
#[derive(Clone, Copy, Debug)]
pub struct Ck {
    pub c: Ix,
    pub cg: Ix,
}
impl Ck {
    pub fn new(name: &str) -> Result<Self> {
        let c = chunk_shape(name)?;
        let sh = shape();
        let mut cg = [0; 6];
        for d in 0..6 {
            if sh[d] % c[d] != 0 {
                return Err(format!("chunk {c:?} does not divide {sh:?}").into());
            }
            cg[d] = sh[d] / c[d];
        }
        Ok(Self { c, cg })
    }
    /// Chunks per time-chunk row (all chunks sharing one time-chunk index).
    pub fn per_tchunk(&self) -> usize {
        self.cg[1..].iter().product()
    }
    pub fn id(&self, c: &Ix) -> usize {
        lin(c, &self.cg)
    }
    pub fn coords(&self, mut id: usize) -> Ix {
        let mut c = [0; 6];
        for d in (0..6).rev() {
            c[d] = id % self.cg[d];
            id /= self.cg[d];
        }
        c
    }
    /// Runs of consecutive chunk ids touched by a selection: `f(first_id, n)`.
    pub fn runs(&self, start: &Ix, count: &Ix, mut f: impl FnMut(usize, usize)) {
        let mut lo = [0; 6];
        let mut hi = [0; 6];
        for d in 0..6 {
            lo[d] = start[d] / self.c[d];
            hi[d] = (start[d] + count[d] - 1) / self.c[d] + 1;
        }
        each_run(&lo, &hi, |c| f(self.id(c), hi[5] - lo[5]));
    }
    /// Copy the part of chunk `id` (little-endian bytes) inside the selection
    /// into `out` (row-major, shape `count`).
    pub fn copy_out(&self, start: &Ix, count: &Ix, id: usize, chunk: &[u8], out: &mut [f32]) {
        let c = self.coords(id);
        let mut lo = [0; 6];
        let mut hi = [0; 6];
        for d in 0..6 {
            lo[d] = start[d].max(c[d] * self.c[d]);
            hi[d] = (start[d] + count[d]).min((c[d] + 1) * self.c[d]);
        }
        let os = strides(count);
        let cs = strides(&self.c);
        let n = hi[5].saturating_sub(lo[5]);
        each_run(&lo, &hi, |i| {
            let (mut o, mut k) = (0, 0);
            for d in 0..6 {
                o += (i[d] - start[d]) * os[d];
                k += (i[d] - c[d] * self.c[d]) * cs[d];
            }
            for (o, b) in out[o..o + n]
                .iter_mut()
                .zip(chunk[k * 4..(k + n) * 4].chunks_exact(4))
            {
                *o = f32::from_le_bytes(b.try_into().unwrap());
            }
        });
    }
    /// Write time step `t` (row-major over shape()[1..]) into its slice of
    /// chunk `id` (which must contain `t`); other time slices are untouched.
    pub fn put_step(&self, step: &[f32], t: usize, id: usize, out: &mut [u8]) {
        let c = self.coords(id);
        let mut lo = [0; 6];
        let mut hi = [0; 6];
        for d in 1..6 {
            lo[d] = c[d] * self.c[d];
            hi[d] = lo[d] + self.c[d];
        }
        hi[0] = 1;
        let ss = strides(&shape());
        let cs = strides(&self.c);
        let dt = t % self.c[0];
        each_run(&lo, &hi, |i| {
            let s: usize = (1..6).map(|d| i[d] * ss[d]).sum();
            let mut k = dt * cs[0];
            for d in 1..6 {
                k += (i[d] - c[d] * self.c[d]) * cs[d];
            }
            for (j, v) in step[s..s + self.c[5]].iter().enumerate() {
                out[(k + j) * 4..(k + j) * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        });
    }
    /// Chunk ids holding time step `t`.
    pub fn ids_of_step(&self, t: usize) -> std::ops::Range<usize> {
        let tc = t / self.c[0];
        tc * self.per_tchunk()..(tc + 1) * self.per_tchunk()
    }
}

fn strides(shape: &Ix) -> Ix {
    let mut s = [1; 6];
    for d in (0..5).rev() {
        s[d] = s[d + 1] * shape[d + 1];
    }
    s
}
fn lin(i: &Ix, shape: &Ix) -> usize {
    let s = strides(shape);
    (0..6).map(|d| i[d] * s[d]).sum()
}
pub fn value_at(l: usize) -> f32 {
    (splitmix(l as u64) >> 40) as f32 / 1024.0
}

/// Calls `f(i)` for every index in [lo, hi) over axes 0..5 with `i[5] = lo[5]`.
fn each_run(lo: &Ix, hi: &Ix, mut f: impl FnMut(&Ix)) {
    if (0..6).any(|d| lo[d] >= hi[d]) {
        return;
    }
    let mut i = *lo;
    loop {
        f(&i);
        let mut d = 4;
        loop {
            i[d] += 1;
            if i[d] < hi[d] {
                break;
            }
            i[d] = lo[d];
            if d == 0 {
                return;
            }
            d -= 1;
        }
    }
}

pub trait GridWrite {
    /// One time step, row-major over shape()[1..].
    fn step(&mut self, t: usize, step: &[f32]) -> Result<()>;
    fn finish(self: Box<Self>) -> Result<()>;
}
pub trait GridRead {
    /// Row-major hyperslab `start .. start + count`.
    fn slab(&mut self, start: Ix, count: Ix, out: &mut Vec<f32>) -> Result<()>;
}
pub fn prepare(count: &Ix, out: &mut Vec<f32>) {
    out.clear();
    out.resize(count.iter().product(), 0.0);
}

pub fn candidates(engine: &str) -> Result<&'static [&'static str]> {
    Ok(match engine {
        "varlit" => &[
            "matrix-bal",
            "matrix-plane",
            "matrix-cube",
            "matrix-time",
            "keyed-bal",
            "keyed-plane",
            "keyed-cube",
            "keyed-time",
        ],
        "sqlite" | "hdf5" | "parquet" => SHAPES,
        _ => return Err(format!("unknown engine {engine}").into()),
    })
}
pub fn adapter_files(engine: &str, cand: &str) -> &'static str {
    match engine {
        "varlit" if cand.starts_with("keyed") => "src/grid/vl_keyed.rs",
        "varlit" => "src/grid/vl_matrix.rs",
        "sqlite" => "src/grid/sq.rs",
        "hdf5" => "src/grid/h5.rs",
        "parquet" => "src/grid/pq.rs",
        _ => "",
    }
}
fn create(engine: &str, cand: &str, p: &Path) -> Result<Box<dyn GridWrite>> {
    let shape_of = |c: &str| Ck::new(c.rsplit('-').next().unwrap());
    match engine {
        "varlit" if cand.starts_with("keyed") => vl_keyed::create(p, shape_of(cand)?),
        "varlit" => vl_matrix::create(p, shape_of(cand)?),
        "sqlite" => sq::create(p, shape_of(cand)?),
        "hdf5" => h5::create(p, shape_of(cand)?),
        "parquet" => pq::create(p, shape_of(cand)?),
        _ => Err("engine".into()),
    }
}
fn open(engine: &str, cand: &str, p: &Path) -> Result<Box<dyn GridRead>> {
    let shape_of = |c: &str| Ck::new(c.rsplit('-').next().unwrap());
    match engine {
        "varlit" if cand.starts_with("keyed") => vl_keyed::open(p, shape_of(cand)?),
        "varlit" => vl_matrix::open(p, shape_of(cand)?),
        "sqlite" => sq::open(p, shape_of(cand)?),
        "hdf5" => h5::open(p, shape_of(cand)?),
        _ => pq::open(p, shape_of(cand)?),
    }
}

// ---------------------------------------------------------------- driver
fn verify(start: &Ix, count: &Ix, v: &[f32], what: &str) -> Result<()> {
    if v.len() != count.iter().product::<usize>() {
        return Err(format!("{what} size").into());
    }
    let mut bad = false;
    let mut hi = [0; 6];
    for d in 0..6 {
        hi[d] = start[d] + count[d];
    }
    let mut k = 0;
    each_run(start, &hi, |i| {
        let base = lin(i, &shape());
        for x in 0..count[5] {
            bad |= v[k] != value_at(base + x);
            k += 1;
        }
    });
    if bad {
        return Err(format!("{what} mismatch").into());
    }
    Ok(())
}

pub fn run(fmt: &str, dir: &Path) -> Result<Metrics> {
    let cand = cand();
    let cand = if cand.is_empty() {
        candidates(fmt)?[0].to_string()
    } else {
        cand
    };
    let p = dir.join(format!("grid.{fmt}"));
    let mut m = Metrics::default();
    m.set("candidate", cand.as_str());
    let sh = shape();
    m.set("shape", format!("{sh:?}"));
    let mut step = vec![0f32; step_len()];
    let t = Timer::start();
    let mut gen_s = 0.0;
    let mut w = create(fmt, &cand, &p)?;
    for ts in 0..sh[0] {
        let g = Timer::start();
        let base = ts * step_len();
        for (j, v) in step.iter_mut().enumerate() {
            *v = value_at(base + j);
        }
        gen_s += g.secs();
        w.step(ts, &step)?;
    }
    w.finish()?;
    drop(step);
    m.set("write_s", t.secs() - gen_s);
    m.set("bytes", dir_bytes(dir));
    m.set("writer_peak_rss_mb", peak_rss_mb());

    // Query plan (untimed): every direction.
    let mut rng = 0x5EEDu64;
    let mut next = |n: usize| {
        rng = splitmix(rng);
        (rng % n as u64) as usize
    };
    let point = |next: &mut dyn FnMut(usize) -> usize| {
        let mut s = [0; 6];
        for d in 0..6 {
            s[d] = next(sh[d]);
        }
        s
    };
    let mut sets: Vec<(String, Vec<(Ix, Ix)>)> = Vec::new();
    for d in 0..6 {
        let sels = (0..8)
            .map(|_| {
                let mut s = point(&mut next);
                let mut c = [1; 6];
                s[d] = 0;
                c[d] = sh[d];
                (s, c)
            })
            .collect();
        sets.push((format!("line{d}"), sels));
    }
    for a in 0..6 {
        for b in a + 1..6 {
            let sels = (0..4)
                .map(|_| {
                    let mut s = point(&mut next);
                    let mut c = [1; 6];
                    for d in [a, b] {
                        s[d] = 0;
                        c[d] = sh[d];
                    }
                    (s, c)
                })
                .collect();
            sets.push((format!("plane{a}{b}"), sels));
        }
    }
    let cube = [8, 4, 2, 4, 64, 64];
    let cubes = (0..16)
        .map(|_| {
            let mut s = [0; 6];
            for d in 0..6 {
                s[d] = next(sh[d] - cube[d] + 1);
            }
            (s, cube)
        })
        .collect();
    sets.push(("cubes".into(), cubes));
    let (ne, ny, nx) = (sh[1], sh[4], sh[5]);
    let means: Vec<(Ix, Ix)> = (0..4)
        .map(|_| {
            (
                [next(sh[0]), 0, next(sh[2]), next(sh[3]), 0, 0],
                [1, ne, 1, 1, ny, nx],
            )
        })
        .collect();

    // One pass from a cold cache; under the memory cap nothing stays warm.
    evict(dir);
    let t = Timer::start();
    let mut r = open(fmt, &cand, &p)?;
    m.set("open_ms", t.secs() * 1e3);
    let mut total = 0.0;
    let mut out = Vec::new();
    for (name, sels) in &sets {
        let mut secs = 0.0;
        for (s, c) in sels {
            let t = Timer::start();
            r.slab(*s, *c, &mut out)?;
            secs += t.secs();
            verify(s, c, &out, name)?;
        }
        m.set(&format!("{name}_s"), secs);
        total += secs;
    }
    // Ensemble mean: read all members of a lat x lon plane, average.
    let mut secs = 0.0;
    for (s, c) in &means {
        let t = Timer::start();
        r.slab(*s, *c, &mut out)?;
        let plane = ny * nx;
        let mut mean = vec![0f32; plane];
        for e in 0..ne {
            for (a, v) in mean.iter_mut().zip(&out[e * plane..(e + 1) * plane]) {
                *a += v;
            }
        }
        mean.iter_mut().for_each(|a| *a /= ne as f32);
        secs += t.secs();
        std::hint::black_box(&mean);
        verify(s, c, &out, "ensmean")?;
    }
    m.set("ensmean_s", secs);
    total += secs;
    m.set("reads_s", total);
    m.set("peak_rss_mb", peak_rss_mb());
    m.set("verified", true);
    Ok(m)
}
