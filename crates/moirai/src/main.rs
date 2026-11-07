//! Moirai: three bodies, dancing forever.

mod engine;
mod hud;
mod render;
mod term;
mod window;

use std::process::ExitCode;

use engine::{EngineConfig, PLANET_PACE, SceneRequest, planet_runs, snapshot_scene, still_frame};
use moirai_core::library::catalog;
use moirai_core::scene::DirectorConfig;
use render::{Gpu, Look, OFFSCREEN_FORMAT, Offscreen, PALETTES, Renderer, TrailStyle, instance, palette_index};

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
                       scene runs at half speed while it is shown)
  --planet-stats N     simulate N scenes without showing anything and report how long their
                       planets lasted
  --fps N              frame rate (default 60 in a window, 30 in a terminal)
  --scale F            terminal render resolution relative to the pane (default 1)
  --seed N             random seed
  --size WxH           snapshot size (default 1280x800)
  --at R               snapshot time in revolutions (default 6)
  --frames N           with --snapshot: N consecutive frames at --fps (FILE-000.png, ...)
  --dry-run SECONDS    measure the frame cost at --size without showing anything (with
                       --term: including the copy into the frame the terminal reads)
";

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
    let revs_per_frame = 1.0 / (fps * seconds_per_rev);
    let last = a.at + (a.frames - 1) as f64 * revs_per_frame;
    let (scene, _) = snapshot_scene(a.scene, a.seed, last);
    let mut renderer = Renderer::new(&gpu, OFFSCREEN_FORMAT, w, h);
    let off = Offscreen::new(&gpu, w, h);
    let mut trails = Default::default();
    let mut planet_path = Vec::new();
    let mut px = vec![0u8; (w * h * 4) as usize];
    for i in 0..a.frames {
        let revs = a.at + i as f64 * revs_per_frame;
        let time = (revs * seconds_per_rev) as f32;
        let input = still_frame(&scene, revs * moirai_core::REV, &mut trails, &mut planet_path, a.look, a.palettes, time);
        let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("snapshot") });
        renderer.render(&gpu, &mut enc, &off.view, &input);
        off.copy_out(&mut enc);
        gpu.queue.submit([enc.finish()]);
        off.read(&gpu, &mut px);
        let file_name = if a.frames == 1 {
            path.to_string()
        } else {
            let (stem, ext) = path.rsplit_once('.').unwrap_or((path, "png"));
            format!("{stem}-{i:03}.{ext}")
        };
        let file = std::fs::File::create(&file_name).map_err(|e| format!("{file_name}: {e}"))?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().map_err(|e| e.to_string())?;
        wr.write_image_data(&px).map_err(|e| e.to_string())?;
        if i == 0 || i + 1 == a.frames {
            println!("{} ({}) -> {file_name}", scene.title, input.palette.name);
        }
    }
    Ok(())
}

/// Simulate scenes without showing them and report how long their planets lasted.
fn planet_stats(a: &Args, count: usize) -> Result<(), String> {
    use moirai_core::planet::Fate;
    let director = DirectorConfig { dance_share: a.dances, scene_revs: a.scene_revs };
    let mut all: Vec<f64> = Vec::new();
    let (mut eras, mut civs): (Vec<f64>, Vec<f64>) = (Vec::new(), Vec::new());
    let (mut swallowed, mut ejected, mut survived) = (0, 0, 0);
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    for (title, runs) in planet_runs(a.seed, director, a.scene, count) {
        let mut revs: Vec<f64> = runs.iter().map(|r| r.0).collect();
        let longest = revs.iter().copied().fold(0.0, f64::max);
        let n = |f: Option<Fate>| runs.iter().filter(|r| r.1 == f).count();
        let (s, e, k) = (n(Some(Fate::Swallowed)), n(Some(Fate::Ejected)), n(None));
        println!(
            "{title:<44} {:3} planets  median {:5.1} rev  longest {:5.1} rev  {e} ejected, {s} swallowed{}",
            runs.len(),
            median(&mut revs),
            longest,
            if k > 0 { ", 1 lasted to the end" } else { "" }
        );
        all.extend(&revs);
        eras.extend(runs.iter().map(|r| r.2.era as f64));
        civs.extend(runs.iter().map(|r| r.2.civilisation as f64));
        (swallowed, ejected, survived) = (swallowed + s, ejected + e, survived + k);
    }
    let m = median(&mut all);
    println!(
        "\nall {count} scenes: {} planets, median lifetime {m:.1} rev ({:.0} s on screen, where the planet slows the \
         scene down); {ejected} ejected, {swallowed} swallowed, {survived} lasted to the end of their scene",
        all.len(),
        m * a.seconds_per_rev / PLANET_PACE,
    );
    let most = |v: &Vec<f64>| v.iter().copied().fold(0.0, f64::max);
    println!(
        "per planet: median {:.0} eras (most {:.0}) and {:.0} civilisations (most {:.0})",
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
            term::TermOptions { fps: a.fps.unwrap_or(30.0), scale: a.scale, look: a.look, dry_run: a.dry_run, dry_size: a.size },
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
