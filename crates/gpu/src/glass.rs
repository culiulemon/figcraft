//! The glass pane's pixels on the GPU: the same formula the CPU paints with, from the pane's
//! geometry (kept as textures, worked out when the shape, size or zoom changes) and its frosted
//! backdrop (uploaded each frame).

use std::sync::{Arc, Mutex};

use figcraft_render::{GlassPaint, Pixmap};
use wgpu::{
    BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, BufferDescriptor,
    BufferUsages, ColorTargetState, ColorWrites, CommandEncoderDescriptor, Device, Extent3d, FragmentState, LoadOp, Operations,
    PipelineCompilationOptions, PipelineLayoutDescriptor, Queue, RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline,
    RenderPipelineDescriptor, Sampler, SamplerDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, StoreOp, TexelCopyBufferInfo,
    TexelCopyBufferLayout, TexelCopyTextureInfo, Texture, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureViewDescriptor,
    VertexState,
};

/// A pane's geometry as the GPU keeps it: coverage alpha, distance to the outline, inward normals.
struct Geometry {
    alpha: Texture,
    dist: Texture,
    normals: Texture,
    bytes: usize,
}

pub(crate) struct Glass {
    pipeline: RenderPipeline,
    layout: BindGroupLayout,
    sampler: Sampler,
    /// Kept geometry, least recently used last (keys are the renderer's geometry ids).
    kept: Mutex<Vec<(u64, Arc<Geometry>)>>,
}

/// The most the kept geometry may take (bytes; about 13 a pixel).
const KEPT_BUDGET: usize = 64 << 20;

impl Glass {
    pub(crate) fn new(device: &Device) -> Option<Self> {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("figcraft glass"),
            source: ShaderSource::Wgsl(include_str!("glass.wgsl").into()),
        });
        let texture = |binding: u32, filterable: bool| BindGroupLayoutEntry {
            binding,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("figcraft glass"),
            entries: &[
                texture(0, true),
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                texture(2, false),
                texture(3, false),
                texture(4, false),
                BindGroupLayoutEntry {
                    binding: 5,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("figcraft glass"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("figcraft glass"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: PipelineCompilationOptions::default(),
                targets: &[Some(ColorTargetState {
                    format: TextureFormat::Rgba8Unorm,
                    blend: Some(BlendState::REPLACE),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("figcraft glass"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        Some(Self { pipeline, layout, sampler, kept: Mutex::new(Vec::new()) })
    }

    /// The pane's pixels, or `None` when the device can't (the CPU paints them).
    pub(crate) fn paint(&self, device: &Device, queue: &Queue, p: &GlassPaint<'_>) -> Option<Pixmap> {
        let (w, h) = (p.w, p.h);
        if w == 0 || h == 0 || p.cw == 0 || p.ch == 0 {
            return None;
        }
        let geometry = self.geometry(device, queue, p)?;
        let frosted = upload(
            device,
            queue,
            "figcraft glass backdrop",
            p.frosted.width(),
            p.frosted.height(),
            TextureFormat::Rgba8Unorm,
            p.frosted.data(),
            p.frosted.width() as usize * 4,
        )?;
        let out = device.create_texture(&TextureDescriptor {
            label: Some("figcraft glass out"),
            size: Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let uniform = device.create_buffer(&BufferDescriptor {
            label: Some("figcraft glass params"),
            size: 64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&uniform, 0, &params(p));
        let view = |t: &Texture| t.create_view(&TextureViewDescriptor::default());
        let bind = device.create_bind_group(&BindGroupDescriptor {
            label: Some("figcraft glass"),
            layout: &self.layout,
            entries: &[
                BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view(&frosted)) },
                BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&view(&geometry.alpha)) },
                BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&view(&geometry.dist)) },
                BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&view(&geometry.normals)) },
                BindGroupEntry { binding: 5, resource: uniform.as_entire_binding() },
            ],
        });
        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor { label: Some("figcraft glass") });
        {
            let target = view(&out);
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("figcraft glass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &target,
                    resolve_target: None,
                    ops: Operations { load: LoadOp::Clear(wgpu::Color::TRANSPARENT), store: StoreOp::Store },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
        }
        read_back(device, queue, encoder, &out, w, h)
    }

    /// This pane's geometry as textures, worked out (and uploaded) the first time it is asked for.
    fn geometry(&self, device: &Device, queue: &Queue, p: &GlassPaint<'_>) -> Option<Arc<Geometry>> {
        let Ok(mut kept) = self.kept.lock() else { return None };
        if let Some(i) = kept.iter().position(|(id, _)| *id == p.geometry) {
            let entry = kept.remove(i);
            let g = entry.1.clone();
            kept.push(entry);
            return Some(g);
        }
        let (cw, ch) = (u32::try_from(p.cw).ok()?, u32::try_from(p.ch).ok()?);
        let alpha = upload(device, queue, "figcraft glass coverage", cw, ch, TextureFormat::R8Unorm, p.alpha, p.cw)?;
        let mut dist_bytes = Vec::with_capacity(p.dist.len() * 4);
        for d in p.dist {
            dist_bytes.extend_from_slice(&d.to_le_bytes());
        }
        let dist = upload(device, queue, "figcraft glass distance", cw, ch, TextureFormat::R32Float, &dist_bytes, p.cw * 4)?;
        let mut normal_bytes = Vec::with_capacity(p.normals.len() * 8);
        for (x, y) in p.normals {
            normal_bytes.extend_from_slice(&x.to_le_bytes());
            normal_bytes.extend_from_slice(&y.to_le_bytes());
        }
        let normals = upload(device, queue, "figcraft glass normals", cw, ch, TextureFormat::Rg32Float, &normal_bytes, p.cw * 8)?;
        let bytes = p.cw * p.ch * 13;
        let g = Arc::new(Geometry { alpha, dist, normals, bytes });
        if bytes <= KEPT_BUDGET {
            let mut total: usize = kept.iter().map(|(_, g)| g.bytes).sum();
            while total.saturating_add(bytes) > KEPT_BUDGET && !kept.is_empty() {
                total = total.saturating_sub(kept.remove(0).1.bytes);
            }
            kept.push((p.geometry, g.clone()));
        }
        Some(g)
    }
}

/// `bytes` (`bytes_per_row` a row) uploaded as a `w` × `h` texture of `format`.
fn upload(device: &Device, queue: &Queue, label: &str, w: u32, h: u32, format: TextureFormat, bytes: &[u8], bytes_per_row: usize) -> Option<Texture> {
    let texture = device.create_texture(&TextureDescriptor {
        label: Some(label),
        size: Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: Default::default(), aspect: Default::default() },
        bytes,
        TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(u32::try_from(bytes_per_row).ok()?), rows_per_image: Some(h) },
        Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    Some(texture)
}

/// The pane's parameters as the shader's uniform (64 bytes of f32).
fn params(p: &GlassPaint<'_>) -> [u8; 64] {
    let mut out = [0u8; 64];
    let values = [
        p.cw as f32,
        p.ch as f32,
        p.m as f32,
        p.pad as f32,
        p.depth,
        p.bend_max,
        p.fringe,
        p.power,
        p.band,
        p.reach,
        p.floor,
        p.fall,
        p.intensity,
        p.light.0,
        p.light.1,
        0.0,
    ];
    for (i, v) in values.into_iter().enumerate() {
        if let Some(slot) = out.get_mut(i * 4..i * 4 + 4) {
            slot.copy_from_slice(&v.to_le_bytes());
        }
    }
    out
}

fn read_back(device: &Device, queue: &Queue, mut encoder: wgpu::CommandEncoder, texture: &Texture, w: u32, h: u32) -> Option<Pixmap> {
    let row = (w as usize * 4).div_ceil(256) * 256;
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some("figcraft glass read-back"),
        size: (row as u64).saturating_mul(u64::from(h)),
        usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        TexelCopyTextureInfo { texture, mip_level: 0, origin: Default::default(), aspect: Default::default() },
        TexelCopyBufferInfo {
            buffer: &buffer,
            layout: TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row as u32), rows_per_image: Some(h) },
        },
        Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    queue.submit(Some(encoder.finish()));
    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    if device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).is_err() || rx.recv().ok().and_then(Result::ok).is_none() {
        return None;
    }
    let mut out = Pixmap::new(w, h)?;
    {
        let Ok(mapped) = slice.get_mapped_range() else { return None };
        let data = out.data_mut();
        for y in 0..h as usize {
            let from = y * row;
            let to = y * w as usize * 4;
            if let (Some(src), Some(dst)) = (mapped.get(from..from + w as usize * 4), data.get_mut(to..to + w as usize * 4)) {
                dst.copy_from_slice(src);
            }
        }
    }
    buffer.unmap();
    Some(out)
}
