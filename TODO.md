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

## Performance (Ghostty pane)

Ghostty's own cost is mostly per frame: in a quarter-screen pane at 30 fps about 8% of a core
to redraw its window and about 11% to take in each frame (read, copy, new GPU texture), against
moirai's 6–7%. Measured with a separate `ghostty --gtk-single-instance=false -e moirai --term`
window and `/proc/<pid>/stat`. Ruled out: synchronized output (Ghostty already draws once per
frame), one image id replaced in place (no gain), animation-frame edits (Ghostty 1.4 composes
them on the CPU and still re-uploads the whole texture: costlier), dirty tiles (the twinkling
starfield changes every tile every frame).

The pane now runs at 20 fps by default (a fifth less for Ghostty than 30 fps); at a quarter of
the screen that looks fine without motion blur.

## Planet

- [ ] Step 3, only if runs feel too short: launch selection by look-ahead (try a few dozen
      launches against the already computed future, keep the longest-lived; never nudge the
      planet itself).

## Later

- [ ] Start automatically when the session goes idle (GNOME: Mutter's IdleMonitor over D-Bus).
