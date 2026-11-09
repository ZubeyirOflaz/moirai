//! The planet's readout, in the top-right corner: the era, the counts of eras, planets and
//! civilisations, and a graph of the surface temperature over the last half minute or so; and
//! the story told when a scene ends, of all its worlds, civilisations and eras. Text is set in
//! Inter (SIL Open Font License, see `assets/Inter-LICENSE.txt`), rasterised once per
//! size into a small atlas; this module only lays out quads, the renderer draws them over the
//! finished frame.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use fontdue::{Font, FontSettings};
use moirai_core::annals::{self, Annals, Durations, Slice, Span};
use moirai_core::planet::{CIV_COLD, CIV_HOT, Chronicle};

use crate::render::{HistoryPoint, StoryView};

const LIGHT: &[u8] = include_bytes!("../assets/Inter-Light.ttf");
const REGULAR: &[u8] = include_bytes!("../assets/Inter-Regular.ttf");

/// The graphs' temperature range (K), on a log scale: a planet is nearly always out of the
/// picture before it gets colder, and lost to a sun soon after it gets hotter. Beyond it the
/// line runs along the edge.
const T_LOW: f32 = 100.0;
const T_HIGH: f32 = 1000.0;
/// Where the graphs have gridlines (K).
const GRID_TEMPS: [f32; 4] = [100.0, 200.0, 500.0, 1000.0];

// Colours (linear RGB): quiet inks for text, luminous accents that sit well in the night sky.
const INK: [f32; 3] = [0.82, 0.85, 0.92];
const INK_2: [f32; 3] = [0.50, 0.54, 0.62];
const MUTED: [f32; 3] = [0.28, 0.31, 0.38];
const GRID: [f32; 3] = [0.045, 0.048, 0.062];
const SKY: [f32; 3] = [0.004, 0.004, 0.008];
const STABLE: [f32; 3] = [0.30, 0.85, 0.70];
const CHAOTIC: [f32; 3] = [1.0, 0.55, 0.22];

/// One quad of the readout, in pixels. `kind`: 0 glyph (alpha from the atlas), 1 solid
/// rectangle, 2 round dot, 3 line from `rect.xy` to `rect.zw`, `uv.x` half as wide. Colours are
/// linear RGB with straight alpha.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct HudQuad {
    pub rect: [f32; 4],
    pub uv: [f32; 4],
    pub color: [f32; 4],
    pub kind: u32,
    pub _pad: [u32; 3],
}

#[derive(Clone, Copy)]
struct Glyph {
    uv: [f32; 4],
    size: [f32; 2],
    /// From the pen position on the baseline to the bitmap's top-left corner.
    offset: [f32; 2],
    advance: f32,
}

/// The sizes of type: the story's title and the era's name (light), text, and small print.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Size {
    Display,
    Heading,
    Body,
    Small,
}

pub struct Hud {
    light: Font,
    regular: Font,
    /// Pixel size of body text the atlas was made for.
    body_px: u32,
    glyphs: HashMap<(char, Size), Glyph>,
    pub atlas_width: u32,
    pub atlas_height: u32,
    pub atlas: Vec<u8>,
}

/// The temperature as a colour (linear RGB): dim blue in deep cold, icy blue, a living teal at
/// Earth's warmth, yellow and amber in the heat, red-orange where rock glows, white-hot beyond.
pub fn temperature_color(t: f32) -> [f32; 3] {
    const STOPS: [(f32, [f32; 3]); 9] = [
        (3.0, [0.35, 0.45, 0.70]),
        (100.0, [0.55, 0.72, 1.00]),
        (200.0, [0.50, 0.85, 1.00]),
        (288.0, [0.40, 1.00, 0.75]),
        (373.0, [0.85, 0.95, 0.45]),
        (600.0, [1.00, 0.70, 0.30]),
        (1500.0, [1.00, 0.40, 0.15]),
        (4000.0, [1.00, 0.85, 0.70]),
        (10000.0, [0.90, 0.92, 1.00]),
    ];
    let x = t.clamp(STOPS[0].0, STOPS[8].0).ln();
    let i = STOPS.iter().position(|s| s.0.ln() >= x).unwrap_or(8).max(1);
    let (a, b) = (STOPS[i - 1], STOPS[i]);
    let f = ((x - a.0.ln()) / (b.0.ln() - a.0.ln())).clamp(0.0, 1.0);
    std::array::from_fn(|k| a.1[k] + (b.1[k] - a.1[k]) * f)
}

/// How high a temperature sits in the graph, 0 (bottom) to 1 (top).
fn level(t: f32) -> f32 {
    ((t.max(T_LOW).ln() - T_LOW.ln()) / (T_HIGH.ln() - T_LOW.ln())).clamp(0.0, 1.0)
}

fn rgba(c: [f32; 3], a: f32) -> [f32; 4] {
    [c[0], c[1], c[2], a]
}

/// "1,000" style thousands separators.
fn grouped(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// "1 world", "3 worlds".
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Items in prose: "a", "a and b", "a, b and c".
fn listed(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A length in years for the story's table: tenths up to ten, whole years beyond.
fn in_years(y: f64) -> String {
    if y < 0.05 {
        "<0.1".into()
    } else if y < 9.95 {
        format!("{y:.1}")
    } else {
        grouped(y.round() as u32)
    }
}

/// What the story calls a scene: a choreography by its name, a free dance "This dance".
fn scene_name(title: &str) -> &str {
    if title.starts_with("Dance") { "This dance" } else { title.split(" \u{b7} ").next().unwrap_or(title) }
}

/// How the worlds ended, in a sentence ("5 worlds fell into a sun, 13 were flung into space and
/// the last one survived.").
fn worlds_sentence(an: &Annals, years: f32) -> String {
    let [swallowed, ejected, scorched, left] = an.fates();
    if an.worlds.len() == 1 && left == 1 {
        return format!("Its one world lasted all {} years.", grouped(years.round() as u32));
    }
    let mut parts = Vec::new();
    for (n, one, many) in [
        (swallowed, "fell into a sun", "fell into a sun"),
        (ejected, "was flung into space", "were flung into space"),
        (scorched, "was scorched", "were scorched"),
    ] {
        if n > 0 {
            let noun = match (parts.is_empty(), n) {
                (true, 1) => " world",
                (true, _) => " worlds",
                _ => "",
            };
            parts.push(format!("{n}{noun} {}", if n == 1 { one } else { many }));
        }
    }
    if left > 0 {
        parts.push("the last one survived".into());
    }
    format!("{}.", listed(&parts))
}

/// How the civilisations ended: in the heat or the cold of their world, with their world, or not
/// at all.
fn civilisations_sentence(an: &Annals) -> String {
    let (mut perished, mut lost, mut alive) = (0, 0, 0);
    for c in &an.civilisations {
        // the world it lived on: the last one to begin before it rose
        let Some(w) = an.worlds.iter().rev().find(|w| w.span.start <= c.start) else { continue };
        if c.end < w.span.end - 1e-9 {
            perished += 1;
        } else if w.fate.is_some() {
            lost += 1;
        } else {
            alive += 1;
        }
    }
    let mut parts = Vec::new();
    for (n, one, many) in [
        (perished, "perished in the heat or the cold", "perished in the heat or the cold"),
        (lost, "was lost with its world", "were lost with their world"),
        (alive, "lives on", "live on"),
    ] {
        if n > 0 {
            let noun = match (parts.is_empty(), n) {
                (true, 1) => " civilisation",
                (true, _) => " civilisations",
                _ => "",
            };
            parts.push(format!("{n}{noun} {}", if n == 1 { one } else { many }));
        }
    }
    format!("{}.", listed(&parts))
}

/// The temperature over `slices`, merged.
fn merged(slices: &[Option<Slice>]) -> Option<Slice> {
    slices.iter().flatten().fold(None, |acc: Option<Slice>, s| {
        Some(match acc {
            None => *s,
            Some(a) => Slice { lo: a.lo.min(s.lo), hi: a.hi.max(s.hi), last: s.last },
        })
    })
}

impl Hud {
    /// Body text size for a frame whose shorter side is `m` px.
    pub fn body_px(m: f32) -> u32 {
        (0.016 * m).round().clamp(11.0, 38.0) as u32
    }

    pub fn new(m: f32) -> Self {
        let font = |bytes| Font::from_bytes(bytes, FontSettings::default()).expect("the embedded font loads");
        let mut hud = Self {
            light: font(LIGHT),
            regular: font(REGULAR),
            body_px: 0,
            glyphs: HashMap::new(),
            atlas_width: 1,
            atlas_height: 1,
            atlas: vec![0],
        };
        hud.prepare(m);
        hud
    }

    /// Make sure the atlas suits a frame whose shorter side is `m` px; whether it was remade.
    pub fn prepare(&mut self, m: f32) -> bool {
        let px = Self::body_px(m);
        if px == self.body_px {
            return false;
        }
        self.body_px = px;
        self.build_atlas();
        true
    }

    fn px(&self, size: Size) -> f32 {
        let body = self.body_px as f32;
        match size {
            Size::Display => (2.3 * body).round(),
            Size::Heading => (1.45 * body).round(),
            Size::Body => body,
            Size::Small => (0.8 * body).round().max(9.0),
        }
    }

    fn font(&self, size: Size) -> &Font {
        if matches!(size, Size::Display | Size::Heading) { &self.light } else { &self.regular }
    }

    /// Rasterise the printable ASCII range in every size and pack it into rows of a
    /// single-channel atlas.
    fn build_atlas(&mut self) {
        let mut bitmaps = Vec::new();
        for size in [Size::Display, Size::Heading, Size::Body, Size::Small] {
            let px = self.px(size);
            for ch in (32u8..=126).map(char::from) {
                let (m, bitmap) = self.font(size).rasterize(ch, px);
                bitmaps.push((ch, size, m, bitmap));
            }
        }
        let width = 1024usize;
        let (mut x, mut y, mut row) = (1usize, 1usize, 0usize);
        let mut places = Vec::with_capacity(bitmaps.len());
        for (_, _, m, _) in &bitmaps {
            if x + m.width + 1 > width {
                (x, y, row) = (1, y + row + 1, 0);
            }
            places.push((x, y));
            x += m.width + 1;
            row = row.max(m.height);
        }
        let height = (y + row + 1).next_multiple_of(4);
        let mut atlas = vec![0u8; width * height];
        self.glyphs.clear();
        for ((ch, size, m, bitmap), (gx, gy)) in bitmaps.into_iter().zip(places) {
            for r in 0..m.height {
                atlas[(gy + r) * width + gx..(gy + r) * width + gx + m.width]
                    .copy_from_slice(&bitmap[r * m.width..(r + 1) * m.width]);
            }
            let (w, h) = (width as f32, height as f32);
            self.glyphs.insert(
                (ch, size),
                Glyph {
                    uv: [gx as f32 / w, gy as f32 / h, (gx + m.width) as f32 / w, (gy + m.height) as f32 / h],
                    size: [m.width as f32, m.height as f32],
                    offset: [m.xmin as f32, -(m.ymin as f32 + m.height as f32)],
                    advance: m.advance_width,
                },
            );
        }
        self.atlas_width = width as u32;
        self.atlas_height = height as u32;
        self.atlas = atlas;
    }

    /// How wide `s` is set in `size`.
    fn measure(&self, s: &str, size: Size) -> f32 {
        let px = self.px(size);
        let mut w = 0.0;
        let mut prev = None;
        for ch in s.chars() {
            if let Some(p) = prev {
                w += self.font(size).horizontal_kern(p, ch, px).unwrap_or(0.0);
            }
            w += self.glyphs.get(&(ch, size)).map_or(0.0, |g| g.advance);
            prev = Some(ch);
        }
        w
    }

    /// Lay out `s` with its pen starting at `at` (x, baseline).
    fn text(&self, out: &mut Vec<HudQuad>, s: &str, at: [f32; 2], size: Size, color: [f32; 4]) {
        let px = self.px(size);
        let baseline = at[1].round();
        let mut pen = at[0];
        let mut prev = None;
        for ch in s.chars() {
            if let Some(p) = prev {
                pen += self.font(size).horizontal_kern(p, ch, px).unwrap_or(0.0);
            }
            if let Some(g) = self.glyphs.get(&(ch, size)) {
                if g.size[0] > 0.0 {
                    let (x0, y0) = ((pen + g.offset[0]).round(), baseline + g.offset[1]);
                    out.push(HudQuad { rect: [x0, y0, x0 + g.size[0], y0 + g.size[1]], uv: g.uv, color, kind: 0, _pad: [0; 3] });
                }
                pen += g.advance;
            }
            prev = Some(ch);
        }
    }

    /// The readout for a frame of `width` x `height` px (shorter side `m`), faded by `alpha`.
    #[allow(clippy::too_many_arguments)]
    pub fn layout(
        &self,
        out: &mut Vec<HudQuad>,
        width: f32,
        m: f32,
        planet: u32,
        c: &Chronicle,
        history: &[HistoryPoint],
        history_seconds: f32,
        alpha: f32,
    ) {
        out.clear();
        let quad = |out: &mut Vec<HudQuad>, rect: [f32; 4], color: [f32; 4], kind: u32, uv: [f32; 4]| {
            out.push(HudQuad { rect, uv, color, kind, _pad: [0; 3] });
        };
        let (body, small, heading) = (self.px(Size::Body), self.px(Size::Small), self.px(Size::Heading));
        let margin = (0.035 * m).round();
        let panel = (0.30 * m).round();
        let (x1, x0) = (width - margin, width - margin - panel);

        // the era, with a swatch of its colour, and where the planet's story stands
        let era_color = if c.stable { STABLE } else { CHAOTIC };
        let y = margin + (1.05 * heading).round();
        let dot = (0.24 * body).max(2.5);
        let (dx, dy) = (x0 + dot, y - 0.36 * heading);
        quad(out, [dx - dot, dy - dot, dx + dot, dy + dot], rgba(era_color, alpha), 2, [0.0; 4]);
        let era = if c.stable { "Stable era" } else { "Chaotic era" };
        self.text(out, era, [x0 + 2.0 * dot + 0.45 * body, y], Size::Heading, rgba(INK, 0.95 * alpha));
        let y = y + (1.5 * body).round();
        self.text(out, &format!("Era {} of planet {planet}", c.era), [x0, y], Size::Body, rgba(INK_2, alpha));
        let y = y + (1.3 * body).round();
        let civilisation = if c.alive {
            format!("Civilisation {} is alive", c.civilisation)
        } else {
            format!("Civilisation {} has perished", c.civilisation)
        };
        self.text(out, &civilisation, [x0, y], Size::Body, rgba(INK_2, alpha));

        // the temperature graph: time to the right, temperature up on a log scale
        let top = y + (1.3 * body).round();
        let height = (0.115 * m).round();
        // tick labels in a column on the left, the reading in one on the right
        let ticks_w = self.measure("1,000", Size::Small) + 0.6 * small;
        let label_w = self.measure("1,000 K", Size::Body) + 0.7 * body;
        let (gx0, gx1) = (x0 + ticks_w, x1 - label_w);
        let at = |x: f32, t: f32| [gx0 + x * (gx1 - gx0), top + height * (1.0 - level(t))];
        let hair = (0.0011 * m).max(1.0);

        // eras along the time axis, as faint washes
        let mut i = 0;
        while i + 1 < history.len() {
            let stable = history[i].stable;
            let mut j = i + 1;
            while j + 1 < history.len() && history[j].stable == stable {
                j += 1;
            }
            let (a, b) = (at(history[i].x, T_LOW)[0], at(history[j].x, T_LOW)[0]);
            quad(out, [a, top, b, top + height], rgba(if stable { STABLE } else { CHAOTIC }, 0.07 * alpha), 1, [0.0; 4]);
            i = j;
        }
        // where a civilisation can live, labelled just above (the line is rarely that hot), and
        // gridlines at every power of ten, labelled in the column on the left
        let (band_top, band_bottom) = (at(0.0, CIV_HOT as f32)[1], at(0.0, CIV_COLD as f32)[1]);
        quad(out, [gx0, band_top, gx1, band_bottom], rgba([0.5, 0.55, 0.65], 0.09 * alpha), 1, [0.0; 4]);
        self.text(out, "habitable", [gx0 + 0.35 * small, band_top - 0.3 * small], Size::Small, rgba(MUTED, alpha));
        for t in GRID_TEMPS {
            let y = at(0.0, t)[1].round();
            quad(out, [gx0, y - 0.5 * hair, gx1, y + 0.5 * hair], rgba(GRID, alpha), 1, [0.0; 4]);
            let label = grouped(t as u32);
            let x = gx0 - 0.4 * small - self.measure(&label, Size::Small);
            self.text(out, &label, [x, y + 0.36 * small], Size::Small, rgba(MUTED, alpha));
        }
        // a hairline where one planet was lost and the next began
        for w in history.windows(2).filter(|w| w[1].run != w[0].run) {
            let x = at(w[1].x, T_LOW)[0].round();
            quad(out, [x - 0.5 * hair, top, x + 0.5 * hair, top + height], rgba(MUTED, 0.8 * alpha), 1, [0.0; 4]);
        }
        // the line, coloured by temperature; earlier planets dimmed
        let line = (0.0011 * m).max(0.75);
        let current = history.last().map_or(0, |h| h.run);
        for w in history.windows(2).filter(|w| w[1].run == w[0].run) {
            let (a, b) = (at(w[0].x, w[0].temp), at(w[1].x, w[1].temp));
            let col = temperature_color(0.5 * (w[0].temp + w[1].temp));
            let dim = if w[0].run == current { 1.0 } else { 0.4 };
            quad(out, [a[0], a[1], b[0], b[1]], [col[0] * dim, col[1] * dim, col[2] * dim, alpha], 3, [line, 0.0, 0.0, 0.0]);
        }
        // now: a dot in the planet's colour, ringed with the sky so it stays clear, and the reading
        let now = at(1.0, c.temp);
        let r = (0.0045 * m).max(3.5);
        let ring = r + 2.0 * hair;
        quad(out, [now[0] - ring, now[1] - ring, now[0] + ring, now[1] + ring], rgba(SKY, alpha), 2, [0.0; 4]);
        quad(out, [now[0] - r, now[1] - r, now[0] + r, now[1] + r], rgba(temperature_color(c.temp), alpha), 2, [0.0; 4]);
        let reading = format!("{} K", grouped(c.temp.round() as u32));
        let base = (now[1] + 0.36 * body).clamp(top + body, top + height);
        self.text(out, &reading, [gx1 + 0.6 * body, base], Size::Body, rgba(INK, 0.95 * alpha));
        let caption = format!("Surface temperature (K), last {:.0} s", history_seconds);
        self.text(out, &caption, [x0, top + height + (1.9 * small).round()], Size::Small, rgba(MUTED, alpha));
    }
}

impl Hud {
    /// Greedy line breaks so that `s` set in `size` fits `width`.
    fn wrap(&self, s: &str, size: Size, width: f32) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for word in s.split(' ') {
            match lines.last_mut() {
                Some(line) if self.measure(&format!("{line} {word}"), size) <= width => {
                    line.push(' ');
                    line.push_str(word);
                }
                _ => lines.push(word.to_string()),
            }
        }
        lines
    }

    /// The story of a scene's planets, centred in a frame of `width` x `height` px: how many
    /// years, worlds, civilisations and eras it held and how they ended, a timeline of them all
    /// with the surface temperature, and how long they lasted.
    pub fn story(&self, out: &mut Vec<HudQuad>, width: f32, height: f32, v: &StoryView) {
        out.clear();
        let quad = |out: &mut Vec<HudQuad>, rect: [f32; 4], color: [f32; 4], kind: u32, half: f32| {
            out.push(HudQuad { rect, uv: [half, 0.0, 0.0, 0.0], color, kind, _pad: [0; 3] });
        };
        let (an, a) = (v.annals, v.alpha);
        let m = width.min(height);
        let (display, body, small) = (self.px(Size::Display), self.px(Size::Body), self.px(Size::Small));
        let hair = (0.0011 * m).max(1.0);
        let panel = (0.82 * width).min(1.2 * m).round();
        let (x0, x1) = (((width - panel) / 2.0).round(), ((width + panel) / 2.0).round());
        let year = annals::year();
        let end = an.worlds.last().map_or(year, |w| w.span.end).max(1e-6);

        // laid out from y = 0, then moved to the middle of the frame
        let mut y = display;
        let title = format!("{} years under three suns", grouped(v.years.round() as u32));
        self.text(out, &title, [x0, y], Size::Display, rgba(INK, 0.95 * a));
        y += 0.5 * display + 0.9 * body;
        let eras = an.eras.len();
        let saw = format!(
            "{} saw {}, {} and {}.",
            scene_name(v.title),
            count(an.worlds.len(), "world", "worlds"),
            count(an.civilisations.len(), "civilisation", "civilisations"),
            count(eras, "era", "eras")
        );
        for (k, sentence) in [saw, worlds_sentence(an, v.years), civilisations_sentence(an)].iter().enumerate() {
            for line in self.wrap(sentence, Size::Body, panel) {
                y += (1.5 * body).round();
                self.text(out, &line, [x0, y], Size::Body, rgba(if k == 0 { INK } else { INK_2 }, a));
            }
        }

        // the timeline: years to the right, temperature up on a log scale, eras as washes,
        // the worlds between hairlines, the civilisations as bars below
        let top = (y + 2.4 * body).round();
        let graph = (0.17 * m).round();
        let ticks_w = self.measure("1,000", Size::Small) + 0.6 * small;
        let (gx0, gx1) = (x0 + ticks_w, x1);
        let drawn = v.drawn as f64 * end;
        let x_of = |t: f64| gx0 + (t / end) as f32 * (gx1 - gx0);
        let y_of = |t: f32| top + graph * (1.0 - level(t));
        let shown = |s: &Span| (s.start < drawn).then(|| (s.start, s.end.min(drawn)));
        for e in &an.eras {
            if let Some((t0, t1)) = shown(&e.span) {
                let c = if e.stable { STABLE } else { CHAOTIC };
                quad(out, [x_of(t0), top, x_of(t1), top + graph], rgba(c, 0.12 * a), 1, 0.0);
            }
        }
        let (band_top, band_bottom) = (y_of(CIV_HOT as f32), y_of(CIV_COLD as f32));
        quad(out, [gx0, band_top, gx1, band_bottom], rgba([0.5, 0.55, 0.65], 0.09 * a), 1, 0.0);
        self.text(out, "habitable", [gx0 + 0.35 * small, band_top - 0.3 * small], Size::Small, rgba(MUTED, a));
        for t in GRID_TEMPS {
            let gy = y_of(t).round();
            quad(out, [gx0, gy - 0.5 * hair, gx1, gy + 0.5 * hair], rgba(GRID, a), 1, 0.0);
            let label = grouped(t as u32);
            let lx = gx0 - 0.4 * small - self.measure(&label, Size::Small);
            self.text(out, &label, [lx, gy + 0.36 * small], Size::Small, rgba(MUTED, a));
        }
        // years: a gridline and a label every so many, at most six
        let years = end / year;
        let step = [10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0].into_iter().find(|s| years / s <= 6.0).unwrap_or(2000.0);
        let civ_y = top + graph + (0.9 * small).round();
        let label_y = civ_y + (1.7 * small).round();
        let ticks = (years / step).floor() as usize;
        for k in 0..=ticks {
            let x = x_of(k as f64 * step * year).round();
            if k > 0 {
                quad(out, [x - 0.5 * hair, top, x + 0.5 * hair, top + graph], rgba(GRID, a), 1, 0.0);
            }
            let n = grouped((k as f64 * step) as u32);
            let label = if k == ticks && k > 0 { format!("{n} years") } else { n };
            let lx = if k == 0 { x } else { x - 0.5 * self.measure(&label, Size::Small) };
            self.text(out, &label, [lx, label_y], Size::Small, rgba(MUTED, a));
        }
        // the worlds: a hairline where one ended and the next began, numbered where there is room
        for (i, w) in an.worlds.iter().enumerate() {
            let Some((t0, _)) = shown(&w.span) else { continue };
            let xa = x_of(t0).round();
            if i > 0 {
                quad(out, [xa - 0.5 * hair, top, xa + 0.5 * hair, top + graph], rgba(MUTED, 0.8 * a), 1, 0.0);
            }
            let label = (i + 1).to_string();
            let (lw, xb) = (self.measure(&label, Size::Small), x_of(w.span.end));
            if w.span.end <= drawn && xb - xa >= lw + 0.8 * small {
                let lx = 0.5 * (xa + xb - lw);
                self.text(out, &label, [lx, top - 0.45 * small], Size::Small, rgba(MUTED, a));
            }
        }
        // the temperature, a column at a time (its coldest to hottest, joined to the column
        // before within a world), coloured by how hot it is
        let col = (0.0016 * m).round().max(1.0);
        let cols = ((gx1 - gx0) / col).floor().max(1.0) as usize;
        let slice = Annals::slice_length();
        let mut prev: Option<(usize, f32)> = None;
        for i in 0..cols {
            let (ta, tb) = (end * i as f64 / cols as f64, (end * (i + 1) as f64 / cols as f64).min(drawn));
            if ta >= drawn {
                break;
            }
            let (ka, kb) = ((ta / slice) as usize, (tb / slice) as usize);
            let Some(c) = merged(an.slices.get(ka..=kb.max(ka).min(an.slices.len().saturating_sub(1))).unwrap_or(&[]))
            else {
                prev = None;
                continue;
            };
            let world = an.worlds.partition_point(|w| w.span.start <= tb).saturating_sub(1);
            let (mut lo, mut hi) = (c.lo, c.hi);
            if let Some((pw, last)) = prev
                && pw == world
            {
                (lo, hi) = (lo.min(last), hi.max(last));
            }
            let x = gx0 + (i as f32 + 0.5) * col;
            quad(out, [x, y_of(hi), x, y_of(lo)], rgba(temperature_color((lo * hi).sqrt()), a), 3, 0.5 * col + 0.35);
            prev = Some((world, c.last));
        }
        // the civilisations, from rise to fall
        let bar = (0.0042 * m).max(2.0);
        let inset = 0.5 * bar + 0.75;
        for c in &an.civilisations {
            if let Some((t0, t1)) = shown(c) {
                let (xa, xb) = (x_of(t0), x_of(t1));
                let (xa, xb) = if xb - xa > 2.0 * inset { (xa + inset, xb - inset) } else { (0.5 * (xa + xb), 0.5 * (xa + xb)) };
                quad(out, [xa, civ_y, xb, civ_y], rgba(INK, 0.75 * a), 3, 0.5 * bar);
            }
        }
        let caption_y = label_y + (1.55 * small).round();
        self.text(out, "Surface temperature (K) by year", [x0, caption_y], Size::Small, rgba(MUTED, a));

        // how long things lasted; the swatches double as the timeline's legend
        let rows: [(&str, Option<Durations>); 4] = [
            ("Worlds", Durations::of(an.worlds.iter().map(|w| w.span))),
            ("Civilisations", Durations::of(an.civilisations.iter().copied())),
            ("Stable eras", an.era_durations(true)),
            ("Chaotic eras", an.era_durations(false)),
        ];
        let swatch = (1.5 * body).round();
        let label_w = swatch + rows.iter().map(|r| self.measure(r.0, Size::Body)).fold(0.0, f32::max) + body;
        let col_w = self.measure("Shortest", Size::Small).max(self.measure("1,000", Size::Body)) + 1.6 * body;
        let right = |k: usize| x0 + label_w + (k + 1) as f32 * col_w;
        let mut y = caption_y + (2.6 * body).round();
        for (k, head) in ["Number", "Shortest", "Median", "Longest"].iter().enumerate() {
            self.text(out, head, [right(k) - self.measure(head, Size::Small), y], Size::Small, rgba(MUTED, a));
        }
        for (r, (label, d)) in rows.iter().enumerate() {
            y += (1.6 * body).round();
            let (sx, sy) = (x0 + 0.35 * body, y - 0.36 * body);
            match r {
                0 => quad(out, [sx - 0.5 * hair, sy - 0.5 * body, sx + 0.5 * hair, sy + 0.5 * body], rgba(MUTED, a), 1, 0.0),
                1 => quad(out, [sx - 0.15 * body, sy, sx + 0.55 * body, sy], rgba(INK, 0.75 * a), 3, 0.5 * bar),
                _ => {
                    let dot = (0.24 * body).max(2.5);
                    let c = if r == 2 { STABLE } else { CHAOTIC };
                    quad(out, [sx - dot, sy - dot, sx + dot, sy + dot], rgba(c, a), 2, 0.0);
                }
            }
            self.text(out, label, [x0 + swatch, y], Size::Body, rgba(INK_2, a));
            let cells = match d {
                Some(d) => [d.count.to_string(), in_years(d.shortest / year), in_years(d.median / year), in_years(d.longest / year)],
                None => ["0".into(), String::new(), String::new(), String::new()],
            };
            for (k, cell) in cells.iter().enumerate() {
                self.text(out, cell, [right(k) - self.measure(cell, Size::Body), y], Size::Body, rgba(INK, a));
            }
        }
        y += (2.1 * body).round();
        let note = format!("Lengths in years. A year is one orbit of a newborn planet, {:.1} s on screen.", v.seconds_per_year);
        self.text(out, &note, [x0, y], Size::Small, rgba(MUTED, a));

        // into the middle of the frame
        let dy = ((height - (y + 0.4 * small)) / 2.0).round().max(0.0);
        for q in out.iter_mut() {
            q.rect[1] += dy;
            q.rect[3] += dy;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(x: f32, temp: f32, run: u32) -> HistoryPoint {
        HistoryPoint { x, temp, stable: temp < 400.0, run }
    }

    #[test]
    fn readout_lays_out_inside_the_frame() {
        let hud = Hud::new(800.0);
        let c = Chronicle { temp: 288.0, stable: false, era: 12, civilisation: 3, alive: false };
        let history = [point(0.0, 120.0, 4), point(0.3, 2500.0, 4), point(0.31, 288.0, 5), point(0.7, 600.0, 5), point(1.0, 288.0, 5)];
        let mut quads = Vec::new();
        hud.layout(&mut quads, 1280.0, 800.0, 6, &c, &history, 36.0, 1.0);
        assert!(quads.len() > 60, "{} quads", quads.len());
        let inside = |q: &HudQuad| q.rect[0] >= 0.0 && q.rect[1] >= 0.0 && q.rect[2] <= 1280.0 && q.rect[3] <= 800.0;
        assert!(quads.iter().all(inside));
        // in the top-right corner
        assert!(quads.iter().all(|q| q.rect[0].min(q.rect[2]) > 640.0 && q.rect[1].max(q.rect[3]) < 400.0));
        // glyphs point into the atlas; lines join points of the same planet only
        assert!(quads.iter().filter(|q| q.kind == 0).all(|q| q.uv.iter().all(|&u| (0.0..=1.0).contains(&u))));
        assert_eq!(quads.iter().filter(|q| q.kind == 3).count(), 3);
    }

    /// A scene of 3 worlds: one swallowed after a chaotic spell (its civilisation perished in
    /// it), one flung away, one still there at the end.
    fn annals() -> Annals {
        use moirai_core::planet::{Fate, PlanetSample};
        let s = |t: f64, run: u32, era: u32, stable: bool, alive: bool, temp: f32, fate: Option<Fate>| PlanetSample {
            t,
            p: [0.0; 2],
            v: [0.0; 2],
            run,
            fate,
            chronicle: Chronicle { temp, stable, era, civilisation: 1, alive },
        };
        let mut samples = Vec::new();
        for k in 0..400 {
            let t = k as f64 * 0.05;
            let (era, stable, alive) = if t < 8.0 { (1, true, true) } else { (2, false, t < 9.0) };
            samples.push(s(t, 0, era, stable, alive, 288.0 + 60.0 * (t * 24.0).sin() as f32, None));
        }
        samples.push(s(20.0, 0, 2, false, false, 2500.0, Some(Fate::Swallowed)));
        samples.push(s(20.0, 1, 1, true, true, 288.0, None));
        samples.push(s(30.0, 1, 1, true, true, 40.0, Some(Fate::Ejected)));
        samples.push(s(30.0, 2, 1, true, true, 288.0, None));
        samples.push(s(36.0, 2, 1, true, true, 300.0, None));
        Annals::of(&samples, 40.0)
    }

    #[test]
    fn the_story_is_told_in_the_middle_of_the_frame() {
        let an = annals();
        assert_eq!(an.fates(), [1, 1, 0, 1]);
        let story = |drawn: f32| StoryView { annals: &an, title: "Moth I \u{b7} choreography #2", years: 153.0, seconds_per_year: 0.6, alpha: 1.0, drawn };
        assert_eq!(worlds_sentence(&an, 153.0), "1 world fell into a sun, 1 was flung into space and the last one survived.");
        assert_eq!(civilisations_sentence(&an), "1 civilisation perished in the heat or the cold, 1 was lost with its world and 1 lives on.");
        assert_eq!(scene_name("Moth I \u{b7} choreography #2"), "Moth I");
        assert_eq!((in_years(0.01), in_years(3.04), in_years(12.6), in_years(1234.0)), ("<0.1".into(), "3.0".into(), "13".into(), "1,234".into()));
        for (w, h) in [(1280.0f32, 800.0f32), (2880.0, 1800.0), (640.0, 1000.0)] {
            let hud = Hud::new(w.min(h));
            let mut quads = Vec::new();
            hud.story(&mut quads, w, h, &story(1.0));
            assert!(quads.len() > 300, "{} quads", quads.len());
            let inside = |q: &HudQuad| q.rect[0].min(q.rect[2]) >= 0.0 && q.rect[2].max(q.rect[0]) <= w && q.rect[1].min(q.rect[3]) >= 0.0 && q.rect[3].max(q.rect[1]) <= h;
            assert!(quads.iter().all(inside), "{w}x{h}");
            // centred: as much room above as below
            let (top, bottom) = quads.iter().fold((h, 0.0f32), |(t, b), q| (t.min(q.rect[1].min(q.rect[3])), b.max(q.rect[3].max(q.rect[1]))));
            assert!((top - (h - bottom)).abs() < 0.06 * h, "{w}x{h}: {top} above, {} below", h - bottom);
            // a quarter drawn (the samples cover the first half): about half the temperature
            let mut part = Vec::new();
            hud.story(&mut part, w, h, &story(0.25));
            let lines = |v: &[HudQuad]| v.iter().filter(|q| q.kind == 3).count();
            assert!(lines(&part) < lines(&quads) * 2 / 3, "{} of {} lines", lines(&part), lines(&quads));
        }
    }

    #[test]
    fn temperatures_have_colours_and_places() {
        assert!(level(100.0) == 0.0 && level(1000.0) == 1.0 && level(30.0) == 0.0 && level(5000.0) == 1.0);
        assert!(level(288.0) > level(180.0));
        let earth = temperature_color(288.0);
        assert!(earth[1] > earth[0], "a living green at Earth's warmth");
        let hot = temperature_color(1500.0);
        assert!(hot[0] > hot[2], "glowing red-orange when rock glows");
        assert_eq!(grouped(10_000), "10,000");
        assert_eq!(grouped(288), "288");
    }
}
