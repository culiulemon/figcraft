//! The GPU canvas: a vello scene drawn into a texture the window's egui samples, plus the
//! read-back that screenshots and the parity tests use.

use figcraft_render::{Pixmap, View};
use vello::kurbo::Rect as KRect;
use vello::{AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene};
use wgpu::{Device, Queue, Texture, TextureFormat, TextureUsages, TextureView};

/// vello draws into `Rgba8Unorm` (premultiplied, sRGB-encoded bytes) — what tiny-skia's pixmap
/// holds, and what egui can show without a conversion.
pub const FORMAT: TextureFormat = TextureFormat::Rgba8Unorm;

/// A canvas-sized vello target on the window's device.
pub struct CanvasGpu {
    renderer: Renderer,
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
        let (texture, view) = target(device, width, height);
        Some(Self { renderer, texture, view, size: (width, height) })
    }

    /// The size in device pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The texture the scene is drawn into (egui registers this view once per size).
    pub fn view(&self) -> &TextureView {
        &self.view
    }

    /// Make the canvas `width` × `height` device pixels (reallocates on a resize).
    pub fn resize(&mut self, device: &Device, width: u32, height: u32) {
        let width = width.clamp(1, 16_384);
        let height = height.clamp(1, 16_384);
        if (width, height) == self.size {
            return;
        }
        let (texture, view) = target(device, width, height);
        self.texture = texture;
        self.view = view;
        self.size = (width, height);
    }

    /// Draw `scene` over `background` into the canvas texture.
    pub fn render(&mut self, device: &Device, queue: &Queue, scene: &Scene, background: figcraft_color::Color) -> bool {
        let (width, height) = self.size;
        let params = RenderParams { base_color: crate::scene::pcolor(background), width, height, antialiasing_method: AaConfig::Area };
        match self.renderer.render_to_texture(device, queue, scene, &self.view, &params) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("the GPU canvas couldn't draw: {e}");
                false
            }
        }
    }

    /// The canvas pixels, premultiplied (screenshots, exports and the parity tests).
    pub fn read(&self, device: &Device, queue: &Queue) -> Option<Pixmap> {
        let (width, height) = self.size;
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
            wgpu::TexelCopyTextureInfo { texture: &self.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
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
}

fn target(device: &Device, width: u32, height: u32) -> (Texture, TextureView) {
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
    (texture, view)
}

/// The view rectangle a scene is clipped to (its device pixels), for layers.
pub fn view_rect(view: &View) -> KRect {
    KRect::new(0.0, 0.0, f64::from(view.width), f64::from(view.height))
}
