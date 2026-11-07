# To do

## Performance (smoke)

Measure with `moirai --dry-run 20 --size 2880x1800` (and `moirai --term --dry-run 20 --size
1280x800`) on a busy scene such as Moth I (`--scene choreo:2`), and compare `--snapshot` renders
with the previous commit to keep the look. Savings below are rough estimates per full-screen
frame (smoke currently costs about 2.9 ms more than lines there).

- [ ] Tighter smoke quads: size each segment by the swell and sideways drift actually sampled
      along it (the vertex shader can read the same noise) instead of the worst case
      (2.5 × 1.82 half-widths plus the full drift). Same look; about 0.3 ms.
- [ ] Quarter resolution for the oldest smoke (from about 60% of the trail on), crossfaded like
      the half-resolution layer. Barely visible; about 0.3–0.5 ms.
- [ ] Redraw the old-smoke layer only every other frame at 60 fps (it changes slowly). About
      0.5 ms; needs reprojection while a dance zooms, or it will judder.
- [ ] On battery: lower the full-screen frame rate (30–45 fps; the motion is slow) or switch to
      stardust automatically when unplugged (`/sys/class/power_supply`).
- [ ] Re-run the battery power benchmark for smoke and stardust (unplugged, machine idle).

## Planet

- [ ] Step 2: temperature on a log scale (3 K to 10,000 K, liquid-water band marked; the suns'
      light with inverse-square falloff, smoothed over about a planet year like thermal
      inertia), the planet tinted by it, and the era (stable when bound to one sun and the
      others pull less than about 5% as hard, held for a couple of planet years; chaotic
      otherwise). A civilisation counter: a civilisation ends when it gets hot enough to boil
      the oceans or cold enough to freeze the air, and the next starts once it is habitable
      again; the planet carries on. Needs a small text renderer (an embedded open-licensed
      font).
- [ ] Step 3, only if runs feel too short: launch selection by look-ahead (try a few dozen
      launches against the already computed future, keep the longest-lived; never nudge the
      planet itself).

## Later

- [ ] Start automatically when the session goes idle (GNOME: Mutter's IdleMonitor over D-Bus).
