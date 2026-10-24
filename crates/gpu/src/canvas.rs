//! The GPU canvas: a vello scene drawn into one of two textures (double buffered, so the window
//! can paint the last frame while the next one is drawn) and the read-back that screenshots and
//! the parity tests use.

use std::sync::{Arc, Mutex, OnceLock};

use figcraft_doc::{Document, NodeId};
use figcraft_render::{Pixmap, View};
use vello::peniko::{Blob, ImageAlphaType, ImageData, ImageFormat};
use vello::{AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene};
use wgpu::{Device, Queue, Texture, TextureFormat, TextureUsages, TextureView};

use crate::blur::Blur;

/// vello draws into `Rgba8Unorm` (premultiplied, sRGB-encoded bytes) — what tiny-skia's pixmap
/// holds, and what egui can show without a conversion.
pub const FORMAT: TextureFormat = TextureFormat::Rgba8Unorm;

/// A canvas-sized vello target on the window's device.
pub struct CanvasGpu {
    renderer: Renderer,
    /// The layer blur (the effects' own pipeline default).
    blur: Blur,
    targets: [Target; 2],
    /// The target the next frame is drawn into.
    next: usize,
    /// The target the last frame was drawn into.
    last: usize,
    /// Bumped when the targets are reallocated: the window registers new textures for a new
    /// generation and frees the old ones.
    generation: u64,
}

struct Target {
    texture: Texture,
    view: TextureView,
    size: (u32, u32),
}

impl CanvasGpu {
    /// The canvas of `width` × `height` device pixels, or `None` when vello can't run on `device`.
    pub fn new(device: &Device, width: u32, height: u32) -> Option<Self> {
        let width = width.clamp(1, 16_384);
        let height = height.clamp(1, 16_384);
        let renderer = Renderer::new(
            device,
            RendererOptions {
                use_cpu: false,
                // The canvas uses area anti-aliasing only: fewer pipelines to compile at start-up.
                antialiasing_support: AaSupport { area: true, msaa8: false, msaa16: false },
                num_init_threads: None,
                pipeline_cache: None,
            },
        )
        .ok()?;
        let blur = Blur::new(device)?;
        Some(Self { renderer, blur, targets: [target(device, width, height), target(device, width, height)], next: 0, last: 0, generation: 1 })
    }

    /// The size in device pixels.
    pub fn size(&self) -> (u32, u32) {
        self.targets.first().map_or((1, 1), |t| t.size)
    }

    /// The last frame's texture (egui registers this view once per generation).
    pub fn view(&self) -> Option<&TextureView> {
        self.targets.get(self.last).map(|t| &t.view)
    }

    /// Which buffer the last frame is in.
    pub fn slot(&self) -> u8 {
        self.last as u8
    }

    /// The generation of the textures (changes when they are reallocated).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Make the canvas `width` × `height` device pixels (reallocates on a resize).
    pub fn resize(&mut self, device: &Device, width: u32, height: u32) {
        let width = width.clamp(1, 16_384);
        let height = height.clamp(1, 16_384);
        if (width, height) == self.size() {
            return;
        }
        self.targets = [target(device, width, height), target(device, width, height)];
        self.next = 0;
        self.last = 0;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Draw `scene` over `background` into the next target; returns its texture view.
    pub fn render(&mut self, device: &Device, queue: &Queue, scene: &Scene, background: figcraft_color::Color) -> Option<TextureView> {
        let (width, height) = self.size();
        let params = RenderParams { base_color: crate::scene::pcolor(background), width, height, antialiasing_method: AaConfig::Area };
        let count = self.targets.len();
        let target = self.targets.get_mut(self.next)?;
        match self.renderer.render_to_texture(device, queue, scene, &target.view, &params) {
            Ok(()) => {
                let view = target.view.clone();
                self.last = self.next;
                self.next = (self.next + 1) % count;
                Some(view)
            }
            Err(e) => {
                log::warn!("the GPU canvas couldn't draw: {e}");
                None
            }
        }
    }

    /// The last frame's pixels, premultiplied (screenshots, exports and the parity tests).
    pub fn read(&self, device: &Device, queue: &Queue) -> Option<Pixmap> {
        let target = self.targets.get(self.last)?;
        read(device, queue, &target.texture, target.size)
    }

    /// Draw `scene` into a new, transparent `width` × `height` texture (an effect layer).
    pub(crate) fn render_layer(&mut self, device: &Device, queue: &Queue, scene: &Scene, width: u32, height: u32) -> Option<Texture> {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("figcraft layer"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: TextureUsages::STORAGE_BINDING | TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_SRC | TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let params = RenderParams { base_color: vello::peniko::Color::new([0.0; 4]), width, height, antialiasing_method: AaConfig::Area };
        self.renderer.render_to_texture(device, queue, scene, &view, &params).ok()?;
        Some(texture)
    }

    /// Hand `texture` to vello as an image (copied into its atlas before the next render).
    pub(crate) fn register_layer(&mut self, texture: Texture) -> ImageData {
        let data = blank_image(texture.width(), texture.height());
        let _ = self.renderer.override_image(
            &data,
            Some(wgpu::TexelCopyTextureInfoBase { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All }),
        );
        data
    }
}

/// An image with no bytes of its own: vello reads a registered texture instead.
pub(crate) fn blank_image(width: u32, height: u32) -> ImageData {
    ImageData { data: Blob::new(std::sync::Arc::new(&[])), format: ImageFormat::Rgba8, alpha_type: ImageAlphaType::AlphaPremultiplied, width, height }
}

/// The GPU's effect layers: a node's content rendered offscreen and blurred without leaving the
/// device (see [`crate::scene::Layers`]).
pub(crate) struct GpuLayers<'a> {
    pub canvas: &'a mut CanvasGpu,
    pub device: &'a Device,
    pub queue: &'a Queue,
}

impl crate::scene::Layers for GpuLayers<'_> {
    fn blurred(&mut self, scene: &Scene, width: u32, height: u32, sigma: f64) -> Option<ImageData> {
        let texture = self.canvas.render_layer(self.device, self.queue, scene, width, height)?;
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let blurred = crate::blur::blur_texture(self.device, self.queue, &self.canvas.blur, &view, (width, height), sigma)?;
        Some(self.canvas.register_layer(blurred))
    }
}

fn target(device: &Device, width: u32, height: u32) -> Target {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("figcraft canvas"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        // vello writes through storage textures; egui samples the result, and screenshots copy it.
        usage: TextureUsages::STORAGE_BINDING | TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Target { texture, view, size: (width, height) }
}

fn read(device: &Device, queue: &Queue, texture: &Texture, (width, height): (u32, u32)) -> Option<Pixmap> {
    let unpadded = width.checked_mul(4)?;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align).checked_mul(align)?;
    let size = u64::from(padded).checked_mul(u64::from(height))?;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("figcraft canvas read-back"),
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("figcraft canvas read-back") });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(height) },
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    rx.recv().ok()?.ok()?;
    let mapped = slice.get_mapped_range().ok()?;
    let mut pix = Pixmap::new(width, height)?;
    let row = unpadded as usize;
    let dst = pix.data_mut();
    for y in 0..height as usize {
        let src = mapped.get(y * padded as usize..y * padded as usize + row)?;
        let out = dst.get_mut(y * row..(y + 1) * row)?;
        out.copy_from_slice(src);
    }
    drop(mapped);
    buffer.unmap();
    Some(pix)
}

/// One frame drawn on the GPU, on its way to the window.
#[derive(Clone)]
pub struct Frame {
    /// The view the texture shows.
    pub view: View,
    /// The texture it is in (one of the two buffers).
    pub texture: TextureView,
    /// Which buffer, and which generation of them.
    pub slot: u8,
    pub generation: u64,
}

/// The canvas the window shares with its render thread: the device, queue and the two buffers
/// (built on the first frame drawn, off the UI thread).
struct Backend {
    device: Arc<Device>,
    queue: Arc<Queue>,
    canvas: Mutex<Option<CanvasGpu>>,
}

fn backend_slot() -> &'static OnceLock<Option<Arc<Backend>>> {
    static BACKEND: OnceLock<Option<Arc<Backend>>> = OnceLock::new();
    &BACKEND
}

fn backend() -> Option<&'static Arc<Backend>> {
    backend_slot().get().and_then(Option::as_ref)
}

/// Give the canvas the window's GPU (once; later calls do nothing). Returns whether the GPU
/// canvas is available.
pub fn install(device: Arc<Device>, queue: Arc<Queue>) -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        // Building the renderer (its pipelines compile) happens on the first frame drawn, on the
        // render thread: a window still setting up its surface must not wait for it.
        backend_slot().set(Some(Arc::new(Backend { device, queue, canvas: Mutex::new(None) }))).is_ok()
    })
}

/// Why the last [`present`] left the frame to the CPU; `None` when it drew it.
fn refusal_slot() -> &'static Mutex<Option<&'static str>> {
    static LAST: Mutex<Option<&'static str>> = Mutex::new(None);
    &LAST
}

/// Why the last [`present`] left the frame to the CPU (for `ui.inspect`); `None` when it drew it.
pub fn last_refusal() -> Option<&'static str> {
    refusal_slot().lock().ok().and_then(|r| *r)
}

fn refusal(reason: &'static str) -> Option<Frame> {
    if let Ok(mut r) = refusal_slot().lock() {
        *r = Some(reason);
    }
    None
}

/// Draw page `page` of `doc` through `view` on the GPU, when the page is in the scene builder's
/// feature set (see [`crate::scene::supported`]); `None` leaves the frame to the CPU renderer.
pub fn present(doc: &Document, page: NodeId, view: &View) -> Option<Frame> {
    let Some(backend) = backend() else { return refusal("not installed") };
    let background = crate::scene::background(doc, page);
    let Ok(mut slot) = backend.canvas.lock() else { return refusal("canvas is busy") };
    if slot.is_none() {
        let Some(canvas) = CanvasGpu::new(&backend.device, view.width, view.height) else {
            return refusal("the GPU can't run the rasterizer");
        };
        *slot = Some(canvas);
    }
    let Some(canvas) = slot.as_mut() else { return refusal("the GPU can't run the rasterizer") };
    canvas.resize(&backend.device, view.width, view.height);
    // Effect layers are drawn and blurred on the device while the scene is built.
    let scene = {
        let mut layers = GpuLayers { canvas, device: &backend.device, queue: &backend.queue };
        match crate::scene::build_with(doc, page, view, &mut layers) {
            Ok(scene) => scene,
            Err(e) => return refusal(e.reason()),
        }
    };
    let Some(texture) = canvas.render(&backend.device, &backend.queue, &scene, background) else {
        return refusal("draw failed");
    };
    let frame = Frame { view: view.clone(), texture, slot: canvas.slot(), generation: canvas.generation() };
    if let Ok(mut r) = refusal_slot().lock() {
        *r = None;
    }
    Some(frame)
}

/// Whether the GPU canvas is installed (a window with a device built it).
pub fn installed() -> bool {
    backend().is_some()
}
