//! Lachesis: measures the thread before it is spun. When the simulated future contains an
//! event, try tiny conservation-preserving nudges some revolutions earlier and keep the
//! smallest one whose future is clean. Chaos amplifies a perturbation roughly 3x per
//! revolution, so nudges of 1e-9 placed 8 revolutions ahead usually suffice.

use crate::events::{Limits, flags};
use crate::physics::{Model, NB, REV, Stop, Y, integrate, to_com};
use crate::rng::Rng;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SteerParams {
    /// How far ahead of the display the physics runs (time units). Nudges are placed up to this
    /// long before the event they prevent.
    pub buffer: f64,
    /// A candidate must stay clean until this long after the original event.
    pub horizon: f64,
    /// Smallest relative nudge tried first; each level multiplies by `escalate` up to `eta_max`.
    pub eta0: f64,
    pub eta_max: f64,
    pub escalate: f64,
    /// Before anything is displayed, nudges at t = 0 may be this large (choosing the start).
    pub eta_preroll: f64,
    /// Candidates per level.
    pub candidates: usize,
    /// Accept a level's best candidate if it postpones the event by at least this much.
    pub min_postpone: f64,
    /// Only nudge velocities: positions stay continuous on screen.
    pub velocity_only: bool,
}

impl Default for SteerParams {
    fn default() -> Self {
        Self {
            buffer: 8.0 * REV,
            horizon: 4.0 * REV,
            eta0: 1e-9,
            eta_max: 1e-2,
            escalate: 10.0,
            eta_preroll: 0.3,
            candidates: 64,
            min_postpone: REV,
            velocity_only: true,
        }
    }
}

/// Perturb a state by a relative amount `eta` while keeping momentum, angular momentum and
/// energy exactly as they were.
pub fn nudge(y: &mut Y, m: &Model, eta: f64, rng: &mut Rng, velocity_only: bool) {
    let e0 = m.energy(y);
    let l0 = m.angular_momentum(y);
    let mt = m.total_mass();
    let (mut r, mut v) = (0.0, 0.0);
    for i in 0..NB {
        r += m.m[i] * (y[2 * i].powi(2) + y[2 * i + 1].powi(2));
        v += m.m[i] * (y[6 + 2 * i].powi(2) + y[7 + 2 * i].powi(2));
    }
    let (r, v) = ((r / mt).sqrt(), (v / mt).sqrt());
    for i in 0..6 {
        if !velocity_only {
            y[i] += eta * r * rng.normal();
        }
        y[6 + i] += eta * v * rng.normal();
    }
    if velocity_only {
        // cancel only the momentum change; positions stay bit-identical
        let p = m.momentum(y);
        for i in 0..NB {
            y[6 + 2 * i] -= p[0] / mt;
            y[7 + 2 * i] -= p[1] / mt;
        }
    } else {
        to_com(y, m);
    }
    // restore angular momentum with a rigid-rotation increment
    let inertia: f64 = (0..NB).map(|i| m.m[i] * (y[2 * i].powi(2) + y[2 * i + 1].powi(2))).sum();
    let w = (l0 - m.angular_momentum(y)) / inertia;
    for i in 0..NB {
        y[6 + 2 * i] -= w * y[2 * i + 1];
        y[7 + 2 * i] += w * y[2 * i];
    }
    // restore energy by scaling only the non-rotational part of the velocities (keeps L and P)
    let wr = l0 / inertia;
    let t_rot = 0.5 * wr * wr * inertia;
    let u = m.potential(y);
    let mut rest = [0.0; 6];
    let mut t_rest = 0.0;
    for i in 0..NB {
        rest[2 * i] = y[6 + 2 * i] + wr * y[2 * i + 1];
        rest[2 * i + 1] = y[7 + 2 * i] - wr * y[2 * i];
        t_rest += 0.5 * m.m[i] * (rest[2 * i].powi(2) + rest[2 * i + 1].powi(2));
    }
    let s2 = (e0 - u - t_rot) / t_rest;
    let s = if s2 > 0.0 { s2.sqrt() } else { 1.0 };
    for i in 0..NB {
        y[6 + 2 * i] = -wr * y[2 * i + 1] + s * rest[2 * i];
        y[7 + 2 * i] = wr * y[2 * i] + s * rest[2 * i + 1];
    }
}

/// A chosen nudge.
#[derive(Clone, Copy, Debug)]
pub struct Choice {
    /// The nudged state at the nudge time.
    pub y: Y,
    pub eta: f64,
    /// The candidate stayed clean through the whole evaluation window.
    pub clean: bool,
}

/// Find a nudge at grid time `tn` (state `yn`) that removes the event at `te`.
/// Returns `None` if no candidate postpones the event at all.
#[allow(clippy::too_many_arguments)]
pub fn search(
    yn: &Y,
    tn: f64,
    te: f64,
    t_cap: f64,
    m: &Model,
    l: &Limits,
    p: &SteerParams,
    grid: f64,
    preroll: bool,
    rng: &mut Rng,
) -> Option<Choice> {
    let eta_top = if preroll { p.eta_max.max(p.eta_preroll) } else { p.eta_max };
    let t_eval = (te + p.horizon).min(t_cap);
    let mut best: Option<(f64, Y, f64)> = None;
    let mut eta = p.eta0;
    while eta <= eta_top * 1.0001 {
        let mut level_best: Option<(f64, Y)> = None;
        for _ in 0..p.candidates {
            let mut yc = *yn;
            nudge(&mut yc, m, eta, rng, p.velocity_only);
            let (mut ys, mut tc) = (yc, tn);
            match integrate(&mut ys, &mut tc, t_eval, m, grid, |y| flags(y, m, l), |_| {}) {
                Stop::Reached => return Some(Choice { y: yc, eta, clean: true }),
                Stop::Event { t, .. } => {
                    if level_best.is_none_or(|(r, _)| t > r) {
                        level_best = Some((t, yc));
                    }
                    if best.is_none_or(|(r, _, _)| t > r) {
                        best = Some((t, yc, eta));
                    }
                }
            }
        }
        if let Some((reach, y)) = level_best
            && reach >= te + p.min_postpone {
                return Some(Choice { y, eta, clean: false });
            }
        eta *= p.escalate;
    }
    match best {
        Some((reach, y, eta)) if reach > te + 1e-9 => Some(Choice { y, eta, clean: false }),
        _ => None,
    }
}

/// Result of an offline steered run.
#[derive(Clone, Debug)]
pub struct Offline {
    /// (time, nudged state, eta) in order.
    pub nudges: Vec<(f64, Y, f64)>,
    pub failed_at: Option<f64>,
    pub end: f64,
}

/// Steer a whole run at once, as if a display were trailing the physics by `p.buffer`.
pub fn run_offline(y0: &Y, t_end: f64, m: &Model, l: &Limits, p: &SteerParams, grid: f64, rng: &mut Rng) -> Offline {
    let kmax = (t_end / grid) as usize + 4;
    let mut ck: Vec<Option<Y>> = vec![None; kmax];
    ck[0] = Some(*y0);
    let (mut y, mut t) = (*y0, 0.0);
    let mut last = 0.0;
    let mut nudges = Vec::new();
    loop {
        let stop = integrate(&mut y, &mut t, t_end, m, grid, |s| flags(s, m, l), |s| {
            if let Some(k) = s.grid_index
                && (k as usize) < kmax {
                    ck[k as usize] = Some(*s.y1);
                }
        });
        let Stop::Event { t: te, .. } = stop else {
            return Offline { nudges, failed_at: None, end: t };
        };
        let kn = ((te - p.buffer).max(last).max(0.0) / grid - 1e-9).ceil() as usize;
        let tn = kn as f64 * grid;
        if tn >= te {
            return Offline { nudges, failed_at: Some(te), end: te };
        }
        let yn = ck[kn].expect("checkpoint");
        match search(&yn, tn, te, t_end, m, l, p, grid, kn == 0, rng) {
            Some(c) => {
                ck[kn] = Some(c.y);
                nudges.push((tn, c.y, c.eta));
                y = c.y;
                t = tn;
                last = tn;
            }
            None => return Offline { nudges, failed_at: Some(te), end: te },
        }
    }
}

/// Re-run a steered trajectory from its start, applying the recorded nudges, and count events.
pub fn replay_events(y0: &Y, run: &Offline, m: &Model, l: &Limits, grid: f64) -> usize {
    let (mut y, mut t) = (*y0, 0.0);
    let mut events = 0;
    for &(tn, yn, _) in &run.nudges {
        if tn > t
            && let Stop::Event { .. } = integrate(&mut y, &mut t, tn, m, grid, |s| flags(s, m, l), |_| {}) {
                events += 1;
                integrate(&mut y, &mut t, tn, m, grid, |_| 0, |_| {});
            }
        y = yn;
    }
    if t < run.end
        && let Stop::Event { .. } = integrate(&mut y, &mut t, run.end, m, grid, |s| flags(s, m, l), |_| {}) {
            events += 1;
        }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::random_democratic;

    #[test]
    fn nudge_preserves_invariants() {
        let m = Model::default();
        let mut rng = Rng::new(7);
        let mut y = random_democratic(&mut rng, &m);
        let (e0, l0, p0) = (m.energy(&y), m.angular_momentum(&y), m.momentum(&y));
        let before = y;
        nudge(&mut y, &m, 1e-3, &mut rng, true);
        assert!(((m.energy(&y) - e0) / e0).abs() < 1e-12);
        assert!((m.angular_momentum(&y) - l0).abs() < 1e-12);
        let p = m.momentum(&y);
        assert!(p[0].abs() + p[1].abs() < 1e-12 + p0[0].abs() + p0[1].abs());
        assert_eq!(&y[..6], &before[..6], "velocity-only nudge must not move bodies");
        assert_ne!(&y[6..], &before[6..]);
    }

    #[test]
    fn steered_dance_stays_clean_with_tiny_nudges() {
        let m = Model::default();
        let l = Limits::dance();
        let p = SteerParams::default();
        let grid = REV / 20.0;
        let mut clean = 0;
        for seed in 0..4u64 {
            let mut rng = Rng::new(seed);
            let y0 = random_democratic(&mut rng, &m);
            let run = run_offline(&y0, 40.0 * REV, &m, &l, &p, grid, &mut rng);
            if run.failed_at.is_none() {
                clean += 1;
                assert_eq!(replay_events(&y0, &run, &m, &l, grid), 0, "replay must reproduce the clean path");
                let visible: Vec<f64> = run.nudges.iter().filter(|n| n.0 > 0.0).map(|n| n.2).collect();
                let tiny = visible.iter().filter(|&&e| e <= 1e-6).count();
                assert!(tiny * 2 >= visible.len(), "most visible nudges should be tiny: {visible:?}");
            }
        }
        assert!(clean >= 3, "only {clean}/4 runs stayed clean for 40 revolutions");
    }
}
