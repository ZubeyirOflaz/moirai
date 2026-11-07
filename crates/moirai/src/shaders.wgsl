// Moirai shaders: trails (smoke plumes, glowing lines or stardust motes), a starfield drawn
// once per size, and a single composite pass.

struct Globals {
    size: vec2<f32>,        // target size in pixels
    center: vec2<f32>,      // world point at the centre of the frame
    scale: f32,             // pixels per world unit
    fade: f32,              // brightness of trails and bodies (scene transitions)
    time: f32,              // seconds, for twinkling and churning
    core_px: f32,           // line core half-width
    glow_px: f32,           // line glow radius
    glow_amp: f32,
    body_core_px: f32,
    body_halo_px: f32,
    encode_srgb: f32,       // 1 when the target is a linear (non-sRGB) format
    exposure: f32,
    stars: f32,             // starfield brightness
    line_core: f32,         // crisp line core (0 for stardust, which keeps only the glow)
    dust_px: f32,           // typical mote radius
    dust_spread_px: f32,    // how far motes drift by the end of the trail
    dust_gain: f32,         // mote brightness
    dust_hot: f32,          // white-hot share of freshly shed motes
    core_span: f32,         // lines fade out over this share of the trail
    smoke_on: f32,
    smoke_w0: f32,          // plume half-width at birth (px)
    smoke_w1: f32,          // plume half-width at the end of the trail (px)
    smoke_drift: f32,       // sideways drift of the oldest smoke (px)
    smoke_freq: f32,        // noise tiles per world unit of path
    smoke_gain: f32,
    smoke_hot: f32,
    smoke_flow: f32,        // how far fresh plasma streams back from the body (px)
    pad0: f32,
    pad1: f32,
    pad2: f32,
    colors: array<vec4<f32>, 3>,
    bodies: array<vec4<f32>, 3>,   // xy: position (world), z: brightness
    bg_center: vec4<f32>,
    bg_edge: vec4<f32>,
    planet: vec4<f32>,             // xy: position (world), z: 1 when shown
    planet_color: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;

fn to_px(w: vec2<f32>) -> vec2<f32> {
    return vec2<f32>(g.size.x * 0.5 + (w.x - g.center.x) * g.scale,
                     g.size.y * 0.5 - (w.y - g.center.y) * g.scale);
}

fn to_clip(p: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(p.x / g.size.x * 2.0 - 1.0, 1.0 - p.y / g.size.y * 2.0, 0.0, 1.0);
}

// Corners of a quad as two triangles, in [-1, 1]^2.
fn quad_corner(vi: u32) -> vec2<f32> {
    var c = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0));
    return c[vi];
}

// Corner `vi` of a rectangle around segment a-b: r to either side, `back` before a and `ahead`
// past b.
fn segment_corner(a: vec2<f32>, b: vec2<f32>, r: f32, back: f32, ahead: f32, vi: u32) -> vec2<f32> {
    var dir = b - a;
    let len = length(dir);
    if (len < 1e-4) {
        dir = vec2<f32>(1.0, 0.0);
    } else {
        dir = dir / len;
    }
    let nrm = vec2<f32>(-dir.y, dir.x);
    let c = quad_corner(vi);
    let along = select(-back, len + ahead, c.x > 0.0);
    return a + dir * along + nrm * c.y * r;
}

// Corner `vi` of a rectangle around segment a-b, padded by r on every side.
fn capsule_corner(a: vec2<f32>, b: vec2<f32>, r: f32, vi: u32) -> vec2<f32> {
    return segment_corner(a, b, r, r, r, vi);
}

// Write intensity `v` into the channel of `body`, and `hot` into the white-hot channel.
fn body_channel(body: u32, v: f32, hot: f32) -> vec4<f32> {
    var out = vec4<f32>(0.0, 0.0, 0.0, hot);
    if (body == 0u) {
        out.x = v;
    } else if (body == 1u) {
        out.y = v;
    } else {
        out.z = v;
    }
    return out;
}

// One merged piece of a trail. `s` is the path length at both ends (world units).
struct Seg {
    @location(0) p0: vec2<f32>,
    @location(1) p1: vec2<f32>,
    @location(2) ages: vec2<f32>,
    @location(3) s: vec2<f32>,
    @location(4) body: u32,
    @location(5) caps: vec2<f32>, // reach past each end as a share of the width (smoke only)
};

// ---------------------------------------------------------------- line trails

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
    let p = capsule_corner(a, b, g.core_px + 2.6 * g.glow_px + 1.0, vi); // glow < 1/1000 beyond
    var o: TrailOut;
    o.pos = to_clip(p);
    if (min(s.ages.x, s.ages.y) >= g.core_span) {
        o.pos = vec4<f32>(2.0, 2.0, 0.0, 1.0); // entirely past the line's span: draw nothing
    }
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
    let life = 1.0 - mix(i.ages.x, i.ages.y, t) / g.core_span;
    if (life <= 0.0) {
        discard;
    }
    let fade = life * life * (3.0 - 2.0 * life);
    let w = g.core_px * (0.35 + 0.65 * life);
    let core = 1.0 - smoothstep(w - 0.7, w + 0.7, d);
    let glow = g.glow_amp * exp(-(d * d) / (g.glow_px * g.glow_px));
    let c = g.line_core * core;
    return body_channel(i.body, fade * (c + glow), fade * c);
}

// ---------------------------------------------------------------- smoke

@group(1) @binding(0) var noise_tex: texture_3d<f32>;
@group(1) @binding(1) var noise_smp: sampler;

// Half-width of the plume: slim near the body, spreading as it ages.
fn smoke_width(age: f32) -> f32 {
    return mix(g.smoke_w0, g.smoke_w1, pow(max(age, 0.0), 0.9));
}

// Overlap of neighbouring smoke segments past their joints, in plume half-widths.
const SMOKE_CAP_MARGIN: f32 = 1.0;

// How far the oldest smoke may have drifted sideways.
fn smoke_drift_amp(age: f32) -> f32 {
    return g.smoke_drift * age * sqrt(age);
}

// The noise volume as unit-variance gradient noise: features of a quarter, an eighth and a
// sixteenth of a tile in x, y and z, and an independent quarter in w.
fn noise4(p: vec3<f32>) -> vec4<f32> {
    return (textureSampleLevel(noise_tex, noise_smp, p, 0.0) - vec4<f32>(0.5)) * 6.6666667;
}

// Turbulence keeps its eddies about as large as the plume is wide: features of `l` px count
// fully in a plume whose eddies are `l` px, and less and less over an octave or two either
// way (a broad mix of sizes, so that nothing looks regular). The weights of the octaves of
// one noise sample (features of l, l/2 and l/4 px), given x = log2(l / e).
fn octaves(x: f32) -> vec3<f32> {
    let d = vec3<f32>(x, x - 1.0, x - 2.0);
    return exp(-0.5 * d * d);
}

// Those octaves of noise sample `n`, weighted by `k`: the weighted sum and the sum of squared
// weights (which normalises the variance).
fn eddies(n: vec4<f32>, k: vec3<f32>) -> vec2<f32> {
    return vec2<f32>(dot(k, n.xyz), dot(k, k));
}

struct SmokeOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) px: vec2<f32>,
    @location(1) @interpolate(flat) a: vec2<f32>,
    @location(2) @interpolate(flat) b: vec2<f32>,
    @location(3) @interpolate(flat) ages: vec2<f32>,
    @location(4) @interpolate(flat) s: vec2<f32>,
    @location(5) @interpolate(flat) body: u32,
};

// Smoke is drawn in two layers: the young part, with its fine detail, at full resolution, and the
// older, softer and much wider part at half resolution (a quarter of the pixels to shade). They
// overlap over a stretch of the trail where one fades in and the other fades out.
const SMOKE_FAR_IN: f32 = 0.15;   // the half-resolution layer starts here...
const SMOKE_FAR_FULL: f32 = 0.25; // ...and is complete from here on
const SMOKE_NEAR_END: f32 = 0.4;  // the full-resolution layer has faded out by here

const OFFSCREEN: vec4<f32> = vec4<f32>(2.0, 2.0, 0.0, 1.0); // outside the clip volume: nothing is drawn

@vertex
fn vs_smoke_near(@builtin(vertex_index) vi: u32, s: Seg) -> SmokeOut {
    var o = smoke_vertex(vi, s);
    if (min(s.ages.x, s.ages.y) >= SMOKE_NEAR_END) {
        o.pos = OFFSCREEN;
    }
    return o;
}

@vertex
fn vs_smoke_far(@builtin(vertex_index) vi: u32, s: Seg) -> SmokeOut {
    var o = smoke_vertex(vi, s);
    if (max(s.ages.x, s.ages.y) < SMOKE_FAR_IN) {
        o.pos = OFFSCREEN;
    }
    return o;
}

@fragment
fn fs_smoke_near(i: SmokeOut) -> @location(0) vec4<f32> {
    return smoke(i, true);
}

@fragment
fn fs_smoke_far(i: SmokeOut) -> @location(0) vec4<f32> {
    return smoke(i, false);
}

fn smoke_vertex(vi: u32, s: Seg) -> SmokeOut {
    let a = to_px(s.p0);
    let b = to_px(s.p1);
    let age = max(s.ages.x, s.ages.y);
    // the widest puff (2.5 local half-widths, which swell up to 1.82x) and the furthest drift
    let r = 4.6 * smoke_width(age) + smoke_drift_amp(age) + 2.0;
    // past the ends only as far as this segment's share of a bend needs, plus some overlap
    // with the neighbour (which draws the same smoke beyond that)
    let margin = 2.0 + SMOKE_CAP_MARGIN * smoke_width(age);
    let p = segment_corner(a, b, r, r * s.caps.x + margin, r * s.caps.y + margin, vi);
    var o: SmokeOut;
    o.pos = to_clip(p);
    o.px = p;
    o.a = a;
    o.b = b;
    o.ages = s.ages;
    o.s = s.s;
    o.body = s.body;
    return o;
}

// The smoke at this pixel for the near (full resolution) or far (half resolution) layer.
fn smoke(i: SmokeOut, near: bool) -> vec4<f32> {
    // Coordinates pinned to the path: u along it (in noise periods) and the distance across it
    // (px), sampled at the same scale both ways so that every feature is round, not streaked.
    // Every along-path frequency is a multiple of 1/4: the CPU rebases u by multiples of 4.
    let ab = i.b - i.a;
    let len = max(length(ab), 1e-3);
    let dir = ab / len;
    let rel = i.px - i.a;
    let proj = dot(rel, dir);
    let t = clamp(proj / len, 0.0, 1.0);
    let age = mix(i.ages.x, i.ages.y, t);
    if (age >= 1.0 || (near && age >= SMOKE_NEAR_END) || (!near && age < SMOKE_FAR_IN)) {
        discard;
    }
    var layer = smoothstep(SMOKE_FAR_IN, SMOKE_FAR_FULL, age);
    if (near) {
        layer = 1.0 - smoothstep(SMOKE_FAR_FULL, SMOKE_NEAR_END, age);
    }
    let beyond = proj - t * len; // px past either end of the segment
    let u = (mix(i.s.x, i.s.y, t) + beyond / g.scale) * g.smoke_freq;
    let pp = g.scale / g.smoke_freq; // px per noise period
    let w = smoke_width(age);
    let seed = f32(i.body) * 0.29;
    // log2 of the noise's quarter period and of the eddy size (2.2 plume half-widths), from
    // which every octave weight follows
    let lq = log2(0.25 * pp);
    let le = log2(2.2 * w);

    // freshly shed plasma streams back from the body and slows down, then only churns in place
    let uf = u + g.smoke_flow / pp * (1.0 - exp(-8.0 * age));

    // Along the path, the older plume meanders sideways (features of a period and more) and
    // swells into puffs and narrows between them (features a few plume widths long).
    let big = noise4(vec3<f32>(uf * 0.25, 0.31 + seed, g.time * 0.011));
    let mid = noise4(vec3<f32>(uf, 0.77 + seed, g.time * 0.019));
    let ls = le + 0.862496; // log2 of 4 half-widths: 4 / 2.2
    let sa = eddies(big, octaves(lq + 2.0 - ls));
    let sb = eddies(mid, octaves(lq - ls));
    let swell = (sa.x + sb.x) * inverseSqrt(sa.y + sb.y + 1e-3);
    let wl = w * exp(0.3 * clamp(swell, -2.5, 2.0));
    let lat = (dir.x * rel.y - dir.y * rel.x) - clamp(0.5 * big.w, -1.0, 1.0) * smoke_drift_amp(age);
    let r2 = (lat * lat + beyond * beyond) / (wl * wl);
    if (r2 > 6.25) {
        discard;
    }

    // large eddies (features of pp/4 to pp/16): lumps on the plume's edges, and the swirl
    // that carries the fine eddies around
    let e = 2.2 * w; // typical eddy size
    let q1 = vec3<f32>(uf, lat / pp + 0.37 + seed, g.time * 0.021);
    let n1 = noise4(q1);
    let n1b = noise4(q1 + vec3<f32>(0.5, 0.23, 0.61));
    let s1 = eddies(n1, octaves(lq - le));
    let se = eddies(n1, octaves(lq - le - 0.678072)); // eddies 1.6 times larger
    let lumps = se.x * inverseSqrt(se.y + 1e-3);
    let kw = octaves(lq - le - 1.321928); // eddies 2.5 times larger
    let wx = eddies(n1, kw);
    let wy = eddies(n1b, kw);
    let swirl = vec2<f32>(wx.x * inverseSqrt(wx.y + 1e-3), wy.x * inverseSqrt(wy.y + 1e-3)) * (0.35 * e);

    // fine eddies (pp/16 to pp/64): soft variations of density inside the plume
    let q2 = vec3<f32>(4.0 * (uf + swirl.x / pp), 4.0 * (lat + swirl.y) / pp + 0.71 + seed, g.time * 0.033 + 0.5);
    let s2 = eddies(noise4(q2), octaves(lq - 2.0 - le));
    let billow = (s1.x + s2.x) * inverseSqrt(s1.y + s2.y + 1e-3); // about unit variance

    // a soft plume with lumpy edges and denser puffs; late in life it gathers into clouds
    let r = sqrt(r2) * (1.0 - 0.15 * clamp(lumps, -2.5, 2.0));
    let prof = exp(-1.6 * r * r);
    let clump = mix(1.0, smoothstep(-1.0, 0.8, big.y + 0.4), 0.7 * smoothstep(0.35, 1.0, age));
    let dens = prof * exp(0.4 * billow - 0.08) * clump * sqrt(w / wl);
    let halo = 0.10 * exp(-0.3 * r2); // the soft glow around the plasma
    let edge = smoothstep(6.25, 3.5, r2);

    // it thins as it spreads, and fades out by the end of the trail
    let life = 1.0 - age;
    let thin = pow(g.smoke_w0 / w, 0.5);
    let light = g.smoke_gain * (dens + halo) * edge * life * life * thin;
    return body_channel(i.body, light * layer, g.smoke_hot * dens * edge * exp(-5.0 * age) * layer);
}

// ---------------------------------------------------------------- stardust

struct MoteIn {
    @location(0) p: vec2<f32>,
    @location(1) age: f32,      // through its life (0 = just shed, 1 = gone)
    @location(2) word: u32,
    @location(3) drift: f32,    // its age by time alone
};

struct MoteOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) radius: f32,
    @location(2) @interpolate(flat) amp: f32,
    @location(3) @interpolate(flat) hot: f32,
    @location(4) @interpolate(flat) body: u32,
};

fn pcg(v: u32) -> u32 {
    let s = v * 747796405u + 2891336453u;
    let w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

fn unit(h: u32) -> f32 {
    return f32(h >> 8u) * (1.0 / 16777216.0);
}

@vertex
fn vs_mote(@builtin(vertex_index) vi: u32, m: MoteIn) -> MoteOut {
    // every property of a mote follows from its seed, so it is the same mote in every frame
    let h1 = pcg(m.word);
    let h2 = pcg(h1);
    let h3 = pcg(h2);
    let h4 = pcg(h3);
    let h5 = pcg(h4);
    let h6 = pcg(h5);
    let h7 = pcg(h6);
    let r_dir = unit(h1);
    let r_dist = unit(h2);
    let r_size = unit(h3);
    let r_flux = unit(h4);
    let r_rate = unit(h5);
    let r_phase = unit(h6);
    let age = m.age;
    // most motes glow for most of the trail; a few burn out a little earlier
    let own = age / (0.6 + 0.4 * unit(h7));
    let life = 1.0 - own;

    // drift away from the path, slowly at first, curling a little; most stay close to it
    let ang = 6.2831853 * r_dir + (r_phase - 0.5) * 3.0 * m.drift;
    let dist = g.dust_spread_px * (0.15 + 0.85 * r_dist * r_dist) * pow(m.drift, 1.5);
    let c = to_px(m.p) + vec2<f32>(cos(ang), sin(ang)) * dist;

    // fine grains and a few brighter sparks, shrinking as they burn out. Grains smaller than a
    // pixel are drawn about a pixel wide and correspondingly fainter, which keeps them steady.
    let size = g.dust_px * (0.6 + r_size * r_size * r_size) * (1.0 - 0.3 * own);
    let radius = sqrt(size * size + 0.36);
    let r2 = r_flux * r_flux;
    let spark = r2 * r2 * r2;
    let flux = (0.6 + 3.0 * spark) * size * size / (radius * radius);
    let twinkle = 0.6 + 0.4 * sin(g.time * (0.8 + 3.2 * r_rate) + 6.2831853 * r_phase);
    let fade = life * life * (3.0 - 2.0 * life);
    let amp = g.dust_gain * flux * twinkle * fade;

    let reach = 2.6 * radius + 1.0;
    let corner = quad_corner(vi);
    var o: MoteOut;
    o.pos = to_clip(c + corner * reach);
    if (own >= 1.0 || amp < 1e-4) {
        o.pos = vec4<f32>(2.0, 2.0, 0.0, 1.0); // outside the clip volume: nothing is drawn
    }
    o.local = corner * reach;
    o.radius = radius;
    o.amp = amp;
    o.hot = g.dust_hot * exp(-7.0 * m.drift);
    o.body = m.word & 3u;
    return o;
}

@fragment
fn fs_mote(i: MoteOut) -> @location(0) vec4<f32> {
    let v = i.amp * exp(-dot(i.local, i.local) / (i.radius * i.radius));
    return body_channel(i.body, v, v * i.hot);
}

// ---------------------------------------------------------------- planet

struct PlanetOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) px: vec2<f32>,
    @location(1) @interpolate(flat) a: vec2<f32>,
    @location(2) @interpolate(flat) b: vec2<f32>,
    @location(3) @interpolate(flat) ages: vec2<f32>,
};

// The planet's trail is a hairline, far finer than the suns' threads.
fn planet_core_px() -> f32 {
    return max(0.35 * g.core_px, 0.5);
}

fn planet_glow_px() -> f32 {
    return max(0.25 * g.glow_px, 1.2);
}

@vertex
fn vs_planet(@builtin(vertex_index) vi: u32, s: Seg) -> PlanetOut {
    let a = to_px(s.p0);
    let b = to_px(s.p1);
    let p = capsule_corner(a, b, planet_core_px() + 2.6 * planet_glow_px() + 1.0, vi);
    var o: PlanetOut;
    o.pos = to_clip(p);
    o.px = p;
    o.a = a;
    o.b = b;
    o.ages = s.ages;
    return o;
}

@fragment
fn fs_planet(i: PlanetOut) -> @location(0) vec4<f32> {
    let ab = i.b - i.a;
    let l2 = max(dot(ab, ab), 1e-6);
    let t = clamp(dot(i.px - i.a, ab) / l2, 0.0, 1.0);
    let d = length(i.px - (i.a + ab * t));
    let life = 1.0 - mix(i.ages.x, i.ages.y, t);
    if (life <= 0.0) {
        discard;
    }
    let fade = life * life * (3.0 - 2.0 * life);
    let w = planet_core_px() * (0.5 + 0.5 * life);
    let core = 1.0 - smoothstep(w - 0.6, w + 0.6, d);
    let gw = planet_glow_px();
    let glow = 0.12 * exp(-(d * d) / (gw * gw));
    return vec4<f32>(fade * (0.45 * core + glow), 0.0, 0.0, 0.0);
}

// ---------------------------------------------------------------- starfield (drawn once per size)

struct StarIn {
    @location(0) pos: vec2<f32>,
    @location(1) radius: f32,
    @location(2) halo: f32,
    @location(3) color: vec3<f32>,
    @location(4) phase: f32,
};

struct StarOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) radius: f32,
    @location(2) @interpolate(flat) halo: f32,
    @location(3) @interpolate(flat) color: vec3<f32>,
    @location(4) @interpolate(flat) phase: f32,
};

@vertex
fn vs_star(@builtin(vertex_index) vi: u32, s: StarIn) -> StarOut {
    let reach = s.radius * select(3.0, 9.0, s.halo > 0.0);
    let corner = quad_corner(vi);
    var o: StarOut;
    o.pos = to_clip(s.pos + corner * reach);
    o.local = corner * reach;
    o.radius = s.radius;
    o.halo = s.halo;
    o.color = s.color;
    o.phase = s.phase;
    return o;
}

@fragment
fn fs_star(i: StarOut) -> @location(0) vec4<f32> {
    let d2 = dot(i.local, i.local);
    let r2 = i.radius * i.radius;
    let v = exp(-d2 / r2) + i.halo * exp(-d2 / (9.0 * r2));
    // alpha carries the star's twinkle phase to the composite
    return vec4<f32>(i.color * v, select(0.0, i.phase, v > 0.02));
}

// ---------------------------------------------------------------- composite

@group(0) @binding(1) var trail_tex: texture_2d<f32>;
@group(0) @binding(2) var star_tex: texture_2d<f32>;
@group(0) @binding(3) var smoke_near_tex: texture_2d<f32>;
@group(0) @binding(4) var smoke_smp: sampler;
@group(0) @binding(5) var smoke_far_tex: texture_2d<f32>;
@group(0) @binding(6) var planet_tex: texture_2d<f32>;

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

fn srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

// Per-body intensities (xyz) and white heat (w) to light.
fn trail_light(t: vec4<f32>) -> vec3<f32> {
    return t.x * g.colors[0].rgb + t.y * g.colors[1].rgb + t.z * g.colors[2].rgb + vec3<f32>(t.w * 0.18);
}

@fragment
fn fs_comp(@builtin(position) fc: vec4<f32>) -> @location(0) vec4<f32> {
    let px = fc.xy;
    let ip = vec2<i32>(px);
    let m = min(g.size.x, g.size.y);
    let uv = (px - 0.5 * g.size) / m;
    var col = mix(g.bg_center.rgb, g.bg_edge.rgb, smoothstep(0.0, 0.95, length(uv)));
    if (g.stars > 0.0) {
        // the sky stays while scenes fade; each star breathes at its own slow pace
        let st = textureLoad(star_tex, ip, 0);
        let rate = 0.35 + 1.4 * fract(st.a * 17.13);
        let twinkle = 1.0 + 0.3 * sin(g.time * rate + 6.2831853 * st.a);
        col += st.rgb * (twinkle * g.stars);
    }
    var fg = trail_light(textureLoad(trail_tex, ip, 0));
    if (g.smoke_on > 0.5) {
        // the layers combine like overlapping smoke within one: the brighter wins
        let near = textureLoad(smoke_near_tex, ip, 0);
        let far = textureSampleLevel(smoke_far_tex, smoke_smp, px / g.size, 0.0);
        fg += trail_light(max(near, far));
    }
    for (var k = 0; k < 3; k++) {
        let bp = to_px(g.bodies[k].xy);
        let d = length(px - bp);
        let c = g.colors[k].rgb * g.bodies[k].z;
        let hr = g.body_halo_px;
        let core = 1.0 - smoothstep(g.body_core_px - 0.9, g.body_core_px + 0.9, d);
        let halo = exp(-(d * d) / (hr * hr));
        let tail = 0.16 * hr * hr / (d * d + hr * hr);
        fg += c * (1.5 * halo + tail) + mix(c, vec3<f32>(g.bodies[k].z), 0.7) * core * 2.8;
    }
    if (g.planet.z > 0.5) {
        // its trail, then the planet itself: a small bright dot with a little glow
        let pc = g.planet_color.rgb;
        fg += pc * textureLoad(planet_tex, ip, 0).r;
        let d = length(px - to_px(g.planet.xy));
        let r = max(0.4 * g.body_core_px, 1.2);
        let hr = max(0.25 * g.body_halo_px, 2.5);
        // a ring of night sky around it keeps it visible in front of a bright sun
        fg = fg * (1.0 - 0.85 * (1.0 - smoothstep(r + 1.0, r + 2.5, d)));
        let core = 1.0 - smoothstep(r - 0.7, r + 0.7, d);
        fg += pc * (0.8 * exp(-(d * d) / (hr * hr))) + mix(pc, vec3<f32>(1.0), 0.6) * core * 2.0;
    }
    col += fg * g.fade;
    var mapped = vec3<f32>(1.0) - exp(-col * g.exposure);
    if (g.encode_srgb > 0.5) {
        // encode ourselves, then dither in the encoded domain to break up banding in the darks
        mapped = srgb(mapped) + (hash2(px) - 0.5) / 255.0;
    }
    return vec4<f32>(mapped, 1.0);
}

// ---------------------------------------------------------------- readout (over the finished frame)

@group(1) @binding(2) var hud_atlas: texture_2d<f32>;
@group(1) @binding(3) var hud_smp: sampler;

struct HudIn {
    @location(0) rect: vec4<f32>,   // px: left, top, right, bottom (a line: its two ends)
    @location(1) uv: vec4<f32>,     // atlas: left, top, right, bottom (a line: half its width)
    @location(2) color: vec4<f32>,  // linear RGB, straight alpha
    @location(3) kind: u32,         // 0 glyph, 1 solid, 2 round dot, 3 line
};

struct HudOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) local: vec2<f32>,
    @location(2) @interpolate(flat) color: vec4<f32>,
    @location(3) @interpolate(flat) kind: u32,
    @location(4) px: vec2<f32>,
    @location(5) @interpolate(flat) ends: vec4<f32>,
    @location(6) @interpolate(flat) half_width: f32,
};

@vertex
fn vs_hud(@builtin(vertex_index) vi: u32, q: HudIn) -> HudOut {
    let c = quad_corner(vi);
    let f = 0.5 * (c + vec2<f32>(1.0));
    var p = mix(q.rect.xy, q.rect.zw, f);
    if (q.kind == 3u) {
        p = capsule_corner(q.rect.xy, q.rect.zw, q.uv.x + 1.0, vi); // round caps, a pixel to fade
    }
    var o: HudOut;
    o.pos = to_clip(p);
    o.uv = mix(q.uv.xy, q.uv.zw, f);
    o.local = c;
    o.color = q.color;
    o.kind = q.kind;
    o.px = p;
    o.ends = q.rect;
    o.half_width = q.uv.x;
    return o;
}

@fragment
fn fs_hud(i: HudOut) -> @location(0) vec4<f32> {
    var a = i.color.a;
    if (i.kind == 0u) {
        a = a * textureSampleLevel(hud_atlas, hud_smp, i.uv, 0.0).r;
    } else if (i.kind == 2u) {
        a = a * (1.0 - smoothstep(0.55, 1.0, length(i.local)));
    } else if (i.kind == 3u) {
        let ab = i.ends.zw - i.ends.xy;
        let t = clamp(dot(i.px - i.ends.xy, ab) / max(dot(ab, ab), 1e-6), 0.0, 1.0);
        let d = length(i.px - (i.ends.xy + ab * t));
        a = a * clamp(i.half_width + 0.5 - d, 0.0, 1.0);
    }
    var rgb = i.color.rgb;
    if (g.encode_srgb > 0.5) {
        rgb = srgb(rgb); // the target stores encoded values: blend in that space, as UI usually is
    }
    return vec4<f32>(rgb * a, a);
}
