//! The planet: a massless world among the three suns (the restricted four-body problem). The
//! stars pull it and it pulls nothing, so they move exactly as they would without it.
//!
//! It rides along with the timeline: inside every step of the stars it takes small steps of its
//! own, reading the stars' positions from that step's dense output (cubic Hermite), so it is as
//! repeatable as they are and is recomputed whenever steering rewrites their future. When it is
//! swallowed by a star or leaves the system, a new planet starts on a circular orbit in the
//! habitable zone of the star farthest from the other two.
//!
//! Along the way it keeps a chronicle: its surface temperature (the suns' light, with thermal
//! inertia), whether it lives through a stable or a chaotic era, and its civilisations, which
//! perish when the seas boil or freeze and rise again once the climate has been mild for a while.

use std::collections::VecDeque;
use std::f64::consts::TAU;

use crate::physics::{Model, NB, REV, Step};
use crate::rng::Rng;

/// Radius of a circular orbit in a sun's habitable zone (units of a).
pub const HZ_RADIUS: f64 = 0.12;
/// Radius of a Sun-like star whose habitable zone lies at `HZ_RADIUS` (1 AU is 215 solar radii).
pub const STAR_RADIUS: f64 = HZ_RADIUS / 215.0;
/// A planet this close to a star's centre has hit it, or been torn apart by its tides if molten.
pub const SWALLOW_RADIUS: f64 = 1.5 * STAR_RADIUS;
/// A planet that is no longer bound to the system has left it for good once it is this many
/// times the stars' spread (their largest distance from the centre of mass, at least 1) away from
/// the centre of mass: about where it leaves the picture. One twice as far is given up on even if
/// it is still bound (it would be out of sight for a long time).
pub const EJECT_SPREAD: f64 = 2.0;

/// Surface temperature in a sun's habitable zone with the other suns far away (K): Earth's.
pub const HZ_TEMP: f64 = 288.0;
/// How quickly the surface follows the suns' light: its radiated energy (T^4) relaxes toward
/// the received with this time constant (time units; a year in the habitable zone is about 0.26).
/// It heats up quickly and cools down slowly.
const THERMAL_TIME: f64 = 0.3;
/// Calm: bound to the nearest sun, whose pull the other suns' tides disturb by less than this share.
const CALM_TIDE: f64 = 0.05;
/// A stable era begins once it has been calm this long (time units: about two planet years) and
/// ends after unrest this long.
const ERA_SETTLE: f64 = 0.6;
const ERA_BREAK: f64 = 0.12;
/// A civilisation perishes when the surface gets this hot (the seas boil) or this cold (they
/// freeze solid), in K ...
pub const CIV_HOT: f64 = 400.0;
pub const CIV_COLD: f64 = 180.0;
/// ... and a new one rises once the climate has stayed mild (between these, K) this long.
pub const MILD: (f64, f64) = (250.0, 330.0);
const CIV_RISE: f64 = 1.5;

/// Accuracy: each step is this share of the shortest orbital time scale sqrt(d^3 / G m) to any star.
const ETA: f64 = 0.015;
/// A sample of the planet's path is kept at least this often (time units), whenever it has moved
/// this far, or once its direction has turned this much (radians).
const SAMPLE_DT: f64 = REV / 240.0;
const SAMPLE_DS: f64 = 0.008;
const SAMPLE_TURN: f64 = 0.15;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Planet {
    pub p: [f64; 2],
    pub v: [f64; 2],
    /// Which planet this is: 0 for the first of a scene, counting up with every new start.
    pub run: u32,
    pub story: Story,
}

/// What a planet has lived through, as the readout shows it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chronicle {
    /// Surface temperature (K).
    pub temp: f32,
    /// Whether the current era is stable (bound to one sun, undisturbed) or chaotic.
    pub stable: bool,
    /// Eras and civilisations so far on this planet, counting from 1.
    pub era: u32,
    pub civilisation: u32,
    /// Whether that civilisation is alive (otherwise the planet waits for the next).
    pub alive: bool,
}

/// The chronicle and what it takes to keep it: how long the conditions for the other kind of era
/// and for a new civilisation have held.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Story {
    temp: f64,
    stable: bool,
    era: u32,
    civilisation: u32,
    alive: bool,
    unrest: f64,
    mild: f64,
}

impl Story {
    /// A new planet: a stable era and a civilisation from the start.
    fn new(temp: f64) -> Self {
        Self { temp, stable: true, era: 1, civilisation: 1, alive: true, unrest: 0.0, mild: 0.0 }
    }

    pub fn chronicle(&self) -> Chronicle {
        Chronicle {
            temp: self.temp as f32,
            stable: self.stable,
            era: self.era,
            civilisation: self.civilisation,
            alive: self.alive,
        }
    }

    /// Live through `h` more time units at equilibrium temperature `teq`, calm or not. Returns
    /// whether the era or the civilisation changed.
    fn advance(&mut self, h: f64, teq: f64, calm: bool) -> bool {
        let (u, ueq) = (self.temp.powi(4), teq.powi(4));
        self.temp = (ueq + (u - ueq) * (-h / THERMAL_TIME).exp()).powf(0.25);
        let mut changed = false;
        self.unrest = if calm == self.stable { 0.0 } else { self.unrest + h };
        if self.unrest >= if self.stable { ERA_BREAK } else { ERA_SETTLE } {
            self.stable = !self.stable;
            self.era += 1;
            self.unrest = 0.0;
            changed = true;
        }
        if self.alive {
            if self.temp >= CIV_HOT || self.temp <= CIV_COLD {
                self.alive = false;
                self.mild = 0.0;
                changed = true;
            }
        } else {
            self.mild = if (MILD.0..=MILD.1).contains(&self.temp) { self.mild + h } else { 0.0 };
            if self.mild >= CIV_RISE {
                self.alive = true;
                self.civilisation += 1;
                changed = true;
            }
        }
        changed
    }
}

/// The surface temperature the suns' light would hold the planet at (K): each sun gives the
/// habitable-zone warmth at `HZ_RADIUS`, its light falling off with the square of the distance
/// (and the temperature with the fourth root of the light).
pub fn equilibrium_temp(stars: &[[f64; 2]; NB], p: [f64; 2]) -> f64 {
    let light: f64 = stars.iter().map(|s| HZ_RADIUS * HZ_RADIUS / ((s[0] - p[0]).powi(2) + (s[1] - p[1]).powi(2))).sum();
    HZ_TEMP * light.powf(0.25)
}

/// Calm: bound to the nearest sun, and the other suns' tides disturb its pull only a little.
fn calm(model: &Model, stars: &[[f64; 2]; NB], vstars: &[[f64; 2]; NB], b: &Planet) -> bool {
    let d = |s: [f64; 2]| (s[0] - b.p[0]).hypot(s[1] - b.p[1]);
    let host = (0..NB).min_by(|&i, &j| d(stars[i]).total_cmp(&d(stars[j]))).unwrap_or(0);
    let (sh, dh, mh) = (stars[host], d(stars[host]), model.m[host]);
    let rel = [b.v[0] - vstars[host][0], b.v[1] - vstars[host][1]];
    if 0.5 * (rel[0] * rel[0] + rel[1] * rel[1]) >= model.g * mh / dh {
        return false;
    }
    let mut tide = [0.0; 2];
    for j in (0..NB).filter(|&j| j != host) {
        let (to_p, to_h) = ([stars[j][0] - b.p[0], stars[j][1] - b.p[1]], [stars[j][0] - sh[0], stars[j][1] - sh[1]]);
        let (rp, rh) = ((to_p[0].hypot(to_p[1])).powi(3), (to_h[0].hypot(to_h[1])).powi(3));
        for k in 0..2 {
            tide[k] += model.g * model.m[j] * (to_p[k] / rp - to_h[k] / rh);
        }
    }
    tide[0].hypot(tide[1]) < CALM_TIDE * model.g * mh / (dh * dh)
}

/// How a planet's run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    Swallowed,
    Ejected,
}

/// One point of the planet's path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlanetSample {
    pub t: f64,
    pub p: [f32; 2],
    pub v: [f32; 2],
    pub run: u32,
    /// Set on the last sample of a run that ended (the next sample starts the new planet).
    pub fate: Option<Fate>,
    pub chronicle: Chronicle,
}

impl PlanetSample {
    fn of(t: f64, b: &Planet, fate: Option<Fate>) -> Self {
        Self { t, p: b.p.map(|x| x as f32), v: b.v.map(|x| x as f32), run: b.run, fate, chronicle: b.story.chronicle() }
    }
}

/// The stars' positions and velocities at `t` inside an integration step (cubic Hermite
/// interpolation of the step's end states and its derivative).
fn stars_at(step: &Step, t: f64) -> ([[f64; 2]; NB], [[f64; 2]; NB]) {
    let h = step.t1 - step.t0;
    let s = if h > 0.0 { ((t - step.t0) / h).clamp(0.0, 1.0) } else { 1.0 };
    let (s2, s3) = (s * s, s * s * s);
    let (h00, h10, h01, h11) = (2.0 * s3 - 3.0 * s2 + 1.0, s3 - 2.0 * s2 + s, -2.0 * s3 + 3.0 * s2, s3 - s2);
    let (d00, d10, d01, d11) = (6.0 * s2 - 6.0 * s, 3.0 * s2 - 4.0 * s + 1.0, 6.0 * s - 6.0 * s2, 3.0 * s2 - 2.0 * s);
    let (y0, y1) = (step.y0, step.y1);
    let mut p = [[0.0; 2]; NB];
    let mut v = [[0.0; 2]; NB];
    for b in 0..NB {
        for k in 0..2 {
            let i = 2 * b + k;
            let (x0, v0, x1, v1) = (y0[i], y0[6 + i], y1[i], y1[6 + i]);
            p[b][k] = h00 * x0 + h10 * h * v0 + h01 * x1 + h11 * h * v1;
            v[b][k] = if h > 0.0 { (d00 * x0 + d01 * x1) / h + d10 * v0 + d11 * v1 } else { v1 };
        }
    }
    (p, v)
}

fn accel(model: &Model, stars: &[[f64; 2]; NB], p: [f64; 2]) -> [f64; 2] {
    let mut a = [0.0; 2];
    for (s, m) in stars.iter().zip(model.m) {
        let (dx, dy) = (s[0] - p[0], s[1] - p[1]);
        let r2 = dx * dx + dy * dy;
        let k = model.g * m / (r2 * r2.sqrt());
        a[0] += k * dx;
        a[1] += k * dy;
    }
    a
}

/// Whether the planet's run is over: inside a star, or gone from the system.
pub fn fate(model: &Model, stars: &[[f64; 2]; NB], vstars: &[[f64; 2]; NB], b: &Planet) -> Option<Fate> {
    if stars.iter().any(|s| (s[0] - b.p[0]).hypot(s[1] - b.p[1]) < SWALLOW_RADIUS) {
        return Some(Fate::Swallowed);
    }
    let mass: f64 = model.m.iter().sum();
    let centre = |v: &[[f64; 2]; NB]| -> [f64; 2] {
        std::array::from_fn(|k| (0..NB).map(|i| model.m[i] * v[i][k]).sum::<f64>() / mass)
    };
    let (c, vc) = (centre(stars), centre(vstars));
    let spread = stars.iter().map(|s| (s[0] - c[0]).hypot(s[1] - c[1])).fold(1.0, f64::max);
    let r = (b.p[0] - c[0]).hypot(b.p[1] - c[1]);
    let out = EJECT_SPREAD * spread;
    if r <= out {
        return None;
    }
    // far out: gone if it is no longer bound to the system as a whole, or very far out
    let energy = 0.5 * ((b.v[0] - vc[0]).powi(2) + (b.v[1] - vc[1]).powi(2)) - model.g * mass / r;
    (energy > 0.0 || r > 2.0 * out).then_some(Fate::Ejected)
}

/// A new planet on a circular orbit in the habitable zone of the star farthest from the other
/// two, at a random place on that orbit and going either way round.
pub fn launch(model: &Model, stars: &[[f64; 2]; NB], vstars: &[[f64; 2]; NB], seed: u64, run: u32) -> Planet {
    let dist = |i: usize, j: usize| (stars[i][0] - stars[j][0]).hypot(stars[i][1] - stars[j][1]);
    let isolation = |k: usize| (0..NB).filter(|&j| j != k).map(|j| dist(k, j)).fold(f64::INFINITY, f64::min);
    let host = (0..NB).max_by(|&a, &b| isolation(a).total_cmp(&isolation(b))).unwrap_or(0);
    // the same planet for the same scene and run, however often the future is recomputed
    let mut rng = Rng::new(seed ^ u64::from(run).wrapping_add(1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let phase = rng.uniform() * TAU;
    let turn = if rng.uniform() < 0.5 { 1.0 } else { -1.0 };
    let speed = (model.g * model.m[host] / HZ_RADIUS).sqrt();
    let (s, v) = (stars[host], vstars[host]);
    let p = [s[0] + HZ_RADIUS * phase.cos(), s[1] + HZ_RADIUS * phase.sin()];
    Planet {
        p,
        v: [v[0] - turn * speed * phase.sin(), v[1] + turn * speed * phase.cos()],
        run,
        story: Story::new(equilibrium_temp(stars, p)),
    }
}

/// The planet of one scene: its current state and the path it has taken.
#[derive(Clone, Debug)]
pub struct PlanetSim {
    seed: u64,
    /// `None` until it is first placed (at the start of the first step).
    pub body: Option<Planet>,
    pub samples: VecDeque<PlanetSample>,
}

impl PlanetSim {
    pub fn new(seed: u64) -> Self {
        Self { seed, body: None, samples: VecDeque::new() }
    }

    /// Follow the planet through one step of the stars.
    pub fn advance(&mut self, model: &Model, step: &Step) {
        let mut t = step.t0;
        let mut b = match self.body {
            Some(b) => b,
            None => {
                let (s, v) = stars_at(step, t);
                let b = launch(model, &s, &v, self.seed, 0);
                self.samples.push_back(PlanetSample::of(t, &b, None));
                b
            }
        };
        while t < step.t1 - 1e-15 {
            let (s0, _) = stars_at(step, t);
            let tmin = s0
                .iter()
                .zip(model.m)
                .map(|(s, m)| {
                    let d2 = (s[0] - b.p[0]).powi(2) + (s[1] - b.p[1]).powi(2);
                    (d2 * d2.sqrt() / (model.g * m)).sqrt()
                })
                .fold(f64::INFINITY, f64::min);
            let mut h = (ETA * tmin).max(1e-12);
            let land = t + h >= step.t1 - 1e-15;
            if land {
                h = step.t1 - t;
            }
            // classic RK4 for the planet in the moving field of the stars
            let f = |tt: f64, p: [f64; 2]| accel(model, &stars_at(step, tt).0, p);
            let (p, v) = (b.p, b.v);
            let a1 = f(t, p);
            let p2 = [p[0] + 0.5 * h * v[0], p[1] + 0.5 * h * v[1]];
            let v2 = [v[0] + 0.5 * h * a1[0], v[1] + 0.5 * h * a1[1]];
            let a2 = f(t + 0.5 * h, p2);
            let p3 = [p[0] + 0.5 * h * v2[0], p[1] + 0.5 * h * v2[1]];
            let v3 = [v[0] + 0.5 * h * a2[0], v[1] + 0.5 * h * a2[1]];
            let a3 = f(t + 0.5 * h, p3);
            let p4 = [p[0] + h * v3[0], p[1] + h * v3[1]];
            let v4 = [v[0] + h * a3[0], v[1] + h * a3[1]];
            let a4 = f(t + h, p4);
            for k in 0..2 {
                b.p[k] = p[k] + h / 6.0 * (v[k] + 2.0 * v2[k] + 2.0 * v3[k] + v4[k]);
                b.v[k] = v[k] + h / 6.0 * (a1[k] + 2.0 * a2[k] + 2.0 * a3[k] + a4[k]);
            }
            t = if land { step.t1 } else { t + h };

            let (s1, v1) = stars_at(step, t);
            if let Some(end) = fate(model, &s1, &v1, &b) {
                self.samples.push_back(PlanetSample::of(t, &b, Some(end)));
                b = launch(model, &s1, &v1, self.seed, b.run + 1);
                self.samples.push_back(PlanetSample::of(t, &b, None));
            } else {
                let calm = calm(model, &s1, &v1, &b);
                let changed = b.story.advance(h, equilibrium_temp(&s1, b.p), calm);
                if changed || self.samples.back().is_none_or(|last| due(last, t, &b)) {
                    self.samples.push_back(PlanetSample::of(t, &b, None));
                }
            }
        }
        self.body = Some(b);
    }

    /// Forget the path after `t` and continue from `body` (the state at `t`), as when steering
    /// rewrites the stars' future from there.
    pub fn rewind(&mut self, t: f64, body: Option<Planet>) {
        // with nothing to continue from, the planet is placed afresh at `t`: drop the old placement too
        let keep_until = if body.is_some() { t + 1e-12 } else { t - 1e-12 };
        while self.samples.back().is_some_and(|s| s.t > keep_until) {
            self.samples.pop_back();
        }
        self.body = body;
    }

    /// Forget the path before `t` (keeping one sample before it).
    pub fn trim(&mut self, t: f64) {
        while self.samples.len() > 2 && self.samples[1].t <= t {
            self.samples.pop_front();
        }
    }
}

/// Whether the path has changed enough since `last` to keep another sample.
fn due(last: &PlanetSample, t: f64, b: &Planet) -> bool {
    let moved = (b.p[0] - last.p[0] as f64).hypot(b.p[1] - last.p[1] as f64);
    let (lv, v) = ([last.v[0] as f64, last.v[1] as f64], b.v);
    let turned = (lv[0] * v[1] - lv[1] * v[0]).atan2(lv[0] * v[0] + lv[1] * v[1]).abs();
    t - last.t >= SAMPLE_DT || moved >= SAMPLE_DS || turned >= SAMPLE_TURN
}

/// The planet's position at `t`, interpolated from its path (cubic Hermite between the samples
/// around `t`), and the last sample at or before `t` (its run and chronicle).
pub fn planet_at(samples: &VecDeque<PlanetSample>, t: f64) -> Option<([f64; 2], PlanetSample)> {
    let n = samples.len();
    if n == 0 || t < samples[0].t - 1e-9 || t > samples[n - 1].t + 1e-9 {
        return None;
    }
    let i = samples.partition_point(|s| s.t <= t).saturating_sub(1);
    let a = samples[i];
    let Some(b) = samples.get(i + 1).filter(|b| b.run == a.run && b.t > a.t) else {
        return Some((a.p.map(f64::from), a));
    };
    let h = b.t - a.t;
    let s = ((t - a.t) / h).clamp(0.0, 1.0);
    let (s2, s3) = (s * s, s * s * s);
    let (h00, h10, h01, h11) = (2.0 * s3 - 3.0 * s2 + 1.0, s3 - 2.0 * s2 + s, -2.0 * s3 + 3.0 * s2, s3 - s2);
    let p = std::array::from_fn(|k| {
        h00 * a.p[k] as f64 + h10 * h * a.v[k] as f64 + h01 * b.p[k] as f64 + h11 * h * b.v[k] as f64
    });
    Some((p, a))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::physics::{Y, integrate};

    /// A star at the origin, far from a wide pair of stars: it is the most isolated, so the planet
    /// starts around it, and it barely moves.
    fn lonely_star() -> (Model, Y) {
        let model = Model::default();
        let y: Y = [0.0, 0.0, 400.0, 0.0, 430.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        (model, y)
    }

    /// Integrate the stars to `t_end` with the planet riding along; the stars' final state.
    fn run(model: &Model, mut y: Y, t_end: f64, sim: &mut PlanetSim) -> Y {
        let mut t = 0.0;
        integrate(&mut y, &mut t, t_end, model, REV / 20.0, |_| 0, |s| sim.advance(model, s));
        y
    }

    fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
        (a[0] - b[0]).hypot(a[1] - b[1])
    }

    #[test]
    fn keeps_a_circular_orbit_around_a_lonely_star() {
        let (model, y0) = lonely_star();
        let mut sim = PlanetSim::new(5);
        let y = run(&model, y0, 2.0 * REV, &mut sim); // about 40 orbits
        let b = sim.body.expect("placed");
        assert!(sim.samples.iter().all(|s| s.run == 0 && s.fate.is_none()), "no restart");
        let r = dist(b.p, [y[0], y[1]]);
        assert!((r - HZ_RADIUS).abs() < 1e-4 * HZ_RADIUS, "radius drifted to {r}");
        let speed = dist(b.v, [y[6], y[7]]);
        assert!((speed - (1.0 / HZ_RADIUS).sqrt()).abs() < 1e-3, "speed {speed}");
        // a quiet life: Earth's warmth, one stable era, one civilisation
        let c = b.story.chronicle();
        assert!((c.temp - 288.0).abs() < 1.0, "{} K", c.temp);
        assert_eq!((c.stable, c.era, c.civilisation, c.alive), (true, 1, 1, true));
    }

    #[test]
    fn eras_and_civilisations_come_and_go() {
        let mut story = Story::new(HZ_TEMP);
        let h = 0.01;
        // a brief disturbance does not end a stable era, a lasting one does
        story.advance(0.5 * ERA_BREAK, HZ_TEMP, false);
        assert!(story.stable);
        story.advance(h, HZ_TEMP, true);
        for _ in 0..20 {
            story.advance(h, HZ_TEMP, false);
        }
        assert_eq!((story.stable, story.era), (false, 2));
        // two suns overhead: the seas boil and the civilisation perishes
        while story.alive {
            story.advance(h, 2500.0, false);
        }
        assert!(story.temp >= CIV_HOT);
        // calm and mild again for long enough: a new stable era and a new civilisation
        for _ in 0..1000 {
            story.advance(h, HZ_TEMP, true);
        }
        let c = story.chronicle();
        assert_eq!((c.stable, c.era, c.civilisation, c.alive), (true, 3, 2, true));
        assert!((c.temp - 288.0).abs() < 1.0);
        // heat comes fast, cold slowly
        let mut warm = Story::new(HZ_TEMP);
        let mut cool = Story::new(HZ_TEMP);
        warm.advance(0.1, 600.0, true);
        cool.advance(0.1, 100.0, true);
        assert!(warm.temp - HZ_TEMP > HZ_TEMP - cool.temp);
    }

    #[test]
    fn the_suns_light_sets_the_temperature() {
        let one = [[0.0, 0.0], [1e6, 0.0], [-1e6, 0.0]];
        assert!((equilibrium_temp(&one, [HZ_RADIUS, 0.0]) - HZ_TEMP).abs() < 1e-6);
        // twice as close: four times the light, the square root of two times the temperature
        let t = equilibrium_temp(&one, [0.5 * HZ_RADIUS, 0.0]);
        assert!((t - HZ_TEMP * 2f64.sqrt()).abs() < 1e-6);
        // two suns at the same distance: the fourth root of two times warmer
        let two = [[0.0, 0.0], [2.0 * HZ_RADIUS, 0.0], [1e6, 0.0]];
        assert!((equilibrium_temp(&two, [HZ_RADIUS, 0.0]) - HZ_TEMP * 2f64.powf(0.25)).abs() < 1e-6);
    }

    #[test]
    fn a_planet_hitting_a_star_is_replaced() {
        let (model, y0) = lonely_star();
        let mut sim = PlanetSim::new(9);
        // place it by hand, at rest next to the star at the origin, so it falls straight in
        let start = Planet { p: [0.05, 0.0], v: [0.0, 0.0], run: 0, story: Story::new(HZ_TEMP) };
        sim.body = Some(start);
        sim.samples.push_back(PlanetSample::of(0.0, &start, None));
        let y = run(&model, y0, 0.2 * REV, &mut sim);
        let ends: Vec<Fate> = sim.samples.iter().filter_map(|s| s.fate).collect();
        assert_eq!(ends, vec![Fate::Swallowed]);
        // the new planet circles the most isolated star in its habitable zone
        let b = sim.body.unwrap();
        assert_eq!(b.run, 1);
        assert!((dist(b.p, [y[0], y[1]]) - HZ_RADIUS).abs() < 1e-3);
    }

    #[test]
    fn a_planet_flung_away_is_replaced() {
        // the figure-eight (spread about 1): a planet heading out well above escape speed
        let model = Model::default();
        let (y0, _) = crate::physics::figure_eight();
        let mut sim = PlanetSim::new(4);
        let start = Planet { p: [0.0, 1.5], v: [0.0, 3.0], run: 0, story: Story::new(HZ_TEMP) };
        sim.body = Some(start);
        sim.samples.push_back(PlanetSample::of(0.0, &start, None));
        run(&model, y0, 0.2 * REV, &mut sim);
        let ends: Vec<(f64, Fate)> = sim.samples.iter().filter_map(|s| Some((s.t, s.fate?))).collect();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0].1, Fate::Ejected);
        // declared gone once it has passed twice the spread, not long after
        assert!(ends[0].0 < 0.5, "at t = {}", ends[0].0);
    }

    #[test]
    fn interpolates_its_path_within_a_run() {
        let (model, y0) = lonely_star();
        let mut sim = PlanetSim::new(3);
        run(&model, y0, 0.5 * REV, &mut sim);
        let s = &sim.samples;
        for i in 1..s.len() {
            let mid = 0.5 * (s[i - 1].t + s[i].t);
            let (p, sample) = planet_at(s, mid).unwrap();
            assert_eq!(sample.run, 0);
            // halfway between two samples it is still on its circle (the star moves < 1e-4)
            let r = dist(p, [0.0, 0.0]);
            assert!((r - HZ_RADIUS).abs() < 1e-3, "r = {r} at t = {mid}");
        }
    }
}
