// Moirai shaders: glowing trails (max-blended per body) and a single composite pass.

struct Globals {
    size: vec2<f32>,        // target size in pixels
    center: vec2<f32>,      // world point at the centre of the frame
    scale: f32,             // pixels per world unit
    fade: f32,              // global brightness (scene transitions)
    time: f32,              // seconds, for the starfield twinkle
    core_px: f32,           // trail core half-width
    glow_px: f32,           // trail glow radius
    glow_amp: f32,
    body_core_px: f32,
    body_halo_px: f32,
    encode_srgb: f32,       // 1 when the target is a linear (non-sRGB) format
    exposure: f32,
    stars: f32,
    _pad: f32,
    colors: array<vec4<f32>, 3>,
    bodies: array<vec4<f32>, 3>,   // xy: position (world), z: brightness
    bg_center: vec4<f32>,
    bg_edge: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;

fn to_px(w: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(g.size.x * 0.5 + (w.x - g.center.x) * g.scale,
                     g.size.y * 0.5 - (w.y - g.center.y) * g.scale);
}

// ---------------------------------------------------------------- trails

struct Seg {
    @location(0) p0: vec2<f32>,
    @location(1) p1: vec2<f32>,
    @location(2) ages: vec2<f32>,
    @location(3) body: u32,
};

struct TrailOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) px: vec2<f32>,
    @location(1) @interpolate(flat) a: vec2<f32>,
    @location(2) @interpolate(flat) b: vec2<f32>,
    @location(3) @interpolate(flat) ages: vec2<f32>,
    @location(4) @interpolate(flat) body: u32,
};

@vertex
fn vs_trail(@builtin(vertex_index) vi: u32, s: Seg) -> TrailOut {
    let a = to_px(s.p0);
    let b = to_px(s.p1);
    let r = g.core_px + 3.0 * g.glow_px + 1.0;
    var dir = b - a;
    let len = length(dir);
    if (len < 1e-4) {
        dir = vec2<f32>(1.0, 0.0);
    } else {
        dir = dir / len;
    }
    let nrm = vec2<f32>(-dir.y, dir.x);
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0));
    let c = corners[vi];
    let along = select(-r, len + r, c.x > 0.5);
    let p = a + dir * along + nrm * c.y * r;
    var o: TrailOut;
    o.pos = vec4<f32>(p.x / g.size.x * 2.0 - 1.0, 1.0 - p.y / g.size.y * 2.0, 0.0, 1.0);
    o.px = p;
    o.a = a;
    o.b = b;
    o.ages = s.ages;
    o.body = s.body;
    return o;
}

@fragment
fn fs_trail(i: TrailOut) -> @location(0) vec4<f32> {
    let ab = i.b - i.a;
    let l2 = max(dot(ab, ab), 1e-6);
    let t = clamp(dot(i.px - i.a, ab) / l2, 0.0, 1.0);
    let d = length(i.px - (i.a + ab * t));
    let life = 1.0 - mix(i.ages.x, i.ages.y, t);
    if (life <= 0.0) {
        discard;
    }
    let fade = life * life * (3.0 - 2.0 * life);
    let w = g.core_px * (0.35 + 0.65 * life);
    let core = 1.0 - smoothstep(w - 0.7, w + 0.7, d);
    let glow = g.glow_amp * exp(-(d * d) / (g.glow_px * g.glow_px));
    let v = fade * (core + glow);
    var out = vec4<f32>(0.0, 0.0, 0.0, fade * core);
    if (i.body == 0u) {
        out.x = v;
    } else if (i.body == 1u) {
        out.y = v;
    } else {
        out.z = v;
    }
    return out;
}

// ---------------------------------------------------------------- composite

@group(0) @binding(1) var trail_tex: texture_2d<f32>;

@vertex
fn vs_full(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

fn hash2(p: vec2<f32>) -> f32 {
    let q = fract(p * vec2<f32>(0.1031, 0.1030));
    let r = q + dot(q, q.yx + 33.33);
    return fract((r.x + r.y) * r.x);
}

fn starfield(px: vec2<f32>) -> vec3<f32> {
    let cell = 9.0 * max(1.0, min(g.size.x, g.size.y) / 900.0);
    let id = floor(px / cell);
    let h = hash2(id + 17.0);
    if (h > 0.07) {
        return vec3<f32>(0.0);
    }
    let jitter = vec2<f32>(hash2(id + 7.1), hash2(id + 3.7));
    let sp = (id + 0.25 + 0.5 * jitter) * cell;
    let d = length(px - sp);
    let b = pow(hash2(id + 1.3), 3.0) * 0.05;
    let tw = 0.7 + 0.3 * sin(g.time * (0.25 + 0.6 * hash2(id + 9.2)) + 40.0 * h);
    let tint = mix(vec3<f32>(0.75, 0.82, 1.0), vec3<f32>(1.0, 0.88, 0.75), hash2(id + 5.5));
    let s = cell / 9.0;
    return tint * b * tw * exp(-(d * d) / (0.9 * s * s));
}

fn srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs_comp(@builtin(position) fc: vec4<f32>) -> @location(0) vec4<f32> {
    let px = fc.xy;
    let tr = textureLoad(trail_tex, vec2<i32>(px), 0);
    let m = min(g.size.x, g.size.y);
    let uv = (px - 0.5 * g.size) / m;
    var col = mix(g.bg_center.rgb, g.bg_edge.rgb, smoothstep(0.0, 0.95, length(uv)));
    if (g.stars > 0.0) {
        col += starfield(px) * g.stars;
    }
    col += tr.x * g.colors[0].rgb + tr.y * g.colors[1].rgb + tr.z * g.colors[2].rgb;
    col += tr.w * 0.18 * vec3<f32>(1.0);
    for (var k = 0; k < 3; k++) {
        let bp = to_px(g.bodies[k].xy);
        let d = length(px - bp);
        let c = g.colors[k].rgb * g.bodies[k].z;
        let hr = g.body_halo_px;
        let core = 1.0 - smoothstep(g.body_core_px - 0.9, g.body_core_px + 0.9, d);
        let halo = exp(-(d * d) / (hr * hr));
        let tail = 0.16 * hr * hr / (d * d + hr * hr);
        col += c * (1.5 * halo + tail) + mix(c, vec3<f32>(g.bodies[k].z), 0.7) * core * 2.8;
    }
    col *= g.fade;
    var mapped = vec3<f32>(1.0) - exp(-col * g.exposure);
    if (g.encode_srgb > 0.5) {
        // encode ourselves, then dither in the encoded domain to break up banding in the darks
        mapped = srgb(mapped) + (hash2(px) - 0.5) / 255.0;
    }
    return vec4<f32>(mapped, 1.0);
}
