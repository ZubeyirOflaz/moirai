//! Clotho's spindle: the committed future of one scene. The physics frontier runs ahead of
//! the display; Lachesis may rewrite anything that has not been shown yet.

use std::collections::VecDeque;

use crate::events::{Limits, flags};
use crate::physics::{Model, NB, REV, Step, Stop, Y, hermite, integrate};
use crate::rng::Rng;
use crate::steer::{SteerParams, search};

/// Emit a trail sample at least this often (time units) ...
const SAMPLE_DT: f64 = REV / 240.0;
/// ... or whenever a body has moved this far (units of a), which keeps fast swings smooth.
const SAMPLE_DS: f64 = 0.006;
/// Never rewrite closer than this to what is on screen (the worker may lag a little).
const SAFETY_MARGIN: f64 = 0.5 * REV;

/// Positions and velocities of the three bodies at time `t`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub t: f64,
    pub p: [[f32; 2]; NB],
    pub v: [[f32; 2]; NB],
    /// Path length each body has travelled since the scene began (along the sample chords).
    /// Stable once a sample is shown, so renderers can pin things to places on the path.
    pub s: [f64; NB],
}

impl Sample {
    fn from_state(t: f64, y: &Y, s: [f64; NB]) -> Self {
        Self {
            t,
            p: std::array::from_fn(|b| [y[2 * b] as f32, y[2 * b + 1] as f32]),
            v: std::array::from_fn(|b| [y[6 + 2 * b] as f32, y[7 + 2 * b] as f32]),
            s,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub events: u32,
    pub nudges: u32,
    pub visible_nudges: u32,
    pub max_visible_eta: f64,
    pub failures: u32,
    pub steps: u64,
}

pub struct Timeline {
    model: Model,
    limits: Option<Limits>,
    steer: Option<SteerParams>,
    grid: f64,
    y: Y,
    t: f64,
    checkpoints: VecDeque<(i64, Y)>,
    samples: VecDeque<Sample>,
    last_nudge: f64,
    rng: Rng,
    failed_at: Option<f64>,
    pub stats: Stats,
}

fn emit(samples: &mut VecDeque<Sample>, s: &Step) {
    let last = samples.back().expect("timeline always holds a sample");
    let moved: [f64; NB] = std::array::from_fn(|b| {
        let dx = s.y1[2 * b] - last.p[b][0] as f64;
        let dy = s.y1[2 * b + 1] - last.p[b][1] as f64;
        dx.hypot(dy)
    });
    if moved.iter().any(|&d| d >= SAMPLE_DS) || s.t1 - last.t >= SAMPLE_DT {
        let travelled = std::array::from_fn(|b| last.s[b] + moved[b]);
        samples.push_back(Sample::from_state(s.t1, s.y1, travelled));
    }
}

impl Timeline {
    pub fn new(y0: Y, model: Model, limits: Option<Limits>, steer: Option<SteerParams>, seed: u64) -> Self {
        let mut checkpoints = VecDeque::new();
        checkpoints.push_back((0, y0));
        let mut samples = VecDeque::new();
        samples.push_back(Sample::from_state(0.0, &y0, [0.0; NB]));
        Self {
            model,
            limits,
            steer,
            grid: REV / 20.0,
            y: y0,
            t: 0.0,
            checkpoints,
            samples,
            last_nudge: 0.0,
            rng: Rng::new(seed ^ 0x5eed_cafe),
            failed_at: None,
            stats: Stats::default(),
        }
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    /// How far the committed future reaches.
    pub fn frontier(&self) -> f64 {
        self.t
    }

    /// Time of an event that steering could not prevent (the scene should end before it).
    pub fn failed_at(&self) -> Option<f64> {
        self.failed_at
    }

    pub fn samples(&self) -> &VecDeque<Sample> {
        &self.samples
    }

    /// Extend the committed future to at least `target`. Anything before `display` may already
    /// be on screen and is never changed. `preroll`: nothing is shown yet, so the start itself
    /// may be adjusted freely. Returns the failure time if steering ever gave up.
    pub fn fill(&mut self, display: f64, target: f64, preroll: bool) -> Option<f64> {
        let grid = self.grid;
        let target = (target / grid - 1e-9).ceil() * grid;
        while self.t < target - 1e-9 {
            let model = self.model;
            let checks = if self.failed_at.is_none() { self.limits } else { None };
            let samples = &mut self.samples;
            let checkpoints = &mut self.checkpoints;
            let steps = &mut self.stats.steps;
            let stop = integrate(
                &mut self.y,
                &mut self.t,
                target,
                &model,
                grid,
                |y| checks.as_ref().map_or(0, |l| flags(y, &model, l)),
                |s| {
                    *steps += 1;
                    emit(samples, s);
                    if let Some(k) = s.grid_index {
                        checkpoints.push_back((k, *s.y1));
                    }
                },
            );
            if let Stop::Event { t: te, .. } = stop {
                self.stats.events += 1;
                if !self.steer_away(te, display, preroll) {
                    self.failed_at = Some(te);
                    self.stats.failures += 1;
                }
            }
        }
        self.failed_at
    }

    fn steer_away(&mut self, te: f64, display: f64, preroll: bool) -> bool {
        let (Some(steer), Some(limits)) = (self.steer, self.limits) else {
            return false;
        };
        let earliest = if preroll { 0.0 } else { display + SAFETY_MARGIN };
        let tn_raw = (te - steer.buffer).max(self.last_nudge).max(earliest).max(0.0);
        let kn = (tn_raw / self.grid - 1e-9).ceil() as i64;
        let tn = kn as f64 * self.grid;
        if tn >= te {
            return false;
        }
        let Some(yn) = self.checkpoints.iter().find(|c| c.0 == kn).map(|c| c.1) else {
            return false;
        };
        let pre = preroll && kn == 0;
        let model = self.model;
        let Some(choice) = search(&yn, tn, te, f64::INFINITY, &model, &limits, &steer, self.grid, pre, &mut self.rng)
        else {
            return false;
        };
        self.rewrite(kn, choice.y);
        self.last_nudge = tn;
        self.stats.nudges += 1;
        if kn > 0 {
            self.stats.visible_nudges += 1;
            self.stats.max_visible_eta = self.stats.max_visible_eta.max(choice.eta);
        }
        true
    }

    /// Replace the state at grid point `kn` and drop everything after it.
    fn rewrite(&mut self, kn: i64, y: Y) {
        let tn = kn as f64 * self.grid;
        while self.checkpoints.back().is_some_and(|c| c.0 > kn) {
            self.checkpoints.pop_back();
        }
        match self.checkpoints.back_mut() {
            Some(c) if c.0 == kn => c.1 = y,
            _ => self.checkpoints.push_back((kn, y)),
        }
        while self.samples.len() > 1 && self.samples.back().is_some_and(|s| s.t > tn + 1e-12) {
            self.samples.pop_back();
        }
        self.y = y;
        self.t = tn;
    }

    /// Forget samples and checkpoints that can no longer matter (older than `before`).
    pub fn trim(&mut self, before: f64) {
        while self.samples.len() > 2 && self.samples[1].t <= before {
            self.samples.pop_front();
        }
        let kb = (before / self.grid).floor() as i64;
        while self.checkpoints.len() > 1 && self.checkpoints[1].0 <= kb {
            self.checkpoints.pop_front();
        }
    }

    /// Interpolated positions at time `t` (must lie within the committed samples).
    pub fn positions_at(&self, t: f64) -> Option<[[f64; 2]; NB]> {
        interpolate(self.samples.len(), t, |i| self.samples[i])
    }
}

/// Interpolate positions at `t` from `len` time-ordered samples (Hermite, using velocities).
pub fn interpolate(len: usize, t: f64, get: impl Fn(usize) -> Sample) -> Option<[[f64; 2]; NB]> {
    if len == 0 || t < get(0).t - 1e-9 || t > get(len - 1).t + 1e-9 {
        return None;
    }
    // binary search for the last sample with time <= t
    let (mut lo, mut hi) = (0usize, len - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if get(mid).t <= t {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let a = get(lo);
    if lo + 1 >= len {
        return Some(std::array::from_fn(|b| [a.p[b][0] as f64, a.p[b][1] as f64]));
    }
    let b = get(lo + 1);
    Some(hermite(a.t, &state_of(&a), b.t, &state_of(&b), t))
}

fn state_of(s: &Sample) -> Y {
    let mut y = [0.0; 12];
    for b in 0..NB {
        y[2 * b] = s.p[b][0] as f64;
        y[2 * b + 1] = s.p[b][1] as f64;
        y[6 + 2 * b] = s.v[b][0] as f64;
        y[7 + 2 * b] = s.v[b][1] as f64;
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::random_democratic;

    #[test]
    fn realtime_fill_matches_constraints() {
        let model = Model::default();
        let limits = Limits::dance();
        let mut rng = Rng::new(11);
        let y0 = random_democratic(&mut rng, &model);
        let mut tl = Timeline::new(y0, model, Some(limits), Some(SteerParams::default()), 11);
        let buffer = SteerParams::default().buffer;
        tl.fill(0.0, buffer, true);
        // advance a pretend display clock in small increments, as the app would
        let mut display = 0.0;
        while display < 30.0 * REV && tl.failed_at().is_none() {
            display += 0.25 * REV;
            tl.fill(display, display + buffer, false);
            tl.trim(display - 2.0 * REV);
        }
        let s = tl.samples();
        assert!(s.iter().zip(s.iter().skip(1)).all(|(a, b)| b.t > a.t), "samples must be strictly ordered");
        assert!(
            s.iter().zip(s.iter().skip(1)).all(|(a, b)| (0..NB).all(|k| b.s[k] >= a.s[k])),
            "path length must never decrease"
        );
        assert!(tl.positions_at(display).is_some());
        if tl.failed_at().is_none() {
            assert!(tl.stats.max_visible_eta <= 1e-2);
        }
    }
}
