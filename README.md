# Moirai

Three bodies, dancing forever.

<p>
  <img src="docs/moth-i.png" width="49%" alt="Moth I choreography, nebula palette">
  <img src="docs/dance.png" width="49%" alt="A steered free dance, solar palette">
</p>

Real three-body systems are chaotic: almost every configuration falls apart within a few
revolutions, as one body is flung away. Moirai shows long, beautiful three-body dances that look
completely natural, using two tricks that never show on screen:

- **Choreographies** — 385 genuinely stable periodic orbits (figure-eight, Moth I, Butterfly III
  and hundreds of rarer ones). Each showing gets a tiny random kick, so it is a slightly different
  quasi-periodic variant every time, and the whole scene is verified before it starts. Pure,
  untouched Newtonian gravity.
- **Dances** — random triangles kept alive by look-ahead steering. The physics runs about eight
  revolutions ahead of the screen; when that future contains an ejection, a near-collision or a
  pair locking into a dull binary, the engine rewrites it with a nudge typically smaller than
  one part in ten million (energy, momentum and angular momentum stay exactly conserved). Chaos
  amplifies the nudge until the bad future simply never happens.

The name: the Moirai are the three Fates. **Clotho** spins the thread (the renderer drawing the
trails), **Lachesis** measures it (the look-ahead steering), **Atropos** cuts it (the events the
engine prevents).

## Running it

```sh
cargo build --release
./target/release/moirai            # fullscreen, Esc or q to quit
./target/release/moirai --window   # in a window
./target/release/moirai --term     # inside the current terminal pane (Ghostty, kitty, WezTerm)
```

Keys: `Esc`/`q` quit · `n` next scene · `space` pause. See `moirai --help` for all options,
`moirai --list` for the choreographies, and `moirai --snapshot frame.png --scene choreo:2` for a still.

### In a Ghostty split

Open a split (e.g. `ctrl+shift+o` / `ctrl+shift+e` by default) and run `moirai --term` there.
Frames are rendered on the GPU and handed to Ghostty through shared memory (kitty graphics
protocol, `t=s`), so the pane shows real pixels at its native resolution, not text-mode art.

Cost on a Ryzen AI laptop (Radeon 860M), measured with `moirai --term --dry-run 15 --size WxH`:

| pane size | frame rate | moirai CPU (one core) |
|---|---|---|
| 1280×800 | 30 fps | ~6% |
| 640×400 | 30 fps | ~3% |

Ghostty additionally uploads each frame to its own GPU texture. To save battery, lower
`--fps` (the motion is slow; 20–24 fps looks fine) or `--scale 0.75`.

### As a screensaver (GNOME)

GNOME on Wayland has no screensaver plug-in API, so bind a keyboard shortcut instead:
Settings → Keyboard → View and Customize Shortcuts → Custom Shortcuts → `+`, with the command set
to the full path of the binary (e.g. `/home/you/Documents/Repos/moirai/target/release/moirai`).

## How it works

```
crates/moirai-core   physics, Atropos (events), Lachesis (steering), timeline, scenes; no dependencies
crates/moirai        wgpu renderer, window (winit), terminal (kitty graphics) and snapshot outputs
data/                the stable-orbit catalogue
```

- `physics` — Newtonian gravity, adaptive RK4 whose steps land on a fixed time grid, so any
  saved state restarts bit-for-bit (the steering relies on this).
- `events` — flags near-collisions, wide excursions, hard binaries and "frozen" hierarchical
  triples, preferably *before* they happen (Kepler predictions), while the motion is still
  chaotic enough to steer.
- `steer` — tries nudges of 1e-9, 1e-8, … at the earliest still-unseen moment and keeps the
  smallest one whose future is clean.
- `timeline` / `engine` — a background thread keeps each scene computed ahead of the display
  and sleeps otherwise; scenes fade in and out.
- Rendering — each body's trail is drawn into its own channel of an HDR texture with *max*
  blending (smooth glowing threads without beads at the joints, no blur passes), then one
  composite pass adds the bodies, a faint starfield, filmic tone mapping and dithering.

`cargo test --release` covers orbit closure and conservation, bit-identical restarts,
conservation-preserving nudges, steered runs replaying cleanly, and the real-time timeline.

## Credits

Stable orbits: I. Hristov, R. Hristova and K. Tanikawa, *An extensive search for stable periodic
orbits of the equal-mass zero angular momentum three-body problem* (2025),
[arXiv:2510.22802](https://arxiv.org/abs/2510.22802), data at
<https://db2.fmi.uni-sofia.bg/3bodystab971/>. Orbit names from M. Šuvakov and V. Dmitrašinović,
*Three classes of Newtonian three-body planar periodic orbits*, PRL 110 (2013).
