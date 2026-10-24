// A box blur pass of `radius` pixels, along rows or down columns (the CPU blur's passes).

struct Params {
    radius: u32,
    vertical: u32,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> params: Params;

// A full-screen triangle: three vertices cover the view.
@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let dims = vec2<i32>(textureDimensions(src));
    let p = vec2<i32>(i32(pos.x), i32(pos.y));
    let r = i32(params.radius);
    var sum = vec4<f32>(0.0);
    // Outside the image counts as transparent, as it does on the CPU.
    for (var i = -r; i <= r; i = i + 1) {
        var q = p;
        if (params.vertical == 1u) {
            q.y = q.y + i;
        } else {
            q.x = q.x + i;
        }
        if (q.x >= 0 && q.y >= 0 && q.x < dims.x && q.y < dims.y) {
            sum = sum + textureLoad(src, q, 0);
        }
    }
    // The CPU blur divides whole numbers and keeps the floor: do the same, so the two agree.
    return floor(sum / f32(2 * r + 1) * 255.0 + 0.0001) / 255.0;
}
