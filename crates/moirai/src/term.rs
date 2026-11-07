//! Run inside a terminal pane using the kitty graphics protocol (Ghostty, kitty, WezTerm).
//!
//! Frames are rendered on the GPU and read back into memory the terminal reads directly, so
//! only a ~100 byte command per frame flows through the pty. Preferred transport: two files in
//! a tmpfs (`t=f`), created and mapped once and overwritten in turn. The terminal reads a file
//! and answers OK, and only then is that file written again, so there is no tearing, natural
//! backpressure, and no per-frame allocation. Terminals that refuse files get a fresh POSIX
//! shared-memory object per frame (`t=s`, unlinked by the terminal after reading; creating and
//! zeroing it costs about three times the copy itself). The last resort is base64 through the
//! pty at a low frame rate.

use std::ffi::CString;
use std::io::{self, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::engine::{Engine, EngineConfig};
use crate::render::{Gpu, Look, OFFSCREEN_FORMAT, Offscreen, Renderer, instance};

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

/// A frame still unacknowledged after this long is assumed lost (its file is reused).
const ACK_TIMEOUT: Duration = Duration::from_secs(2);

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

/// Map `len` bytes of an open file read-write (pre-faulted), closing the descriptor.
unsafe fn map_fd(fd: libc::c_int, len: usize) -> io::Result<*mut libc::c_void> {
    unsafe {
        let ptr = if libc::ftruncate(fd, len as libc::off_t) == 0 {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                0,
            )
        } else {
            libc::MAP_FAILED
        };
        let err = io::Error::last_os_error();
        libc::close(fd);
        if ptr == libc::MAP_FAILED { Err(err) } else { Ok(ptr) }
    }
}

/// Create a POSIX shared-memory object of `len` bytes and let `fill` write into the mapping.
fn write_shm(name: &str, len: usize, fill: impl FnOnce(&mut [u8])) -> io::Result<()> {
    let c = CString::new(name).map_err(io::Error::other)?;
    unsafe {
        let fd = libc::shm_open(c.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let ptr = map_fd(fd, len).inspect_err(|_| {
            libc::shm_unlink(c.as_ptr());
        })?;
        fill(std::slice::from_raw_parts_mut(ptr.cast::<u8>(), len));
        libc::munmap(ptr, len);
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

/// Where frame files live: a tmpfs the terminal may read (Ghostty refuses /dev except /dev/shm).
fn frame_dir() -> String {
    if std::path::Path::new("/dev/shm").is_dir() {
        return "/dev/shm".into();
    }
    std::env::var("XDG_RUNTIME_DIR").ok().filter(|d| std::path::Path::new(d).is_dir()).unwrap_or_else(|| "/tmp".into())
}

/// A frame file in a tmpfs: created and mapped once, then overwritten in place (`t=f`).
struct FrameFile {
    path: String,
    ptr: *mut libc::c_void,
    len: usize,
    /// When the frame in it was sent, until the terminal's reply arrives.
    pending: Option<Instant>,
}

impl FrameFile {
    fn create(path: String, len: usize) -> io::Result<Self> {
        let c = CString::new(path.as_str()).map_err(io::Error::other)?;
        unsafe {
            let flags = libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let fd = libc::open(c.as_ptr(), flags, 0o600);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let ptr = map_fd(fd, len).inspect_err(|_| {
                libc::unlink(c.as_ptr());
            })?;
            Ok(Self { path, ptr, len, pending: None })
        }
    }

    fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.cast::<u8>(), self.len) }
    }

    /// Free to be written: the terminal has answered for its last frame (or never will).
    fn ready(&self) -> bool {
        self.pending.is_none_or(|t| t.elapsed() > ACK_TIMEOUT)
    }
}

impl Drop for FrameFile {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr, self.len) };
        if let Ok(c) = CString::new(self.path.as_str()) {
            unsafe { libc::unlink(c.as_ptr()) };
        }
    }
}

/// A fresh pair of frame files. Each set gets new names, so a command still in flight for
/// an old set fails cleanly instead of reading a frame of the new size.
fn frame_files(dir: &str, len: usize) -> io::Result<Vec<FrameFile>> {
    static GENERATION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    (1..=2).map(|k| FrameFile::create(format!("{dir}/moirai-{pid}-{generation}-frame-{k}"), len)).collect()
}

/// How frames reach the terminal.
enum Transport {
    /// Two mapped files, alternating; the terminal acknowledges each frame (`t=f`).
    /// `files` is empty until the frame size is known.
    Files { dir: String, files: Vec<FrameFile>, replied: bool, failures: u32, shm_fallback: bool },
    /// A fresh shared-memory object per frame (`t=s`).
    Shm { sent: Vec<String> },
    /// Base64 through the pty.
    Direct { pixels: Vec<u8> },
}

/// Something that arrived from the terminal: a key, or the reply to a graphics command.
#[derive(Debug, PartialEq)]
enum Event {
    Key(u8),
    Escape,
    Reply { id: u32, ok: bool },
}

/// Splits terminal input into keys and graphics replies. Sequences may arrive in pieces; a
/// lone ESC counts as the Escape key once a whole poll passes without a continuation.
#[derive(Default)]
struct InputParser {
    buf: Vec<u8>,
    lone_esc: bool,
}

impl InputParser {
    fn feed(&mut self, data: &[u8], out: &mut Vec<Event>) {
        self.buf.extend_from_slice(data);
        let b = &self.buf;
        let mut i = 0;
        while i < b.len() {
            if b[i] != 0x1b {
                out.push(Event::Key(b[i]));
                i += 1;
                continue;
            }
            let Some(&next) = b.get(i + 1) else {
                if self.lone_esc && data.is_empty() {
                    out.push(Event::Escape);
                    i += 1;
                }
                break;
            };
            let end = match next {
                // APC, e.g. ESC _ G i=1,p=1;OK ESC \
                b'_' => b[i + 2..].windows(2).position(|w| w == b"\x1b\\").map(|p| {
                    let body = &b[i + 2..i + 2 + p];
                    if let Some(reply) = parse_reply(body) {
                        out.push(reply);
                    }
                    i + 2 + p + 2
                }),
                // CSI (arrows, focus events, ...): skip to the final byte
                b'[' => b[i + 2..].iter().position(|c| (0x40..=0x7e).contains(c)).map(|p| i + 2 + p + 1),
                b'O' => (i + 2 < b.len()).then_some(i + 3),
                // ESC followed by anything else (e.g. Alt+key) is the Escape key
                _ => {
                    out.push(Event::Escape);
                    Some(i + 1)
                }
            };
            match end {
                Some(e) => i = e,
                None if b.len() - i > 1 << 16 => i = b.len(), // runaway garbage: drop it
                None => break,                                // incomplete: wait for the rest
            }
        }
        self.buf.drain(..i);
        self.lone_esc = self.buf == [0x1b];
    }
}

/// `G i=1,p=1;OK` -> Reply { id: 1, ok: true }
fn parse_reply(body: &[u8]) -> Option<Event> {
    let body = std::str::from_utf8(body.strip_prefix(b"G")?).ok()?;
    let (keys, msg) = body.split_once(';').unwrap_or((body, ""));
    let id = keys.split(',').find_map(|kv| kv.strip_prefix("i=")).and_then(|v| v.parse().ok())?;
    Some(Event::Reply { id, ok: msg == "OK" })
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
    pub look: Look,
    /// Measure the cost of producing frames without a terminal (frame files are created and
    /// removed again, nothing is printed).
    pub dry_run: Option<f64>,
    pub dry_size: (u32, u32),
}

pub fn run(cfg: EngineConfig, opts: TermOptions) -> Result<(), String> {
    let gpu = Gpu::new(instance(), None)?;
    if let Some(seconds) = opts.dry_run {
        return dry_run(&gpu, cfg, opts.look, opts.fps, opts.dry_size, seconds, true);
    }
    if unsafe { libc::isatty(libc::STDIN_FILENO) == 0 || libc::isatty(libc::STDOUT_FILENO) == 0 } {
        return Err("--term needs an interactive terminal".into());
    }
    enter_raw().map_err(|e| format!("cannot configure the terminal: {e}"))?;
    let result = run_inner(&gpu, cfg, &opts);
    restore_terminal();
    result
}

/// Ask the terminal which transports it accepts: a direct 1x1 image, a 1x1 image in a file
/// and in shared memory, then DA1 as a sentinel.
fn probe(out: &mut impl Write) -> Result<Transport, String> {
    let io_err = |e: io::Error| e.to_string();
    let pid = std::process::id();
    let dir = frame_dir();
    let file_probe = format!("{dir}/moirai-{pid}-probe-file");
    let shm_probe = format!("/moirai-{pid}-probe-shm");
    let file_ok = std::fs::write(&file_probe, [0u8; 3]).is_ok();
    let shm_ok = write_shm(&shm_probe, 3, |d| d.fill(0)).is_ok();
    let mut q = String::from("\x1b_Gi=4242,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    if file_ok {
        q += &format!("\x1b_Gi=4244,s=1,v=1,a=q,t=f,f=24;{}\x1b\\", b64(file_probe.as_bytes()));
    }
    if shm_ok {
        q += &format!("\x1b_Gi=4243,s=1,v=1,a=q,t=s,f=24;{}\x1b\\", b64(shm_probe.as_bytes()));
    }
    q += "\x1b[c";
    out.write_all(q.as_bytes()).map_err(io_err)?;
    out.flush().map_err(io_err)?;
    let reply = read_until(Duration::from_millis(1500), has_da1);
    let _ = std::fs::remove_file(&file_probe);
    unlink_shm(&shm_probe);
    if !contains(&reply, b"i=4242;OK") {
        return Err("this terminal does not support the kitty graphics protocol (try Ghostty, kitty or WezTerm)".into());
    }
    let shm = shm_ok && contains(&reply, b"i=4243;OK");
    if file_ok && contains(&reply, b"i=4244;OK") {
        return Ok(Transport::Files { dir, files: Vec::new(), replied: false, failures: 0, shm_fallback: shm });
    }
    Ok(Transport::fallback(shm))
}

impl Transport {
    fn fallback(shm: bool) -> Self {
        if shm { Self::Shm { sent: Vec::new() } } else { Self::Direct { pixels: Vec::new() } }
    }

    /// Size the frame buffers for `len` bytes (start-up and pane resizes).
    fn resize(&mut self, len: usize) -> Result<(), String> {
        match self {
            Self::Files { dir, files, .. } => *files = frame_files(dir, len).map_err(|e| format!("frame files: {e}"))?,
            Self::Shm { .. } => {}
            Self::Direct { pixels } => *pixels = vec![0u8; len],
        }
        Ok(())
    }

    /// Can a frame go out in `slot` now?
    fn ready(&self, slot: usize) -> bool {
        match self {
            Self::Files { files, .. } => files.get(slot).is_some_and(FrameFile::ready),
            // the terminal unlinks each object once read; wait while the one before last is unread
            Self::Shm { sent } => sent.len() < 2 || !shm_exists(&sent[sent.len() - 2]),
            Self::Direct { .. } => true,
        }
    }

    fn max_fps(&self) -> f64 {
        if matches!(self, Self::Direct { .. }) { 10.0 } else { f64::INFINITY }
    }

    /// A reply to a frame command arrived. Returns a fallback transport if files keep failing
    /// before ever working (the probe passed, but the terminal refuses the real frames).
    fn reply(&mut self, id: u32, ok: bool) -> Option<Transport> {
        let Self::Files { files, replied, failures, shm_fallback, .. } = self else { return None };
        if let Some(f) = files.get_mut((id as usize).wrapping_sub(1)) {
            f.pending = None;
        }
        if ok {
            *replied = true;
        } else if !*replied {
            *failures += 1;
            if *failures >= 3 {
                return Some(Transport::fallback(*shm_fallback));
            }
        }
        None
    }

    /// Without any reply within the timeout, the terminal does not acknowledge frames: use
    /// shared memory (whose backpressure needs no replies) or, failing that, direct data.
    fn check_silence(&self, first_sent: Option<Instant>) -> Option<Transport> {
        match self {
            Self::Files { replied: false, shm_fallback, .. } if first_sent.is_some_and(|t| t.elapsed() > ACK_TIMEOUT) => {
                Some(Transport::fallback(*shm_fallback))
            }
            _ => None,
        }
    }

    /// Read the rendered frame into the transport and return the command that shows it.
    fn send(&mut self, gpu: &Gpu, off: &Offscreen, head: &str, slot: usize, counter: u64) -> Result<String, String> {
        match self {
            Self::Files { files, .. } => {
                let f = files.get_mut(slot).ok_or("frame files are not set up")?;
                off.read(gpu, f.bytes());
                f.pending = Some(Instant::now());
                Ok(format!("\x1b_G{head},t=f,S={};{}\x1b\\", f.len, b64(f.path.as_bytes())))
            }
            Self::Shm { sent } => {
                let name = format!("/moirai-{}-{counter}", std::process::id());
                let len = (off.width * off.height * 4) as usize;
                write_shm(&name, len, |dst| off.read(gpu, dst)).map_err(|e| format!("shared memory: {e}"))?;
                sent.push(name.clone());
                if sent.len() > 4 {
                    sent.remove(0);
                }
                Ok(format!("\x1b_G{head},q=2,t=s;{}\x1b\\", b64(name.as_bytes())))
            }
            Self::Direct { pixels } => {
                off.read(gpu, pixels);
                let data = b64(pixels);
                let mut cmd = String::with_capacity(data.len() + data.len() / 4096 * 16 + 128);
                let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
                for (i, c) in chunks.iter().enumerate() {
                    let more = if i + 1 < chunks.len() { 1 } else { 0 };
                    let c = std::str::from_utf8(c).unwrap_or("");
                    if i == 0 {
                        cmd += &format!("\x1b_G{head},q=2,t=d,m={more};{c}\x1b\\");
                    } else {
                        cmd += &format!("\x1b_Gm={more};{c}\x1b\\");
                    }
                }
                Ok(cmd)
            }
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        if let Self::Shm { sent } = self {
            for name in sent.iter() {
                unlink_shm(name);
            }
        }
    }
}

fn run_inner(gpu: &Gpu, cfg: EngineConfig, opts: &TermOptions) -> Result<(), String> {
    let mut out = io::stdout().lock();
    let mut look = opts.look;
    let io_err = |e: io::Error| e.to_string();

    let mut transport = probe(&mut out)?;
    out.write_all(ENTER.as_bytes()).map_err(io_err)?;
    out.flush().map_err(io_err)?;

    let mut geo = geometry(opts.scale).map_err(io_err)?;
    let mut renderer = Renderer::new(gpu, OFFSCREEN_FORMAT, geo.width, geo.height);
    let mut off = Offscreen::new(gpu, geo.width, geo.height);
    transport.resize((geo.width * geo.height * 4) as usize)?;

    let mut engine = Engine::start(cfg);
    let mut counter: u64 = 0;
    let mut first_sent: Option<Instant> = None;
    let mut last = Instant::now();
    let mut next_frame = Instant::now();
    let mut parser = InputParser::default();
    let mut events = Vec::new();
    let mut input = [0u8; 4096];

    while !QUIT.load(Ordering::Relaxed) {
        // sleep until the next frame is due, waking early for keys and replies
        let wait = next_frame.saturating_duration_since(Instant::now());
        let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut pfd, 1, wait.as_micros().div_ceil(1000) as i32) };
        let n = unsafe { libc::read(libc::STDIN_FILENO, input.as_mut_ptr().cast(), input.len()) };
        parser.feed(if n > 0 { &input[..n as usize] } else { &[] }, &mut events);
        let mut quit = false;
        for ev in events.drain(..) {
            match ev {
                Event::Escape | Event::Key(b'q' | b'Q' | 3) => quit = true,
                Event::Key(b'n') => engine.skip(),
                Event::Key(b' ') => engine.paused = !engine.paused,
                Event::Key(b't') => look.style = look.style.next(),
                Event::Key(b'p') => look.planet = !look.planet,
                Event::Key(b'c') => engine.cycle_palette(),
                Event::Key(_) => {}
                Event::Reply { id, ok } => {
                    if let Some(fallback) = transport.reply(id, ok) {
                        transport = fallback;
                        transport.resize((geo.width * geo.height * 4) as usize)?;
                    }
                }
            }
        }
        if quit {
            break;
        }
        if let Some(fallback) = transport.check_silence(first_sent) {
            transport = fallback;
            transport.resize((geo.width * geo.height * 4) as usize)?;
        }
        let now = Instant::now();
        if now < next_frame {
            continue;
        }
        let fps = opts.fps.min(transport.max_fps());
        next_frame = (next_frame + Duration::from_secs_f64(1.0 / fps)).max(now);

        if RESIZED.swap(false, Ordering::Relaxed) {
            geo = geometry(opts.scale).map_err(io_err)?;
            renderer.resize(gpu, geo.width, geo.height);
            off = Offscreen::new(gpu, geo.width, geo.height);
            transport.resize((geo.width * geo.height * 4) as usize)?;
            out.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\\x1b[2J").map_err(io_err)?;
        }
        engine.update((now - last).as_secs_f64(), look);
        last = now;

        // ids 1 and 2 alternate: show the new image, then delete the previous one
        let slot = ((counter + 1) % 2) as usize;
        if !transport.ready(slot) {
            continue; // the terminal has not caught up: skip this frame
        }
        let frame = engine.frame(look);
        let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        renderer.render(gpu, &mut enc, &off.view, &frame);
        off.copy_out(&mut enc);
        gpu.queue.submit([enc.finish()]);
        counter += 1;
        let (id, old) = (slot + 1, 2 - slot);
        let head = format!("a=T,f=32,s={},v={},c={},r={},i={id},p=1,C=1", geo.width, geo.height, geo.cols, geo.rows);
        let mut cmd = String::from("\x1b[H");
        cmd += &transport.send(gpu, &off, &head, slot, counter)?;
        cmd += &format!("\x1b_Ga=d,d=I,i={old},q=2\x1b\\");
        out.write_all(cmd.as_bytes()).map_err(io_err)?;
        out.flush().map_err(io_err)?;
        first_sent.get_or_insert(now);
    }
    Ok(())
}

/// Produce frames without showing them and report the cost. With `readback` they take the
/// terminal path (GPU -> frame file); without, they stay on the GPU as in a window.
pub fn dry_run(
    gpu: &Gpu,
    cfg: EngineConfig,
    look: Look,
    fps: f64,
    (w, h): (u32, u32),
    seconds: f64,
    readback: bool,
) -> Result<(), String> {
    let mut renderer = Renderer::new(gpu, OFFSCREEN_FORMAT, w, h);
    let off = Offscreen::new(gpu, w, h);
    let mut files = if readback {
        Some(frame_files(&frame_dir(), (w * h * 4) as usize).map_err(|e| format!("frame files: {e}"))?)
    } else {
        None
    };
    let mut engine = Engine::start(cfg);
    let frame_dt = Duration::from_secs_f64(1.0 / fps);
    let cpu0 = cpu_seconds();
    let t0 = Instant::now();
    let mut frames = 0u64;
    let mut render_time = Duration::ZERO;
    let mut last = Instant::now();
    while t0.elapsed().as_secs_f64() < seconds {
        let start = Instant::now();
        engine.update((start - last).as_secs_f64(), look);
        last = start;
        let frame = engine.frame(look);
        let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        renderer.render(gpu, &mut enc, &off.view, &frame);
        if files.is_some() {
            off.copy_out(&mut enc);
        }
        gpu.queue.submit([enc.finish()]);
        match files.as_mut() {
            Some(files) => off.read(gpu, files[(frames % 2) as usize].bytes()),
            None => {
                let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
            }
        }
        frames += 1;
        render_time += start.elapsed();
        std::thread::sleep(frame_dt.saturating_sub(start.elapsed()));
    }
    let wall = t0.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu0;
    println!(
        "{frames} frames of {w}x{h} in {wall:.1}s ({:.1} fps): {:.2} ms per frame on the render thread \
         (including the GPU{}), process CPU {:.1}% of one core, scene: {}",
        frames as f64 / wall,
        render_time.as_secs_f64() * 1000.0 / frames as f64,
        if readback { " and the copy into the frame file" } else { "" },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(p: &mut InputParser, data: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        p.feed(data, &mut out);
        out
    }

    #[test]
    fn replies_and_keys_are_separated() {
        let mut p = InputParser::default();
        let ev = feed(&mut p, b"n\x1b_Gi=1,p=1;OK\x1b\\ \x1b_Gi=2;ENOENT:gone\x1b\\");
        assert_eq!(
            ev,
            vec![
                Event::Key(b'n'),
                Event::Reply { id: 1, ok: true },
                Event::Key(b' '),
                Event::Reply { id: 2, ok: false }
            ]
        );
    }

    #[test]
    fn split_reply_waits_for_its_end() {
        let mut p = InputParser::default();
        assert!(feed(&mut p, b"\x1b").is_empty());
        assert!(feed(&mut p, b"_Gi=2;O").is_empty());
        assert_eq!(feed(&mut p, b"K\x1b\\q"), vec![Event::Reply { id: 2, ok: true }, Event::Key(b'q')]);
    }

    #[test]
    fn lone_escape_is_the_escape_key_after_a_quiet_poll() {
        let mut p = InputParser::default();
        assert!(feed(&mut p, b"\x1b").is_empty());
        assert_eq!(feed(&mut p, b""), vec![Event::Escape]);
        // arrow keys are not Escape
        assert!(feed(&mut p, b"\x1b[A\x1bOB").is_empty());
    }
}
