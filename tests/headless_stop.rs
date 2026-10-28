//! Headless Ctrl+C against a private X server.
//!
//! The click loop itself is covered without a display. This checks the signal
//! handler, which only exists in the running binary: the first Ctrl+C has to
//! stop the session and print the summary instead of killing the process.

#![cfg(unix)]

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Parallel tests each need their own server. Numbers only move forward.
static NEXT_DISPLAY: AtomicU32 = AtomicU32::new(80);

struct PrivateDisplay {
    child: Child,
    name: String,
}

impl PrivateDisplay {
    fn start() -> Option<Self> {
        for _ in 0..1000 {
            let number = NEXT_DISPLAY.fetch_add(1, Ordering::Relaxed);
            let name = format!(":{number}");
            let socket = format!("/tmp/.X11-unix/X{number}");
            if Path::new(&socket).exists() {
                continue;
            }
            let mut child = Command::new("Xvfb")
                .args([
                    &name,
                    "-screen",
                    "0",
                    "640x480x24",
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
                    // The socket can appear a moment before the server accepts.
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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_pipes(child: &mut Child) -> (String, String) {
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(pipe) = child.stdout.as_mut() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(pipe) = child.stderr.as_mut() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    (stdout, stderr)
}

fn xvfb_installed() -> bool {
    Command::new("Xvfb")
        .arg("-help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

#[test]
fn ctrl_c_stops_a_headless_run_and_prints_the_summary() {
    if !xvfb_installed() {
        eprintln!("skipping Ctrl+C test: Xvfb is not installed");
        return;
    }
    let display = PrivateDisplay::start().expect("Xvfb did not open a display");

    let mut child = Command::new(env!("CARGO_BIN_EXE_featherclick"))
        .args([
            "--headless",
            "--seconds",
            "30",
            "--interval",
            "20",
            "--at",
            "40,40",
            "--quiet",
        ])
        .env("DISPLAY", &display.name)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("XAUTHORITY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn featherclick");

    let started = Instant::now();
    // Long enough to pass connect and enter the click loop, short enough that
    // a missed signal still has the 30s cap behind it.
    thread::sleep(Duration::from_millis(500));
    match child.try_wait() {
        Ok(Some(status)) => {
            let (stdout, stderr) = read_pipes(&mut child);
            panic!("exited before SIGINT ({status})\nstdout {stdout}\nstderr {stderr}");
        }
        Ok(None) => {}
        Err(err) => panic!("could not poll featherclick: {err}"),
    }

    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(killed.success(), "kill -INT failed");

    let output = child.wait_with_output().expect("wait for featherclick");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status {:?}\nstdout {stdout}\nstderr {stderr}",
        output.status
    );
    assert!(
        stdout.lines().any(|line| line.ends_with(" - stopped")),
        "expected a user stop, got {stdout}{stderr}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "SIGINT left the 30s run going for {:?}",
        started.elapsed()
    );
}

#[test]
fn fixed_target_stop_on_move_ignores_the_clickers_own_placement() {
    if !xvfb_installed() {
        eprintln!("skipping stop-on-move test: Xvfb is not installed");
        return;
    }
    let display = PrivateDisplay::start().expect("Xvfb did not open a display");
    let root = std::env::temp_dir().join(format!(
        "featherclick-stop-{}-{}",
        std::process::id(),
        display.name.trim_start_matches(':')
    ));
    let dir = root.join("featherclick");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        r#"{
            "timing": "fixed",
            "interval_ms": 20,
            "position": "fixed",
            "point": [40, 40],
            "jitter": "off",
            "max_clicks": 4,
            "max_seconds": 0,
            "stop_on_move_px": 5
        }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_featherclick"))
        .args(["--headless", "--quiet"])
        .env("DISPLAY", &display.name)
        .env("XDG_CONFIG_HOME", &root)
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("XAUTHORITY")
        .output()
        .expect("spawn featherclick");
    let _ = std::fs::remove_dir_all(&root);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status {:?}\nstdout {stdout}\nstderr {stderr}",
        output.status
    );
    assert!(
        stdout.contains("4 clicks") && stdout.contains("click limit reached"),
        "placing the pointer on a fixed target must not count as a move: {stdout}{stderr}"
    );
}
