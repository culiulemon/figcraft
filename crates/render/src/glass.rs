//! The glass effect: the backdrop seen through a pane whose edges curve down like a lens.
//!
//! Within `depth` of the outline the surface curves: the backdrop there is bent towards the inside
//! (strongest at the very edge, `refraction` × `depth` far), each colour channel by a slightly
//! different amount (`dispersion`, a rainbow fringe), and the rim catches the light: brightest
//! where it faces `light_angle`, half as bright on the opposite side, spread wider by `splay`.
//! `frost` blurs the backdrop first. Distances to the outline come from an exact Euclidean
//! distance transform of the shape's coverage.

use std::sync::{Arc, Mutex, OnceLock};

use figcraft_doc::Glass;
use figcraft_geom::Affine;
use tiny_skia::Pixmap;

use crate::fx;
use crate::shadow::{Outline, spread_alpha};

/// Squared distance transform of one row or column (Felzenszwalb & Huttenlocher): `f` holds 0 at
/// sources and a large value elsewhere; `d` gets each sample's squared distance to the nearest
/// source. `v` and `z` are scratch space (`f.len()` and `f.len() + 1` long).
fn edt_1d(f: &[f64], d: &mut [f64], v: &mut [usize], z: &mut [f64]) {
    let n = f.len();
    // Every index below stays under these lengths (k <= q < n), checked once here.
    if n == 0 || d.len() < n || v.len() < n || z.len() < n + 1 {
        return;
    }
    let mut k = 0usize;
    v[0] = 0;
    z[0] = f64::NEG_INFINITY;
    z[1] = f64::INFINITY;
    for q in 1..n {
        let qf = q as f64;
        let mut s;
        loop {
            let vk = v[k] as f64;
            s = ((f[q] + qf * qf) - (f[v[k]] + vk * vk)) / (2.0 * qf - 2.0 * vk);
            if s <= z[k] && k > 0 {
                k -= 1;
            } else {
                break;
            }
        }
        if s <= z[k] {
            // Only the first parabola is left and this one hides it.
            v[0] = q;
            z[1] = f64::INFINITY;
        } else {
            k += 1;
            v[k] = q;
            z[k] = s;
            z[k + 1] = f64::INFINITY;
        }
    }
    let mut k = 0usize;
    for q in 0..n {
        let qf = q as f64;
        while z[k + 1] < qf {
            k += 1;
        }
        let dv = qf - v[k] as f64;
        d[q] = dv * dv + f[v[k]];
    }
}

/// For a `w` × `h` coverage pixmap: each pixel's distance (pixels) from its centre to the edge of
/// the shape (pixels at least half covered), 0 outside.
pub fn inside_distance(cov: &Pixmap) -> Vec<f32> {
    let (w, h) = (cov.width() as usize, cov.height() as usize);
    const FAR: f64 = 1e20;
    let mut g: Vec<f64> = cov.data().chunks_exact(4).map(|p| if p.get(3).copied().unwrap_or(0) >= 128 { FAR } else { 0.0 }).collect();
    let n = w.max(h);
    let (mut f, mut d, mut v, mut z) = (vec![0f64; n], vec![0f64; n], vec![0usize; n], vec![0f64; n + 1]);
    for x in 0..w {
        for y in 0..h {
            if let (Some(fy), Some(gv)) = (f.get_mut(y), g.get(y * w + x)) {
                *fy = *gv;
            }
        }
        edt_1d(f.get(..h).unwrap_or(&[]), d.get_mut(..h).unwrap_or(&mut []), &mut v, &mut z);
        for y in 0..h {
            if let (Some(gv), Some(dy)) = (g.get_mut(y * w + x), d.get(y)) {
                *gv = *dy;
            }
        }
    }
    for y in 0..h {
        let row = y * w;
        if let (Some(src), Some(dst)) = (g.get(row..row + w), f.get_mut(..w)) {
            dst.copy_from_slice(src);
        }
        edt_1d(f.get(..w).unwrap_or(&[]), d.get_mut(..w).unwrap_or(&mut []), &mut v, &mut z);
        if let (Some(dst), Some(src)) = (g.get_mut(row..row + w), d.get(..w)) {
            dst.copy_from_slice(src);
        }
    }
    // Centre-to-centre distance to the nearest outside pixel → distance to the edge between.
    g.iter().map(|s| if *s > 0.0 { (s.sqrt() - 0.5).max(0.0) as f32 } else { 0.0 }).collect()
}

/// Running-sum box blur of a `w` × `h` field along its rows, radius `r`, zero outside.
fn box_pass(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    let norm = 1.0 / (2 * r + 1) as f32;
    for l in 0..h {
        let base = l * w;
        let at = |i: usize| src.get(base + i).copied().unwrap_or(0.0);
        let mut acc: f32 = (0..=r.min(w.saturating_sub(1))).map(at).sum();
        for i in 0..w {
            if let Some(o) = dst.get_mut(base + i) {
                *o = acc * norm;
            }
            if i + r + 1 < w {
                acc += at(i + r + 1);
            }
            if i >= r {
                acc -= at(i - r);
            }
        }
    }
}

/// [`box_pass`] down the columns: a running sum per column, row by row, so every read and write is
/// a row in memory (a pass down a column would take a cache miss per pixel).
fn box_pass_cols(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    let norm = 1.0 / (2 * r + 1) as f32;
    let mut acc = vec![0f32; w];
    for y in 0..=r.min(h.saturating_sub(1)) {
        let base = y * w;
        for x in 0..w {
            if let (Some(a), Some(s)) = (acc.get_mut(x), src.get(base + x)) {
                *a += *s;
            }
        }
    }
    for y in 0..h {
        let base = y * w;
        for x in 0..w {
            if let (Some(a), Some(d)) = (acc.get(x), dst.get_mut(base + x)) {
                *d = a * norm;
            }
        }
        if y + r + 1 < h {
            let add = (y + r + 1) * w;
            for x in 0..w {
                if let (Some(a), Some(s)) = (acc.get_mut(x), src.get(add + x)) {
                    *a += *s;
                }
            }
        }
        if y >= r {
            let sub = (y - r) * w;
            for x in 0..w {
                if let (Some(a), Some(s)) = (acc.get_mut(x), src.get(sub + x)) {
                    *a -= *s;
                }
            }
        }
    }
}

/// Unit inward normals of the shape from its inside `dist`ance field (`w` × `h`): the distance's
/// gradient, smoothed over `r` pixels so curved edges bend the backdrop smoothly instead of in the
/// steps of the pixel grid. Zero where there is no direction (outside, or a flat middle).
fn smooth_normals(dist: &[f32], w: usize, h: usize, r: usize) -> Vec<(f32, f32)> {
    let at = |x: usize, y: usize| dist.get(y * w + x).copied().unwrap_or(0.0);
    let mut gx = vec![0f32; w * h];
    let mut gy = vec![0f32; w * h];
    for y in 1..h.saturating_sub(1) {
        for x in 1..w.saturating_sub(1) {
            if let (Some(a), Some(b)) = (gx.get_mut(y * w + x), gy.get_mut(y * w + x)) {
                *a = (at(x + 1, y) - at(x - 1, y)) * 0.5;
                *b = (at(x, y + 1) - at(x, y - 1)) * 0.5;
            }
        }
    }
    let mut tmp = vec![0f32; w * h];
    for g in [&mut gx, &mut gy] {
        for _ in 0..2 {
            box_pass(g, &mut tmp, w, h, r);
            box_pass_cols(&tmp, g, w, h, r);
        }
    }
    gx.iter()
        .zip(&gy)
        .map(|(x, y)| {
            let len = (x * x + y * y).sqrt();
            if len > 1e-4 { (x / len, y / len) } else { (0.0, 0.0) }
        })
        .collect()
}

/// The part of a pane's pixels that depends only on its shape, its size and the zoom — not on
/// where it sits or what is behind it: its coverage's distance field and the normals smoothed from
/// it. A drag moves a pane without changing any of that, so every frame after the first finds them
/// (a render from scratch would work them out again).
struct Geometry {
    /// What it was worked out from, compared exactly on the way in.
    shape: Outline,
    key: Key,
    /// Which geometry this is, for an accelerator keeping it as textures.
    id: u64,
    /// The coverage's alpha, `cw` × `ch`.
    cw: usize,
    ch: usize,
    alpha: Vec<u8>,
    dist: Vec<f32>,
    normals: Vec<(f32, f32)>,
    /// What it takes, for the cache's budget.
    bytes: usize,
}

/// Kept geometry, least recently used first.
static GEOMETRY: OnceLock<Mutex<Vec<Arc<Geometry>>>> = OnceLock::new();

/// Keys worked out lately: a pane's geometry is kept once it is asked for again, so a resize or a
/// zoom (a new shape every frame) doesn't fill the cache with what is never reused.
static SEEN: OnceLock<Mutex<std::collections::VecDeque<Key>>> = OnceLock::new();

/// The most keys remembered (a few panes' bands, a few frames back).
const SEEN_MAX: usize = 64;

/// What a pane's geometry is keyed by: its coverage's size, the margin and smoothing, and the grid
/// it is rasterised on.
type Key = (u32, u32, u32, usize, [u64; 6]);

/// The most the kept geometry may take (bytes; an entry is about 12 bytes a pixel).
const GEOMETRY_BUDGET: usize = 64 << 20;

// How many geometries this thread worked out (tests: a moved pane works out none).
#[cfg(test)]
thread_local! {
    static COMPUTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn computed() -> usize {
    COMPUTED.with(|c| c.get())
}

/// The transform a pane's pixels are rasterised with, bit for bit: a render that cuts the pane
/// (a band, a patch) shifts it, while moving the pane leaves it alone.
fn grid(xf: Affine) -> [u64; 6] {
    xf.as_coeffs().map(f64::to_bits)
}

/// The geometry of a pane with `cov`erage (its shape through `xf`, grown by `m` each way, smoothed
/// over `r` pixels), from the cache when this pane was drawn before.
fn geometry(shape: &Outline, xf: Affine, m: u32, r: usize, cov: &Pixmap) -> Arc<Geometry> {
    let (cw, ch) = (cov.width(), cov.height());
    let key: Key = (cw, ch, m, r, grid(xf));
    let cache = GEOMETRY.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut kept) = cache.lock()
        && let Some(i) = kept.iter().position(|g| g.key == key && g.shape == *shape)
    {
        let g = kept.remove(i);
        kept.push(g.clone());
        return g;
    }
    let dist = inside_distance(cov);
    let normals = smooth_normals(&dist, cw as usize, ch as usize, r);
    #[cfg(test)]
    COMPUTED.with(|c| c.set(c.get() + 1));
    let alpha: Vec<u8> = cov.data().chunks_exact(4).map(|p| p.get(3).copied().unwrap_or(0)).collect();
    let bytes = alpha.len() + dist.len() * std::mem::size_of::<f32>() + normals.len() * std::mem::size_of::<(f32, f32)>();
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let g = Arc::new(Geometry { shape: shape.clone(), key, id, cw: cw as usize, ch: ch as usize, alpha, dist, normals, bytes });
    // Kept from the second sight on: a moving pane asks for its geometry every frame, while a
    // resize or a zoom never asks twice (keeping those would only fill memory).
    if seen(&key)
        && bytes <= GEOMETRY_BUDGET
        && let Ok(mut kept) = cache.lock()
    {
        let mut total: usize = kept.iter().map(|g| g.bytes).sum();
        while total.saturating_add(bytes) > GEOMETRY_BUDGET && !kept.is_empty() {
            total = total.saturating_sub(kept.remove(0).bytes);
        }
        kept.push(g.clone());
    }
    g
}

/// Whether `key` was worked out before (remembering it for the next time).
fn seen(key: &Key) -> bool {
    let seen = SEEN.get_or_init(|| Mutex::new(std::collections::VecDeque::new()));
    let Ok(mut seen) = seen.lock() else { return false };
    if seen.contains(key) {
        return true;
    }
    if seen.len() >= SEEN_MAX {
        seen.pop_front();
    }
    seen.push_back(*key);
    false
}

/// Empty the kept geometry (tests).
#[cfg(test)]
fn clear_geometry() {
    if let Some(cache) = GEOMETRY.get()
        && let Ok(mut kept) = cache.lock()
    {
        kept.clear();
    }
    if let Some(seen) = SEEN.get()
        && let Ok(mut seen) = seen.lock()
    {
        seen.clear();
    }
}

/// The kept geometry is one cache for the whole process: the tests that empty it take turns.
#[cfg(test)]
fn geometry_turn() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Bilinear sample of premultiplied `p` at pixel coordinates `(x, y)` (centres at +0.5), each
/// channel 0..=255; transparent outside.
#[inline]
fn sample(p: &Pixmap, x: f32, y: f32) -> [f32; 4] {
    let (w, h) = (p.width() as i64, p.height() as i64);
    let (x, y) = (x - 0.5, y - 0.5);
    let (x0, y0) = (x.floor(), y.floor());
    let (tx, ty) = (x - x0, y - y0);
    let (x0, y0) = (x0 as i64, y0 as i64);
    let data = p.data();
    // Wholly inside (the common case): two rows of two texels, read in one piece each.
    if x0 >= 0 && y0 >= 0 && x0 + 1 < w && y0 + 1 < h {
        let i = ((y0 * w + x0) * 4) as usize;
        let j = i + w as usize * 4;
        if let (Some(r0), Some(r1)) = (data.get(i..i + 8), data.get(j..j + 8)) {
            let lerp = |a: u8, b: u8, c: u8, d: u8| {
                let (a, b, c, d) = (f32::from(a), f32::from(b), f32::from(c), f32::from(d));
                let top = a + (b - a) * tx;
                let bot = c + (d - c) * tx;
                top + (bot - top) * ty
            };
            return [
                lerp(r0[0], r0[4], r1[0], r1[4]),
                lerp(r0[1], r0[5], r1[1], r1[5]),
                lerp(r0[2], r0[6], r1[2], r1[6]),
                lerp(r0[3], r0[7], r1[3], r1[7]),
            ];
        }
    }
    let px = |xi: i64, yi: i64| -> [f32; 4] {
        if xi < 0 || yi < 0 || xi >= w || yi >= h {
            return [0.0; 4];
        }
        let i = ((yi * w + xi) * 4) as usize;
        match data.get(i..i + 4) {
            Some(s) => [f32::from(s[0]), f32::from(s[1]), f32::from(s[2]), f32::from(s[3])],
            None => [0.0; 4],
        }
    };
    let (a, b, c, d) = (px(x0, y0), px(x0 + 1, y0), px(x0, y0 + 1), px(x0 + 1, y0 + 1));
    std::array::from_fn(|k| {
        let top = a[k] + (b[k] - a[k]) * tx;
        let bot = c[k] + (d[k] - c[k]) * tx;
        top + (bot - top) * ty
    })
}

/// The furthest in (pixels) the rim light is worth half a level or more: where
/// `255 × intensity × e^(−d / band)` falls to 0.5, at most [`MAX_LIGHT_PX`] (0 for no light).
fn light_reach(band: f64, intensity: f64) -> f64 {
    let k = 255.0 * intensity / 0.5;
    if k > 1.0 && band.is_finite() { (band * k.ln()).min(MAX_LIGHT_PX) } else { 0.0 }
}

/// The light reaches no further in than this many pixels (a wide light on a deep pane at a high
/// zoom would otherwise need a huge margin of coverage around every cut).
const MAX_LIGHT_PX: f64 = 512.0;

/// The glass of a `w` × `h` layer whose `shape` maps through `xf` (local → layer pixels), from
/// `back`, its backdrop padded by `pad` pixels each way, at `scale` device pixels per unit.
#[allow(clippy::too_many_arguments)]
pub fn glass(g: &Glass, shape: &Outline, back: &Pixmap, pad: u32, w: u32, h: u32, xf: Affine, scale: f64) -> Option<Pixmap> {
    let g = g.sanitized();
    let depth = (g.depth * scale).clamp(1.0, 1024.0);
    let band = (depth * (0.15 + 0.35 * g.splay)).max(1.0);
    let reach = light_reach(band, g.light_intensity);
    let r = (depth * 0.1).clamp(1.0, 8.0) as usize;
    // Coverage with a margin, so wherever the layer is cut every distance the pixels read is
    // exact: the bend's (within `depth`), the light's (within `reach`) and those the normals are
    // smoothed from (2r + 1 further).
    let m = depth.max(reach).ceil() as u32 + 4 * r as u32 + 4;
    let cov = spread_alpha(std::slice::from_ref(shape), 0.0, Affine::translate((f64::from(m), f64::from(m))) * xf, w + 2 * m, h + 2 * m)?;
    // The distance field and normals don't move with the pane: a drag finds the ones it worked out
    // the frame before (see [`geometry`]).
    let geom = geometry(shape, xf, m, r, &cov);
    let cw = cov.width() as usize;
    let mut frosted = back.clone();
    fx::blur(&mut frosted, g.frost / 2.0 * scale);
    let ang = g.light_angle.to_radians();
    let light = (ang.sin() as f32, -ang.cos() as f32);
    let bend_max = (g.refraction * depth) as f32;
    let fringe = (g.dispersion * 0.25) as f32;
    let power = (6.0 - 5.0 * g.splay) as f32;
    let (band, reach) = (band as f32, reach as f32);
    // The falloff, lowered by its value at `reach` and scaled back to 1 at the edge, so the light
    // fades out to 0 at `reach` instead of stopping there.
    let floor = (-reach / band).exp();
    let fall = 1.0 - floor;
    let intensity = g.light_intensity as f32;
    let depth = depth as f32;
    // The pixels themselves: an accelerator draws them where it can (the geometry is its own
    // textures, kept from frame to frame), and the CPU draws them otherwise.
    let paint = fx::GlassPaint {
        alpha: &geom.alpha,
        dist: &geom.dist,
        normals: &geom.normals,
        cw: geom.cw,
        ch: geom.ch,
        m,
        frosted: &frosted,
        w,
        h,
        pad,
        depth,
        bend_max,
        fringe,
        power,
        band,
        reach,
        floor,
        fall,
        intensity,
        light,
        geometry: geom.id,
    };
    if let Some(accelerator) = fx::accelerator()
        && let Some(out) = accelerator.glass(&paint)
    {
        return Some(out);
    }
    let mut out = Pixmap::new(w, h)?;
    let pane = Pane {
        cov: cov.data(),
        dist: &geom.dist,
        normals: &geom.normals,
        frosted: &frosted,
        w: w as usize,
        cw,
        m: m as usize,
        pad: pad as usize,
        depth,
        bend_max,
        fringe,
        power,
        band,
        reach,
        floor,
        fall,
        intensity,
        light,
    };
    let row = w as usize * 4;
    #[cfg(not(target_arch = "wasm32"))]
    {
        use rayon::prelude::*;
        // Rows are independent: one thread per band of rows, same pixels as one thread would draw.
        if (w as usize).saturating_mul(h as usize) >= fx::PARALLEL_PIXELS {
            out.data_mut().par_chunks_mut(row).enumerate().for_each(|(y, dst)| pane.row(y, dst));
        } else {
            for (y, dst) in out.data_mut().chunks_mut(row).enumerate() {
                pane.row(y, dst);
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    for (y, dst) in out.data_mut().chunks_mut(row).enumerate() {
        pane.row(y, dst);
    }
    Some(out)
}

/// Everything a pane's pixels are computed from; one row at a time, so rows can run side by side.
struct Pane<'a> {
    cov: &'a [u8],
    dist: &'a [f32],
    normals: &'a [(f32, f32)],
    frosted: &'a Pixmap,
    w: usize,
    cw: usize,
    m: usize,
    pad: usize,
    depth: f32,
    bend_max: f32,
    fringe: f32,
    power: f32,
    band: f32,
    reach: f32,
    floor: f32,
    fall: f32,
    intensity: f32,
    light: (f32, f32),
}

impl Pane<'_> {
    /// Row `y` of the pane's pixels (premultiplied RGBA bytes, `w` pixels).
    fn row(&self, y: usize, dst: &mut [u8]) {
        let Pane { cov, dist, normals, frosted, w, cw, m, pad, depth, bend_max, fringe, power, band, reach, floor, fall, intensity, light } = *self;
        // The three fields this row reads, as rows of their own: no per-pixel row arithmetic.
        let my = y + m;
        let (Some(cov_row), Some(dist_row), Some(norm_row)) =
            (cov.get(my * cw * 4..(my + 1) * cw * 4), dist.get(my * cw..(my + 1) * cw), normals.get(my * cw..(my + 1) * cw))
        else {
            return;
        };
        for x in 0..w {
            let i = x + m;
            let (Some(&c), Some(&d), Some(&inward)) = (cov_row.get(i * 4 + 3), dist_row.get(i), norm_row.get(i)) else { continue };
            let c = f32::from(c) / 255.0;
            if c <= 0.0 {
                continue;
            }
            let t = (d / depth).min(1.0);
            let bend = bend_max * (1.0 - t) * (1.0 - t);
            let (bx, by) = (x as f32 + pad as f32 + 0.5, y as f32 + pad as f32 + 0.5);
            let base = if bend == 0.0 {
                // Nothing bends (past `depth`, or no refraction): the three channels' samples are
                // at the same point, so one sample.
                let s = sample(frosted, bx, by);
                [s[0].min(s[3]), s[1].min(s[3]), s[2].min(s[3]), s[3]]
            } else {
                let at = |k: f32| sample(frosted, bx + inward.0 * bend * k, by + inward.1 * bend * k);
                let (rs, gs, bs) = (at(1.0 - fringe), at(1.0), at(1.0 + fringe));
                let a = rs[3].max(gs[3]).max(bs[3]);
                [rs[0].min(a), gs[1].min(a), bs[2].min(a), a]
            };
            // The rim catches the light where it faces it, and half as much opposite. It fades out
            // by `reach` and is none past it, wherever the layer is cut.
            let hl = if d < reach && fall > 0.0 {
                let facing = -(inward.0 * light.0 + inward.1 * light.1);
                let spec = facing.max(0.0).powf(power) + 0.5 * (-facing).max(0.0).powf(power);
                (intensity * spec * ((-d / band).exp() - floor) / fall).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let at = x * 4;
            if let Some(p) = dst.get_mut(at..at + 4) {
                for (o, v) in p.iter_mut().zip(base) {
                    *o = ((v * (1.0 - hl) + 255.0 * hl) * c).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_of_a_square() {
        let mut p = Pixmap::new(21, 21).unwrap();
        let r = tiny_skia::Rect::from_xywh(5.0, 5.0, 11.0, 11.0).unwrap();
        let paint = tiny_skia::Paint { shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::WHITE), ..Default::default() };
        p.fill_rect(r, &paint, tiny_skia::Transform::identity(), None);
        let d = inside_distance(&p);
        // Outside: 0. The centre pixel (10, 10) is 5.5 px from each edge.
        assert_eq!(d[0], 0.0);
        assert!((d[10 * 21 + 10] - 5.5).abs() < 1e-4, "{}", d[10 * 21 + 10]);
        // A pixel on the edge is half a pixel from it.
        assert!((d[10 * 21 + 5] - 0.5).abs() < 1e-4, "{}", d[10 * 21 + 5]);
    }

    #[test]
    fn light_reach_is_where_the_light_falls_under_half_a_level() {
        let r = light_reach(10.0, 1.0);
        assert!((255.0 * (-r / 10.0).exp() - 0.5).abs() < 1e-9, "{r}");
        // No light, too faint to show, or a band that isn't a number: it reaches nowhere.
        for (band, intensity) in [(10.0, 0.0), (10.0, 0.5 / 255.0), (f64::NAN, 1.0), (f64::INFINITY, 1.0)] {
            assert_eq!(light_reach(band, intensity), 0.0, "{band} {intensity}");
        }
        assert_eq!(light_reach(1e6, 1.0), MAX_LIGHT_PX);
    }

    /// A rounded pane's outline and its coverage (for the geometry cache).
    fn pane(w: u32, h: u32) -> (Outline, Pixmap) {
        let mut path = figcraft_geom::kurbo::BezPath::new();
        path.move_to((4.0, 4.0));
        path.line_to((f64::from(w) - 4.0, 4.0));
        path.line_to((f64::from(w) - 4.0, f64::from(h) - 4.0));
        path.line_to((4.0, f64::from(h) - 4.0));
        path.close_path();
        let mut cov = Pixmap::new(w, h).unwrap();
        let r = tiny_skia::Rect::from_xywh(4.0, 4.0, (w - 8) as f32, (h - 8) as f32).unwrap();
        let paint = tiny_skia::Paint { shader: tiny_skia::Shader::SolidColor(tiny_skia::Color::WHITE), ..Default::default() };
        cov.fill_rect(r, &paint, tiny_skia::Transform::identity(), None);
        ((path, tiny_skia::FillRule::Winding), cov)
    }

    #[test]
    fn the_geometry_of_a_pane_is_kept_between_frames() {
        let _turn = geometry_turn();
        clear_geometry();
        let (a, cov) = pane(100, 80);
        let (b, _) = pane(100, 80);
        let xf = Affine::scale(1.0);
        // The first sight is worked out, the second keeps it for the frames after.
        let first = geometry(&a, xf, 8, 2, &cov);
        let kept = geometry(&a, xf, 8, 2, &cov);
        assert!(!Arc::ptr_eq(&first, &kept), "the first sight isn't kept yet");
        assert!(Arc::ptr_eq(&kept, &geometry(&b, xf, 8, 2, &cov)), "the same pane finds its geometry");
        // Another shape, zoom, margin or rasterised offset is worked out anew.
        let mut other = pane(100, 80).0;
        other.0.line_to((90.0, 70.0));
        assert!(!Arc::ptr_eq(&kept, &geometry(&other, xf, 8, 2, &cov)), "another shape");
        assert!(!Arc::ptr_eq(&kept, &geometry(&a, Affine::scale(2.0), 8, 2, &cov)), "another zoom");
        assert!(!Arc::ptr_eq(&kept, &geometry(&a, Affine::translate((3.0, 5.0)) * xf, 8, 2, &cov)), "another offset");
        assert!(!Arc::ptr_eq(&kept, &geometry(&a, xf, 9, 2, &cov)), "another margin");
        assert!(!Arc::ptr_eq(&kept, &geometry(&a, xf, 8, 3, &cov)), "another smoothing");
    }

    #[test]
    fn a_dragged_pane_reuses_its_geometry_and_draws_what_a_fresh_one_does() {
        use crate::{Options, View, render_view};
        use figcraft_color::{Color, Paint};
        use figcraft_doc::{Document, Effect, EffectKind, NodeKind};

        let _turn = geometry_turn();
        let mut d = Document::new("glass");
        let p = d.first_page().unwrap();
        // Stripes under the pane, so where it sits shows in what it bends.
        for (i, hex) in ["#e03030", "#2040e0", "#20c060"].into_iter().enumerate() {
            let s = d.create(p, NodeKind::Rectangle, "Stripe", 8.0 + i as f64 * 20.0, 8.0, 20.0, 60.0).unwrap();
            d.get_mut(s).unwrap().fills = vec![Paint::solid(Color::from_hex(hex).unwrap())];
        }
        let pane = d.create(p, NodeKind::Rectangle, "Glass", 0.0, 0.0, 60.0, 40.0).unwrap();
        {
            let n = d.get_mut(pane).unwrap();
            n.transform = Affine::translate((20.0, 20.0));
            n.corners.radii = [8.0; 4];
            n.fills = vec![Paint::solid(Color { a: 0.2, ..Color::WHITE })];
            n.effects = vec![Effect { kind: EffectKind::Glass(Glass::default()), visible: true }];
        }
        let view = View { origin: (0.0, 0.0), scale: 1.0, width: 120, height: 100 };
        clear_geometry();
        let before = render_view(&d, p, &view, &Options::default()).unwrap();
        // The frame after works it out once more, and keeps it from then on.
        let _ = render_view(&d, p, &view, &Options::default()).unwrap();
        let settled = computed();
        assert!(settled > 0, "the first frames work the geometry out");
        // Seven pixels on: the pane's own geometry is the one kept from the frame before.
        d.get_mut(pane).unwrap().transform = Affine::translate((27.0, 20.0));
        let moved = render_view(&d, p, &view, &Options::default()).unwrap();
        assert_eq!(computed(), settled, "the moved pane reused its geometry");
        assert_ne!(before.data(), moved.data(), "the pane moved over its backdrop");
        // Emptied, the cache draws the same pixels as the kept geometry did.
        clear_geometry();
        let fresh = render_view(&d, p, &view, &Options::default()).unwrap();
        assert_eq!(moved.data(), fresh.data(), "kept geometry draws what a fresh one does");
    }
}
