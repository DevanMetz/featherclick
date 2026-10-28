//! The settings file is process-wide state. These tests point a child process
//! at a temporary directory so they cannot touch a real config.
//!
//! Windows stores that path in a known folder the process cannot redirect, so
//! the check runs on Unix only. The decision itself is covered by unit tests
//! on every platform.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn headless_exits_when_the_settings_file_does_not_parse() {
    let home = scratch();
    let file = config_file(&home);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let raw = b"{not json\n";
    std::fs::write(&file, raw).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_featherclick"));
    command
        .env("HOME", &home)
        .args([
            "--headless",
            "--clicks",
            "5",
            "--interval",
            "1000",
            "--quiet",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Linux prefers this over `$HOME/.config` when it is absolute.
    #[cfg(target_os = "linux")]
    command.env(
        "XDG_CONFIG_HOME",
        home.join("config").canonicalize().unwrap(),
    );

    let output = output_with_timeout(&mut command, Duration::from_secs(10));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("cannot parse") && stderr.contains("config.json"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("cannot simulate input"),
        "a broken file must be rejected before any click attempt: {stderr}"
    );
    assert_eq!(std::fs::read(&file).unwrap(), raw);
    assert!(!file.with_file_name("config.json.bak").exists());
    assert!(!file.with_file_name("config.json.tmp").exists());

    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(target_os = "linux")]
fn config_file(home: &std::path::Path) -> PathBuf {
    home.join("config/featherclick/config.json")
}

#[cfg(target_os = "macos")]
fn config_file(home: &std::path::Path) -> PathBuf {
    home.join("Library/Application Support/featherclick/config.json")
}

fn output_with_timeout(command: &mut Command, limit: Duration) -> std::process::Output {
    let mut child = command.spawn().unwrap();
    let started = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            panic!("featherclick did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

fn scratch() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("featherclick-run-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}
