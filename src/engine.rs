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

#[cfg(target_os = "linux")]
#[path = "pick_overlay.rs"]
mod pick_overlay;

#[cfg(any(not(windows), test))]
use enigo::Coordinate;
#[cfg(target_os = "linux")]
use enigo::Settings as EnigoSettings;
use enigo::{Button, Direction, Enigo, InputResult, Mouse};

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
    /// The run is armed. `arm_until` is when the first click is allowed;
    /// `None` means there is no start delay.
    Started {
        arm_until: Option<Instant>,
    },
    /// The start delay is over. The time limit runs from here.
    Clicking,
    /// Total clicks sent so far.
    Trigger(u64),
    Captured((i32, i32)),
    Warning(String),
    Stopped(StopReason),
}

/// Live engine state, as read by the UI.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub running: bool,
    pub clicks: u64,
    /// When the run was armed, including the start delay.
    pub started: Option<Instant>,
    /// When the start delay ends. `None` when there is nothing to wait out.
    pub arm_until: Option<Instant>,
    /// When clicking, and the time limit, actually began.
    pub clicking: Option<Instant>,
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
    /// How long clicking has been going, or how long the last run clicked for.
    ///
    /// The start delay is not included once clicking has begun, so this lines
    /// up with the time limit. A run that was stopped during the delay reports
    /// the time spent waiting instead.
    pub fn elapsed(&self) -> Option<Duration> {
        let origin = self.clicking.or(self.started)?;
        match self.stopped {
            Some(end) => Some(end.saturating_duration_since(origin)),
            None => Some(origin.elapsed()),
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
    /// Latest position of the FeatherClick window. Headless runs never send one.
    Window(WindowShield),
    Shutdown,
}

/// Outer bounds of the FeatherClick window, in physical screen pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl ScreenRect {
    /// Inclusive of the minimum edge, exclusive of the far edge.
    pub(crate) fn contains(self, x: i32, y: i32) -> bool {
        if self.width <= 0 || self.height <= 0 {
            return false;
        }
        let x1 = self.x.saturating_add(self.width);
        let y1 = self.y.saturating_add(self.height);
        x >= self.x && y >= self.y && x < x1 && y < y1
    }
}

/// Where the window is, so a run can avoid clicking it.
///
/// Pressing Start leaves the pointer on that button. The next click would
/// land on Stop and the run would end before it began. A click whose point
/// is inside [`Self::rect`] is held. Wayland cannot report a position;
/// [`Self::pointer_over`] covers cursor mode there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct WindowShield {
    pub rect: Option<ScreenRect>,
    pub pointer_over: bool,
}

#[derive(Clone)]
pub struct Worker {
    tx: Sender<Command>,
    status: Arc<Mutex<Status>>,
}

impl Worker {
    /// Spawns the long-lived engine thread. It owns the platform input handle
    /// for the process lifetime and idles on the command channel when stopped.
    ///
    /// `wake` runs on that thread when the run starts, stops, or a point is
    /// picked. Click counts are left to the window's own timer: waking on
    /// every click would repaint hundreds of times a second.
    pub fn spawn(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = channel();
        let status: Arc<Mutex<Status>> = Arc::new(Mutex::new(Status::default()));
        let thread_status = Arc::clone(&status);
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let builder = thread::Builder::new().name("featherclick-engine".to_string());
        let spawned: std::io::Result<JoinHandle<()>> =
            builder.spawn(move || worker_loop(&rx, &thread_status, wake.as_ref()));
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
        self.send(Command::Start(Box::new(snapshot(settings))));
    }

    pub fn stop(&self) {
        self.send(Command::Stop);
    }

    pub fn toggle(&self, settings: &Settings) {
        self.send(Command::Toggle(Box::new(snapshot(settings))));
    }

    pub fn capture_cursor(&self) {
        self.send(Command::Capture);
    }

    pub(crate) fn set_window(&self, shield: WindowShield) {
        self.send(Command::Window(shield));
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

/// Clamps a copy the same way loading the file does, so a run cannot use a
/// range the next launch would have repaired.
fn snapshot(settings: &Settings) -> Settings {
    let mut settings = settings.clone();
    settings.sanitize();
    settings
}

/// Display name that is not a parseable X11 display and not a Wayland socket.
/// Passing it forces that one backend to fail immediately, so the other is
/// the only connection enigo keeps.
#[cfg(any(target_os = "linux", test))]
const ISOLATED_DISPLAY: &str = "featherclick-no-display";

/// One attempt to open an input connection.
///
/// `None` leaves that server to the environment. `Some` forces the name enigo
/// will try, which is how a backend is kept out of the connection.
#[cfg(any(target_os = "linux", test))]
struct BackendAttempt {
    wayland_display: Option<String>,
    x11_display: Option<String>,
}

/// Servers to try, in order. Each attempt leaves at most one of the two open.
///
/// enigo sends every click to every server it connected, so a session where
/// both Wayland and XWayland answered would click twice. The first attempt is
/// Wayland alone, and only when `WAYLAND_DISPLAY` is set; the last is X11
/// alone, which is also the path on Windows and macOS (those fields are
/// ignored there).
#[cfg(test)]
fn backend_attempts(wayland_is_set: bool) -> Vec<BackendAttempt> {
    let mut attempts = Vec::new();
    if wayland_is_set {
        attempts.push(BackendAttempt {
            wayland_display: None,
            x11_display: Some(ISOLATED_DISPLAY.to_string()),
        });
    }
    attempts.push(BackendAttempt {
        wayland_display: Some(ISOLATED_DISPLAY.to_string()),
        x11_display: None,
    });
    attempts
}

/// Only the Linux X11 pointer fallback opens a connection with forced
/// display names; other platforms connect through `crate::input`.
#[cfg(target_os = "linux")]
fn enigo_settings(attempt: &BackendAttempt) -> EnigoSettings {
    EnigoSettings {
        // Every extra millisecond of per-event delay is a millisecond off the
        // achievable click rate.
        linux_delay: 0,
        wayland_display: attempt.wayland_display.clone(),
        x11_display: attempt.x11_display.clone(),
        ..EnigoSettings::default()
    }
}

/// Connects to one platform input backend.
pub fn connect() -> Result<crate::input::Input, String> {
    crate::input::connect()
}

/// Opens the input backend if a previous attempt failed. One connection
/// attempt per call: the error from that attempt is what the UI shows.
fn ensure_input<'a>(
    enigo: &'a mut Option<crate::input::Input>,
    status: &Arc<Mutex<Status>>,
    wake: &(dyn Fn() + Send + Sync),
) -> Option<&'a mut crate::input::Input> {
    if enigo.is_none() {
        match connect() {
            Ok(handle) => *enigo = Some(handle),
            Err(err) => {
                note_error(status, wake, err);
            }
        }
    }
    enigo.as_mut()
}

fn worker_loop(
    rx: &Receiver<Command>,
    status: &Arc<Mutex<Status>>,
    wake: &(dyn Fn() + Send + Sync),
) {
    #[cfg(target_os = "linux")]
    let deferred = crate::wayland_probe::prefer_portal();
    #[cfg(not(target_os = "linux"))]
    let deferred = false;
    let mut enigo = if deferred {
        None
    } else {
        match connect() {
            Ok(enigo) => Some(enigo),
            Err(err) => {
                // One attempt. A second connection here used to be only for the
                // message, and a success on that retry left the thread with neither
                // a handle nor an error.
                note_error(status, wake, err);
                None
            }
        }
    };

    let mut shield = WindowShield::default();
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Ok(Command::Shutdown) => break,
            Ok(Command::Stop) => {}
            Ok(Command::Window(update)) => shield = update,
            Ok(Command::Capture) => {
                #[cfg(target_os = "linux")]
                if enigo.is_none() && deferred {
                    match crate::portal::read_cursor() {
                        Ok(point) => apply(status, SessionEvent::Captured(point), wake),
                        Err(err) => {
                            capture_unreadable(err, &mut |event| apply(status, event, wake))
                        }
                    }
                    continue;
                }
                // Retry the connection: a display may have appeared since launch.
                if let Some(handle) = ensure_input(&mut enigo, status, wake) {
                    capture(handle, &mut |event| apply(status, event, wake));
                } else {
                    #[cfg(target_os = "linux")]
                    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                        capture_unreadable(String::new(), &mut |event| apply(status, event, wake));
                    }
                }
            }
            Ok(Command::Start(settings)) | Ok(Command::Toggle(settings)) => {
                let idle = !status.lock().unwrap_or_else(|e| e.into_inner()).running;
                if !idle {
                    continue;
                }
                if let Some(handle) = ensure_input(&mut enigo, status, wake) {
                    run_session(
                        &settings,
                        rx,
                        handle,
                        &mut shield,
                        &mut |event| apply(status, event, wake),
                        &|| false,
                    );
                }
            }
        }
    }
}

fn note_error(status: &Mutex<Status>, wake: &(dyn Fn() + Send + Sync), message: String) {
    status.lock().unwrap_or_else(|e| e.into_inner()).error = Some(message);
    wake();
}

/// Applies a session event to the shared status. Kept out of `Worker` so the
/// worker thread can use it without cloning the handle.
fn apply(status: &Arc<Mutex<Status>>, event: SessionEvent, wake: &(dyn Fn() + Send + Sync)) {
    let notify = !matches!(event, SessionEvent::Trigger(_));
    {
        let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            SessionEvent::Started { arm_until } => {
                guard.running = true;
                guard.clicks = 0;
                guard.started = Some(Instant::now());
                guard.arm_until = arm_until;
                guard.clicking = None;
                guard.stopped = None;
                guard.stop_reason = None;
                guard.error = None;
            }
            SessionEvent::Clicking => guard.clicking = Some(Instant::now()),
            SessionEvent::Trigger(clicks) => guard.clicks = clicks,
            SessionEvent::Captured(point) => {
                guard.captured = Some(point);
                guard.capture_seq = guard.capture_seq.wrapping_add(1);
            }
            SessionEvent::Warning(err) => guard.error = Some(err),
            SessionEvent::Stopped(reason) => {
                guard.running = false;
                guard.stopped = Some(Instant::now());
                guard.stop_reason = Some(reason);
            }
        }
    }
    if notify {
        wake();
    }
}

fn capture(enigo: &mut impl Pointer, sink: &mut dyn FnMut(SessionEvent)) {
    match enigo.read_pointer() {
        Ok(point) => sink(SessionEvent::Captured(point)),
        Err(err) => capture_unreadable(err, sink),
    }
}

fn capture_unreadable(err: String, sink: &mut dyn FnMut(SessionEvent)) {
    #[cfg(target_os = "linux")]
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match pick_overlay::pick() {
            Ok(Some(point)) => sink(SessionEvent::Captured(point)),
            Ok(None) => {}
            Err(error) => sink(SessionEvent::Warning(error)),
        }
        return;
    }
    sink(SessionEvent::Warning(err));
}

/// Outcome of draining pending commands mid-session.
enum Polled {
    Continue,
    Stop(StopReason),
}

fn poll(
    rx: &Receiver<Command>,
    enigo: &mut impl Pointer,
    shield: &mut WindowShield,
    sink: &mut dyn FnMut(SessionEvent),
) -> Polled {
    loop {
        match rx.try_recv() {
            Err(TryRecvError::Empty) => return Polled::Continue,
            Err(TryRecvError::Disconnected) => return Polled::Stop(StopReason::Shutdown),
            Ok(Command::Stop) | Ok(Command::Toggle(_)) => return Polled::Stop(StopReason::User),
            Ok(Command::Shutdown) => return Polled::Stop(StopReason::Shutdown),
            Ok(Command::Window(update)) => *shield = update,
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
fn time_limit_due(settings: &Settings, clicking_from: Option<Instant>, now: Instant) -> bool {
    clicking_from
        .is_some_and(|from| time_limit_reached(settings, now.saturating_duration_since(from)))
}

fn places_pointer(settings: &Settings) -> bool {
    settings.position == PositionMode::Fixed || settings.jitter != JitterMode::Off
}

/// Cursor jitter is measured from wherever the pointer already is, and
/// stop-on-move has to watch that pointer. Both need a real reading.
fn anchor_needs_pointer(settings: &Settings) -> bool {
    settings.stop_on_move_px > 0
        || (settings.position == PositionMode::Cursor && settings.jitter != JitterMode::Off)
}

/// The point repeated clicks are measured from.
///
/// `read` is the pointer position when [`anchor_needs_pointer`] is true, and
/// is ignored otherwise. A failed read is returned rather than replaced with
/// the saved point: that saved point is often `(0, 0)`, and jitter would then
/// drag the pointer to the corner.
fn resolve_anchor(
    settings: &Settings,
    read: Result<(i32, i32), String>,
) -> Result<(i32, i32), String> {
    match settings.position {
        PositionMode::Fixed => {
            if settings.stop_on_move_px > 0 {
                read?;
            }
            Ok(settings.point)
        }
        PositionMode::Cursor => {
            if settings.jitter == JitterMode::Off && settings.stop_on_move_px == 0 {
                Ok(settings.point)
            } else {
                read
            }
        }
    }
}

/// Reads the pointer in the same pixel space a click is aimed at.
///
/// Wayland's virtual pointer cannot report a position (enigo returns "Not
/// implemented"). XWayland's root window can, so a second X11-only connection
/// is used for the read and never for clicks.
fn pointer_location(enigo: &Enigo) -> Result<(i32, i32), String> {
    match enigo.location() {
        Ok(point) => Ok(point),
        Err(err) => location_fallback(err.to_string()),
    }
}

fn location_fallback(primary: String) -> Result<(i32, i32), String> {
    #[cfg(target_os = "linux")]
    {
        match x11_pointer_fallback() {
            Ok(point) => Ok(point),
            Err(fallback) => Err(explain_location(&primary, Some(&fallback))),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(explain_location(&primary, None))
    }
}

fn explain_location(primary: &str, x11: Option<&str>) -> String {
    #[cfg(target_os = "linux")]
    if primary.contains("Not implemented") || primary.contains("does not report a position") {
        return match x11 {
            None => "cannot read the pointer position: Wayland does not report it, and no X11 display is connected".to_string(),
            Some(err) => format!(
                "cannot read the pointer position: Wayland does not report it, and X11 failed ({err})"
            ),
        };
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = x11;
    }
    format!("cannot read the pointer position: {primary}")
}

/// X11 pointer read that does not join the click connection.
///
/// The connection is kept on this thread. Opening one per sample would add a
/// full X handshake to every click, and stop-on-move samples on that loop.
#[cfg(target_os = "linux")]
fn x11_pointer_fallback() -> Result<(i32, i32), String> {
    use std::cell::RefCell;

    if std::env::var_os("DISPLAY").is_none() {
        return Err("DISPLAY is not set".into());
    }

    struct Slot {
        enigo: Option<Enigo>,
        error: Option<String>,
    }

    thread_local! {
        static LOCATOR: RefCell<Slot> = const { RefCell::new(Slot { enigo: None, error: None }) };
    }

    LOCATOR.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.enigo.is_none() && slot.error.is_none() {
            let settings = enigo_settings(&BackendAttempt {
                wayland_display: Some(ISOLATED_DISPLAY.to_string()),
                x11_display: None,
            });
            match Enigo::new(&settings) {
                Ok(enigo) => slot.enigo = Some(enigo),
                Err(err) => slot.error = Some(err.to_string()),
            }
        }
        if let Some(enigo) = slot.enigo.as_ref() {
            return enigo.location().map_err(|err| err.to_string());
        }
        Err(slot
            .error
            .clone()
            .unwrap_or_else(|| "could not connect to X11".to_string()))
    })
}

/// Puts the pointer on a physical pixel.
///
/// On Windows, enigo's absolute move normalises into the primary monitor, so
/// a point on any other monitor never lands and would not match
/// `GetCursorPos`. `SetCursorPos` uses that virtual-desktop space.
fn place_pointer(enigo: &mut Enigo, x: i32, y: i32) -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = enigo;
        crate::platform::set_cursor_pos(x, y)
    }
    #[cfg(not(windows))]
    {
        enigo
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|err| err.to_string())
    }
}

pub(crate) trait Pointer: Mouse {
    fn read_pointer(&self) -> Result<(i32, i32), String>;
    fn place(&mut self, x: i32, y: i32) -> Result<(), String>;
}

impl Pointer for Enigo {
    fn read_pointer(&self) -> Result<(i32, i32), String> {
        pointer_location(self)
    }
    fn place(&mut self, x: i32, y: i32) -> Result<(), String> {
        place_pointer(self, x, y)
    }
}

impl Pointer for crate::input::Input {
    fn read_pointer(&self) -> Result<(i32, i32), String> {
        self.location().or_else(location_fallback)
    }
    fn place(&mut self, x: i32, y: i32) -> Result<(), String> {
        #[cfg(windows)]
        {
            crate::platform::set_cursor_pos(x, y)
        }
        #[cfg(not(windows))]
        {
            self.move_abs(x, y)
        }
    }
}

/// `threshold` of 0 is off. A point sitting on the radius is still inside:
/// only a strict departure counts, compared in squared pixels so the old
/// `sqrt as i32` truncation cannot hide one.
fn moved_away(now: (i32, i32), parked: (i32, i32), threshold: u32) -> bool {
    if threshold == 0 {
        return false;
    }
    let dx = i128::from(now.0) - i128::from(parked.0);
    let dy = i128::from(now.1) - i128::from(parked.1);
    let limit = i128::from(threshold);
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
struct Run<'a, M: Pointer> {
    settings: &'a Settings,
    clicking_from: Option<Instant>,
    /// Where the pointer is allowed to rest. `None` until we have a reference:
    /// the anchor when we never move it, the last placed point when we do.
    parked: Option<(i32, i32)>,
    rx: &'a Receiver<Command>,
    shield: &'a mut WindowShield,
    enigo: &'a mut M,
    sink: &'a mut dyn FnMut(SessionEvent),
    /// Headless Ctrl+C. The GUI passes a closure that is always false; a
    /// terminal handler would fight the window's own lifetime.
    interrupt: &'a dyn Fn() -> bool,
}

impl<M: Pointer> Run<'_, M> {
    fn finish(&mut self, reason: StopReason) {
        (self.sink)(SessionEvent::Stopped(reason));
    }

    /// Limits, pointer movement, pending commands, and a headless interrupt.
    /// Called on every sleep slice, so none of them waits out the interval.
    fn stop_check(&mut self) -> Option<StopReason> {
        if (self.interrupt)() {
            return Some(StopReason::User);
        }
        if time_limit_due(self.settings, self.clicking_from, Instant::now()) {
            return Some(StopReason::TimeLimit);
        }
        if let Some(parked) = self.parked {
            // A failed read is not a move: losing the pointer for one sample
            // should not end an otherwise healthy run.
            if let Ok(now) = self.enigo.read_pointer()
                && moved_away(now, parked, self.settings.stop_on_move_px)
            {
                return Some(StopReason::MouseMoved);
            }
        }
        match poll(self.rx, self.enigo, self.shield, self.sink) {
            Polled::Continue => None,
            Polled::Stop(reason) => Some(reason),
        }
    }

    fn deliver(
        &mut self,
        waiter: &mut Waiter,
        target: (i32, i32),
        clicks_so_far: u64,
    ) -> Result<ClickOutcome, String> {
        if places_pointer(self.settings) {
            self.enigo.place(target.0, target.1)?;
            if self.settings.stop_on_move_px > 0 {
                // Establish the new reference before checks inside holds/gaps.
                // Screen edges and backend coordinate mapping may clamp the
                // request. Watch the position actually reached when readable.
                self.parked = Some(self.enigo.read_pointer().unwrap_or(target));
            }
        }
        let button = Button::from(self.settings.button);
        match self.settings.style {
            ClickStyle::Single => {
                press_release(self.enigo, button).map_err(|err| err.to_string())?;
                Ok(ClickOutcome {
                    presses: presses_per_trigger(ClickStyle::Single),
                    stop: None,
                })
            }
            ClickStyle::Hold => {
                self.enigo
                    .button(button, Direction::Press)
                    .map_err(|err| err.to_string())?;
                let deadline =
                    Instant::now() + Duration::from_millis(u64::from(self.settings.hold_ms));
                let stop = waiter.wait(deadline, &mut || self.stop_check());
                // Every path after a successful press releases, including a
                // stop that arrived during the hold.
                release_button(self.enigo, button).map_err(|err| err.to_string())?;
                Ok(ClickOutcome {
                    presses: presses_per_trigger(ClickStyle::Hold),
                    stop,
                })
            }
            ClickStyle::Double => {
                press_release(self.enigo, button).map_err(|err| err.to_string())?;
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
                press_release(self.enigo, button).map_err(|err| err.to_string())?;
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
    enigo: &mut impl Pointer,
    shield: &mut WindowShield,
    sink: &mut dyn FnMut(SessionEvent),
    interrupt: &dyn Fn() -> bool,
) {
    let _timer = crate::platform::TimerResolution::acquire();
    let started = Instant::now();
    let arm_until = (settings.start_delay_ms > 0)
        .then(|| started + Duration::from_millis(u64::from(settings.start_delay_ms)));
    sink(SessionEvent::Started { arm_until });
    let mut waiter = Waiter::new();
    let mut run = Run {
        settings,
        clicking_from: None,
        parked: None,
        rx,
        shield,
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

    run.clicking_from = Some(Instant::now());
    (run.sink)(SessionEvent::Clicking);

    let mut anchor = None;
    let mut clicks: u64 = 0;
    let mut next = Instant::now();
    loop {
        // A wait that wakes after its deadline does not look one last time.
        // Catch a move or a stop here, before the next click goes out.
        if let Some(stopped) = run.stop_check() {
            run.finish(stopped);
            return;
        }

        if settings.position == PositionMode::Cursor
            && run.shield.rect.is_none()
            && run.shield.pointer_over
        {
            if let Some(stopped) =
                waiter.wait(Instant::now() + MAX_SLEEP_SLICE, &mut || run.stop_check())
            {
                run.finish(stopped);
                return;
            }
            next = Instant::now();
            continue;
        }
        let read = run.enigo.read_pointer();
        if anchor.is_none()
            && anchor_needs_pointer(settings)
            && let Err(err) = resolve_anchor(settings, read.clone())
        {
            run.finish(StopReason::Error(err));
            return;
        }
        let target = match click_plan(settings, &mut anchor, read.ok(), run.shield) {
            ClickPlan::Hold => {
                if let Some(stopped) =
                    waiter.wait(Instant::now() + MAX_SLEEP_SLICE, &mut || run.stop_check())
                {
                    run.finish(stopped);
                    return;
                }
                next = Instant::now();
                continue;
            }
            ClickPlan::Fire(target) => target,
        };
        if run.parked.is_none() && settings.stop_on_move_px > 0 && !places_pointer(settings) {
            run.parked = anchor;
        }
        let outcome = match run.deliver(&mut waiter, target, clicks) {
            Ok(outcome) => outcome,
            Err(err) => {
                run.finish(StopReason::Error(err.to_string()));
                return;
            }
        };
        clicks += outcome.presses;
        (run.sink)(SessionEvent::Trigger(clicks));

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

        next = next_deadline(next, interval(settings), Instant::now());
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

/// `places` is true when the click will be moved to `aim`. With no rectangle
/// (Wayland), only a click that stays at the live pointer can be held, and
/// only while that pointer is over the window.
fn aimed_at_window(aim: (i32, i32), places: bool, shield: &WindowShield) -> bool {
    if let Some(rect) = shield.rect {
        return rect.contains(aim.0, aim.1);
    }
    !places && shield.pointer_over
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickPlan {
    /// Skip this trigger; the aim is on the FeatherClick window.
    Hold,
    Fire((i32, i32)),
}

/// Decides the next trigger without touching the input connection, so the
/// window check can be tested on its own.
fn click_plan(
    settings: &Settings,
    anchor: &mut Option<(i32, i32)>,
    pointer: Option<(i32, i32)>,
    shield: &WindowShield,
) -> ClickPlan {
    if anchor.is_none() {
        match settings.position {
            PositionMode::Fixed => *anchor = Some(settings.point),
            PositionMode::Cursor => match pointer {
                // A failed read is not "over the window": keep the old fallback
                // point and let the click go out.
                Some(point) if !aimed_at_window(point, false, shield) => *anchor = Some(point),
                Some(_) => return ClickPlan::Hold,
                None => *anchor = Some(settings.point),
            },
        }
    }
    let Some(anchor_point) = *anchor else {
        return ClickPlan::Hold;
    };
    let target = target_point(settings, anchor_point);
    let places = places_pointer(settings);
    // Cursor mode clicks at the live pointer. Anything that moves the pointer
    // is judged by where it would be placed, so jitter can still miss the window.
    let aim = if places { Some(target) } else { pointer };
    if aim.is_some_and(|point| aimed_at_window(point, places, shield)) {
        return ClickPlan::Hold;
    }
    ClickPlan::Fire(target)
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
                clamp_normal(mean, sigma, gaussian())
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
    offset_target(anchor, ox, oy, settings.jitter == JitterMode::Ellipse)
}

fn offset_target(anchor: (i32, i32), ox: f64, oy: f64, ellipse: bool) -> (i32, i32) {
    // Rounding outwards can put a boundary sample outside the ellipse.
    let pixel = |v: f64| {
        if ellipse {
            v.trunc() as i32
        } else {
            v.round() as i32
        }
    };
    (
        anchor.0.saturating_add(pixel(ox)),
        anchor.1.saturating_add(pixel(oy)),
    )
}

fn next_deadline(previous: Instant, interval: Duration, now: Instant) -> Instant {
    let planned = previous + interval;
    if planned < now {
        // Missed deadlines are discarded, including after suspend or a hold
        // longer than the interval. Never emit a catch-up click immediately.
        now + interval
    } else {
        planned
    }
}

fn clamp_sigma(value: f64) -> f64 {
    value.clamp(-3.0, 3.0)
}

/// One Gaussian draw, in milliseconds, kept inside ±4 sigma and at least 1.
///
/// Clamping only the top used to leave the bottom at 1 ms, so a draw could
/// land far below `mean - 4 sigma`.
fn clamp_normal(mean: f64, sigma: f64, z: f64) -> f64 {
    let low = (mean - 4.0 * sigma).max(1.0);
    let high = (mean + 4.0 * sigma).max(low);
    (mean + sigma * z).clamp(low, high)
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
        self.wait_with(deadline, abort, &mut Instant::now, &mut thread::sleep)
    }

    fn wait_with(
        &mut self,
        deadline: Instant,
        abort: &mut dyn FnMut() -> Option<StopReason>,
        now: &mut dyn FnMut() -> Instant,
        sleep: &mut dyn FnMut(Duration),
    ) -> Option<StopReason> {
        loop {
            if let Some(reason) = abort() {
                return Some(reason);
            }
            let sampled = now();
            if sampled >= deadline {
                return None;
            }
            let wake = deadline.checked_sub(self.slack).unwrap_or(deadline);
            let sampled = now();
            if sampled < wake {
                // `slack >= SLACK_FLOOR > MIN_SLEEP`, so this never sleeps past the deadline.
                let sleep_for = (wake - sampled).min(MAX_SLEEP_SLICE).max(MIN_SLEEP);
                let intended = sampled + sleep_for;
                sleep(sleep_for);
                let overshoot = now().saturating_duration_since(intended);
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
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    use enigo::{Axis, InputError, Mouse};

    use super::*;
    use crate::config::{JitterMode, PositionMode, TimingMode};

    fn settings() -> Settings {
        let mut s = Settings::default();
        s.sanitize();
        s
    }

    #[test]
    fn deadlines_preserve_cadence_without_catching_up() {
        let origin = Instant::now();
        let step = Duration::from_millis(100);
        assert_eq!(
            next_deadline(origin, step, origin + Duration::from_millis(30)),
            origin + step
        );
        // A hold or suspended process has missed its deadline.
        for late in [101, 500, 60_000] {
            let now = origin + Duration::from_millis(late);
            assert_eq!(next_deadline(origin, step, now), now + step);
        }
        assert_eq!(next_deadline(origin, step, origin + step), origin + step);
    }

    #[test]
    fn fake_clock_wait_slices_long_delays_and_checks_stop_after_suspend() {
        let origin = Instant::now();
        let elapsed = RefCell::new(Duration::ZERO);
        let sleeps = RefCell::new(Vec::new());
        let mut checks = 0;
        let reason = Waiter::new().wait_with(
            origin + Duration::from_secs(30),
            &mut || {
                checks += 1;
                (checks == 2).then_some(StopReason::User)
            },
            &mut || origin + *elapsed.borrow(),
            &mut |slice| {
                sleeps.borrow_mut().push(slice);
                // The machine suspends during the first sleep.
                *elapsed.borrow_mut() += Duration::from_secs(60);
            },
        );
        assert_eq!(reason, Some(StopReason::User));
        assert_eq!(*sleeps.borrow(), vec![MAX_SLEEP_SLICE]);
    }

    #[test]
    fn expired_wait_still_checks_stop_before_delivering() {
        assert_eq!(
            Waiter::new().wait(Instant::now(), &mut || Some(StopReason::User)),
            Some(StopReason::User)
        );
    }

    #[test]
    fn jitter_pixels_respect_shapes_and_integer_extremes() {
        assert_eq!(
            offset_target((i32::MAX, i32::MIN), 100.0, -100.0, false),
            (i32::MAX, i32::MIN)
        );
        // Rounding (0.71, 0.71) to (1, 1) would leave a unit circle.
        assert_eq!(offset_target((0, 0), 0.71, -0.71, true), (0, 0));
        for angle in 0..360 {
            let angle = f64::from(angle).to_radians();
            let (x, y) = offset_target((0, 0), 30.0 * angle.cos(), 10.0 * angle.sin(), true);
            assert!((f64::from(x) / 30.0).powi(2) + (f64::from(y) / 10.0).powi(2) <= 1.0);
        }
        assert!(!moved_away(
            (i32::MIN, i32::MIN),
            (i32::MIN, i32::MIN),
            u32::MAX
        ));
        assert!(moved_away(
            (i32::MIN, i32::MIN),
            (i32::MAX, i32::MAX),
            u32::MAX
        ));
    }

    #[test]
    fn limits_include_the_exact_boundary_and_exclude_start_delay() {
        let mut s = armed(100, 3);
        assert!(!click_limit_reached(&s, 2));
        assert!(click_limit_reached(&s, 3));
        s.max_seconds = 1;
        let now = Instant::now();
        assert!(!time_limit_due(&s, None, now));
        assert!(!time_limit_due(
            &s,
            Some(now),
            now + Duration::from_millis(999)
        ));
        assert!(time_limit_due(&s, Some(now), now + Duration::from_secs(1)));
    }

    #[test]
    fn relocating_a_fixed_point_does_not_abort_holds_or_double_gaps() {
        for style in [ClickStyle::Hold, ClickStyle::Double] {
            let mut s = armed(1, 4);
            s.style = style;
            s.hold_ms = 1;
            s.stop_on_move_px = 1;
            let (_tx, rx) = channel();
            let mut mouse = FakePointer::at((0, 0));
            let mut shield = WindowShield::default();
            let mut sink = |_| {};
            let mut run = Run {
                settings: &s,
                clicking_from: None,
                parked: Some((0, 0)),
                rx: &rx,
                shield: &mut shield,
                enigo: &mut mouse,
                sink: &mut sink,
                interrupt: &|| false,
            };
            let outcome = run.deliver(&mut Waiter::new(), (40, 40), 0).unwrap();
            assert_eq!(outcome.stop, None);
            assert_eq!(outcome.presses, presses_per_trigger(style));
            assert_buttons_released(&mouse);
        }
    }

    #[test]
    fn a_run_snapshot_is_clamped_like_a_loaded_config() {
        let mut raw = settings();
        raw.timing = TimingMode::Uniform;
        raw.interval_ms = 0;
        raw.interval_min_ms = 300;
        raw.interval_max_ms = 50;
        raw.interval_mean_ms = 40;
        raw.interval_std_ms = 900;

        let ready = snapshot(&raw);
        assert_eq!(ready.interval_ms, 1);
        assert_eq!((ready.interval_min_ms, ready.interval_max_ms), (50, 300));
        assert!(ready.interval_std_ms <= ready.interval_mean_ms);
        // The caller's fields stay where the drag left them.
        assert_eq!(raw.interval_min_ms, 300);

        for _ in 0..1_000 {
            let ms = interval(&ready).as_secs_f64() * 1000.0;
            assert!(
                (50.0..=300.0).contains(&ms),
                "{ms} ms is outside the clamped range"
            );
        }
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
    fn normal_timing_clamps_both_sides_of_the_four_sigma_window() {
        // mean 100, sigma 10: the window is 60..=140, not 1..=140.
        assert_eq!(clamp_normal(100.0, 10.0, 0.0), 100.0);
        assert_eq!(clamp_normal(100.0, 10.0, -4.0), 60.0);
        assert_eq!(clamp_normal(100.0, 10.0, 4.0), 140.0);
        assert_eq!(
            clamp_normal(100.0, 10.0, -100.0),
            60.0,
            "a deep tail must stop at mean - 4 sigma, not at 1 ms"
        );
        assert_eq!(clamp_normal(100.0, 10.0, 100.0), 140.0);
        // Four sigma below this mean crosses 1 ms, so the floor applies.
        assert_eq!(clamp_normal(100.0, 25.0, -100.0), 1.0);
        assert_eq!(clamp_normal(100.0, 25.0, 100.0), 200.0);

        let mut s = settings();
        s.timing = TimingMode::Normal;
        s.interval_mean_ms = 100;
        s.interval_std_ms = 10;
        let mut below_mean = false;
        let mut above_mean = false;
        for _ in 0..20_000 {
            let ms = interval(&s).as_secs_f64() * 1000.0;
            assert!(
                (60.0 - 1e-6..=140.0 + 1e-6).contains(&ms),
                "{ms} ms is outside mean ± 4 sigma"
            );
            below_mean |= ms < 90.0;
            above_mean |= ms > 110.0;
        }
        assert!(
            below_mean && above_mean,
            "sigma of 10 ms should spread around the mean"
        );
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
        // Pixel conversion must not push a boundary draw outside the disc.
        let reach = 20.0 * 20.0;
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
    fn the_time_limit_does_not_run_before_clicking_starts() {
        let mut s = settings();
        s.max_seconds = 1;
        let now = Instant::now();
        assert!(
            !time_limit_due(&s, None, now + Duration::from_secs(3_600)),
            "the arming delay is not clicking time"
        );
        assert!(!time_limit_due(
            &s,
            Some(now),
            now + Duration::from_millis(999)
        ));
        assert!(time_limit_due(&s, Some(now), now + Duration::from_secs(1)));

        s.max_seconds = 0;
        assert!(!time_limit_due(
            &s,
            Some(now),
            now + Duration::from_secs(3_600)
        ));
    }

    #[test]
    fn the_reported_duration_starts_when_clicking_does() {
        let started = Instant::now();
        let clicking = started + Duration::from_millis(1_500);
        let finished = Status {
            started: Some(started),
            clicking: Some(clicking),
            stopped: Some(clicking + Duration::from_millis(1_000)),
            ..Status::default()
        };
        assert_eq!(finished.elapsed(), Some(Duration::from_millis(1_000)));

        let interrupted = Status {
            started: Some(started),
            stopped: Some(started + Duration::from_millis(400)),
            ..Status::default()
        };
        assert_eq!(
            interrupted.elapsed(),
            Some(Duration::from_millis(400)),
            "stopping during the delay reports the wait, not nothing"
        );
    }

    /// Clicks on the real pointer. Ignored so `cargo test` on a desktop does
    /// not move it; CI runs these under Xvfb.
    fn session_settings() -> Settings {
        let mut s = settings();
        s.timing = TimingMode::Fixed;
        s.interval_ms = 40;
        s.position = PositionMode::Fixed;
        s.point = (24, 24);
        s.jitter = JitterMode::Off;
        s.style = ClickStyle::Single;
        s
    }

    fn run_for(settings: &Settings, rx: &Receiver<Command>) -> Vec<SessionEvent> {
        let mut enigo = connect().expect("Xvfb should accept an X11 connection");
        let mut events = Vec::new();
        run_session(
            settings,
            rx,
            &mut enigo,
            &mut WindowShield::default(),
            &mut |event| events.push(event),
            &|| false,
        );
        events
    }

    #[test]
    #[ignore = "clicks; run under xvfb"]
    fn a_time_limit_shorter_than_the_start_delay_still_clicks() {
        let mut s = session_settings();
        // Longer than the limit on purpose: counting the delay against the
        // limit used to stop the run before the first click.
        s.start_delay_ms = 1_200;
        s.max_seconds = 1;
        s.max_clicks = 80;

        let (_tx, rx) = mpsc::channel();
        let start = Instant::now();
        let events = run_for(&s, &rx);
        let elapsed = start.elapsed();

        assert!(
            matches!(
                events.first(),
                Some(SessionEvent::Started { arm_until: Some(_) })
            ),
            "the window needs the delay deadline, got {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::Clicking)),
            "clicking never started: {events:?}"
        );
        let clicks = events
            .iter()
            .rev()
            .find_map(|event| match event {
                SessionEvent::Trigger(n) => Some(*n),
                _ => None,
            })
            .unwrap_or(0);
        assert!(
            clicks >= 8,
            "the delay consumed the time limit ({clicks} clicks) in {elapsed:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| { matches!(event, SessionEvent::Stopped(StopReason::TimeLimit)) }),
            "{events:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(2_000),
            "finished before the delay plus the time limit: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(4_000),
            "ran past the delay plus the time limit: {elapsed:?}"
        );
    }

    #[test]
    #[ignore = "clicks; run under xvfb"]
    fn a_stop_during_the_start_delay_clicks_nothing() {
        let mut s = session_settings();
        s.start_delay_ms = 3_000;
        s.max_seconds = 30;
        s.max_clicks = 20;

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            let _ = tx.send(Command::Stop);
        });

        let start = Instant::now();
        let events = run_for(&s, &rx);
        let elapsed = start.elapsed();

        assert!(
            events.iter().all(|event| {
                !matches!(event, SessionEvent::Clicking | SessionEvent::Trigger(_))
            }),
            "a stop during the delay must not click: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::Stopped(StopReason::User))),
            "{events:?}"
        );
        assert!(
            elapsed < Duration::from_millis(1_000),
            "stop waited out the delay: {elapsed:?}"
        );
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

    fn window_at(x: i32, y: i32) -> WindowShield {
        WindowShield {
            rect: Some(ScreenRect {
                x,
                y,
                width: 200,
                height: 200,
            }),
            pointer_over: false,
        }
    }

    #[test]
    fn screen_rect_contains_is_half_open_and_does_not_panic_at_the_extremes() {
        let rect = ScreenRect {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        };
        assert!(rect.contains(10, 20));
        assert!(rect.contains(39, 59));
        assert!(!rect.contains(40, 20));
        assert!(!rect.contains(10, 60));
        assert!(!rect.contains(9, 20));
        assert!(
            !ScreenRect {
                x: 0,
                y: 0,
                width: 0,
                height: 10
            }
            .contains(0, 0)
        );
        assert!(
            !ScreenRect {
                x: 0,
                y: 0,
                width: 10,
                height: -1
            }
            .contains(5, 0)
        );

        let edge = ScreenRect {
            x: i32::MAX - 5,
            y: i32::MIN,
            width: i32::MAX,
            height: i32::MAX,
        };
        assert!(edge.contains(i32::MAX - 5, i32::MIN));
        assert!(!edge.contains(i32::MAX, i32::MIN));
        let _ = ScreenRect {
            x: i32::MIN,
            y: i32::MIN,
            width: i32::MAX,
            height: i32::MAX,
        }
        .contains(0, 0);
    }

    #[test]
    fn cursor_mode_holds_until_the_pointer_leaves_the_window() {
        let mut s = settings();
        s.position = PositionMode::Cursor;
        s.jitter = JitterMode::Off;
        s.point = (150, 150);
        let shield = window_at(100, 100);

        let mut anchor = None;
        assert_eq!(
            click_plan(&s, &mut anchor, Some((150, 150)), &shield),
            ClickPlan::Hold
        );
        assert_eq!(anchor, None, "the Start button must not become the anchor");

        assert_eq!(
            click_plan(&s, &mut anchor, Some((10, 10)), &shield),
            ClickPlan::Fire((10, 10))
        );
        assert_eq!(anchor, Some((10, 10)));

        assert_eq!(
            click_plan(&s, &mut anchor, Some((150, 150)), &shield),
            ClickPlan::Hold
        );
        assert_eq!(anchor, Some((10, 10)));

        // A missing pointer read is not evidence the pointer is over the window,
        // even when the fallback point sits inside the rectangle.
        let mut anchor = None;
        assert_eq!(
            click_plan(&s, &mut anchor, None, &shield),
            ClickPlan::Fire((150, 150))
        );
        assert_eq!(anchor, Some((150, 150)));

        // Jitter still has to leave the window before the anchor is taken, so a
        // scatter cannot click Stop on the first triggers.
        s.jitter = JitterMode::Rectangle;
        s.jitter_x = 40;
        s.jitter_y = 40;
        let mut anchor = None;
        assert_eq!(
            click_plan(&s, &mut anchor, Some((150, 150)), &shield),
            ClickPlan::Hold
        );
        assert_eq!(anchor, None);
    }

    #[test]
    fn fixed_mode_holds_only_while_the_point_is_inside_the_window() {
        let mut s = settings();
        s.position = PositionMode::Fixed;
        s.jitter = JitterMode::Off;
        s.point = (150, 160);
        let shield = window_at(100, 100);

        let mut anchor = None;
        assert_eq!(
            click_plan(&s, &mut anchor, Some((10, 10)), &shield),
            ClickPlan::Hold
        );
        assert_eq!(anchor, Some((150, 160)));

        s.point = (10, 20);
        let mut anchor = None;
        assert_eq!(
            click_plan(&s, &mut anchor, Some((150, 150)), &shield),
            ClickPlan::Fire((10, 20))
        );
        assert_eq!(anchor, Some((10, 20)));
    }

    #[test]
    fn without_a_rectangle_only_cursor_mode_watches_the_hover() {
        let mut s = settings();
        s.position = PositionMode::Cursor;
        s.jitter = JitterMode::Off;
        s.point = (4, 5);
        let over = WindowShield {
            rect: None,
            pointer_over: true,
        };

        let mut anchor = None;
        assert_eq!(
            click_plan(&s, &mut anchor, Some((150, 150)), &WindowShield::default()),
            ClickPlan::Fire((150, 150))
        );

        let mut anchor = None;
        assert_eq!(
            click_plan(&s, &mut anchor, Some((150, 150)), &over),
            ClickPlan::Hold
        );
        assert_eq!(anchor, None);

        s.position = PositionMode::Fixed;
        s.point = (10, 20);
        let mut anchor = None;
        assert_eq!(
            click_plan(&s, &mut anchor, Some((150, 150)), &over),
            ClickPlan::Fire((10, 20))
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
            if inner.fail_location && inner.locations > 0 {
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

    impl Pointer for FakePointer {
        fn read_pointer(&self) -> Result<(i32, i32), String> {
            self.location().map_err(|err| err.to_string())
        }
        fn place(&mut self, x: i32, y: i32) -> Result<(), String> {
            self.move_mouse(x, y, Coordinate::Abs)
                .map_err(|err| err.to_string())
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
            &mut WindowShield::default(),
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
            // Read 1 plans the click, read 2 confirms the placed point.
            // The next sample, during the interval, sees the user leave.
            inner.jump_after = Some(2);
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
    #[test]
    fn each_connection_attempt_opens_at_most_one_server() {
        let wayland = backend_attempts(true);
        assert_eq!(wayland.len(), 2, "Wayland is tried before X11");
        assert!(
            wayland[0].wayland_display.is_none() && wayland[0].x11_display.is_some(),
            "the first attempt must not also open X11"
        );
        assert!(
            wayland[1].wayland_display.is_some() && wayland[1].x11_display.is_none(),
            "the X11 fallback must not also open Wayland"
        );

        let x11_only = backend_attempts(false);
        assert_eq!(x11_only.len(), 1);
        assert!(x11_only[0].wayland_display.is_some());
        assert!(x11_only[0].x11_display.is_none());

        for attempt in wayland.iter().chain(&x11_only) {
            assert!(
                attempt.wayland_display.is_some() || attempt.x11_display.is_some(),
                "an attempt that names neither server is how both end up connected"
            );
        }
    }

    #[test]
    fn anchor_uses_a_real_pointer_only_when_the_run_needs_one() {
        let mut s = settings();
        s.position = PositionMode::Cursor;
        s.jitter = JitterMode::Off;
        s.stop_on_move_px = 0;
        s.point = (9, 9);
        assert!(!anchor_needs_pointer(&s));
        assert!(!places_pointer(&s));
        assert_eq!(
            resolve_anchor(&s, Err("no pointer".into())).unwrap(),
            (9, 9),
            "cursor mode that does not move can run without a reading"
        );

        s.jitter = JitterMode::Rectangle;
        assert!(anchor_needs_pointer(&s));
        assert!(places_pointer(&s));
        let err = resolve_anchor(&s, Err("no pointer".into())).unwrap_err();
        assert!(err.contains("no pointer"), "{err}");
        assert_eq!(resolve_anchor(&s, Ok((3, 4))).unwrap(), (3, 4));

        s.jitter = JitterMode::Off;
        s.stop_on_move_px = 5;
        assert!(anchor_needs_pointer(&s));
        assert_eq!(resolve_anchor(&s, Ok((3, 4))).unwrap(), (3, 4));

        s.position = PositionMode::Fixed;
        s.point = (10, 20);
        s.stop_on_move_px = 0;
        assert!(!anchor_needs_pointer(&s));
        assert!(places_pointer(&s));
        assert_eq!(
            resolve_anchor(&s, Err("no pointer".into())).unwrap(),
            (10, 20)
        );

        s.stop_on_move_px = 8;
        assert!(anchor_needs_pointer(&s));
        let err = resolve_anchor(&s, Err("gone".into())).unwrap_err();
        assert!(err.contains("gone"), "{err}");
        assert_eq!(
            resolve_anchor(&s, Ok((1, 1))).unwrap(),
            (10, 20),
            "a fixed target stays the saved point once the pointer can be watched"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wayland_location_failure_names_the_x11_fallback() {
        let missing = explain_location("simulating input failed: (Not implemented yet)", None);
        assert!(missing.contains("Wayland does not report it"), "{missing}");
        assert!(missing.contains("no X11 display"), "{missing}");

        let failed = explain_location(
            "simulating input failed: (Not implemented yet)",
            Some("DISPLAY is not set"),
        );
        assert!(failed.contains("X11 failed"), "{failed}");
        assert!(failed.contains("DISPLAY is not set"), "{failed}");

        let other = explain_location("connection reset", Some("ignored"));
        assert_eq!(other, "cannot read the pointer position: connection reset");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "moves the pointer; run under xvfb"]
    fn screen_edge_clipping_is_not_treated_as_user_movement() {
        let mut pointer = connect().expect("Xvfb connection");
        let mut s = armed(5, 2);
        s.point = (-100, -100);
        s.style = ClickStyle::Hold;
        s.hold_ms = 1;
        s.stop_on_move_px = 1;
        let (_tx, rx) = channel();
        let mut events = Vec::new();
        run_session(
            &s,
            &rx,
            &mut pointer,
            &mut WindowShield::default(),
            &mut |event| events.push(event),
            &|| false,
        );
        assert_eq!(stop_reason(&events), StopReason::ClickLimit);
        assert_eq!(pointer.read_pointer().unwrap(), (0, 0));
    }

    /// Moves the pointer. Run under Xvfb, not on a desktop session:
    /// `xvfb-run -a cargo test -- --ignored pointer_placement`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "moves the pointer; run under xvfb"]
    fn pointer_placement_round_trips_on_x11() {
        let mut enigo = connect().expect("Xvfb should accept an X11 connection");
        let target = (80, 60);
        enigo.place(target.0, target.1).expect("X11 move");
        let seen = enigo.read_pointer().expect("X11 read");
        assert_eq!(
            seen, target,
            "the click connection did not land on the requested pixel"
        );
        let via_fallback = x11_pointer_fallback().expect("X11 fallback read");
        assert_eq!(
            via_fallback, target,
            "the position reader and the click connection disagree"
        );
    }
}
