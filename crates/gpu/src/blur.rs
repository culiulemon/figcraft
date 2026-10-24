//! The blur: three box passes each way (the CPU blur's radii), a full-screen pass at a time,
//! then the pixels read back.

use figcraft_render::Pixmap;
use wgpu::{
    BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, BufferDescriptor,
    BufferUsages, ColorTargetState, ColorWrites, CommandEncoderDescriptor, Device, Extent3d, FragmentState, LoadOp, Operations,
    PipelineCompilationOptions, PipelineLayoutDescriptor, Queue, RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline,
    RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, StoreOp, TexelCopyBufferInfo, TexelCopyBufferLayout,
    TexelCopyTextureInfo, Texture, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureViewDescriptor, VertexState,
};

/// Blurs smaller than this many bytes stay on the CPU (a read-back costs more than they save).
pub(crate) const GPU_MIN_BYTES: usize = 200_000;

pub(crate) struct Blur {
    pipeline: RenderPipeline,
    layout: BindGroupLayout,
}

impl Blur {
    pub(crate) fn new(device: &Device) -> Option<Self> {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("figcraft blur"),
            source: ShaderSource::Wgsl(include_str!("blur.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("figcraft blur"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("figcraft blur"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("figcraft blur"),
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
        Some(Self { pipeline, layout })
    }
}

/// Blur `p` in place on the GPU; false when the device can't (the CPU blurs it instead).
pub(crate) fn blur(device: &Device, queue: &Queue, blur: &Blur, p: &mut Pixmap, sigma: f64) -> bool {
    let (w, h) = (p.width(), p.height());
    if w == 0 || h == 0 || p.data().len() < (w as usize).saturating_mul(h as usize) * 4 {
        return false;
    }
    let size = Extent3d { width: w, height: h, depth_or_array_layers: 1 };
    let make = |label: &str| {
        device.create_texture(&TextureDescriptor {
            label: Some(label),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_DST | TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    };
    let (a, b) = (make("figcraft blur a"), make("figcraft blur b"));
    queue.write_texture(
        TexelCopyTextureInfo { texture: &a, mip_level: 0, origin: Default::default(), aspect: Default::default() },
        p.data(),
        TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
        size,
    );
    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor { label: Some("figcraft blur") });
    let mut src: &Texture = &a;
    let mut dst: &Texture = &b;
    for r in figcraft_render::fx::box_radii(sigma) {
        for vertical in [false, true] {
            let mut params = [0u8; 16];
            params[0..4].copy_from_slice(&(r as u32).to_le_bytes());
            params[4..8].copy_from_slice(&u32::from(vertical).to_le_bytes());
            let uniform = device.create_buffer(&BufferDescriptor {
                label: Some("figcraft blur params"),
                size: 16,
                usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            queue.write_buffer(&uniform, 0, &params);
            let bind = device.create_bind_group(&BindGroupDescriptor {
                label: Some("figcraft blur"),
                layout: &blur.layout,
                entries: &[
                    BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&src.create_view(&TextureViewDescriptor::default())) },
                    BindGroupEntry { binding: 1, resource: uniform.as_entire_binding() },
                ],
            });
            let target = dst.create_view(&TextureViewDescriptor::default());
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("figcraft blur"),
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
            pass.set_pipeline(&blur.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
            drop(pass);
            std::mem::swap(&mut src, &mut dst);
        }
    }
    // Rows of a read-back buffer must be a multiple of 256 bytes.
    let row = ((w as usize * 4).div_ceil(256)) * 256;
    let readback = device.create_buffer(&BufferDescriptor {
        label: Some("figcraft blur read-back"),
        size: (row as u64).saturating_mul(u64::from(h)),
        usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        TexelCopyTextureInfo { texture: src, mip_level: 0, origin: Default::default(), aspect: Default::default() },
        TexelCopyBufferInfo {
            buffer: &readback,
            layout: TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row as u32), rows_per_image: Some(h) },
        },
        size,
    );
    queue.submit(Some(encoder.finish()));
    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    if device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).is_err() || rx.recv().ok().and_then(Result::ok).is_none() {
        return false;
    }
    {
        let Ok(mapped) = slice.get_mapped_range() else { return false };
        let data = p.data_mut();
        for y in 0..h as usize {
            let from = y * row;
            let to = y * w as usize * 4;
            if let (Some(row_bytes), Some(out)) = (mapped.get(from..from + w as usize * 4), data.get_mut(to..to + w as usize * 4)) {
                out.copy_from_slice(row_bytes);
            }
        }
    }
    readback.unmap();
    true
}
