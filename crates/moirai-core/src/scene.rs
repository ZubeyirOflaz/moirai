//! The director: picks what is shown next and prepares it so that nothing ugly is ever on screen.
//!
//! * Choreographies: a stable periodic orbit with a small random kick (so every showing is a
//!   slightly different quasi-periodic variant). The whole scene is verified before it starts,
//!   so it runs as pure, untouched Newtonian gravity.
//! * Dances: a random democratic triangle kept alive by look-ahead steering.

use crate::events::{Limits, flags};
use crate::library::{Orbit, catalog, orbit_by_id, random_democratic};
use crate::physics::{Model, REV, Stop, integrate};
use crate::rng::Rng;
use crate::steer::{SteerParams, nudge};
use crate::timeline::Timeline;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Choreography { id: u32, kick: f64 },
    Dance { seed: u64 },
}

pub struct Scene {
    pub kind: Kind,
    pub title: String,
    pub timeline: Timeline,
    /// Scene length in physics time units.
    pub duration: f64,
    /// Trail length in physics time units.
    pub trail: f64,
    /// Fixed framing radius (units of a); `None` means follow the motion.
    pub frame_radius: Option<f64>,
    /// Palette selector (the renderer takes it modulo its palette count).
    pub palette: u32,
    /// Look-ahead the worker should keep filled.
    pub lookahead: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct DirectorConfig {
    /// Probability that the next scene is a dance rather than a choreography.
    pub dance_share: f64,
    /// Scene length in revolutions.
    pub scene_revs: f64,
}

impl Default for DirectorConfig {
    fn default() -> Self {
        Self { dance_share: 0.35, scene_revs: 24.0 }
    }
}

pub struct Director {
    rng: Rng,
    cfg: DirectorConfig,
    recent: Vec<u32>,
}

/// Model for dances: Newton plus safety nets that only engage if steering ever fails
/// (a tiny soft core, and a soft wall far outside the frame).
pub fn dance_model() -> Model {
    Model { eps: 0.001, wall_r: 6.0, wall_k: 0.5, ..Model::default() }
}

impl Director {
    pub fn new(seed: u64, cfg: DirectorConfig) -> Self {
        Self { rng: Rng::new(seed), cfg, recent: Vec::new() }
    }

    pub fn next_scene(&mut self) -> Scene {
        if self.rng.uniform() < self.cfg.dance_share {
            let seed = self.rng.next_u64();
            self.dance(seed)
        } else {
            let id = self.pick_orbit();
            self.choreography(id)
        }
    }

    /// Favour the varied "other" orbits over the many figure-eight relatives; avoid repeats.
    fn pick_orbit(&mut self) -> u32 {
        let cat = catalog();
        let others: Vec<&Orbit> = cat.iter().filter(|o| !o.figure_eight_family).collect();
        let eights: Vec<&Orbit> = cat.iter().filter(|o| o.figure_eight_family).collect();
        for _ in 0..16 {
            let pool = if self.rng.uniform() < 0.6 { &others } else { &eights };
            let id = pool[self.rng.below(pool.len())].id;
            if !self.recent.contains(&id) {
                self.recent.push(id);
                if self.recent.len() > 12 {
                    self.recent.remove(0);
                }
                return id;
            }
        }
        cat[self.rng.below(cat.len())].id
    }

    pub fn choreography(&mut self, id: u32) -> Scene {
        let orbit = orbit_by_id(id).unwrap_or(&catalog()[0]);
        let model = Model::default();
        let (y0, period) = orbit.initial_state(&model);
        let duration = self.cfg.scene_revs * REV;
        let grid = REV / 20.0;
        let check = Limits::geometric(0.5 * orbit.closest, 1.25 * orbit.extent + 0.05);
        let mut kick = orbit.kick * self.rng.uniform();
        let mut y = y0;
        for attempt in 0..5 {
            let mut yk = y0;
            if kick > 0.0 && attempt < 4 {
                nudge(&mut yk, &model, kick, &mut self.rng, true);
            } else {
                kick = 0.0;
            }
            let (mut yv, mut t) = (yk, 0.0);
            let stop = integrate(&mut yv, &mut t, duration + 2.0 * REV, &model, grid, |s| flags(s, &model, &check), |_| {});
            if stop == Stop::Reached {
                y = yk;
                break;
            }
            kick /= 4.0;
        }
        let title = match orbit.name() {
            Some(n) => format!("{n} · choreography #{}", orbit.id),
            None => format!("Choreography #{}", orbit.id),
        };
        let mut timeline = Timeline::new(y, model, None, None, self.rng.next_u64());
        timeline.add_planet();
        Scene {
            kind: Kind::Choreography { id: orbit.id, kick },
            title,
            timeline,
            duration,
            trail: period.clamp(1.2 * REV, 3.0 * REV),
            frame_radius: Some(orbit.extent * 1.12),
            palette: self.rng.next_u64() as u32,
            lookahead: 2.0 * REV,
        }
    }

    pub fn dance(&mut self, seed: u64) -> Scene {
        let model = dance_model();
        let steer = SteerParams::default();
        let mut s = seed;
        for _ in 0..64 {
            let mut rng = Rng::new(s);
            let y0 = random_democratic(&mut rng, &model);
            if flags(&y0, &model, &Limits::dance()) == 0 {
                let mut tl = Timeline::new(y0, model, Some(Limits::dance()), Some(steer), s);
                tl.add_planet();
                if tl.fill(0.0, steer.buffer, true).is_none() {
                    return Scene {
                        kind: Kind::Dance { seed: s },
                        title: format!("Dance #{s:016x}"),
                        timeline: tl,
                        duration: self.cfg.scene_revs * REV,
                        trail: 1.2 * REV,
                        frame_radius: None,
                        palette: self.rng.next_u64() as u32,
                        lookahead: steer.buffer,
                    };
                }
            }
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        }
        // Practically unreachable; fall back to the figure-eight.
        self.choreography(1)
    }
}
