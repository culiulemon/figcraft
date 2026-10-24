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
use vello::peniko::{BlendMode as PBlend, Brush, Color as PColor, ColorStop, ColorStops, Compose, Extend, Fill, Gradient as PGradient, Mix};

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

/// The page's background colour, drawn under the scene.
pub fn background(doc: &Document, page: NodeId) -> Color {
    match doc.node(page).map(|p| &p.kind) {
        Some(NodeKind::Page { background, .. }) => *background,
        _ => Color::DEFAULT_PAGE,
    }
}

/// Whether `page` can be drawn on the GPU at all: `Ok` when every visible layer is in the
/// builder's feature set, else the first reason it isn't.
pub fn supported(doc: &Document, page: NodeId) -> Result<(), Unsupported> {
    let mut b = Builder { doc, scene: None };
    b.children(page, Affine::IDENTITY, 0)
}

/// Draw page `page` of `doc` as seen through `view` into a vello scene.
pub fn build(doc: &Document, page: NodeId, view: &View) -> Result<Scene, Unsupported> {
    let mut b = Builder { doc, scene: Some(Scene::new()) };
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
        let Some(n) = self.doc.node(id) else { return Ok(()) };
        if !n.visible || n.opacity <= 0.0 {
            return Ok(());
        }
        if n.is_mask {
            return Err(Unsupported::Mask);
        }
        if n.effects.iter().any(|e| e.visible) {
            return Err(Unsupported::Effect);
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
