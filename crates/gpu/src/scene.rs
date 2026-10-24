//! Document → vello scene: what the GPU canvas draws (see `docs/gpu-rasterizer.md`).
//!
//! Geometry is the renderer's: [`figcraft_render::node_outline`] builds a node's outline (shapes,
//! vectors, booleans and text as glyph outlines), so both rasterizers draw the same paths. This
//! module only turns outlines and paints into vello commands, and refuses — with a reason — what
//! it cannot draw exactly yet, which makes the canvas fall back to the CPU for that frame.

use figcraft_color::{BlendMode, Color, Gradient, GradientKind, Paint, PaintKind};
use figcraft_doc::{Document, Node, NodeId, NodeKind};
use figcraft_geom::Affine;
use figcraft_render::{FillRule, View, node_outline};
use vello::Scene;
use vello::kurbo::{Affine as KAffine, Point, Rect as KRect, Stroke};
use vello::peniko::color::DynamicColor;
use vello::peniko::{
    BlendMode as PBlend, Brush, Color as PColor, ColorStop, ColorStops, Compose, Extend, Fill, Gradient as PGradient, ImageBrush, ImageData,
    ImageQuality, ImageSampler, Mix,
};

/// Why a page can't be drawn on the GPU yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsupported {
    /// A visible effect (shadow, blur, glass, grain): drawn by GPU passes in a later phase.
    Effect,
    /// A paint the scene builder doesn't draw yet (image, pattern, diamond gradient).
    Paint,
    /// A stroke shape it doesn't draw yet (inside/outside, dashes, variable width, brushes, arrows).
    Stroke,
    /// A mask, or a text run with its own fills or colour glyphs.
    Mask,
    /// Text whose glyphs can't be reduced to one outline.
    Text,
    /// Clipping, opacity or blend in a place the builder doesn't handle.
    Structure,
}

impl Unsupported {
    /// A short reason for logs.
    pub fn reason(self) -> &'static str {
        match self {
            Unsupported::Effect => "an effect",
            Unsupported::Paint => "a paint",
            Unsupported::Stroke => "a stroke",
            Unsupported::Mask => "a mask or styled text",
            Unsupported::Text => "text",
            Unsupported::Structure => "a structure",
        }
    }
}

/// Deepest nesting drawn (the renderer's export cap).
const MAX_DEPTH: usize = 64;

/// What a node's backdrop effect is, by its id (the canvas makes its pixels from the accumulated
/// page).
pub fn backdrop_of_node(doc: &Document, id: NodeId) -> Option<Backdrop> {
    backdrop_of(doc.node(id)?)
}

/// A background-blurred node's layer: the device rectangle of its backdrop (the layer grown by
/// `pad` each way) — what the CPU blurs, and therefore the region the canvas crops and blurs.
pub fn blur_rect(doc: &Document, id: NodeId, view: &View) -> Option<((f64, f64), (u32, u32))> {
    let n = doc.node(id)?;
    let (x0, y0, w, h) = layer_rect_impl(doc, n, view).ok()??;
    let pad = f64::from(figcraft_render::backdrop::pad(n, view.scale));
    let w = (f64::from(w) + 2.0 * pad).clamp(1.0, 16_384.0) as u32;
    let h = (f64::from(h) + 2.0 * pad).clamp(1.0, 16_384.0) as u32;
    Some(((x0 - pad, y0 - pad), (w, h)))
}

/// A glass node's pane geometry and its layer's device position, for the canvas's crop.
pub fn glass_setup(doc: &Document, id: NodeId, view: &View) -> Option<(figcraft_render::glass::GlassPane, (i32, i32))> {
    let n = doc.node(id)?;
    let Backdrop::Glass(g) = backdrop_of(n)? else { return None };
    let (x0, y0, w, h) = layer_rect_impl(doc, n, view).ok()??;
    let parent = n.parent.map(|p| doc.world_transform(p)).unwrap_or(Affine::IDENTITY);
    let xf = view.transform() * parent * n.transform;
    let shape = figcraft_render::backdrop::outline(doc, n)?;
    let layer_xf = Affine::translate((-x0, -y0)) * xf;
    let mut pane = figcraft_render::glass::pane(&g, &shape, w, h, layer_xf, view.scale)?;
    pane.pad = figcraft_render::backdrop::pad(n, view.scale);
    Some((pane, (x0 as i32, y0 as i32)))
}

/// The device rectangle of `n`'s effect layer, in whole pixels (`None` past the layer cap).
fn layer_rect_impl(doc: &Document, n: &Node, view: &View) -> Result<Option<(f64, f64, u32, u32)>, Unsupported> {
    let Some(bounds) = figcraft_render::paint_bounds(doc, n.id) else { return Ok(None) };
    let (x0f, y0f) = ((bounds.x0 - view.origin.0) * view.scale, (bounds.y0 - view.origin.1) * view.scale);
    let (x1f, y1f) = ((bounds.x1 - view.origin.0) * view.scale, (bounds.y1 - view.origin.1) * view.scale);
    if !(x0f.is_finite() && y0f.is_finite() && x1f.is_finite() && y1f.is_finite()) {
        return Err(Unsupported::Structure);
    }
    let (x0, y0) = (x0f.floor(), y0f.floor());
    let (w, h) = ((x1f.ceil() - x0).max(1.0), (y1f.ceil() - y0).max(1.0));
    const MAX_LAYER: f64 = 8_192.0;
    if w > MAX_LAYER || h > MAX_LAYER {
        return Err(Unsupported::Effect);
    }
    Ok(Some((x0, y0, w as u32, h as u32)))
}

/// The page's background colour, drawn under the scene.
pub fn background(doc: &Document, page: NodeId) -> Color {
    match doc.node(page).map(|p| &p.kind) {
        Some(NodeKind::Page { background, .. }) => *background,
        _ => Color::DEFAULT_PAGE,
    }
}

/// What a node's backdrop effect is (one glass pane, or one uniform background blur).
pub enum Backdrop {
    Blur(f64),
    Glass(figcraft_doc::Glass),
}

fn backdrop_of(n: &Node) -> Option<Backdrop> {
    let mut visible = n.effects.iter().filter(|e| e.visible);
    let e = visible.next()?;
    if visible.next().is_some() {
        return None;
    }
    match &e.kind {
        figcraft_doc::EffectKind::BackgroundBlur(b) if !b.is_progressive() && b.max_radius() > 0.0 => Some(Backdrop::Blur(b.sanitized().radius)),
        figcraft_doc::EffectKind::Glass(g) => Some(Backdrop::Glass(g.clone())),
        _ => None,
    }
}

/// One step of a page drawn in segments: ordinary layers, or a node that reads the page under it.
pub enum Step {
    Nodes(Vec<NodeId>),
    /// A glass pane or a background-blurred layer.
    Backdrop(NodeId),
}

/// How to draw `page` in segments: its top-level children in order, with backdrop nodes as steps
/// of their own (the page is accumulated up to each one). `Err` when a backdrop node is nested —
/// there is no segment boundary inside a layer, so such a page stays on the CPU.
pub fn plan(doc: &Document, page: NodeId) -> Result<Vec<Step>, Unsupported> {
    let Some(p) = doc.node(page) else { return Ok(Vec::new()) };
    let mut steps = Vec::new();
    let mut run: Vec<NodeId> = Vec::new();
    for c in &p.children {
        let Some(n) = doc.node(*c) else { continue };
        if !n.visible || n.opacity <= 0.0 {
            continue;
        }
        if backdrop_of(n).is_some() {
            if !run.is_empty() {
                steps.push(Step::Nodes(std::mem::take(&mut run)));
            }
            steps.push(Step::Backdrop(*c));
        } else {
            if subtree_backdrop(doc, *c, 0) {
                return Err(Unsupported::Effect);
            }
            run.push(*c);
        }
    }
    if !run.is_empty() {
        steps.push(Step::Nodes(run));
    }
    Ok(steps)
}

/// Whether `id` or anything in it reads its backdrop.
fn subtree_backdrop(doc: &Document, id: NodeId, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return true;
    }
    let Some(n) = doc.node(id) else { return false };
    if backdrop_of(n).is_some() {
        return true;
    }
    n.children.iter().any(|c| subtree_backdrop(doc, *c, depth + 1))
}

/// Draw `ids` (children of `page`, in order) into `out`.
pub fn draw_into(doc: &Document, page: NodeId, view: &View, layers: &mut dyn Layers, ids: &[NodeId], out: &mut Scene) -> Result<(), Unsupported> {
    let mut b = Builder { doc, scene: Some(Scene::new()), layers, view: view.clone(), page, hidden: Vec::new(), no_strokes: Vec::new() };
    for id in ids {
        b.node(*id, view.transform(), 0)?;
    }
    if let Some(s) = b.scene.take() {
        out.append(&s, None);
    }
    Ok(())
}

/// Draw a backdrop node over its effect pixels (`image`, made from the page drawn under it).
pub fn draw_backdrop_into(
    doc: &Document,
    page: NodeId,
    view: &View,
    layers: &mut dyn Layers,
    id: NodeId,
    image: &ImageData,
    out: &mut Scene,
) -> Result<(), Unsupported> {
    let Some(n) = doc.node(id) else { return Ok(()) };
    if !n.visible || n.opacity <= 0.0 {
        return Ok(());
    }
    if n.is_mask {
        return Err(Unsupported::Mask);
    }
    let Some(effect) = backdrop_of(n) else { return Err(Unsupported::Effect) };
    let mut b = Builder { doc, scene: Some(Scene::new()), layers, view: view.clone(), page, hidden: Vec::new(), no_strokes: Vec::new() };
    // A blur's pixels are its padded layer (a rectangle, so the shape clips them); a pane's pixels
    // are its layer, with the shape's own coverage in the alpha.
    let place = match effect {
        Backdrop::Blur(_) => {
            let ((x, y), _) = blur_rect(doc, id, view).ok_or(Unsupported::Structure)?;
            ((x, y), true)
        }
        Backdrop::Glass(_) => {
            let (x0, y0, _, _) = b.layer_rect(n)?.ok_or(Unsupported::Structure)?;
            ((x0, y0), false)
        }
    };
    b.draw_backdrop_image(n, 0, image, place)?;
    if let Some(s) = b.scene.take() {
        out.append(&s, None);
    }
    Ok(())
}

/// Where a node's effect layer goes: the GPU renders it offscreen and blurs it without leaving the
/// device (see `crate::canvas::GpuLayers`). [`supported`] accepts such layers without drawing them.
pub trait Layers {
    /// A blurred `width` × `height` layer of `scene` (device pixels, transparent ground).
    fn blurred(&mut self, scene: &Scene, width: u32, height: u32, sigma: f64) -> Option<ImageData>;
    /// What lies under a node — a whole page drawn into a `width` × `height` texture — blurred.
    fn backdrop(&mut self, scene: &Scene, width: u32, height: u32, sigma: f64) -> Option<ImageData>;
    /// A glass pane's pixels from `scene` (the page under the node); its layer's top-left is `at`
    /// in the `view`-sized scene, and `pane.pad` around it is the backdrop it reads.
    fn glass(&mut self, scene: &Scene, view: (u32, u32), pane: &figcraft_render::glass::GlassPane, at: (i32, i32)) -> Option<ImageData>;
    /// A backdrop node's effect pixels from `back`, the accumulated page (`view`-sized): the whole
    /// view blurred, or a glass pane's layer computed from a crop of it.
    fn backdrop_effect(&mut self, doc: &Document, id: NodeId, view: &View, back: &wgpu::Texture) -> Option<ImageData>;
}

/// Accepts effect layers without drawing them (the feature check).
struct Probe;

impl Layers for Probe {
    fn blurred(&mut self, _scene: &Scene, width: u32, height: u32, _sigma: f64) -> Option<ImageData> {
        Some(crate::canvas::blank_image(width, height))
    }
    fn backdrop(&mut self, scene: &Scene, width: u32, height: u32, sigma: f64) -> Option<ImageData> {
        self.blurred(scene, width, height, sigma)
    }
    fn glass(&mut self, _scene: &Scene, _view: (u32, u32), pane: &figcraft_render::glass::GlassPane, _at: (i32, i32)) -> Option<ImageData> {
        Some(crate::canvas::blank_image(pane.w, pane.h))
    }
    fn backdrop_effect(&mut self, _doc: &Document, _id: NodeId, view: &View, _back: &wgpu::Texture) -> Option<ImageData> {
        Some(crate::canvas::blank_image(view.width, view.height))
    }
}

/// Refuses effect layers: a build without a device.
pub struct NoLayers;

impl Layers for NoLayers {
    fn blurred(&mut self, _scene: &Scene, _width: u32, _height: u32, _sigma: f64) -> Option<ImageData> {
        None
    }
    fn backdrop(&mut self, _scene: &Scene, _width: u32, _height: u32, _sigma: f64) -> Option<ImageData> {
        None
    }
    fn glass(&mut self, _scene: &Scene, _view: (u32, u32), _pane: &figcraft_render::glass::GlassPane, _at: (i32, i32)) -> Option<ImageData> {
        None
    }
    fn backdrop_effect(&mut self, _doc: &Document, _id: NodeId, _view: &View, _back: &wgpu::Texture) -> Option<ImageData> {
        None
    }
}

/// Whether `page` can be drawn on the GPU at all: `Ok` when every visible layer is in the
/// builder's feature set, else the first reason it isn't.
pub fn supported(doc: &Document, page: NodeId) -> Result<(), Unsupported> {
    let mut probe = Probe;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: 1, height: 1 };
    let mut b = Builder { doc, scene: None, layers: &mut probe, view, page, hidden: Vec::new(), no_strokes: Vec::new() };
    b.children(page, Affine::IDENTITY, 0)
}

/// Draw page `page` of `doc` as seen through `view` into a vello scene (no effect layers).
pub fn build(doc: &Document, page: NodeId, view: &View) -> Result<Scene, Unsupported> {
    build_with(doc, page, view, &mut NoLayers)
}

/// Draw page `page` of `doc` as seen through `view` into a vello scene, drawing effect layers
/// through `layers` (the GPU canvas).
pub fn build_with(doc: &Document, page: NodeId, view: &View, layers: &mut dyn Layers) -> Result<Scene, Unsupported> {
    let mut b = Builder { doc, scene: Some(Scene::new()), layers, view: view.clone(), page, hidden: Vec::new(), no_strokes: Vec::new() };
    let xf = view.transform();
    let Some(p) = doc.node(page) else { return Ok(Scene::new()) };
    for c in &p.children {
        b.node(*c, xf, 0)?;
    }
    Ok(b.scene.unwrap_or_default())
}

struct Builder<'a> {
    doc: &'a Document,
    /// `None` while only checking whether the page is supported.
    scene: Option<Scene>,
    layers: &'a mut dyn Layers,
    view: View,
    /// The page drawn (backdrop effects render what lies under a node).
    page: NodeId,
    /// Layers left out (painted after a backdrop node: see [`figcraft_render::backdrop::above`]).
    hidden: Vec<NodeId>,
    /// Layers whose strokes are left out (they are painted after their children's).
    no_strokes: Vec<NodeId>,
}

/// What a node's visible effects need of the GPU, if anything.
enum Effects {
    None,
    /// One layer blur: its radius in document units.
    LayerBlur(f64),
    /// One background blur (uniform): its radius in document units.
    BackgroundBlur(f64),
    /// One glass pane.
    Glass(figcraft_doc::Glass),
    /// Drop shadows a rounded, opaque rectangle can cast straight into the scene (their shape is
    /// the rectangle's own outline, so no layer is needed).
    Shadows(Vec<figcraft_doc::Shadow>),
    /// Anything else (glass, grain, inner shadows, several kinds of effect).
    Other,
}

/// A shadow drawn without a layer: vello's blurred rounded rect, cut off where an opaque card
/// covers it (as the CPU masks it).
fn simple_shadows(n: &Node) -> Option<Vec<figcraft_doc::Shadow>> {
    if !matches!(n.kind, NodeKind::Rectangle) || !n.children.is_empty() {
        return None;
    }
    if n.corners.uniform().is_none() || n.corners.smoothing != 0.0 {
        return None;
    }
    if n.opacity < 1.0 || !matches!(n.blend, BlendMode::PassThrough | BlendMode::Normal) || n.has_visible_stroke() {
        return None;
    }
    // The card must cover the shadow where they overlap, or the shadow would show through it.
    let [paint] = n.fills.as_slice() else { return None };
    if paint.blend != BlendMode::Normal || paint.opacity < 1.0 {
        return None;
    }
    let color = paint.solid_color()?;
    if color.a < 1.0 {
        return None;
    }
    let mut shadows = Vec::new();
    for e in n.effects.iter().filter(|e| e.visible) {
        let figcraft_doc::EffectKind::DropShadow(s) = &e.kind else { return None };
        if s.spread != 0.0 || s.show_behind || s.blend != BlendMode::Normal {
            return None;
        }
        shadows.push(s.clone());
    }
    (!shadows.is_empty()).then_some(shadows)
}

fn effects_of(n: &Node) -> Effects {
    if let Some(b) = backdrop_of(n) {
        return match b {
            Backdrop::Blur(radius) => Effects::BackgroundBlur(radius),
            Backdrop::Glass(g) => Effects::Glass(g),
        };
    }
    let mut visible = n.effects.iter().filter(|e| e.visible);
    let Some(e) = visible.next() else { return Effects::None };
    let single = visible.next().is_none();
    if single
        && let figcraft_doc::EffectKind::LayerBlur(b) = &e.kind
        && !b.is_progressive()
    {
        return Effects::LayerBlur(b.sanitized().radius);
    }
    match simple_shadows(n) {
        Some(shadows) => Effects::Shadows(shadows),
        None => Effects::Other,
    }
}

impl Builder<'_> {
    fn children(&mut self, id: NodeId, xf: Affine, depth: usize) -> Result<(), Unsupported> {
        let Some(n) = self.doc.node(id) else { return Ok(()) };
        for c in &n.children {
            self.node(*c, xf, depth)?;
        }
        Ok(())
    }

    fn node(&mut self, id: NodeId, parent_xf: Affine, depth: usize) -> Result<(), Unsupported> {
        if depth > MAX_DEPTH {
            return Err(Unsupported::Structure);
        }
        if self.hidden.contains(&id) {
            return Ok(());
        }
        let Some(n) = self.doc.node(id) else { return Ok(()) };
        if !n.visible || n.opacity <= 0.0 {
            return Ok(());
        }
        if n.is_mask {
            return Err(Unsupported::Mask);
        }
        match effects_of(n) {
            Effects::None => {}
            Effects::LayerBlur(radius) => return self.blurred_layer(n, parent_xf, depth, radius),
            Effects::BackgroundBlur(radius) => return self.backdrop_layer(n, depth, radius),
            Effects::Glass(glass) => return self.glass_layer(n, depth, &glass),
            Effects::Shadows(shadows) => {
                let xf = parent_xf * n.transform;
                self.draw_shadows(n, &shadows, xf);
            }
            Effects::Other => return Err(Unsupported::Effect),
        }
        // vello has no plus-darker composition: the CPU draws it.
        if n.blend == BlendMode::PlusDarker {
            return Err(Unsupported::Structure);
        }
        let xf = parent_xf * n.transform;
        let isolated = n.opacity < 1.0 || !matches!(n.blend, BlendMode::PassThrough | BlendMode::Normal);
        if isolated {
            self.push_layer(n);
        }
        self.content(n, xf, depth)?;
        if isolated && let Some(s) = &mut self.scene {
            s.pop_layer();
        }
        Ok(())
    }

    /// Drop shadows drawn without a layer: vello's blurred rounded rect is the card's own
    /// silhouette offset and blurred, and the card paints over it (it hides the shadow exactly
    /// where the CPU masks it, because the card is opaque).
    fn draw_shadows(&mut self, n: &Node, shadows: &[figcraft_doc::Shadow], xf: Affine) {
        let Some(radius) = n.corners.uniform() else { return };
        let Some(scene) = &mut self.scene else { return };
        let rect = KRect::new(0.0, 0.0, n.width, n.height);
        for s in shadows {
            let c = s.color.sanitized();
            let at = KAffine::translate(s.offset) * k_affine(xf);
            // The radius and the deviation are in the shape's own units (the transform scales
            // them, and the blur with them).
            scene.draw_blurred_rounded_rect(at, rect, PColor::new([c.r, c.g, c.b, c.a]), radius.max(0.0), (s.radius / 2.0).max(0.0));
        }
    }

    /// The page as it is under node `id`: the background, then every layer painted before it (the
    /// node and everything after it left out).
    fn backdrop_of(&mut self, id: NodeId) -> Result<Scene, Unsupported> {
        let view = self.view.clone();
        let page = self.page;
        let above = figcraft_render::backdrop::above(self.doc, id, &[page]);
        let mut below = Builder {
            doc: self.doc,
            scene: Some(Scene::new()),
            layers: &mut *self.layers,
            view: view.clone(),
            page,
            hidden: above.hidden,
            no_strokes: above.strokes,
        };
        if let Some(s) = &mut below.scene {
            let bg = background(self.doc, page);
            s.fill(Fill::NonZero, KAffine::IDENTITY, PColor::new([bg.r, bg.g, bg.b, bg.a]), None, &view_rect(&view));
        }
        if let Some(p) = self.doc.node(page) {
            for c in &p.children {
                below.node(*c, view.transform(), 0)?;
            }
        }
        below.scene.take().ok_or(Unsupported::Structure)
    }

    /// A node with one background blur: everything painted under it (the page without it or
    /// anything after it) is drawn and blurred on the GPU, shown through its shape, and its own
    /// content goes over that.
    fn backdrop_layer(&mut self, n: &Node, depth: usize, radius: f64) -> Result<(), Unsupported> {
        if n.blend == BlendMode::PlusDarker {
            return Err(Unsupported::Structure);
        }
        let view = self.view.clone();
        let back = self.backdrop_of(n.id)?;
        let sigma = radius / 2.0 * view.scale;
        let Some(image) = self.layers.backdrop(&back, view.width, view.height, sigma) else { return Err(Unsupported::Effect) };
        self.draw_backdrop_image(n, depth, &image, ((0.0, 0.0), true))
    }

    /// A node with one glass effect: its layer's pixels come from the page under it, refracted by
    /// the GPU's glass shader (the CPU's geometry), and its own content goes over them.
    fn glass_layer(&mut self, n: &Node, depth: usize, glass: &figcraft_doc::Glass) -> Result<(), Unsupported> {
        if n.blend == BlendMode::PlusDarker {
            return Err(Unsupported::Structure);
        }
        let view = self.view.clone();
        let Some((x0, y0, w, h)) = self.layer_rect(n)? else { return Ok(()) };
        let parent = n.parent.map(|p| self.doc.world_transform(p)).unwrap_or(Affine::IDENTITY);
        let xf = view.transform() * parent * n.transform;
        let Some(shape) = figcraft_render::backdrop::outline(self.doc, n) else { return Err(Unsupported::Structure) };
        let layer_xf = Affine::translate((-x0, -y0)) * xf;
        let mut pane = figcraft_render::glass::pane(glass, &shape, w, h, layer_xf, view.scale).ok_or(Unsupported::Effect)?;
        pane.pad = figcraft_render::backdrop::pad(n, view.scale);
        let back = self.backdrop_of(n.id)?;
        let Some(image) = self.layers.glass(&back, (view.width, view.height), &pane, (x0 as i32, y0 as i32)) else {
            return Err(Unsupported::Effect);
        };
        self.draw_backdrop_image(n, depth, &image, ((x0, y0), false))
    }

    /// The device rectangle of `n`'s effect layer, in whole pixels (`None` past the layer cap).
    fn layer_rect(&self, n: &Node) -> Result<Option<(f64, f64, u32, u32)>, Unsupported> {
        layer_rect_impl(self.doc, n, &self.view)
    }

    /// `n`'s backdrop effect pixels: its layer texture with its top-left at `at`, clipped to the
    /// node's shape when `clip` (a blurred layer is a rectangle; a pane carries its own coverage);
    /// then the node's content over them.
    fn draw_backdrop_image(&mut self, n: &Node, depth: usize, image: &ImageData, ((x, y), clip): ((f64, f64), bool)) -> Result<(), Unsupported> {
        let view = self.view.clone();
        let parent = n.parent.map(|p| self.doc.world_transform(p)).unwrap_or(Affine::IDENTITY);
        let xf = view.transform() * parent * n.transform;
        let isolated = n.opacity < 1.0 || !matches!(n.blend, BlendMode::PassThrough | BlendMode::Normal);
        if isolated {
            self.push_layer(n);
        }
        let sampler = ImageSampler { x_extend: Extend::Pad, y_extend: Extend::Pad, quality: ImageQuality::Low, alpha: 1.0 };
        if clip {
            if let Some((path, rule)) = node_outline(self.doc, n)
                && let Some(s) = &mut self.scene
            {
                s.push_clip_layer(fill_rule(rule), k_affine(xf), &path);
                s.draw_image(ImageBrush { image, sampler }, KAffine::translate((x, y)));
                s.pop_layer();
            }
        } else if let Some(s) = &mut self.scene {
            s.draw_image(ImageBrush { image, sampler }, KAffine::translate((x, y)));
        }
        self.content(n, xf, depth)?;
        if isolated && let Some(s) = &mut self.scene {
            s.pop_layer();
        }
        Ok(())
    }

    /// A node with one layer blur: its content is drawn into a layer of its own, blurred on the
    /// GPU, and composited with its opacity and blend mode (as the CPU renderer isolates it).
    fn blurred_layer(&mut self, n: &Node, _parent_xf: Affine, depth: usize, radius: f64) -> Result<(), Unsupported> {
        if n.blend == BlendMode::PlusDarker {
            return Err(Unsupported::Structure);
        }
        let Some(bounds) = figcraft_render::paint_bounds(self.doc, n.id) else { return Ok(()) };
        let view = self.view.clone();
        let scale = view.scale.max(1e-9);
        let (x0f, y0f) = ((bounds.x0 - view.origin.0) * view.scale, (bounds.y0 - view.origin.1) * view.scale);
        let (x1f, y1f) = ((bounds.x1 - view.origin.0) * view.scale, (bounds.y1 - view.origin.1) * view.scale);
        if !(x0f.is_finite() && y0f.is_finite() && x1f.is_finite() && y1f.is_finite()) {
            return Err(Unsupported::Structure);
        }
        let (x0, y0) = (x0f.floor(), y0f.floor());
        let (w, h) = ((x1f.ceil() - x0).max(1.0), (y1f.ceil() - y0).max(1.0));
        // A layer this big is cheaper (and exact) on the CPU.
        const MAX_LAYER: f64 = 8_192.0;
        if w > MAX_LAYER || h > MAX_LAYER {
            return Err(Unsupported::Effect);
        }
        let (w, h) = (w as u32, h as u32);
        let layer = View { origin: (view.origin.0 + x0 / scale, view.origin.1 + y0 / scale), scale: view.scale, width: w, height: h };
        let parent = n.parent.map(|p| self.doc.world_transform(p)).unwrap_or(Affine::IDENTITY);
        let mut sub = Builder {
            doc: self.doc,
            scene: Some(Scene::new()),
            layers: &mut *self.layers,
            view: layer.clone(),
            page: self.page,
            hidden: self.hidden.clone(),
            no_strokes: self.no_strokes.clone(),
        };
        // The content only: its opacity and blend mode apply to the blurred result, not inside it.
        sub.content(n, layer.transform() * parent * n.transform, depth)?;
        let sub_scene = sub.scene.take().ok_or(Unsupported::Structure)?;
        drop(sub);
        let sigma = radius / 2.0 * view.scale;
        let Some(image) = self.layers.blurred(&sub_scene, w, h, sigma) else { return Err(Unsupported::Effect) };
        let isolated = n.opacity < 1.0 || !matches!(n.blend, BlendMode::PassThrough | BlendMode::Normal);
        if isolated {
            self.push_layer(n);
        }
        let sampler = ImageSampler { x_extend: Extend::Pad, y_extend: Extend::Pad, quality: ImageQuality::Low, alpha: 1.0 };
        if let Some(s) = &mut self.scene {
            s.draw_image(ImageBrush { image: &image, sampler }, KAffine::translate((x0, y0)));
        }
        if isolated && let Some(s) = &mut self.scene {
            s.pop_layer();
        }
        Ok(())
    }

    /// A layer for the node's opacity and blend mode, clipped to what the scene can see: the whole
    /// target (the builder doesn't know the view's bounds here; the canvas keeps its view).
    fn push_layer(&mut self, n: &Node) {
        let Some(s) = &mut self.scene else { return };
        let target = KRect::new(0.0, 0.0, 32_768.0, 32_768.0);
        s.push_layer(Fill::NonZero, blend(n.blend), n.opacity.clamp(0.0, 1.0), KAffine::IDENTITY, &target);
    }

    /// Fills, children and strokes of `n`, without opacity, blend or effects.
    fn content(&mut self, n: &Node, xf: Affine, depth: usize) -> Result<(), Unsupported> {
        if matches!(n.kind, NodeKind::Page { .. } | NodeKind::Group) {
            return self.children(n.id, xf, depth + 1);
        }
        let text = matches!(n.kind, NodeKind::Text(_));
        if text {
            self.check_text(n)?;
        }
        // The outline fills, clips children to rounded corners and strokes booleans.
        let outline = node_outline(self.doc, n);
        if let Some((path, rule)) = &outline
            && !path.is_empty()
        {
            for p in &n.fills {
                self.fill(p, path, *rule, (n.width, n.height), xf)?;
            }
        }
        if !matches!(n.kind, NodeKind::BooleanOp { .. }) && !n.hides_contents() {
            let clips = n.clip && n.kind.is_frame_like() && !n.children.is_empty();
            let clip = if clips { outline.as_ref().map(|(p, rule)| (p.clone(), *rule)) } else { None };
            if let Some((p, rule)) = &clip
                && let Some(s) = &mut self.scene
            {
                s.push_clip_layer(fill_rule(*rule), k_affine(xf), p);
            }
            self.children(n.id, xf, depth + 1)?;
            if clip.is_some()
                && let Some(s) = &mut self.scene
            {
                s.pop_layer();
            }
        }
        if n.has_visible_stroke()
            && !self.no_strokes.contains(&n.id)
            && let Some((path, _)) = &outline
        {
            self.strokes(n, path, (n.width, n.height), xf, text)?;
        }
        Ok(())
    }

    /// Text paints its glyph outlines with one set of fills: runs with their own fills and colour
    /// glyphs stay on the CPU for now.
    fn check_text(&self, n: &Node) -> Result<(), Unsupported> {
        let Some((layout, _)) = figcraft_text::layout_node(n) else { return Ok(()) };
        if layout.styles.iter().any(|s| s.fills.as_ref().is_some_and(|f| !f.is_empty())) {
            return Err(Unsupported::Text);
        }
        let scale = 1.0;
        if layout.glyphs.iter().any(|g| g.color(scale).is_some()) {
            return Err(Unsupported::Text);
        }
        Ok(())
    }

    fn fill(&mut self, paint: &Paint, path: &figcraft_geom::BezPath, rule: FillRule, size: (f64, f64), xf: Affine) -> Result<(), Unsupported> {
        let Some((brush, brush_xf)) = self.brush(paint, size)? else { return Ok(()) };
        if let Some(s) = &mut self.scene {
            s.fill(fill_rule(rule), k_affine(xf), &brush, brush_xf, path);
        }
        Ok(())
    }

    fn strokes(&mut self, n: &Node, path: &figcraft_geom::BezPath, size: (f64, f64), xf: Affine, text: bool) -> Result<(), Unsupported> {
        let props = &n.stroke;
        let plain = props.align == figcraft_doc::StrokeAlign::Center
            && props.sides.is_none()
            && props.dashes.is_empty()
            && props.profile().is_none()
            && props.brush.is_none()
            && is_plain_cap(props.cap_start)
            && props.cap_start == props.cap_end;
        if !plain {
            return Err(Unsupported::Stroke);
        }
        let stroke = Stroke::new(props.weight.max(0.0)).with_join(join(props.join)).with_caps(cap(props.cap_start));
        if text {
            // Glyph outlines are closed: inside and outside strokes clip to them.
            return Err(Unsupported::Stroke);
        }
        for p in &n.strokes {
            let Some((brush, brush_xf)) = self.brush(p, size)? else { continue };
            if let Some(s) = &mut self.scene {
                s.stroke(&stroke, k_affine(xf), &brush, brush_xf, path);
            }
        }
        Ok(())
    }

    /// The paint's brush and its unit-box → local transform (`None`: the paint draws nothing).
    #[allow(clippy::type_complexity)]
    fn brush(&self, paint: &Paint, size: (f64, f64)) -> Result<Option<(Brush, Option<KAffine>)>, Unsupported> {
        if !paint.visible || paint.opacity <= 0.0 {
            return Ok(None);
        }
        // A paint's own blend mode needs a layer per paint: not drawn yet.
        if !matches!(paint.blend, BlendMode::PassThrough | BlendMode::Normal) {
            return Err(Unsupported::Paint);
        }
        let op = paint.opacity.clamp(0.0, 1.0);
        let brush = match &paint.kind {
            PaintKind::Solid { color } => {
                let c = color.with_alpha(color.a * op);
                if c.a <= 0.0 {
                    return Ok(None);
                }
                Brush::Solid(pcolor(c))
            }
            PaintKind::Gradient(g) => Brush::Gradient(gradient(g, op)?),
            PaintKind::Image(_) | PaintKind::Pattern(_) => return Err(Unsupported::Paint),
        };
        let frame = match &paint.kind {
            PaintKind::Gradient(g) => Some(k_affine(handle_frame(g, size.0, size.1))),
            _ => None,
        };
        Ok(Some((brush, frame)))
    }
}

/// A gradient brush in its unit box (`(0,0)` → from, `(1,0)` → to; see
/// [`figcraft_render::paint`]), which the brush transform maps onto the shape's box.
fn gradient(g: &Gradient, opacity: f32) -> Result<PGradient, Unsupported> {
    let kind = match g.kind {
        GradientKind::Linear => PGradient::new_linear(Point::new(0.0, 0.0), Point::new(1.0, 0.0)),
        GradientKind::Radial => PGradient::new_radial(Point::new(0.0, 0.0), 1.0),
        GradientKind::Angular => PGradient::new_sweep(Point::new(0.0, 0.0), 0.0, std::f32::consts::TAU),
        GradientKind::Diamond => return Err(Unsupported::Paint),
    };
    let mut stops = ColorStops::new();
    stops.extend(
        g.sorted_stops()
            .into_iter()
            .map(|s| ColorStop { offset: s.position, color: DynamicColor::from_alpha_color(pcolor(s.color.with_alpha(s.color.a * opacity))) }),
    );
    Ok(kind.with_extend(Extend::Pad).with_stops(stops))
}

/// The unit-box → local mapping for a gradient's handles (the renderer's `handle_frame`).
fn handle_frame(g: &Gradient, w: f64, h: f64) -> Affine {
    let w = w.max(1e-6);
    let h = h.max(1e-6);
    let f = (g.from.0 * w, g.from.1 * h);
    let t = (g.to.0 * w, g.to.1 * h);
    let k = (g.width.0 * w, g.width.1 * h);
    Affine::new([t.0 - f.0, t.1 - f.1, k.0 - f.0, k.1 - f.1, f.0, f.1])
}

fn fill_rule(r: FillRule) -> Fill {
    match r {
        FillRule::EvenOdd => Fill::EvenOdd,
        FillRule::Winding => Fill::NonZero,
    }
}

/// The view's rectangle in its own device pixels.
fn view_rect(view: &View) -> KRect {
    KRect::new(0.0, 0.0, f64::from(view.width), f64::from(view.height))
}

fn k_affine(a: Affine) -> KAffine {
    KAffine::new(a.as_coeffs())
}

/// `figcraft_color::Color` (sRGB, straight alpha) → peniko.
pub fn pcolor(c: Color) -> PColor {
    PColor::new([c.r, c.g, c.b, c.a])
}

fn blend(b: BlendMode) -> PBlend {
    use BlendMode as B;
    let mix = match b {
        B::PassThrough | B::Normal => Mix::Normal,
        B::Darken => Mix::Darken,
        B::Multiply => Mix::Multiply,
        B::ColorBurn => Mix::ColorBurn,
        B::Lighten => Mix::Lighten,
        B::Screen => Mix::Screen,
        B::ColorDodge => Mix::ColorDodge,
        B::Overlay => Mix::Overlay,
        B::SoftLight => Mix::SoftLight,
        B::HardLight => Mix::HardLight,
        B::Difference => Mix::Difference,
        B::Exclusion => Mix::Exclusion,
        B::Hue => Mix::Hue,
        B::Saturation => Mix::Saturation,
        B::Color => Mix::Color,
        B::Luminosity => Mix::Luminosity,
        B::PlusLighter => Mix::Normal,
        B::PlusDarker => Mix::Normal,
    };
    let compose = match b {
        B::PlusLighter => Compose::Plus,
        _ => Compose::SrcOver,
    };
    PBlend::new(mix, compose)
}

fn join(j: figcraft_doc::StrokeJoin) -> vello::kurbo::Join {
    match j {
        figcraft_doc::StrokeJoin::Miter => vello::kurbo::Join::Miter,
        figcraft_doc::StrokeJoin::Bevel => vello::kurbo::Join::Bevel,
        figcraft_doc::StrokeJoin::Round => vello::kurbo::Join::Round,
    }
}

fn is_plain_cap(c: figcraft_doc::StrokeCap) -> bool {
    matches!(c, figcraft_doc::StrokeCap::None | figcraft_doc::StrokeCap::Round | figcraft_doc::StrokeCap::Square)
}

fn cap(c: figcraft_doc::StrokeCap) -> vello::kurbo::Cap {
    match c {
        figcraft_doc::StrokeCap::Round => vello::kurbo::Cap::Round,
        figcraft_doc::StrokeCap::Square => vello::kurbo::Cap::Square,
        _ => vello::kurbo::Cap::Butt,
    }
}
