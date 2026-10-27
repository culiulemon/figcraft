//! The GPU canvas: a vello scene drawn into one of two textures (double buffered, so the window
//! can paint the last frame while the next one is drawn) and the read-back that screenshots and
//! the parity tests use.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use figcraft_doc::{Document, NodeId};
use figcraft_render::{Pixmap, View};
use vello::peniko::{Blob, ImageAlphaType, ImageData, ImageFormat};
use vello::{AaConfig, AaSupport, RenderParams, Renderer, RendererOptions, Scene};
use wgpu::{Device, Queue, Texture, TextureFormat, TextureUsages, TextureView};

use crate::blur::Blur;
use crate::scene::{Layer, Layers};

/// vello draws into `Rgba8Unorm` (premultiplied, sRGB-encoded bytes) — what tiny-skia's pixmap
/// holds, and what egui can show without a conversion.
pub const FORMAT: TextureFormat = TextureFormat::Rgba8Unorm;

/// A canvas-sized vello target on the window's device.
pub struct CanvasGpu {
    renderer: Renderer,
    /// The layer blur (the effects' own pipeline default).
    blur: Blur,
    /// The glass shader (its geometry is kept here between frames).
    glass: crate::glass::Glass,
    /// The shadow passes: a blurred silhouette tinted, and a layer's alpha grown or shrunk.
    shadow: crate::shadow::Shadow,
    /// The progressive blur's blend of its levels.
    mix: crate::mix::Mix,
    /// Lays a page segment over the accumulation.
    compose: crate::compose::Compose,
    /// Plus-darker onto the page (through a copy of it).
    plus: crate::plus::Plus,
    targets: [Target; 2],
    /// The target the next frame is drawn into.
    next: usize,
    /// The target the last frame was drawn into.
    last: usize,
    /// Bumped when the targets are reallocated: the window registers new textures for a new
    /// generation and frees the old ones.
    generation: u64,
    /// The page being accumulated (kept between frames: a canvas-sized allocation a frame is not
    /// free, and neither is the clear it would need).
    page: Option<Scratch>,
    /// One segment on its way onto the page.
    segment: Option<Scratch>,
    /// A copy of the page, for a pass that reads it as it writes it (plus-darker).
    under: Option<Scratch>,
    /// Layer textures a frame is done with, for the next frame's layers: a drag of a blurred card
    /// would otherwise make and drop a dozen full-size textures a frame, which shows as judder.
    layers_pool: Vec<Texture>,
    /// The plan steps' own segments, with the key of what each holds, kept between frames.
    steps: Vec<Option<(u64, Scratch)>>,
    /// Shadow images kept between frames, by what they were made from: while one layer moves, the
    /// shadows of the others are drawn again as they are instead of being rebuilt.
    shadows: HashMap<(u64, u64), Kept>,
    shadows_bytes: usize,
    /// Page-level nodes' encoded scenes, by their key: a drag re-encodes only what it changed.
    nodes: HashMap<u64, CachedScene>,
    /// Look-ups that found a kept scene (tests: a drag re-encodes only what moved).
    #[cfg(test)]
    node_hits: u64,
    /// Look-ups that found a kept shadow or layer (tests: a drag reuses the blurred layer).
    #[cfg(test)]
    shadow_hits: u64,
    /// Bumped on every look-up and keep: the least recently used shadow or scene goes first.
    tick: u64,
}

/// An encoded page node's scene, kept between frames by what it encodes.
struct CachedScene {
    scene: Arc<Scene>,
    used: u64,
}

/// The most encoded node scenes kept: each is kilobytes, so a page's layers fit many times over.
const MAX_NODE_SCENES: usize = 2048;

/// A kept shadow: its image, the device bytes it takes, and when it was last used.
struct Kept {
    layer: crate::scene::Layer,
    bytes: usize,
    used: u64,
}

/// The most device bytes the kept shadows take (a page of effect layers fits many times over).
const KEPT_SHADOW_BYTES: usize = 64 << 20;

/// The most device bytes the kept step segments take: a canvas-sized texture each, and a segmented
/// page has one per backdrop node (8 at 2400 × 1600).
const STEP_BYTES: u64 = 128 << 20;

struct Target {
    texture: Texture,
    view: TextureView,
    size: (u32, u32),
}

/// A canvas-sized layer kept between frames.
struct Scratch {
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
        let glass = crate::glass::Glass::new(device)?;
        let shadow = crate::shadow::Shadow::new(device)?;
        let mix = crate::mix::Mix::new(device)?;
        let compose = crate::compose::Compose::new(device)?;
        let plus = crate::plus::Plus::new(device)?;
        Some(Self {
            renderer,
            blur,
            glass,
            shadow,
            mix,
            compose,
            plus,
            targets: [target(device, width, height), target(device, width, height)],
            next: 0,
            last: 0,
            generation: 1,
            page: None,
            segment: None,
            under: None,
            layers_pool: Vec::new(),
            steps: Vec::new(),
            shadows: HashMap::new(),
            shadows_bytes: 0,
            nodes: HashMap::new(),
            #[cfg(test)]
            node_hits: 0,
            #[cfg(test)]
            shadow_hits: 0,
            tick: 0,
        })
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
        // A kept step's texture is re-made at the new size: what it held is gone. Node scenes are
        // keyed on the view, so they would miss anyway; drop them rather than hold them.
        self.steps.clear();
        self.nodes.clear();
        self.layers_pool.clear();
    }

    /// A `width` × `height` layer texture: one a frame was done with when its size matches.
    fn take_layer(&mut self, device: &Device, label: &str, width: u32, height: u32) -> Texture {
        if let Some(i) = self.layers_pool.iter().position(|t| t.width() == width && t.height() == height) {
            return self.layers_pool.swap_remove(i);
        }
        layer_texture(device, label, width, height)
    }

    /// Keep `texture` for a later frame's layers (its passes are done: a queue runs in order). The
    /// oldest go first when the pool is full: clearing it would make the next frame allocate them
    /// all again.
    fn give_layer(&mut self, texture: Texture) {
        const POOL_BYTES: u64 = 128 << 20;
        let bytes = |t: &Texture| u64::from(t.width()) * u64::from(t.height()) * 4;
        let mut used: u64 = self.layers_pool.iter().map(bytes).sum();
        let want = bytes(&texture);
        while used.saturating_add(want) > POOL_BYTES && !self.layers_pool.is_empty() {
            let old = self.layers_pool.remove(0);
            used = used.saturating_sub(bytes(&old));
        }
        if used.saturating_add(want) <= POOL_BYTES {
            self.layers_pool.push(texture);
        }
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

    /// The accumulation texture (kept between frames). Its pixels are all written each frame — the
    /// background fill, then every segment vello draws — so it needs no clear of its own.
    pub(crate) fn page_layer(&mut self, device: &Device) -> Option<(Texture, TextureView)> {
        let (w, h) = self.size();
        keep(&mut self.page, device, (w, h), "figcraft page")
    }

    /// The segment texture (kept between frames): one segment is drawn into it and laid over the
    /// page before the next one. It is sampled by the compose pass and written after it, and the
    /// queue runs the two in order, so one is enough.
    pub(crate) fn segment_layer(&mut self, device: &Device) -> Option<TextureView> {
        let (w, h) = self.size();
        keep(&mut self.segment, device, (w, h), "figcraft segment").map(|(_, view)| view)
    }

    /// The segment texture of plan step `i`, kept with the key of what it holds: a step that did
    /// not change is laid over the page again instead of being drawn again.
    pub(crate) fn step_kept(&self, i: usize, key: u64) -> Option<TextureView> {
        self.steps.get(i)?.as_ref().filter(|(kept, _)| *kept == key).map(|(_, s)| s.view.clone())
    }

    /// The step segment for plan step `i`, made or resized as needed; `None` when the page has
    /// more steps than the kept segments afford, and the step is drawn into the shared one.
    pub(crate) fn step_layer(&mut self, device: &Device, i: usize) -> Option<TextureView> {
        if i >= self.step_slots() {
            return None;
        }
        let (w, h) = self.size();
        if self.steps.len() <= i {
            self.steps.resize_with(i + 1, || None);
        }
        let slot = self.steps.get_mut(i)?;
        let stale = slot.as_ref().is_none_or(|(_, s)| s.size != (w, h));
        if stale {
            let texture = layer_texture(device, "figcraft step", w, h);
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            *slot = Some((0, Scratch { texture, view, size: (w, h) }));
        }
        slot.as_ref().map(|(_, s)| s.view.clone())
    }

    /// Mark what step `i`'s segment holds, once the step has drawn into it.
    pub(crate) fn keep_step(&mut self, i: usize, key: u64) {
        if let Some(Some((kept, _))) = self.steps.get_mut(i) {
            *kept = key;
        }
    }

    /// How many step segments are kept: as many canvas-sized textures as [`STEP_BYTES`] affords
    /// (a page has one per backdrop node, and a handful more).
    fn step_slots(&self) -> usize {
        let (w, h) = self.size();
        let bytes = (u64::from(w) * u64::from(h) * 4).max(1);
        usize::try_from((STEP_BYTES / bytes).clamp(1, 16)).unwrap_or(1)
    }

    /// Fill a layer with `color` (the page background under the accumulation).
    pub(crate) fn fill_layer(&self, device: &Device, queue: &Queue, view: &TextureView, color: figcraft_color::Color) {
        let c = color.sanitized();
        crate::compose::clear(device, queue, view, wgpu::Color { r: f64::from(c.r), g: f64::from(c.g), b: f64::from(c.b), a: f64::from(c.a) });
    }

    /// Draw `scene` into `view` over a transparent ground (a page segment).
    pub(crate) fn draw_into(&mut self, device: &Device, queue: &Queue, scene: &Scene, view: &TextureView) -> bool {
        let (width, height) = self.size();
        let params = RenderParams { base_color: vello::peniko::Color::new([0.0; 4]), width, height, antialiasing_method: AaConfig::Area };
        self.renderer.render_to_texture(device, queue, scene, view, &params).is_ok()
    }

    /// Lay `src` over `dst` (both canvas-sized).
    pub(crate) fn compose_over(&self, device: &Device, queue: &Queue, dst: &TextureView, src: &TextureView) {
        self.compose.over(device, queue, dst, src);
    }

    /// `src` (a step's pixels, straight alpha from vello) plus-darker'd onto the page — the CPU's
    /// `fx::plus_darker`, through a copy of the page (a pass can't read what it writes).
    pub(crate) fn plus_darker(&mut self, device: &Device, queue: &Queue, page: &Texture, page_view: &TextureView, src: &TextureView) -> Option<()> {
        let (w, h) = self.size();
        let (under, _) = keep(&mut self.under, device, (w, h), "figcraft under")?;
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("figcraft under copy") });
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo { texture: page, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyTextureInfo { texture: &under, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        let under_view = under.create_view(&wgpu::TextureViewDescriptor::default());
        self.plus.run(device, queue, page_view, &under_view, src);
        Some(())
    }

    /// Put the accumulated page on screen (into the next buffer).
    pub(crate) fn present_layer(&mut self, device: &Device, queue: &Queue, src: &Texture) -> Option<TextureView> {
        let index = self.next;
        let target = self.targets.get(index)?;
        if src.width() != target.size.0 || src.height() != target.size.1 {
            return None;
        }
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("figcraft present") });
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo { texture: src, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyTextureInfo { texture: &target.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::Extent3d { width: target.size.0, height: target.size.1, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        let view = target.view.clone();
        self.last = index;
        self.next = (index + 1) % self.targets.len();
        Some(view)
    }

    /// Draw `scene` into a new, transparent `width` × `height` layer (an effect layer). The pixels
    /// come out premultiplied, as the blurs, the glass shader and vello's own image brush read them
    /// (vello's targets hold straight alpha, so the render is premultiplied before it is handed on).
    pub(crate) fn render_layer(&mut self, device: &Device, queue: &Queue, scene: &Scene, width: u32, height: u32) -> Option<Texture> {
        let texture = self.take_layer(device, "figcraft layer", width, height);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let params = RenderParams { base_color: vello::peniko::Color::new([0.0; 4]), width, height, antialiasing_method: AaConfig::Area };
        self.renderer.render_to_texture(device, queue, scene, &view, &params).ok()?;
        let out = self.take_layer(device, "figcraft layer", width, height);
        let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
        self.compose.premultiply(device, queue, &view, &out_view);
        self.give_layer(texture);
        Some(out)
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

    /// `texture` as a layer: its image for the scene, and the texture the passes read.
    pub(crate) fn as_layer(&mut self, texture: Texture) -> crate::scene::Layer {
        let size = (texture.width(), texture.height());
        let image = self.register_layer(texture.clone());
        crate::scene::Layer { image, texture: Some(texture), size }
    }

    /// A shadow kept from an earlier frame, if it was made from the same thing.
    pub(crate) fn kept_shadow(&mut self, key: (u64, u64)) -> Option<crate::scene::Layer> {
        self.tick += 1;
        let tick = self.tick;
        let kept = self.shadows.get_mut(&key)?;
        kept.used = tick;
        let layer = kept.layer.clone();
        self.renderer.mark_override_image_dirty(&layer.image);
        #[cfg(test)]
        {
            self.shadow_hits += 1;
        }
        Some(layer)
    }

    /// Look-ups that found a kept shadow or layer (tests).
    #[cfg(test)]
    pub(crate) fn kept_shadow_hits(&self) -> u64 {
        self.shadow_hits
    }

    /// Keep `layer` as the shadow made from `key`, dropping the least recently used while the
    /// cache is over its budget.
    pub(crate) fn keep_shadow(&mut self, key: (u64, u64), layer: crate::scene::Layer, bytes: usize) {
        if bytes > KEPT_SHADOW_BYTES {
            return;
        }
        self.tick += 1;
        let tick = self.tick;
        if let Some(old) = self.shadows.remove(&key) {
            self.shadows_bytes = self.shadows_bytes.saturating_sub(old.bytes);
        }
        while self.shadows_bytes + bytes > KEPT_SHADOW_BYTES {
            let Some(oldest) = self.shadows.iter().min_by_key(|(_, k)| k.used).map(|(k, _)| *k) else { break };
            if let Some(old) = self.shadows.remove(&oldest) {
                self.shadows_bytes = self.shadows_bytes.saturating_sub(old.bytes);
            }
        }
        self.shadows_bytes += bytes;
        self.shadows.insert(key, Kept { layer, bytes, used: tick });
    }

    /// `content` (a scene) cut by `mask`'s luminance × alpha into a `width` × `height` layer (a
    /// luminance mask, as the CPU's `MaskType::Luminance`), kept under `key` when made before.
    pub(crate) fn luminance_masked(
        &mut self,
        device: &Device,
        queue: &Queue,
        content: &Scene,
        mask: &crate::scene::Layer,
        width: u32,
        height: u32,
        key: Option<(u64, u64)>,
    ) -> Option<crate::scene::Layer> {
        if let Some(key) = key
            && let Some(hit) = self.kept_shadow(key)
        {
            return Some(hit);
        }
        let mask_texture = mask.texture.clone()?;
        let content_texture = self.render_layer(device, queue, content, width, height)?;
        let content_view = content_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mask_view = mask_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let out = layer_texture(device, "figcraft masked layer", width, height);
        let out_view = out.create_view(&wgpu::TextureViewDescriptor::default());
        self.compose.luminance(device, queue, &out_view, &content_view, &mask_view);
        let layer = self.as_layer(out);
        if let Some(key) = key {
            let bytes = width as usize * height as usize * 4;
            self.keep_shadow(key, layer.clone(), bytes);
        }
        Some(layer)
    }

    /// How many shadows are kept (tests: a drag keeps the ones that did not move).
    #[cfg(test)]
    pub(crate) fn kept_shadows(&self) -> usize {
        self.shadows.len()
    }

    /// A page-level node's encoded scene from an earlier frame, if `key` still holds what it
    /// encodes (its subtree, the view and the zoom).
    pub(crate) fn node_scene(&mut self, key: u64) -> Option<Arc<Scene>> {
        self.tick += 1;
        let tick = self.tick;
        let scene = {
            let kept = self.nodes.get_mut(&key)?;
            kept.used = tick;
            Arc::clone(&kept.scene)
        };
        #[cfg(test)]
        {
            self.node_hits += 1;
        }
        Some(scene)
    }

    /// Keep `scene` as the encoded scene for `key`, dropping the least recently used past the cap.
    pub(crate) fn keep_node_scene(&mut self, key: u64, scene: Arc<Scene>) {
        self.tick += 1;
        let tick = self.tick;
        if !self.nodes.contains_key(&key)
            && self.nodes.len() >= MAX_NODE_SCENES
            && let Some(oldest) = self.nodes.iter().min_by_key(|(_, s)| s.used).map(|(k, _)| *k)
        {
            self.nodes.remove(&oldest);
        }
        self.nodes.insert(key, CachedScene { scene, used: tick });
    }

    /// How many page nodes' scenes are kept (tests: a drag keeps the ones that did not move).
    #[cfg(test)]
    pub(crate) fn kept_node_scenes(&self) -> usize {
        self.nodes.len()
    }

    /// Look-ups that found a kept scene (tests: a drag re-encodes only what moved).
    #[cfg(test)]
    pub(crate) fn node_hits(&self) -> u64 {
        self.node_hits
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
    fn blurred(&mut self, scene: &Scene, width: u32, height: u32, sigma: f64) -> Option<Layer> {
        let texture = self.canvas.render_layer(self.device, self.queue, scene, width, height)?;
        self.blurred_texture(texture, sigma)
    }

    fn backdrop(&mut self, scene: &Scene, width: u32, height: u32, sigma: f64) -> Option<Layer> {
        self.blurred(scene, width, height, sigma)
    }

    fn glass(&mut self, scene: &Scene, (vw, vh): (u32, u32), pane: &figcraft_render::glass::GlassPane, at: (i32, i32)) -> Option<Layer> {
        let back = self.canvas.render_layer(self.device, self.queue, scene, vw, vh)?;
        self.glass_from(&back, pane, at)
    }

    fn backdrop_effect(&mut self, doc: &Document, id: NodeId, view: &View, back: &Texture) -> Option<Layer> {
        match crate::scene::backdrop_of_node(doc, id)? {
            crate::scene::Backdrop::Blur(radius) => {
                // The blur reads the node's padded layer, as the CPU's does — not the whole view.
                let ((x, y), (w, h)) = crate::scene::blur_rect(doc, id, view)?;
                let cropped = crop(self.device, self.queue, back, (x as i32, y as i32), (w, h))?;
                let sigma = radius / 2.0 * view.scale;
                self.blurred_texture(cropped, sigma)
            }
            crate::scene::Backdrop::Glass(_) => {
                let (pane, at) = crate::scene::glass_setup(doc, id, view)?;
                self.glass_from(back, &pane, at)
            }
        }
    }

    fn layer(&mut self, scene: &Scene, width: u32, height: u32, key: Option<(u64, u64)>) -> Option<Layer> {
        if let Some(key) = key
            && let Some(hit) = self.canvas.kept_shadow(key)
        {
            return Some(hit);
        }
        let texture = self.canvas.render_layer(self.device, self.queue, scene, width, height)?;
        let layer = self.canvas.as_layer(texture);
        self.keep(key, &layer);
        Some(layer)
    }

    fn blur_layer(&mut self, layer: &Layer, sigma: f64, key: Option<(u64, u64)>) -> Option<Layer> {
        if let Some(key) = key
            && let Some(hit) = self.canvas.kept_shadow(key)
        {
            return Some(hit);
        }
        let blurred = self.blurred_texture(layer.texture.clone()?, sigma)?;
        self.keep(key, &blurred);
        Some(blurred)
    }

    fn progressive(&mut self, layer: &Layer, s0: f64, s1: f64, a: (f64, f64), b: (f64, f64), key: Option<(u64, u64)>) -> Option<Layer> {
        // What the device pays: the layer (4 bytes a pixel), its levels (each blurred only in its
        // band, so together a few times the layer), and the mix's output (a layer), all in flight
        // at once — 16 M pixels is 64 MB a full-size texture, about 300 MB together, the most a
        // card's blur should ask of a device that also draws the window. Past that the blur stays
        // on the CPU, which has the RAM. 16 M keeps the 0 → 300 blur on a 200 × 300 card on the
        // GPU to 244% zoom; at 300% it is 18 M and leaves the canvas on the CPU.
        const MAX_PIXELS: u64 = 16_000_000;
        if u64::from(layer.size.0).saturating_mul(u64::from(layer.size.1)) > MAX_PIXELS {
            return None;
        }
        if let Some(key) = key
            && let Some(hit) = self.canvas.kept_shadow(key)
        {
            return Some(hit);
        }
        let texture = layer.texture.clone()?;
        let cap = |s: f64| if s.is_finite() { s.clamp(0.0, figcraft_render::fx::MAX_SIGMA) } else { 0.0 };
        let (s0, s1) = (cap(s0), cap(s1));
        // Too flat a ramp is one blur, as the renderer does it.
        if (s1 - s0).abs() < 0.05 {
            let made = self.blurred_texture(texture, s1)?;
            self.keep(key, &made);
            return Some(made);
        }
        let deviations = figcraft_render::blur::levels(s0.min(s1), s0.max(s1));
        let size = layer.size;
        let t0 = std::time::Instant::now();
        // A level is a blur: past this many pixels it is computed at half resolution (a quarter of
        // the work) and the blend reads it upscaled — the detail a box pass averages away anyway —
        // while the layer itself, which the ramp's sharp end shows, stays at full resolution. This
        // is what keeps a layer the size of the window (a card zoomed past the screen) smooth.
        const LEVEL_BUDGET: u64 = 2_000_000;
        let pixels = u64::from(size.0).saturating_mul(u64::from(size.1));
        let level_scale = if pixels > 8_000_000 {
            0.25
        } else if pixels > LEVEL_BUDGET {
            0.5
        } else {
            1.0
        };
        // The levels and their blend reach the device in one submission (with their scratch kept
        // until it lands): a submit a pass made the canvas queue a dozen of them a frame, and in
        // the app each of those waited behind the window's own submits.
        let source = texture.create_view(&wgpu::TextureViewDescriptor::default());
        // The layer at the levels' resolution (themselves), when that is smaller.
        let small = (level_scale < 1.0).then(|| {
            let (sw, sh) = (scale_px(size.0, level_scale), scale_px(size.1, level_scale));
            let out = self.canvas.take_layer(self.device, "figcraft blur small", sw, sh);
            let view = out.create_view(&wgpu::TextureViewDescriptor::default());
            self.canvas.compose.downscale(self.device, self.queue, &view, &source);
            (out, (sw, sh))
        });
        let (read, read_size) = match &small {
            Some((out, size)) => (out.create_view(&wgpu::TextureViewDescriptor::default()), *size),
            None => (source.clone(), size),
        };
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("figcraft progressive blur") });
        let mut levels: Vec<(Texture, (f32, f32), f32)> = Vec::with_capacity(deviations.len());
        let mut after: Vec<Texture> = Vec::new();
        // One running-sums texture for every level: its passes run one after another, and eight of
        // its size would be most of the pool (and of the memory a drag holds).
        let sums = self.canvas.blur.take_sums(self.device, read_size);
        for (k, d) in deviations.iter().enumerate() {
            if !d.is_finite() || *d <= 0.3 {
                // The renderer leaves a blur this small out: the layer itself is the level.
                levels.push((texture.clone(), (0.0, 0.0), 1.0));
            } else {
                // Only the pixels that use this level (plus its blur's reach) are blurred: an
                // 0 → 300 ramp on a 200 × 300 card blurs a few hundred rows a level instead of the
                // whole 1100 × 1200 layer eight times (the renderer crops the same way).
                let reach = (d * 3.0).ceil().clamp(0.0, 8192.0) as u32 + 3;
                let (x, y, w, h) = level_crop(&deviations, k, s0, s1, a, b, size, reach);
                // The same crop of the smaller layer: its texels, its radius, and its position
                // kept in layer pixels for the blend, so the two grids line up.
                let (x0, y0) = (scale_px(x, level_scale), scale_px(y, level_scale));
                let (cw, ch) = (
                    scale_px(w, level_scale).min(read_size.0.saturating_sub(x0)).max(1),
                    scale_px(h, level_scale).min(read_size.1.saturating_sub(y0)).max(1),
                );
                let (level, scratch) = crate::blur::blur_texture_in(
                    self.device,
                    self.queue,
                    &self.canvas.blur,
                    &mut encoder,
                    &read,
                    (x0, y0),
                    (cw, ch),
                    *d * level_scale,
                    sums.as_ref(),
                )?;
                // The crop's corner in layer pixels: the blend reads the level from there.
                let at = (x0 as f32 / level_scale as f32, y0 as f32 / level_scale as f32);
                levels.push((level.clone(), at, level_scale as f32));
                after.push(level);
                after.extend(scratch);
            }
        }
        after.extend(sums);
        let out = self.canvas.mix.encode(self.device, self.queue, &mut encoder, &levels, &deviations, (s0, s1, a, b), size)?;
        let small = small.map(|(texture, _)| texture);
        let encoded = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = std::time::Instant::now();
        let cb = encoder.finish();
        let finish = t1.elapsed().as_secs_f64() * 1000.0;
        let t2 = std::time::Instant::now();
        self.queue.submit([cb]);
        let submit = t2.elapsed().as_secs_f64() * 1000.0;
        if std::env::var("FIGCRAFT_PRESENT_LOG").is_ok() {
            // What the device takes for the chain the submission queued (a stall either way).
            let t3 = std::time::Instant::now();
            let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
            eprintln!(
                "progressive {:?}: {} levels encoded {encoded:.1}, finish {finish:.1}, submit {submit:.1}, device {:.1} ms",
                size,
                levels.len(),
                t3.elapsed().as_secs_f64() * 1000.0
            );
        }
        // The levels and scratch are made again next frame: keep their textures for it.
        for texture in after {
            self.canvas.blur.give_back(texture);
        }
        if let Some(small) = small {
            self.canvas.give_layer(small);
        }
        let made = self.canvas.as_layer(out);
        self.keep(key, &made);
        Some(made)
    }

    fn shadow(&mut self, scene: &Scene, width: u32, height: u32, sigma: f64, color: figcraft_color::Color, key: Option<(u64, u64)>) -> Option<Layer> {
        let texture = self.canvas.render_layer(self.device, self.queue, scene, width, height)?;
        self.cast(texture, sigma, color, None, key)
    }

    fn shadow_of_pixels(
        &mut self,
        scene: &Scene,
        width: u32,
        height: u32,
        grow: i32,
        sigma: f64,
        color: figcraft_color::Color,
        key: Option<(u64, u64)>,
    ) -> Option<Layer> {
        let texture = self.canvas.render_layer(self.device, self.queue, scene, width, height)?;
        self.cast(texture, sigma, color, (grow != 0).then_some(grow), key)
    }

    fn shadow_of_layer(&mut self, layer: &Layer, grow: i32, sigma: f64, color: figcraft_color::Color, key: Option<(u64, u64)>) -> Option<Layer> {
        self.cast(layer.texture.clone()?, sigma, color, (grow != 0).then_some(grow), key)
    }

    fn layer_pixels(&mut self, layer: &Layer) -> Option<figcraft_render::Pixmap> {
        read(self.device, self.queue, layer.texture.as_ref()?, layer.size)
    }

    fn kept_shadow(&mut self, key: (u64, u64)) -> Option<Layer> {
        self.canvas.kept_shadow(key)
    }

    fn luminance_masked(&mut self, content: &Scene, mask: &Layer, width: u32, height: u32, key: Option<(u64, u64)>) -> Option<Layer> {
        self.canvas.luminance_masked(self.device, self.queue, content, mask, width, height, key)
    }

    fn cached_node(&mut self, key: u64) -> Option<Arc<Scene>> {
        self.canvas.node_scene(key)
    }

    fn keep_node(&mut self, key: u64, scene: Arc<Scene>) {
        self.canvas.keep_node_scene(key, scene);
    }
}

impl GpuLayers<'_> {
    /// `texture` blurred by `sigma` (as it is, when the renderer would leave the blur out).
    fn blur_texture_only(&mut self, texture: &Texture, sigma: f64) -> Option<Texture> {
        if !sigma.is_finite() || sigma <= 0.3 {
            return Some(texture.clone());
        }
        let size = (texture.width(), texture.height());
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        crate::blur::blur_texture(self.device, self.queue, &self.canvas.blur, &view, size, sigma)
    }

    /// `texture` blurred by `sigma` (no blur when the renderer would leave it out).
    fn blurred_texture(&mut self, texture: Texture, sigma: f64) -> Option<Layer> {
        let out = self.blur_texture_only(&texture, sigma)?;
        Some(self.canvas.as_layer(out))
    }

    /// A shadow from `texture`: grown or shrunk by whole pixels, blurred by `sigma`, and tinted
    /// `color` — all on the device. A shadow kept from an earlier frame under `key` is reused.
    fn cast(&mut self, texture: Texture, sigma: f64, color: figcraft_color::Color, grow: Option<i32>, key: Option<(u64, u64)>) -> Option<Layer> {
        if let Some(key) = key
            && let Some(hit) = self.canvas.kept_shadow(key)
        {
            return Some(hit);
        }
        let size = (texture.width(), texture.height());
        let shaped = match grow {
            Some(grow) => self.canvas.shadow.morph(self.device, self.queue, &texture, size, grow, false)?,
            None => texture,
        };
        let blurred = if sigma > 0.3 {
            let view = shaped.create_view(&wgpu::TextureViewDescriptor::default());
            crate::blur::blur_texture(self.device, self.queue, &self.canvas.blur, &view, size, sigma)?
        } else {
            shaped
        };
        let tinted = self.canvas.shadow.tint(self.device, self.queue, &blurred, size, color)?;
        let layer = self.canvas.as_layer(tinted);
        self.keep(key, &layer);
        Some(layer)
    }

    /// Keep `layer` under `key` for the next frame.
    fn keep(&mut self, key: Option<(u64, u64)>, layer: &Layer) {
        if let Some(key) = key {
            let bytes = layer.size.0 as usize * layer.size.1 as usize * 4;
            self.canvas.keep_shadow(key, layer.clone(), bytes);
        }
    }

    /// A pane's pixels from `back` (the page under it): its padded region cropped, frosted and
    /// run through the glass shader.
    fn glass_from(&mut self, back: &Texture, pane: &figcraft_render::glass::GlassPane, at: (i32, i32)) -> Option<Layer> {
        let pad = pane.pad;
        let (cw, ch) = (pane.w.checked_add(pad.checked_mul(2)?)?, pane.h.checked_add(pad.checked_mul(2)?)?);
        let cropped = crop(self.device, self.queue, back, (at.0 - pad as i32, at.1 - pad as i32), (cw, ch))?;
        let view = cropped.create_view(&wgpu::TextureViewDescriptor::default());
        let frosted = crate::blur::blur_texture(self.device, self.queue, &self.canvas.blur, &view, (cw, ch), pane.frost_sigma)?;
        let frosted_view = frosted.create_view(&wgpu::TextureViewDescriptor::default());
        let glass = self.canvas.glass.paint_texture(self.device, self.queue, pane, &frosted_view)?;
        Some(self.canvas.as_layer(glass))
    }
}

/// A length in pixels at `scale` (at least one).
fn scale_px(v: u32, scale: f64) -> u32 {
    let scaled = (f64::from(v) * scale).round();
    if scaled.is_finite() { (scaled.max(1.0) as u32).max(1) } else { 1 }
}

/// The pixels of a progressive blur's `k`th level (its index in `levels`) that the blend can pick,
/// plus `reach` for the blur's own reading, as `(x, y, w, h)` in the layer: level `k` is used
/// where the ramp's standard deviation lies between its neighbours', which is a band across the
/// ramp (`figcraft_render::blur::brackets`). A ramp with no direction (or a level the ramp never
/// reaches) keeps the whole layer, which is always right.
pub(crate) fn level_crop(
    levels: &[f64],
    k: usize,
    s0: f64,
    s1: f64,
    a: (f64, f64),
    b: (f64, f64),
    (w, h): (u32, u32),
    reach: u32,
) -> (u32, u32, u32, u32) {
    let full = (0, 0, w.max(1), h.max(1));
    let lo = if k == 0 {
        f64::NEG_INFINITY
    } else {
        match levels.get(k - 1) {
            Some(v) => *v,
            None => return full,
        }
    };
    let hi = levels.get(k + 1).copied().unwrap_or(f64::INFINITY);
    let d = s1 - s0;
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let finite = [d, len2, a.0, a.1, b.0, b.1].iter().all(|v| v.is_finite());
    if !finite || d.abs() < 1e-9 || len2 <= 1e-12 {
        return full;
    }
    let (tx, ty) = (dx / len2, dy / len2);
    // The ramp's `t` at a pixel, as the blend works it out (`mix.wgsl`).
    let t0 = (0.5 - a.0) * tx + (0.5 - a.1) * ty + 0.5 * tx + 0.5 * ty;
    // The band in `t` where the deviation is between the neighbours' (cut to the ramp's own range).
    let (mut lt, mut ht) = ((lo - s0) / d, (hi - s0) / d);
    if lt > ht {
        std::mem::swap(&mut lt, &mut ht);
    }
    let (lt, ht) = (lt.max(0.0), ht.min(1.0));
    if ht < lt {
        // The ramp never reaches this level: nothing to blur (the blend never picks it).
        return (0, 0, 1, 1);
    }
    let (wf, hf) = (f64::from(w.max(1) - 1), f64::from(h.max(1) - 1));
    let (xf0, xf1) = (0.0f64.min(wf * tx), 0.0f64.max(wf * tx));
    let (yf0, yf1) = (0.0f64.min(hf * ty), 0.0f64.max(hf * ty));
    let xs = between(lt - t0 - yf1, ht - t0 - yf0, tx, wf);
    let ys = between(lt - t0 - xf1, ht - t0 - xf0, ty, hf);
    let pad = f64::from(reach);
    let (x0, x1) = ((xs.0 - pad).floor().max(0.0), (xs.1 + pad).ceil().min(wf));
    let (y0, y1) = ((ys.0 - pad).floor().max(0.0), (ys.1 + pad).ceil().min(hf));
    let x0 = x0.min(wf);
    let y0 = y0.min(hf);
    let cw = (x1 - x0).max(0.0) as u32 + 1;
    let ch = (y1 - y0).max(0.0) as u32 + 1;
    (x0 as u32, y0 as u32, cw, ch)
}

/// The range of `x` where `x * t` lies between `lo` and `hi`, cut to `[0, limit]` (the whole
/// range when the ramp has no component along the axis).
fn between(lo: f64, hi: f64, t: f64, limit: f64) -> (f64, f64) {
    if !t.is_finite() || t.abs() < 1e-12 {
        return (0.0, limit);
    }
    let (u, v) = (lo / t, hi / t);
    let (a, b) = if u <= v { (u, v) } else { (v, u) };
    (a.max(0.0).min(limit), b.max(0.0).min(limit))
}

/// A `(w, h)` copy of `src` starting at `(x, y)`, transparent where it lies outside `src`.
fn crop(device: &Device, queue: &Queue, src: &Texture, (x, y): (i32, i32), (w, h): (u32, u32)) -> Option<Texture> {
    if w == 0 || h == 0 {
        return None;
    }
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("figcraft backdrop crop"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: TextureUsages::COPY_DST | TextureUsages::COPY_SRC | TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("figcraft backdrop crop") });
    {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("figcraft backdrop crop"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }
    let (sx, sy) = (x.max(0), y.max(0));
    let (dx, dy) = ((-x).max(0) as u32, (-y).max(0) as u32);
    let copy_w = (i64::from(w) - i64::from(dx)).min(i64::from(src.width()) - i64::from(sx));
    let copy_h = (i64::from(h) - i64::from(dy)).min(i64::from(src.height()) - i64::from(sy));
    if copy_w > 0 && copy_h > 0 {
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: src,
                mip_level: 0,
                origin: wgpu::Origin3d { x: sx as u32, y: sy as u32, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: dx, y: dy, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d { width: copy_w as u32, height: copy_h as u32, depth_or_array_layers: 1 },
        );
    }
    queue.submit([encoder.finish()]);
    Some(texture)
}

/// The kept layer in `slot`, allocated (or reallocated) for `size`.
fn keep(slot: &mut Option<Scratch>, device: &Device, (w, h): (u32, u32), label: &str) -> Option<(Texture, TextureView)> {
    let stale = match slot.as_ref() {
        Some(l) => l.size != (w, h),
        None => true,
    };
    if stale {
        let texture = layer_texture(device, label, w, h);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        *slot = Some(Scratch { texture, view, size: (w, h) });
    }
    slot.as_ref().map(|l| (l.texture.clone(), l.view.clone()))
}

/// A `width` × `height` layer vello draws into, or that holds a copy of one.
fn layer_texture(device: &Device, label: &str, width: u32, height: u32) -> Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: TextureUsages::STORAGE_BINDING
            | TextureUsages::TEXTURE_BINDING
            | TextureUsages::COPY_SRC
            | TextureUsages::COPY_DST
            | TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    })
}

fn target(device: &Device, width: u32, height: u32) -> Target {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("figcraft canvas"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        // vello writes through storage textures; egui samples the result, screenshots copy it, and
        // the segmented renderer's accumulation is copied in.
        usage: TextureUsages::STORAGE_BINDING
            | TextureUsages::TEXTURE_BINDING
            | TextureUsages::COPY_SRC
            | TextureUsages::COPY_DST
            | TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Target { texture, view, size: (width, height) }
}

/// Read `texture`'s pixels back (tests, screenshots and exports).
pub(crate) fn read(device: &Device, queue: &Queue, texture: &Texture, (width, height): (u32, u32)) -> Option<Pixmap> {
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
    let t0 = std::time::Instant::now();
    let log = std::env::var("FIGCRAFT_PRESENT_LOG").is_ok();
    let Some(backend) = backend() else { return refusal("not installed") };
    // An animated image's frame changes under a document that did not: the CPU plays it.
    if figcraft_render::images::animation_clock(doc, page, 0.0).is_some() {
        return refusal("an animated image");
    }
    let background = crate::scene::background(doc, page);
    let Ok(mut slot) = backend.canvas.lock() else { return refusal("canvas is busy") };
    let locked = t0.elapsed().as_secs_f64() * 1000.0;
    if slot.is_none() {
        let Some(canvas) = CanvasGpu::new(&backend.device, view.width, view.height) else {
            return refusal("the GPU can't run the rasterizer");
        };
        *slot = Some(canvas);
    }
    let Some(canvas) = slot.as_mut() else { return refusal("the GPU can't run the rasterizer") };
    canvas.resize(&backend.device, view.width, view.height);
    let plan = match crate::scene::plan(doc, page) {
        Ok(plan) => plan,
        Err(e) => return refusal(e.reason()),
    };
    let planned = t0.elapsed().as_secs_f64() * 1000.0;
    // A page with a backdrop node or a plus-darker layer is drawn in segments (a container with a
    // nested backdrop is a step of its own there too, so the layers before it are kept).
    let segmented = plan.iter().any(|s| matches!(s, crate::scene::Step::Backdrop(_) | crate::scene::Step::PlusDarker(_)));
    let texture = if segmented {
        match segmented_frame(&backend.device, &backend.queue, canvas, doc, page, view, &plan, background) {
            Some(texture) => texture,
            None => return refusal("draw failed"),
        }
    } else {
        // Effect layers are drawn and blurred on the device while the scene is built.
        let scene = {
            let mut layers = GpuLayers { canvas, device: &backend.device, queue: &backend.queue };
            match crate::scene::build_with(doc, page, view, &mut layers) {
                Ok(scene) => scene,
                Err(e) => return refusal(e.reason()),
            }
        };
        let built = t0.elapsed().as_secs_f64() * 1000.0;
        let Some(texture) = canvas.render(&backend.device, &backend.queue, &scene, background) else {
            return refusal("draw failed");
        };
        if log {
            let _ = backend.device.poll(wgpu::PollType::wait_indefinitely());
            eprintln!(
                "present {}×{} scale {:.3} origin {:?}: lock {locked:.1}, plan {:.1}, build {:.1}, render {:.1} ms",
                view.width,
                view.height,
                view.scale,
                view.origin,
                planned - locked,
                built - planned,
                t0.elapsed().as_secs_f64() * 1000.0 - built
            );
        }
        texture
    };
    let frame = Frame { view: view.clone(), texture, slot: canvas.slot(), generation: canvas.generation() };
    if let Ok(mut r) = refusal_slot().lock() {
        *r = None;
    }
    Some(frame)
}

/// A page with backdrop effects, drawn in segments: the layers before each such node accumulate
/// into one texture, the node makes its effect from that texture, and its content goes on top.
///
/// Each ordinary step is kept between frames under the key of what it holds: while one layer moves,
/// the steps it did not change are laid over the page again instead of being drawn again.
pub(crate) fn segmented_frame(
    device: &Device,
    queue: &Queue,
    canvas: &mut CanvasGpu,
    doc: &Document,
    page: NodeId,
    view: &View,
    plan: &[crate::scene::Step],
    background: figcraft_color::Color,
) -> Option<TextureView> {
    let (acc, acc_view) = canvas.page_layer(device)?;
    canvas.fill_layer(device, queue, &acc_view, background);
    for (i, step) in plan.iter().enumerate() {
        match step {
            crate::scene::Step::Nodes(ids) => {
                let key = crate::scene::step_is_self_contained(doc, ids).then(|| crate::scene::step_key(doc, view, ids));
                if let Some(key) = key
                    && let Some(kept) = canvas.step_kept(i, key)
                {
                    canvas.compose_over(device, queue, &acc_view, &kept);
                    continue;
                }
                let mut scene = Scene::new();
                let drawn = {
                    let mut layers = GpuLayers { canvas, device, queue };
                    crate::scene::draw_into(doc, page, view, &mut layers, ids, &mut scene)
                };
                drawn.ok()?;
                match key.and_then(|key| canvas.step_layer(device, i).map(|view| (view, key))) {
                    Some((target, key)) => {
                        canvas.draw_into(device, queue, &scene, &target).then_some(())?;
                        canvas.compose_over(device, queue, &acc_view, &target);
                        canvas.keep_step(i, key);
                    }
                    None => lay_over(device, queue, canvas, &acc_view, &scene)?,
                }
            }
            crate::scene::Step::Backdrop(id) => {
                // Every step before it is on the page already: the effect reads the accumulation.
                let image = {
                    let mut layers = GpuLayers { canvas, device, queue };
                    layers.backdrop_effect(doc, *id, view, &acc)?
                };
                let mut scene = Scene::new();
                let drawn = {
                    let mut layers = GpuLayers { canvas, device, queue };
                    crate::scene::draw_backdrop_into(doc, page, view, &mut layers, *id, &image, &mut scene)
                };
                drawn.ok()?;
                lay_over(device, queue, canvas, &acc_view, &scene)?;
            }
            crate::scene::Step::PlusDarker(id) => {
                // Its own pixels (blend left to the pass), then plus-darker onto the page.
                let ids = [*id];
                let key = crate::scene::step_is_self_contained(doc, &ids).then(|| crate::scene::step_key(doc, view, &ids));
                let target = if let Some(key) = key
                    && let Some(kept) = canvas.step_kept(i, key)
                {
                    kept
                } else {
                    let mut scene = Scene::new();
                    let drawn = {
                        let mut layers = GpuLayers { canvas, device, queue };
                        crate::scene::draw_plus_into(doc, page, view, &mut layers, *id, &mut scene)
                    };
                    drawn.ok()?;
                    match key.and_then(|key| canvas.step_layer(device, i).map(|view| (view, key))) {
                        Some((target, key)) => {
                            canvas.draw_into(device, queue, &scene, &target).then_some(())?;
                            canvas.keep_step(i, key);
                            target
                        }
                        None => {
                            let target = canvas.segment_layer(device)?;
                            canvas.draw_into(device, queue, &scene, &target).then_some(())?;
                            target
                        }
                    }
                };
                canvas.plus_darker(device, queue, &acc, &acc_view, &target)?;
            }
        }
    }
    canvas.present_layer(device, queue, &acc)
}

/// Draw `segment` into the kept segment texture and lay it over the page.
fn lay_over(device: &Device, queue: &Queue, canvas: &mut CanvasGpu, page: &TextureView, segment: &Scene) -> Option<()> {
    let view = canvas.segment_layer(device)?;
    canvas.draw_into(device, queue, segment, &view).then_some(())?;
    canvas.compose_over(device, queue, page, &view);
    Some(())
}

/// Whether the GPU canvas is installed (a window with a device built it).
pub fn installed() -> bool {
    backend().is_some()
}
