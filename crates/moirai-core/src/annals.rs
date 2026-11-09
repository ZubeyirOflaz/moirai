//! The story of a scene's planets, told when the scene ends: every world, era and civilisation
//! with when it began and ended, and the surface temperature over the whole scene. It is
//! gathered from the planet's samples in order (see [`crate::planet`]), as they are shown.

use std::f64::consts::TAU;

use crate::planet::{Fate, HZ_RADIUS, PlanetSample};

/// A year, in time units: one orbit of a newly launched planet, which starts on a circular orbit
/// at `HZ_RADIUS` around a sun of mass 1 (G = 1). Every planet starts that way, so the first
/// planet's year is everyone's: about 19 years to a revolution of the suns.
pub fn year() -> f64 {
    TAU * HZ_RADIUS.powi(3).sqrt()
}

/// Temperatures are kept per slice of a quarter year: fine enough for any timeline drawn from
/// them, a few thousand slices for a whole scene.
const SLICES_PER_YEAR: f64 = 4.0;

/// From `start` to `end` (time units from the start of the scene).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub start: f64,
    pub end: f64,
}

impl Span {
    pub fn length(&self) -> f64 {
        self.end - self.start
    }
}

/// One planet's life, and how it ended (`None`: it was still there when the scene ended).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct World {
    pub span: Span,
    pub fate: Option<Fate>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Era {
    pub span: Span,
    pub stable: bool,
}

/// The surface temperature over one slice: its extremes and where it ended (K).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slice {
    pub lo: f32,
    pub hi: f32,
    pub last: f32,
}

/// How many of something there were, and the shortest, median and longest of their lengths.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Durations {
    pub count: usize,
    pub shortest: f64,
    pub median: f64,
    pub longest: f64,
}

impl Durations {
    pub fn of(spans: impl IntoIterator<Item = Span>) -> Option<Self> {
        let mut v: Vec<f64> = spans.into_iter().map(|s| s.length()).collect();
        v.sort_by(f64::total_cmp);
        let n = v.len();
        let median = match n {
            0 => return None,
            _ if n % 2 == 1 => v[n / 2],
            _ => 0.5 * (v[n / 2 - 1] + v[n / 2]),
        };
        Some(Self { count: n, shortest: v[0], median, longest: v[n - 1] })
    }
}

/// What is going on on the world being lived on.
#[derive(Clone, Copy, Debug)]
struct Open {
    run: u32,
    start: f64,
    era: u32,
    stable: bool,
    era_start: f64,
    /// The living civilisation and when it rose.
    civilisation: Option<(u32, f64)>,
}

#[derive(Clone, Debug, Default)]
pub struct Annals {
    pub worlds: Vec<World>,
    pub eras: Vec<Era>,
    pub civilisations: Vec<Span>,
    /// Slice `k` covers quarter year `k` from the start of the scene (`None`: no sample in it).
    pub slices: Vec<Option<Slice>>,
    open: Option<Open>,
    /// The time and run of the last sample taken (each sample is taken once, in order).
    last: Option<(f64, u32)>,
    finished: bool,
}

impl Annals {
    /// The annals of a whole scene that ended at `end`, from its samples.
    pub fn of<'a>(samples: impl IntoIterator<Item = &'a PlanetSample>, end: f64) -> Self {
        let mut annals = Self::default();
        for s in samples.into_iter().take_while(|s| s.t <= end) {
            annals.record(s);
        }
        annals.finish(end);
        annals
    }

    /// Whether `s` comes after everything recorded so far (two samples share a time only where
    /// one planet ends and the next begins).
    pub fn is_new(&self, s: &PlanetSample) -> bool {
        self.last.is_none_or(|(t, run)| s.t > t || (s.t == t && s.run > run))
    }

    /// Time units per temperature slice.
    pub fn slice_length() -> f64 {
        year() / SLICES_PER_YEAR
    }

    /// Take the next sample of the planet's path.
    pub fn record(&mut self, s: &PlanetSample) {
        if self.finished || !self.is_new(s) {
            return;
        }
        self.last = Some((s.t, s.run));
        let c = s.chronicle;
        if self.open.is_none_or(|o| o.run != s.run) {
            self.close(s.t, None);
            self.open = Some(Open {
                run: s.run,
                start: s.t,
                era: c.era,
                stable: c.stable,
                era_start: s.t,
                civilisation: c.alive.then_some((c.civilisation, s.t)),
            });
        }
        if let Some(o) = self.open.as_mut() {
            if c.era != o.era {
                self.eras.push(Era { span: Span { start: o.era_start, end: s.t }, stable: o.stable });
                (o.era, o.stable, o.era_start) = (c.era, c.stable, s.t);
            }
            match o.civilisation {
                Some((n, start)) if !c.alive || n != c.civilisation => {
                    self.civilisations.push(Span { start, end: s.t });
                    o.civilisation = c.alive.then_some((c.civilisation, s.t));
                }
                None if c.alive => o.civilisation = Some((c.civilisation, s.t)),
                _ => {}
            }
        }
        self.temperature(s.t, c.temp);
        if s.fate.is_some() {
            self.close(s.t, s.fate);
        }
    }

    /// The scene ended at `t`: whatever is still going on ends there.
    pub fn finish(&mut self, t: f64) {
        self.close(t, None);
        self.finished = true;
    }

    fn close(&mut self, t: f64, fate: Option<Fate>) {
        if let Some(o) = self.open.take() {
            self.eras.push(Era { span: Span { start: o.era_start, end: t }, stable: o.stable });
            if let Some((_, start)) = o.civilisation {
                self.civilisations.push(Span { start, end: t });
            }
            self.worlds.push(World { span: Span { start: o.start, end: t }, fate });
        }
    }

    fn temperature(&mut self, t: f64, temp: f32) {
        let k = (t / Self::slice_length()).max(0.0) as usize;
        if self.slices.len() <= k {
            self.slices.resize(k + 1, None);
        }
        let slice = &mut self.slices[k];
        match slice {
            Some(s) => {
                s.lo = s.lo.min(temp);
                s.hi = s.hi.max(temp);
                s.last = temp;
            }
            None => *slice = Some(Slice { lo: temp, hi: temp, last: temp }),
        }
    }

    /// How many worlds met each fate: swallowed, ejected, scorched, and still there at the end.
    pub fn fates(&self) -> [usize; 4] {
        let mut n = [0; 4];
        for w in &self.worlds {
            n[match w.fate {
                Some(Fate::Swallowed) => 0,
                Some(Fate::Ejected) => 1,
                Some(Fate::Scorched) => 2,
                None => 3,
            }] += 1;
        }
        n
    }

    pub fn era_durations(&self, stable: bool) -> Option<Durations> {
        Durations::of(self.eras.iter().filter(|e| e.stable == stable).map(|e| e.span))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planet::Chronicle;

    fn sample(t: f64, run: u32, era: u32, stable: bool, civilisation: u32, alive: bool, fate: Option<Fate>) -> PlanetSample {
        let chronicle = Chronicle { temp: 200.0 + t as f32, stable, era, civilisation, alive };
        PlanetSample { t, p: [0.0; 2], v: [0.0; 2], run, fate, chronicle }
    }

    #[test]
    fn a_year_is_the_first_orbit() {
        // a circular orbit of radius r around mass 1 takes 2 pi r^1.5
        assert!((year() - 0.261_19).abs() < 1e-4, "{}", year());
        assert!((crate::REV / year() - 19.25).abs() < 0.01);
    }

    #[test]
    fn worlds_eras_and_civilisations_are_told_with_their_spans() {
        let samples = [
            // world 0: stable, then chaotic (its civilisation perishes), stable again (a new one
            // rises), swallowed at 10
            sample(0.0, 0, 1, true, 1, true, None),
            sample(2.0, 0, 2, false, 1, true, None),
            sample(3.0, 0, 2, false, 1, false, None),
            sample(6.0, 0, 3, true, 2, true, None),
            sample(10.0, 0, 3, true, 2, true, Some(Fate::Swallowed)),
            // world 1 starts the same moment and is still there at the end (14)
            sample(10.0, 1, 1, true, 1, true, None),
            sample(12.0, 1, 2, false, 1, true, None),
        ];
        let a = Annals::of(&samples, 14.0);
        let spans = |v: Vec<Span>| v.into_iter().map(|s| (s.start, s.end)).collect::<Vec<_>>();
        assert_eq!(spans(a.worlds.iter().map(|w| w.span).collect()), [(0.0, 10.0), (10.0, 14.0)]);
        assert_eq!(a.worlds.iter().map(|w| w.fate).collect::<Vec<_>>(), [Some(Fate::Swallowed), None]);
        assert_eq!(a.fates(), [1, 0, 0, 1]);
        assert_eq!(spans(a.eras.iter().map(|e| e.span).collect()), [(0.0, 2.0), (2.0, 6.0), (6.0, 10.0), (10.0, 12.0), (12.0, 14.0)]);
        assert_eq!(a.eras.iter().map(|e| e.stable).collect::<Vec<_>>(), [true, false, true, true, false]);
        assert_eq!(spans(a.civilisations.clone()), [(0.0, 3.0), (6.0, 10.0), (10.0, 14.0)]);
        let stable = a.era_durations(true).unwrap();
        assert_eq!((stable.count, stable.shortest, stable.median, stable.longest), (3, 2.0, 2.0, 4.0));
        let chaotic = a.era_durations(false).unwrap();
        assert_eq!((chaotic.count, chaotic.shortest, chaotic.median, chaotic.longest), (2, 2.0, 3.0, 4.0));
        // temperatures land in their quarter-year slices
        let k = (12.0 / Annals::slice_length()) as usize;
        assert_eq!(a.slices[k].map(|s| s.last), Some(212.0));
        assert!(a.slices.iter().flatten().count() >= 6);
    }

    #[test]
    fn samples_are_taken_once_in_order() {
        let mut a = Annals::default();
        let s0 = sample(1.0, 0, 1, true, 1, true, Some(Fate::Ejected));
        let s1 = sample(1.0, 1, 1, true, 1, true, None);
        a.record(&s0);
        a.record(&s0);
        assert!(!a.is_new(&s0) && a.is_new(&s1));
        a.record(&s1);
        a.record(&sample(0.5, 1, 1, true, 1, true, None));
        a.finish(3.0);
        a.record(&sample(4.0, 1, 2, false, 1, true, None));
        assert_eq!(a.worlds.len(), 2);
        assert_eq!(a.worlds[1].span, Span { start: 1.0, end: 3.0 });
        assert_eq!(a.eras.len(), 2);
    }
}
