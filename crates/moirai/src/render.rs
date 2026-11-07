//! Clotho: turns the bodies' recent history into light.
//!
//! Trails go into HDR textures with one channel per body plus a white-hot channel, in one of
//! three styles:
//! * smoke: each body sheds a glowing plume that streams back, widens into billowing puffs,
//!   meanders and slowly churns. It is shaded from a small tileable noise volume in coordinates
//!   pinned to the path (path length along it, distance across it), with eddies that grow with
//!   the plume, under a thin hot filament near the body;
//! * lines: glowing threads, merged into the longest segments that stay on the path and drawn
//!   with *max* blending (smooth, no bright beads at the joints, no blur passes);
//! * dust: fine motes pinned to places on the path that slowly drift apart, twinkle and fade
//!   (sooner where the path curls tightly), blended additively, so each mote is the same in
//!   every frame without any particle state.
//!
//! The starfield is drawn once per size. One composite pass adds background, stars, trails and
//! bodies, then tone maps.

use bytemuck::{Pod, Zeroable};
use moirai_core::rng::Rng;

use crate::hud::{Hud, HudQuad, temperature_color};

pub struct Gpu {
    /// Kept alive alongside the device and any surfaces created from it.
    #[allow(dead_code)]
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

pub fn instance() -> wgpu::Instance {
    // The display handle only matters for GL on Wayland; Vulkan (our path) ignores it.
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::VULKAN | wgpu::Backends::GL;
    wgpu::Instance::new(desc)
}

impl Gpu {
    pub fn new(instance: wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> Result<Self, String> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: surface,
            ..Default::default()
        }))
        .map_err(|e| format!("no usable GPU adapter: {e}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("moirai"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .map_err(|e| format!("could not open the GPU: {e}"))?;
        Ok(Self { instance, adapter, device, queue })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub name: &'static str,
    /// Linear RGB per body.
    pub bodies: [[f32; 3]; 3],
    /// Part of the default rotation (the others are only used when asked for by name).
    pub featured: bool,
}

/// Featured palettes are analogous: two saturated neighbouring hues plus one pale member.
pub const PALETTES: &[Palette] = &[
    Palette { name: "solar", bodies: [[1.00, 0.70, 0.20], [1.00, 0.32, 0.14], [1.00, 0.90, 0.62]], featured: true },
    Palette { name: "moonlight", bodies: [[0.82, 0.86, 1.00], [0.40, 0.62, 1.00], [0.72, 0.50, 1.00]], featured: true },
    Palette { name: "aurora", bodies: [[0.22, 1.00, 0.52], [0.12, 0.70, 1.00], [0.74, 1.00, 0.88]], featured: true },
    Palette { name: "ember-ice", bodies: [[1.00, 0.42, 0.10], [0.18, 0.58, 1.00], [0.95, 0.24, 0.58]], featured: false },
    Palette { name: "nebula", bodies: [[1.00, 0.30, 0.52], [0.30, 0.82, 1.00], [0.66, 0.40, 1.00]], featured: false },
];

pub fn palette_index(name: &str) -> Option<usize> {
    PALETTES.iter().position(|p| p.name.eq_ignore_ascii_case(name))
}

const BG_CENTER: [f32; 4] = [0.0060, 0.0068, 0.0130, 1.0];
const BG_EDGE: [f32; 4] = [0.0010, 0.0010, 0.0022, 1.0];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrailStyle {
    /// Glowing plumes of plasma that widen, fray and drift.
    Smoke,
    /// Continuous glowing threads (the cheapest).
    Lines,
    /// Motes shed along the path that drift apart, twinkle and fade.
    Dust,
}

impl TrailStyle {
    pub fn next(self) -> Self {
        match self {
            Self::Smoke => Self::Lines,
            Self::Lines => Self::Dust,
            Self::Dust => Self::Smoke,
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "smoke" | "plasma" => Some(Self::Smoke),
            "lines" | "line" => Some(Self::Lines),
            "dust" | "stardust" | "particles" => Some(Self::Dust),
            _ => None,
        }
    }
}

/// Viewer choices that can change while running.
#[derive(Clone, Copy, Debug)]
pub struct Look {
    pub style: TrailStyle,
    /// Starfield brightness (0 = none).
    pub stars: f32,
    /// Show the planet (see `moirai_core::planet`).
    pub planet: bool,
}

/// Dust motes are shed every `DUST_SPACING * radius` of path, where `radius` is the scene's
/// framing radius: about 0.4 px apart on an 800 px tall frame.
const DUST_SPACING: f64 = 0.0009;
/// A stardust trail is cut short where its path has turned this far (radians): a body circling
/// quickly keeps only its last loop and a half, which would otherwise pile up into a busy knot,
/// while one on a long, gentle arc keeps its whole trail.
const DUST_TURNING: f32 = 3.0 * std::f32::consts::PI;
/// When that cut moves closer (a sharp turn, such as a slingshot past another body, uses up much
/// of the allowance at once), the trail shrinks toward it by at most this share of its full
/// length per second instead of losing its tail at once.
const DUST_RETRACT: f32 = 0.3;
/// The smoke's noise period along the path is `SMOKE_PERIOD * radius` (about 200 px on an 800
/// px tall frame). The shader samples the noise at several multiples of it, with features from
/// a whole period (the slow meander) down to 1/64 of one (the finest eddies near the body).
const SMOKE_PERIOD: f64 = 0.5;
/// The shader also samples noise at a quarter and a half of the path frequency, which repeat
/// only every 4 periods: path coordinates may be shifted by multiples of this many periods.
const SMOKE_REBASE: f64 = 4.0;

/// One point of a body's recent path; `age` runs from 0 (now) to 1 (end of the trail).
#[derive(Clone, Copy, Debug)]
pub struct TrailPoint {
    pub p: [f32; 2],
    pub age: f32,
    /// Path length travelled since the scene began (pins dust and smoke to the path).
    pub s: f64,
}

pub struct FrameInput<'a> {
    /// Oldest first.
    pub trails: [&'a [TrailPoint]; 3],
    pub bodies: [[f32; 2]; 3],
    /// World point at the centre of the frame and the radius that fits the shorter side.
    pub center: [f32; 2],
    pub radius: f32,
    pub palette: &'a Palette,
    pub fade: f32,
    pub time: f32,
    pub look: Look,
    /// The scene's nominal framing radius, fixed for its lifetime: effects pinned to the path
    /// (dust spacing, smoke texture) scale with it.
    pub path_radius: f64,
    /// Per-scene salt so that every scene has its own dust.
    pub salt: u32,
    /// The planet, when it is shown.
    pub planet: Option<PlanetView<'a>>,
}

/// The planet as drawn: its recent path (oldest first; `age` runs over the trail), where it is,
/// which planet of the scene it is (from 1) and what it has lived through.
pub struct PlanetView<'a> {
    pub trail: &'a [TrailPoint],
    pub pos: [f32; 2],
    pub number: u32,
    pub chronicle: moirai_core::planet::Chronicle,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    size: [f32; 2],
    center: [f32; 2],
    scale: f32,
    fade: f32,
    time: f32,
    core_px: f32,
    glow_px: f32,
    glow_amp: f32,
    body_core_px: f32,
    body_halo_px: f32,
    encode_srgb: f32,
    exposure: f32,
    stars: f32,
    line_core: f32,
    dust_px: f32,
    dust_spread_px: f32,
    dust_gain: f32,
    dust_hot: f32,
    core_span: f32,
    smoke_on: f32,
    smoke_w0: f32,
    smoke_w1: f32,
    smoke_drift: f32,
    smoke_freq: f32,
    smoke_gain: f32,
    smoke_hot: f32,
    smoke_flow: f32,
    _pad: [f32; 3],
    colors: [[f32; 4]; 3],
    bodies: [[f32; 4]; 3],
    bg_center: [f32; 4],
    bg_edge: [f32; 4],
    /// xy: the planet's position (world), z: 1 when it is shown.
    planet: [f32; 4],
    planet_color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Seg {
    p0: [f32; 2],
    p1: [f32; 2],
    ages: [f32; 2],
    /// Path length at both ends (world units, relative to a whole number of noise periods).
    s: [f32; 2],
    /// How far the smoke must reach past each end, as a share of its width: the sine of half
    /// the turn at that joint (1 at the ends of the trail).
    caps: [f32; 2],
    body: u32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Mote {
    /// Where on the path it was shed (world units).
    p: [f32; 2],
    /// How far through its life it is (0 = just shed, 1 = gone).
    age: f32,
    /// Its age by time alone, which sets how far it has drifted.
    drift: f32,
    /// Hash seed in the upper 30 bits, body in the lower 2.
    word: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Star {
    pos: [f32; 2],
    radius: f32,
    halo: f32,
    color: [f32; 3],
    phase: f32,
}

/// Trail segments are as long as possible up to this length, as long as they stay within
/// `MAX_SAG_PX` of every trail point they replace. Each segment is drawn as a quad as wide as
/// its glow or smoke, so fewer, longer segments mean far less overdraw. The tolerance must stay
/// this tight even for wide smoke: the segmentation is rebuilt every frame as the trail slides,
/// and a looser chord would make anything shaded in path coordinates shimmer.
const MAX_SEG_PX: f32 = 40.0;
const MAX_SAG_PX: f32 = 0.3;
/// Segments always end at the first trail point past every multiple of `SEG_ANCHOR * radius`
/// of path (about 32 px on an 800 px tall frame), see `segment_end`.
const SEG_ANCHOR: f64 = 0.08;
const TRAIL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// The planet's trail: one intensity channel, tinted with the planet's colour in the composite.
const PLANET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R16Float;

/// While the planet is shown the suns' glow is this much smaller, so that it does not drown a
/// planet circling close to its sun (most of all when a dance zooms out).
const PLANET_SUN_GLOW: f32 = 0.55;
const STAR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// Edge of the smoke noise volume in texels.
const NOISE_SIZE: u32 = 64;

/// Stars in the square that circumscribes the frame (about 1800 visible at 16:10).
const STAR_COUNT: usize = 2900;
/// Faintest star, and the power law of brightness above it (many faint, few bright).
const STAR_MIN: f32 = 0.03;
const STAR_SLOPE: f32 = 0.8;
const STAR_MAX: f32 = 1.6;

/// Smoke half-widths at birth and at the end of the trail, the sideways drift of the oldest
/// smoke and how far fresh plasma streams back, as fractions of the frame's shorter side.
const SMOKE_W0: f32 = 0.0025;
const SMOKE_W1: f32 = 0.020;
const SMOKE_DRIFT: f32 = 0.014;
const SMOKE_FLOW: f32 = 0.03;
/// The smoke's older, softer part is drawn at 1/SMOKE_FAR_DIV of the frame's resolution (the
/// young part, with its fine detail, at full resolution; see the shader for where they meet).
const SMOKE_FAR_DIV: u32 = 2;
/// The crisp hot filament under the smoke fades out over this share of the trail, and the soft
/// glow under the stardust over this one (the oldest motes sparkle on their own).
const SMOKE_CORE_SPAN: f32 = 0.25;
const DUST_GLOW_SPAN: f32 = 0.6;

fn smoke_width(m: f32, age: f32) -> f32 {
    m * (SMOKE_W0 + (SMOKE_W1 - SMOKE_W0) * age.max(0.0).powf(0.9))
}

/// A grow-only GPU buffer of instances.
struct Instances<T: Pod> {
    data: Vec<T>,
    buf: wgpu::Buffer,
    cap: usize,
    label: &'static str,
}

impl<T: Pod> Instances<T> {
    fn new(dev: &wgpu::Device, label: &'static str, cap: usize) -> Self {
        Self { data: Vec::with_capacity(cap), buf: Self::alloc(dev, label, cap), cap, label }
    }

    fn alloc(dev: &wgpu::Device, label: &'static str, cap: usize) -> wgpu::Buffer {
        dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: (cap * std::mem::size_of::<T>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn upload(&mut self, gpu: &Gpu) {
        if self.data.len() > self.cap {
            self.cap = self.data.len().next_power_of_two();
            self.buf = Self::alloc(&gpu.device, self.label, self.cap);
        }
        if !self.data.is_empty() {
            gpu.queue.write_buffer(&self.buf, 0, bytemuck::cast_slice(&self.data));
        }
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, pipeline: &wgpu::RenderPipeline) {
        if !self.data.is_empty() {
            pass.set_pipeline(pipeline);
            pass.set_vertex_buffer(0, self.buf.slice(..));
            pass.draw(0..6, 0..self.data.len() as u32);
        }
    }
}

/// Size-dependent render targets and the composite bind group that reads them.
struct Targets {
    trail: wgpu::TextureView,
    /// Young smoke at full resolution, older smoke at a lower one.
    smoke_near: wgpu::TextureView,
    smoke_far: wgpu::TextureView,
    planet: wgpu::TextureView,
    stars: wgpu::TextureView,
    comp_bg: wgpu::BindGroup,
}

pub struct Renderer {
    width: u32,
    height: u32,
    encode_srgb: bool,
    globals: wgpu::Buffer,
    line_pipeline: wgpu::RenderPipeline,
    dust_pipeline: wgpu::RenderPipeline,
    smoke_near_pipeline: wgpu::RenderPipeline,
    smoke_far_pipeline: wgpu::RenderPipeline,
    planet_pipeline: wgpu::RenderPipeline,
    star_pipeline: wgpu::RenderPipeline,
    comp_pipeline: wgpu::RenderPipeline,
    globals_bg: wgpu::BindGroup,
    noise_bg: wgpu::BindGroup,
    comp_bgl: wgpu::BindGroupLayout,
    smoke_sampler: wgpu::Sampler,
    targets: Targets,
    /// The starfield texture must be (re)drawn before the next composite.
    stars_dirty: bool,
    segs: Instances<Seg>,
    motes: Instances<Mote>,
    stars: Instances<Star>,
    planet_segs: Instances<Seg>,
    /// The planet's readout: layout and glyph atlas, its pipeline, atlas binding and quads.
    hud: Hud,
    hud_pipeline: wgpu::RenderPipeline,
    hud_bgl: wgpu::BindGroupLayout,
    hud_bg: wgpu::BindGroup,
    hud_quads: Instances<HudQuad>,
    /// Age of every trail point as drawn (scratch, rebuilt every frame).
    ages: [Vec<f32>; 3],
    /// Share of each stardust trail shown, and the time and scene it was last updated for.
    dust_reach: [f32; 3],
    dust_clock: Option<(f32, u32)>,
}

fn uniform_entry(binding: u32, vis: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: vis,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn texture_entry(binding: u32, dim: wgpu::TextureViewDimension, filterable: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable },
            view_dimension: dim,
            multisampled: false,
        },
        count: None,
    }
}

fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn linear_sampler(dev: &wgpu::Device, label: &str, address_mode: wgpu::AddressMode) -> wgpu::Sampler {
    dev.create_sampler(&wgpu::SamplerDescriptor {
        label: Some(label),
        address_mode_u: address_mode,
        address_mode_v: address_mode,
        address_mode_w: address_mode,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    })
}

/// An instanced quad pipeline (6 vertices per instance) drawing into an HDR texture.
#[allow(clippy::too_many_arguments)]
fn instanced_pipeline(
    dev: &wgpu::Device,
    module: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    label: &str,
    entry: (&str, &str),
    stride: usize,
    attributes: &[wgpu::VertexAttribute],
    format: wgpu::TextureFormat,
    op: wgpu::BlendOperation,
) -> wgpu::RenderPipeline {
    let blend = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: op };
    dev.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some(entry.0),
            compilation_options: Default::default(),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: stride as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes,
            })],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(entry.1),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState { color: blend, alpha: blend }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn hdr_texture(dev: &wgpu::Device, label: &str, format: wgpu::TextureFormat, width: u32, height: u32) -> wgpu::TextureView {
    dev.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
    .create_view(&wgpu::TextureViewDescriptor::default())
}

/// Star colours by temperature, from blue-white through white to pale gold and orange.
fn star_tint(x: f32) -> [f32; 3] {
    const STOPS: [(f32, [f32; 3]); 5] = [
        (0.00, [0.70, 0.80, 1.00]),
        (0.30, [0.90, 0.94, 1.00]),
        (0.65, [1.00, 0.97, 0.92]),
        (0.88, [1.00, 0.88, 0.70]),
        (1.00, [1.00, 0.74, 0.50]),
    ];
    let i = STOPS.iter().position(|s| s.0 >= x).unwrap_or(STOPS.len() - 1).max(1);
    let (a, b) = (STOPS[i - 1], STOPS[i]);
    let t = ((x - a.0) / (b.0 - a.0)).clamp(0.0, 1.0);
    std::array::from_fn(|k| a.1[k] + (b.1[k] - a.1[k]) * t)
}

/// The same sky at every size: positions live in the square circumscribing the frame and
/// sizes scale with the shorter side. Brightness follows a power law, and brighter stars are
/// larger and carry a faint halo.
fn make_stars(width: u32, height: u32, out: &mut Vec<Star>) {
    out.clear();
    let (w, h) = (width as f32, height as f32);
    let side = w.max(h);
    let unit = w.min(h) / 800.0;
    let mut rng = Rng::new(0x5ee_d0f5_7a25);
    for _ in 0..STAR_COUNT {
        // draw every number for every star so the sky does not depend on the frame size
        let x = (rng.uniform() as f32 - 0.5) * side + 0.5 * w;
        let y = (rng.uniform() as f32 - 0.5) * side + 0.5 * h;
        let u = rng.uniform() as f32;
        let size = rng.uniform() as f32;
        let temperature = rng.uniform() as f32;
        let phase = rng.uniform() as f32;
        let b = (STAR_MIN * u.max(1e-6).powf(-STAR_SLOPE)).min(STAR_MAX);
        let mag = (b / STAR_MIN).log10(); // 0 for the faintest, ~1.7 for the brightest
        let radius = (unit * (0.5 + 0.38 * mag) * (0.7 + 0.6 * size)).max(0.5);
        let halo = 0.05 * (mag - 0.8).max(0.0);
        let reach = radius * if halo > 0.0 { 9.0 } else { 3.0 };
        if x < -reach || y < -reach || x > w + reach || y > h + reach {
            continue;
        }
        let tint = star_tint(temperature);
        out.push(Star {
            pos: [x.floor() + 0.5, y.floor() + 0.5],
            radius,
            halo,
            color: std::array::from_fn(|k| tint[k] * b),
            phase,
        });
    }
}

/// Tileable 3D gradient noise in about -1..1, with `period` lattice cells per tile.
fn perlin3(p: [f32; 3], period: i32, seed: u32) -> f32 {
    let cell = p.map(f32::floor);
    let f: [f32; 3] = std::array::from_fn(|k| p[k] - cell[k]);
    let u = f.map(|t| t * t * t * (t * (t * 6.0 - 15.0) + 10.0));
    let corner = |dx: i32, dy: i32, dz: i32| -> f32 {
        let i = |k: usize, d: i32| (cell[k] as i32 + d).rem_euclid(period) as u32;
        let mut h = i(0, dx).wrapping_mul(0x8da6_b343)
            ^ i(1, dy).wrapping_mul(0xd816_3841)
            ^ i(2, dz).wrapping_mul(0xcb1a_b31f)
            ^ seed.wrapping_mul(0x9e37_79b9);
        h ^= h >> 15;
        h = h.wrapping_mul(0x2c1b_3c6d);
        h ^= h >> 12;
        h = h.wrapping_mul(0x297a_2d39);
        h ^= h >> 15;
        let (x, y, z) = (f[0] - dx as f32, f[1] - dy as f32, f[2] - dz as f32);
        match h % 12 {
            0 => x + y,
            1 => y - x,
            2 => x - y,
            3 => -x - y,
            4 => x + z,
            5 => z - x,
            6 => x - z,
            7 => -x - z,
            8 => y + z,
            9 => z - y,
            10 => y - z,
            _ => -y - z,
        }
    };
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let x00 = lerp(corner(0, 0, 0), corner(1, 0, 0), u[0]);
    let x10 = lerp(corner(0, 1, 0), corner(1, 1, 0), u[0]);
    let x01 = lerp(corner(0, 0, 1), corner(1, 0, 1), u[0]);
    let x11 = lerp(corner(0, 1, 1), corner(1, 1, 1), u[0]);
    lerp(lerp(x00, x10, u[1]), lerp(x01, x11, u[1]), u[2])
}

/// The smoke's noise: an RGBA8 volume that tiles in all three directions, with gradient noise
/// of 4, 8 and 16 lattice cells per tile in red, green and blue, and an independent 4-cell
/// noise in alpha. Every channel is normalised to mean 0.5 and standard deviation 0.15.
fn noise_volume(n: usize) -> Vec<u8> {
    const CHANNELS: [(i32, u32); 4] = [(4, 1), (8, 2), (16, 3), (4, 4)];
    let mut out = vec![0u8; n * n * n * 4];
    let mut vals = vec![0f32; n * n * n];
    for (c, &(period, seed)) in CHANNELS.iter().enumerate() {
        let k = period as f32 / n as f32;
        for (i, v) in vals.iter_mut().enumerate() {
            let (x, y, z) = (i % n, i / n % n, i / (n * n));
            *v = perlin3([x as f32 * k, y as f32 * k, z as f32 * k], period, seed);
        }
        let mean = vals.iter().sum::<f32>() / vals.len() as f32;
        let sd = (vals.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / vals.len() as f32).sqrt().max(1e-6);
        for (i, v) in vals.iter().enumerate() {
            out[i * 4 + c] = ((0.5 + 0.15 * (v - mean) / sd).clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    out
}

/// Upload the smoke noise volume and bind it with a repeating, filtering sampler.
fn noise_bind_group(gpu: &Gpu, layout: &wgpu::BindGroupLayout) -> wgpu::BindGroup {
    let n = NOISE_SIZE;
    let size = wgpu::Extent3d { width: n, height: n, depth_or_array_layers: n };
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("smoke noise"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        &noise_volume(n as usize),
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(n * 4), rows_per_image: Some(n) },
        size,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = linear_sampler(&gpu.device, "smoke noise sampler", wgpu::AddressMode::Repeat);
    gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("noise bg"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
        ],
    })
}

/// The furthest point after `start` that a single straight segment can reach: at most
/// `max_len` px long on screen, never further than `max_sag` px from the points it skips, and
/// never past an anchor (the first point after a multiple of `anchor` of path length).
///
/// The anchors keep the segmentation the same from frame to frame: without them it would be
/// redone from the tail of the trail, which moves every frame, so the joints would shift and
/// everything drawn from segments that overlap (wide smoke) would flicker.
fn segment_end(pts: &[TrailPoint], start: usize, scale: f32, max_len: f32, max_sag: f32, anchor: f64) -> usize {
    let a = pts[start].p;
    let cell = |i: usize| (pts[i].s / anchor).floor();
    let mut end = start + 1;
    for j in start + 2..pts.len() {
        if cell(j - 1) != cell(j - 2) {
            break; // j - 1 is an anchor: a segment ends there
        }
        let c = pts[j].p;
        let (dx, dy) = ((c[0] - a[0]) * scale, (c[1] - a[1]) * scale);
        let len = dx.hypot(dy);
        if len > max_len {
            break;
        }
        let straight = pts[start + 1..j].iter().all(|q| {
            let (qx, qy) = ((q.p[0] - a[0]) * scale, (q.p[1] - a[1]) * scale);
            // distance from the chord (or from its start, for a chord too short to have a direction)
            let off = if len > 1e-3 { (qx * dy - qy * dx).abs() / len } else { qx.hypot(qy) };
            off <= max_sag
        });
        if !straight {
            break;
        }
        end = j;
    }
    end
}

/// The trails as polylines of instanced segments, each as long as possible, with `ages[b][i]`
/// as the age of point `i` of trail `b`. Path lengths are stored relative to a whole number of
/// `SMOKE_REBASE` noise periods, which keeps them small enough for f32 without moving anything
/// pinned to the path.
fn build_segments(out: &mut Vec<Seg>, f: &FrameInput, ages: &[Vec<f32>; 3], scale: f32, period: f64) {
    out.clear();
    for (b, pts) in f.trails.iter().enumerate() {
        let age = &ages[b];
        let Some(first) = pts.first() else { continue };
        let span = SMOKE_REBASE * period;
        let base = (first.s / span).floor() * span;
        let anchor = SEG_ANCHOR * f.path_radius.max(1e-3);
        let begin = out.len();
        let mut start = 0;
        while start + 1 < pts.len() {
            let end = segment_end(pts, start, scale, MAX_SEG_PX, MAX_SAG_PX, anchor);
            let (a, c) = (pts[start], pts[end]);
            let moved = a.p != c.p; // a repeated point has no direction
            if moved && (age[start] < 1.0 || age[end] < 1.0) {
                out.push(Seg {
                    p0: a.p,
                    p1: c.p,
                    ages: [age[start], age[end]],
                    s: [(a.s - base) as f32, (c.s - base) as f32],
                    caps: [1.0; 2],
                    body: b as u32,
                    _pad: 0,
                });
            }
            start = end;
        }
        set_caps(&mut out[begin..]);
    }
}

/// How far each segment's smoke has to reach past its ends. On the outside of a bend there is
/// a wedge that neither segment covers between its ends; each fills its half, which takes the
/// sine of half the turn times the smoke's width. Anywhere else the neighbouring segment draws
/// the same smoke, so reaching further would only redo it. The ends of the trail stay round.
fn set_caps(segs: &mut [Seg]) {
    let dirs: Vec<[f32; 2]> = segs
        .iter()
        .map(|s| {
            let (dx, dy) = (s.p1[0] - s.p0[0], s.p1[1] - s.p0[1]);
            let l = dx.hypot(dy).max(1e-12);
            [dx / l, dy / l]
        })
        .collect();
    // sin(turn / 2) = sqrt((1 - cos(turn)) / 2)
    let half_turn = |a: [f32; 2], b: [f32; 2]| ((1.0 - (a[0] * b[0] + a[1] * b[1]).clamp(-1.0, 1.0)) * 0.5).sqrt();
    for (k, seg) in segs.iter_mut().enumerate() {
        let d = dirs[k];
        seg.caps = [
            if k > 0 { half_turn(dirs[k - 1], d) } else { 1.0 },
            dirs.get(k + 1).map_or(1.0, |&next| half_turn(d, next)),
        ];
    }
}

/// How much of a trail (as a share of its full length, from the body back) is left before its
/// path has turned `budget` radians; 1 if it never turns that far.
fn turning_reach(pts: &[TrailPoint], budget: f32) -> f32 {
    let mut turned = 0.0f32;
    let mut heading: Option<f32> = None; // of the chord after the current point
    for k in (0..pts.len().saturating_sub(1)).rev() {
        let (a, b) = (pts[k].p, pts[k + 1].p);
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        if dx == 0.0 && dy == 0.0 {
            continue;
        }
        let h = dy.atan2(dx);
        if let Some(next) = heading {
            let pi = std::f32::consts::PI;
            let turn = ((next - h + pi).rem_euclid(2.0 * pi) - pi).abs();
            if turned + turn >= budget {
                // spread the joint's turn over the chord before it, so the cut moves smoothly
                let f = (budget - turned) / turn;
                return pts[k + 1].age + (pts[k].age - pts[k + 1].age) * f;
            }
            turned += turn;
        }
        heading = Some(h);
    }
    1.0
}

/// The share of a stardust trail shown after `dt` seconds, moving toward `target`: it grows
/// with the trail (which never outpaces the motes being shed), but shrinks by at most
/// `DUST_RETRACT` per second and slows down as it gets there.
fn ease_reach(current: f32, target: f32, dt: f32) -> f32 {
    if target >= current {
        return target;
    }
    let rate = ((current - target) * 4.0).min(DUST_RETRACT);
    (current - rate * dt).max(target)
}

impl Renderer {
    pub fn new(gpu: &Gpu, format: wgpu::TextureFormat, width: u32, height: u32) -> Self {
        let dev = &gpu.device;
        let module = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("moirai shaders"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders.wgsl").into()),
        });
        let globals = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals bgl"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::VERTEX_FRAGMENT)],
        });
        let noise_bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("noise bgl"),
            entries: &[texture_entry(0, wgpu::TextureViewDimension::D3, true), sampler_entry(1)],
        });
        let comp_bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("composite bgl"),
            entries: &[
                uniform_entry(0, wgpu::ShaderStages::VERTEX_FRAGMENT),
                texture_entry(1, wgpu::TextureViewDimension::D2, false),
                texture_entry(2, wgpu::TextureViewDimension::D2, false),
                texture_entry(3, wgpu::TextureViewDimension::D2, true),
                sampler_entry(4),
                texture_entry(5, wgpu::TextureViewDimension::D2, true),
                texture_entry(6, wgpu::TextureViewDimension::D2, false),
            ],
        });
        let layout = |label: &str, groups: &[Option<&wgpu::BindGroupLayout>]| {
            dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: groups,
                immediate_size: 0,
            })
        };
        let globals_layout = layout("globals layout", &[Some(&globals_bgl)]);
        let smoke_layout = layout("smoke layout", &[Some(&globals_bgl), Some(&noise_bgl)]);
        let comp_layout = layout("composite layout", &[Some(&comp_bgl)]);
        // in the order of the fields of `Seg` (the macro lays them out one after another)
        let seg_attrs = wgpu::vertex_attr_array![
            0 => Float32x2, 1 => Float32x2, 2 => Float32x2, 3 => Float32x2, 5 => Float32x2, 4 => Uint32
        ];
        let line_pipeline = instanced_pipeline(
            dev,
            &module,
            &globals_layout,
            "line pipeline",
            ("vs_trail", "fs_trail"),
            std::mem::size_of::<Seg>(),
            &seg_attrs,
            TRAIL_FORMAT,
            wgpu::BlendOperation::Max,
        );
        let smoke_pipeline = |label: &str, entry: (&str, &str)| {
            instanced_pipeline(
                dev,
                &module,
                &smoke_layout,
                label,
                entry,
                std::mem::size_of::<Seg>(),
                &seg_attrs,
                TRAIL_FORMAT,
                wgpu::BlendOperation::Max,
            )
        };
        let smoke_near_pipeline = smoke_pipeline("smoke near pipeline", ("vs_smoke_near", "fs_smoke_near"));
        let smoke_far_pipeline = smoke_pipeline("smoke far pipeline", ("vs_smoke_far", "fs_smoke_far"));
        let planet_pipeline = instanced_pipeline(
            dev,
            &module,
            &globals_layout,
            "planet pipeline",
            ("vs_planet", "fs_planet"),
            std::mem::size_of::<Seg>(),
            &seg_attrs,
            PLANET_FORMAT,
            wgpu::BlendOperation::Max,
        );
        let dust_pipeline = instanced_pipeline(
            dev,
            &module,
            &globals_layout,
            "dust pipeline",
            ("vs_mote", "fs_mote"),
            std::mem::size_of::<Mote>(),
            // in the order of the fields of `Mote` (the macro lays them out one after another)
            &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32, 3 => Float32, 2 => Uint32],
            TRAIL_FORMAT,
            wgpu::BlendOperation::Add,
        );
        let star_pipeline = instanced_pipeline(
            dev,
            &module,
            &globals_layout,
            "star pipeline",
            ("vs_star", "fs_star"),
            std::mem::size_of::<Star>(),
            &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32, 2 => Float32, 3 => Float32x3, 4 => Float32],
            STAR_FORMAT,
            wgpu::BlendOperation::Max,
        );
        let comp_pipeline = dev.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("composite pipeline"),
            layout: Some(&comp_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_full"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_comp"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let hud_bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("hud bgl"),
            entries: &[texture_entry(2, wgpu::TextureViewDimension::D2, true), sampler_entry(3)],
        });
        let hud_pipeline = hud_pipeline(dev, &module, &layout("hud layout", &[Some(&globals_bgl), Some(&hud_bgl)]), format);
        let hud = Hud::new(width.min(height) as f32);
        let hud_bg = hud_atlas(gpu, &hud_bgl, &hud);
        let globals_bg = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals bg"),
            layout: &globals_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });
        let noise_bg = noise_bind_group(gpu, &noise_bgl);
        let smoke_sampler = linear_sampler(dev, "smoke sampler", wgpu::AddressMode::ClampToEdge);
        let targets = Self::make_targets(dev, &comp_bgl, &globals, &smoke_sampler, width, height);
        Self {
            width,
            height,
            encode_srgb: !format.is_srgb(),
            globals,
            line_pipeline,
            dust_pipeline,
            smoke_near_pipeline,
            smoke_far_pipeline,
            planet_pipeline,
            star_pipeline,
            comp_pipeline,
            globals_bg,
            noise_bg,
            comp_bgl,
            smoke_sampler,
            targets,
            stars_dirty: true,
            segs: Instances::new(dev, "segments", 4096),
            motes: Instances::new(dev, "motes", 65536),
            stars: Instances::new(dev, "stars", STAR_COUNT),
            planet_segs: Instances::new(dev, "planet segments", 512),
            hud,
            hud_pipeline,
            hud_bgl,
            hud_bg,
            hud_quads: Instances::new(dev, "hud quads", 256),
            ages: Default::default(),
            dust_reach: [1.0; 3],
            dust_clock: None,
        }
    }

    fn make_targets(
        dev: &wgpu::Device,
        comp_bgl: &wgpu::BindGroupLayout,
        globals: &wgpu::Buffer,
        smoke_sampler: &wgpu::Sampler,
        width: u32,
        height: u32,
    ) -> Targets {
        let trail = hdr_texture(dev, "trail intensity", TRAIL_FORMAT, width, height);
        let smoke_near = hdr_texture(dev, "smoke near", TRAIL_FORMAT, width, height);
        // upsampled with bilinear filtering in the composite
        let (fw, fh) = (width.div_ceil(SMOKE_FAR_DIV), height.div_ceil(SMOKE_FAR_DIV));
        let smoke_far = hdr_texture(dev, "smoke far", TRAIL_FORMAT, fw, fh);
        let planet = hdr_texture(dev, "planet trail", PLANET_FORMAT, width, height);
        let stars = hdr_texture(dev, "starfield", STAR_FORMAT, width, height);
        let comp_bg = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("composite bg"),
            layout: comp_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&trail) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&stars) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&smoke_near) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(smoke_sampler) },
                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&smoke_far) },
                wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&planet) },
            ],
        });
        Targets { trail, smoke_near, smoke_far, planet, stars, comp_bg }
    }

    pub fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        if (width, height) == (self.width, self.height) {
            return;
        }
        self.width = width;
        self.height = height;
        self.targets = Self::make_targets(&gpu.device, &self.comp_bgl, &self.globals, &self.smoke_sampler, width, height);
        self.stars_dirty = true;
        if self.hud.prepare(width.min(height) as f32) {
            self.hud_bg = hud_atlas(gpu, &self.hud_bgl, &self.hud);
        }
    }

    /// Dust: a mote wherever the path length crosses a multiple of the spacing. Its seed is
    /// that multiple, so it keeps its identity (drift, size, twinkle) from frame to frame.
    fn build_motes(&mut self, f: &FrameInput) {
        self.motes.data.clear();
        let spacing = (DUST_SPACING * f.path_radius).max(1e-6);
        for (b, pts) in f.trails.iter().enumerate() {
            let ages = &self.ages[b];
            for (i, w) in pts.windows(2).enumerate() {
                let (a, c) = (w[0], w[1]);
                let (age_a, age_c) = (ages[i], ages[i + 1]);
                let ds = c.s - a.s;
                if ds <= 0.0 || (age_a >= 1.0 && age_c >= 1.0) {
                    continue;
                }
                let k0 = (a.s / spacing).ceil() as i64;
                let k1 = (c.s / spacing).ceil() as i64;
                for k in k0..k1 {
                    let u = ((k as f64 * spacing - a.s) / ds) as f32;
                    let age = age_a + (age_c - age_a) * u;
                    if age >= 1.0 {
                        continue;
                    }
                    let seed = (k as u32).wrapping_mul(0x9e37_79b1) ^ f.salt;
                    self.motes.data.push(Mote {
                        p: [a.p[0] + (c.p[0] - a.p[0]) * u, a.p[1] + (c.p[1] - a.p[1]) * u],
                        age,
                        drift: a.age + (c.age - a.age) * u,
                        word: (seed << 2) | b as u32,
                    });
                }
            }
        }
    }

    /// Record the frame into `encoder`, drawing into `target` (same size as the renderer).
    pub fn render(&mut self, gpu: &Gpu, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, f: &FrameInput) {
        let (w, h) = (self.width as f32, self.height as f32);
        let m = w.min(h);
        let scale = 0.5 * m / f.radius.max(1e-3);
        let style = f.look.style;
        let smoke_period = SMOKE_PERIOD * f.path_radius.max(1e-3);

        // One set of segments serves the lines, the hot filament under the smoke (which only
        // spans the youngest part of the trail) and the smoke itself; stardust keeps only the
        // lines' soft glow, over the younger part of its trail.
        let core_span = match style {
            TrailStyle::Smoke => SMOKE_CORE_SPAN,
            TrailStyle::Dust => DUST_GLOW_SPAN,
            TrailStyle::Lines => 1.0,
        };
        // Stardust shows as much of each trail as its turning allows, easing toward that when it
        // shrinks; the other styles show whole trails.
        let dust = style == TrailStyle::Dust;
        let dt = match self.dust_clock {
            Some((time, salt)) if salt == f.salt && f.time >= time => Some((f.time - time).min(0.5)),
            _ => None, // first stardust frame of this scene: no history to ease from
        };
        for ((ages, pts), shown) in self.ages.iter_mut().zip(f.trails).zip(&mut self.dust_reach) {
            ages.clear();
            if dust {
                let target = turning_reach(pts, DUST_TURNING);
                *shown = dt.map_or(target, |dt| ease_reach(*shown, target, dt)).max(0.01);
                ages.extend(pts.iter().map(|p| (p.age / *shown).min(1.0)));
            } else {
                ages.extend(pts.iter().map(|p| p.age));
            }
        }
        self.dust_clock = dust.then_some((f.time, f.salt));
        build_segments(&mut self.segs.data, f, &self.ages, scale, smoke_period);
        self.segs.upload(gpu);
        self.motes.data.clear();
        if style == TrailStyle::Dust {
            self.build_motes(f);
            self.motes.upload(gpu);
        }
        // the planet's trail: short and drawn as it is (a segment between every two points)
        self.planet_segs.data.clear();
        if let Some(planet) = &f.planet {
            for w in planet.trail.windows(2).filter(|w| w[0].p != w[1].p) {
                self.planet_segs.data.push(Seg {
                    p0: w[0].p,
                    p1: w[1].p,
                    ages: [w[0].age, w[1].age],
                    s: [0.0; 2],
                    caps: [1.0; 2],
                    body: 3,
                    _pad: 0,
                });
            }
            self.planet_segs.upload(gpu);
        }
        // Motes are shed at a fixed path spacing; when a dance zooms out they crowd together,
        // so dim them to keep the light per unit of screen length roughly constant.
        let spacing_px = (DUST_SPACING * f.path_radius) as f32 * scale;
        let crowding = (spacing_px / (0.5 * DUST_SPACING as f32 * m)).clamp(0.4, 1.5);

        let p = f.palette.bodies;
        let globals = Globals {
            size: [w, h],
            center: f.center,
            scale,
            fade: f.fade,
            time: f.time,
            core_px: (0.0014 * m).max(0.9),
            glow_px: (0.0065 * m).max(2.0),
            glow_amp: match style {
                TrailStyle::Lines => 0.32,
                TrailStyle::Dust => 0.12,
                TrailStyle::Smoke => 0.22,
            },
            body_core_px: (0.0060 * m).max(2.0),
            body_halo_px: (0.026 * m).max(6.0) * if f.planet.is_some() { PLANET_SUN_GLOW } else { 1.0 },
            encode_srgb: if self.encode_srgb { 1.0 } else { 0.0 },
            exposure: 1.0,
            stars: f.look.stars,
            line_core: if style == TrailStyle::Dust { 0.0 } else { 1.0 },
            dust_px: 0.0007 * m,
            dust_spread_px: 0.02 * m,
            dust_gain: 1.2 * crowding,
            dust_hot: 1.6,
            core_span,
            smoke_on: if style == TrailStyle::Smoke { 1.0 } else { 0.0 },
            smoke_w0: smoke_width(m, 0.0),
            smoke_w1: smoke_width(m, 1.0),
            smoke_drift: SMOKE_DRIFT * m,
            smoke_freq: (1.0 / smoke_period) as f32,
            smoke_gain: 0.9,
            smoke_hot: 0.8,
            smoke_flow: SMOKE_FLOW * m,
            _pad: [0.0; 3],
            colors: std::array::from_fn(|k| [p[k][0], p[k][1], p[k][2], 1.0]),
            bodies: std::array::from_fn(|k| [f.bodies[k][0], f.bodies[k][1], 1.0, 0.0]),
            bg_center: BG_CENTER,
            bg_edge: BG_EDGE,
            planet: match &f.planet {
                Some(p) => [p.pos[0], p.pos[1], 1.0, 0.0],
                None => [0.0; 4],
            },
            planet_color: match &f.planet {
                Some(p) => {
                    let [r, g, b] = temperature_color(p.chronicle.temp);
                    [r, g, b, 1.0]
                }
                None => [0.0; 4],
            },
        };
        gpu.queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        if self.stars_dirty {
            self.stars_dirty = false;
            make_stars(self.width, self.height, &mut self.stars.data);
            self.stars.upload(gpu);
            let mut pass = begin(encoder, "starfield", &self.targets.stars);
            pass.set_bind_group(0, &self.globals_bg, &[]);
            self.stars.draw(&mut pass, &self.star_pipeline);
        }
        {
            let mut pass = begin(encoder, "trails", &self.targets.trail);
            pass.set_bind_group(0, &self.globals_bg, &[]);
            self.segs.draw(&mut pass, &self.line_pipeline);
            self.motes.draw(&mut pass, &self.dust_pipeline);
        }
        if style == TrailStyle::Smoke {
            for (label, target, pipeline) in [
                ("smoke near", &self.targets.smoke_near, &self.smoke_near_pipeline),
                ("smoke far", &self.targets.smoke_far, &self.smoke_far_pipeline),
            ] {
                let mut pass = begin(encoder, label, target);
                pass.set_bind_group(0, &self.globals_bg, &[]);
                pass.set_bind_group(1, &self.noise_bg, &[]);
                self.segs.draw(&mut pass, pipeline);
            }
        }
        if f.planet.is_some() {
            let mut pass = begin(encoder, "planet", &self.targets.planet);
            pass.set_bind_group(0, &self.globals_bg, &[]);
            self.planet_segs.draw(&mut pass, &self.planet_pipeline);
        }
        {
            let mut pass = begin(encoder, "composite", target);
            pass.set_pipeline(&self.comp_pipeline);
            pass.set_bind_group(0, &self.targets.comp_bg, &[]);
            pass.draw(0..3, 0..1);
        }
        if let Some(planet) = &f.planet {
            self.hud.layout(&mut self.hud_quads.data, h, m, planet.number, &planet.chronicle, f.fade);
            self.hud_quads.upload(gpu);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("readout"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.globals_bg, &[]);
            pass.set_bind_group(1, &self.hud_bg, &[]);
            self.hud_quads.draw(&mut pass, &self.hud_pipeline);
        }
    }
}

/// The readout's pipeline: quads over the finished frame, blended with premultiplied alpha.
fn hud_pipeline(
    dev: &wgpu::Device,
    module: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let blend = wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    };
    dev.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("hud pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_hud"),
            compilation_options: Default::default(),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<HudQuad>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Uint32],
            })],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_hud"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState { color: blend, alpha: blend }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Upload the readout's glyph atlas and bind it with a filtering sampler.
fn hud_atlas(gpu: &Gpu, layout: &wgpu::BindGroupLayout, hud: &Hud) -> wgpu::BindGroup {
    let size = wgpu::Extent3d { width: hud.atlas_width, height: hud.atlas_height, depth_or_array_layers: 1 };
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("glyph atlas"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        &hud.atlas,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(hud.atlas_width), rows_per_image: Some(hud.atlas_height) },
        size,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = linear_sampler(&gpu.device, "glyph sampler", wgpu::AddressMode::ClampToEdge);
    gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("hud bg"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&sampler) },
        ],
    })
}

/// A render pass that clears `view` to transparent black.
fn begin<'e>(encoder: &'e mut wgpu::CommandEncoder, label: &str, view: &wgpu::TextureView) -> wgpu::RenderPass<'e> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

/// An offscreen RGBA8 target whose pixels can be read back (terminal output, snapshots).
pub struct Offscreen {
    pub width: u32,
    pub height: u32,
    texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    buffer: wgpu::Buffer,
    padded_row: u32,
}

pub const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

impl Offscreen {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let padded_row = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self { width, height, texture, view, buffer, padded_row }
    }

    pub fn copy_out(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: 1 },
        );
    }

    /// Wait for the GPU and copy tightly packed RGBA rows into `out` (len = w * h * 4).
    pub fn read(&self, gpu: &Gpu, out: &mut [u8]) {
        let slice = self.buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
        {
            let data = slice.get_mapped_range().expect("readback buffer is mapped");
            let row = (self.width * 4) as usize;
            for y in 0..self.height as usize {
                let src = y * self.padded_row as usize;
                out[y * row..(y + 1) * row].copy_from_slice(&data[src..src + row]);
            }
        }
        self.buffer.unmap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_volume_tiles_and_is_normalised() {
        let n = 16;
        let v = noise_volume(n);
        for c in 0..4 {
            let vals: Vec<f32> = v.iter().skip(c).step_by(4).map(|&b| b as f32 / 255.0).collect();
            let mean = vals.iter().sum::<f32>() / vals.len() as f32;
            assert!((mean - 0.5).abs() < 0.02, "channel {c} mean {mean}");
        }
        // the lattice wraps: noise just past the tile edge equals noise at its start
        for &(period, seed) in &[(4, 1u32), (16, 3)] {
            let a = perlin3([0.3, 1.7, 2.2], period, seed);
            let b = perlin3([0.3 + period as f32, 1.7, 2.2 - period as f32], period, seed);
            assert!((a - b).abs() < 1e-5);
        }
    }

    #[test]
    fn stardust_is_cut_where_the_path_has_turned_enough() {
        use std::f32::consts::PI;
        let budget = 3.0 * PI;
        // a straight path is shown whole
        let line: Vec<TrailPoint> = (0..100)
            .map(|i| TrailPoint { p: [i as f32 * 0.01, 0.0], age: 1.0 - i as f32 / 99.0, s: i as f64 * 0.01 })
            .collect();
        assert_eq!(turning_reach(&line, budget), 1.0);
        // two quick loops (oldest first) that are young in time: cut a loop and a half back
        let n = 400;
        let circle: Vec<TrailPoint> = (0..=n)
            .map(|i| {
                let th = 4.0 * PI * i as f32 / n as f32;
                TrailPoint { p: [th.cos(), th.sin()], age: 0.1 * (1.0 - i as f32 / n as f32), s: th as f64 }
            })
            .collect();
        let reach = turning_reach(&circle, budget);
        assert!((reach - 0.075).abs() < 0.001, "{reach}");
    }

    #[test]
    fn stardust_shrinks_gradually_and_grows_with_the_trail() {
        // a slingshot moves the cut from the whole trail to a fifth of it
        let mut shown = 1.0;
        let mut t = 0.0;
        while shown > 0.2 + 1e-4 {
            let before = shown;
            shown = ease_reach(shown, 0.2, 1.0 / 60.0);
            assert!(before - shown <= DUST_RETRACT / 60.0 + 1e-6);
            t += 1.0 / 60.0;
            assert!(t < 10.0);
        }
        assert!(t > 0.8 / DUST_RETRACT, "took {t} s");
        // growing back follows the trail at once
        assert_eq!(ease_reach(0.2, 0.25, 1.0 / 60.0), 0.25);
    }
}
