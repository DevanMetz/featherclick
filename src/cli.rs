//! Command line handling.
//!
//! The window is the default. Any run flag switches to `--headless`, which
//! drives the same engine with no GUI: that is what the CI smoke test and
//! scripts use, and it keeps one click loop rather than two.

use std::sync::mpsc::channel;
use std::time::Instant;

use crate::config::{self, JitterMode, MouseButton, PositionMode, Settings, TimingMode};
use crate::engine::{self, SessionEvent, StopReason};

pub const USAGE: &str = "\
FeatherClick - a tiny cross-platform autoclicker

USAGE:
    featherclick [OPTIONS]

With no options the settings window opens. Any run option clicks without opening
a window, using the saved settings as defaults. A limit of 0 means unlimited;
passing --clicks or --seconds replaces both saved limits.

RUN OPTIONS:
        --headless              click without opening a window
        --clicks <N>            stop after N clicks
        --seconds <S>           stop after S seconds
        --interval <MS>         click every MS milliseconds
        --at <X,Y>              click a fixed screen point
        --jitter <PX>           vary the point by up to +-PX pixels
        --button <left|right|middle>
        --delay <MS>            wait before the first click; the time limit starts afterwards
        --quiet                 print only the final summary

OTHER OPTIONS:
    -h, --help                  print this help
    -V, --version               print the version

Exit status is 0 on success (including stop conditions and Ctrl+C), 1 on a
runtime error, and 2 for invalid arguments. A second Ctrl+C exits immediately.";

#[derive(Debug, Default)]
pub struct Args {
    pub help: bool,
    pub version: bool,
    pub run: Option<Run>,
}

/// Overrides for a headless run. `None` keeps the saved setting.
#[derive(Debug, Default)]
pub struct Run {
    clicks: Option<u64>,
    seconds: Option<u32>,
    interval_ms: Option<u32>,
    at: Option<(i32, i32)>,
    jitter_px: Option<u32>,
    button: Option<MouseButton>,
    delay_ms: Option<u32>,
    quiet: bool,
}

pub fn parse(raw: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let args: Vec<String> = raw.into_iter().collect();
    let mut parsed = Args::default();
    let mut run = Run::default();
    let mut seen_run_flag = false;
    let mut index = 0;

    while index < args.len() {
        let arg = args[index].clone();
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name.to_string(), Some(value.to_string())),
            None => (arg.clone(), None),
        };
        index += 1;

        // Pulls the value from `--flag=value` or the following argument.
        macro_rules! value {
            () => {
                match inline.clone() {
                    Some(value) => value,
                    None => {
                        let value = args
                            .get(index)
                            .cloned()
                            .ok_or_else(|| format!("{name} needs a value\n\n{USAGE}"))?;
                        index += 1;
                        value
                    }
                }
            };
        }

        match name.as_str() {
            "-h" | "--help" => parsed.help = true,
            "-V" | "--version" => parsed.version = true,
            "--headless" => seen_run_flag = true,
            "--quiet" => {
                run.quiet = true;
                seen_run_flag = true;
            }
            "--clicks" => {
                run.clicks = Some(number(&value!(), &name)?);
                seen_run_flag = true;
            }
            "--seconds" => {
                run.seconds = Some(number(&value!(), &name)?);
                seen_run_flag = true;
            }
            "--interval" => {
                run.interval_ms = Some(number(&value!(), &name)?);
                seen_run_flag = true;
            }
            "--delay" => {
                run.delay_ms = Some(number(&value!(), &name)?);
                seen_run_flag = true;
            }
            "--jitter" => {
                run.jitter_px = Some(number(&value!(), &name)?);
                seen_run_flag = true;
            }
            "--at" => {
                let raw = value!();
                let (x, y) = raw
                    .split_once(',')
                    .ok_or_else(|| format!("{name} wants X,Y but got {raw}\n\n{USAGE}"))?;
                run.at = Some((number(x.trim(), &name)?, number(y.trim(), &name)?));
                seen_run_flag = true;
            }
            "--button" => {
                let raw = value!();
                run.button = Some(match raw.to_ascii_lowercase().as_str() {
                    "left" | "l" => MouseButton::Left,
                    "right" | "r" => MouseButton::Right,
                    "middle" | "m" => MouseButton::Middle,
                    other => return Err(format!("unknown button: {other}\n\n{USAGE}")),
                });
                seen_run_flag = true;
            }
            other => {
                return Err(format!(
                    "unknown option: {other}. Note that arguments are not passed to the GUI.\n\n{USAGE}"
                ));
            }
        }
    }

    if seen_run_flag {
        parsed.run = Some(run);
    }
    Ok(parsed)
}

fn number<T: std::str::FromStr>(raw: &str, flag: &str) -> Result<T, String> {
    raw.trim()
        .parse()
        .map_err(|_| format!("{flag} wants a number but got {raw}\n\n{USAGE}"))
}

impl Run {
    pub fn apply(&self, settings: &mut Settings) {
        // An explicit limit replaces both saved limits. Otherwise, for example,
        // `--seconds 30` could still be cut short by a saved click cap, or
        // `--clicks 20` by a saved time cap.
        if self.clicks.is_some() || self.seconds.is_some() {
            settings.max_clicks = 0;
            settings.max_seconds = 0;
        }
        if let Some(clicks) = self.clicks {
            settings.max_clicks = clicks;
        }
        if let Some(seconds) = self.seconds {
            settings.max_seconds = seconds;
        }
        if let Some(interval) = self.interval_ms {
            settings.timing = TimingMode::Fixed;
            settings.interval_ms = interval;
        }
        if let Some(at) = self.at {
            settings.position = PositionMode::Fixed;
            settings.point = at;
        }
        if let Some(px) = self.jitter_px {
            settings.jitter = if px == 0 {
                JitterMode::Off
            } else {
                JitterMode::Rectangle
            };
            settings.jitter_x = px;
            settings.jitter_y = px;
        }
        if let Some(button) = self.button {
            settings.button = button;
        }
        if let Some(delay) = self.delay_ms {
            settings.start_delay_ms = delay;
        }
    }

    /// Runs one session on the calling thread. Returns a process exit code.
    pub fn execute(self) -> u8 {
        // Arm before connecting so an interrupt during a slow connection is retained.
        crate::platform::arm_headless_interrupt();

        // A broken file must not become a click run.
        let outcome = config::load_outcome();
        let settings = match settings_from(outcome, &self) {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("featherclick: {error}");
                return 1;
            }
        };

        let mut enigo = match engine::connect() {
            Ok(enigo) => enigo,
            Err(err) => {
                eprintln!("featherclick: {err}");
                return 1;
            }
        };

        if !self.quiet {
            println!("{}", describe(&settings));
            if settings.max_clicks == 0 && settings.max_seconds == 0 {
                println!("no click or time limit set - press Ctrl+C to stop");
            }
        }

        let (_never, commands) = channel();
        let start = Instant::now();
        let mut stopped: Option<StopReason> = None;
        let mut clicks: u64 = 0;
        let mut sink = |event: SessionEvent| match event {
            SessionEvent::Trigger(total) => {
                clicks = total;
                if !self.quiet && settings.max_clicks == 0 && total % 100 == 0 {
                    println!("{total} clicks");
                }
            }
            SessionEvent::Captured(point) => println!("{},{}", point.0, point.1),
            SessionEvent::Stopped(reason) => stopped = Some(reason),
            SessionEvent::Started { .. } | SessionEvent::Clicking => {}
            SessionEvent::Warning(message) => eprintln!("featherclick: {message}"),
        };

        engine::run_session(
            &settings,
            &commands,
            &mut enigo,
            &mut sink,
            &crate::platform::interrupt_requested,
        );

        let elapsed = start.elapsed().as_secs_f64();
        let reason = stopped.unwrap_or(StopReason::Shutdown);
        let rate = if elapsed > 0.0 {
            clicks as f64 / elapsed
        } else {
            0.0
        };
        println!(
            "{clicks} clicks in {elapsed:.2}s ({rate:.1}/s) - {}",
            reason.describe()
        );
        u8::from(reason.is_error())
    }
}

/// Saved settings plus this run's overrides. A load error is fatal: clicking
/// with defaults would hide the broken file.
fn settings_from(outcome: config::LoadOutcome, run: &Run) -> Result<Settings, String> {
    let config::LoadOutcome { settings, error } = outcome;
    if let Some(error) = error {
        return Err(error);
    }
    let mut settings = settings;
    run.apply(&mut settings);
    settings.sanitize();
    Ok(settings)
}

fn describe(settings: &Settings) -> String {
    let timing = match settings.timing {
        TimingMode::Fixed => format!("every {} ms", settings.interval_ms),
        TimingMode::Uniform => format!(
            "every {}-{} ms",
            settings.interval_min_ms, settings.interval_max_ms
        ),
        TimingMode::Normal => format!(
            "every {}+-{} ms",
            settings.interval_mean_ms, settings.interval_std_ms
        ),
    };
    let place = match settings.position {
        PositionMode::Cursor => "at the pointer".to_string(),
        PositionMode::Fixed => format!("at {},{}", settings.point.0, settings.point.1),
    };
    let jitter = match settings.jitter {
        JitterMode::Off => String::new(),
        _ => format!(" (+-{}x{} px)", settings.jitter_x, settings.jitter_y),
    };
    let limits = match (settings.max_clicks, settings.max_seconds) {
        (0, 0) => "no limit".to_string(),
        (clicks, 0) => format!("{clicks} clicks"),
        (0, seconds) => format!("{seconds} s"),
        (clicks, seconds) => format!("{clicks} clicks or {seconds} s"),
    };
    let delay = match settings.start_delay_ms {
        0 => String::new(),
        ms => format!(", after {ms} ms"),
    };
    format!(
        "{} {} {}{jitter}, {limits}{delay}",
        settings.button.label().to_ascii_lowercase(),
        timing,
        place
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(raw: &[&str]) -> Result<Args, String> {
        parse(raw.iter().map(|arg| arg.to_string()))
    }

    fn applied(saved: &Settings, raw: &[&str]) -> Settings {
        let mut settings = saved.clone();
        parse_args(raw).unwrap().run.unwrap().apply(&mut settings);
        settings.sanitize();
        settings
    }

    #[test]
    fn values_work_in_both_separate_and_inline_forms() {
        let args = parse_args(&[
            "--headless",
            "--clicks=12",
            "--at",
            "-5, 7",
            "--button",
            "RIGHT",
            "--jitter",
            "0",
            "--quiet",
        ])
        .unwrap();
        assert!(args.run.is_some());
        let run = args.run.unwrap();
        assert_eq!(run.clicks, Some(12));
        assert_eq!(run.at, Some((-5, 7)));
        assert_eq!(run.button, Some(MouseButton::Right));
        assert_eq!(run.jitter_px, Some(0));
        assert!(run.quiet);
    }

    #[test]
    fn explicit_limits_replace_both_saved_limits() {
        let saved = Settings {
            max_clicks: 50,
            max_seconds: 5,
            ..Settings::default()
        };

        let seconds = applied(&saved, &["--seconds", "30"]);
        assert_eq!(seconds.max_seconds, 30);
        assert_eq!(
            seconds.max_clicks, 0,
            "a saved click cap must not cut the requested duration short"
        );

        let clicks = applied(&saved, &["--clicks", "20"]);
        assert_eq!(clicks.max_clicks, 20);
        assert_eq!(
            clicks.max_seconds, 0,
            "a saved time cap must not cut the requested click count short"
        );

        let both = applied(&saved, &["--clicks", "20", "--seconds", "30"]);
        assert_eq!((both.max_clicks, both.max_seconds), (20, 30));

        let unlimited = applied(&saved, &["--clicks", "0"]);
        assert_eq!((unlimited.max_clicks, unlimited.max_seconds), (0, 0));
    }

    #[test]
    fn limits_left_alone_keep_the_saved_values() {
        let saved = Settings {
            max_clicks: 50,
            max_seconds: 5,
            ..Settings::default()
        };
        let run = applied(&saved, &["--interval", "10"]);
        assert_eq!((run.max_clicks, run.max_seconds), (50, 5));
    }

    #[test]
    fn the_summary_keeps_the_delay_apart_from_the_limit() {
        let settings = Settings {
            timing: TimingMode::Fixed,
            interval_ms: 100,
            position: PositionMode::Fixed,
            point: (1, 2),
            max_seconds: 30,
            start_delay_ms: 2_000,
            ..Settings::default()
        };
        assert_eq!(
            describe(&settings),
            "left every 100 ms at 1,2, 30 s, after 2000 ms"
        );
    }

    #[test]
    fn a_broken_config_is_not_a_run_and_a_valid_one_keeps_explicit_limits() {
        let broken = config::LoadOutcome {
            settings: Settings {
                max_clicks: 50,
                ..Settings::default()
            },
            error: Some("cannot parse config.json: trailing comma".to_string()),
        };
        let error = settings_from(broken, &Run::default()).unwrap_err();
        assert!(error.contains("cannot parse"), "{error}");

        let saved = Settings {
            max_clicks: 50,
            max_seconds: 5,
            interval_ms: 0,
            ..Settings::default()
        };
        let run = Run {
            seconds: Some(30),
            ..Run::default()
        };
        let settings = settings_from(
            config::LoadOutcome {
                settings: saved,
                error: None,
            },
            &run,
        )
        .unwrap();
        assert_eq!(settings.max_seconds, 30);
        assert_eq!(settings.max_clicks, 0);
        assert_eq!(settings.interval_ms, 1, "a run uses the clamped interval");
    }

    #[test]
    fn malformed_values_name_the_option() {
        let error = parse_args(&["--clicks", "many"]).unwrap_err();
        assert!(error.contains("--clicks wants a number"), "{error}");

        let error = parse_args(&["--at", "10"]).unwrap_err();
        assert!(error.contains("--at wants X,Y"), "{error}");

        let error = parse_args(&["--button", "side"]).unwrap_err();
        assert!(error.contains("unknown button: side"), "{error}");
    }
}
