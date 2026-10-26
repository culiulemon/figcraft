// A box blur pass of `radius` pixels, along rows or down columns (the CPU blur's passes), in
// whole 0–255 units so the floor matches the CPU's integer division:
//
// 1. `scan` gives every pixel the running sum along its line from the line's start (one compute
//    workgroup a line, a tile of 256 pixels at a time), and
// 2. `fs_box` takes the window's sum as the difference of two running sums.
//
// So a pass costs the same whatever the radius (a radius of thousands costs what one costs). A
// pass runs a strip of lines at a time: the running sums are only ever a strip tall (or wide).

struct Params {
    radius: u32,
    vertical: u32,
    /// Where the strip starts along the lines' own axis (the sums' line 0 is the strip's first).
    offset: u32,
    _pad: u32,
};

const TILE: u32 = 256u;

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<uniform> params: Params;
@group(0) @binding(2) var prefix_out: texture_storage_2d<rgba32float, write>;

var<workgroup> tile: array<vec4<f32>, 256>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

// Pixel `i` of `line`, along the pass's line.
fn at(i: i32, line: i32, vertical: bool) -> vec2<i32> {
    return select(vec2<i32>(i, line), vec2<i32>(line, i), vertical);
}

// The running sums along each of the strip's lines, from each line's start, in whole 0–255 units.
@compute @workgroup_size(256, 1, 1)
fn scan(@builtin(local_invocation_id) lid: vec3<u32>, @builtin(workgroup_id) wid: vec3<u32>) {
    let vertical = params.vertical == 1u;
    let dims = textureDimensions(src);
    let len = select(dims.x, dims.y, vertical);
    // The line of the source this workgroup reads (its sums go to the strip's line `wid.x`).
    let line = i32(params.offset + wid.x);
    let out_line = i32(wid.x);
    // The sums of the tiles behind this one (the same in every thread: they read one value).
    var base = vec4<f32>(0.0);
    var t = 0u;
    loop {
        if t >= len {
            break;
        }
        let i = t + lid.x;
        var v = vec4<f32>(0.0);
        if i < len {
            v = round(textureLoad(src, at(i32(i), line, vertical), 0) * 255.0);
        }
        tile[lid.x] = v;
        workgroupBarrier();
        // Hillis–Steele inclusive scan over the tile.
        var step = 1u;
        loop {
            if step >= TILE {
                break;
            }
            var add = vec4<f32>(0.0);
            if lid.x >= step {
                add = tile[lid.x - step];
            }
            workgroupBarrier();
            tile[lid.x] = tile[lid.x] + add;
            workgroupBarrier();
            step = step * 2u;
        }
        if i < len {
            textureStore(prefix_out, at(i32(i), out_line, vertical), tile[lid.x] + base);
        }
        // The last tile's total moves the next tile's sums (read after the write above).
        base = base + tile[TILE - 1u];
        workgroupBarrier();
        t = t + TILE;
    }
}

// The window's average, from the running sums in `src`.
@fragment
fn fs_box(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let dims = textureDimensions(src);
    let vertical = params.vertical == 1u;
    let p = vec2<i32>(i32(pos.x), i32(pos.y));
    let len = i32(select(dims.x, dims.y, vertical));
    let i = select(p.x, p.y, vertical);
    // The sums hold the strip only: the line is the pixel's, less where the strip starts.
    let line = select(p.y, p.x, vertical) - i32(params.offset);
    let r = i32(params.radius);
    // Pixels outside the image count as transparent, as they do on the CPU.
    let hi = min(i + r, len - 1);
    let lo = i - r - 1;
    var sum = vec4<f32>(0.0);
    if hi >= 0 {
        sum = textureLoad(src, at(hi, line, vertical), 0);
    }
    if lo >= 0 {
        sum = sum - textureLoad(src, at(lo, line, vertical), 0);
    }
    // The CPU divides whole numbers and keeps the floor: do the same, so the two agree.
    let avg = floor(sum / f32(2 * r + 1) + 0.0001);
    return clamp(avg / 255.0, vec4<f32>(0.0), vec4<f32>(1.0));
}

// A box pass that reads its window directly: for a device that can't run `scan`.
@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let dims = vec2<i32>(textureDimensions(src));
    let p = vec2<i32>(i32(pos.x), i32(pos.y));
    let r = i32(params.radius);
    let vertical = params.vertical == 1u;
    var sum = vec4<f32>(0.0);
    // Outside the image counts as transparent, as it does on the CPU.
    for (var i = -r; i <= r; i = i + 1) {
        var q = p;
        if (vertical) {
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
