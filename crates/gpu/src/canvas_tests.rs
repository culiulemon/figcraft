//! The GPU canvas against the CPU renderer: pixel parity, the fallback predicate, read-back and
//! frame timings (see `docs/gpu-rasterizer.md`).

use std::sync::Arc;

use figcraft_color::{BlendMode, Color, Gradient, GradientKind, Paint};
use figcraft_doc::{Document, Effect, EffectKind, Glass, NodeKind, StrokeAlign, TextData};
use figcraft_render::{Options, Pixmap, View, render_view};
use wgpu::{Device, Queue};

use crate::canvas::CanvasGpu;
use crate::scene;
use crate::scene::Layers as _;

/// The machine's adapter (none in a headless CI container: the tests then skip).
fn device() -> Option<(Arc<Device>, Arc<Queue>)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;
    Some((Arc::new(device), Arc::new(queue)))
}

/// Shapes, corners, gradients, opacity, blend and a stroke (no text, images or effects).
fn parity_doc() -> Document {
    let mut d = Document::new("gpu parity");
    let p = d.first_page().unwrap();
    let hex = |s: &str| Color::from_hex(s).unwrap_or(Color::WHITE);

    let solid = d.create(p, NodeKind::Rectangle, "solid", 24.0, 24.0, 160.0, 100.0).unwrap();
    d.get_mut(solid).unwrap().fills = vec![Paint::solid(hex("E03030"))];

    let rounded = d.create(p, NodeKind::Rectangle, "rounded", 220.0, 24.0, 160.0, 100.0).unwrap();
    {
        let n = d.get_mut(rounded).unwrap();
        n.corners.radii = [28.0; 4];
        n.fills = vec![Paint::solid(hex("2040E0"))];
    }

    let gradient = d.create(p, NodeKind::Rectangle, "gradient", 416.0, 24.0, 160.0, 100.0).unwrap();
    {
        let g = Gradient {
            kind: GradientKind::Linear,
            stops: vec![
                figcraft_color::ColorStop { position: 0.0, color: hex("F0C830") },
                figcraft_color::ColorStop { position: 1.0, color: hex("20B0D0") },
            ],
            from: (0.0, 0.5),
            to: (1.0, 0.5),
            width: (0.0, -0.5),
        };
        d.get_mut(gradient).unwrap().fills = vec![Paint::gradient(g)];
    }

    let ellipse =
        d.create(p, NodeKind::Ellipse { start: 0.0, sweep: std::f64::consts::TAU, inner: 0.0 }, "ellipse", 24.0, 160.0, 160.0, 100.0).unwrap();
    d.get_mut(ellipse).unwrap().fills = vec![Paint::solid(hex("20C060"))];

    let half = d.create(p, NodeKind::Rectangle, "half", 220.0, 160.0, 160.0, 100.0).unwrap();
    {
        let n = d.get_mut(half).unwrap();
        n.opacity = 0.5;
        n.fills = vec![Paint::solid(hex("A040E0"))];
    }

    let stroke = d.create(p, NodeKind::Rectangle, "stroke", 416.0, 160.0, 160.0, 100.0).unwrap();
    {
        let n = d.get_mut(stroke).unwrap();
        n.fills.clear();
        n.corners.radii = [16.0; 4];
        n.strokes = vec![Paint::solid(hex("303030"))];
        n.stroke.weight = 6.0;
        n.stroke.align = StrokeAlign::Center;
    }

    let blended = d.create(p, NodeKind::Rectangle, "blended", 300.0, 90.0, 120.0, 80.0).unwrap();
    {
        let n = d.get_mut(blended).unwrap();
        n.blend = BlendMode::Multiply;
        n.fills = vec![Paint::solid(hex("40E0A0"))];
    }
    d
}

/// The parity canvas: 600 × 300 device pixels at 100%.
const PARITY: (u32, u32) = (600, 300);

/// The CPU render of `view` and the GPU one, through the same doc.
fn both(d: &Document, view: &View) -> Option<(Pixmap, Pixmap)> {
    let p = d.first_page()?;
    let cpu = render_view(d, p, view, &Options::default())?;
    let (device, queue) = device()?;
    let scene = scene::build(d, p, view).ok()?;
    let mut canvas = CanvasGpu::new(&device, view.width, view.height)?;
    canvas.render(&device, &queue, &scene, scene::background(d, p))?;
    let gpu = canvas.read(&device, &queue)?;
    Some((cpu, gpu))
}

/// The worst channel difference and the mean absolute difference (0–255 scale), with where the
/// worst pixel is and how many pixels differ by more than 8 in a channel.
fn diff(a: &Pixmap, b: &Pixmap) -> (u8, f64, (u32, u32), usize) {
    let (mut worst, mut sum, mut n) = (0u8, 0f64, 0usize);
    let mut at = (0, 0);
    let mut over = 0;
    let w = a.width().max(1) as usize;
    for (i, (x, y)) in a.data().iter().zip(b.data()).enumerate() {
        let d = x.abs_diff(*y);
        if d > worst {
            worst = d;
            at = ((i / 4 % w) as u32, (i / 4 / w) as u32);
        }
        sum += f64::from(d);
        n += 1;
    }
    for px in 0..(a.data().len() / 4).min(b.data().len() / 4) {
        let (sa, sb) = (a.data().get(px * 4..px * 4 + 4), b.data().get(px * 4..px * 4 + 4));
        if let (Some(sa), Some(sb)) = (sa, sb)
            && sa.iter().zip(sb).any(|(x, y)| x.abs_diff(*y) > 8)
        {
            over += 1;
        }
    }
    (worst, sum / n.max(1) as f64, at, over)
}

#[test]
fn the_gpu_canvas_matches_the_cpu_render() {
    if device().is_none() {
        eprintln!("no GPU adapter: skipping");
        return;
    }
    let d = parity_doc();
    let (w, h) = PARITY;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let (cpu, gpu) = both(&d, &view).expect("both renderers drew the page");
    assert_eq!((cpu.width(), cpu.height()), (gpu.width(), gpu.height()));
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    if std::env::var("FIGCRAFT_PARITY_PNG").is_ok() {
        let dir = std::env::temp_dir();
        let _ = cpu.save_png(dir.join("figcraft-parity-cpu.png"));
        let _ = gpu.save_png(dir.join("figcraft-parity-gpu.png"));
        println!("wrote {}", dir.display());
    }
    let px = |p: &Pixmap, (x, y): (u32, u32)| {
        let i = ((y as usize * p.width() as usize) + x as usize) * 4;
        p.data().get(i..i + 4).map(<[u8]>::to_vec)
    };
    println!(
        "parity: worst channel difference {worst} at {at:?} (CPU {:?}, GPU {:?}), mean {mean:.3}, \
         {over} pixels differ by more than 8 (0–255)",
        px(&cpu, at),
        px(&gpu, at)
    );
    assert!(worst <= 96, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 0.5, "the mean difference is {mean:.3}");
    assert!(over < (PARITY.0 * PARITY.1) as usize / 200, "{over} pixels differ by more than 8");
}

#[test]
fn unsupported_pages_are_refused() {
    let mut d = parity_doc();
    let p = d.first_page().unwrap();
    assert_eq!(scene::supported(&d, p), Ok(()));

    // A drop shadow on an opaque card is vello's blurred rounded rect, drawn under the card.
    let id = d.node(p).and_then(|n| n.children.first().copied()).unwrap();
    d.get_mut(id).unwrap().effects = vec![Effect::drop_shadow()];
    assert_eq!(scene::supported(&d, p), Ok(()));
    // A translucent card casts a shadow as faint as it paints, hidden where it covers: the card's
    // silhouette, clipped outside its own outline.
    let opaque = d.node(id).and_then(|n| n.fills.first().cloned()).unwrap();
    d.get_mut(id).unwrap().fills[0].opacity = 0.5;
    assert_eq!(scene::supported(&d, p), Ok(()));
    // Beside a stroke of its own the card's shadow is cast from its pixels, one layer.
    d.get_mut(id).unwrap().strokes = vec![Paint::solid(Color::WHITE)];
    d.get_mut(id).unwrap().stroke.weight = 4.0;
    assert_eq!(scene::supported(&d, p), Ok(()));
    d.get_mut(id).unwrap().strokes.clear();
    d.get_mut(id).unwrap().stroke.weight = 0.0;
    // Nor when the fill fades: the shadow is cast from the card's own pixels.
    d.get_mut(id).unwrap().fills = vec![Paint::gradient(Gradient {
        kind: GradientKind::Linear,
        stops: vec![
            figcraft_color::ColorStop { position: 0.0, color: Color::WHITE },
            figcraft_color::ColorStop { position: 1.0, color: Color { a: 0.0, ..Color::WHITE } },
        ],
        from: (0.0, 0.5),
        to: (1.0, 0.5),
        width: (0.0, 1.0),
    })];
    assert_eq!(scene::supported(&d, p), Ok(()));
    d.get_mut(id).unwrap().fills[0] = opaque;
    // A shadow that spreads grows the card's own silhouette; one that shows behind is hidden by an
    // opaque card either way; one with a blend mode composites as a layer. None needs a layer.
    d.get_mut(id).unwrap().effects = vec![Effect::drop_shadow()];
    if let Some(figcraft_doc::EffectKind::DropShadow(s)) = d.get_mut(id).ok().and_then(|n| n.effects.first_mut()).map(|e| &mut e.kind) {
        s.spread = 6.0;
    }
    assert_eq!(scene::supported(&d, p), Ok(()));
    if let Some(figcraft_doc::EffectKind::DropShadow(s)) = d.get_mut(id).ok().and_then(|n| n.effects.first_mut()).map(|e| &mut e.kind) {
        s.spread = 0.0;
        s.show_behind = true;
        s.blend = BlendMode::Multiply;
    }
    assert_eq!(scene::supported(&d, p), Ok(()));
    // A gradient fill covers the card as opaquely as a solid one (its stops are all opaque).
    let solid = d.node(id).and_then(|n| n.fills.first().cloned()).unwrap();
    let mut g = Gradient {
        kind: GradientKind::Linear,
        stops: vec![
            figcraft_color::ColorStop { position: 0.0, color: Color::from_hex("F0C830").unwrap_or(Color::WHITE) },
            figcraft_color::ColorStop { position: 1.0, color: Color::from_hex("20B0D0").unwrap_or(Color::WHITE) },
        ],
        from: (0.0, 0.5),
        to: (1.0, 0.5),
        width: (0.0, 1.0),
    };
    d.get_mut(id).unwrap().fills = vec![Paint::gradient(g.clone())];
    assert_eq!(scene::supported(&d, p), Ok(()));
    // A linear gradient whose width handle isn't square to its axis (imported files can skew one)
    // draws different bands in vello's projection: the CPU has it.
    g.from = (0.0, 0.0);
    g.to = (1.0, 1.0);
    g.width = (1.0, 0.0);
    d.get_mut(id).unwrap().fills = vec![Paint::gradient(g)];
    assert_eq!(scene::supported(&d, p), Err(scene::Unsupported::Paint));
    d.get_mut(id).unwrap().fills = vec![solid];
    // An opaque stroke grows the silhouette; a translucent one is cast from the card's pixels.
    d.get_mut(id).unwrap().strokes = vec![Paint::solid(Color::WHITE)];
    d.get_mut(id).unwrap().stroke.weight = 4.0;
    d.get_mut(id).unwrap().stroke.align = StrokeAlign::Outside;
    assert_eq!(scene::supported(&d, p), Ok(()), "an outside stroke is the renderer's band");
    d.get_mut(id).unwrap().strokes[0].opacity = 0.5;
    assert_eq!(scene::supported(&d, p), Ok(()), "a translucent band casts from the pixels");
    d.get_mut(id).unwrap().strokes.clear();
    d.get_mut(id).unwrap().stroke.weight = 0.0;
    d.get_mut(id).unwrap().effects = vec![Effect::drop_shadow()];
    // One layer blur is drawn on the GPU (its own layer, blurred there).
    d.get_mut(id).unwrap().effects = vec![Effect::layer_blur()];
    assert_eq!(scene::supported(&d, p), Ok(()));
    // One layer of its own: a blur and a shadow together, in the CPU's order.
    d.get_mut(id).unwrap().effects = vec![Effect::drop_shadow(), Effect::layer_blur()];
    assert_eq!(scene::supported(&d, p), Ok(()));
    // A background blur reads what lies under the node: drawn on the GPU from a backdrop render.
    d.get_mut(id).unwrap().effects = vec![Effect::background_blur()];
    assert_eq!(scene::supported(&d, p), Ok(()));
    d.get_mut(id).unwrap().effects = vec![Effect::background_blur(), Effect::drop_shadow()];
    assert_eq!(scene::supported(&d, p), Err(scene::Unsupported::Effect));
    // Glass is a pane over the backdrop, drawn by the GPU's glass shader.
    d.get_mut(id).unwrap().effects = vec![Effect { kind: EffectKind::Glass(Glass::default()), visible: true }];
    assert_eq!(scene::supported(&d, p), Ok(()));
    d.get_mut(id).unwrap().effects.clear();

    // An image fill draws from the renderer's own decoded, filtered, cached pixmap.
    d.get_mut(id).unwrap().fills = vec![Paint::image("hash", figcraft_color::ImageScaleMode::Fill)];
    assert_eq!(scene::supported(&d, p), Ok(()), "an image fill is the renderer's pixmap");
    // A paint's own blend mode composites in a layer of its own; plus-darker has no vello mode.
    let mut p0 = Paint::solid(Color::WHITE);
    p0.blend = BlendMode::Multiply;
    d.get_mut(id).unwrap().fills = vec![p0];
    assert_eq!(scene::supported(&d, p), Ok(()));
    d.get_mut(id).unwrap().fills[0].blend = BlendMode::PlusDarker;
    assert_eq!(scene::supported(&d, p), Err(scene::Unsupported::Paint));
    // An inner shadow of a shape is one blurred buffer clipped to it; a group's is the CPU's (it
    // is cast from the group's pixels).
    let mut inner = figcraft_doc::Shadow::default();
    inner.radius = 10.0;
    d.get_mut(id).unwrap().fills = vec![Paint::solid(Color::WHITE)];
    d.get_mut(id).unwrap().effects = vec![Effect { kind: EffectKind::InnerShadow(inner), visible: true }];
    assert_eq!(scene::supported(&d, p), Ok(()));
    d.get_mut(id).unwrap().kind = NodeKind::Group;
    assert_eq!(scene::supported(&d, p), Err(scene::Unsupported::Effect));
}

/// Two cards, the upper one rounded and blurred as a layer.
fn blurred_doc() -> Document {
    let mut d = Document::new("gpu layer blur");
    let p = d.first_page().unwrap();
    let under = d.create(p, NodeKind::Rectangle, "under", 120.0, 60.0, 220.0, 160.0).unwrap();
    d.get_mut(under).unwrap().fills = vec![Paint::solid(Color::from_hex("2040E0").unwrap_or(Color::WHITE))];
    let card = d.create(p, NodeKind::Rectangle, "blurred", 300.0, 80.0, 200.0, 140.0).unwrap();
    {
        let n = d.get_mut(card).unwrap();
        n.corners.radii = [24.0; 4];
        n.fills = vec![Paint::solid(Color::from_hex("E03030").unwrap_or(Color::WHITE))];
        n.effects = vec![Effect::layer_blur()];
    }
    d
}

#[test]
fn the_gpu_layer_blur_matches_the_cpu_render() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = blurred_doc();
    let p = d.first_page().unwrap();
    let (w, h) = PARITY;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let cpu = render_view(&d, p, &view, &Options::default()).unwrap();
    let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
    let built = {
        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
        scene::build_with(&d, p, &view, &mut layers)
    };
    let built = built.expect("the GPU draws a layer blur");
    assert!(canvas.render(&device, &queue, &built, scene::background(&d, p)).is_some());
    let gpu = canvas.read(&device, &queue).unwrap();
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    println!("layer blur parity: worst {worst} at {at:?}, mean {mean:.3}, {over} pixels differ by more than 8");
    assert!(worst <= 128, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 2.0, "the mean difference is {mean:.3}");
}

/// A rounded white card with a drop shadow over a light ground.
fn shadow_doc() -> Document {
    let mut d = Document::new("gpu shadow");
    let p = d.first_page().unwrap();
    let ground = d.create(p, NodeKind::Rectangle, "ground", 60.0, 40.0, 480.0, 220.0).unwrap();
    d.get_mut(ground).unwrap().fills = vec![Paint::solid(Color::from_hex("E8E8E8").unwrap_or(Color::WHITE))];
    let card = d.create(p, NodeKind::Rectangle, "card", 200.0, 90.0, 200.0, 130.0).unwrap();
    {
        let n = d.get_mut(card).unwrap();
        n.corners.radii = [16.0; 4];
        n.fills = vec![Paint::solid(Color::WHITE)];
        n.effects = vec![Effect::drop_shadow()];
    }
    d
}

#[test]
fn the_gpu_drop_shadow_matches_the_cpu_render() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = shadow_doc();
    let p = d.first_page().unwrap();
    let (w, h) = PARITY;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let cpu = render_view(&d, p, &view, &Options::default()).unwrap();
    let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
    let built = {
        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
        scene::build_with(&d, p, &view, &mut layers)
    };
    let built = built.expect("the GPU draws a drop shadow");
    assert!(canvas.render(&device, &queue, &built, scene::background(&d, p)).is_some());
    let gpu = canvas.read(&device, &queue).unwrap();
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    println!("drop shadow parity: worst {worst} at {at:?}, mean {mean:.3}, {over} pixels differ by more than 8");
    assert!(worst <= 128, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 2.0, "the mean difference is {mean:.3}");
}

/// Text in one style: the renderer's layout turns it into glyph outlines.
fn text_doc() -> Document {
    let mut d = Document::new("gpu text");
    let p = d.first_page().unwrap();
    let mut td = TextData::new("Hello, FigCraft");
    td.style.font_size = 40.0;
    let id = d.create(p, NodeKind::Text(td), "title", 30.0, 70.0, 400.0, 80.0).unwrap();
    d.get_mut(id).unwrap().fills = vec![Paint::solid(Color::BLACK)];
    let mut sub = TextData::new("smaller text 123");
    sub.style.font_size = 18.0;
    let id = d.create(p, NodeKind::Text(sub), "body", 30.0, 160.0, 400.0, 40.0).unwrap();
    d.get_mut(id).unwrap().fills = vec![Paint::solid(Color::from_hex("404040").unwrap_or(Color::BLACK))];
    d
}

#[test]
fn the_gpu_text_matches_the_cpu_render() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = text_doc();
    let p = d.first_page().unwrap();
    let (w, h) = PARITY;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let cpu = render_view(&d, p, &view, &Options::default()).unwrap();
    let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
    let built = scene::build_with(&d, p, &view, &mut scene::NoLayers).expect("the GPU draws text");
    assert!(canvas.render(&device, &queue, &built, scene::background(&d, p)).is_some());
    let gpu = canvas.read(&device, &queue).unwrap();
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    println!("text parity: worst {worst} at {at:?}, mean {mean:.3}, {over} pixels differ by more than 8");
    assert!(worst <= 160, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 1.0, "the mean difference is {mean:.3}");
}

/// Coloured stripes with a rounded pane over them that blurs what it covers.
fn backdrop_doc() -> Document {
    let mut d = Document::new("gpu backdrop");
    let p = d.first_page().unwrap();
    let colours = ["E03030", "2040E0", "20C060", "F0C830", "A040E0", "20B0D0"];
    for i in 0..8u32 {
        let id = d.create(p, NodeKind::Rectangle, "stripe", 20.0 + f64::from(i) * 70.0, 60.0, 62.0, 200.0).unwrap();
        d.get_mut(id).unwrap().fills = vec![Paint::solid(Color::from_hex(colours[i as usize % colours.len()]).unwrap_or(Color::WHITE))];
    }
    let pane = d.create(p, NodeKind::Rectangle, "pane", 150.0, 90.0, 240.0, 150.0).unwrap();
    {
        let n = d.get_mut(pane).unwrap();
        n.corners.radii = [28.0; 4];
        n.fills = vec![Paint::solid(Color { a: 0.15, ..Color::WHITE })];
        n.effects = vec![Effect::background_blur()];
    }
    d
}

#[test]
fn the_gpu_background_blur_matches_the_cpu_render() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = backdrop_doc();
    let p = d.first_page().unwrap();
    let (w, h) = PARITY;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let cpu = render_view(&d, p, &view, &Options::default()).unwrap();
    let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
    let built = {
        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
        scene::build_with(&d, p, &view, &mut layers)
    }
    .expect("the GPU draws a background blur");
    assert!(canvas.render(&device, &queue, &built, scene::background(&d, p)).is_some());
    let gpu = canvas.read(&device, &queue).unwrap();
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    println!("background blur parity: worst {worst} at {at:?}, mean {mean:.3}, {over} pixels differ by more than 8");
    assert!(worst <= 128, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 2.0, "the mean difference is {mean:.3}");
}

/// Coloured stripes with a glass pane over them.
fn glass_doc() -> Document {
    let mut d = Document::new("gpu glass");
    let p = d.first_page().unwrap();
    let colours = ["E03030", "2040E0", "20C060", "F0C830"];
    for i in 0..12u32 {
        let id = d.create(p, NodeKind::Rectangle, "stripe", f64::from(i) * 50.0, 0.0, 50.0, 300.0).unwrap();
        d.get_mut(id).unwrap().fills = vec![Paint::solid(Color::from_hex(colours[i as usize % colours.len()]).unwrap_or(Color::WHITE))];
    }
    let pane = d.create(p, NodeKind::Rectangle, "glass", 120.0, 60.0, 300.0, 180.0).unwrap();
    {
        let n = d.get_mut(pane).unwrap();
        n.corners.radii = [40.0; 4];
        n.fills = vec![Paint::solid(Color { a: 0.1, ..Color::WHITE })];
        n.effects = vec![Effect { kind: EffectKind::Glass(Glass::default()), visible: true }];
    }
    d
}

#[test]
fn the_gpu_glass_matches_the_cpu_canvas() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = glass_doc();
    let p = d.first_page().unwrap();
    let (w, h) = PARITY;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let cpu = render_view(&d, p, &view, &Options::default()).unwrap();
    let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
    let built = {
        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
        scene::build_with(&d, p, &view, &mut layers)
    }
    .expect("the GPU draws glass");
    assert!(canvas.render(&device, &queue, &built, scene::background(&d, p)).is_some());
    let gpu = canvas.read(&device, &queue).unwrap();
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    println!("glass parity: worst {worst} at {at:?}, mean {mean:.3}, {over} pixels differ by more than 8");
    assert!(worst <= 128, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 2.0, "the mean difference is {mean:.3}");
}

/// Draw `d` through the segmented path and compare with the CPU render; returns the statistics.
fn segmented_parity(d: &Document, label: &str) -> Option<(u8, f64, usize)> {
    let (device, queue) = device()?;
    let p = d.first_page()?;
    let (w, h) = PARITY;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let cpu = render_view(d, p, &view, &Options::default())?;
    let plan = scene::plan(d, p).ok()?;
    let mut canvas = CanvasGpu::new(&device, w, h)?;
    crate::canvas::segmented_frame(&device, &queue, &mut canvas, d, p, &view, &plan, scene::background(d, p))?;
    let gpu = canvas.read(&device, &queue)?;
    if std::env::var("FIGCRAFT_PARITY_PNG").is_ok() {
        let dir = std::env::temp_dir();
        let name = label.replace(' ', "-");
        let _ = cpu.save_png(dir.join(format!("figcraft-seg-{name}-cpu.png")));
        let _ = gpu.save_png(dir.join(format!("figcraft-seg-{name}-gpu.png")));
    }
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    let (mut lo, mut hi) = ((u32::MAX, u32::MAX), (0u32, 0u32));
    let w0 = cpu.width().max(1) as usize;
    let mut first = None;
    for (i, (a, b)) in cpu.data().iter().zip(gpu.data()).enumerate() {
        if a.abs_diff(*b) > 8 {
            let (x, y) = ((i / 4 % w0) as u32, (i / 4 / w0) as u32);
            lo = (lo.0.min(x), lo.1.min(y));
            hi = (hi.0.max(x), hi.1.max(y));
            if first.is_none() {
                first = Some((x, y));
            }
        }
    }
    println!("{label} (segmented): worst {worst} at {at:?}, mean {mean:.3}, {over} differ >8, span {lo:?}..{hi:?}, first {first:?}");
    let px = |p: &Pixmap, (x, y): (u32, u32)| {
        let i = ((y as usize * p.width() as usize) + x as usize) * 4;
        p.data().get(i..i + 4).map(<[u8]>::to_vec)
    };
    println!("  at {at:?}: CPU {:?}, GPU {:?}", px(&cpu, at), px(&gpu, at));
    Some((worst, mean, over))
}

#[test]
fn the_segmented_page_matches_the_cpu_with_one_glass_pane() {
    if device().is_none() {
        eprintln!("no GPU adapter: skipping");
        return;
    }
    let d = glass_doc();
    let (worst, mean, _) = segmented_parity(&d, "one glass pane").expect("drawn");
    assert!(worst <= 128, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 2.0, "the mean difference is {mean:.3}");
}

#[test]
fn the_segmented_page_matches_the_cpu_with_one_background_blur() {
    if device().is_none() {
        eprintln!("no GPU adapter: skipping");
        return;
    }
    let d = backdrop_doc();
    let (worst, mean, _) = segmented_parity(&d, "one background blur").expect("drawn");
    assert!(worst <= 128, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 2.0, "the mean difference is {mean:.3}");
}

#[test]
fn the_gpu_page_matches_with_several_backdrop_layers() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = heavy_effects_doc();
    let p = d.first_page().unwrap();
    let (w, h) = PARITY;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let cpu = render_view(&d, p, &view, &Options::default()).unwrap();
    let plan = scene::plan(&d, p).expect("the page is drawn in segments");
    assert!(plan.iter().filter(|s| matches!(s, scene::Step::Backdrop(_))).count() >= 2, "several backdrop layers");
    let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
    let drawn = crate::canvas::segmented_frame(&device, &queue, &mut canvas, &d, p, &view, &plan, scene::background(&d, p));
    assert!(drawn.is_some(), "the segmented page drew");
    let gpu = canvas.read(&device, &queue).unwrap();
    if std::env::var("FIGCRAFT_PARITY_PNG").is_ok() {
        let dir = std::env::temp_dir();
        let _ = cpu.save_png(dir.join("figcraft-seg-cpu.png"));
        let _ = gpu.save_png(dir.join("figcraft-seg-gpu.png"));
    }
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    let px = |p: &Pixmap, (x, y): (u32, u32)| {
        let i = ((y as usize * p.width() as usize) + x as usize) * 4;
        p.data().get(i..i + 4).map(<[u8]>::to_vec)
    };
    println!(
        "segmented page parity: worst {worst} at {at:?} (CPU {:?}, GPU {:?}), mean {mean:.3}, {over} pixels differ by more than 8",
        px(&cpu, at),
        px(&gpu, at)
    );
    assert!(worst <= 160, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 2.0, "the mean difference is {mean:.3}");
}

/// The shadow variations the fast path now draws without a layer: spread, several shadows, a
/// gradient fill, an opaque stroke, a clipping frame with children, a rotation and a shadow with a
/// blend mode of its own.
fn shadow_breadth_doc() -> Document {
    let mut d = Document::new("gpu shadow breadth");
    let p = d.first_page().unwrap();
    let hex = |s: &str| Color::from_hex(s).unwrap_or(Color::WHITE);
    let stops =
        |a: Color, b: Color| vec![figcraft_color::ColorStop { position: 0.0, color: a }, figcraft_color::ColorStop { position: 1.0, color: b }];
    let shadow = |radius: f64, spread: f64, offset: (f64, f64), show_behind: bool, blend: BlendMode| {
        let mut e = Effect::drop_shadow();
        if let EffectKind::DropShadow(s) = &mut e.kind {
            s.radius = radius;
            s.spread = spread;
            s.offset = offset;
            s.show_behind = show_behind;
            s.blend = blend;
        }
        e
    };

    let spread = d.create(p, NodeKind::Rectangle, "spread", 24.0, 20.0, 140.0, 90.0).unwrap();
    {
        let n = d.get_mut(spread).unwrap();
        n.corners.radii = [18.0; 4];
        let mut g =
            Gradient { kind: GradientKind::Linear, stops: stops(hex("F0C830"), hex("E03030")), from: (0.0, 0.0), to: (1.0, 1.0), width: (-0.5, 0.5) };
        g.square_linear_width(140.0, 90.0);
        n.fills = vec![Paint::gradient(g)];
        n.effects = vec![shadow(10.0, 6.0, (4.0, 6.0), false, BlendMode::Normal)];
    }

    let two = d.create(p, NodeKind::Rectangle, "two shadows", 200.0, 20.0, 140.0, 90.0).unwrap();
    {
        let n = d.get_mut(two).unwrap();
        n.corners.radii = [12.0; 4];
        n.fills = vec![Paint::solid(hex("2040E0"))];
        n.effects = vec![shadow(8.0, 0.0, (0.0, 4.0), false, BlendMode::Normal), shadow(14.0, 2.0, (2.0, 8.0), false, BlendMode::Multiply)];
    }

    let stroked = d.create(p, NodeKind::Rectangle, "stroked", 380.0, 20.0, 150.0, 90.0).unwrap();
    {
        let n = d.get_mut(stroked).unwrap();
        n.corners.radii = [8.0; 4];
        n.fills = vec![Paint::solid(hex("20C060"))];
        n.strokes = vec![Paint::solid(hex("FFFFFF"))];
        n.stroke.weight = 6.0;
        n.stroke.align = StrokeAlign::Outside;
        n.effects = vec![shadow(6.0, 0.0, (0.0, 3.0), false, BlendMode::Normal)];
    }

    // A clipping frame: its children paint over the shadow, and cannot reach past the frame.
    let frame = d.create(p, NodeKind::Frame, "clipped card", 24.0, 150.0, 160.0, 110.0).unwrap();
    {
        let n = d.get_mut(frame).unwrap();
        n.corners.radii = [16.0; 4];
        n.fills = vec![Paint::solid(hex("FFFFFF"))];
        n.clip = true;
        n.effects = vec![shadow(6.0, -2.0, (0.0, 2.0), false, BlendMode::Normal)];
    }
    let child = d.create(frame, NodeKind::Rectangle, "child", 96.0, 60.0, 120.0, 90.0).unwrap();
    d.get_mut(child).unwrap().fills = vec![Paint::solid(hex("20B0D0"))];

    let turned = d.create(p, NodeKind::Rectangle, "turned", 240.0, 160.0, 120.0, 80.0).unwrap();
    {
        let n = d.get_mut(turned).unwrap();
        let turn = 12f64.to_radians();
        n.transform = figcraft_geom::Affine::new([turn.cos(), turn.sin(), -turn.sin(), turn.cos(), 260.0, 170.0]);
        n.corners.radii = [10.0; 4];
        n.fills = vec![Paint::solid(hex("A040E0"))];
        n.effects = vec![shadow(8.0, 3.0, (3.0, 5.0), false, BlendMode::Normal)];
    }

    let behind = d.create(p, NodeKind::Rectangle, "shows behind", 400.0, 160.0, 150.0, 90.0).unwrap();
    {
        let n = d.get_mut(behind).unwrap();
        n.corners.radii = [20.0; 4];
        n.fills = vec![Paint::solid(hex("F0C830"))];
        n.effects = vec![shadow(9.0, 1.0, (0.0, 6.0), true, BlendMode::Normal)];
    }

    // A translucent card: its shadow is as faint as it paints and hidden where it covers.
    let faint = d.create(p, NodeKind::Rectangle, "faint card", 455.0, 240.0, 125.0, 50.0).unwrap();
    {
        let n = d.get_mut(faint).unwrap();
        n.corners.radii = [12.0; 4];
        n.fills = vec![Paint::solid(Color { a: 0.35, ..hex("20B0D0") })];
        // One shadow cast from the layer's pixels (as faint as the card) and one from its outline.
        n.effects = vec![shadow(10.0, 0.0, (3.0, 6.0), false, BlendMode::Normal), shadow(6.0, 3.0, (0.0, 2.0), false, BlendMode::Normal)];
    }
    d
}

#[test]
fn the_gpu_draws_the_shadow_variations() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = shadow_breadth_doc();
    let p = d.first_page().unwrap();
    assert_eq!(scene::supported(&d, p), Ok(()), "a page of shadow variations stays on the GPU");
    let (w, h) = PARITY;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let cpu = render_view(&d, p, &view, &Options::default()).unwrap();
    let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
    let built = scene::build_with(&d, p, &view, &mut scene::NoLayers).expect("the GPU draws the shadows");
    assert!(canvas.render(&device, &queue, &built, scene::background(&d, p)).is_some());
    let gpu = canvas.read(&device, &queue).unwrap();
    if std::env::var("FIGCRAFT_PARITY_PNG").is_ok() {
        let dir = std::env::temp_dir();
        let _ = cpu.save_png(dir.join("figcraft-shadows-cpu.png"));
        let _ = gpu.save_png(dir.join("figcraft-shadows-gpu.png"));
    }
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    let px = |p: &Pixmap, (x, y): (u32, u32)| {
        let i = ((y as usize * p.width() as usize) + x as usize) * 4;
        p.data().get(i..i + 4).map(<[u8]>::to_vec)
    };
    println!(
        "shadow breadth parity: worst {worst} at {at:?} (CPU {:?}, GPU {:?}), mean {mean:.3}, {over} pixels differ by more than 8",
        px(&cpu, at),
        px(&gpu, at)
    );
    assert!(worst <= 96, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 1.0, "the mean difference is {mean:.3}");
}

/// A photo-like PNG: a diagonal gradient with a checker of squares, opaque.
fn photo_png(w: u32, h: u32) -> Vec<u8> {
    use image::ImageEncoder as _;
    let mut img = image::RgbaImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let (fx, fy) = (f64::from(x) / f64::from(w), f64::from(y) / f64::from(h));
        let t = fx * 0.6 + fy * 0.4;
        let band = if (x / 16 + y / 16) % 2 == 0 { 1.0 } else { 0.86 };
        let c = |a: f64, b: f64| ((a + (b - a) * t) * band * 255.0).round().clamp(0.0, 255.0) as u8;
        *px = image::Rgba([c(0.08, 0.95), c(0.30, 0.72), c(0.85, 0.18), 255]);
    }
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8).expect("png");
    out
}

/// Every paint the GPU now draws as the renderer's own pixmap or in a layer of its own: a photo in
/// each scale mode (1:1, fitted, cropped, tiled, rotated, translucent), a filtered one, a diamond
/// gradient, a pattern fill and paints with their own blend modes.
fn paints_doc() -> Document {
    let mut d = Document::new("gpu paints");
    let p = d.first_page().unwrap();
    let hex = |s: &str| Color::from_hex(s).unwrap_or(Color::WHITE);
    let photo = "photo".to_string();
    d.images.insert(photo.clone(), figcraft_doc::ImageData { bytes: Arc::new(photo_png(320, 200)), width: 320, height: 200 });

    let image = |mode: figcraft_color::ImageScaleMode, tune: &dyn Fn(&mut figcraft_color::ImagePaint)| {
        let mut paint = Paint::image(photo.clone(), mode);
        if let figcraft_color::PaintKind::Image(img) = &mut paint.kind {
            tune(img);
        }
        paint
    };
    // 1:1, fitted (letterboxed), cropped, tiled and rotated.
    fn add(d: &mut Document, p: figcraft_doc::NodeId, name: &str, at: (f64, f64), size: (f64, f64), paints: Vec<Paint>) -> figcraft_doc::NodeId {
        let id = d.create(p, NodeKind::Rectangle, name, at.0, at.1, size.0, size.1).unwrap();
        d.get_mut(id).unwrap().fills = paints;
        id
    }

    add(&mut d, p, "photo 1:1", (20.0, 20.0), (320.0, 200.0), vec![image(figcraft_color::ImageScaleMode::Fill, &|_| {})]);
    add(&mut d, p, "photo fit", (360.0, 20.0), (180.0, 200.0), vec![image(figcraft_color::ImageScaleMode::Fit, &|_| {})]);
    add(
        &mut d,
        p,
        "photo crop",
        (560.0, 20.0),
        (180.0, 200.0),
        vec![image(figcraft_color::ImageScaleMode::Crop, &|i| i.crop = Some([2.0, 0.0, 0.0, 1.6, -0.5, -0.3]))],
    );
    add(&mut d, p, "photo tile", (20.0, 240.0), (180.0, 180.0), vec![image(figcraft_color::ImageScaleMode::Tile, &|i| i.scale = 0.5)]);
    add(&mut d, p, "photo turned", (220.0, 240.0), (200.0, 180.0), vec![image(figcraft_color::ImageScaleMode::Fill, &|i| i.rotation = 1)]);
    // A filtered photo, and a translucent one (the sampler's own alpha).
    add(
        &mut d,
        p,
        "photo tuned",
        (440.0, 240.0),
        (140.0, 180.0),
        vec![image(figcraft_color::ImageScaleMode::Fill, &|i| i.filters.exposure = 0.3)],
    );
    let mut faint = image(figcraft_color::ImageScaleMode::Fill, &|_| {});
    faint.opacity = 0.55;
    add(&mut d, p, "photo faint", (600.0, 240.0), (140.0, 180.0), vec![faint]);

    // A diamond gradient: the renderer's 256 × 256 tile through the handle frame.
    let diamond = Gradient {
        kind: GradientKind::Diamond,
        stops: vec![
            figcraft_color::ColorStop { position: 0.0, color: hex("FFF3C0") },
            figcraft_color::ColorStop { position: 0.5, color: hex("F0A030") },
            figcraft_color::ColorStop { position: 1.0, color: hex("6030A0") },
        ],
        from: (0.0, 0.0),
        to: (1.0, 1.0),
        width: (0.0, 1.0),
    };
    add(&mut d, p, "diamond", (760.0, 20.0), (120.0, 200.0), vec![Paint::gradient(diamond)]);

    // A pattern fill: a hidden 24 × 24 layer tiled at its own size.
    let tile = add(&mut d, p, "tile source", (0.0, 0.0), (24.0, 24.0), vec![Paint::solid(hex("F0C830"))]);
    d.get_mut(tile).unwrap().visible = false;
    d.get_mut(tile).unwrap().kind = NodeKind::Frame;
    let dot = d.create(tile, NodeKind::Rectangle, "dot", 7.0, 7.0, 10.0, 10.0).unwrap();
    d.get_mut(dot).unwrap().fills = vec![Paint::solid(hex("2040E0"))];
    add(&mut d, p, "pattern", (760.0, 240.0), (120.0, 180.0), vec![Paint::pattern(tile.0)]);

    // Paints with their own blend modes, in a layer each.
    add(&mut d, p, "ground", (20.0, 440.0), (300.0, 40.0), vec![Paint::solid(hex("40A060"))]);
    let mut over = Paint::solid(hex("E03030"));
    over.blend = BlendMode::Multiply;
    add(&mut d, p, "multiplied", (120.0, 450.0), (260.0, 40.0), vec![over]);
    let mut screen = Paint::solid(hex("2040E0"));
    screen.blend = BlendMode::Screen;
    add(&mut d, p, "screened", (420.0, 450.0), (200.0, 40.0), vec![screen]);
    d
}

#[test]
fn the_gpu_draws_the_paint_variations() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = paints_doc();
    let p = d.first_page().unwrap();
    assert_eq!(scene::supported(&d, p), Ok(()), "a page of images, patterns and paint blends stays on the GPU");
    let (w, h) = (900u32, 500u32);
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let cpu = render_view(&d, p, &view, &Options::default()).unwrap();
    let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
    let built = scene::build_with(&d, p, &view, &mut scene::NoLayers).expect("the GPU draws the paints");
    assert!(canvas.render(&device, &queue, &built, scene::background(&d, p)).is_some());
    let gpu = canvas.read(&device, &queue).unwrap();
    if std::env::var("FIGCRAFT_PARITY_PNG").is_ok() {
        let dir = std::env::temp_dir();
        let _ = cpu.save_png(dir.join("figcraft-paints-cpu.png"));
        let _ = gpu.save_png(dir.join("figcraft-paints-gpu.png"));
    }
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    let px = |p: &Pixmap, (x, y): (u32, u32)| {
        let i = ((y as usize * p.width() as usize) + x as usize) * 4;
        p.data().get(i..i + 4).map(<[u8]>::to_vec)
    };
    println!(
        "paint breadth parity: worst {worst} at {at:?} (CPU {:?}, GPU {:?}), mean {mean:.3}, {over} pixels differ by more than 8",
        px(&cpu, at),
        px(&gpu, at)
    );
    assert!(worst <= 128, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 1.5, "the mean difference is {mean:.3}");
}

/// Drop shadows of shapes that aren't one rounded rect (an ellipse, a star, a polygon, per-corner
/// radii, text, a group, a frame whose children reach past it) and inner shadows on shapes.
fn shadow_shape_doc() -> Document {
    let mut d = Document::new("gpu shadow shapes");
    let p = d.first_page().unwrap();
    let hex = |s: &str| Color::from_hex(s).unwrap_or(Color::WHITE);
    let shadow = |radius: f64, spread: f64, offset: (f64, f64)| {
        let mut e = Effect::drop_shadow();
        if let EffectKind::DropShadow(s) = &mut e.kind {
            s.radius = radius;
            s.spread = spread;
            s.offset = offset;
        }
        e
    };
    let inner = |radius: f64, spread: f64, offset: (f64, f64)| Effect {
        kind: EffectKind::InnerShadow(figcraft_doc::Shadow { radius, spread, offset, ..figcraft_doc::Shadow::default() }),
        visible: true,
    };
    let add = |d: &mut Document, kind: NodeKind, name: &str, at: (f64, f64), size: (f64, f64), fill: &str| {
        let id = d.create(p, kind, name, at.0, at.1, size.0, size.1).unwrap();
        d.get_mut(id).unwrap().fills = vec![Paint::solid(hex(fill))];
        id
    };

    let ellipse = add(&mut d, NodeKind::Ellipse { start: 0.0, sweep: std::f64::consts::TAU, inner: 0.0 }, "ellipse", (20.0, 16.0), (160.0, 120.0), "E03030");
    d.get_mut(ellipse).unwrap().effects = vec![shadow(12.0, 0.0, (0.0, 6.0))];

    let star = add(&mut d, NodeKind::Star { count: 6, inner_ratio: 0.45 }, "star", (210.0, 16.0), (140.0, 120.0), "2040E0");
    d.get_mut(star).unwrap().effects = vec![shadow(10.0, 4.0, (4.0, 6.0))];

    let polygon = add(&mut d, NodeKind::Polygon { count: 7 }, "polygon", (380.0, 16.0), (130.0, 120.0), "20C060");
    d.get_mut(polygon).unwrap().effects = vec![shadow(9.0, -3.0, (3.0, 4.0))];

    let corners = add(&mut d, NodeKind::Rectangle, "per-corner card", (540.0, 16.0), (150.0, 120.0), "F0C830");
    {
        let n = d.get_mut(corners).unwrap();
        n.corners.radii = [0.0, 40.0, 8.0, 30.0];
        n.effects = vec![shadow(10.0, 0.0, (3.0, 5.0))];
    }

    let mut td = TextData::new("Shadowed");
    td.style.font_size = 34.0;
    let text = d.create(p, NodeKind::Text(td), "text", 20.0, 210.0, 200.0, 60.0).unwrap();
    d.get_mut(text).unwrap().fills = vec![Paint::solid(hex("303030"))];
    d.get_mut(text).unwrap().effects = vec![shadow(8.0, 2.0, (2.0, 3.0))];

    // A group: its shadow is cast from its pixels, not one outline.
    let group = d.create(p, NodeKind::Group, "group", 240.0, 190.0, 150.0, 120.0).unwrap();
    d.get_mut(group).unwrap().effects = vec![shadow(11.0, 0.0, (2.0, 8.0))];
    for (name, at, size, colour) in [("a", (0.0, 0.0), (80.0, 70.0), "A040E0"), ("b", (60.0, 45.0), (80.0, 70.0), "20B0D0")] {
        let c = d.create(group, NodeKind::Rectangle, name, at.0, at.1, size.0, size.1).unwrap();
        d.get_mut(c).unwrap().fills = vec![Paint::solid(hex(colour))];
    }

    // A frame whose child reaches past it: also cast from the pixels.
    let frame = d.create(p, NodeKind::Frame, "overflowing frame", 450.0, 190.0, 160.0, 120.0).unwrap();
    {
        let n = d.get_mut(frame).unwrap();
        n.fills = vec![Paint::solid(hex("FFFFFF"))];
        n.corners.radii = [18.0; 4];
        n.clip = false;
        n.effects = vec![shadow(10.0, 3.0, (2.0, 6.0))];
    }
    let reach = d.create(frame, NodeKind::Rectangle, "reaching child", 100.0, 40.0, 120.0, 100.0).unwrap();
    d.get_mut(reach).unwrap().fills = vec![Paint::solid(hex("F06030"))];

    // Inner shadows: a card, one with a spread, and an ellipse.
    let inner_card = add(&mut d, NodeKind::Rectangle, "inner card", (20.0, 380.0), (150.0, 120.0), "F0F0F0");
    {
        let n = d.get_mut(inner_card).unwrap();
        n.corners.radii = [24.0; 4];
        n.effects = vec![inner(14.0, 0.0, (0.0, 4.0))];
    }
    let inner_spread = add(&mut d, NodeKind::Rectangle, "inner spread", (200.0, 380.0), (150.0, 120.0), "E8E8F8");
    {
        let n = d.get_mut(inner_spread).unwrap();
        n.corners.radii = [16.0; 4];
        n.effects = vec![inner(10.0, 6.0, (4.0, 4.0))];
    }
    let inner_ellipse = add(&mut d, NodeKind::Ellipse { start: 0.0, sweep: std::f64::consts::TAU, inner: 0.0 }, "inner ellipse", (380.0, 380.0), (150.0, 120.0), "D0E8F0");
    d.get_mut(inner_ellipse).unwrap().effects = vec![inner(8.0, 0.0, (2.0, 2.0))];

    // Effects together, in the CPU's order: inner shadows, the layer blur and grain, the shadows
    // cast from that result.
    let blurred = add(&mut d, NodeKind::Rectangle, "blur and shadow", (20.0, 530.0), (150.0, 110.0), "F0C830");
    {
        let n = d.get_mut(blurred).unwrap();
        n.corners.radii = [20.0; 4];
        n.effects = vec![Effect::layer_blur(), shadow(10.0, 0.0, (2.0, 6.0))];
    }
    let inner_and_shadow = add(&mut d, NodeKind::Rectangle, "inner and shadow", (200.0, 530.0), (150.0, 110.0), "FFFFFF");
    {
        let n = d.get_mut(inner_and_shadow).unwrap();
        n.corners.radii = [18.0; 4];
        n.effects = vec![inner(12.0, 0.0, (0.0, 4.0)), shadow(8.0, 0.0, (3.0, 7.0))];
    }
    let grain = add(&mut d, NodeKind::Rectangle, "blur, grain and shadow", (380.0, 530.0), (150.0, 110.0), "2040E0");
    {
        let n = d.get_mut(grain).unwrap();
        n.corners.radii = [24.0; 4];
        let mut nz = figcraft_doc::Noise::default();
        nz.size = 1.0;
        nz.density = 0.6;
        n.effects = vec![Effect::layer_blur(), Effect { kind: EffectKind::Noise(nz), visible: true }, shadow(10.0, 0.0, (2.0, 5.0))];
    }
    let ramp = add(&mut d, NodeKind::Rectangle, "progressive blur", (560.0, 530.0), (150.0, 110.0), "20C060");
    {
        let n = d.get_mut(ramp).unwrap();
        n.corners.radii = [20.0; 4];
        let blur = figcraft_doc::Blur {
            radius: 24.0,
            blur_type: figcraft_doc::BlurType::Progressive,
            start_radius: 0.0,
            start: (0.5, 0.0),
            end: (0.5, 1.0),
        };
        n.effects = vec![Effect { kind: EffectKind::LayerBlur(blur), visible: true }, shadow(8.0, 0.0, (2.0, 4.0))];
    }
    d
}

#[test]
fn the_gpu_draws_shadows_of_any_shape() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = shadow_shape_doc();
    let p = d.first_page().unwrap();
    assert_eq!(scene::supported(&d, p), Ok(()), "shadows of every shape stay on the GPU");
    let (w, h) = (900u32, 660u32);
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let cpu = render_view(&d, p, &view, &Options::default()).unwrap();
    let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
    let built = {
        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
        scene::build_with(&d, p, &view, &mut layers)
    }
    .expect("the GPU draws the shadows");
    assert!(canvas.render(&device, &queue, &built, scene::background(&d, p)).is_some());
    let gpu = canvas.read(&device, &queue).unwrap();
    if std::env::var("FIGCRAFT_PARITY_PNG").is_ok() {
        let dir = std::env::temp_dir();
        let _ = cpu.save_png(dir.join("figcraft-shadow-shapes-cpu.png"));
        let _ = gpu.save_png(dir.join("figcraft-shadow-shapes-gpu.png"));
    }
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    let px = |p: &Pixmap, (x, y): (u32, u32)| {
        let i = ((y as usize * p.width() as usize) + x as usize) * 4;
        p.data().get(i..i + 4).map(<[u8]>::to_vec)
    };
    println!(
        "shadow shapes parity: worst {worst} at {at:?} (CPU {:?}, GPU {:?}), mean {mean:.3}, {over} pixels differ by more than 8",
        px(&cpu, at),
        px(&gpu, at)
    );
    if std::env::var("FIGCRAFT_REGIONS").is_ok() {
        for (name, (rx, ry, rw, rh)) in [
            ("ellipse", (20u32, 16u32, 160u32, 120u32)),
            ("star", (210, 16, 140, 120)),
            ("polygon", (380, 16, 130, 120)),
            ("corners", (540, 16, 150, 120)),
            ("text", (20, 210, 200, 60)),
            ("group", (240, 190, 150, 120)),
            ("frame", (450, 190, 160, 120)),
            ("reaching child", (540, 220, 150, 130)),
            ("inner card", (20, 380, 150, 120)),
            ("inner spread", (200, 380, 150, 120)),
            ("inner ellipse", (380, 380, 150, 120)),
            ("blur and shadow", (20, 530, 150, 110)),
            ("inner and shadow", (200, 530, 150, 110)),
            ("blur, grain", (380, 530, 150, 110)),
            ("progressive", (560, 530, 150, 110)),
        ] {
            let (mut sum, mut n, mut w) = (0f64, 0usize, 0u8);
            let mut wp = (rx, ry);
            for y in ry..(ry + rh).min(cpu.height()) {
                for x in rx..(rx + rw).min(cpu.width()) {
                    let i = ((y as usize * cpu.width() as usize) + x as usize) * 4;
                    for c in 0..4 {
                        let d = cpu.data()[i + c].abs_diff(gpu.data()[i + c]);
                        sum += f64::from(d);
                        n += 1;
                        if d > w {
                            w = d;
                            wp = (x, y);
                        }
                    }
                }
            }
            println!(
                "  {name:14} worst {w:3} at {wp:?} (CPU {:?}, GPU {:?}), mean {:.3}",
                px(&cpu, wp),
                px(&gpu, wp),
                sum / n.max(1) as f64
            );
        }
    }
    assert!(worst <= 96, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 0.5, "the mean difference is {mean:.3}");
    assert!(over < (w * h) as usize / 100, "{over} pixels differ by more than 8");
    // Drawn again with the shadows kept: the same pixels, and an edit makes a new one.
    let built = {
        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
        scene::build_with(&d, p, &view, &mut layers)
    }
    .unwrap();
    assert!(canvas.render(&device, &queue, &built, scene::background(&d, p)).is_some());
    let again = canvas.read(&device, &queue).unwrap();
    let (w1, m1, _, o1) = diff(&gpu, &again);
    let shadowed = d
        .nodes()
        .filter(|n| {
            n.effects.iter().any(|e| {
                e.visible && matches!(e.kind, EffectKind::DropShadow(_) | EffectKind::InnerShadow(_))
            })
        })
        .count();
    println!("kept shadows: worst {w1}, mean {m1:.3}, {o1} differ by more than 8, {} kept of {shadowed} shadowed nodes", canvas.kept_shadows());
    if std::env::var("FIGCRAFT_REGIONS").is_ok() {
        let (w2, m2, _, o2) = diff(&cpu, &again);
        println!("  the second frame against the CPU: worst {w2}, mean {m2:.3}, {o2} differ by more than 8");
        for (name, (rx, ry, rw, rh)) in [("ellipse", (20u32, 16u32, 160u32, 120u32)), ("group", (240, 190, 150, 120))] {
            let (mut sum, mut n) = (0f64, 0usize);
            for y in ry..(ry + rh).min(gpu.height()) {
                for x in rx..(rx + rw).min(gpu.width()) {
                    let i = ((y as usize * gpu.width() as usize) + x as usize) * 4;
                    for c in 0..4 {
                        sum += f64::from(gpu.data()[i + c].abs_diff(again.data()[i + c]));
                        n += 1;
                    }
                }
            }
            println!("    {name}: frame 1 against frame 2 mean {:.3}", sum / n.max(1) as f64);
        }
        let mut span = ((u32::MAX, u32::MAX), (0u32, 0u32));
        for (i, (a, b)) in gpu.data().iter().zip(again.data()).enumerate() {
            if a.abs_diff(*b) > 8 {
                let (x, y) = ((i / 4 % gpu.width() as usize) as u32, (i / 4 / gpu.width() as usize) as u32);
                span = ((span.0.0.min(x), span.0.1.min(y)), (span.1.0.max(x), span.1.1.max(y)));
            }
        }
        println!("  kept differ span {span:?}");
    }
    assert_eq!((w1, o1), (0, 0), "a kept shadow draws the same pixels");
    assert!(canvas.kept_shadows() >= 10, "the page's shadows are kept");
    // An edit is a new node, and so a new shadow.
    let before = canvas.kept_shadows();
    let mut changed = d.clone();
    let star = changed.node(p).and_then(|n| n.children.get(1).copied()).unwrap();
    changed.get_mut(star).unwrap().fills = vec![Paint::solid(Color::from_hex("20C060").unwrap_or(Color::WHITE))];
    let built = {
        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
        scene::build_with(&changed, p, &view, &mut layers)
    }
    .unwrap();
    assert!(canvas.render(&device, &queue, &built, scene::background(&changed, p)).is_some());
    assert_eq!(canvas.kept_shadows(), before + 1, "a changed node's shadow is made again");
}

/// `n` rounded cards with a shadow no fast path draws straight into the scene (an ellipse), and
/// `n` per-corner cards (their shadows are cast from a layer).
fn shadow_layer_cards(n: u32, inner: bool) -> Document {
    let mut d = Document::new("gpu shadow layers");
    let p = d.first_page().unwrap();
    let colours = ["FFFFFF", "E03030", "2040E0", "20C060", "F0C830", "A040E0"];
    for i in 0..n {
        let (x, y) = (f64::from(i % 6) * 150.0 + 10.0, f64::from(i / 6) * 150.0 + 10.0);
        let kind = if inner { NodeKind::Rectangle } else { NodeKind::Ellipse { start: 0.0, sweep: std::f64::consts::TAU, inner: 0.0 } };
        let id = d.create(p, kind, "Card", x, y, 130.0, 130.0).unwrap();
        let node = d.get_mut(id).unwrap();
        if inner {
            node.corners.radii = [0.0, 34.0, 12.0, 26.0];
            let mut s = figcraft_doc::Shadow::default();
            s.radius = 14.0;
            s.offset = (0.0, 4.0);
            node.effects = vec![Effect { kind: EffectKind::InnerShadow(s), visible: true }];
        } else {
            node.effects = vec![Effect::drop_shadow()];
        }
        node.fills = vec![Paint::solid(Color::from_hex(colours[i as usize % colours.len()]).unwrap_or(Color::WHITE))];
    }
    d
}

/// The same if only `n` per-corner cards with drop shadows.
fn shadow_layer_cards_rounded(n: u32, shadow: bool) -> Document {
    let mut d = Document::new("gpu shadow rounded");
    let p = d.first_page().unwrap();
    for i in 0..n {
        let (x, y) = (f64::from(i % 6) * 150.0 + 10.0, f64::from(i / 6) * 150.0 + 10.0);
        let id = d.create(p, NodeKind::Rectangle, "Card", x, y, 130.0, 130.0).unwrap();
        let node = d.get_mut(id).unwrap();
        node.corners.radii = [0.0, 34.0, 12.0, 26.0];
        node.fills = vec![Paint::solid(Color::WHITE)];
        if shadow {
            node.effects = vec![Effect::drop_shadow()];
        }
    }
    d
}

#[test]
#[ignore = "timing: run in release with --ignored --nocapture"]
fn shadow_pass_timings() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let mut canvas = CanvasGpu::new(&device, 904, 860).unwrap();
    let mut scene = vello::Scene::new();
    use figcraft_geom::Shape as _;
    let circle = figcraft_geom::kurbo::Circle::new((65.0, 65.0), 60.0).to_path(0.1);
    let white: vello::peniko::Brush = vello::peniko::Brush::Solid(vello::peniko::Color::new([1.0, 1.0, 1.0, 1.0]));
    scene.fill(vello::peniko::Fill::NonZero, vello::kurbo::Affine::IDENTITY, &white, None, &circle);
    let color = Color { a: 0.25, ..Color::BLACK };
    let n = 60;
    {
        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
        let _ = layers.shadow(&scene, 130, 130, 5.5, color, None);
    }
    let t = std::time::Instant::now();
    {
        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
        for _ in 0..n {
            let _ = layers.shadow(&scene, 130, 130, 5.5, color, None);
        }
    }
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    println!("shadow pass, 130 × 130: {:.3} ms a shadow", t.elapsed().as_secs_f64() * 1000.0 / f64::from(n));
    // The same with only the render (no blur, no tint).
    let t = std::time::Instant::now();
    {
        for _ in 0..n {
            let texture = canvas.render_layer(&device, &queue, &scene, 130, 130).unwrap();
            let _ = &texture;
        }
    }
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    println!("  render only: {:.3} ms", t.elapsed().as_secs_f64() * 1000.0 / f64::from(n));
    // What the pieces cost on their own.
    let make = |w: u32, h: u32| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: crate::canvas::FORMAT,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
    };
    let t = std::time::Instant::now();
    for _ in 0..n {
        let _ = make(130, 130);
    }
    println!("  a texture: {:.3} ms", t.elapsed().as_secs_f64() * 1000.0 / f64::from(n));
    let t = std::time::Instant::now();
    for _ in 0..n {
        let encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        queue.submit([encoder.finish()]);
    }
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    println!("  an empty submit: {:.3} ms", t.elapsed().as_secs_f64() * 1000.0 / f64::from(n));
    let t = std::time::Instant::now();
    for _ in 0..n {
        let encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        queue.submit([encoder.finish()]);
    }
    println!("  an empty submit (unpolled): {:.3} ms", t.elapsed().as_secs_f64() * 1000.0 / f64::from(n));
}

#[test]
#[ignore = "timing: run in release with --ignored --nocapture"]
fn shadow_layer_timings() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    for (label, d) in [
        ("30 ellipse shadows", shadow_layer_cards(30, false)),
        ("30 per-corner shadows", shadow_layer_cards_rounded(30, true)),
        ("30 per-corner cards", shadow_layer_cards_rounded(30, false)),
        ("30 inner shadows", shadow_layer_cards(30, true)),
    ] {
        for (w, h) in [(904u32, 860u32), (2400, 1600)] {
            let p = d.first_page().unwrap();
            let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
            let _ = render_view(&d, p, &view, &Options::default());
            let cpu = frame_ms(|| render_view(&d, p, &view, &Options::default()).is_some(), 3);
            let background = scene::background(&d, p);
            let draw = |canvas: &mut CanvasGpu| {
                let built = {
                    let mut layers = crate::canvas::GpuLayers { canvas, device: &device, queue: &queue };
                    scene::build_with(&d, p, &view, &mut layers)
                };
                let ok = built.is_ok_and(|s| canvas.render(&device, &queue, &s, background).is_some());
                let _ = device.poll(wgpu::PollType::wait_indefinitely());
                ok
            };
            // The first frame: every shadow is made (and its pipelines compile).
            let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
            let t = std::time::Instant::now();
            assert!(draw(&mut canvas));
            let cold = t.elapsed().as_secs_f64() * 1000.0;
            // Then the frame as the app draws it: a drag redraws the page, the shadows are kept.
            let frame = frame_ms(|| draw(&mut canvas), 5);
            println!("{label}, {w} × {h}: CPU {cpu:.2} ms | GPU first frame {cold:.2} ms, then {frame:.2} ms");
        }
    }
}

/// Text with a styled run (its own fill) and text with a stroke.
fn styled_text_doc() -> Document {
    let mut d = Document::new("gpu styled text");
    let p = d.first_page().unwrap();
    let hex = |s: &str| Color::from_hex(s).unwrap_or(Color::WHITE);
    let mut td = TextData::new("plain and coloured words");
    td.style.font_size = 28.0;
    td.runs = vec![figcraft_doc::StyleRun {
        start: 10,
        end: 18,
        style: figcraft_doc::TextStyle { fills: Some(vec![Paint::solid(hex("E03030"))]), ..td.style.clone() },
    }];
    let run = d.create(p, NodeKind::Text(td), "runs", 24.0, 40.0, 460.0, 80.0).unwrap();
    d.get_mut(run).unwrap().fills = vec![Paint::solid(hex("202020"))];

    let mut stroked = TextData::new("Outlined");
    stroked.style.font_size = 44.0;
    let id = d.create(p, NodeKind::Text(stroked), "stroked", 24.0, 150.0, 400.0, 90.0).unwrap();
    {
        let n = d.get_mut(id).unwrap();
        n.fills = vec![Paint::solid(hex("F0C830"))];
        n.strokes = vec![Paint::solid(hex("2040E0"))];
        n.stroke.weight = 3.0;
        n.stroke.align = StrokeAlign::Outside;
    }
    d
}

#[test]
fn the_gpu_draws_styled_text() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = styled_text_doc();
    let p = d.first_page().unwrap();
    assert_eq!(scene::supported(&d, p), Ok(()), "styled runs and stroked text stay on the GPU");
    let (w, h) = PARITY;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let cpu = render_view(&d, p, &view, &Options::default()).unwrap();
    let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
    let built = scene::build_with(&d, p, &view, &mut scene::NoLayers).expect("the GPU draws the text");
    assert!(canvas.render(&device, &queue, &built, scene::background(&d, p)).is_some());
    let gpu = canvas.read(&device, &queue).unwrap();
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    let px = |p: &Pixmap, (x, y): (u32, u32)| {
        let i = ((y as usize * p.width() as usize) + x as usize) * 4;
        p.data().get(i..i + 4).map(<[u8]>::to_vec)
    };
    println!(
        "styled text parity: worst {worst} at {at:?} (CPU {:?}, GPU {:?}), mean {mean:.3}, {over} pixels differ by more than 8",
        px(&cpu, at),
        px(&gpu, at)
    );
    assert!(worst <= 160, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 1.0, "the mean difference is {mean:.3}");
}

/// Masks: a layer clipped by an ellipse above it, a vector mask, and a mask inside a group.
fn mask_doc() -> Document {
    let mut d = Document::new("gpu masks");
    let p = d.first_page().unwrap();
    let hex = |s: &str| Color::from_hex(s).unwrap_or(Color::WHITE);
    let frame = d.create(p, NodeKind::Frame, "masked", 20.0, 20.0, 300.0, 200.0).unwrap();
    d.get_mut(frame).unwrap().fills = vec![Paint::solid(hex("FFFFFF"))];
    let mut card = |d: &mut Document, parent, name: &str, x: f64, y: f64, w: f64, h: f64, color: &str| {
        let id = d.create(parent, NodeKind::Rectangle, name, x, y, w, h).unwrap();
        d.get_mut(id).unwrap().fills = vec![Paint::solid(Color::from_hex(color).unwrap_or(Color::WHITE))];
        d.get_mut(id).unwrap().corners.radii = [12.0; 4];
        id
    };
    card(&mut d, frame, "under", 0.0, 0.0, 300.0, 200.0, "E8F0FF");
    let mask = d.create(frame, NodeKind::Ellipse { start: 0.0, sweep: std::f64::consts::TAU, inner: 0.0 }, "mask", 40.0, 20.0, 220.0, 160.0).unwrap();
    d.get_mut(mask).unwrap().fills = vec![Paint::solid(hex("FFFFFF"))];
    d.get_mut(mask).unwrap().is_mask = true;
    card(&mut d, frame, "over", 0.0, 0.0, 300.0, 200.0, "F06030");

    // A vector mask: the outline of the node clips what is above it.
    let group = d.create(p, NodeKind::Group, "vector masked", 360.0, 20.0, 220.0, 160.0).unwrap();
    card(&mut d, group, "vector content", 0.0, 0.0, 220.0, 160.0, "30C080");
    let vector = d.create(group, NodeKind::Rectangle, "vector mask", 20.0, 20.0, 180.0, 120.0).unwrap();
    {
        let n = d.get_mut(vector).unwrap();
        n.fills = vec![Paint::solid(hex("FFFFFF"))];
        n.corners.radii = [30.0; 4];
        n.is_mask = true;
        n.mask_type = figcraft_doc::MaskType::Vector;
    }
    card(&mut d, group, "vector over", 0.0, 0.0, 220.0, 160.0, "2050C0");

    // A mask inside a group, masking a card above it.
    let inner = d.create(p, NodeKind::Group, "nested", 360.0, 220.0, 220.0, 160.0).unwrap();
    card(&mut d, inner, "nested content", 0.0, 0.0, 220.0, 160.0, "FFD040");
    let star = d.create(inner, NodeKind::Rectangle, "nested mask", 20.0, 20.0, 160.0, 120.0).unwrap();
    {
        let n = d.get_mut(star).unwrap();
        n.fills = vec![Paint::solid(hex("FFFFFF"))];
        n.corners.radii = [60.0; 4];
        n.is_mask = true;
    }
    card(&mut d, inner, "nested over", 0.0, 0.0, 220.0, 160.0, "8040E0");
    d
}

#[test]
fn the_gpu_draws_masks() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = mask_doc();
    let p = d.first_page().unwrap();
    assert_eq!(scene::supported(&d, p), Ok(()), "masks stay on the GPU");
    let (w, h) = PARITY;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let cpu = render_view(&d, p, &view, &Options::default()).unwrap();
    let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
    let built = {
        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
        scene::build_with(&d, p, &view, &mut layers)
    }
    .expect("the GPU draws the masks");
    assert!(canvas.render(&device, &queue, &built, scene::background(&d, p)).is_some());
    let gpu = canvas.read(&device, &queue).unwrap();
    let (worst, mean, at, over) = diff(&cpu, &gpu);
    let px = |p: &Pixmap, (x, y): (u32, u32)| {
        let i = ((y as usize * p.width() as usize) + x as usize) * 4;
        p.data().get(i..i + 4).map(<[u8]>::to_vec)
    };
    println!(
        "mask parity: worst {worst} at {at:?} (CPU {:?}, GPU {:?}), mean {mean:.3}, {over} pixels differ by more than 8",
        px(&cpu, at),
        px(&gpu, at)
    );
    if std::env::var("FIGCRAFT_REGIONS").is_ok() {
        for (x, y) in [(164u32, 140u32), (30, 30), (170, 120), (460, 100), (470, 300)] {
            println!("  mask {x},{y}: CPU {:?} GPU {:?}", px(&cpu, (x, y)), px(&gpu, (x, y)));
        }
    }
    assert!(worst <= 96, "a channel differs by {worst} (mean {mean:.3})");
    assert!(mean <= 0.6, "the mean difference is {mean:.3}");
}

#[test]
fn the_gpu_canvas_reads_back() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = parity_doc();
    let p = d.first_page().unwrap();
    let (w, h) = PARITY;
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
    let scene = scene::build(&d, p, &view).unwrap();
    let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
    assert_eq!(canvas.size(), (w, h));
    assert!(canvas.render(&device, &queue, &scene, scene::background(&d, p)).is_some());
    let pix = canvas.read(&device, &queue).unwrap();
    assert_eq!((pix.width(), pix.height()), (w, h));
    // The page is opaque and its background is drawn: the corner pixel is the page colour.
    assert_eq!(pix.data().get(3), Some(&255), "the read-back is opaque");
    canvas.resize(&device, 320, 200);
    assert_eq!(canvas.size(), (320, 200));
}

/// A page of `n` rounded, gradient-filled cards at 2400 × 1600: enough geometry to show what the
/// rasterizer costs, without effects.
fn heavy_doc(n: u32) -> Document {
    let mut d = Document::new("gpu frame");
    let p = d.first_page().unwrap();
    let colours = ["E03030", "2040E0", "20C060", "F0C830", "A040E0", "20B0D0"];
    let stops = |a: &str, b: &str| {
        vec![
            figcraft_color::ColorStop { position: 0.0, color: Color::from_hex(a).unwrap_or(Color::WHITE) },
            figcraft_color::ColorStop { position: 1.0, color: Color::from_hex(b).unwrap_or(Color::WHITE) },
        ]
    };
    for i in 0..n {
        let (x, y) = (f64::from(i % 6) * 380.0 + 20.0, f64::from(i / 6) * 150.0 + 20.0);
        let id = d.create(p, NodeKind::Rectangle, "Card", x, y, 340.0, 120.0).unwrap();
        let node = d.get_mut(id).unwrap();
        node.corners.radii = [24.0; 4];
        let g = Gradient {
            kind: GradientKind::Linear,
            stops: stops(colours[i as usize % colours.len()], colours[(i as usize + 3) % colours.len()]),
            from: (0.0, 0.0),
            to: (1.0, 1.0),
            width: (-0.5, 0.5),
        };
        node.fills = vec![Paint::gradient(g)];
    }
    d
}

fn frame_ms(mut f: impl FnMut() -> bool, n: u32) -> f64 {
    let t = std::time::Instant::now();
    for _ in 0..n {
        assert!(f());
    }
    t.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
}

#[test]
#[ignore = "timing: run in release with --ignored --nocapture"]
fn layer_blur_timings() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = blurred_doc();
    let p = d.first_page().unwrap();
    let view = View { origin: (0.0, 0.0), scale: 1.0, width: 1200, height: 900 };
    let mut canvas = CanvasGpu::new(&device, view.width, view.height).unwrap();
    let _ = render_view(&d, p, &view, &Options::default());
    let cpu = frame_ms(|| render_view(&d, p, &view, &Options::default()).is_some(), 5);
    // Warm-up: pipelines, atlas.
    for _ in 0..2 {
        let built = {
            let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
            scene::build_with(&d, p, &view, &mut layers)
        }
        .unwrap();
        let _ = canvas.render(&device, &queue, &built, scene::background(&d, p));
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
    }
    let build = frame_ms(
        || {
            let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
            scene::build_with(&d, p, &view, &mut layers).is_ok()
        },
        5,
    );
    let frame = frame_ms(
        || {
            let built = {
                let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
                scene::build_with(&d, p, &view, &mut layers)
            }
            .unwrap();
            let ok = canvas.render(&device, &queue, &built, scene::background(&d, p)).is_some();
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            ok
        },
        5,
    );
    println!("layer blur, 1200 × 900: CPU {cpu:.2} ms | GPU build {build:.2} ms, frame {frame:.2} ms");
}

/// `n` shadowed cards (opaque, rounded) at `w` × `h`.
fn shadow_cards(n: u32) -> Document {
    shadow_cards_with(n, 0.0)
}

/// `n` shadowed cards whose shadows spread by `spread` document units.
fn shadow_cards_with(n: u32, spread: f64) -> Document {
    let mut d = Document::new("gpu shadows");
    let p = d.first_page().unwrap();
    let colours = ["FFFFFF", "E03030", "2040E0", "20C060", "F0C830", "A040E0"];
    for i in 0..n {
        let (x, y) = (f64::from(i % 6) * 150.0 + 10.0, f64::from(i / 6) * 150.0 + 10.0);
        let id = d.create(p, NodeKind::Rectangle, "Card", x, y, 130.0, 130.0).unwrap();
        let node = d.get_mut(id).unwrap();
        node.corners.radii = [16.0; 4];
        node.fills = vec![Paint::solid(Color::from_hex(colours[i as usize % colours.len()]).unwrap_or(Color::WHITE))];
        node.effects = vec![Effect::drop_shadow()];
        if let Some(figcraft_doc::EffectKind::DropShadow(s)) = node.effects.first_mut().map(|e| &mut e.kind) {
            s.radius = 12.0;
            s.spread = spread;
        }
    }
    d
}

#[test]
#[ignore = "timing: run in release with --ignored --nocapture"]
fn shadow_timings() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    for (spread, label) in [(0.0, "30 shadowed cards"), (6.0, "30 spread shadows")] {
        for (w, h) in [(904u32, 860u32), (2320, 1440)] {
            let d = shadow_cards_with(30, spread);
            let p = d.first_page().unwrap();
            let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
            let _ = render_view(&d, p, &view, &Options::default());
            let cpu = frame_ms(|| render_view(&d, p, &view, &Options::default()).is_some(), 5);
            let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
            let _ = canvas.render(&device, &queue, &scene::build(&d, p, &view).unwrap(), scene::background(&d, p));
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            let frame = frame_ms(
                || {
                    let s = scene::build(&d, p, &view).unwrap();
                    let ok = canvas.render(&device, &queue, &s, scene::background(&d, p)).is_some();
                    let _ = device.poll(wgpu::PollType::wait_indefinitely());
                    ok
                },
                8,
            );
            println!("{label}, {w} × {h}: CPU {cpu:.2} ms | GPU frame {frame:.2} ms");
        }
    }
}

/// The page the owner drags in: thirty shadowed cards, three glass panes, a background-blurred
/// card and a layer-blurred one.
fn heavy_effects_doc() -> Document {
    let mut d = shadow_cards(30);
    let p = d.first_page().unwrap();
    for (x, y, w, h) in [(80.0, 80.0, 300.0, 220.0), (360.0, 260.0, 300.0, 220.0), (600.0, 480.0, 280.0, 200.0)] {
        let id = d.create(p, NodeKind::Rectangle, "Glass Pane", x, y, w, h).unwrap();
        let n = d.get_mut(id).unwrap();
        n.corners.radii = [36.0; 4];
        n.fills = vec![Paint::solid(Color { a: 0.08, ..Color::WHITE })];
        n.effects = vec![Effect { kind: EffectKind::Glass(Glass::default()), visible: true }];
    }
    let card = d.create(p, NodeKind::Rectangle, "Blurred card", 200.0, 560.0, 260.0, 160.0).unwrap();
    d.get_mut(card).unwrap().effects = vec![Effect::background_blur()];
    let card2 = d.create(p, NodeKind::Rectangle, "Layer blur", 520.0, 640.0, 220.0, 140.0).unwrap();
    d.get_mut(card2).unwrap().effects = vec![Effect::layer_blur()];
    d
}

#[test]
#[ignore = "timing: run in release with --ignored --nocapture"]
fn heavy_scene_gpu_timings() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    for (name, d) in [("one glass pane", glass_doc()), ("one background blur", backdrop_doc())] {
        let p = d.first_page().unwrap();
        let view = View { origin: (0.0, 0.0), scale: 1.0, width: 904, height: 860 };
        let _ = render_view(&d, p, &view, &Options::default());
        let cpu = frame_ms(|| render_view(&d, p, &view, &Options::default()).is_some(), 5);
        let mut canvas = CanvasGpu::new(&device, view.width, view.height).unwrap();
        let frame = frame_ms(
            || {
                let built = {
                    let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
                    scene::build_with(&d, p, &view, &mut layers)
                };
                let ok = built.is_ok_and(|s| canvas.render(&device, &queue, &s, scene::background(&d, p)).is_some());
                let _ = device.poll(wgpu::PollType::wait_indefinitely());
                ok
            },
            5,
        );
        println!("{name}, 904 × 860: CPU {cpu:.2} ms | GPU frame {frame:.2} ms");
        // Pipelined, as the app draws: submit frame after frame, wait only for the last.
        let t = std::time::Instant::now();
        for _ in 0..8 {
            let built = {
                let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
                scene::build_with(&d, p, &view, &mut layers)
            };
            if let Ok(s) = built {
                let _ = canvas.render(&device, &queue, &s, scene::background(&d, p));
            }
        }
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        println!("{name}: pipelined {:.2} ms a frame", t.elapsed().as_secs_f64() * 1000.0 / 8.0);
    }
    for (w, h) in [(904u32, 860u32), (2400, 1600)] {
        let d = heavy_effects_doc();
        let p = d.first_page().unwrap();
        let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
        let _ = render_view(&d, p, &view, &Options::default());
        let cpu = frame_ms(|| render_view(&d, p, &view, &Options::default()).is_some(), 5);
        let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
        let build = frame_ms(
            || {
                let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
                scene::build_with(&d, p, &view, &mut layers).is_ok()
            },
            5,
        );
        let frame = frame_ms(
            || {
                let built = {
                    let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
                    scene::build_with(&d, p, &view, &mut layers)
                };
                let ok = built.is_ok_and(|s| canvas.render(&device, &queue, &s, scene::background(&d, p)).is_some());
                let _ = device.poll(wgpu::PollType::wait_indefinitely());
                ok
            },
            5,
        );
        println!("heavy scene {w} × {h}: CPU {cpu:.2} ms | GPU build {build:.2} ms, frame {frame:.2} ms");
    }
}

/// The segmented renderer (pages with backdrop effects) against the CPU, pipelined as the app
/// draws: frames submitted back to back, waiting only for the last.
#[test]
#[ignore = "timing: run in release with --ignored --nocapture"]
fn segmented_timings() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let docs = [("one glass pane", glass_doc()), ("one background blur", backdrop_doc()), ("heavy", heavy_effects_doc())];
    for (name, d) in docs {
        // The second view is the app's canvas after View ▸ Zoom to fit on the heavy page: a
        // 904 × 860 canvas at 34.6%, the whole page in view.
        for (w, h, scale, origin) in
            [(904u32, 860u32, 1.0, (0.0, 0.0)), (2400, 1600, 1.0, (0.0, 0.0)), (904, 860, 0.3464285714285714, (-164.6, -841.2))]
        {
            let p = d.first_page().unwrap();
            let view = View { origin, scale, width: w, height: h };
            let Ok(plan) = scene::plan(&d, p) else { continue };
            let _ = render_view(&d, p, &view, &Options::default());
            let cpu = frame_ms(|| render_view(&d, p, &view, &Options::default()).is_some(), 3);
            let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
            let background = scene::background(&d, p);
            // Warm-up: the pipelines, the glass geometry.
            for _ in 0..2 {
                let _ = crate::canvas::segmented_frame(&device, &queue, &mut canvas, &d, p, &view, &plan, background);
            }
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            let t = std::time::Instant::now();
            for _ in 0..6 {
                assert!(crate::canvas::segmented_frame(&device, &queue, &mut canvas, &d, p, &view, &plan, background).is_some());
            }
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            let frame = t.elapsed().as_secs_f64() * 1000.0 / 6.0;
            println!("segmented {name} {w} × {h} at {:.0}%: CPU {cpu:.2} ms | GPU {frame:.2} ms a frame (pipelined)", scale * 100.0);
        }
    }
}

/// Where a segmented frame's time goes: encoding each segment (vello's CPU side), the composes,
/// the effects, and the present.
#[test]
#[ignore = "timing: run in release with --ignored --nocapture"]
fn segmented_breakdown() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    let d = heavy_effects_doc();
    let p = d.first_page().unwrap();
    let view = View { origin: (-164.6, -841.2), scale: 0.3464285714285714, width: 904, height: 860 };
    let plan = scene::plan(&d, p).unwrap();
    let mut canvas = CanvasGpu::new(&device, view.width, view.height).unwrap();
    let background = scene::background(&d, p);
    let mut t_draw = 0.0f64;
    let mut t_effect = 0.0f64;
    let mut frames = 0u32;
    let mut segments = 0u32;
    let mut t_all = 0.0f64;
    // The same steps as `canvas::segmented_frame`, with a clock around each part.
    for _ in 0..6 {
        let frame_start = std::time::Instant::now();
        let (acc, acc_view) = canvas.page_layer(&device).unwrap();
        canvas.fill_layer(&device, &queue, &acc_view, background);
        let mut pending = vello::Scene::new();
        let mut dirty = false;
        let mut over = |canvas: &mut CanvasGpu, pending: &vello::Scene| {
            let segment_view = canvas.segment_layer(&device).unwrap();
            assert!(canvas.draw_into(&device, &queue, pending, &segment_view));
            canvas.compose_over(&device, &queue, &acc_view, &segment_view);
            segments += 1;
        };
        for step in &plan {
            match step {
                scene::Step::Nodes(ids) => {
                    let t = std::time::Instant::now();
                    {
                        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
                        scene::draw_into(&d, p, &view, &mut layers, ids, &mut pending).unwrap();
                    }
                    t_draw += t.elapsed().as_secs_f64() * 1000.0;
                    dirty = true;
                }
                scene::Step::Backdrop(id) => {
                    if dirty {
                        over(&mut canvas, &pending);
                        pending = vello::Scene::new();
                    }
                    let t = std::time::Instant::now();
                    let image = {
                        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
                        layers.backdrop_effect(&d, *id, &view, &acc)
                    };
                    t_effect += t.elapsed().as_secs_f64() * 1000.0;
                    let image = image.unwrap();
                    {
                        let mut layers = crate::canvas::GpuLayers { canvas: &mut canvas, device: &device, queue: &queue };
                        scene::draw_backdrop_into(&d, p, &view, &mut layers, *id, &image, &mut pending).unwrap();
                    }
                    dirty = true;
                }
            }
        }
        if dirty {
            over(&mut canvas, &pending);
        }
        canvas.present_layer(&device, &queue, &acc);
        t_all += frame_start.elapsed().as_secs_f64() * 1000.0;
        frames += 1;
    }
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    println!(
        "segmented breakdown, {} segments a frame: {:.2} ms (encoding {:.2}, effects {:.2}, segments and composes {:.2})",
        segments / frames,
        t_all / f64::from(frames),
        t_draw / f64::from(frames),
        t_effect / f64::from(frames),
        (t_all - t_draw - t_effect) / f64::from(frames)
    );
}

#[test]
#[ignore = "timing: run in release with --ignored --nocapture"]
fn canvas_frame_timings() {
    let Some((device, queue)) = device() else {
        eprintln!("no GPU adapter: skipping");
        return;
    };
    for (w, h) in [(2320u32, 1440u32), (1160, 720)] {
        for (name, cards) in [("one card", 1u32), ("360 cards", 360)] {
            let d = heavy_doc(cards);
            let p = d.first_page().unwrap();
            let view = View { origin: (0.0, 0.0), scale: 1.0, width: w, height: h };
            let bg = scene::background(&d, p);
            let _ = render_view(&d, p, &view, &Options::default());
            let cpu = frame_ms(|| render_view(&d, p, &view, &Options::default()).is_some(), 5);
            let mut canvas = CanvasGpu::new(&device, w, h).unwrap();
            let built = scene::build(&d, p, &view).unwrap();
            // Warm-up: vello compiles its pipelines on the first render.
            let _ = canvas.render(&device, &queue, &built, bg);
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            let build = frame_ms(
                || {
                    let _ = scene::build(&d, p, &view);
                    true
                },
                8,
            );
            // A frame as the canvas draws it: build, draw, and wait for the result (what the
            // next frame's overlay and any screenshot see).
            let frame = frame_ms(
                || {
                    let s = scene::build(&d, p, &view).unwrap();
                    let ok = canvas.render(&device, &queue, &s, bg).is_some();
                    let _ = device.poll(wgpu::PollType::wait_indefinitely());
                    ok
                },
                8,
            );
            // Pipelined, as the app draws: submit frame after frame, wait only for the last.
            let t = std::time::Instant::now();
            for _ in 0..8 {
                let s = scene::build(&d, p, &view).unwrap();
                let _ = canvas.render(&device, &queue, &s, bg);
            }
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            let pipelined = t.elapsed().as_secs_f64() * 1000.0 / 8.0;
            let read = frame_ms(|| canvas.read(&device, &queue).is_some(), 3);
            println!(
                "{w} × {h}, {name:>9}: CPU {cpu:5.2} ms | GPU frame {frame:5.2} ms (pipelined {pipelined:5.2}, \
                 scene {build:5.2}), read-back {read:5.2}"
            );
        }
    }
}
