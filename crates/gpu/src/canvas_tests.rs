//! The GPU canvas against the CPU renderer: pixel parity, the fallback predicate, read-back and
//! frame timings (see `docs/gpu-rasterizer.md`).

use std::sync::Arc;

use figcraft_color::{BlendMode, Color, Gradient, GradientKind, Paint};
use figcraft_doc::{Document, Effect, NodeKind, StrokeAlign};
use figcraft_render::{Options, Pixmap, View, render_view};
use wgpu::{Device, Queue};

use crate::canvas::CanvasGpu;
use crate::scene;

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
    if !canvas.render(&device, &queue, &scene, scene::background(d, p)) {
        return None;
    }
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

    // A drop shadow is a GPU pass of a later phase.
    let id = d.node(p).and_then(|n| n.children.first().copied()).unwrap();
    d.get_mut(id).unwrap().effects = vec![Effect::drop_shadow()];
    assert_eq!(scene::supported(&d, p), Err(scene::Unsupported::Effect));
    d.get_mut(id).unwrap().effects.clear();

    // An image fill needs the renderer's texture atlas.
    d.get_mut(id).unwrap().fills = vec![Paint::image("hash", figcraft_color::ImageScaleMode::Fill)];
    assert_eq!(scene::supported(&d, p), Err(scene::Unsupported::Paint));
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
    assert!(canvas.render(&device, &queue, &scene, scene::background(&d, p)));
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
                    let ok = canvas.render(&device, &queue, &s, bg);
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
