//! The engine: a background physics thread keeps each scene's future computed ahead of the
//! display (and sleeps otherwise); the render side advances a clock, fades between scenes,
//! frames the camera and turns samples into trails.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use moirai_core::REV;
use moirai_core::planet::{PlanetSample, planet_at};
use moirai_core::scene::{Director, DirectorConfig, Scene};
use moirai_core::timeline::{Sample, interpolate};

use crate::render::{FrameInput, Look, PALETTES, Palette, PlanetView, TrailPoint};

struct AtomicF64(AtomicU64);

impl AtomicF64 {
    fn new(v: f64) -> Self {
        Self(AtomicU64::new(v.to_bits()))
    }
    fn load(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Acquire))
    }
    fn store(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Release)
    }
}

/// What the render side sees of a scene; the worker keeps `samples` filled ahead of `display`.
pub struct SceneShared {
    pub title: String,
    pub trail: f64,
    pub frame_radius: Option<f64>,
    pub palette: u32,
    end: AtomicF64,
    display: AtomicF64,
    frontier: AtomicF64,
    samples: Mutex<VecDeque<Sample>>,
    /// The planet's path (see `moirai_core::planet`).
    planet: Mutex<VecDeque<PlanetSample>>,
}

impl SceneShared {
    fn from_scene(s: &Scene) -> Arc<Self> {
        let shared = Arc::new(Self {
            title: s.title.clone(),
            trail: s.trail,
            frame_radius: s.frame_radius,
            palette: s.palette,
            end: AtomicF64::new(s.duration),
            display: AtomicF64::new(0.0),
            frontier: AtomicF64::new(s.timeline.frontier()),
            samples: Mutex::new(VecDeque::new()),
            planet: Mutex::new(VecDeque::new()),
        });
        shared.sync(s);
        shared
    }

    fn sync(&self, s: &Scene) {
        {
            let mut dst = self.samples.lock().expect("samples lock");
            dst.clear();
            dst.extend(s.timeline.samples().iter().copied());
        }
        if let Some(planet) = s.timeline.planet() {
            let mut dst = self.planet.lock().expect("planet lock");
            dst.clear();
            dst.extend(planet.samples.iter().copied());
        }
        self.frontier.store(s.timeline.frontier());
    }
}

#[derive(Clone, Copy, Debug)]
pub enum SceneRequest {
    Random,
    Choreography(u32),
    Dance(u64),
}

#[derive(Clone, Copy, Debug)]
pub struct EngineConfig {
    pub seed: u64,
    pub director: DirectorConfig,
    pub first: SceneRequest,
    /// Seconds of screen time per revolution.
    pub seconds_per_rev: f64,
    /// Fade in/out length in seconds.
    pub fade_seconds: f64,
    /// Palettes in rotation, as a bit set over `PALETTES` (0 means the featured ones).
    pub palettes: u32,
}

/// Indices into `PALETTES` selected by a bit set (the featured palettes if it is empty).
pub fn palette_rotation(mask: u32) -> Vec<usize> {
    let picked: Vec<usize> = (0..PALETTES.len()).filter(|&i| mask & (1 << i) != 0).collect();
    if picked.is_empty() { (0..PALETTES.len()).filter(|&i| PALETTES[i].featured).collect() } else { picked }
}

/// Framing radius a free dance is designed around (its camera follows the motion).
const DANCE_RADIUS: f64 = 1.6;

/// Length of the planet's trail in time units: four to five orbits in the habitable zone.
const PLANET_TRAIL: f64 = 1.2;

/// With the planet shown the scene runs at this share of its normal speed: otherwise the planet
/// circles its sun several times a second and its orbit is hard to follow.
pub const PLANET_PACE: f64 = 0.5;

/// The planet's recent path up to `t` (of its current run only), oldest first, into `out`, and
/// where it is at `t` with its latest sample (run and chronicle).
fn planet_trail(samples: &VecDeque<PlanetSample>, t: f64, out: &mut Vec<TrailPoint>) -> Option<([f32; 2], PlanetSample)> {
    out.clear();
    let (head, now) = planet_at(samples, t)?;
    let start = samples.partition_point(|s| s.t < t - PLANET_TRAIL).saturating_sub(1);
    for p in samples.iter().skip(start).take_while(|p| p.t <= t).filter(|p| p.run == now.run) {
        let age = ((t - p.t) / PLANET_TRAIL).clamp(0.0, 1.0) as f32;
        push_point(out, p.p, age);
    }
    let head = [head[0] as f32, head[1] as f32];
    push_point(out, head, 0.0);
    Some((head, now))
}

/// Append a trail point, measuring the path length along the way.
fn push_point(trail: &mut Vec<TrailPoint>, p: [f32; 2], age: f32) {
    let s = trail.last().map_or(0.0, |l| l.s + ((p[0] - l.p[0]) as f64).hypot((p[1] - l.p[1]) as f64));
    trail.push(TrailPoint { p, age, s });
}

/// Per-scene salt for the dust, derived from the scene's random palette pick.
fn dust_salt(palette: u32) -> u32 {
    palette.rotate_left(13).wrapping_mul(0x85eb_ca6b)
}

fn make_scene(d: &mut Director, req: SceneRequest) -> Scene {
    match req {
        SceneRequest::Random => d.next_scene(),
        SceneRequest::Choreography(id) => d.choreography(id),
        SceneRequest::Dance(seed) => d.dance(seed),
    }
}

/// The physics thread: fill the current scene ahead of the display, then prepare the next one.
fn worker(cfg: EngineConfig, tx: mpsc::SyncSender<Arc<SceneShared>>, quit: Arc<AtomicBool>) {
    let mut director = Director::new(cfg.seed, cfg.director);
    let mut scene = make_scene(&mut director, cfg.first);
    let mut shared = SceneShared::from_scene(&scene);
    if tx.send(shared.clone()).is_err() {
        return;
    }
    while !quit.load(Ordering::Relaxed) {
        let display = shared.display.load();
        let end = shared.end.load();
        let target = (display + scene.lookahead + 0.5 * REV).min(end + REV);
        let frontier = scene.timeline.frontier();
        if frontier < target - 1e-9 {
            let fail = scene.timeline.fill(display, (frontier + REV).min(target), false);
            if let Some(tf) = fail {
                // Steering gave up on an event at tf (still in the future): end the scene before it.
                let new_end = (tf - REV).max(display + 0.5 * REV);
                if new_end < shared.end.load() {
                    shared.end.store(new_end);
                }
            }
            scene.timeline.trim(display - scene.trail - REV);
            shared.sync(&scene);
        } else if frontier >= end + REV - 1e-9 {
            // Fully computed: prepare the next scene and hand it over when the renderer takes it.
            let next = make_scene(&mut director, SceneRequest::Random);
            let next_shared = SceneShared::from_scene(&next);
            let mut pending = Some(next_shared.clone());
            while let Some(s) = pending.take() {
                match tx.try_send(s) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Full(s)) => {
                        if quit.load(Ordering::Relaxed) {
                            return;
                        }
                        pending = Some(s);
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => return,
                }
            }
            scene = next;
            shared = next_shared;
        } else {
            thread::sleep(Duration::from_millis(30));
        }
    }
}

pub struct Engine {
    cfg: EngineConfig,
    rx: mpsc::Receiver<Arc<SceneShared>>,
    current: Arc<SceneShared>,
    quit: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    clock: f64,
    elapsed: f64,
    radius: f64,
    radius_target: f64,
    pub paused: bool,
    rotation: Vec<usize>,
    /// Share of the configured speed the scene runs at (eases toward 1 or `PLANET_PACE`).
    pace: f64,
    /// Palette steps taken with the cycle key in the current scene.
    palette_shift: usize,
    trails: [Vec<TrailPoint>; 3],
    bodies: [[f32; 2]; 3],
    planet_trail: Vec<TrailPoint>,
}

fn smoothstep(e0: f64, e1: f64, x: f64) -> f64 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl Engine {
    pub fn start(cfg: EngineConfig) -> Self {
        let (tx, rx) = mpsc::sync_channel(1);
        let quit = Arc::new(AtomicBool::new(false));
        let q = quit.clone();
        let worker = thread::Builder::new()
            .name("moirai-physics".into())
            .spawn(move || worker(cfg, tx, q))
            .expect("spawn physics thread");
        let current = rx.recv().expect("first scene");
        let radius = current.frame_radius.unwrap_or(1.8);
        Self {
            cfg,
            rx,
            current,
            quit,
            worker: Some(worker),
            clock: 0.0,
            elapsed: 0.0,
            radius,
            radius_target: radius,
            paused: false,
            rotation: palette_rotation(cfg.palettes),
            pace: 1.0,
            palette_shift: 0,
            trails: Default::default(),
            bodies: [[0.0; 2]; 3],
            planet_trail: Vec::new(),
        }
    }

    pub fn title(&self) -> &str {
        &self.current.title
    }

    fn palette(&self) -> &'static Palette {
        let n = self.rotation.len();
        &PALETTES[self.rotation[(self.current.palette as usize % n + self.palette_shift) % n]]
    }

    /// Show the current scene in the next palette of the rotation.
    pub fn cycle_palette(&mut self) {
        self.palette_shift += 1;
    }

    fn speed(&self) -> f64 {
        REV / self.cfg.seconds_per_rev * self.pace
    }

    /// Fade out and move on to the next scene.
    pub fn skip(&mut self) {
        let fade = self.cfg.fade_seconds * self.speed();
        let end = self.current.end.load();
        if self.clock < end - fade {
            self.current.end.store(self.clock + fade);
        }
    }

    /// Advance the clock by `dt` seconds of wall time, at the pace `look` calls for.
    pub fn update(&mut self, dt: f64, look: Look) {
        self.elapsed += dt;
        let pace = if look.planet { PLANET_PACE } else { 1.0 };
        self.pace += (pace - self.pace) * (1.0 - (-dt / 0.4).exp()); // eased over about a second
        if !self.paused {
            let end = self.current.end.load();
            let frontier = self.current.frontier.load();
            // never run past the committed future (the worker is normally far ahead)
            let mut t = (self.clock + dt * self.speed()).min(frontier).max(self.clock);
            if t >= end {
                t = end;
                if let Ok(next) = self.rx.try_recv() {
                    self.current = next;
                    self.palette_shift = 0;
                    t = 0.0;
                    self.radius = self.current.frame_radius.unwrap_or(self.radius);
                }
            }
            self.clock = t;
            self.current.display.store(t);
        }
        // critically damped-ish camera zoom (only for free dances)
        let k = 1.0 - (-dt / 2.5).exp();
        self.radius += (self.radius_target - self.radius) * k;
    }

    fn fade(&self) -> f64 {
        let f = self.cfg.fade_seconds * self.speed();
        let end = self.current.end.load();
        smoothstep(0.0, f, self.clock) * smoothstep(0.0, f, end - self.clock)
    }

    /// Build the frame for the current clock.
    pub fn frame(&mut self, look: Look) -> FrameInput<'_> {
        let cur = self.current.clone();
        let t = self.clock;
        let trail = cur.trail;
        for v in &mut self.trails {
            v.clear();
        }
        let mut rmax: f64 = 0.0;
        {
            let samples = cur.samples.lock().expect("samples lock");
            let n = samples.len();
            // first sample inside the trail window
            let start = samples.partition_point(|s| s.t < t - trail);
            for s in samples.iter().skip(start.saturating_sub(1)) {
                if s.t > t {
                    break;
                }
                let age = ((t - s.t) / trail).clamp(0.0, 1.0) as f32;
                for b in 0..3 {
                    self.trails[b].push(TrailPoint { p: s.p[b], age, s: s.s[b] });
                    rmax = rmax.max((s.p[b][0] as f64).hypot(s.p[b][1] as f64));
                }
            }
            if let Some(pos) = interpolate(n, t, |i| samples[i]) {
                for ((trail, body), q) in self.trails.iter_mut().zip(&mut self.bodies).zip(pos) {
                    let p = [q[0] as f32, q[1] as f32];
                    push_head(trail, p);
                    *body = p;
                }
            }
        }
        self.radius_target = match cur.frame_radius {
            Some(r) => r,
            None => (rmax * 1.18).clamp(1.1, 3.5),
        };
        if cur.frame_radius.is_some() {
            self.radius = self.radius_target;
        }
        let planet = if look.planet {
            planet_trail(&cur.planet.lock().expect("planet lock"), t, &mut self.planet_trail)
        } else {
            None
        };
        FrameInput {
            trails: [&self.trails[0], &self.trails[1], &self.trails[2]],
            bodies: self.bodies,
            center: [0.0, 0.0],
            radius: self.radius as f32,
            palette: self.palette(),
            fade: self.fade() as f32,
            time: self.elapsed as f32,
            look,
            path_radius: cur.frame_radius.unwrap_or(DANCE_RADIUS),
            salt: dust_salt(cur.palette),
            planet: planet.map(|(pos, now)| PlanetView { trail: &self.planet_trail, pos, number: now.run + 1, chronicle: now.chronicle }),
        }
    }
}

/// Append the interpolated current position as the newest trail point.
fn push_head(trail: &mut Vec<TrailPoint>, p: [f32; 2]) {
    push_point(trail, p, 0.0);
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        // unblock a worker waiting to hand over a scene
        while self.rx.try_recv().is_ok() {}
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// For each planet of a scene: how long it lasted (revolutions), how it ended (`None`: still
/// there when the scene ended) and what it had lived through by then.
pub type PlanetRuns = Vec<(f64, Option<moirai_core::planet::Fate>, moirai_core::planet::Chronicle)>;

/// Simulate `count` scenes (the first as requested, then as the director picks them) and return
/// each one's title and planet runs.
pub fn planet_runs(seed: u64, director: DirectorConfig, first: SceneRequest, count: usize) -> Vec<(String, PlanetRuns)> {
    let mut d = Director::new(seed, director);
    (0..count)
        .map(|i| {
            let mut scene = make_scene(&mut d, if i == 0 { first } else { SceneRequest::Random });
            let mut display = 0.0;
            while display < scene.duration {
                display += REV;
                scene.timeline.fill(display, display + scene.lookahead, false);
            }
            let mut runs = Vec::new();
            if let Some(planet) = scene.timeline.planet() {
                let mut start: Option<f64> = None;
                let mut last = None;
                for s in planet.samples.iter().take_while(|s| s.t <= scene.duration) {
                    let begun = *start.get_or_insert(s.t);
                    last = Some(s.chronicle);
                    if s.fate.is_some() {
                        runs.push(((s.t - begun) / REV, s.fate, s.chronicle));
                        start = None;
                    }
                }
                if let (Some(begun), Some(c)) = (start, last) {
                    runs.push(((scene.duration - begun) / REV, None, c));
                }
            }
            (scene.title, runs)
        })
        .collect()
}

/// Build a single frame synchronously at scene time `at_revs` (for snapshots and tests).
pub fn snapshot_scene(req: SceneRequest, seed: u64, at_revs: f64) -> (Scene, f64) {
    let mut d = Director::new(seed, DirectorConfig::default());
    let mut scene = make_scene(&mut d, req);
    let t = at_revs * REV;
    let mut display = 0.0;
    while display < t {
        display = (display + REV).min(t);
        scene.timeline.fill(display, display + scene.lookahead, false);
    }
    (scene, t)
}

/// Frame input for a scene computed synchronously (snapshots). `palettes` is the rotation as
/// in `EngineConfig`; `time` drives the twinkling.
pub fn still_frame<'a>(
    scene: &Scene,
    t: f64,
    trails: &'a mut [Vec<TrailPoint>; 3],
    planet_path: &'a mut Vec<TrailPoint>,
    look: Look,
    palettes: u32,
    time: f32,
) -> FrameInput<'a> {
    let samples = scene.timeline.samples();
    let trail = scene.trail;
    let mut bodies = [[0.0f32; 2]; 3];
    let mut rmax: f64 = 0.0;
    for v in trails.iter_mut() {
        v.clear();
    }
    for s in samples.iter().filter(|s| s.t >= t - trail && s.t <= t) {
        let age = ((t - s.t) / trail) as f32;
        for (b, trail) in trails.iter_mut().enumerate() {
            trail.push(TrailPoint { p: s.p[b], age, s: s.s[b] });
            rmax = rmax.max((s.p[b][0] as f64).hypot(s.p[b][1] as f64));
        }
    }
    if let Some(pos) = interpolate(samples.len(), t, |i| samples[i]) {
        for b in 0..3 {
            bodies[b] = [pos[b][0] as f32, pos[b][1] as f32];
            push_head(&mut trails[b], bodies[b]);
        }
    }
    let radius = scene.frame_radius.unwrap_or((rmax * 1.18).clamp(1.1, 3.5));
    let rotation = palette_rotation(palettes);
    let planet = match scene.timeline.planet() {
        Some(p) if look.planet => planet_trail(&p.samples, t, planet_path),
        _ => None,
    };
    FrameInput {
        trails: [&trails[0], &trails[1], &trails[2]],
        bodies,
        center: [0.0, 0.0],
        radius: radius as f32,
        palette: &PALETTES[rotation[scene.palette as usize % rotation.len()]],
        fade: 1.0,
        time,
        look,
        path_radius: scene.frame_radius.unwrap_or(DANCE_RADIUS),
        salt: dust_salt(scene.palette),
        planet: planet.map(|(pos, now)| PlanetView { trail: &planet_path[..], pos, number: now.run + 1, chronicle: now.chronicle }),
    }
}
