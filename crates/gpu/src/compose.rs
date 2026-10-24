//! Laying one canvas-sized layer over another: the page's segments are drawn into their own
//! textures and composited (premultiplied, source over) as the accumulated page moves forward.

use wgpu::{
    BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendComponent, BlendFactor,
    BlendOperation, BlendState, ColorTargetState, ColorWrites, CommandEncoderDescriptor, Device, FragmentState, LoadOp, Operations,
    PipelineCompilationOptions, PipelineLayoutDescriptor, Queue, RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline,
    RenderPipelineDescriptor, Sampler, SamplerDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, StoreOp, TextureFormat, TextureView,
    VertexState,
};

pub(crate) struct Compose {
    /// Straight-alpha `src` premultiplied over the accumulation.
    pipeline: RenderPipeline,
    /// Straight-alpha `src` premultiplied into a fresh target (an effect layer).
    replace: RenderPipeline,
    layout: BindGroupLayout,
    sampler: Sampler,
}

impl Compose {
    pub(crate) fn new(device: &Device) -> Option<Self> {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("figcraft compose"),
            source: ShaderSource::Wgsl(include_str!("compose.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("figcraft compose"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("figcraft compose"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        // Premultiplied "source over".
        let over = BlendComponent { src_factor: BlendFactor::One, dst_factor: BlendFactor::OneMinusSrcAlpha, operation: BlendOperation::Add };
        let build = |label: &str, blend: Option<BlendState>| {
            device.create_render_pipeline(&RenderPipelineDescriptor {
                label: Some(label),
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
                    targets: &[Some(ColorTargetState { format: TextureFormat::Rgba8Unorm, blend, write_mask: ColorWrites::ALL })],
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let pipeline = build("figcraft compose", Some(BlendState { color: over, alpha: over }));
        let replace = build("figcraft premultiply", None);
        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("figcraft compose"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        Some(Self { pipeline, replace, layout, sampler })
    }

    /// Lay `src` over `dst` (both the canvas' size), keeping what `dst` already holds.
    pub(crate) fn over(&self, device: &Device, queue: &Queue, dst: &TextureView, src: &TextureView) {
        self.run(device, queue, dst, src, LoadOp::Load);
    }

    /// Premultiply `src` (straight alpha, as vello leaves it) into all of `dst`.
    pub(crate) fn premultiply(&self, device: &Device, queue: &Queue, src: &TextureView, dst: &TextureView) {
        self.run(device, queue, dst, src, LoadOp::Clear(wgpu::Color::TRANSPARENT));
    }

    /// Draw a straight-alpha `src` through `fs` into `dst`: premultiplied, over what `load` keeps
    /// or replacing it.
    fn run(&self, device: &Device, queue: &Queue, dst: &TextureView, src: &TextureView, load: LoadOp<wgpu::Color>) {
        let bind = device.create_bind_group(&BindGroupDescriptor {
            label: Some("figcraft compose"),
            layout: &self.layout,
            entries: &[
                BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(src) },
                BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        let replaces = matches!(load, LoadOp::Clear(_));
        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor { label: Some("figcraft compose") });
        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("figcraft compose"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: dst,
                    resolve_target: None,
                    ops: Operations { load, store: StoreOp::Store },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(if replaces { &self.replace } else { &self.pipeline });
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
        }
        queue.submit([encoder.finish()]);
    }
}

/// Clear `view` to `color` (the accumulation's first fill and the segments' transparent ground).
pub(crate) fn clear(device: &Device, queue: &Queue, view: &TextureView, color: wgpu::Color) {
    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor { label: Some("figcraft clear") });
    {
        let _pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("figcraft clear"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view,
                resolve_target: None,
                ops: Operations { load: LoadOp::Clear(color), store: StoreOp::Store },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }
    queue.submit([encoder.finish()]);
}
