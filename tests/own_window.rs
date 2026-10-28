//! Clicking Start must not click the FeatherClick window.
//!
//! Pointer mode leaves the pointer on Start. The next synthetic click used to
//! land on Stop, and the run ended after that one click. A catcher beside the
//! window stays silent while the pointer is over FeatherClick and starts
//! receiving clicks once the pointer moves onto it. Fixed mode still delivers
//! clicks to a point the window does not cover.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Parallel tests each need their own server. Numbers only move forward, and
/// they sit above the displays other suites in this repo have used.
static NEXT_DISPLAY: AtomicU32 = AtomicU32::new(140);

const SCREEN_W: i32 = 1280;
const SCREEN_H: i32 = 800;
const CATCHER: i32 = 160;
/// Distance from the window origin to a point on the Start button. Measured
/// against an undecorated Xvfb window: the content origin is the window origin.
const START_OFFSET_Y: i32 = 90;

struct PrivateDisplay {
    child: Child,
    name: String,
}

impl PrivateDisplay {
    fn start() -> Option<Self> {
        for _ in 0..10_000 {
            let number = NEXT_DISPLAY.fetch_add(1, Ordering::Relaxed);
            let name = format!(":{number}");
            let socket = format!("/tmp/.X11-unix/X{number}");
            let lock = format!("/tmp/.X{number}-lock");
            if Path::new(&socket).exists() || Path::new(&lock).exists() {
                continue;
            }
            let mut child = Command::new("Xvfb")
                .args([
                    &name,
                    "-screen",
                    "0",
                    "1280x800x24",
                    "-nolisten",
                    "tcp",
                    "-ac",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if Path::new(&socket).exists() {
                    thread::sleep(Duration::from_millis(100));
                    return Some(Self { child, name });
                }
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        None
    }
}

impl Drop for PrivateDisplay {
    fn drop(&mut self) {
        // Let Xvfb remove its socket and lock before reaping it.
        // SAFETY: the child is still owned here and its PID identifies Xvfb.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

fn program_exists(program: &str, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

#[derive(Clone, Copy, Debug)]
struct Geom {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Geom {
    fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }

    fn overlaps(self, other: Geom) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }
}

fn xdotool(display: &str, args: &[&str]) -> std::process::Output {
    Command::new("xdotool")
        .args(args)
        .env("DISPLAY", display)
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap_or_else(|err| panic!("xdotool {args:?} failed to start: {err}"))
}

fn search_window(display: &str) -> String {
    let output = Command::new("timeout")
        .args([
            "15",
            "xdotool",
            "search",
            "--sync",
            "--onlyvisible",
            "--name",
            "FeatherClick",
        ])
        .env("DISPLAY", display)
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("search for the FeatherClick window");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "FeatherClick window did not appear\n{stdout}{stderr}"
    );
    let mut best_area = i64::MIN;
    let mut best_id = None;
    for id in stdout.split_whitespace() {
        let geom = geometry(display, id);
        let area = geom.width as i64 * geom.height as i64;
        if area > best_area {
            best_area = area;
            best_id = Some(id.to_string());
        }
    }
    best_id.expect("xdotool returned no window id")
}

fn geometry(display: &str, id: &str) -> Geom {
    let output = xdotool(display, &["getwindowgeometry", "--shell", id]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "getwindowgeometry failed: {text}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut geom = Geom {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Ok(value) = value.trim().parse() else {
            continue;
        };
        match key {
            "X" => geom.x = value,
            "Y" => geom.y = value,
            "WIDTH" => geom.width = value,
            "HEIGHT" => geom.height = value,
            _ => {}
        }
    }
    assert!(geom.width > 0 && geom.height > 0, "empty geometry: {text}");
    geom
}

fn mouse_move(display: &str, x: i32, y: i32) {
    let output = xdotool(
        display,
        &["mousemove", "--sync", &x.to_string(), &y.to_string()],
    );
    assert!(
        output.status.success(),
        "mousemove failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn mouse_click(display: &str) {
    let output = xdotool(display, &["click", "1"]);
    assert!(
        output.status.success(),
        "click failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn window_move(display: &str, id: &str, x: i32, y: i32) {
    let output = xdotool(display, &["windowmove", id, &x.to_string(), &y.to_string()]);
    assert!(
        output.status.success(),
        "windowmove failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Catcher {
    child: Child,
    clicks: Arc<AtomicU32>,
    ready: Arc<AtomicU32>,
}

impl Catcher {
    fn spawn(display: &str, bin: &Path, x: i32, y: i32) -> Self {
        let mut child = Command::new(bin)
            .args([
                x.to_string(),
                y.to_string(),
                CATCHER.to_string(),
                CATCHER.to_string(),
            ])
            .env("DISPLAY", display)
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn click catcher");
        let stdout = child.stdout.take().expect("catcher stdout");
        let clicks = Arc::new(AtomicU32::new(0));
        let ready = Arc::new(AtomicU32::new(0));
        let clicks_thread = Arc::clone(&clicks);
        let ready_thread = Arc::clone(&ready);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                match line.as_str() {
                    "ready" => ready_thread.store(1, Ordering::Relaxed),
                    "click" => {
                        clicks_thread.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        });
        Self {
            child,
            clicks,
            ready,
        }
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.ready.load(Ordering::Relaxed) == 1 {
                return;
            }
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("click catcher did not map a window");
    }

    fn clicks(&self) -> u32 {
        self.clicks.load(Ordering::Relaxed)
    }
}

impl Drop for Catcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Gui {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl Gui {
    fn spawn(display: &str, home: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_featherclick"))
            .env("DISPLAY", display)
            .env("WINIT_UNIX_BACKEND", "x11")
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("XAUTHORITY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn featherclick");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        Self {
            child,
            stdout: drain(stdout),
            stderr: drain(stderr),
        }
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn output(&self) -> String {
        let stdout = self.stdout.lock().unwrap_or_else(|err| err.into_inner());
        let stderr = self.stderr.lock().unwrap_or_else(|err| err.into_inner());
        format!("stdout:\n{stdout}\nstderr:\n{stderr}")
    }
}

impl Drop for Gui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn drain(pipe: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let text = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&text);
    thread::spawn(move || {
        let mut reader = pipe;
        let mut buf = [0_u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    sink.lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .push_str(&chunk);
                }
            }
        }
    });
    text
}

fn write_config(home: &Path, body: &str) {
    let dir = home.join(".config/featherclick");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), body).unwrap();
}

fn compile_catcher(dest: &Path) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/click_catcher.c");
    let output = Command::new("cc")
        .args(["-O2", "-o"])
        .arg(dest)
        .arg(&source)
        .arg("-lX11")
        .output()
        .unwrap_or_else(|err| panic!("cc failed to start: {err}"));
    assert!(
        output.status.success(),
        "cc failed\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A spot that does not touch the FeatherClick window, preferring the right.
fn catcher_origin(window: Geom) -> (i32, i32) {
    let gap = 30;
    let candidates = [
        (window.x + window.width + gap, window.y.max(0)),
        (window.x - gap - CATCHER, window.y.max(0)),
        (window.x.max(0), window.y + window.height + gap),
        (window.x.max(0), window.y - gap - CATCHER),
    ];
    for (x, y) in candidates {
        let catcher = Geom {
            x,
            y,
            width: CATCHER,
            height: CATCHER,
        };
        if x >= 0
            && y >= 0
            && x + CATCHER <= SCREEN_W
            && y + CATCHER <= SCREEN_H
            && !window.overlaps(catcher)
        {
            return (x, y);
        }
    }
    panic!("no room for a catcher beside {window:?}");
}

fn require_display_tools() -> Option<PrivateDisplay> {
    if !program_exists("Xvfb", "-help") {
        eprintln!("skipping own-window test: Xvfb is not installed");
        return None;
    }
    assert!(
        program_exists("xdotool", "--version"),
        "xdotool is required when Xvfb is installed"
    );
    assert!(
        program_exists("cc", "--version"),
        "cc is required when Xvfb is installed"
    );
    Some(PrivateDisplay::start().expect("Xvfb did not open a display"))
}

fn temp_home(label: &str, display: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "featherclick-own-{label}-{}-{}",
        std::process::id(),
        display.trim_start_matches(':')
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

struct Scratch {
    home: PathBuf,
    catcher_bin: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_file(&self.catcher_bin);
    }
}

fn click_start(display: &str, window: Geom) {
    let x = window.x + window.width / 2;
    let y = window.y + START_OFFSET_Y;
    mouse_move(display, x, y);
    // The motion wakes a frame, which publishes the window rectangle before
    // the click that would otherwise hit Stop.
    thread::sleep(Duration::from_millis(250));
    mouse_click(display);
}

#[test]
fn pointer_mode_waits_on_the_window_then_clicks_where_the_pointer_moves() {
    let Some(display) = require_display_tools() else {
        return;
    };
    let scratch = Scratch {
        home: temp_home("pointer", &display.name),
        catcher_bin: std::env::temp_dir().join(format!(
            "featherclick-catcher-pointer-{}-{}",
            std::process::id(),
            display.name.trim_start_matches(':')
        )),
    };
    write_config(
        &scratch.home,
        r#"{
            "timing": "fixed",
            "interval_ms": 30,
            "position": "cursor",
            "jitter": "off",
            "max_clicks": 20,
            "max_seconds": 5,
            "stop_on_move_px": 0,
            "start_delay_ms": 0,
            "hotkey_toggle": "",
            "hotkey_capture": ""
        }"#,
    );
    compile_catcher(&scratch.catcher_bin);

    let mut gui = Gui::spawn(&display.name, &scratch.home);
    let id = search_window(&display.name);
    assert!(
        gui.alive(),
        "featherclick exited before the test\n{}",
        gui.output()
    );
    let window = geometry(&display.name, &id);
    let (x, y) = catcher_origin(window);
    let mut catcher = Catcher::spawn(&display.name, &scratch.catcher_bin, x, y);
    catcher.wait_ready();
    let catcher_geom = Geom {
        x,
        y,
        width: CATCHER,
        height: CATCHER,
    };
    assert!(
        !window.overlaps(catcher_geom),
        "catcher {catcher_geom:?} overlaps window {window:?}"
    );

    click_start(&display.name, window);
    thread::sleep(Duration::from_millis(400));
    assert!(
        gui.alive(),
        "featherclick exited while the pointer was over its window\n{}",
        gui.output()
    );
    assert_eq!(
        catcher.clicks(),
        0,
        "clicks reached the catcher while the pointer was still on the window\n{}",
        gui.output()
    );

    mouse_move(
        &display.name,
        catcher_geom.x + CATCHER / 2,
        catcher_geom.y + CATCHER / 2,
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && catcher.clicks() < 5 {
        thread::sleep(Duration::from_millis(30));
    }
    assert!(
        gui.alive(),
        "featherclick exited after the pointer left the window\n{}",
        gui.output()
    );
    assert!(
        catcher.clicks() >= 5,
        "expected clicks on the catcher after the pointer moved, got {}\nwindow {window:?}\n{}",
        catcher.clicks(),
        gui.output()
    );
}

#[test]
fn fixed_point_away_from_the_window_still_receives_clicks() {
    let Some(display) = require_display_tools() else {
        return;
    };
    let scratch = Scratch {
        home: temp_home("fixed", &display.name),
        catcher_bin: std::env::temp_dir().join(format!(
            "featherclick-catcher-fixed-{}-{}",
            std::process::id(),
            display.name.trim_start_matches(':')
        )),
    };
    // The point stays outside both a window parked at the origin and one moved
    // to (40, 40), so a stale rectangle cannot swallow the target.
    let point = (1100, 160);
    write_config(
        &scratch.home,
        r#"{
            "timing": "fixed",
            "interval_ms": 30,
            "position": "fixed",
            "point": [1100, 160],
            "jitter": "off",
            "max_clicks": 20,
            "max_seconds": 5,
            "stop_on_move_px": 0,
            "start_delay_ms": 0,
            "hotkey_toggle": "",
            "hotkey_capture": ""
        }"#,
    );
    compile_catcher(&scratch.catcher_bin);

    let gui = Gui::spawn(&display.name, &scratch.home);
    let id = search_window(&display.name);
    window_move(&display.name, &id, 40, 40);
    thread::sleep(Duration::from_millis(200));
    let window = geometry(&display.name, &id);
    assert!(
        !window.contains(point.0, point.1),
        "window {window:?} covers the fixed point"
    );

    let origin = (point.0 - CATCHER / 2, point.1 - CATCHER / 2);
    let mut catcher = Catcher::spawn(&display.name, &scratch.catcher_bin, origin.0, origin.1);
    catcher.wait_ready();
    click_start(&display.name, window);

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && catcher.clicks() < 5 {
        thread::sleep(Duration::from_millis(30));
    }
    assert!(
        catcher.clicks() >= 5,
        "fixed point {} ,{} delivered {} clicks\nwindow {window:?}\n{}",
        point.0,
        point.1,
        catcher.clicks(),
        gui.output()
    );
}
