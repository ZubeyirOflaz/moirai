//! Atropos: recognises the moments that would spoil the dance, ideally while they are still
//! only a prediction (and the dynamics are still chaotic enough to be steered away from them).

use crate::physics::{Model, NB, Y};

/// A pair is closer than `dmin`.
pub const CLOSE: u32 = 1;
/// A body is beyond `rfar` from the centre of mass, or its outgoing orbit will take it there.
pub const FAR: u32 = 2;
/// A hard binary is forming (pair semi-major axis below `amin`).
pub const BINARY: u32 = 4;
/// An isolated bound pair whose Kepler pericentre lies below `dmin`.
pub const PERI: u32 = 8;
/// A "frozen" hierarchical triple: outer pericentre above `hier` x inner semi-major axis.
/// Such states are regular (not chaotic), so tiny nudges lose their leverage once inside.
pub const HIER: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    pub dmin: f64,
    pub rfar: f64,
    /// 0 disables the hard-binary check.
    pub amin: f64,
    /// Enable the predictive checks (outer apocentre, isolated-pair pericentre).
    pub predict: bool,
    /// 0 disables the frozen-hierarchy check.
    pub hier: f64,
}

impl Limits {
    /// The settings validated in the research runs (98% clean through 30 revolutions).
    pub fn dance() -> Self {
        Self { dmin: 0.02, rfar: 3.0, amin: 0.2, predict: true, hier: 3.0 }
    }

    /// Plain geometric limits only (used to validate choreographies).
    pub fn geometric(dmin: f64, rfar: f64) -> Self {
        Self { dmin, rfar, amin: 0.0, predict: false, hier: 0.0 }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    /// Smallest pair distance.
    pub dmin: f64,
    /// Largest pair distance.
    pub dmax: f64,
    /// Largest distance of a body from the centre of mass.
    pub rmax: f64,
}

pub fn geometry(y: &Y, m: &Model) -> Geometry {
    let mt = m.total_mass();
    let (mut cx, mut cy) = (0.0, 0.0);
    for i in 0..NB {
        cx += m.m[i] * y[2 * i];
        cy += m.m[i] * y[2 * i + 1];
    }
    cx /= mt;
    cy /= mt;
    let mut g = Geometry { dmin: f64::INFINITY, dmax: 0.0, rmax: 0.0 };
    for i in 0..NB {
        g.rmax = g.rmax.max((y[2 * i] - cx).hypot(y[2 * i + 1] - cy));
        for j in i + 1..NB {
            let d = (y[2 * j] - y[2 * i]).hypot(y[2 * j + 1] - y[2 * i + 1]);
            g.dmin = g.dmin.min(d);
            g.dmax = g.dmax.max(d);
        }
    }
    g
}

/// Kepler eccentricity from orbital energy `e`, angular momentum `h`, reduced mass `mu`, `k = G m1 m2`.
#[inline]
fn eccentricity(e: f64, h: f64, mu: f64, k: f64) -> f64 {
    (1.0 + 2.0 * e * h * h / (mu * k * k)).max(0.0).sqrt()
}

/// Event flags for a state (0 = all fine).
pub fn flags(y: &Y, m: &Model, l: &Limits) -> u32 {
    let geo = geometry(y, m);
    let mut f = 0;
    if geo.dmin < l.dmin {
        f |= CLOSE;
    }
    if geo.rmax > l.rfar {
        f |= FAR;
    }
    if l.amin <= 0.0 && !l.predict && l.hier <= 0.0 {
        return f;
    }
    let mt = m.total_mass();
    for k in 0..NB {
        let (i, j) = ((k + 1) % 3, (k + 2) % 3);
        let (dx, dy) = (y[2 * j] - y[2 * i], y[2 * j + 1] - y[2 * i + 1]);
        let r = dx.hypot(dy);
        let (dvx, dvy) = (y[6 + 2 * j] - y[6 + 2 * i], y[7 + 2 * j] - y[7 + 2 * i]);
        let mp = m.m[i] + m.m[j];
        let mu = m.m[i] * m.m[j] / mp;
        let kij = m.g * m.m[i] * m.m[j];
        let e_ij = 0.5 * mu * (dvx * dvx + dvy * dvy) - kij / r;
        // body k relative to the centre of mass of pair (i, j)
        let cx = (m.m[i] * y[2 * i] + m.m[j] * y[2 * j]) / mp;
        let cy = (m.m[i] * y[2 * i + 1] + m.m[j] * y[2 * j + 1]) / mp;
        let vx = (m.m[i] * y[6 + 2 * i] + m.m[j] * y[6 + 2 * j]) / mp;
        let vy = (m.m[i] * y[7 + 2 * i] + m.m[j] * y[7 + 2 * j]) / mp;
        let (rx, ry) = (y[2 * k] - cx, y[2 * k + 1] - cy);
        let (wx, wy) = (y[6 + 2 * k] - vx, y[7 + 2 * k] - vy);
        let big_r = rx.hypot(ry);
        let mu_o = m.m[k] * mp / mt;
        let k_o = m.g * m.m[k] * mp;
        let e_k = 0.5 * mu_o * (wx * wx + wy * wy) - k_o / big_r;

        if e_ij < 0.0 {
            let a_ij = -kij / (2.0 * e_ij);
            if l.amin > 0.0 && a_ij < l.amin && big_r > 2.5 * r {
                f |= BINARY;
            }
            if l.predict && big_r > 2.0 * r {
                let h_ij = mu * (dx * dvy - dy * dvx);
                if a_ij * (1.0 - eccentricity(e_ij, h_ij, mu, kij)) < l.dmin {
                    f |= PERI;
                }
            }
            if l.hier > 0.0 && big_r > 1.5 * r && e_k < 0.0 {
                let a_o = -k_o / (2.0 * e_k);
                let h_o = mu_o * (rx * wy - ry * wx);
                if a_o * (1.0 - eccentricity(e_k, h_o, mu_o, k_o)) > l.hier * a_ij {
                    f |= HIER;
                }
            }
        }
        if l.predict && big_r > 1.5 * r {
            if e_k >= 0.0 {
                if big_r * mp / mt > 0.6 * l.rfar {
                    f |= FAR;
                }
            } else {
                let a_o = -k_o / (2.0 * e_k);
                let h_o = mu_o * (rx * wy - ry * wx);
                if a_o * (1.0 + eccentricity(e_k, h_o, mu_o, k_o)) * mp / mt > l.rfar {
                    f |= FAR;
                }
            }
        }
    }
    f
}
