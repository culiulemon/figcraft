//! The blur: three box passes each way (the CPU blur's radii), a full-screen pass at a time, then
//! the pixels read back.
//!
//! A box pass is O(1) a pixel: a compute pass gives every pixel the running sum along its line, and
//! a pass takes the window's average from two of those sums. (A device that can't run the scan
//! falls back to reading the window directly, which costs what the radius costs.)

use figcraft_render::Pixmap;
use wgpu::{
    BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, BufferDescriptor,
    BufferUsages, ColorTargetState, ColorWrites, CommandEncoder, CommandEncoderDescriptor, ComputePassDescriptor, ComputePipeline,
    ComputePipelineDescriptor, Device, Extent3d, FragmentState, LoadOp, Operations, PipelineCompilationOptions, PipelineLayoutDescriptor, Queue,
    RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages,
    StoreOp, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages, TextureView, TextureViewDescriptor, VertexState,
};

/// Blurs smaller than this many bytes stay on the CPU (a read-back costs more than they save).
pub(crate) const GPU_MIN_BYTES: usize = 200_000;

/// The scan's workgroup size (one pixel a thread, a tile at a time).
const SCAN_WORKGROUP: u32 = 256;
/// The lines one scan pass covers: its running sums are only ever this many lines.
const STRIP: u32 = 256;

pub(crate) struct Blur {
    /// The sampled texture and the pass's parameters.
    layout: BindGroupLayout,
    /// The scan's layout: the same, plus the prefix sums it writes.
    scan_layout: Option<BindGroupLayout>,
    /// The running sums, a line a workgroup.
    scan: Option<ComputePipeline>,
    /// The window's average from those sums.
    average: Option<RenderPipeline>,
    /// The window read directly, for a device without the scan.
    direct: Option<RenderPipeline>,
    /// The textures a run reads and writes, kept between runs: a progressive blur would otherwise
    /// make (and drop) a dozen full-size textures a frame, and that churn shows as judder.
    pool: std::sync::Mutex<Vec<Texture>>,
}

/// Most bytes the pool keeps: a blur's textures are full-size, and a progressive blur needs about
/// ten of them at once (its levels, the two a box pass ping-pongs between).
const POOL_BYTES: u64 = 128 << 20;

impl Blur {
    pub(crate) fn new(device: &Device) -> Option<Self> {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("figcraft blur"),
            source: ShaderSource::Wgsl(include_str!("blur.wgsl").into()),
        });
        let texture = |binding| BindGroupLayoutEntry {
            binding,
            // The compute pass reads the source too (its running sums).
            visibility: ShaderStages::FRAGMENT | ShaderStages::COMPUTE,
            ty: BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let uniform = BindGroupLayoutEntry {
            binding: 1,
            visibility: ShaderStages::FRAGMENT | ShaderStages::COMPUTE,
            ty: BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
            count: None,
        };
        let layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor { label: Some("figcraft blur"), entries: &[texture(0), uniform] });
        let pass = |label: &str, entry: &'static str, layout: &BindGroupLayout| {
            let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(layout)],
                immediate_size: 0,
            });
            device.create_render_pipeline(&RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: VertexState {
                    module: &shader,
                    entry_point: Some("vs"),
                    compilation_options: PipelineCompilationOptions::default(),
                    buffers: &[],
                },
                fragment: Some(FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
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
            })
        };
        // The scan needs a compute pass (`rgba32float` storage is core WebGPU, so every desktop
        // backend the canvas runs on can write the sums).
        let scan_supported =
            device.limits().max_compute_workgroup_size_x >= SCAN_WORKGROUP && device.limits().max_compute_invocations_per_workgroup >= SCAN_WORKGROUP;
        let (scan_layout, scan, average) = if scan_supported {
            let sums = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
                label: Some("figcraft blur scan"),
                entries: &[
                    texture(0),
                    uniform,
                    BindGroupLayoutEntry {
                        binding: 2,
                        visibility: ShaderStages::COMPUTE,
                        ty: BindingType::StorageTexture {
                            access: wgpu::StorageTextureAccess::WriteOnly,
                            format: TextureFormat::Rgba32Float,
                            view_dimension: wgpu::TextureViewDimension::D2,
                        },
                        count: None,
                    },
                ],
            });
            let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
                label: Some("figcraft blur scan"),
                bind_group_layouts: &[Some(&sums)],
                immediate_size: 0,
            });
            let scan = device.create_compute_pipeline(&ComputePipelineDescriptor {
                label: Some("figcraft blur scan"),
                layout: Some(&pipeline_layout),
                module: &shader,
                entry_point: Some("scan"),
                compilation_options: PipelineCompilationOptions::default(),
                cache: None,
            });
            let average = pass("figcraft blur average", "fs_box", &layout);
            (Some(sums), Some(scan), Some(average))
        } else {
            (None, None, None)
        };
        let direct = Some(pass("figcraft blur", "fs", &layout));
        Some(Self { direct, layout, scan_layout, scan, average, pool: std::sync::Mutex::new(Vec::new()) })
    }

    /// A texture of `size`, `format` and `usage` for a run: one kept from an earlier run when its
    /// shape matches, else a new one.
    fn take(&self, device: &Device, label: &str, (w, h): (u32, u32), format: TextureFormat, usage: TextureUsages) -> Texture {
        let mut pool = self.pool.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(i) = pool.iter().position(|t| t.width() == w && t.height() == h && t.format() == format && t.usage() == usage) {
            return pool.swap_remove(i);
        }
        texture(device, label, (w, h), format, usage)
    }

    /// Keep `texture` for the next run (its passes are done: a queue runs in order). The oldest
    /// textures go first when the pool is full: clearing it would make the next run allocate them
    /// all again, which is exactly the churn the pool is there to avoid.
    pub(crate) fn give_back(&self, texture: Texture) {
        let mut pool = self.pool.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes = |t: &Texture| u64::from(t.width()) * u64::from(t.height()) * 4;
        let mut used: u64 = pool.iter().map(bytes).sum();
        let want = bytes(&texture);
        while used.saturating_add(want) > POOL_BYTES && !pool.is_empty() {
            let old = pool.remove(0);
            used = used.saturating_sub(bytes(&old));
        }
        if used.saturating_add(want) <= POOL_BYTES {
            pool.push(texture);
        }
    }
}

/// The textures a blur reads and writes: one after one, a box pass a time.
fn texture(device: &Device, label: &str, (w, h): (u32, u32), format: TextureFormat, usage: TextureUsages) -> Texture {
    device.create_texture(&TextureDescriptor {
        label: Some(label),
        size: Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

/// The blur's box passes over `src` into `encoder` (`None` when the device can't): the result
/// texture, and the scratch to hand back to the pool once the caller has submitted it. A
/// progressive blur's levels and its blend then reach the device in one submission — a submit a
/// box pass made the canvas queue a dozen of them a frame, each waiting behind the window's.
/// The scan runs a strip of lines at a time, so its sums stay small whatever the layer's size.
fn boxes(
    device: &Device,
    queue: &Queue,
    blur: &Blur,
    encoder: &mut CommandEncoder,
    src: &TextureView,
    (w, h): (u32, u32),
    sigma: f64,
) -> Option<(Texture, Vec<Texture>)> {
    if w == 0 || h == 0 {
        return None;
    }
    if blur.scan.is_none() && blur.direct.is_none() {
        return None;
    }
    let io = TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_DST | TextureUsages::COPY_SRC;
    let mut current = blur.take(device, "figcraft blur a", (w, h), TextureFormat::Rgba8Unorm, io);
    let mut other = blur.take(device, "figcraft blur b", (w, h), TextureFormat::Rgba8Unorm, io);
    // A box pass's running sums: a strip of rows across, or of columns down.
    let sums_io = TextureUsages::STORAGE_BINDING | TextureUsages::TEXTURE_BINDING;
    let (sums_across, sums_down) = if blur.scan.is_some() {
        (
            Some(blur.take(device, "figcraft blur sums", (w, STRIP.min(h)), TextureFormat::Rgba32Float, sums_io)),
            Some(blur.take(device, "figcraft blur sums", (STRIP.min(w), h), TextureFormat::Rgba32Float, sums_io)),
        )
    } else {
        (None, None)
    };
    let mut read = src.clone();
    for r in figcraft_render::fx::box_radii(sigma) {
        for vertical in [false, true] {
            let target = other.create_view(&TextureViewDescriptor::default());
            let lines = if vertical { w } else { h };
            let mut started = false;
            if let (Some(scan), Some(average), Some(scan_layout)) = (&blur.scan, &blur.average, &blur.scan_layout)
                && let Some(sums) = if vertical { &sums_down } else { &sums_across }
            {
                let written = sums.create_view(&TextureViewDescriptor::default());
                let mut start = 0u32;
                while start < lines {
                    let count = STRIP.min(lines - start);
                    let mut params = [0u8; 16];
                    params[0..4].copy_from_slice(&(r as u32).to_le_bytes());
                    params[4..8].copy_from_slice(&u32::from(vertical).to_le_bytes());
                    params[8..12].copy_from_slice(&start.to_le_bytes());
                    let uniform = device.create_buffer(&BufferDescriptor {
                        label: Some("figcraft blur params"),
                        size: 16,
                        usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    queue.write_buffer(&uniform, 0, &params);
                    // The running sums along the strip's lines, then the window's average from them.
                    let bind = device.create_bind_group(&BindGroupDescriptor {
                        label: Some("figcraft blur scan"),
                        layout: scan_layout,
                        entries: &[
                            BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&read) },
                            BindGroupEntry { binding: 1, resource: uniform.as_entire_binding() },
                            BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&written) },
                        ],
                    });
                    {
                        let mut pass =
                            encoder.begin_compute_pass(&ComputePassDescriptor { label: Some("figcraft blur scan"), timestamp_writes: None });
                        pass.set_pipeline(scan);
                        pass.set_bind_group(0, &bind, &[]);
                        pass.dispatch_workgroups(count, 1, 1);
                    }
                    let bind = device.create_bind_group(&BindGroupDescriptor {
                        label: Some("figcraft blur average"),
                        layout: &blur.layout,
                        entries: &[
                            BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&written) },
                            BindGroupEntry { binding: 1, resource: uniform.as_entire_binding() },
                        ],
                    });
                    let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                        label: Some("figcraft blur average"),
                        color_attachments: &[Some(RenderPassColorAttachment {
                            view: &target,
                            resolve_target: None,
                            // The first strip clears the whole target; the rest write their own.
                            ops: Operations {
                                load: if start == 0 { LoadOp::Clear(wgpu::Color::TRANSPARENT) } else { LoadOp::Load },
                                store: StoreOp::Store,
                            },
                            depth_slice: None,
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                    // Only the strip: the rest of the destination is another strip's.
                    if vertical {
                        pass.set_viewport(start as f32, 0.0, count as f32, h as f32, 0.0, 1.0);
                    } else {
                        pass.set_viewport(0.0, start as f32, w as f32, count as f32, 0.0, 1.0);
                    }
                    pass.set_pipeline(average);
                    pass.set_bind_group(0, &bind, &[]);
                    pass.draw(0..3, 0..1);
                    start += count;
                    started = true;
                }
            }
            if !started {
                let Some(direct) = &blur.direct else { return None };
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
                        BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&read) },
                        BindGroupEntry { binding: 1, resource: uniform.as_entire_binding() },
                    ],
                });
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
                pass.set_pipeline(direct);
                pass.set_bind_group(0, &bind, &[]);
                pass.draw(0..3, 0..1);
            }
            std::mem::swap(&mut current, &mut other);
            read = current.create_view(&TextureViewDescriptor::default());
        }
    }
    // After the last pass the freshly written texture is `current`; the rest goes back after the
    // caller submits.
    let mut scratch = vec![other];
    if let Some(s) = sums_across {
        scratch.push(s);
    }
    if let Some(s) = sums_down {
        scratch.push(s);
    }
    Some((current, scratch))
}

/// Blur the pixels of `src` into a new texture, the same three box passes as [`blur`] (`None`
/// when the device can't): the result never leaves the GPU.
pub(crate) fn blur_texture(device: &Device, queue: &Queue, blur: &Blur, src: &TextureView, (w, h): (u32, u32), sigma: f64) -> Option<Texture> {
    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor { label: Some("figcraft blur") });
    let (out, scratch) = boxes(device, queue, blur, &mut encoder, src, (w, h), sigma)?;
    queue.submit([encoder.finish()]);
    for t in scratch {
        blur.give_back(t);
    }
    Some(out)
}

/// [`blur_texture`] into `encoder`: the result, and the scratch to hand back once the caller has
/// submitted it. A progressive blur's levels and its mix then go in one submission.
pub(crate) fn blur_texture_in(
    device: &Device,
    queue: &Queue,
    blur: &Blur,
    encoder: &mut CommandEncoder,
    src: &TextureView,
    (w, h): (u32, u32),
    sigma: f64,
) -> Option<(Texture, Vec<Texture>)> {
    boxes(device, queue, blur, encoder, src, (w, h), sigma)
}

/// Blur `p` in place on the GPU; false when the device can't (the CPU blurs it instead).
pub(crate) fn blur(device: &Device, queue: &Queue, blur: &Blur, p: &mut Pixmap, sigma: f64) -> bool {
    let (w, h) = (p.width(), p.height());
    if w == 0 || h == 0 || p.data().len() < (w as usize).saturating_mul(h as usize) * 4 {
        return false;
    }
    let size = Extent3d { width: w, height: h, depth_or_array_layers: 1 };
    let upload = blur.take(device, "figcraft blur in", (w, h), TextureFormat::Rgba8Unorm, TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST);
    queue.write_texture(
        TexelCopyTextureInfo { texture: &upload, mip_level: 0, origin: Default::default(), aspect: Default::default() },
        p.data(),
        TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
        size,
    );
    let source = upload.create_view(&TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor { label: Some("figcraft blur") });
    let Some((done, scratch)) = boxes(device, queue, blur, &mut encoder, &source, (w, h), sigma) else { return false };
    // Rows of a read-back buffer must be a multiple of 256 bytes.
    let row = ((w as usize * 4).div_ceil(256)) * 256;
    let readback = device.create_buffer(&BufferDescriptor {
        label: Some("figcraft blur read-back"),
        size: (row as u64).saturating_mul(u64::from(h)),
        usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        TexelCopyTextureInfo { texture: &done, mip_level: 0, origin: Default::default(), aspect: Default::default() },
        TexelCopyBufferInfo {
            buffer: &readback,
            layout: TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row as u32), rows_per_image: Some(h) },
        },
        size,
    );
    queue.submit(Some(encoder.finish()));
    blur.give_back(upload);
    for t in scratch {
        blur.give_back(t);
    }
    blur.give_back(done);
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
