//! `gavno-cam`: the finger pointer helper. The compositor starts it with a
//! private socket (`GAVNO_CAM_FD`) while the mode is on and stops it with
//! SIGTERM; frames never leave this process.
//!
//! `gavno-cam --probe [seconds]` prints only frame rate, inference time,
//! hand confidence and fingertip coordinates.

use gavno_cam::{
    camera::Camera,
    gesture::{Config, Engine, Landmarks, INDEX_TIP},
    models::{self, Hand, Models},
    protocol::Message,
    vision,
};
use std::{
    os::fd::{FromRawFd, OwnedFd, RawFd},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

static TERMINATE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    TERMINATE.store(true, Ordering::Relaxed);
}

/// Gives up after this long without a hand.
const IDLE_EXIT: Duration = Duration::from_secs(60);
/// With no hand for this long the camera branch drops to `IDLE_FPS`.
const IDLE_SLOW: Duration = Duration::from_secs(2);
const ACTIVE_FPS: i32 = 30;
const IDLE_FPS: i32 = 6;
const CONFIDENCE: f32 = 0.5;

fn setup_process() {
    // SAFETY: plain libc calls with valid arguments; the handler only stores
    // to an atomic.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
        // The parent may already be gone.
        if libc::getppid() == 1 { TERMINATE.store(true, Ordering::Relaxed); }
    }
}

fn env_f32(name: &str) -> Option<f32> {
    std::env::var(name).ok()?.parse().ok()
}

fn config(width: usize, height: usize) -> Config {
    let mut cfg = Config { aspect: width as f32 / height as f32, ..Config::default() };
    if let Some((w, h)) = std::env::var("GAVNO_CAM_OUTPUT").ok().and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?))
    }) {
        if w >= 1.0 && h >= 1.0 { (cfg.out_w, cfg.out_h) = (w, h); }
    }
    if let Some(percent) = env_f32("GAVNO_CAM_BOX") { cfg.active_box = (percent / 100.0).clamp(0.3, 1.0); }
    if let Some(v) = env_f32("GAVNO_CAM_MIN_CUTOFF") { cfg.min_cutoff = v.clamp(0.01, 30.0); }
    if let Some(v) = env_f32("GAVNO_CAM_BETA") { cfg.beta = v.clamp(0.0, 10.0); }
    cfg
}

/// Gray-world white balance and exposure. The laptop's uncalibrated software
/// ISP delivers a dark, blue frame in which the models do not see skin.
struct Balance {
    gains: [f32; 3],
    frames: u32,
}

impl Balance {
    const TARGET: f32 = 118.0;
    fn new() -> Self { Self { gains: [1.0; 3], frames: 0 } }

    /// Channel gains that bring every channel's mean to the target gray.
    fn measure(rgb: &[u8]) -> [f32; 3] {
        let mut sum = [0u64; 3];
        let mut n = 0u64;
        for px in rgb.chunks_exact(3).step_by(17) {
            for c in 0..3 { sum[c] += px[c] as u64; }
            n += 1;
        }
        let mut gains = [1.0; 3];
        if n > 0 {
            for c in 0..3 {
                let mean = (sum[c] as f32 / n as f32).max(1.0);
                gains[c] = (Self::TARGET / mean).clamp(0.5, 4.0);
            }
        }
        gains
    }

    fn apply(&mut self, rgb: &[u8], out: &mut Vec<u8>) {
        if self.frames % 8 == 0 {
            let target = Self::measure(rgb);
            for c in 0..3 {
                // Smooth so the image does not pump when a hand enters.
                self.gains[c] += (target[c] - self.gains[c]) * if self.frames == 0 { 1.0 } else { 0.5 };
            }
        }
        self.frames = self.frames.wrapping_add(1);
        let lut: [[u8; 256]; 3] = std::array::from_fn(|c| std::array::from_fn(|v| (v as f32 * self.gains[c]).min(255.0) as u8));
        out.clear();
        out.extend(rgb.chunks_exact(3).flat_map(|px| [lut[0][px[0] as usize], lut[1][px[1] as usize], lut[2][px[2] as usize]]));
    }
}

/// Palm detection while nothing is tracked, then the landmark network on the
/// region predicted from the previous frame.
struct Tracker {
    models: Models,
    balance: Balance,
    corrected: Vec<u8>,
    roi: Option<vision::Roi>,
    palm_ms: f32,
    hand_ms: f32,
}

impl Tracker {
    fn step(&mut self, raw: &[u8], width: usize, height: usize) -> Result<Option<Hand>, String> {
        self.balance.apply(raw, &mut self.corrected);
        let rgb = std::mem::take(&mut self.corrected);
        let result = self.track(&rgb, width, height);
        self.corrected = rgb;
        result
    }

    fn track(&mut self, rgb: &[u8], width: usize, height: usize) -> Result<Option<Hand>, String> {
        let mut palm_ms = 0.0f32;
        if self.roi.is_none() {
            let started = Instant::now();
            let palm = self.models.detect_palm(rgb, width, height)?;
            palm_ms = started.elapsed().as_secs_f32() * 1000.0;
            self.roi = palm.map(|p| vision::roi_from_palm(&p));
        }
        let Some(roi) = self.roi else { self.palm_ms = palm_ms; return Ok(None) };
        let started = Instant::now();
        let hand = self.models.landmarks(rgb, width, height, roi)?;
        let hand_ms = started.elapsed().as_secs_f32() * 1000.0;
        self.palm_ms = palm_ms;
        self.hand_ms = hand_ms;
        if hand.confidence < CONFIDENCE {
            self.roi = None;
            return Ok(None);
        }
        self.roi = Some(vision::roi_from_landmarks(&hand.landmarks));
        Ok(Some(hand))
    }
}

fn normalized(hand: &Hand, width: usize, height: usize) -> Landmarks {
    let mut lm = [[0.0; 2]; 21];
    for (out, p) in lm.iter_mut().zip(&hand.landmarks) {
        *out = [p[0] / width as f32, p[1] / height as f32];
    }
    lm
}

fn open_socket() -> Option<RawFd> {
    let fd: RawFd = std::env::var("GAVNO_CAM_FD").ok()?.parse().ok()?;
    // SAFETY: the compositor passes an open socket it no longer uses.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    Some(std::os::fd::IntoRawFd::into_raw_fd(owned))
}

/// Longest a button, scroll or hand message waits for a full socket.
const SEND_WAIT: Duration = Duration::from_millis(250);

/// Returns false when the peer is gone. Only motion is dropped on a full
/// socket; every other message is delivered, waiting briefly for room.
fn send(fd: RawFd, messages: &[Message]) -> bool {
    let mut text = String::new();
    for message in messages {
        text.clear();
        message.encode(&mut text);
        let droppable = matches!(message, Message::Motion { .. });
        if !write_line(fd, text.as_bytes(), droppable, SEND_WAIT) {
            return false;
        }
    }
    true
}

/// Writes one whole line. A droppable line is skipped when nothing could be
/// written at once; a started line is always completed so framing holds.
/// Returns false only when the peer is gone.
fn write_line(fd: RawFd, line: &[u8], droppable: bool, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    let mut done = 0;
    while done < line.len() {
        let rest = &line[done..];
        // SAFETY: valid buffer and descriptor.
        let n = unsafe { libc::send(fd, rest.as_ptr().cast(), rest.len(), libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) };
        if n >= 0 {
            done += n as usize;
            continue;
        }
        match std::io::Error::last_os_error().kind() {
            std::io::ErrorKind::Interrupted => {}
            std::io::ErrorKind::WouldBlock => {
                if done == 0 && droppable {
                    return true;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() && done == 0 {
                    // The compositor is not reading; losing this one is better than hanging.
                    return true;
                }
                // A half-written line must be finished, so allow it extra time.
                let left = if left.is_zero() { SEND_WAIT } else { left };
                let mut pfd = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
                // SAFETY: one valid pollfd.
                unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
                if pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn run(probe: Option<Duration>) -> Result<(), String> {
    let dir = models::model_dir().ok_or("models not found (set GAVNO_CAM_MODELS)")?;
    let mut tracker = Tracker { models: Models::load(&dir)?, balance: Balance::new(), corrected: Vec::new(), roi: None, palm_ms: 0.0, hand_ms: 0.0 };
    let socket = if probe.is_none() { Some(open_socket().ok_or("GAVNO_CAM_FD is not a socket")?) } else { None };
    let camera = Camera::open()?;
    let started = Instant::now();
    let mut engine: Option<Engine> = None;
    let mut out = Vec::new();
    let mut last_hand = Instant::now();
    let mut slow = false;
    let mut stats = Stats::default();
    if probe.is_some() { eprintln!("pipeline: {}", camera.description); }
    while !TERMINATE.load(Ordering::Relaxed) {
        if probe.is_some_and(|d| started.elapsed() >= d) { break; }
        if probe.is_none() && last_hand.elapsed() >= IDLE_EXIT { break; }
        let idle = last_hand.elapsed() >= IDLE_SLOW;
        if idle != slow {
            slow = idle;
            camera.set_max_rate(if idle { IDLE_FPS } else { ACTIVE_FPS });
        }
        let result = camera.pull(Duration::from_millis(100), |frame| {
            let hand = tracker.step(frame.rgb, frame.width, frame.height)?;
            Ok::<_, String>((frame.width, frame.height, hand))
        });
        let Some(result) = result else { continue };
        let (width, height, hand) = result?;
        stats.frame(&tracker);
        if hand.is_some() { last_hand = Instant::now(); }
        let t = started.elapsed().as_secs_f64();
        let lm = hand.as_ref().map(|h| normalized(h, width, height));
        if probe.is_some() {
            if let Some(lm) = &lm { stats.tip = Some(lm[INDEX_TIP]); }
            stats.confidence = hand.as_ref().map_or(0.0, |h| h.confidence);
            stats.report(started.elapsed());
        }
        let engine = engine.get_or_insert_with(|| Engine::new(config(width, height)));
        out.clear();
        engine.update(t, lm.as_ref(), &mut out);
        if let Some(fd) = socket {
            if !out.is_empty() && !send(fd, &out) { break; }
        }
    }
    if let (Some(fd), Some(engine)) = (socket, engine.as_mut()) {
        out.clear();
        engine.release(&mut out);
        send(fd, &out);
    }
    if probe.is_some() { stats.summary(started.elapsed()); }
    Ok(())
}

#[derive(Default)]
struct Stats {
    frames: u32,
    window_frames: u32,
    window_start: Option<Instant>,
    palm_ms: Vec<f32>,
    hand_ms: Vec<f32>,
    confidence: f32,
    tip: Option<[f32; 2]>,
    cpu_start: Option<f64>,
}

fn cpu_seconds() -> f64 {
    // SAFETY: zeroed rusage is a valid out parameter.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 / 1e6;
    t(usage.ru_utime) + t(usage.ru_stime)
}

fn rss_mib() -> f64 {
    std::fs::read_to_string("/proc/self/status").ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("VmRSS:").and_then(|v| v.trim().trim_end_matches(" kB").trim().parse::<f64>().ok())))
        .map_or(0.0, |kb| kb / 1024.0)
}

fn mean(v: &[f32]) -> f32 { if v.is_empty() { 0.0 } else { v.iter().sum::<f32>() / v.len() as f32 } }

impl Stats {
    fn frame(&mut self, tracker: &Tracker) {
        self.cpu_start.get_or_insert_with(cpu_seconds);
        self.window_start.get_or_insert_with(Instant::now);
        self.frames += 1;
        self.window_frames += 1;
        if tracker.palm_ms > 0.0 { self.palm_ms.push(tracker.palm_ms); }
        if tracker.hand_ms > 0.0 { self.hand_ms.push(tracker.hand_ms); }
    }
    fn report(&mut self, _elapsed: Duration) {
        let Some(start) = self.window_start else { return };
        if start.elapsed() < Duration::from_secs(1) { return; }
        let fps = self.window_frames as f32 / start.elapsed().as_secs_f32();
        let tip = self.tip.map_or("-".into(), |t| format!("({:.3}, {:.3})", t[0], t[1]));
        println!("fps {fps:.1}  palm {:.1} ms  landmarks {:.1} ms  confidence {:.2}  index tip {tip}",
            mean(&self.palm_ms), mean(&self.hand_ms), self.confidence);
        self.window_frames = 0;
        self.window_start = Some(Instant::now());
    }
    fn summary(&self, elapsed: Duration) {
        let cpu = cpu_seconds() - self.cpu_start.unwrap_or(0.0);
        println!("summary: {} analysed frames in {:.1} s, palm {:.1} ms (n={}), landmarks {:.1} ms (n={}), process CPU {:.0}% of one core, RSS {:.0} MiB",
            self.frames, elapsed.as_secs_f32(), mean(&self.palm_ms), self.palm_ms.len(), mean(&self.hand_ms), self.hand_ms.len(),
            cpu / elapsed.as_secs_f64() * 100.0, rss_mib());
    }
}

enum Probe { Raw(Vec<String>) }

/// Developer check without a camera: `--probe-raw WIDTH HEIGHT FILE` runs the
/// networks on one raw RGB8 frame and prints the result.
fn probe_raw(args: &[String]) -> Result<(), String> {
    let (w, h, file, bench) = match args {
        [w, h, file] => (w, h, file, None),
        [w, h, file, secs] => (w, h, file, Some(secs.parse::<f64>().map_err(|_| "bad seconds")?)),
        _ => return Err("usage: --probe-raw WIDTH HEIGHT FILE [PACED_SECONDS]".into()),
    };
    let (w, h): (usize, usize) = (w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?);
    let rgb = std::fs::read(file).map_err(|e| e.to_string())?;
    if rgb.len() != w * h * 3 { return Err("file size does not match WIDTH x HEIGHT x 3".into()); }
    let dir = models::model_dir().ok_or("models not found (set GAVNO_CAM_MODELS)")?;
    let mut tracker = Tracker { models: Models::load(&dir)?, balance: Balance::new(), corrected: Vec::new(), roi: None, palm_ms: 0.0, hand_ms: 0.0 };
    if let Some(seconds) = bench {
        // Tracked-hand cost: the same frame at 30 fps, landmarks every frame.
        let (start, cpu) = (Instant::now(), cpu_seconds());
        let mut frames = 0;
        let mut worst = 0.0f32;
        while start.elapsed().as_secs_f64() < seconds {
            let frame = Instant::now();
            tracker.step(&rgb, w, h)?;
            worst = worst.max(frame.elapsed().as_secs_f32() * 1000.0);
            frames += 1;
            if let Some(rest) = Duration::from_millis(33).checked_sub(frame.elapsed()) { std::thread::sleep(rest); }
        }
        println!("paced 30 fps: {frames} frames, landmarks {:.1} ms (worst frame {worst:.1} ms), CPU {:.0}% of one core, RSS {:.0} MiB",
            tracker.hand_ms, (cpu_seconds() - cpu) / start.elapsed().as_secs_f64() * 100.0, rss_mib());
        return Ok(());
    }
    for pass in 0..3 {
        let started = Instant::now();
        let hand = tracker.step(&rgb, w, h)?;
        println!("pass {pass}: {:.1} ms, palm {:.1} ms, landmarks {:.1} ms", started.elapsed().as_secs_f32() * 1000.0, tracker.palm_ms, tracker.hand_ms);
        match hand {
            Some(hand) => {
                let lm = normalized(&hand, w, h);
                println!("  confidence {:.3}, wrist ({:.3}, {:.3}), index tip ({:.3}, {:.3}), thumb tip ({:.3}, {:.3})",
                    hand.confidence, lm[0][0], lm[0][1], lm[8][0], lm[8][1], lm[4][0], lm[4][1]);
            }
            None => println!("  no hand"),
        }
    }
    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut raw = None;
    let probe = match args.next().as_deref() {
        Some("--probe-raw") => { raw = Some(Probe::Raw(args.collect())); None }
        Some("--probe") => Some(Duration::from_secs(args.next().and_then(|s| s.parse().ok()).unwrap_or(10))),
        None => None,
        Some(other) => {
            eprintln!("usage: gavno-cam [--probe [seconds]]  (unknown argument {other})");
            std::process::exit(64);
        }
    };
    setup_process();
    if let Some(Probe::Raw(args)) = raw {
        if let Err(error) = probe_raw(&args) {
            eprintln!("gavno-cam: {error}");
            std::process::exit(2);
        }
        return;
    }
    if let Err(error) = run(probe) {
        eprintln!("gavno-cam: {error}");
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn balance_neutralises_a_dark_blue_frame() {
        let frame: Vec<u8> = (0..640 * 360).flat_map(|_| [44u8, 55, 100]).collect();
        let mut balance = super::Balance::new();
        let mut out = Vec::new();
        balance.apply(&frame, &mut out);
        let px = &out[..3];
        assert!(px.iter().all(|&v| (110..=125).contains(&v)), "{px:?}");
        assert_eq!(out.len(), frame.len());
    }

    use super::*;
    use std::os::{fd::AsRawFd, unix::net::UnixStream};

    fn fill(stream: &UnixStream) {
        let chunk = [b'x'; 4096];
        loop {
            // SAFETY: valid buffer and descriptor.
            let n = unsafe { libc::send(stream.as_raw_fd(), chunk.as_ptr().cast(), chunk.len(), libc::MSG_DONTWAIT) };
            if n < 0 { break; }
        }
    }

    #[test]
    fn a_full_socket_drops_motion_but_delivers_buttons() {
        let (tx, rx) = UnixStream::pair().unwrap();
        fill(&tx);
        let fd = tx.as_raw_fd();
        assert!(send(fd, &[Message::Motion { x: 0.5, y: 0.5 }]));
        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let mut rx = rx;
            let mut all = Vec::new();
            let mut buf = [0u8; 8192];
            rx.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
            while let Ok(n) = std::io::Read::read(&mut rx, &mut buf) {
                if n == 0 { break; }
                all.extend_from_slice(&buf[..n]);
            }
            all
        });
        assert!(send(fd, &[Message::Button { right: false, pressed: true }]));
        let all = reader.join().unwrap();
        let text = String::from_utf8_lossy(&all);
        assert!(text.contains("B l 1\n"), "button must arrive");
        assert!(!text.contains("M 0.50000"), "dropped motion must not arrive");
    }

    #[test]
    fn a_closed_peer_is_reported() {
        let (tx, rx) = UnixStream::pair().unwrap();
        drop(rx);
        assert!(!send(tx.as_raw_fd(), &[Message::Hand { present: true }]));
    }
}
