//! The planet's readout: the era, counters for eras, civilisations and planets, and the surface
//! temperature on a log scale, laid out in the bottom-left corner. Text is set in Inter (SIL
//! Open Font License, see `assets/Inter-LICENSE.txt`), rasterised once per size into a small
//! atlas; this module only lays out quads, the renderer draws them over the finished frame.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use fontdue::{Font, FontSettings};
use moirai_core::planet::{CIV_COLD, CIV_HOT, Chronicle};

const FONT: &[u8] = include_bytes!("../assets/Inter-Regular.ttf");

/// The gauge's temperature range (K), on a log scale.
const T_MIN: f32 = 3.0;
const T_MAX: f32 = 10_000.0;
/// Liquid water at one atmosphere (K), highlighted on the gauge.
const WATER: (f32, f32) = (273.0, 373.0);

/// One quad of the readout, in pixels. `kind`: 0 = glyph (alpha from the atlas), 1 = solid,
/// 2 = round dot. Colours are linear RGB with straight alpha.
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

/// Text sizes: the era's name, and everything else.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Size {
    Title,
    Body,
}

/// How a line of text is set: its size, colour and extra space between letters (px).
#[derive(Clone, Copy)]
struct Style {
    size: Size,
    color: [f32; 4],
    tracking: f32,
}

pub struct Hud {
    font: Font,
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

/// Where a temperature sits on the gauge, 0 to 1.
fn gauge(t: f32) -> f32 {
    ((t.max(T_MIN).ln() - T_MIN.ln()) / (T_MAX.ln() - T_MIN.ln())).clamp(0.0, 1.0)
}

impl Hud {
    /// Body text size for a frame whose shorter side is `m` px.
    pub fn body_px(m: f32) -> u32 {
        (0.017 * m).round().clamp(11.0, 40.0) as u32
    }

    pub fn new(m: f32) -> Self {
        let font = Font::from_bytes(FONT, FontSettings::default()).expect("the embedded font loads");
        let mut hud = Self { font, body_px: 0, glyphs: HashMap::new(), atlas_width: 1, atlas_height: 1, atlas: vec![0] };
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
        match size {
            Size::Title => (self.body_px as f32 * 1.3).round(),
            Size::Body => self.body_px as f32,
        }
    }

    /// Rasterise the printable ASCII range (and a few extras) in both sizes and pack them into
    /// rows of a single-channel atlas.
    fn build_atlas(&mut self) {
        let chars: Vec<char> = (32u8..=126).map(char::from).chain(['·', '°', '—']).collect();
        let mut bitmaps = Vec::new();
        for size in [Size::Title, Size::Body] {
            let px = self.px(size);
            for &ch in &chars {
                let (m, bitmap) = self.font.rasterize(ch, px);
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

    /// Lay out `s` with its pen starting at `at` (x, baseline); the pen position after it.
    fn text(&self, out: &mut Vec<HudQuad>, s: &str, at: [f32; 2], style: Style) -> f32 {
        let Style { size, color, tracking } = style;
        let px = self.px(size);
        let baseline = at[1].round();
        let mut pen = at[0];
        let mut prev = None;
        for ch in s.chars() {
            if let Some(p) = prev {
                pen += self.font.horizontal_kern(p, ch, px).unwrap_or(0.0);
            }
            if let Some(g) = self.glyphs.get(&(ch, size)) {
                if g.size[0] > 0.0 {
                    let (x0, y0) = ((pen + g.offset[0]).round(), baseline + g.offset[1]);
                    out.push(HudQuad {
                        rect: [x0, y0, x0 + g.size[0], y0 + g.size[1]],
                        uv: g.uv,
                        color,
                        kind: 0,
                        _pad: [0; 3],
                    });
                }
                pen += g.advance + tracking;
            }
            prev = Some(ch);
        }
        pen
    }

    /// The readout for a frame of `width` x `height` px, faded by `alpha`.
    pub fn layout(&self, out: &mut Vec<HudQuad>, height: f32, m: f32, planet: u32, c: &Chronicle, alpha: f32) {
        out.clear();
        let body = self.body_px as f32;
        let margin = (0.035 * m).round();
        let x0 = margin;
        let bar_y = (height - margin - 0.5 * body).round();
        let solid = |out: &mut Vec<HudQuad>, rect: [f32; 4], color: [f32; 4], kind: u32| {
            out.push(HudQuad { rect, uv: [0.0; 4], color, kind, _pad: [0; 3] });
        };
        let dim = [0.72, 0.76, 0.86, 0.72 * alpha];

        // the era, and the counters below it
        let (era, era_color) = if c.stable {
            ("STABLE ERA", [0.55, 0.95, 0.85, 0.92 * alpha])
        } else {
            ("CHAOTIC ERA", [1.0, 0.66, 0.36, 0.92 * alpha])
        };
        let counters_y = bar_y - 1.25 * body;
        self.text(out, era, [x0, counters_y - 1.55 * body], Style { size: Size::Title, color: era_color, tracking: 0.14 * body });
        let civilisation = if c.alive {
            format!("Civilisation {}", c.civilisation)
        } else {
            format!("Civilisation {} perished", c.civilisation)
        };
        let counters = format!("Era {}  ·  {civilisation}  ·  Planet {planet}", c.era);
        self.text(out, &counters, [x0, counters_y], Style { size: Size::Body, color: dim, tracking: 0.0 });

        // the temperature gauge: a hairline from 3 K to 10,000 K, the liquid-water band, ticks
        // at each power of ten and where the civilisation perishes, and the planet's temperature
        let w = (0.3 * m).round();
        let thick = (0.0018 * m).max(1.0);
        let at = |t: f32| x0 + w * gauge(t);
        solid(out, [x0, bar_y - 0.5 * thick, x0 + w, bar_y + 0.5 * thick], [0.55, 0.6, 0.7, 0.45 * alpha], 1);
        let water = [0.40, 1.0, 0.75, 0.85 * alpha];
        solid(out, [at(WATER.0), bar_y - thick, at(WATER.1), bar_y + thick], water, 1);
        let tick = (0.35 * body).round();
        for t in [10.0, 100.0, 1000.0, 10_000.0] {
            let x = at(t).round();
            solid(out, [x - 0.5 * thick, bar_y - tick, x + 0.5 * thick, bar_y - 0.5 * thick], [0.55, 0.6, 0.7, 0.45 * alpha], 1);
        }
        for t in [CIV_COLD as f32, CIV_HOT as f32] {
            let x = at(t).round();
            solid(out, [x - 0.5 * thick, bar_y + 0.5 * thick, x + 0.5 * thick, bar_y + tick], [1.0, 0.55, 0.4, 0.5 * alpha], 1);
        }
        let [r, g, b] = temperature_color(c.temp);
        let dot = (0.0045 * m).max(3.0);
        let x = at(c.temp);
        solid(out, [x - dot, bar_y - dot, x + dot, bar_y + dot], [r, g, b, alpha], 2);
        let reading = if c.temp < 9999.5 { format!("{:.0} K", c.temp) } else { "> 10 000 K".to_string() };
        let at = [x0 + w + 0.6 * body, bar_y + 0.36 * body];
        self.text(out, &reading, at, Style { size: Size::Body, color: [r, g, b, 0.95 * alpha], tracking: 0.0 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readout_lays_out_inside_the_frame() {
        let hud = Hud::new(800.0);
        let c = Chronicle { temp: 288.0, stable: false, era: 12, civilisation: 3, alive: false };
        let mut quads = Vec::new();
        hud.layout(&mut quads, 800.0, 800.0, 7, &c, 1.0);
        assert!(quads.len() > 40, "{} quads", quads.len());
        assert!(quads.iter().all(|q| q.rect[0] >= 0.0 && q.rect[1] >= 0.0 && q.rect[2] <= 1280.0 && q.rect[3] <= 800.0));
        // the glyphs' texture coordinates point into the atlas
        assert!(quads.iter().filter(|q| q.kind == 0).all(|q| q.uv.iter().all(|&u| (0.0..=1.0).contains(&u))));
    }

    #[test]
    fn temperatures_have_colours_and_places() {
        assert!(gauge(3.0) == 0.0 && gauge(10_000.0) == 1.0);
        assert!(gauge(288.0) > gauge(100.0));
        let earth = temperature_color(288.0);
        assert!(earth[1] > earth[0], "a living green at Earth's warmth");
        let hot = temperature_color(1500.0);
        assert!(hot[0] > hot[2], "glowing red-orange when rock glows");
    }
}
