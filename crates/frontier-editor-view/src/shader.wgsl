// SPDX-License-Identifier: Apache-2.0
// One instanced quad pipeline for everything the editor draws: solid rectangles
// (backgrounds, selections, cursors, minimap pixels) and atlas glyphs.

struct Globals {
    // logical -> clip: x' = (x * a + c) * sx - 1, y' = 1 - (y * d + f) * sy
    // a, d = transform scale, c, f = translate (device px), sx/sy = 2 / target size.
    transform: vec4<f32>,
    viewport: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_samp: sampler;

struct Instance {
    @location(0) rect: vec4<f32>,   // x, y, w, h in view device px (before host transform)
    @location(1) uv: vec4<f32>,     // u0, v0, u1, v1
    @location(2) color: vec4<f32>,  // sRGB + alpha
    @location(3) kind: u32,         // 0 solid, 1 glyph mask, 2 colour glyph
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
};

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: Instance) -> VsOut {
    // two triangles over the unit square: (0,0) (1,0) (0,1) / (1,0) (1,1) (0,1)
    let cx = select(0.0, 1.0, vi == 1u || vi == 3u || vi == 5u);
    let cy = select(0.0, 1.0, vi == 2u || vi == 4u || vi == 5u);
    let local = vec2<f32>(cx, cy);
    let p = inst.rect.xy + local * inst.rect.zw;
    let t = globals.transform;
    let dev = vec2<f32>(p.x * t.x + t.z, p.y * t.y + t.w);
    let clip = vec2<f32>(dev.x * globals.viewport.x - 1.0, 1.0 - dev.y * globals.viewport.y);
    var out: VsOut;
    out.pos = vec4<f32>(clip, 0.0, 1.0);
    out.uv = mix(inst.uv.xy, inst.uv.zw, local);
    out.color = inst.color;
    out.kind = inst.kind;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let rgb = srgb_to_linear(in.color.rgb);
    // sampled unconditionally (uniform control flow); solids ignore the mask
    let t = textureSample(atlas_tex, atlas_samp, in.uv);
    let a = select(t.a * in.color.a, in.color.a, in.kind == 0u);
    let tinted = vec4<f32>(rgb * a, a);
    // a colour glyph carries its own (premultiplied) colours
    let own = vec4<f32>(srgb_to_linear(t.rgb / max(t.a, 0.001)) * t.a, t.a) * in.color.a;
    return select(tinted, own, in.kind == 2u);
}
