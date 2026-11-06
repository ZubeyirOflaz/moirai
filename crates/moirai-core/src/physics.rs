//! Planar three-body physics: forces, conserved quantities and a deterministic adaptive RK4
//! integrator whose steps land exactly on a fixed time grid. Restarting from any grid state
//! reproduces the original continuation bit for bit, which the look-ahead steering relies on.

pub const NB: usize = 3;

/// One "revolution": the figure-eight period at the normalised energy E = -1.5 (size a = 1).
pub const REV: f64 = 5.028_36;

/// Positions (x0, y0, x1, y1, x2, y2) followed by the matching velocities.
pub type Y = [f64; 12];

/// Force law and integrator settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Model {
    pub m: [f64; NB],
    pub g: f64,
    /// Plummer softening of the Newtonian force: the "soft core" safety net (0 = pure Newton).
    pub eps: f64,
    /// Soft wall: an extra spring `wall_k * (r - wall_r)` between any pair farther apart than
    /// `wall_r`. Exactly zero inside, so the visible dynamics stay Newtonian (`wall_k = 0` = off).
    pub wall_r: f64,
    pub wall_k: f64,
    /// Accuracy: step = eta * min over pairs of (free-fall time, crossing time).
    pub eta: f64,
}

impl Default for Model {
    fn default() -> Self {
        Self { m: [1.0; NB], g: 1.0, eps: 0.0, wall_r: 0.0, wall_k: 0.0, eta: 0.01 }
    }
}

#[inline]
fn pair(i: usize) -> (usize, usize) {
    // pairs (0,1), (0,2), (1,2)
    match i {
        0 => (0, 1),
        1 => (0, 2),
        _ => (1, 2),
    }
}

impl Model {
    /// Plain Newtonian gravity with the same masses (used for normalisation).
    pub fn newtonian(&self) -> Self {
        Self { eps: 0.0, wall_k: 0.0, ..*self }
    }

    pub fn total_mass(&self) -> f64 {
        self.m.iter().sum()
    }

    pub fn accel(&self, y: &Y, a: &mut [f64; 6]) {
        *a = [0.0; 6];
        for p in 0..3 {
            let (i, j) = pair(p);
            let dx = y[2 * j] - y[2 * i];
            let dy = y[2 * j + 1] - y[2 * i + 1];
            let r2 = dx * dx + dy * dy;
            let s2 = r2 + self.eps * self.eps;
            let mut f = self.g / (s2 * s2.sqrt());
            if self.wall_k > 0.0 {
                let r = r2.sqrt();
                if r > self.wall_r {
                    f += self.wall_k * (r - self.wall_r) / r;
                }
            }
            a[2 * i] += self.m[j] * f * dx;
            a[2 * i + 1] += self.m[j] * f * dy;
            a[2 * j] -= self.m[i] * f * dx;
            a[2 * j + 1] -= self.m[i] * f * dy;
        }
    }

    pub fn potential(&self, y: &Y) -> f64 {
        let mut u = 0.0;
        for p in 0..3 {
            let (i, j) = pair(p);
            let dx = y[2 * j] - y[2 * i];
            let dy = y[2 * j + 1] - y[2 * i + 1];
            let r2 = dx * dx + dy * dy;
            let mm = self.m[i] * self.m[j];
            u -= self.g * mm / (r2 + self.eps * self.eps).sqrt();
            if self.wall_k > 0.0 {
                let r = r2.sqrt();
                if r > self.wall_r {
                    u += 0.5 * self.wall_k * mm * (r - self.wall_r) * (r - self.wall_r);
                }
            }
        }
        u
    }

    pub fn kinetic(&self, y: &Y) -> f64 {
        (0..NB).map(|i| 0.5 * self.m[i] * (y[6 + 2 * i].powi(2) + y[7 + 2 * i].powi(2))).sum()
    }

    pub fn energy(&self, y: &Y) -> f64 {
        self.kinetic(y) + self.potential(y)
    }

    pub fn angular_momentum(&self, y: &Y) -> f64 {
        (0..NB).map(|i| self.m[i] * (y[2 * i] * y[7 + 2 * i] - y[2 * i + 1] * y[6 + 2 * i])).sum()
    }

    pub fn momentum(&self, y: &Y) -> [f64; 2] {
        let mut p = [0.0; 2];
        for i in 0..NB {
            p[0] += self.m[i] * y[6 + 2 * i];
            p[1] += self.m[i] * y[7 + 2 * i];
        }
        p
    }

    /// Adaptive step size: a fraction of the shortest free-fall or crossing time of any pair.
    pub fn timestep(&self, y: &Y) -> f64 {
        let mut tm = f64::INFINITY;
        for p in 0..3 {
            let (i, j) = pair(p);
            let dx = y[2 * j] - y[2 * i];
            let dy = y[2 * j + 1] - y[2 * i + 1];
            let s2 = dx * dx + dy * dy + self.eps * self.eps;
            let tff = (s2 * s2.sqrt() / (self.g * (self.m[i] + self.m[j]))).sqrt();
            let dvx = y[6 + 2 * j] - y[6 + 2 * i];
            let dvy = y[7 + 2 * j] - y[7 + 2 * i];
            let v2 = dvx * dvx + dvy * dvy;
            tm = tm.min(tff);
            if v2 > 0.0 {
                tm = tm.min((s2 / v2).sqrt());
            }
            if self.wall_k > 0.0 {
                tm = tm.min(1.0 / (self.wall_k * (self.m[i] + self.m[j])).sqrt());
            }
        }
        self.eta * tm
    }

    /// One classical Runge-Kutta 4 step.
    pub fn rk4(&self, y: &mut Y, h: f64) {
        let (mut k1, mut k2, mut k3, mut k4) = ([0.0; 6], [0.0; 6], [0.0; 6], [0.0; 6]);
        let mut t = [0.0; 12];
        self.accel(y, &mut k1);
        for i in 0..6 {
            t[i] = y[i] + 0.5 * h * y[6 + i];
            t[6 + i] = y[6 + i] + 0.5 * h * k1[i];
        }
        self.accel(&t, &mut k2);
        let v2: [f64; 6] = std::array::from_fn(|i| t[6 + i]);
        for i in 0..6 {
            t[i] = y[i] + 0.5 * h * v2[i];
            t[6 + i] = y[6 + i] + 0.5 * h * k2[i];
        }
        self.accel(&t, &mut k3);
        let v3: [f64; 6] = std::array::from_fn(|i| t[6 + i]);
        for i in 0..6 {
            t[i] = y[i] + h * v3[i];
            t[6 + i] = y[6 + i] + h * k3[i];
        }
        self.accel(&t, &mut k4);
        let v4: [f64; 6] = std::array::from_fn(|i| t[6 + i]);
        for i in 0..6 {
            y[i] += h / 6.0 * (y[6 + i] + 2.0 * v2[i] + 2.0 * v3[i] + v4[i]);
            y[6 + i] += h / 6.0 * (k1[i] + 2.0 * k2[i] + 2.0 * k3[i] + k4[i]);
        }
    }
}

/// Shift to the centre-of-mass frame (position and velocity).
pub fn to_com(y: &mut Y, m: &Model) {
    let mt = m.total_mass();
    let mut c = [0.0; 4];
    for i in 0..NB {
        for k in 0..2 {
            c[k] += m.m[i] * y[2 * i + k];
            c[2 + k] += m.m[i] * y[6 + 2 * i + k];
        }
    }
    for i in 0..NB {
        for k in 0..2 {
            y[2 * i + k] -= c[k] / mt;
            y[6 + 2 * i + k] -= c[2 + k] / mt;
        }
    }
}

/// Rescale a bound state so its Newtonian energy becomes `e_target` (< 0).
/// Uses the gravity scaling x -> a x, v -> v / sqrt(a), t -> a^1.5 t; returns `a`.
pub fn normalise(y: &mut Y, m: &Model, e_target: f64) -> f64 {
    let e = m.newtonian().energy(y);
    let a = e / e_target;
    for i in 0..6 {
        y[i] *= a;
        y[6 + i] /= a.sqrt();
    }
    a
}

/// Cubic Hermite interpolation of the three positions between two states.
pub fn hermite(t0: f64, y0: &Y, t1: f64, y1: &Y, t: f64) -> [[f64; 2]; NB] {
    let h = t1 - t0;
    let s = if h > 0.0 { ((t - t0) / h).clamp(0.0, 1.0) } else { 1.0 };
    let (s2, s3) = (s * s, s * s * s);
    let (h00, h10, h01, h11) = (2.0 * s3 - 3.0 * s2 + 1.0, s3 - 2.0 * s2 + s, -2.0 * s3 + 3.0 * s2, s3 - s2);
    std::array::from_fn(|b| {
        std::array::from_fn(|k| {
            let i = 2 * b + k;
            h00 * y0[i] + h10 * h * y0[6 + i] + h01 * y1[i] + h11 * h * y1[6 + i]
        })
    })
}

/// Why an integration stopped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stop {
    Reached,
    Event { t: f64, flags: u32 },
}

/// One accepted integration step, reported to observers.
pub struct Step<'a> {
    pub t0: f64,
    pub y0: &'a Y,
    pub t1: f64,
    pub y1: &'a Y,
    /// `Some(k)` when the step ended exactly on grid point `k * grid`.
    pub grid_index: Option<i64>,
}

/// Steps closer than this are treated as a collision (only reachable without softening).
const MIN_STEP: f64 = 1e-11;

/// Integrate from `*t` to `t_end`. Every step that would cross the next grid point is shortened
/// to land on it, so the step sequence depends only on the state at the last grid point.
/// `check` returns event flags (0 = fine); the first non-zero result stops the integration.
pub fn integrate(
    y: &mut Y,
    t: &mut f64,
    t_end: f64,
    model: &Model,
    grid: f64,
    mut check: impl FnMut(&Y) -> u32,
    mut on_step: impl FnMut(&Step),
) -> Stop {
    let mut kn = (*t / grid + 1e-9).floor() as i64 + 1;
    while *t < t_end - 1e-12 {
        let mut h = model.timestep(y);
        if h < MIN_STEP {
            return Stop::Event { t: *t, flags: crate::events::CLOSE };
        }
        let tg = (kn as f64 * grid).min(t_end);
        let land = *t + h >= tg - 1e-15;
        if land {
            h = tg - *t;
        }
        let y_old = *y;
        let t_old = *t;
        model.rk4(y, h);
        *t = if land { tg } else { *t + h };
        let grid_index = (land && (*t - kn as f64 * grid).abs() < 1e-9).then_some(kn);
        on_step(&Step { t0: t_old, y0: &y_old, t1: *t, y1: &*y, grid_index });
        let f = check(y);
        if f != 0 {
            return Stop::Event { t: *t, flags: f };
        }
        if grid_index.is_some() {
            kn += 1;
        }
    }
    Stop::Reached
}

/// The figure-eight orbit (Hristov, Hristova & Tanikawa 2025, orbit #1) in its native units.
pub fn figure_eight() -> (Y, f64) {
    let (vx, vy) = (0.347_116_888_118_926_94, 0.532_724_945_388_030_3);
    ([-1.0, 0.0, 0.0, 0.0, 1.0, 0.0, vx, vy, -2.0 * vx, -2.0 * vy, vx, vy], 6.325_913_982_926_211)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figure_eight_closes_and_conserves() {
        let m = Model::default();
        let (mut y, period) = figure_eight();
        let y0 = y;
        let (e0, l0) = (m.energy(&y), m.angular_momentum(&y));
        let mut t = 0.0;
        let stop = integrate(&mut y, &mut t, 100.0 * period, &m, period, |_| 0, |_| {});
        assert_eq!(stop, Stop::Reached);
        let dev = y.iter().zip(&y0).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
        assert!(dev < 1e-5, "returned within {dev}");
        assert!(((m.energy(&y) - e0) / e0).abs() < 1e-8);
        assert!((m.angular_momentum(&y) - l0).abs() < 1e-10);
    }

    #[test]
    fn grid_restart_is_bit_identical() {
        let m = Model::default();
        let (y0, _) = figure_eight();
        let grid = REV / 20.0;
        // Run A: straight to t_end, remembering the state at grid point 37.
        let (mut ya, mut ta) = (y0, 0.0);
        let mut at37 = None;
        integrate(&mut ya, &mut ta, 3.0 * REV, &m, grid, |_| 0, |s| {
            if s.grid_index == Some(37) {
                at37 = Some(*s.y1);
            }
        });
        // Run B: restart from grid point 37.
        let (mut yb, mut tb) = (at37.unwrap(), 37.0 * grid);
        integrate(&mut yb, &mut tb, 3.0 * REV, &m, grid, |_| 0, |_| {});
        assert_eq!(ya, yb);
    }

    #[test]
    fn normalisation_sets_energy() {
        let m = Model::default();
        let (mut y, _) = figure_eight();
        normalise(&mut y, &m, -1.5);
        assert!((m.energy(&y) + 1.5).abs() < 1e-12);
    }
}
