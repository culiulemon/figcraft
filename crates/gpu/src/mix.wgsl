// Progressive blur: the renderer blurs a layer at a few radii between its smallest and largest
// and blends every pixel between the two levels around its own radius (see
// `figcraft_render::blur::{levels, bracket}`). This pass is that blend: it picks each pixel's two
// levels from the ramp and mixes them.

struct Params {
    // The levels' standard deviations, nearest first (at most eight).
    levels: array<vec4<f32>, 2>,
    count: f32,
    s0: f32,
    s1: f32,
    // t at pixel (0, 0)'s centre and its change per pixel across and down.
    t0: f32,
    tx: f32,
    ty: f32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var level0: texture_2d<f32>;
@group(0) @binding(2) var level1: texture_2d<f32>;
@group(0) @binding(3) var level2: texture_2d<f32>;
@group(0) @binding(4) var level3: texture_2d<f32>;
@group(0) @binding(5) var level4: texture_2d<f32>;
@group(0) @binding(6) var level5: texture_2d<f32>;
@group(0) @binding(7) var level6: texture_2d<f32>;
@group(0) @binding(8) var level7: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let q = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(q * 2.0 - 1.0, 0.0, 1.0);
}

fn at_level(k: i32, c: vec2<i32>) -> vec4<f32> {
    switch k {
        case 0: { return textureLoad(level0, c, 0); }
        case 1: { return textureLoad(level1, c, 0); }
        case 2: { return textureLoad(level2, c, 0); }
        case 3: { return textureLoad(level3, c, 0); }
        case 4: { return textureLoad(level4, c, 0); }
        case 5: { return textureLoad(level5, c, 0); }
        case 6: { return textureLoad(level6, c, 0); }
        default: { return textureLoad(level7, c, 0); }
    }
}

fn sigma_at(k: i32) -> f32 {
    return params.levels[k / 4][k % 4];
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let c = vec2<i32>(pos.xy);
    let n = i32(params.count);
    if (n < 2) {
        return at_level(max(n - 1, 0), c);
    }
    let t = clamp(params.t0 + pos.x * params.tx + pos.y * params.ty, 0.0, 1.0);
    let s = params.s0 + (params.s1 - params.s0) * t;
    var k = 0;
    for (var i = 0; i < n; i = i + 1) {
        if (sigma_at(i) <= s) {
            k = i;
        }
    }
    k = min(k, n - 2);
    let lo = sigma_at(k);
    let hi = sigma_at(k + 1);
    let f = select(0.0, (s - lo) / (hi - lo), hi > lo);
    // The renderer's weights are whole 1/255ths.
    let w = round(clamp(f, 0.0, 1.0) * 255.0) / 255.0;
    return mix(at_level(k, c), at_level(k + 1, c), w);
}
