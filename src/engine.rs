//! The click engine.
//!
//! [`run_session`] is the whole click loop: it is synchronous, takes the
//! settings snapshot it should honour plus a command channel, and reports
//! progress through a sink. The GUI drives it from a worker thread
//! ([`Worker`]); `--headless` drives the very same function from `main`, so
//! both paths share one implementation.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use enigo::{Button, Coordinate, Direction, Enigo, InputResult, Mouse, Settings as EnigoSettings};

use crate::config::{ClickStyle, JitterMode, MouseButton, PositionMode, Settings, TimingMode};

/// Gap between the two presses of a double click. Well inside every platform's
/// double-click threshold, short enough not to blunt the click rate.
const DOUBLE_GAP: Duration = Duration::from_millis(35);
/// Longest single sleep, so Stop stays responsive during long intervals.
const MAX_SLEEP_SLICE: Duration = Duration::from_millis(25);
/// Shortest sleep worth asking the OS for.
const MIN_SLEEP: Duration = Duration::from_micros(200);
/// The last part of every interval is a spin-wait. Bounds for that slice:
/// measured OS overshoot plus a margin, clamped. Must stay >= MIN_SLEEP.
const SLACK_FLOOR: Duration = Duration::from_micros(300);
const SLACK_CEILING: Duration = Duration::from_millis(2);
const SLACK_INITIAL: Duration = Duration::from_millis(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Stop requested by the user (hotkey, button, or Ctrl-C in headless mode).
    User,
    ClickLimit,
    TimeLimit,
    /// `stop_on_move_px` tripped: the pointer left the click point.
    MouseMoved,
    /// The engine shut down.
    Shutdown,
    Error(String),
}

impl StopReason {
    pub fn describe(&self) -> String {
        match self {
            Self::User => "stopped".to_string(),
            Self::ClickLimit => "stopped: click limit reached".to_string(),
            Self::TimeLimit => "stopped: time limit reached".to_string(),
            Self::MouseMoved => "stopped: pointer moved".to_string(),
            Self::Shutdown => "stopped: engine shut down".to_string(),
            Self::Error(err) => format!("error: {err}"),
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

/// Progress reported by a running session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Started,
    /// Total clicks sent so far.
    Trigger(u64),
    Captured((i32, i32)),
    Stopped(StopReason),
}

/// Live engine state, as read by the UI.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub running: bool,
    pub clicks: u64,
    pub started: Option<Instant>,
    /// Set when the session ends, so the reported duration stops with it.
    pub stopped: Option<Instant>,
    pub stop_reason: Option<StopReason>,
    pub error: Option<String>,
    pub captured: Option<(i32, i32)>,
    /// Bumped on every capture, so a repeated capture of the same point is
    /// still applied.
    pub capture_seq: u64,
}

impl Status {
    /// How long the current session has been running, or how long the last one
    /// ran for.
    pub fn elapsed(&self) -> Option<Duration> {
        match (self.started, self.stopped) {
            (Some(start), Some(end)) => Some(end.saturating_duration_since(start)),
            (Some(start), None) => Some(start.elapsed()),
            _ => None,
        }
    }
}

/// Worker commands. Crate-internal: `run_session` only needs the receiver, and
/// only this crate drives it.
pub(crate) enum Command {
    Start(Box<Settings>),
    Stop,
    /// Start if idle, stop if running. Resolved by the worker, so the decision
    /// is made against authoritative state rather than a stale UI read.
    Toggle(Box<Settings>),
    Capture,
    Shutdown,
}

#[derive(Clone)]
pub struct Worker {
    tx: Sender<Command>,
    status: Arc<Mutex<Status>>,
}

impl Worker {
    /// Spawns the long-lived engine thread. It owns the platform input handle
    /// for the process lifetime and idles on the command channel when stopped.
    pub fn spawn() -> Self {
        let (tx, rx) = channel();
        let status: Arc<Mutex<Status>> = Arc::new(Mutex::new(Status::default()));
        let thread_status = Arc::clone(&status);
        let builder = thread::Builder::new().name("featherclick-engine".to_string());
        let spawned: std::io::Result<JoinHandle<()>> =
            builder.spawn(move || worker_loop(&rx, &thread_status));
        if let Err(err) = spawned {
            let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
            guard.error = Some(format!("cannot start engine thread: {err}"));
        }
        Self { tx, status }
    }

    fn send(&self, command: Command) {
        // A dead worker is reported through `status.error` by the loop itself.
        let _ = self.tx.send(command);
    }

    pub fn start(&self, settings: &Settings) {
        self.send(Command::Start(Box::new(settings.clone())));
    }

    pub fn stop(&self) {
        self.send(Command::Stop);
    }

    pub fn toggle(&self, settings: &Settings) {
        self.send(Command::Toggle(Box::new(settings.clone())));
    }

    pub fn capture_cursor(&self) {
        self.send(Command::Capture);
    }

    pub fn shutdown(&self) {
        self.send(Command::Shutdown);
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Connects to the platform input backend. Boxed error text is what the user
/// needs on a headless Linux box with no X11/Wayland session.
pub fn connect() -> Result<Enigo, String> {
    let settings = EnigoSettings {
        // Every extra millisecond of per-event delay is a millisecond off the
        // achievable click rate.
        linux_delay: 0,
        ..EnigoSettings::default()
    };
    Enigo::new(&settings).map_err(|err| format!("cannot simulate input: {err}"))
}

fn worker_loop(rx: &Receiver<Command>, status: &Arc<Mutex<Status>>) {
    let mut enigo = connect().ok();
    if enigo.is_none() {
        let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
        guard.error = connect().err();
    }

    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Ok(Command::Shutdown) => break,
            Ok(Command::Stop) => {}
            Ok(Command::Capture) => {
                // Retry the connection: a display may have appeared since launch.
                if enigo.is_none() {
                    enigo = connect().ok();
                }
                if let Some(handle) = enigo.as_mut() {
                    capture(handle, &mut |event| apply(status, event));
                }
            }
            Ok(Command::Start(settings)) | Ok(Command::Toggle(settings)) => {
                let idle = !status.lock().unwrap_or_else(|e| e.into_inner()).running;
                if !idle {
                    continue;
                }
                if enigo.is_none() {
                    enigo = connect().ok();
                }
                match enigo.as_mut() {
                    None => {
                        let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
                        guard.error = connect().err();
                    }
                    Some(handle) => {
                        run_session(&settings, rx, handle, &mut |event| apply(status, event));
                    }
                }
            }
        }
    }
}

/// Applies a session event to the shared status. Kept out of `Worker` so the
/// worker thread can use it without cloning the handle.
fn apply(status: &Arc<Mutex<Status>>, event: SessionEvent) {
    let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
    match event {
        SessionEvent::Started => {
            guard.running = true;
            guard.clicks = 0;
            guard.started = Some(Instant::now());
            guard.stopped = None;
            guard.stop_reason = None;
            guard.error = None;
        }
        SessionEvent::Trigger(clicks) => guard.clicks = clicks,
        SessionEvent::Captured(point) => {
            guard.captured = Some(point);
            guard.capture_seq = guard.capture_seq.wrapping_add(1);
        }
        SessionEvent::Stopped(reason) => {
            guard.running = false;
            guard.stopped = Some(Instant::now());
            guard.stop_reason = Some(reason);
        }
    }
}

fn capture(enigo: &mut Enigo, sink: &mut dyn FnMut(SessionEvent)) {
    if let Ok(point) = enigo.location() {
        sink(SessionEvent::Captured(point));
    }
}

/// Outcome of draining pending commands mid-session.
enum Polled {
    Continue,
    Stop(StopReason),
}

fn poll(rx: &Receiver<Command>, enigo: &mut Enigo, sink: &mut dyn FnMut(SessionEvent)) -> Polled {
    loop {
        match rx.try_recv() {
            Err(TryRecvError::Empty) => return Polled::Continue,
            Err(TryRecvError::Disconnected) => return Polled::Stop(StopReason::Shutdown),
            Ok(Command::Stop) | Ok(Command::Toggle(_)) => return Polled::Stop(StopReason::User),
            Ok(Command::Shutdown) => return Polled::Stop(StopReason::Shutdown),
            Ok(Command::Capture) => capture(enigo, sink),
            // Already running; a second Start is a no-op.
            Ok(Command::Start(_)) => {}
        }
    }
}

/// Runs one clicking session to completion. Blocks until a limit is hit or a
/// stop command arrives.
pub(crate) fn run_session(
    settings: &Settings,
    rx: &Receiver<Command>,
    enigo: &mut Enigo,
    sink: &mut dyn FnMut(SessionEvent),
) {
    let _timer = crate::platform::TimerResolution::acquire();
    sink(SessionEvent::Started);

    let started = Instant::now();
    let mut clicks: u64 = 0;
    let mut reason: Option<StopReason> = None;
    let mut waiter = Waiter::new();

    // Give the user time to move the pointer into place before the anchor is taken.
    if settings.start_delay_ms > 0 {
        let deadline = started + Duration::from_millis(settings.start_delay_ms as u64);
        if let Some(stopped) = waiter.wait(deadline, &mut || match poll(rx, enigo, sink) {
            Polled::Continue => None,
            Polled::Stop(reason) => Some(reason),
        }) {
            reason = Some(stopped);
        }
    }

    // Where jitter is measured from: captured once, so repeated clicks never
    // drift away from the intended spot.
    let anchor = match settings.position {
        PositionMode::Fixed => settings.point,
        PositionMode::Cursor => enigo.location().unwrap_or(settings.point),
    };

    let mut next = Instant::now();
    while reason.is_none() {
        if settings.max_clicks > 0 && clicks >= settings.max_clicks {
            reason = Some(StopReason::ClickLimit);
            break;
        }
        if settings.max_seconds > 0
            && started.elapsed() >= Duration::from_secs(settings.max_seconds as u64)
        {
            reason = Some(StopReason::TimeLimit);
            break;
        }

        let target = target_point(settings, anchor);
        if let Err(err) = perform_click(enigo, settings, target) {
            let message = err.to_string();
            reason = Some(StopReason::Error(message));
            break;
        }
        clicks += presses_per_trigger(settings.style);
        sink(SessionEvent::Trigger(clicks));

        if settings.stop_on_move_px > 0 {
            let moved = enigo
                .location()
                .map(|now| distance(now, target) > settings.stop_on_move_px as i32)
                .unwrap_or(false);
            if moved {
                reason = Some(StopReason::MouseMoved);
                break;
            }
        }

        next += interval(settings);
        let now = Instant::now();
        if next < now {
            // A slow click or a busy system put us behind; resynchronise rather
            // than firing a burst to catch up.
            next = now;
        }
        if let Some(stopped) = waiter.wait(next, &mut || match poll(rx, enigo, sink) {
            Polled::Continue => None,
            Polled::Stop(reason) => Some(reason),
        }) {
            reason = Some(stopped);
        }
    }

    sink(SessionEvent::Stopped(
        reason.unwrap_or(StopReason::Shutdown),
    ));
}

fn presses_per_trigger(style: ClickStyle) -> u64 {
    match style {
        ClickStyle::Double => 2,
        ClickStyle::Single | ClickStyle::Hold => 1,
    }
}

fn distance(a: (i32, i32), b: (i32, i32)) -> i32 {
    let dx = (a.0 - b.0) as i64;
    let dy = (a.1 - b.1) as i64;
    ((dx * dx + dy * dy) as f64).sqrt() as i32
}

impl From<MouseButton> for Button {
    fn from(button: MouseButton) -> Self {
        match button {
            MouseButton::Left => Self::Left,
            MouseButton::Right => Self::Right,
            MouseButton::Middle => Self::Middle,
        }
    }
}

/// Moves (when needed) and clicks. Kept panic-free: the pointer is only ever
/// left pressed for the duration of this call.
fn perform_click(enigo: &mut Enigo, settings: &Settings, target: (i32, i32)) -> InputResult<()> {
    let needs_move = settings.position == PositionMode::Fixed || settings.jitter != JitterMode::Off;
    if needs_move {
        enigo.move_mouse(target.0, target.1, Coordinate::Abs)?;
    }
    let button = Button::from(settings.button);
    match settings.style {
        ClickStyle::Single => press_release(enigo, button),
        ClickStyle::Hold => {
            enigo.button(button, Direction::Press)?;
            thread::sleep(Duration::from_millis(settings.hold_ms as u64));
            enigo.button(button, Direction::Release)
        }
        ClickStyle::Double => {
            press_release(enigo, button)?;
            thread::sleep(DOUBLE_GAP);
            press_release(enigo, button)
        }
    }
}

fn press_release(enigo: &mut Enigo, button: Button) -> InputResult<()> {
    enigo.button(button, Direction::Press)?;
    enigo.button(button, Direction::Release)
}

/// Draws the click interval for one trigger.
pub fn interval(settings: &Settings) -> Duration {
    let millis = match settings.timing {
        TimingMode::Fixed => settings.interval_ms as f64,
        TimingMode::Uniform => {
            let (low, high) = (
                settings.interval_min_ms as f64,
                settings.interval_max_ms as f64,
            );
            if high <= low {
                low
            } else {
                rand::random_range(low..=high)
            }
        }
        TimingMode::Normal => {
            let mean = settings.interval_mean_ms as f64;
            let sigma = settings.interval_std_ms as f64;
            if sigma <= 0.0 {
                mean
            } else {
                (mean + sigma * gaussian()).clamp(1.0, mean + 4.0 * sigma)
            }
        }
    };
    Duration::from_secs_f64(millis.max(1.0) / 1000.0)
}

/// Where the next click lands, relative to the anchor.
pub fn target_point(settings: &Settings, anchor: (i32, i32)) -> (i32, i32) {
    let (jx, jy) = (settings.jitter_x as f64, settings.jitter_y as f64);
    let (ox, oy): (f64, f64) = match settings.jitter {
        JitterMode::Off => return anchor,
        JitterMode::Rectangle => {
            let x = if jx > 0.0 {
                rand::random_range(-jx..=jx)
            } else {
                0.0
            };
            let y = if jy > 0.0 {
                rand::random_range(-jy..=jy)
            } else {
                0.0
            };
            (x, y)
        }
        JitterMode::Ellipse => {
            // Uniform over the disc's area, not its radius.
            let angle = rand::random_range(0.0..std::f64::consts::TAU);
            let radius = rand::random_range(0.0f64..1.0).sqrt();
            (jx * radius * angle.cos(), jy * radius * angle.sin())
        }
        JitterMode::Normal => {
            let x = if jx > 0.0 {
                jx * clamp_sigma(gaussian())
            } else {
                0.0
            };
            let y = if jy > 0.0 {
                jy * clamp_sigma(gaussian())
            } else {
                0.0
            };
            (x, y)
        }
    };
    (anchor.0 + ox.round() as i32, anchor.1 + oy.round() as i32)
}

fn clamp_sigma(value: f64) -> f64 {
    value.clamp(-3.0, 3.0)
}

/// Standard normal sample (Box-Muller).
fn gaussian() -> f64 {
    let u1: f64 = rand::random_range(f64::MIN_POSITIVE..1.0);
    let u2: f64 = rand::random_range(0.0..1.0);
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// Sleeps until a deadline, staying responsive to stop commands.
///
/// Sleeps are capped at [`MAX_SLEEP_SLICE`] and shrink towards the deadline by
/// the recently observed OS overshoot, so only the last fraction of a
/// millisecond is ever spun on.
struct Waiter {
    slack: Duration,
}

impl Waiter {
    fn new() -> Self {
        Self {
            slack: SLACK_INITIAL,
        }
    }

    /// Returns `Some(reason)` if aborted, `None` if the deadline was reached.
    fn wait(
        &mut self,
        deadline: Instant,
        abort: &mut dyn FnMut() -> Option<StopReason>,
    ) -> Option<StopReason> {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            if let Some(reason) = abort() {
                return Some(reason);
            }
            let wake = deadline.checked_sub(self.slack).unwrap_or(deadline);
            let now = Instant::now();
            if now < wake {
                // `slack >= SLACK_FLOOR > MIN_SLEEP`, so this never sleeps past the deadline.
                let sleep_for = (wake - now).min(MAX_SLEEP_SLICE).max(MIN_SLEEP);
                let intended = now + sleep_for;
                thread::sleep(sleep_for);
                let overshoot = Instant::now().saturating_duration_since(intended);
                self.slack = (overshoot + SLACK_FLOOR).clamp(SLACK_FLOOR, SLACK_CEILING);
            } else {
                std::hint::spin_loop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{JitterMode, TimingMode};

    fn settings() -> Settings {
        let mut s = Settings::default();
        s.sanitize();
        s
    }

    #[test]
    fn intervals_stay_inside_the_configured_range() {
        let mut s = settings();
        s.timing = TimingMode::Uniform;
        s.interval_min_ms = 30;
        s.interval_max_ms = 70;
        let mut seen_low = false;
        let mut seen_high = false;
        for _ in 0..20_000 {
            let ms = interval(&s).as_secs_f64() * 1000.0;
            assert!((30.0..=70.0).contains(&ms), "{ms} ms outside 30..=70");
            seen_low |= ms < 45.0;
            seen_high |= ms > 55.0;
        }
        assert!(seen_low && seen_high, "random timing must actually vary");

        s.timing = TimingMode::Fixed;
        s.interval_ms = 12;
        for _ in 0..100 {
            assert_eq!(interval(&s), Duration::from_millis(12));
        }
    }

    #[test]
    fn normal_timing_never_returns_zero_or_absurd_intervals() {
        let mut s = settings();
        s.timing = TimingMode::Normal;
        s.interval_mean_ms = 8;
        s.interval_std_ms = 8;
        s.sanitize();
        for _ in 0..20_000 {
            let secs = interval(&s).as_secs_f64();
            assert!(secs >= 0.001, "{secs}s would spin the loop");
            assert!(secs <= 0.040, "{secs}s is beyond the +4 sigma clamp");
        }
    }

    #[test]
    fn jitter_stays_within_its_shape() {
        let anchor = (1000, 500);
        let mut s = settings();

        s.jitter = JitterMode::Off;
        assert_eq!(target_point(&s, anchor), anchor);

        s.jitter = JitterMode::Rectangle;
        s.jitter_x = 30;
        s.jitter_y = 10;
        for _ in 0..20_000 {
            let (x, y) = target_point(&s, anchor);
            assert!(
                (970..=1030).contains(&x) && (490..=510).contains(&y),
                "{x},{y} left the box"
            );
        }

        s.jitter = JitterMode::Ellipse;
        s.jitter_x = 20;
        s.jitter_y = 20;
        // Offsets are rounded to whole pixels, so the bound carries 1px of slack.
        let reach = 21.0 * 21.0;
        let mut away_from_centre = false;
        for _ in 0..20_000 {
            let (x, y) = target_point(&s, anchor);
            let (dx, dy) = ((x - anchor.0) as f64, (y - anchor.1) as f64);
            assert!(dx * dx + dy * dy <= reach, "{dx},{dy} left the ellipse");
            away_from_centre |= dx * dx + dy * dy > 4.0;
        }
        assert!(away_from_centre, "elliptical jitter must move off centre");

        s.jitter = JitterMode::Normal;
        s.jitter_x = 5;
        s.jitter_y = 5;
        let mut palette = std::collections::HashSet::new();
        let mut spread = 0i32;
        for _ in 0..20_000 {
            let (x, y) = target_point(&s, anchor);
            assert!((anchor.0 - 15..=anchor.0 + 15).contains(&x));
            assert!((anchor.1 - 15..=anchor.1 + 15).contains(&y));
            spread = spread.max((x - anchor.0).abs());
            palette.insert((x, y));
        }
        assert!(
            spread >= 3,
            "sigma of 5 px should reach a few pixels, got {spread}"
        );
        assert!(
            palette.len() > 20,
            "jitter should scatter, got {} points",
            palette.len()
        );
    }

    #[test]
    fn double_clicks_count_two_presses() {
        assert_eq!(presses_per_trigger(ClickStyle::Single), 1);
        assert_eq!(presses_per_trigger(ClickStyle::Double), 2);
        assert_eq!(presses_per_trigger(ClickStyle::Hold), 1);
    }

    #[test]
    fn waiter_never_returns_early_and_does_not_systematically_lag() {
        let mut waiter = Waiter::new();
        // Warm up so the overshoot estimate is realistic.
        for _ in 0..5 {
            waiter.wait(Instant::now() + Duration::from_millis(5), &mut || None);
        }

        let start = Instant::now();
        let mut worst = Duration::ZERO;
        for _ in 0..20 {
            let deadline = Instant::now() + Duration::from_millis(20);
            assert_eq!(waiter.wait(deadline, &mut || None), None);
            let now = Instant::now();
            assert!(now >= deadline, "returned early by {:?}", deadline - now);
            worst = worst.max(now.saturating_duration_since(deadline));
        }

        // A shared runner may wake late, so this bound is deliberately loose.
        // What it still catches is the loop waiting on the wrong thing.
        let total = start.elapsed();
        assert!(
            total >= Duration::from_millis(400),
            "20 waits of 20 ms cannot finish in {total:?}"
        );
        assert!(
            total < Duration::from_millis(800),
            "20 waits of 20 ms took {total:?}, worst wake {worst:?}"
        );
    }

    #[test]
    fn waiter_stays_responsive_to_stop_commands() {
        let mut waiter = Waiter::new();
        let mut abort_after = 3;
        let start = Instant::now();
        let reason = waiter.wait(Instant::now() + Duration::from_secs(30), &mut || {
            abort_after -= 1;
            (abort_after <= 0).then_some(StopReason::User)
        });
        assert_eq!(reason, Some(StopReason::User));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "a stop must not wait out a 30s interval"
        );
    }
}
