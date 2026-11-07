//! The planet's readout, in the top-right corner: the era, the counts of eras, planets and
//! civilisations, and a graph of the surface temperature over the last half minute or so. Text
//! is set in Inter (SIL Open Font License, see `assets/Inter-LICENSE.txt`), rasterised once per
//! size into a small atlas; this module only lays out quads, the renderer draws them over the
//! finished frame.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use fontdue::{Font, FontSettings};
use moirai_core::planet::{CIV_COLD, CIV_HOT, Chronicle};

use crate::render::HistoryPoint;

const LIGHT: &[u8] = include_bytes!("../assets/Inter-Light.ttf");
const REGULAR: &[u8] = include_bytes!("../assets/Inter-Regular.ttf");

/// The graph's temperature range (K), on a log scale: colder only happens on the way out of the
/// system, hotter only in a sun.
const T_LOW: f32 = 10.0;
const T_HIGH: f32 = 10_000.0;

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

/// The three sizes of type: the era's name (light), text, and the graph's small print.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Size {
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
            Size::Heading => (1.45 * body).round(),
            Size::Body => body,
            Size::Small => (0.8 * body).round().max(9.0),
        }
    }

    fn font(&self, size: Size) -> &Font {
        if size == Size::Heading { &self.light } else { &self.regular }
    }

    /// Rasterise the printable ASCII range in every size and pack it into rows of a
    /// single-channel atlas.
    fn build_atlas(&mut self) {
        let mut bitmaps = Vec::new();
        for size in [Size::Heading, Size::Body, Size::Small] {
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
        let ticks_w = self.measure("10,000", Size::Small) + 0.6 * small;
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
        for (t, label) in [(10.0, 10), (100.0, 100), (1000.0, 1000), (10_000.0, 10_000)] {
            let y = at(0.0, t)[1].round();
            quad(out, [gx0, y - 0.5 * hair, gx1, y + 0.5 * hair], rgba(GRID, alpha), 1, [0.0; 4]);
            let label = grouped(label);
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

    #[test]
    fn temperatures_have_colours_and_places() {
        assert!(level(10.0) == 0.0 && level(10_000.0) == 1.0 && level(3.0) == 0.0);
        assert!(level(288.0) > level(100.0));
        let earth = temperature_color(288.0);
        assert!(earth[1] > earth[0], "a living green at Earth's warmth");
        let hot = temperature_color(1500.0);
        assert!(hot[0] > hot[2], "glowing red-orange when rock glows");
        assert_eq!(grouped(10_000), "10,000");
        assert_eq!(grouped(288), "288");
    }
}
