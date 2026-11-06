//! Starting points: the catalogue of stable choreographies and random "democratic" triangles.

use std::sync::OnceLock;

use crate::events::geometry;
use crate::physics::{Model, NB, Y, normalise, to_com};
use crate::rng::Rng;

const CSV: &str = include_str!("../../../data/choreographies.csv");

/// A linearly stable, collision-free periodic orbit (Hristov, Hristova & Tanikawa 2025).
#[derive(Clone, Debug, PartialEq)]
pub struct Orbit {
    pub id: u32,
    pub vx: f64,
    pub vy: f64,
    /// Period in the catalogue's native units.
    pub period: f64,
    pub tstar: f64,
    /// Closest pair approach and largest distance from the centre of mass, in units of a.
    pub closest: f64,
    pub extent: f64,
    /// Period in figure-eight periods.
    pub revs: f64,
    /// Largest relative velocity kick that kept the orbit intact in testing.
    pub kick: f64,
    pub figure_eight_family: bool,
}

impl Orbit {
    /// Well-known names from Šuvakov & Dmitrašinović (2013).
    pub fn name(&self) -> Option<&'static str> {
        match self.id {
            1 => Some("Figure-eight"),
            2 => Some("Moth I"),
            3 => Some("Butterfly III"),
            4 => Some("Moth II"),
            _ => None,
        }
    }

    /// Initial state scaled to E = -1.5 and the period in those units.
    pub fn initial_state(&self, m: &Model) -> (Y, f64) {
        let (vx, vy) = (self.vx, self.vy);
        let mut y = [-1.0, 0.0, 0.0, 0.0, 1.0, 0.0, vx, vy, -2.0 * vx, -2.0 * vy, vx, vy];
        let a = normalise(&mut y, m, -1.5);
        (y, self.period * a.powf(1.5))
    }
}

/// The 385 collision-free stable orbits shipped in `data/choreographies.csv`.
pub fn catalog() -> &'static [Orbit] {
    static CAT: OnceLock<Vec<Orbit>> = OnceLock::new();
    CAT.get_or_init(|| {
        CSV.lines()
            .filter(|l| !l.starts_with('#') && !l.starts_with("id,") && !l.trim().is_empty())
            .map(|l| {
                let f: Vec<&str> = l.split(',').collect();
                let num = |i: usize| f[i].trim().parse::<f64>().expect("catalogue number");
                Orbit {
                    id: f[0].parse().expect("catalogue id"),
                    vx: num(1),
                    vy: num(2),
                    period: num(3),
                    tstar: num(4),
                    closest: num(5),
                    extent: num(6),
                    revs: num(7),
                    kick: num(8),
                    figure_eight_family: f[9].trim() == "figure-eight",
                }
            })
            .collect()
    })
}

pub fn orbit_by_id(id: u32) -> Option<&'static Orbit> {
    catalog().iter().find(|o| o.id == id)
}

/// A random bound triangle that starts "democratic": compact, no tight pair, no loner.
pub fn random_democratic(rng: &mut Rng, m: &Model) -> Y {
    loop {
        let q = 0.1 + 0.4 * rng.uniform();
        let mut y = [0.0; 12];
        for i in 0..NB {
            let (px, py) = loop {
                let (px, py) = (2.0 * rng.uniform() - 1.0, 2.0 * rng.uniform() - 1.0);
                if px * px + py * py <= 1.0 {
                    break (px, py);
                }
            };
            y[2 * i] = px;
            y[2 * i + 1] = py;
            y[6 + 2 * i] = rng.normal();
            y[7 + 2 * i] = rng.normal();
        }
        to_com(&mut y, m);
        let n = m.newtonian();
        let (t, u) = (n.kinetic(&y), n.potential(&y));
        let s = (q * u.abs() / t).sqrt();
        for v in &mut y[6..] {
            *v *= s;
        }
        normalise(&mut y, m, -1.5);
        let g = geometry(&y, m);
        if g.rmax < 1.6 && g.dmin > 0.25 && g.dmax / g.dmin < 3.0 {
            return y;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_loads() {
        let cat = catalog();
        assert_eq!(cat.len(), 385);
        assert_eq!(cat[0].name(), Some("Figure-eight"));
        assert_eq!(cat.iter().filter(|o| o.figure_eight_family).count(), 346);
        let (_, period) = cat[0].initial_state(&Model::default());
        assert!((period - crate::physics::REV).abs() < 1e-3, "figure-eight period {period}");
    }
}
