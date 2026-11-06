//! Run inside a terminal pane using the kitty graphics protocol (Ghostty, kitty, WezTerm).
//!
//! Frames are rendered on the GPU, read back, and handed to the terminal through POSIX shared
//! memory (`t=s`): no base64 image data flows through the pty, only a ~100 byte command per
//! frame. The terminal unlinks each shared-memory object after reading it; if it has not
//! consumed the frame before last, we skip a frame instead of piling up memory.

use std::ffi::CString;
use std::io::{self, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::engine::{Engine, EngineConfig};
use crate::render::{Gpu, OFFSCREEN_FORMAT, Offscreen, Renderer, instance};

static ORIGINAL: OnceLock<libc::termios> = OnceLock::new();
static RESIZED: AtomicBool = AtomicBool::new(false);
static QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    RESIZED.store(true, Ordering::Relaxed);
}
extern "C" fn on_quit(_: libc::c_int) {
    QUIT.store(true, Ordering::Relaxed);
}

const ENTER: &str = "\x1b[?1049h\x1b[?25l\x1b[2J";
const LEAVE: &str = "\x1b_Ga=d,d=A,q=2\x1b\\\x1b[?25h\x1b[?1049l";

fn restore_terminal() {
    if let Some(t) = ORIGINAL.get() {
        let mut out = io::stdout();
        let _ = out.write_all(LEAVE.as_bytes());
        let _ = out.flush();
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, t) };
    }
}

fn enter_raw() -> io::Result<()> {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let _ = ORIGINAL.set(t);
    let mut raw = t;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::IEXTEN);
    raw.c_iflag &= !(libc::IXON | libc::ICRNL);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) } != 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_quit as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_quit as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_quit as *const () as libc::sighandler_t);
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev(info);
    }));
    Ok(())
}

/// Read whatever the terminal sends within `timeout`, stopping early once `done` is satisfied.
fn read_until(timeout: Duration, done: impl Fn(&[u8]) -> bool) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    while Instant::now() < deadline && !done(&buf) {
        let left = deadline.saturating_duration_since(Instant::now()).as_millis().min(100) as i32;
        let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, left) } > 0 {
            let n = unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n > 0 {
                buf.extend_from_slice(&chunk[..n as usize]);
            }
        }
    }
    buf
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Primary device attributes reply (`ESC [ ? ... c`) marks the end of our query batch.
fn has_da1(buf: &[u8]) -> bool {
    buf.windows(3).enumerate().any(|(i, w)| w == b"\x1b[?" && buf[i + 3..].contains(&b'c'))
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

/// Create a POSIX shared-memory object of `len` bytes and let `fill` write into the mapping.
fn write_shm(name: &str, len: usize, fill: impl FnOnce(&mut [u8])) -> io::Result<()> {
    let c = CString::new(name).map_err(io::Error::other)?;
    unsafe {
        let fd = libc::shm_open(c.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let ok = libc::ftruncate(fd, len as libc::off_t) == 0;
        let ptr = if ok {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
        } else {
            libc::MAP_FAILED
        };
        if ptr == libc::MAP_FAILED {
            let e = io::Error::last_os_error();
            libc::close(fd);
            libc::shm_unlink(c.as_ptr());
            return Err(e);
        }
        fill(std::slice::from_raw_parts_mut(ptr.cast::<u8>(), len));
        libc::munmap(ptr, len);
        libc::close(fd);
    }
    Ok(())
}

fn unlink_shm(name: &str) {
    if let Ok(c) = CString::new(name) {
        unsafe { libc::shm_unlink(c.as_ptr()) };
    }
}

fn shm_exists(name: &str) -> bool {
    std::path::Path::new(&format!("/dev/shm{name}")).exists()
}

#[derive(Clone, Copy, Debug)]
struct Geometry {
    cols: u32,
    rows: u32,
    width: u32,
    height: u32,
}

fn geometry(scale: f32) -> io::Result<Geometry> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (cols, rows) = (ws.ws_col.max(1) as u32, ws.ws_row.max(1) as u32);
    let (mut cw, mut ch) = (ws.ws_xpixel as f32 / cols as f32, ws.ws_ypixel as f32 / rows as f32);
    if ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        // ask for the cell size: CSI 16 t -> CSI 6 ; height ; width t
        let mut out = io::stdout();
        out.write_all(b"\x1b[16t")?;
        out.flush()?;
        let r = read_until(Duration::from_millis(500), |b| contains(b, b"t") && contains(b, b"\x1b[6;"));
        let s = String::from_utf8_lossy(&r);
        let parsed = s.split("\x1b[6;").nth(1).and_then(|rest| {
            let body = rest.split('t').next()?;
            let mut it = body.split(';');
            Some((it.next()?.parse::<f32>().ok()?, it.next()?.parse::<f32>().ok()?))
        });
        (ch, cw) = parsed.unwrap_or((20.0, 10.0));
    }
    let width = ((cols as f32 * cw.floor() * scale) as u32).clamp(16, 9999);
    let height = ((rows as f32 * ch.floor() * scale) as u32).clamp(16, 9999);
    Ok(Geometry { cols, rows, width, height })
}

pub struct TermOptions {
    pub fps: f64,
    pub scale: f32,
    pub stars: f32,
    /// Measure the cost of producing frames without a terminal (shm objects are created and
    /// removed again, nothing is printed).
    pub dry_run: Option<f64>,
    pub dry_size: (u32, u32),
}

pub fn run(cfg: EngineConfig, opts: TermOptions) -> Result<(), String> {
    let gpu = Gpu::new(instance(), None)?;
    if let Some(seconds) = opts.dry_run {
        return dry_run(&gpu, cfg, &opts, seconds);
    }
    if unsafe { libc::isatty(libc::STDIN_FILENO) == 0 || libc::isatty(libc::STDOUT_FILENO) == 0 } {
        return Err("--term needs an interactive terminal".into());
    }
    enter_raw().map_err(|e| format!("cannot configure the terminal: {e}"))?;
    let result = run_inner(&gpu, cfg, &opts);
    restore_terminal();
    result
}

fn run_inner(gpu: &Gpu, cfg: EngineConfig, opts: &TermOptions) -> Result<(), String> {
    let mut out = io::stdout().lock();
    let io_err = |e: io::Error| e.to_string();

    // Capability probe: a direct 1x1 image, a shared-memory 1x1 image, then DA1 as a sentinel.
    let pid = std::process::id();
    let probe = format!("/moirai-{pid}-probe");
    let shm_probe_ok = write_shm(&probe, 3, |d| d.fill(0)).is_ok();
    let mut q = String::from("\x1b_Gi=4242,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    if shm_probe_ok {
        q += &format!("\x1b_Gi=4243,s=1,v=1,a=q,t=s,f=24;{}\x1b\\", b64(probe.as_bytes()));
    }
    q += "\x1b[c";
    out.write_all(q.as_bytes()).map_err(io_err)?;
    out.flush().map_err(io_err)?;
    let reply = read_until(Duration::from_millis(1500), has_da1);
    let kitty = contains(&reply, b"i=4242;OK");
    let shm = shm_probe_ok && contains(&reply, b"i=4243;OK");
    unlink_shm(&probe);
    if !kitty {
        return Err("this terminal does not support the kitty graphics protocol (try Ghostty, kitty or WezTerm)".into());
    }

    out.write_all(ENTER.as_bytes()).map_err(io_err)?;
    out.flush().map_err(io_err)?;

    let mut geo = geometry(opts.scale).map_err(io_err)?;
    let mut renderer = Renderer::new(gpu, OFFSCREEN_FORMAT, geo.width, geo.height);
    let mut off = Offscreen::new(gpu, geo.width, geo.height);
    let mut pixels = vec![0u8; (geo.width * geo.height * 4) as usize];
    let fps = if shm { opts.fps } else { opts.fps.min(10.0) };
    let frame_dt = Duration::from_secs_f64(1.0 / fps);

    let mut engine = Engine::start(cfg);
    let mut counter: u64 = 0;
    let mut sent: Vec<String> = Vec::new();
    let mut last = Instant::now();
    let mut input = [0u8; 64];

    while !QUIT.load(Ordering::Relaxed) {
        let start = Instant::now();
        // keys
        let n = unsafe { libc::read(libc::STDIN_FILENO, input.as_mut_ptr().cast(), input.len()) };
        if n > 0 {
            let keys = &input[..n as usize];
            if keys == [0x1b] || keys.contains(&b'q') || keys.contains(&b'Q') || keys.contains(&3) {
                break;
            }
            if keys.contains(&b'n') {
                engine.skip();
            }
            if keys.contains(&b' ') {
                engine.paused = !engine.paused;
            }
        }
        if RESIZED.swap(false, Ordering::Relaxed) {
            geo = geometry(opts.scale).map_err(io_err)?;
            renderer.resize(gpu, geo.width, geo.height);
            off = Offscreen::new(gpu, geo.width, geo.height);
            pixels = vec![0u8; (geo.width * geo.height * 4) as usize];
            out.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[2J").map_err(io_err)?;
        }
        let now = Instant::now();
        engine.update((now - last).as_secs_f64());
        last = now;

        // backpressure: the terminal has not yet read the frame before last -> skip this one
        let busy = shm && sent.len() >= 2 && shm_exists(&sent[sent.len() - 2]);
        if !busy {
            let frame = engine.frame(opts.stars);
            let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
            renderer.render(gpu, &mut enc, &off.view, &frame);
            off.copy_out(&mut enc);
            gpu.queue.submit([enc.finish()]);

            counter += 1;
            let id = 1 + (counter % 2);
            let old = 1 + ((counter + 1) % 2);
            let head = format!(
                "a=T,f=32,s={},v={},c={},r={},i={id},p=1,C=1,q=2",
                geo.width, geo.height, geo.cols, geo.rows
            );
            let mut cmd = String::from("\x1b[H");
            if shm {
                let name = format!("/moirai-{pid}-{counter}");
                write_shm(&name, pixels.len(), |dst| off.read(gpu, dst)).map_err(|e| format!("shared memory: {e}"))?;
                cmd += &format!("\x1b_G{head},t=s;{}\x1b\\", b64(name.as_bytes()));
                sent.push(name);
                if sent.len() > 4 {
                    sent.remove(0);
                }
            } else {
                off.read(gpu, &mut pixels);
                let data = b64(&pixels);
                let chunks: Vec<&str> = data.as_bytes().chunks(4096).map(|c| std::str::from_utf8(c).unwrap_or("")).collect();
                for (i, c) in chunks.iter().enumerate() {
                    let more = if i + 1 < chunks.len() { 1 } else { 0 };
                    if i == 0 {
                        cmd += &format!("\x1b_G{head},t=d,m={more};{c}\x1b\\");
                    } else {
                        cmd += &format!("\x1b_Gm={more};{c}\x1b\\");
                    }
                }
            }
            cmd += &format!("\x1b_Ga=d,d=I,i={old},q=2\x1b\\");
            out.write_all(cmd.as_bytes()).map_err(io_err)?;
            out.flush().map_err(io_err)?;
        }

        // sleep until the next frame, waking early for key presses
        let left = frame_dt.saturating_sub(start.elapsed());
        let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) };
        if pfd.revents & libc::POLLIN != 0 {
            // a key arrived: let the next iteration read it, after the rest of the frame time
            std::thread::sleep(frame_dt.saturating_sub(start.elapsed()).min(Duration::from_millis(5)));
        }
    }
    for name in &sent {
        unlink_shm(name);
    }
    Ok(())
}

/// Produce frames exactly as in terminal mode, without a terminal, and report the cost.
fn dry_run(gpu: &Gpu, cfg: EngineConfig, opts: &TermOptions, seconds: f64) -> Result<(), String> {
    let (w, h) = opts.dry_size;
    let mut renderer = Renderer::new(gpu, OFFSCREEN_FORMAT, w, h);
    let off = Offscreen::new(gpu, w, h);
    let len = (w * h * 4) as usize;
    let mut engine = Engine::start(cfg);
    let frame_dt = Duration::from_secs_f64(1.0 / opts.fps);
    let cpu0 = cpu_seconds();
    let t0 = Instant::now();
    let mut frames = 0u64;
    let mut render_time = Duration::ZERO;
    let mut last = Instant::now();
    while t0.elapsed().as_secs_f64() < seconds {
        let start = Instant::now();
        engine.update((start - last).as_secs_f64());
        last = start;
        let frame = engine.frame(opts.stars);
        let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        renderer.render(gpu, &mut enc, &off.view, &frame);
        off.copy_out(&mut enc);
        gpu.queue.submit([enc.finish()]);
        let name = format!("/moirai-{}-dry-{frames}", std::process::id());
        write_shm(&name, len, |dst| off.read(gpu, dst)).map_err(|e| e.to_string())?;
        unlink_shm(&name);
        frames += 1;
        render_time += start.elapsed();
        std::thread::sleep(frame_dt.saturating_sub(start.elapsed()));
    }
    let wall = t0.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu0;
    println!(
        "{frames} frames of {w}x{h} in {wall:.1}s ({:.1} fps): {:.2} ms per frame on the render thread, \
         process CPU {:.1}% of one core (render + physics + shm), scene: {}",
        frames as f64 / wall,
        render_time.as_secs_f64() * 1000.0 / frames as f64,
        100.0 * cpu / wall,
        engine.title()
    );
    Ok(())
}

fn cpu_seconds() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 * 1e-6;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}
