//! Raster effects on premultiplied RGBA8 pixmaps: blur (3× box ≈ Gaussian), dilate/erode (spread),
//! alpha extraction and tinting.

use std::sync::{Arc, Mutex, OnceLock};

use tiny_skia::Pixmap;

/// What a glass pane's pixels are computed from, for [`Accelerator::glass`].
pub struct GlassPaint<'a> {
    /// The pane's coverage alpha (`cw` × `ch`, one byte a pixel).
    pub alpha: &'a [u8],
    /// Its distance to the outline, and the smoothed inward normals (same size).
    pub dist: &'a [f32],
    pub normals: &'a [(f32, f32)],
    pub cw: usize,
    pub ch: usize,
    /// The margin the coverage was grown by (`Geometry`'s `m`).
    pub m: u32,
    /// The backdrop around the pane, blurred (`pad` pixels more each way).
    pub frosted: &'a Pixmap,
    pub w: u32,
    pub h: u32,
    pub pad: u32,
    pub depth: f32,
    pub bend_max: f32,
    pub fringe: f32,
    pub power: f32,
    pub band: f32,
    pub reach: f32,
    pub floor: f32,
    pub fall: f32,
    pub intensity: f32,
    pub light: (f32, f32),
    /// Which geometry this is (the same id whenever the pane's shape, size and zoom are the same):
    /// an accelerator keeps it as textures and works it out once.
    pub geometry: u64,
}

/// A GPU (or other) accelerator for the raster effects that dominate a render. The CPU paths stay
/// (exports, tests and machines without a device): this only stands in for them where it can, and
/// every method says whether it did.
pub trait Accelerator: Send + Sync {
    /// Blur `p` in place with standard deviation `sigma` (pixels); false when it can't, and the
    /// CPU blurs it instead.
    fn blur(&self, p: &mut Pixmap, sigma: f64) -> bool;
    /// A glass pane's pixels; `None` when it can't, and the CPU paints them instead.
    fn glass(&self, paint: &GlassPaint<'_>) -> Option<Pixmap> {
        let _ = paint;
        None
    }
}

static ACCELERATOR: OnceLock<Mutex<Option<Arc<dyn Accelerator>>>> = OnceLock::new();

/// Install (or remove) the accelerator effects use (the canvas installs the window's GPU).
pub fn set_accelerator(accelerator: Option<Arc<dyn Accelerator>>) {
    if let Ok(mut slot) = ACCELERATOR.get_or_init(|| Mutex::new(None)).lock() {
        *slot = accelerator;
    }
}

pub(crate) fn accelerator() -> Option<Arc<dyn Accelerator>> {
    ACCELERATOR.get()?.lock().ok()?.clone()
}

/// Whether effects run on an accelerator (the canvas's GPU) rather than on the CPU.
pub(crate) fn accelerated() -> bool {
    ACCELERATOR.get().is_some_and(|slot| slot.lock().is_ok_and(|s| s.is_some()))
}

/// Box radii for three passes approximating a Gaussian of `sigma` (Kovesi).
fn box_sizes(sigma: f64) -> [usize; 3] {
    let n = 3.0;
    let w_ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut wl = w_ideal.floor() as i64;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wu = wl + 2;
    let m_ideal = (12.0 * sigma * sigma - n * (wl * wl) as f64 - 4.0 * n * wl as f64 - 3.0 * n) / (-4.0 * wl as f64 - 4.0);
    let m = m_ideal.round() as i64;
    let r = |i: i64| -> usize {
        let w = if i < m { wl } else { wu };
        ((w.max(1) - 1) / 2) as usize
    };
    [r(0), r(1), r(2)]
}

/// Below this many pixels a pass stays on one thread (starting threads costs more than it saves).
pub(crate) const PARALLEL_PIXELS: usize = 40_000;

/// Call `row(y, row_pixels)` for every row of `dst` (`w` × `h` premultiplied RGBA bytes), on
/// several threads where the pixmap is big enough for that to pay: rows are independent, so the
/// pixels come out exactly as one thread would draw them.
pub(crate) fn each_row(dst: &mut [u8], w: usize, h: usize, row: impl Fn(usize, &mut [u8]) + Sync + Send) {
    let row_len = w.saturating_mul(4);
    let rows = dst.len().checked_div(row_len).map_or(0, |n| h.min(n));
    let Some(len) = row_len.checked_mul(rows) else { return };
    let Some(dst) = dst.get_mut(..len) else { return };
    let parallel = len >= PARALLEL_PIXELS;
    #[cfg(not(target_arch = "wasm32"))]
    {
        use rayon::prelude::*;
        if parallel {
            dst.par_chunks_mut(row_len).enumerate().for_each(|(y, d)| row(y, d));
            return;
        }
    }
    #[cfg(target_arch = "wasm32")]
    let _ = parallel;
    for (y, d) in dst.chunks_mut(row_len).enumerate() {
        row(y, d);
    }
}

fn box_blur_h(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    if r == 0 {
        dst.copy_from_slice(src);
        return;
    }
    let div = (2 * r + 1) as u32;
    each_row(dst, w, h, |y, dst| {
        let base = y * w * 4;
        let mut acc = [0u32; 4];
        // Edge: transparent outside.
        for x in 0..=r.min(w.saturating_sub(1)) {
            for c in 0..4 {
                acc[c] += u32::from(src.get(base + x * 4 + c).copied().unwrap_or(0));
            }
        }
        for x in 0..w {
            for c in 0..4 {
                if let Some(d) = dst.get_mut(x * 4 + c) {
                    *d = (acc[c] / div) as u8;
                }
            }
            let add = x + r + 1;
            if add < w {
                for c in 0..4 {
                    acc[c] += u32::from(src.get(base + add * 4 + c).copied().unwrap_or(0));
                }
            }
            if x >= r {
                let sub = x - r;
                for c in 0..4 {
                    acc[c] = acc[c].saturating_sub(u32::from(src.get(base + sub * 4 + c).copied().unwrap_or(0)));
                }
            }
        }
    });
}

/// Box blur of `w` × `h` down the columns: a running sum per column, row by row, so every read and
/// write is a row in memory (a pass down a column would take a cache miss per pixel).
fn box_blur_v(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    if r == 0 {
        dst.copy_from_slice(src);
        return;
    }
    let div = (2 * r + 1) as u32;
    let mut acc = vec![[0u32; 4]; w];
    let base = |y: usize| y * w * 4;
    let add_row = |acc: &mut [[u32; 4]], y: usize| {
        let b = base(y);
        for x in 0..w {
            for c in 0..4 {
                if let Some(a) = acc.get_mut(x) {
                    a[c] += u32::from(src.get(b + x * 4 + c).copied().unwrap_or(0));
                }
            }
        }
    };
    for y in 0..=r.min(h.saturating_sub(1)) {
        add_row(&mut acc, y);
    }
    for y in 0..h {
        let b = base(y);
        for x in 0..w {
            for c in 0..4 {
                if let (Some(a), Some(d)) = (acc.get(x), dst.get_mut(b + x * 4 + c)) {
                    *d = (a[c] / div) as u8;
                }
            }
        }
        if y + r + 1 < h {
            add_row(&mut acc, y + r + 1);
        }
        if y >= r {
            let sub = base(y - r);
            for x in 0..w {
                for c in 0..4 {
                    if let Some(a) = acc.get_mut(x) {
                        a[c] = a[c].saturating_sub(u32::from(src.get(sub + x * 4 + c).copied().unwrap_or(0)));
                    }
                }
            }
        }
    }
}

/// The largest blur standard deviation drawn (pixels): far wider than any layer (at most 16,384
/// pixels a side), so wider blurs look the same (everything averages out to nothing). The box
/// passes cost the same at any radius, so the documented blur radii (up to 10,000 units) draw
/// in full at any zoom.
pub const MAX_SIGMA: f64 = 1_000_000.0;

/// The three box radii a blur of `sigma` uses (an accelerator blurs with the same ones).
pub fn box_radii(sigma: f64) -> [usize; 3] {
    box_sizes(if sigma.is_finite() { sigma.min(MAX_SIGMA) } else { MAX_SIGMA })
}

/// Gaussian-like blur of the whole pixmap with standard deviation `sigma` (pixels).
pub fn blur(p: &mut Pixmap, sigma: f64) {
    if !(sigma.is_finite() && sigma > 0.3) {
        return;
    }
    let sigma = sigma.min(MAX_SIGMA);
    if let Some(accelerator) = accelerator()
        && accelerator.blur(p, sigma)
    {
        return;
    }
    let (w, h) = (p.width() as usize, p.height() as usize);
    let radii = box_sizes(sigma);
    let mut a = p.data().to_vec();
    let mut b = vec![0u8; a.len()];
    for r in radii {
        box_blur_h(&a, &mut b, w, h, r);
        std::mem::swap(&mut a, &mut b);
    }
    for r in radii {
        box_blur_v(&a, &mut b, w, h, r);
        std::mem::swap(&mut a, &mut b);
    }
    p.data_mut().copy_from_slice(&a);
}

/// Grow (`r > 0`) or shrink (`r < 0`) the alpha of a pixmap by `r` pixels (square structuring
/// element, separable max/min). Colour channels follow alpha (tinted afterwards anyway).
pub fn morph(p: &mut Pixmap, r: i32) {
    if r == 0 {
        return;
    }
    let grow = r > 0;
    let r = r.unsigned_abs().min(400) as usize;
    let (w, h) = (p.width() as usize, p.height() as usize);
    let alpha: Vec<u8> = p.data().as_chunks::<4>().0.iter().map(|c| c.get(3).copied().unwrap_or(0)).collect();
    let pass = |src: &[u8], w: usize, h: usize, horizontal: bool| -> Vec<u8> {
        let mut out = vec![0u8; src.len()];
        for y in 0..h {
            for x in 0..w {
                let mut v: u8 = if grow { 0 } else { 255 };
                let (lo, hi) = (x.saturating_sub(r), (x + r).min(w.saturating_sub(1)));
                for k in lo..=hi {
                    let idx = if horizontal { y * w + k } else { k * h + y };
                    let s = src.get(idx).copied().unwrap_or(0);
                    v = if grow { v.max(s) } else { v.min(s) };
                }
                // Shrinking treats outside as empty.
                if !grow && (x < r || x + r >= w) {
                    v = 0;
                }
                let o = if horizontal { y * w + x } else { x * h + y };
                if let Some(d) = out.get_mut(o) {
                    *d = v;
                }
            }
        }
        out
    };
    let a1 = pass(&alpha, w, h, true);
    // Vertical pass: treat columns as rows by swapping roles.
    let mut a2 = vec![0u8; a1.len()];
    for x in 0..w {
        for y in 0..h {
            let mut v: u8 = if grow { 0 } else { 255 };
            let (lo, hi) = (y.saturating_sub(r), (y + r).min(h.saturating_sub(1)));
            for k in lo..=hi {
                let s = a1.get(k * w + x).copied().unwrap_or(0);
                v = if grow { v.max(s) } else { v.min(s) };
            }
            if !grow && (y < r || y + r >= h) {
                v = 0;
            }
            if let Some(d) = a2.get_mut(y * w + x) {
                *d = v;
            }
        }
    }
    for (px, a) in p.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(a2) {
        px.copy_from_slice(&[a, a, a, a]);
    }
}

/// Replace every pixel with `color` (non-premultiplied RGBA 0..1) at the pixel's alpha × color alpha.
pub fn tint(p: &mut Pixmap, color: [f32; 4]) {
    let [r, g, b, a] = color;
    for px in p.data_mut().as_chunks_mut::<4>().0 {
        let alpha = f32::from(px.get(3).copied().unwrap_or(0)) / 255.0 * a;
        let q = |c: f32| (c * alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        px.copy_from_slice(&[q(r), q(g), q(b), (alpha * 255.0).round().clamp(0.0, 255.0) as u8]);
    }
}

/// Invert the alpha (premultiplied white with alpha 1-a): used for inner shadows.
pub fn invert_alpha(p: &mut Pixmap) {
    for px in p.data_mut().as_chunks_mut::<4>().0 {
        let a = 255 - px.get(3).copied().unwrap_or(0);
        px.copy_from_slice(&[a, a, a, a]);
    }
}

/// Multiply every pixel of `p` by the alpha of `m` (same size); `invert` uses 1-alpha.
pub fn mask_by(p: &mut Pixmap, m: &Pixmap, invert: bool) {
    for (px, mp) in p.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(m.data().chunks_exact(4)) {
        let ma = u32::from(mp.get(3).copied().unwrap_or(0));
        let ma = if invert { 255 - ma } else { ma };
        for c in px.iter_mut() {
            *c = ((u32::from(*c) * ma + 127) / 255) as u8;
        }
    }
}

/// Call `f(dst_row, src_row, row, first_col)` for each row where `src`, with its top-left at `at`
/// on `dst`, overlaps it (premultiplied RGBA bytes of the overlapping columns; `row` and
/// `first_col` in `src`'s pixels). Nothing past either edge is visited.
pub(crate) fn overlap_rows(dst: &mut Pixmap, at: (i32, i32), src: &Pixmap, mut f: impl FnMut(&mut [u8], &[u8], i64, i64)) {
    let (dw, dh) = (i64::from(dst.width()), i64::from(dst.height()));
    let (sw, sh) = (i64::from(src.width()), i64::from(src.height()));
    let (ax, ay) = (i64::from(at.0), i64::from(at.1));
    let (c0, c1) = ((-ax).max(0), sw.min(dw - ax));
    let (r0, r1) = ((-ay).max(0), sh.min(dh - ay));
    if c1 <= c0 || r1 <= r0 {
        return;
    }
    let len = (c1 - c0) as usize * 4;
    let s = src.data();
    let d = dst.data_mut();
    for r in r0..r1 {
        let (si, di) = ((r * sw + c0) as usize * 4, ((r + ay) * dw + ax + c0) as usize * 4);
        if let (Some(sp), Some(dp)) = (s.get(si..si + len), d.get_mut(di..di + len)) {
            f(dp, sp, r, c0);
        }
    }
}

/// Composite `src` at `opacity` onto `dst` (top-left at `at`) with plus-darker (linear burn:
/// `max(0, backdrop + source - 1)`, through the W3C general compositing formula), which
/// tiny-skia lacks.
pub fn plus_darker(dst: &mut Pixmap, at: (i32, i32), src: &Pixmap, opacity: f32) {
    let o = if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 0.0 };
    overlap_rows(dst, at, src, |dp, sp, _, _| {
        for (d, s) in dp.chunks_exact_mut(4).zip(sp.chunks_exact(4)) {
            let f = |v: u8| f32::from(v) / 255.0;
            let sa = f(s[3]) * o;
            if sa <= 0.0 {
                continue;
            }
            let da = f(d[3]);
            for c in 0..3 {
                let (sc, dc) = (f(s[c]) * o, f(d[c]));
                let v = sc * (1.0 - da) + dc * (1.0 - sa) + (sa * dc + da * sc - sa * da).max(0.0);
                d[c] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
            d[3] = ((sa + da - sa * da).clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_spreads_and_conserves() {
        let mut p = Pixmap::new(64, 64).unwrap();
        let i = (32 * 64 + 32) * 4;
        p.data_mut()[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
        let before: u32 = p.data().iter().map(|v| u32::from(*v)).sum();
        blur(&mut p, 3.0);
        let after: u32 = p.data().iter().map(|v| u32::from(*v)).sum();
        assert!(p.data()[i + 3] < 255);
        // Integer rounding loses a little.
        assert!(after <= before && after > 0, "{before} {after}");
    }

    #[test]
    fn morph_grows() {
        let mut p = Pixmap::new(20, 20).unwrap();
        let i = (10 * 20 + 10) * 4;
        p.data_mut()[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
        morph(&mut p, 2);
        let count = p.data().as_chunks::<4>().0.iter().filter(|c| c[3] == 255).count();
        assert_eq!(count, 25);
        morph(&mut p, -2);
        let count = p.data().as_chunks::<4>().0.iter().filter(|c| c[3] == 255).count();
        assert_eq!(count, 1);
    }

    #[test]
    fn box_sizes_sane() {
        for s in [0.5, 1.0, 4.0, 50.0] {
            let b = box_sizes(s);
            assert!(b.iter().all(|r| *r < 1000));
        }
    }
}
