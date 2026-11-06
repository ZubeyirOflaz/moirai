//! Moirai: three bodies, dancing forever.

mod engine;
mod render;
mod term;
mod window;

use std::process::ExitCode;

use engine::{EngineConfig, SceneRequest, snapshot_scene, still_frame};
use moirai_core::library::catalog;
use moirai_core::scene::DirectorConfig;
use render::{Gpu, OFFSCREEN_FORMAT, Offscreen, Renderer, instance};

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

OPTIONS
  --scene S            first scene: random | figure8 | choreo:<id> | dance:<seed>
  --dances F           share of free dances vs choreographies, 0..1 (default 0.35)
  --scene-revs N       scene length in revolutions (default 24)
  --seconds-per-rev S  screen time per revolution (default 6)
  --fps N              frame rate (default 60 in a window, 30 in a terminal)
  --scale F            terminal render resolution relative to the pane (default 1)
  --no-stars           plain background
  --seed N             random seed
  --size WxH           snapshot size (default 1280x800)
  --at R               snapshot time in revolutions (default 6)
  --dry-run SECONDS    with --term: measure frame cost without a terminal
";

struct Args {
    mode: Mode,
    scene: SceneRequest,
    dances: f64,
    scene_revs: f64,
    seconds_per_rev: f64,
    fps: Option<f64>,
    scale: f32,
    stars: f32,
    seed: u64,
    size: (u32, u32),
    at: f64,
    dry_run: Option<f64>,
}

enum Mode {
    Fullscreen,
    Window,
    Term,
    Snapshot(String),
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
        stars: 1.0,
        seed: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64),
        size: (1280, 800),
        at: 6.0,
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
            "--no-stars" => a.stars = 0.0,
            "--seed" => a.seed = num(it.next(), &k)? as u64,
            "--at" => a.at = num(it.next(), &k)?.max(0.0),
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
    let (scene, t) = snapshot_scene(a.scene, a.seed, a.at);
    let mut renderer = Renderer::new(&gpu, OFFSCREEN_FORMAT, w, h);
    let off = Offscreen::new(&gpu, w, h);
    let mut trails = Default::default();
    let input = still_frame(&scene, t, &mut trails, a.stars);
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("snapshot") });
    renderer.render(&gpu, &mut enc, &off.view, &input);
    off.copy_out(&mut enc);
    gpu.queue.submit([enc.finish()]);
    let mut px = vec![0u8; (w * h * 4) as usize];
    off.read(&gpu, &mut px);
    let file = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(|e| e.to_string())?;
    wr.write_image_data(&px).map_err(|e| e.to_string())?;
    println!("{} ({}) -> {path}", scene.title, input.palette.name);
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
    };
    let result = match &a.mode {
        Mode::Help => {
            print!("{HELP}");
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
        Mode::Term => term::run(
            cfg,
            term::TermOptions { fps: a.fps.unwrap_or(30.0), scale: a.scale, stars: a.stars, dry_run: a.dry_run, dry_size: a.size },
        ),
        Mode::Window | Mode::Fullscreen => window::run(
            cfg,
            window::WindowOptions { fullscreen: matches!(a.mode, Mode::Fullscreen), fps: a.fps.unwrap_or(60.0), stars: a.stars },
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
