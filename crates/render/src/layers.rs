//! Isolated layers (opacity, effects, blend modes) kept between canvas renders.
//!
//! A drag re-renders the damaged area from the document every frame: every layer in it is
//! rasterised again (fills, shadows, layer blurs…). This keeps the pixels of each isolated layer
//! on the page's device pixel grid, keyed by everything they depend on, so a frame with a few
//! moving layers composites the rest instead of drawing them again.
//!
//! Only layers that can't see the backdrop are kept (see [`cacheable`]): glass and background
//! blurs read what lies under them, and masked frames start as a copy of it.

use std::collections::hash_map::{DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use figcraft_doc::{Document, Node};
use figcraft_geom::Affine;
use tiny_skia::Pixmap;

use crate::draw::Ctx;

/// An isolated layer's pixels, rasterised on the page's device pixel grid.
pub(crate) struct Layer {
    pub pix: Pixmap,
    /// Device pixels (page coordinates × scale) of its top-left pixel.
    pub anchor: (i32, i32),
    /// Drop shadows composited under it (they are drawn onto the backdrop, not into the layer).
    pub shadows: Vec<ShadowPart>,
}

/// A drop shadow as the layer draws it: `pix` at the layer's top-left.
pub(crate) struct ShadowPart {
    pub pix: Pixmap,
    pub opacity: f32,
    pub blend: figcraft_color::BlendMode,
}

/// What a layer's pixels depend on (two hashes: a collision would draw the wrong pixels).
type Key = (u64, u64);

/// Layers bigger than this many device pixels aren't kept: they'd crowd out everything else.
pub(crate) const MAX_LAYER_PIXELS: f64 = 4_194_304.0;
/// Kept layers, all together (a browser tab gets less).
#[cfg(not(target_arch = "wasm32"))]
const BUDGET: usize = 128 << 20;
#[cfg(target_arch = "wasm32")]
const BUDGET: usize = 24 << 20;

struct Entry {
    layer: Arc<Layer>,
    bytes: usize,
    /// The [`State::tick`] of the last use: the smallest ones are dropped first.
    used: u64,
}

struct State {
    map: HashMap<Key, Entry>,
    bytes: usize,
    tick: u64,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State { map: HashMap::new(), bytes: 0, tick: 0 }))
}

/// Forget every kept layer (tests compare renders with and without the cache).
#[cfg(test)]
pub(crate) fn clear() {
    let mut state = state().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    state.map.clear();
    state.bytes = 0;
    let mut seen = seen().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    seen.set.clear();
    seen.order.clear();
}

/// The bytes the kept layers take (tests check that a drag doesn't fill the cache).
#[cfg(test)]
pub(crate) fn kept_bytes() -> usize {
    state().lock().unwrap_or_else(std::sync::PoisonError::into_inner).bytes
}

/// Whether node `n`'s layer can be kept: nothing in it may read the backdrop (glass, background
/// blur, masks — see the module docs), it must be on whole device pixels, and it must not be drawn
/// differently every frame (animated GIF frames, outlines).
pub(crate) fn cacheable(ctx: &Ctx, n: &Node) -> bool {
    ctx.cache.is_some() && !ctx.outline_mode && ctx.time == 0.0 && whole(ctx.page_at.0) && whole(ctx.page_at.1) && backdrop_free(ctx.doc, n, 0)
}

/// Whether `v` is a whole device pixel (the scale round trip leaves a hair either side).
fn whole(v: f64) -> bool {
    (v - v.round()).abs() < 1e-6
}

fn backdrop_free(doc: &Document, n: &Node, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    if n.is_mask || crate::backdrop::has_effects(n) {
        return false;
    }
    n.children.iter().all(|c| doc.node(*c).is_none_or(|cn| backdrop_free(doc, cn, depth + 1)))
}

/// The kept layer of `n` drawn through `page_xf` (local → page device pixels), if there is one.
pub(crate) fn get(key: Key) -> Option<Arc<Layer>> {
    let mut state = state().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    state.tick += 1;
    let tick = state.tick;
    let entry = state.map.get_mut(&key)?;
    entry.used = tick;
    Some(Arc::clone(&entry.layer))
}

/// Whether `key` was drawn before: a layer is only kept from its second sight, because a layer
/// that changes every frame (one being dragged) is never found again, and drawing the whole of it
/// to keep it would cost more than cutting it to what a render needs.
pub(crate) fn settled(key: Key) -> bool {
    let mut seen = seen().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !seen.set.insert(key) {
        return true;
    }
    seen.order.push_back(key);
    while seen.order.len() > SEEN_MAX {
        if let Some(old) = seen.order.pop_front() {
            seen.set.remove(&old);
        }
    }
    false
}

/// Keep `layer`, `n`'s pixels drawn through `page_xf` with its top-left at `anchor`.
pub(crate) fn put(key: Key, layer: Arc<Layer>) {
    let bytes = layer.pix.data().len() + layer.shadows.iter().map(|s| s.pix.data().len()).sum::<usize>();
    if bytes > BUDGET {
        return;
    }
    let mut state = state().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    state.tick += 1;
    let tick = state.tick;
    if let Some(old) = state.map.remove(&key) {
        state.bytes = state.bytes.saturating_sub(old.bytes);
    }
    while state.bytes + bytes > BUDGET {
        let Some(oldest) = state.map.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| *k) else { break };
        if let Some(old) = state.map.remove(&oldest) {
            state.bytes = state.bytes.saturating_sub(old.bytes);
        }
    }
    state.bytes += bytes;
    state.map.insert(key, Entry { layer, bytes, used: tick });
}

/// What [`settled`] remembers: the keys drawn lately, oldest out first.
struct Seen {
    set: std::collections::HashSet<Key>,
    order: std::collections::VecDeque<Key>,
}

fn seen() -> &'static Mutex<Seen> {
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(Seen { set: std::collections::HashSet::new(), order: std::collections::VecDeque::new() }))
}

/// Keys remembered as drawn (a page of a few hundred effect layers fits many times over).
const SEEN_MAX: usize = 2048;

pub(crate) fn key(ctx: &Ctx, n: &Node, page_xf: Affine) -> Key {
    let mut first = DefaultHasher::new();
    (ctx.cache, ctx.lod, figcraft_text::layout_generation()).hash(&mut first);
    // Images are decoded into the file's colour space: another profile draws them differently.
    ctx.doc.color_profile.hash(&mut first);
    subtree(ctx.doc, n, 0).hash(&mut first);
    let mut second = DefaultHasher::new();
    for v in page_xf.as_coeffs().map(f64::to_bits) {
        v.hash(&mut second);
    }
    (first.finish(), second.finish())
}

/// The identity of a node and everything under it: nodes are shared `Arc`s, so a changed node is a
/// new address, and a changed child shows here as a changed address too.
fn subtree(doc: &Document, n: &Node, depth: usize) -> u64 {
    let mut h = DefaultHasher::new();
    (n as *const Node as usize).hash(&mut h);
    if depth < 64 {
        for c in &n.children {
            if let Some(cn) = doc.node(*c) {
                subtree(doc, cn, depth + 1).hash(&mut h);
            }
        }
    }
    h.finish()
}
