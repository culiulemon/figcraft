// Lays one canvas-sized layer over another: the page's segments are drawn into their own
// textures and laid over the accumulation as premultiplied pixels.
//
// vello's own targets hold straight (separated) alpha — its fine pass stores `rgb / alpha` (see
// `vello_shaders`' `fine.wgsl`) — so every layer it drew is premultiplied here, at the one place
// it meets the premultiplied accumulation, the blurs and the glass shader (which all work on
// premultiplied pixels, like tiny-skia's pixmaps).

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let q = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(q * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(src));
    let s = textureSample(src, samp, pos.xy / dims);
    return vec4<f32>(s.rgb * s.a, s.a);
}
