//! Clotho: turns the bodies' recent history into light. One instanced pass draws every body's
//! trail into its own channel of an HDR texture with *max* blending (smooth glowing threads,
//! no bright beads at the joints, no blur passes); one composite pass adds the bodies,
//! the background and tone mapping.

use bytemuck::{Pod, Zeroable};

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
}

pub const PALETTES: &[Palette] = &[
    Palette { name: "ember & ice", bodies: [[1.00, 0.42, 0.10], [0.18, 0.58, 1.00], [0.95, 0.24, 0.58]] },
    Palette { name: "aurora", bodies: [[0.22, 1.00, 0.50], [0.26, 0.52, 1.00], [0.80, 0.30, 1.00]] },
    Palette { name: "moonlight", bodies: [[0.82, 0.86, 1.00], [0.40, 0.62, 1.00], [0.72, 0.50, 1.00]] },
    Palette { name: "solar", bodies: [[1.00, 0.70, 0.20], [1.00, 0.32, 0.14], [1.00, 0.90, 0.62]] },
    Palette { name: "nebula", bodies: [[1.00, 0.30, 0.52], [0.30, 0.82, 1.00], [0.66, 0.40, 1.00]] },
];

const BG_CENTER: [f32; 4] = [0.0060, 0.0068, 0.0130, 1.0];
const BG_EDGE: [f32; 4] = [0.0010, 0.0010, 0.0022, 1.0];

/// One point of a body's recent path; `age` runs from 0 (now) to 1 (end of the trail).
#[derive(Clone, Copy, Debug)]
pub struct TrailPoint {
    pub p: [f32; 2],
    pub age: f32,
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
    pub stars: f32,
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
    _pad: f32,
    colors: [[f32; 4]; 3],
    bodies: [[f32; 4]; 3],
    bg_center: [f32; 4],
    bg_edge: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Seg {
    p0: [f32; 2],
    p1: [f32; 2],
    ages: [f32; 2],
    body: u32,
    _pad: u32,
}

/// Trail points closer than this on screen are merged (keeps overdraw low).
const MIN_SEG_PX: f32 = 3.0;
const TRAIL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

pub struct Renderer {
    width: u32,
    height: u32,
    encode_srgb: bool,
    globals: wgpu::Buffer,
    trail_pipeline: wgpu::RenderPipeline,
    comp_pipeline: wgpu::RenderPipeline,
    trail_bg: wgpu::BindGroup,
    comp_bgl: wgpu::BindGroupLayout,
    comp_bg: wgpu::BindGroup,
    trail_view: wgpu::TextureView,
    segs: Vec<Seg>,
    seg_buf: wgpu::Buffer,
    seg_cap: usize,
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
        let trail_bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("trail bgl"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::VERTEX_FRAGMENT)],
        });
        let comp_bgl = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("composite bgl"),
            entries: &[
                uniform_entry(0, wgpu::ShaderStages::VERTEX_FRAGMENT),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let trail_layout = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("trail layout"),
            bind_group_layouts: &[Some(&trail_bgl)],
            immediate_size: 0,
        });
        let comp_layout = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("composite layout"),
            bind_group_layouts: &[Some(&comp_bgl)],
            immediate_size: 0,
        });
        let max_blend = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Max,
        };
        let seg_attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x2, 3 => Uint32];
        let trail_pipeline = dev.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("trail pipeline"),
            layout: Some(&trail_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_trail"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Seg>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &seg_attrs,
                })],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_trail"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: TRAIL_FORMAT,
                    blend: Some(wgpu::BlendState { color: max_blend, alpha: max_blend }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
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
        let trail_bg = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("trail bg"),
            layout: &trail_bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });
        let (trail_view, comp_bg) = Self::make_targets(dev, &comp_bgl, &globals, width, height);
        let seg_cap = 4096;
        let seg_buf = Self::make_seg_buf(dev, seg_cap);
        Self {
            width,
            height,
            encode_srgb: !format.is_srgb(),
            globals,
            trail_pipeline,
            comp_pipeline,
            trail_bg,
            comp_bgl,
            comp_bg,
            trail_view,
            segs: Vec::with_capacity(seg_cap),
            seg_buf,
            seg_cap,
        }
    }

    fn make_seg_buf(dev: &wgpu::Device, cap: usize) -> wgpu::Buffer {
        dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("segments"),
            size: (cap * std::mem::size_of::<Seg>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn make_targets(
        dev: &wgpu::Device,
        comp_bgl: &wgpu::BindGroupLayout,
        globals: &wgpu::Buffer,
        width: u32,
        height: u32,
    ) -> (wgpu::TextureView, wgpu::BindGroup) {
        let tex = dev.create_texture(&wgpu::TextureDescriptor {
            label: Some("trail intensity"),
            size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TRAIL_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bg = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("composite bg"),
            layout: comp_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
            ],
        });
        (view, bg)
    }

    pub fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        if (width, height) == (self.width, self.height) {
            return;
        }
        self.width = width;
        self.height = height;
        let (view, bg) = Self::make_targets(&gpu.device, &self.comp_bgl, &self.globals, width, height);
        self.trail_view = view;
        self.comp_bg = bg;
    }

    /// Record the frame into `encoder`, drawing into `target` (same size as the renderer).
    pub fn render(&mut self, gpu: &Gpu, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, f: &FrameInput) {
        let (w, h) = (self.width as f32, self.height as f32);
        let m = w.min(h);
        let scale = 0.5 * m / f.radius.max(1e-3);

        // trails -> instanced segments, merging points closer than MIN_SEG_PX on screen
        self.segs.clear();
        for (b, pts) in f.trails.iter().enumerate() {
            let Some(first) = pts.first() else { continue };
            let mut last = *first;
            for (i, p) in pts.iter().enumerate().skip(1) {
                let dx = (p.p[0] - last.p[0]) * scale;
                let dy = (p.p[1] - last.p[1]) * scale;
                if dx * dx + dy * dy >= MIN_SEG_PX * MIN_SEG_PX || i == pts.len() - 1 {
                    if last.age < 1.0 || p.age < 1.0 {
                        self.segs.push(Seg { p0: last.p, p1: p.p, ages: [last.age, p.age], body: b as u32, _pad: 0 });
                    }
                    last = *p;
                }
            }
        }
        if self.segs.len() > self.seg_cap {
            self.seg_cap = self.segs.len().next_power_of_two();
            self.seg_buf = Self::make_seg_buf(&gpu.device, self.seg_cap);
        }
        if !self.segs.is_empty() {
            gpu.queue.write_buffer(&self.seg_buf, 0, bytemuck::cast_slice(&self.segs));
        }

        let p = f.palette.bodies;
        let globals = Globals {
            size: [w, h],
            center: f.center,
            scale,
            fade: f.fade,
            time: f.time,
            core_px: (0.0014 * m).max(0.9),
            glow_px: (0.0065 * m).max(2.0),
            glow_amp: 0.32,
            body_core_px: (0.0060 * m).max(2.0),
            body_halo_px: (0.026 * m).max(6.0),
            encode_srgb: if self.encode_srgb { 1.0 } else { 0.0 },
            exposure: 1.0,
            stars: f.stars,
            _pad: 0.0,
            colors: std::array::from_fn(|k| [p[k][0], p[k][1], p[k][2], 1.0]),
            bodies: std::array::from_fn(|k| [f.bodies[k][0], f.bodies[k][1], 1.0, 0.0]),
            bg_center: BG_CENTER,
            bg_edge: BG_EDGE,
        };
        gpu.queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("trails"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.trail_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !self.segs.is_empty() {
                pass.set_pipeline(&self.trail_pipeline);
                pass.set_bind_group(0, &self.trail_bg, &[]);
                pass.set_vertex_buffer(0, self.seg_buf.slice(..));
                pass.draw(0..6, 0..self.segs.len() as u32);
            }
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.comp_pipeline);
            pass.set_bind_group(0, &self.comp_bg, &[]);
            pass.draw(0..3, 0..1);
        }
    }
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
