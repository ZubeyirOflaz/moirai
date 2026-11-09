//! Moirai: three bodies, dancing forever.

mod engine;
mod hud;
mod render;
mod term;
mod window;

use std::process::ExitCode;

use engine::{
    EngineConfig, PLANET_PACE, Paths, STORY_SECONDS, SceneRequest, scene_annals, snapshot_scene, still_frame, story_scene,
    story_still,
};
use moirai_core::annals::{Durations, Span};
use moirai_core::library::catalog;
use moirai_core::scene::DirectorConfig;
use render::{FrameInput, Gpu, Look, OFFSCREEN_FORMAT, Offscreen, PALETTES, Renderer, TrailStyle, instance, palette_index};

const HELP: &str = "\
moirai - three bodies, dancing forever

USAGE
  moirai                 fullscreen (Esc or q to quit)
  moirai --window        in a window
  moirai --term          inside this terminal pane (Ghostty, kitty, WezTerm)
  moirai --snapshot FILE render one still frame to a PNG
  moirai --list          list the stable choreographies

KEYS
  Esc / q  quit      n  next scene      space  pause
  t  trails: smoke -> lines -> stardust  c  next colour palette
  p  show the planet

OPTIONS
  --scene S            first scene: random | figure8 | choreo:<id> | dance:<seed>
  --dances F           share of free dances vs choreographies, 0..1 (default 0.35)
  --scene-revs N       scene length in revolutions (default 24)
  --seconds-per-rev S  screen time per revolution (default 6)
  --trails T           smoke (glowing plasma plumes, default) | lines (glowing threads,
                       cheapest) | dust (stardust motes)
  --palette P,...      palettes to rotate through, or 'all' (default: the featured ones)
                       featured: PALETTES_FEATURED
                       others:   PALETTES_OTHER
  --stars F            starfield brightness (default 1; 0 or --no-stars: plain background)
  --planet             show the planet: a massless world in the habitable zone of one sun,
                       replaced by a new one when it hits a star or leaves the system (the
                       scene runs at half speed while it is shown); when a scene ends, the
                       story of its worlds is told for 30 s
  --planet-stats N     simulate N scenes without showing anything and report how long their
                       planets lasted
  --fps N              frame rate (default 60 in a window, 20 in a terminal)
  --scale F            terminal render resolution relative to the pane (default 1)
  --seed N             random seed
  --size WxH           snapshot size (default 1280x800)
  --at R               snapshot time in revolutions (default 6)
  --frames N           with --snapshot: N consecutive frames at --fps (FILE-000.png, ...)
  --story              with --snapshot: the story told when the scene ends (with --frames: as it
                       appears, from its first second)
  --dry-run SECONDS    measure the frame cost at --size without showing anything (with
                       --term: including the copy into the frame the terminal reads)
";

/// Frame rate in a terminal pane: a quarter-screen pane looks fine at 20 fps, and every frame
/// costs the terminal more than it costs moirai (see the README).
const TERM_FPS: f64 = 20.0;

struct Args {
    mode: Mode,
    scene: SceneRequest,
    dances: f64,
    scene_revs: f64,
    seconds_per_rev: f64,
    fps: Option<f64>,
    scale: f32,
    look: Look,
    palettes: u32,
    seed: u64,
    size: (u32, u32),
    at: f64,
    frames: u32,
    dry_run: Option<f64>,
    /// With `--snapshot`: the story told at the end of the scene.
    story: bool,
}

enum Mode {
    Fullscreen,
    Window,
    Term,
    Snapshot(String),
    PlanetStats(usize),
    List,
    Help,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        mode: Mode::Fullscreen,
        scene: SceneRequest::Random,
        dances: 0.35,
        scene_revs: 24.0,
        seconds_per_rev: 6.0,
        fps: None,
        scale: 1.0,
        look: Look { style: TrailStyle::Smoke, stars: 1.0, planet: false },
        palettes: 0,
        seed: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64),
        size: (1280, 800),
        at: 6.0,
        frames: 1,
        dry_run: None,
        story: false,
    };
    let mut it = std::env::args().skip(1);
    let num = |v: Option<String>, k: &str| -> Result<f64, String> {
        v.ok_or(format!("{k} needs a value"))?.parse::<f64>().map_err(|_| format!("bad number for {k}"))
    };
    while let Some(k) = it.next() {
        match k.as_str() {
            "-h" | "--help" => a.mode = Mode::Help,
            "--window" => a.mode = Mode::Window,
            "--term" => a.mode = Mode::Term,
            "--list" => a.mode = Mode::List,
            "--snapshot" => a.mode = Mode::Snapshot(it.next().ok_or("--snapshot needs a file name")?),
            "--scene" => {
                let v = it.next().ok_or("--scene needs a value")?;
                a.scene = match v.as_str() {
                    "random" => SceneRequest::Random,
                    "figure8" | "figure-eight" => SceneRequest::Choreography(1),
                    s if s.starts_with("choreo:") => {
                        SceneRequest::Choreography(s[7..].parse().map_err(|_| "bad choreography id")?)
                    }
                    s if s.starts_with("dance:") => SceneRequest::Dance(s[6..].parse().map_err(|_| "bad dance seed")?),
                    _ => return Err(format!("unknown scene '{v}'")),
                }
            }
            "--dances" => a.dances = num(it.next(), &k)?.clamp(0.0, 1.0),
            "--scene-revs" => a.scene_revs = num(it.next(), &k)?.max(2.0),
            "--seconds-per-rev" => a.seconds_per_rev = num(it.next(), &k)?.max(0.5),
            "--fps" => a.fps = Some(num(it.next(), &k)?.clamp(1.0, 240.0)),
            "--scale" => a.scale = num(it.next(), &k)?.clamp(0.25, 2.0) as f32,
            "--no-stars" => a.look.stars = 0.0,
            "--planet" => a.look.planet = true,
            "--story" => (a.story, a.look.planet) = (true, true),
            "--planet-stats" => a.mode = Mode::PlanetStats(num(it.next(), &k)?.clamp(1.0, 10_000.0) as usize),
            "--stars" => a.look.stars = num(it.next(), &k)?.clamp(0.0, 10.0) as f32,
            "--trails" => {
                let v = it.next().ok_or("--trails needs a value")?;
                a.look.style = TrailStyle::parse(&v).ok_or(format!("unknown trail style '{v}' (smoke, lines or dust)"))?;
            }
            "--palette" | "--palettes" => {
                let v = it.next().ok_or("--palette needs a name")?;
                a.palettes = 0;
                for name in v.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                    if name == "all" {
                        a.palettes |= (1 << PALETTES.len()) - 1;
                    } else {
                        let i = palette_index(name).ok_or(format!("unknown palette '{name}'"))?;
                        a.palettes |= 1 << i;
                    }
                }
            }
            "--seed" => a.seed = num(it.next(), &k)? as u64,
            "--at" => a.at = num(it.next(), &k)?.max(0.0),
            "--frames" => a.frames = num(it.next(), &k)?.clamp(1.0, 10_000.0) as u32,
            "--dry-run" => a.dry_run = Some(num(it.next(), &k)?.max(1.0)),
            "--size" => {
                let v = it.next().ok_or("--size needs WxH")?;
                let (w, h) = v.split_once('x').ok_or("--size needs WxH")?;
                a.size = (w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?);
            }
            _ => return Err(format!("unknown option '{k}' (see --help)")),
        }
    }
    Ok(a)
}

fn snapshot(a: &Args, path: &str) -> Result<(), String> {
    let gpu = Gpu::new(instance(), None)?;
    let (w, h) = a.size;
    let fps = a.fps.unwrap_or(30.0);
    // with the planet shown the scene runs slower, as it does live
    let seconds_per_rev = a.seconds_per_rev / if a.look.planet { PLANET_PACE } else { 1.0 };
    let mut renderer = Renderer::new(&gpu, OFFSCREEN_FORMAT, w, h);
    let off = Offscreen::new(&gpu, w, h);
    let mut paths = Paths::default();
    let mut px = vec![0u8; (w * h * 4) as usize];
    let file_name = |i: u32| {
        if a.frames == 1 {
            path.to_string()
        } else {
            let (stem, ext) = path.rsplit_once('.').unwrap_or((path, "png"));
            format!("{stem}-{i:03}.{ext}")
        }
    };
    let mut shoot = |input: &FrameInput, file_name: &str| -> Result<(), String> {
        let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("snapshot") });
        renderer.render(&gpu, &mut enc, &off.view, input);
        off.copy_out(&mut enc);
        gpu.queue.submit([enc.finish()]);
        off.read(&gpu, &mut px);
        let file = std::fs::File::create(file_name).map_err(|e| format!("{file_name}: {e}"))?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().map_err(|e| e.to_string())?;
        wr.write_image_data(&px).map_err(|e| e.to_string())
    };
    if a.story {
        // the scene computed to its end, and its story as told then (held, or frame by frame)
        let director = DirectorConfig { dance_share: a.dances, scene_revs: a.scene_revs };
        let (scene, end, annals) = story_scene(a.scene, a.seed, director);
        for i in 0..a.frames {
            let s = if a.frames == 1 { 0.5 * STORY_SECONDS } else { i as f64 / fps };
            let input = story_still(&scene, end, &annals, &mut paths, a.look, a.palettes, seconds_per_rev, s);
            shoot(&input, &file_name(i))?;
            if i == 0 || i + 1 == a.frames {
                println!("{} (its story) -> {}", scene.title, file_name(i));
            }
        }
        return Ok(());
    }
    let revs_per_frame = 1.0 / (fps * seconds_per_rev);
    let last = a.at + (a.frames - 1) as f64 * revs_per_frame;
    let (scene, _) = snapshot_scene(a.scene, a.seed, last);
    for i in 0..a.frames {
        let revs = a.at + i as f64 * revs_per_frame;
        let time = (revs * seconds_per_rev) as f32;
        let input = still_frame(&scene, revs * moirai_core::REV, &mut paths, a.look, a.palettes, time, seconds_per_rev);
        shoot(&input, &file_name(i))?;
        if i == 0 || i + 1 == a.frames {
            println!("{} ({}) -> {}", scene.title, input.palette.name, file_name(i));
        }
    }
    Ok(())
}

/// Simulate scenes without showing them and report how long their planets lasted.
fn planet_stats(a: &Args, count: usize) -> Result<(), String> {
    let director = DirectorConfig { dance_share: a.dances, scene_revs: a.scene_revs };
    let year = moirai_core::annals::year();
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let durations = |d: Option<Durations>| match d {
        Some(d) => format!("{} ({:.1}/{:.1}/{:.0} y)", d.count, d.shortest / year, d.median / year, d.longest / year),
        None => "0".into(),
    };
    let (mut lives, mut eras, mut civs, mut fates) = (Vec::new(), Vec::new(), Vec::new(), [0usize; 4]);
    for (title, end, an) in scene_annals(a.seed, director, a.scene, count) {
        let f = an.fates();
        let within = |w: &Span, s: &Span| s.start >= w.start && s.end <= w.end;
        for w in &an.worlds {
            lives.push(w.span.length() / year);
            eras.push(an.eras.iter().filter(|e| within(&w.span, &e.span)).count() as f64);
            civs.push(an.civilisations.iter().filter(|c| within(&w.span, c)).count() as f64);
        }
        let name = title.split(" · ").next().unwrap_or(&title).to_string();
        println!(
            "{name:<22} {:4.0} years: {:3} worlds ({}/{}/{}/{} swallowed/ejected/scorched/left), {:3} civilisations {}, \
             stable eras {}, chaotic eras {}",
            end / year,
            an.worlds.len(),
            f[0],
            f[1],
            f[2],
            f[3],
            an.civilisations.len(),
            durations(Durations::of(an.civilisations.iter().copied())),
            durations(an.era_durations(true)),
            durations(an.era_durations(false)),
        );
        for k in 0..4 {
            fates[k] += f[k];
        }
    }
    let m = median(&mut lives);
    println!(
        "\nall {count} scenes: {} worlds, median life {m:.0} years ({:.0} s on screen at the planet's pace); {} swallowed, \
         {} ejected, {} scorched, {} lasted to the end of their scene (counts: shortest/median/longest in years)",
        lives.len(),
        m * year / moirai_core::REV * a.seconds_per_rev / PLANET_PACE,
        fates[0],
        fates[1],
        fates[2],
        fates[3],
    );
    let most = |v: &Vec<f64>| v.iter().copied().fold(0.0, f64::max);
    println!(
        "per world: median {:.0} eras (most {:.0}) and {:.0} civilisations (most {:.0})",
        median(&mut eras),
        most(&eras),
        median(&mut civs),
        most(&civs)
    );
    Ok(())
}

fn main() -> ExitCode {
    let a = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("moirai: {e}");
            return ExitCode::from(2);
        }
    };
    let cfg = EngineConfig {
        seed: a.seed,
        director: DirectorConfig { dance_share: a.dances, scene_revs: a.scene_revs },
        first: a.scene,
        seconds_per_rev: a.seconds_per_rev,
        fade_seconds: 2.5,
        palettes: a.palettes,
    };
    let result = match &a.mode {
        Mode::Help => {
            let names = |featured: bool| -> String {
                PALETTES.iter().filter(|p| p.featured == featured).map(|p| p.name).collect::<Vec<_>>().join(", ")
            };
            print!("{}", HELP.replace("PALETTES_FEATURED", &names(true)).replace("PALETTES_OTHER", &names(false)));
            Ok(())
        }
        Mode::List => {
            for o in catalog() {
                println!(
                    "{:4}  {:<14} period {:6.2} rev  closest {:.3}  extent {:.2}{}",
                    o.id,
                    o.name().unwrap_or(if o.figure_eight_family { "figure-8 kin" } else { "" }),
                    o.revs,
                    o.closest,
                    o.extent,
                    if o.kick == 0.0 { "  (no kicks)" } else { "" }
                );
            }
            Ok(())
        }
        Mode::Snapshot(path) => snapshot(&a, path),
        Mode::PlanetStats(n) => planet_stats(&a, *n),
        Mode::Term => term::run(
            cfg,
            term::TermOptions { fps: a.fps.unwrap_or(TERM_FPS), scale: a.scale, look: a.look, dry_run: a.dry_run, dry_size: a.size },
        ),
        Mode::Window | Mode::Fullscreen if a.dry_run.is_some() => Gpu::new(instance(), None).and_then(|gpu| {
            let seconds = a.dry_run.unwrap_or(10.0);
            term::dry_run(&gpu, cfg, a.look, a.fps.unwrap_or(60.0), a.size, seconds, false)
        }),
        Mode::Window | Mode::Fullscreen => window::run(
            cfg,
            window::WindowOptions { fullscreen: matches!(a.mode, Mode::Fullscreen), fps: a.fps.unwrap_or(60.0), look: a.look },
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("moirai: {e}");
            ExitCode::FAILURE
        }
    }
}
