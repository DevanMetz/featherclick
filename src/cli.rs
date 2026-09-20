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

With no options the settings window opens. Run options start clicking in the
background, using the saved settings as defaults; a limit of 0 means run until
interrupted.

RUN OPTIONS:
        --headless              click without opening a window
        --clicks <N>            stop after N clicks
        --seconds <S>           stop after S seconds
        --interval <MS>         click every MS milliseconds
        --at <X,Y>              click a fixed screen point
        --jitter <PX>           vary the point by up to +-PX pixels
        --button <left|right|middle>
        --delay <MS>            wait before the first click
        --quiet                 print only the final summary

OTHER OPTIONS:
    -h, --help                  print this help
    -V, --version               print the version

Exit status is 0 on success (including stop conditions), 1 on error.";

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
        let mut settings = config::load();
        self.apply(&mut settings);
        settings.sanitize();

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
            SessionEvent::Started => {}
        };

        engine::run_session(&settings, &commands, &mut enigo, &mut sink);

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
        ms => format!(" after {ms} ms"),
    };
    format!(
        "{} {} {}{jitter}, {limits}{delay}",
        settings.button.label().to_ascii_lowercase(),
        timing,
        place
    )
}
