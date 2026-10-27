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
                        run_session(
                            &settings,
                            rx,
                            handle,
                            &mut |event| apply(status, event),
                            &|| false,
                        );
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

fn capture(enigo: &mut impl Mouse, sink: &mut dyn FnMut(SessionEvent)) {
    if let Ok(point) = enigo.location() {
        sink(SessionEvent::Captured(point));
    }
}

/// Outcome of draining pending commands mid-session.
enum Polled {
    Continue,
    Stop(StopReason),
}

fn poll(
    rx: &Receiver<Command>,
    enigo: &mut impl Mouse,
    sink: &mut dyn FnMut(SessionEvent),
) -> Polled {
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

fn click_limit_reached(settings: &Settings, clicks: u64) -> bool {
    settings.max_clicks > 0 && clicks >= settings.max_clicks
}

fn time_limit_reached(settings: &Settings, elapsed: Duration) -> bool {
    settings.max_seconds > 0 && elapsed >= Duration::from_secs(settings.max_seconds as u64)
}

/// True when this run moves the pointer itself. Stop-on-move then watches the
/// point we left it on, rather than the position from when the run began.
fn places_pointer(settings: &Settings) -> bool {
    settings.position == PositionMode::Fixed || settings.jitter != JitterMode::Off
}

/// `threshold` of 0 is off. A point sitting on the radius is still inside:
/// only a strict departure counts, compared in squared pixels so the old
/// `sqrt as i32` truncation cannot hide one.
fn moved_away(now: (i32, i32), parked: (i32, i32), threshold: u32) -> bool {
    if threshold == 0 {
        return false;
    }
    let dx = i64::from(now.0) - i64::from(parked.0);
    let dy = i64::from(now.1) - i64::from(parked.1);
    let limit = i64::from(threshold);
    dx * dx + dy * dy > limit * limit
}

struct ClickOutcome {
    presses: u64,
    /// Set when the trigger itself should end the session (a stop during a
    /// hold or double-click gap, or a cap met halfway through a double).
    stop: Option<StopReason>,
}

/// One clicking session. The [`Waiter`] stays outside so a wait can borrow it
/// while the abort check borrows the session.
struct Run<'a, M: Mouse> {
    settings: &'a Settings,
    started: Instant,
    /// Where the pointer is allowed to rest. `None` until we have a reference:
    /// the anchor when we never move it, the last placed point when we do.
    parked: Option<(i32, i32)>,
    rx: &'a Receiver<Command>,
    enigo: &'a mut M,
    sink: &'a mut dyn FnMut(SessionEvent),
    /// Headless Ctrl+C. The GUI passes a closure that is always false; a
    /// terminal handler would fight the window's own lifetime.
    interrupt: &'a dyn Fn() -> bool,
}

impl<M: Mouse> Run<'_, M> {
    fn finish(&mut self, reason: StopReason) {
        (self.sink)(SessionEvent::Stopped(reason));
    }

    /// Limits, pointer movement, pending commands, and a headless interrupt.
    /// Called on every sleep slice, so none of them waits out the interval.
    fn stop_check(&mut self) -> Option<StopReason> {
        if (self.interrupt)() {
            return Some(StopReason::User);
        }
        if time_limit_reached(self.settings, self.started.elapsed()) {
            return Some(StopReason::TimeLimit);
        }
        if let Some(parked) = self.parked {
            // A failed read is not a move: losing the pointer for one sample
            // should not end an otherwise healthy run.
            if let Ok(now) = self.enigo.location()
                && moved_away(now, parked, self.settings.stop_on_move_px)
            {
                return Some(StopReason::MouseMoved);
            }
        }
        match poll(self.rx, self.enigo, self.sink) {
            Polled::Continue => None,
            Polled::Stop(reason) => Some(reason),
        }
    }

    fn deliver(
        &mut self,
        waiter: &mut Waiter,
        target: (i32, i32),
        clicks_so_far: u64,
    ) -> InputResult<ClickOutcome> {
        if places_pointer(self.settings) {
            self.enigo.move_mouse(target.0, target.1, Coordinate::Abs)?;
        }
        let button = Button::from(self.settings.button);
        match self.settings.style {
            ClickStyle::Single => {
                press_release(self.enigo, button)?;
                Ok(ClickOutcome {
                    presses: presses_per_trigger(ClickStyle::Single),
                    stop: None,
                })
            }
            ClickStyle::Hold => {
                self.enigo.button(button, Direction::Press)?;
                let deadline =
                    Instant::now() + Duration::from_millis(u64::from(self.settings.hold_ms));
                let stop = waiter.wait(deadline, &mut || self.stop_check());
                // Every path after a successful press releases, including a
                // stop that arrived during the hold.
                release_button(self.enigo, button)?;
                Ok(ClickOutcome {
                    presses: presses_per_trigger(ClickStyle::Hold),
                    stop,
                })
            }
            ClickStyle::Double => {
                press_release(self.enigo, button)?;
                // The second press is its own click. Skip it when the cap is
                // already met, or when a stop arrives in the gap.
                if click_limit_reached(self.settings, clicks_so_far + 1) {
                    return Ok(ClickOutcome {
                        presses: 1,
                        stop: Some(StopReason::ClickLimit),
                    });
                }
                let gap = Instant::now() + DOUBLE_GAP;
                if let Some(stop) = waiter.wait(gap, &mut || self.stop_check()) {
                    return Ok(ClickOutcome {
                        presses: 1,
                        stop: Some(stop),
                    });
                }
                press_release(self.enigo, button)?;
                Ok(ClickOutcome {
                    presses: presses_per_trigger(ClickStyle::Double),
                    stop: None,
                })
            }
        }
    }
}

/// Runs one clicking session to completion. Blocks until a limit is hit or a
/// stop command arrives.
pub(crate) fn run_session(
    settings: &Settings,
    rx: &Receiver<Command>,
    enigo: &mut impl Mouse,
    sink: &mut dyn FnMut(SessionEvent),
    interrupt: &dyn Fn() -> bool,
) {
    let _timer = crate::platform::TimerResolution::acquire();
    sink(SessionEvent::Started);

    let started = Instant::now();
    let mut waiter = Waiter::new();
    let mut run = Run {
        settings,
        started,
        parked: None,
        rx,
        enigo,
        sink,
        interrupt,
    };

    // Give the user time to move the pointer into place before the anchor is
    // taken. Movement during the delay is positioning, not a stop.
    if settings.start_delay_ms > 0 {
        let deadline = started + Duration::from_millis(u64::from(settings.start_delay_ms));
        if let Some(stopped) = waiter.wait(deadline, &mut || run.stop_check()) {
            run.finish(stopped);
            return;
        }
    }

    // Where jitter is measured from. Captured once, so repeated clicks never
    // drift away from the intended spot. When this run does not move the
    // pointer, it is also the stop-on-move reference.
    let anchor = match settings.position {
        PositionMode::Fixed => settings.point,
        PositionMode::Cursor => match run.enigo.location() {
            Ok(point) => {
                if settings.stop_on_move_px > 0 && !places_pointer(settings) {
                    run.parked = Some(point);
                }
                point
            }
            Err(_) => settings.point,
        },
    };

    let mut clicks: u64 = 0;
    let mut next = Instant::now();
    loop {
        // A wait that wakes after its deadline does not look one last time.
        // Catch a move or a stop here, before the next click goes out.
        if let Some(stopped) = run.stop_check() {
            run.finish(stopped);
            return;
        }

        let target = target_point(settings, anchor);
        let outcome = match run.deliver(&mut waiter, target, clicks) {
            Ok(outcome) => outcome,
            Err(err) => {
                run.finish(StopReason::Error(err.to_string()));
                return;
            }
        };
        clicks += outcome.presses;
        (run.sink)(SessionEvent::Trigger(clicks));

        // Park on the point we just requested. Our own move is not the user
        // taking the mouse; cursor mode keeps the anchor captured above.
        if places_pointer(settings) && settings.stop_on_move_px > 0 {
            run.parked = Some(target);
        }

        if let Some(stopped) = outcome.stop {
            run.finish(stopped);
            return;
        }
        // Do not sleep a whole interval after the final click just to notice
        // that the cap was reached.
        if click_limit_reached(settings, clicks) {
            run.finish(StopReason::ClickLimit);
            return;
        }

        next += interval(settings);
        let now = Instant::now();
        if next < now {
            // A slow click or a busy system put us behind; resynchronise rather
            // than firing a burst to catch up.
            next = now;
        }
        if let Some(stopped) = waiter.wait(next, &mut || run.stop_check()) {
            run.finish(stopped);
            return;
        }
    }
}

fn presses_per_trigger(style: ClickStyle) -> u64 {
    match style {
        ClickStyle::Double => 2,
        ClickStyle::Single | ClickStyle::Hold => 1,
    }
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

/// Retries once. A duplicate release is harmless; a stuck button is not.
fn release_button(enigo: &mut impl Mouse, button: Button) -> InputResult<()> {
    if enigo.button(button, Direction::Release).is_err() {
        return enigo.button(button, Direction::Release);
    }
    Ok(())
}

fn press_release(enigo: &mut impl Mouse, button: Button) -> InputResult<()> {
    enigo.button(button, Direction::Press)?;
    release_button(enigo, button)
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
    use std::cell::RefCell;

    use enigo::{Axis, InputError, Mouse};

    use super::*;
    use crate::config::{JitterMode, PositionMode, TimingMode};

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
    fn click_limit_is_inclusive_and_zero_means_unlimited() {
        let mut s = settings();
        assert!(!click_limit_reached(&s, 0));
        assert!(!click_limit_reached(&s, u64::MAX));

        s.max_clicks = 1;
        assert!(!click_limit_reached(&s, 0));
        assert!(click_limit_reached(&s, 1));
        assert!(click_limit_reached(&s, 2));
    }

    #[test]
    fn time_limit_is_inclusive_and_zero_means_unlimited() {
        let mut s = settings();
        assert!(!time_limit_reached(&s, Duration::from_secs(86_400)));

        s.max_seconds = 2;
        assert!(!time_limit_reached(&s, Duration::from_millis(1_999)));
        assert!(time_limit_reached(&s, Duration::from_secs(2)));
        assert!(time_limit_reached(&s, Duration::from_secs(3)));
    }

    #[test]
    fn waiter_respects_its_deadline_without_stalling() {
        let mut waiter = Waiter::new();
        // A deadline that has already passed must not wait at all.
        let start = Instant::now();
        assert_eq!(waiter.wait(Instant::now(), &mut || None), None);
        assert!(
            start.elapsed() < Duration::from_millis(20),
            "waited on a past deadline"
        );

        // Never returns early, however many times it is asked.
        for _ in 0..20 {
            let deadline = Instant::now() + Duration::from_millis(20);
            assert_eq!(waiter.wait(deadline, &mut || None), None);
            let now = Instant::now();
            assert!(now >= deadline, "returned early by {:?}", deadline - now);
        }

        // How closely the OS wakes a sleeping thread is a property of the
        // machine, not of this loop: a loaded CI runner overshoots by tens of
        // milliseconds no matter what the code does. The bound below only
        // catches the loop waiting on the wrong thing or not terminating.
        let total = Instant::now() - start;
        assert!(
            total < Duration::from_secs(5),
            "20 waits of 20 ms took {total:?}"
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

    #[test]
    fn moved_away_is_strict_and_zero_means_off() {
        let home = (10, -20);
        assert!(!moved_away(home, home, 5));
        // 3-4-5 triangle: exactly the threshold stays inside.
        assert!(!moved_away((13, -16), home, 5));
        assert!(moved_away((13, -15), home, 5));
        assert!(!moved_away((1_000, 1_000), home, 0));
    }

    /// Pointer whose reported position can change under the session, without a
    /// display. `location` counts reads so a test can move the pointer on a
    /// later sample than the one taken immediately after a click.
    struct FakePointer {
        inner: RefCell<FakeState>,
    }

    struct FakeState {
        pos: (i32, i32),
        locations: u32,
        /// Report `jump` once `locations` exceeds this.
        jump_after: Option<u32>,
        jump: (i32, i32),
        fail_location: bool,
        log: Vec<FakeEvent>,
        down: Vec<Button>,
        stop_on_press: Option<Sender<Command>>,
        stop_sent: bool,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum FakeEvent {
        Move((i32, i32)),
        Press,
        Release,
    }

    impl FakePointer {
        fn at(pos: (i32, i32)) -> Self {
            Self {
                inner: RefCell::new(FakeState {
                    pos,
                    locations: 0,
                    jump_after: None,
                    jump: pos,
                    fail_location: false,
                    log: Vec::new(),
                    down: Vec::new(),
                    stop_on_press: None,
                    stop_sent: false,
                }),
            }
        }

        fn presses(&self) -> usize {
            self.inner
                .borrow()
                .log
                .iter()
                .filter(|event| matches!(event, FakeEvent::Press))
                .count()
        }

        fn releases(&self) -> usize {
            self.inner
                .borrow()
                .log
                .iter()
                .filter(|event| matches!(event, FakeEvent::Release))
                .count()
        }

        fn moves(&self) -> usize {
            self.inner
                .borrow()
                .log
                .iter()
                .filter(|event| matches!(event, FakeEvent::Move(_)))
                .count()
        }

        fn locations(&self) -> u32 {
            self.inner.borrow().locations
        }
    }

    impl Mouse for FakePointer {
        fn button(&mut self, button: Button, direction: Direction) -> InputResult<()> {
            let mut inner = self.inner.borrow_mut();
            match direction {
                Direction::Press => {
                    inner.down.push(button);
                    inner.log.push(FakeEvent::Press);
                    if !inner.stop_sent
                        && let Some(tx) = &inner.stop_on_press
                    {
                        let _ = tx.send(Command::Stop);
                        inner.stop_sent = true;
                    }
                }
                Direction::Release => {
                    if let Some(index) = inner.down.iter().rposition(|held| *held == button) {
                        inner.down.remove(index);
                    }
                    inner.log.push(FakeEvent::Release);
                }
                Direction::Click => {
                    inner.log.push(FakeEvent::Press);
                    inner.log.push(FakeEvent::Release);
                }
            }
            Ok(())
        }

        fn move_mouse(&mut self, x: i32, y: i32, coordinate: Coordinate) -> InputResult<()> {
            let mut inner = self.inner.borrow_mut();
            let pos = match coordinate {
                Coordinate::Abs => (x, y),
                Coordinate::Rel => (inner.pos.0 + x, inner.pos.1 + y),
            };
            inner.pos = pos;
            inner.log.push(FakeEvent::Move(pos));
            Ok(())
        }

        fn scroll(&mut self, _length: i32, _axis: Axis) -> InputResult<()> {
            Ok(())
        }

        fn main_display(&self) -> InputResult<(i32, i32)> {
            Ok((1920, 1080))
        }

        fn location(&self) -> InputResult<(i32, i32)> {
            let mut inner = self.inner.borrow_mut();
            if inner.fail_location {
                return Err(InputError::Simulate("no pointer"));
            }
            inner.locations += 1;
            if inner
                .jump_after
                .is_some_and(|after| inner.locations > after)
            {
                return Ok(inner.jump);
            }
            Ok(inner.pos)
        }
    }

    fn drive(
        settings: &Settings,
        mouse: &mut FakePointer,
        rx: &Receiver<Command>,
        interrupt: &dyn Fn() -> bool,
    ) -> (Vec<SessionEvent>, Duration) {
        let mut events = Vec::new();
        let started = Instant::now();
        run_session(
            settings,
            rx,
            mouse,
            &mut |event| events.push(event),
            interrupt,
        );
        (events, started.elapsed())
    }

    fn stop_reason(events: &[SessionEvent]) -> StopReason {
        events
            .iter()
            .rev()
            .find_map(|event| match event {
                SessionEvent::Stopped(reason) => Some(reason.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("session did not stop: {events:?}"))
    }

    fn triggers(events: &[SessionEvent]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::Trigger(n) => Some(*n),
                _ => None,
            })
            .collect()
    }

    fn assert_buttons_released(mouse: &FakePointer) {
        assert_eq!(
            mouse.presses(),
            mouse.releases(),
            "a press was left down: {:?}",
            mouse.inner.borrow().log
        );
        assert!(mouse.inner.borrow().down.is_empty(), "button still held");
    }

    fn armed(interval_ms: u32, clicks: u64) -> Settings {
        let mut s = settings();
        s.timing = TimingMode::Fixed;
        s.interval_ms = interval_ms;
        s.max_clicks = clicks;
        s.max_seconds = 0;
        s.stop_on_move_px = 0;
        s.position = PositionMode::Fixed;
        s.point = (40, 40);
        s.jitter = JitterMode::Off;
        s
    }

    #[test]
    fn stop_on_move_notices_a_fixed_target_left_during_the_interval() {
        let mut s = armed(2_000, 20);
        s.stop_on_move_px = 8;
        let mut mouse = FakePointer::at((40, 40));
        {
            let mut inner = mouse.inner.borrow_mut();
            // The sample taken as the wait starts still sees the placed point.
            // The next sample, during the interval, sees the user leave.
            inner.jump_after = Some(1);
            inner.jump = (40, 80);
        }
        let (tx, rx) = channel();
        let (events, elapsed) = drive(&s, &mut mouse, &rx, &|| false);
        drop(tx);

        assert_eq!(stop_reason(&events), StopReason::MouseMoved);
        assert_eq!(triggers(&events), vec![1]);
        assert_eq!(
            mouse.presses(),
            1,
            "must not click again after the pointer leaves"
        );
        assert!(mouse.moves() >= 1);
        assert!(
            mouse.locations() >= 2,
            "the departure has to show up on a later sample, not the first"
        );
        assert!(
            elapsed < Duration::from_millis(1_500),
            "stop-on-move waited out the 2s interval: {elapsed:?}"
        );
        assert_buttons_released(&mouse);
    }

    #[test]
    fn stop_on_move_watches_the_anchor_when_the_pointer_is_not_placed() {
        let mut s = armed(2_000, 20);
        s.position = PositionMode::Cursor;
        s.stop_on_move_px = 5;
        let mut mouse = FakePointer::at((10, 10));
        {
            let mut inner = mouse.inner.borrow_mut();
            // Read 1 is the anchor, read 2 is the pre-click check, read 3 is
            // the first sample of the interval.
            inner.jump_after = Some(2);
            inner.jump = (80, 10);
        }
        let (tx, rx) = channel();
        let (events, elapsed) = drive(&s, &mut mouse, &rx, &|| false);
        drop(tx);

        assert_eq!(stop_reason(&events), StopReason::MouseMoved);
        assert_eq!(mouse.presses(), 1);
        assert_eq!(mouse.moves(), 0, "cursor mode must not move the pointer");
        assert!(mouse.locations() >= 3);
        assert!(elapsed < Duration::from_millis(1_500), "{elapsed:?}");
        assert_buttons_released(&mouse);
    }

    #[test]
    fn placing_the_pointer_is_not_treated_as_the_user_moving_it() {
        let mut s = armed(5, 3);
        s.stop_on_move_px = 1;
        // Start elsewhere so the session's own move is a real relocation.
        let mut mouse = FakePointer::at((0, 0));
        let (tx, rx) = channel();
        let (events, _) = drive(&s, &mut mouse, &rx, &|| false);
        drop(tx);

        assert_eq!(stop_reason(&events), StopReason::ClickLimit);
        assert_eq!(triggers(&events), vec![1, 2, 3]);
        assert_eq!(mouse.presses(), 3);
        assert_buttons_released(&mouse);
    }

    #[test]
    fn stop_on_move_off_ignores_a_jump_and_a_failed_read_is_not_a_move() {
        let mut s = armed(5, 2);
        s.stop_on_move_px = 0;
        let mut mouse = FakePointer::at((40, 40));
        mouse.inner.borrow_mut().jump_after = Some(0);
        mouse.inner.borrow_mut().jump = (400, 400);
        let (tx, rx) = channel();
        let (events, _) = drive(&s, &mut mouse, &rx, &|| false);
        drop(tx);
        assert_eq!(stop_reason(&events), StopReason::ClickLimit);
        assert_eq!(mouse.presses(), 2);

        let mut s = armed(5, 2);
        s.stop_on_move_px = 5;
        let mut mouse = FakePointer::at((40, 40));
        mouse.inner.borrow_mut().fail_location = true;
        let (tx, rx) = channel();
        let (events, _) = drive(&s, &mut mouse, &rx, &|| false);
        drop(tx);
        assert_eq!(stop_reason(&events), StopReason::ClickLimit);
        assert_eq!(mouse.presses(), 2);
        assert_buttons_released(&mouse);
    }

    #[test]
    fn a_stop_during_a_hold_releases_the_button_without_waiting_it_out() {
        let mut s = armed(100, 5);
        s.style = ClickStyle::Hold;
        s.hold_ms = 5_000;
        s.position = PositionMode::Cursor;
        let (tx, rx) = channel();
        let mut mouse = FakePointer::at((1, 1));
        mouse.inner.borrow_mut().stop_on_press = Some(tx);
        let (events, elapsed) = drive(&s, &mut mouse, &rx, &|| false);

        assert_eq!(stop_reason(&events), StopReason::User);
        assert_eq!(triggers(&events), vec![1]);
        assert_eq!(mouse.presses(), 1);
        assert_buttons_released(&mouse);
        assert!(
            elapsed < Duration::from_millis(1_000),
            "hold ran on after the stop: {elapsed:?}"
        );
    }

    #[test]
    fn a_double_click_does_not_send_the_second_press_past_a_stop_or_the_cap() {
        let mut s = armed(100, 10);
        s.style = ClickStyle::Double;
        s.position = PositionMode::Cursor;
        let (tx, rx) = channel();
        let mut mouse = FakePointer::at((1, 1));
        mouse.inner.borrow_mut().stop_on_press = Some(tx);
        let (events, elapsed) = drive(&s, &mut mouse, &rx, &|| false);
        assert_eq!(stop_reason(&events), StopReason::User);
        assert_eq!(triggers(&events), vec![1]);
        assert_eq!(mouse.presses(), 1);
        assert_buttons_released(&mouse);
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");

        let mut s = armed(50, 1);
        s.style = ClickStyle::Double;
        s.position = PositionMode::Cursor;
        let (tx, rx) = channel();
        let mut mouse = FakePointer::at((1, 1));
        let (events, _) = drive(&s, &mut mouse, &rx, &|| false);
        drop(tx);
        assert_eq!(stop_reason(&events), StopReason::ClickLimit);
        assert_eq!(triggers(&events), vec![1], "an odd cap must not overshoot");
        assert_eq!(mouse.presses(), 1);
        assert_buttons_released(&mouse);

        let mut s = armed(50, 2);
        s.style = ClickStyle::Double;
        s.position = PositionMode::Cursor;
        let (tx, rx) = channel();
        let mut mouse = FakePointer::at((1, 1));
        let (events, _) = drive(&s, &mut mouse, &rx, &|| false);
        drop(tx);
        assert_eq!(stop_reason(&events), StopReason::ClickLimit);
        assert_eq!(triggers(&events), vec![2]);
        assert_eq!(mouse.presses(), 2);
        assert_buttons_released(&mouse);
    }

    #[test]
    fn a_pending_stop_or_interrupt_cancels_before_the_next_click() {
        let s = armed(2_000, 10);
        let (tx, rx) = channel();
        tx.send(Command::Stop).unwrap();
        let mut mouse = FakePointer::at((40, 40));
        let (events, elapsed) = drive(&s, &mut mouse, &rx, &|| false);
        drop(tx);
        assert_eq!(stop_reason(&events), StopReason::User);
        assert_eq!(mouse.presses(), 0);
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");

        let mut s = armed(100, 10);
        s.start_delay_ms = 5_000;
        let (tx, rx) = channel();
        let mut mouse = FakePointer::at((40, 40));
        let (events, elapsed) = drive(&s, &mut mouse, &rx, &|| true);
        drop(tx);
        assert_eq!(stop_reason(&events), StopReason::User);
        assert!(triggers(&events).is_empty());
        assert_eq!(mouse.presses(), 0);
        assert!(
            elapsed < Duration::from_millis(500),
            "interrupt waited out the start delay: {elapsed:?}"
        );
    }
}
