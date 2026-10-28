//! Global hotkey registration.
//!
//! Windows, macOS, and X11 grabs must be created on the thread that owns the
//! platform event loop, so `Hotkeys` is built in `App::new`. A Wayland session
//! prefers the GlobalShortcuts portal, then reads keyboard devices. All backends
//! publish on one channel, which the notifier thread reads.

use std::str::FromStr;
use std::sync::mpsc::{Receiver, RecvError, Sender, channel};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

pub use global_hotkey::GlobalHotKeyEvent;
use global_hotkey::hotkey::{HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyManager, HotKeyState};

#[cfg(target_os = "linux")]
#[path = "evdev_keys.rs"]
mod evdev_keys;

#[cfg(target_os = "linux")]
#[path = "portal_hotkeys.rs"]
mod portal_hotkeys;

/// A press or release of a registered chord, from whichever backend is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyEvent {
    pub id: u32,
    pub pressed: bool,
}

// `Receiver` is `Send` but not `Sync`, so it cannot sit in a `OnceLock` by
// itself. The notifier is the only reader and holds the mutex across `recv`.
// Publishers only touch the sender.
static TX: OnceLock<Sender<HotkeyEvent>> = OnceLock::new();
static RX: OnceLock<Mutex<Receiver<HotkeyEvent>>> = OnceLock::new();

fn ensure_bus() {
    let _ = TX.get_or_init(|| {
        let (tx, rx) = channel();
        let _ = RX.set(Mutex::new(rx));
        tx
    });
}

/// Blocks until the next chord edge. The notifier thread is the only caller.
pub fn recv_event() -> Result<HotkeyEvent, RecvError> {
    ensure_bus();
    RX.get()
        .expect("hotkey receiver")
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .recv()
}

fn publish(id: u32, pressed: bool) {
    ensure_bus();
    let _ = TX
        .get()
        .expect("hotkey sender")
        .send(HotkeyEvent { id, pressed });
}

fn ensure_forwarder() {
    static START: Once = Once::new();
    START.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("featherclick-grab".to_string())
            .spawn(|| {
                let incoming = GlobalHotKeyEvent::receiver();
                while let Ok(event) = incoming.recv() {
                    publish(event.id, is_press(&event));
                }
            });
    });
}

/// What a registered hotkey does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Toggle,
    Capture,
}

/// Parses a config string ("F6", "Ctrl+Shift+K"). Returns `None` for an empty
/// or malformed binding rather than failing the whole config.
pub fn parse(text: &str) -> Option<HotKey> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    HotKey::from_str(trimmed).ok()
}

/// True when the binding text is present but cannot be parsed, which is worth
/// telling the user about.
pub fn invalid(text: &str) -> bool {
    !text.trim().is_empty() && parse(text).is_none()
}

enum Backend {
    Grab(GlobalHotKeyManager),
    /// Wayland sessions. An X11 grab does not see keys typed into native
    /// windows, even when XWayland has set `DISPLAY`.
    #[cfg(target_os = "linux")]
    Devices(evdev_keys::Listener),
    #[cfg(target_os = "linux")]
    Portal(portal_hotkeys::Listener),
}

// XWayland grabs cannot see native Wayland windows. Only X11 sessions use grabs.
#[cfg(target_os = "linux")]
fn wayland_backend<T>(
    portal: impl FnOnce() -> Result<T, String>,
    devices: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    portal().or_else(|_| devices())
}

pub struct Hotkeys {
    backend: Backend,
    toggle: Option<HotKey>,
    capture: Option<HotKey>,
}

impl Hotkeys {
    pub fn new() -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        if evdev_keys::session_is_wayland(
            std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
            std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        ) {
            let backend = wayland_backend(
                || portal_hotkeys::Listener::open().map(Backend::Portal),
                || evdev_keys::Listener::start(publish).map(Backend::Devices),
            )?;
            return Ok(Self {
                backend,
                toggle: None,
                capture: None,
            });
        }
        #[cfg(target_os = "linux")]
        ensure_x11_hotkeys()?;
        ensure_forwarder();
        GlobalHotKeyManager::new()
            .map(|manager| Self {
                backend: Backend::Grab(manager),
                toggle: None,
                capture: None,
            })
            .map_err(|err| format!("global hotkeys unavailable: {err}"))
    }

    /// Replaces every binding. Returns a message when one could not be
    /// registered, which in practice means another application owns it.
    pub fn apply(&mut self, toggle: Option<HotKey>, capture: Option<HotKey>) -> Option<String> {
        let (toggle, capture, shared) = separate_duplicate(toggle, capture);
        #[cfg(target_os = "linux")]
        if let Backend::Portal(listener) = &mut self.backend {
            if self.toggle == toggle && self.capture == capture {
                return shared;
            }
            match listener.set(toggle, capture) {
                Ok(()) => {
                    self.toggle = toggle;
                    self.capture = capture;
                    return shared;
                }
                Err(portal_error) => match evdev_keys::Listener::start(publish) {
                    Ok(listener) => self.backend = Backend::Devices(listener),
                    Err(device_error) => {
                        self.toggle = None;
                        self.capture = None;
                        return Some(format!("{portal_error}; {device_error}"));
                    }
                },
            }
        }
        let mut problem = match &mut self.backend {
            Backend::Grab(manager) => {
                for hotkey in [self.toggle, self.capture].into_iter().flatten() {
                    let _ = manager.unregister(hotkey);
                }
                self.toggle = None;
                self.capture = None;

                let mut problem = None;
                for (action, hotkey) in [(Action::Toggle, toggle), (Action::Capture, capture)] {
                    let Some(hotkey) = hotkey else { continue };
                    match manager.register(hotkey) {
                        Ok(()) => match action {
                            Action::Toggle => self.toggle = Some(hotkey),
                            Action::Capture => self.capture = Some(hotkey),
                        },
                        Err(err) => {
                            problem.get_or_insert_with(|| format!("{hotkey} unavailable: {err}"));
                        }
                    }
                }
                problem
            }
            #[cfg(target_os = "linux")]
            Backend::Devices(listener) => listener.set(toggle, capture),
            #[cfg(target_os = "linux")]
            Backend::Portal(_) => unreachable!("portal handled above"),
        };
        if problem.is_none() {
            problem = shared;
        }
        problem
    }
}

/// One key cannot be both actions: the backend rejects the second register
/// and would only say so as a debug dump. Start/stop keeps the key.
pub fn separate_duplicate(
    toggle: Option<HotKey>,
    capture: Option<HotKey>,
) -> (Option<HotKey>, Option<HotKey>, Option<String>) {
    match (toggle, capture) {
        (Some(toggle), Some(capture)) if toggle.id() == capture.id() => {
            let name = pretty(&toggle.to_string());
            (
                Some(toggle),
                None,
                Some(format!(
                    "{name} is used by both start / stop and pick position"
                )),
            )
        }
        pair => (pair.0, pair.1, None),
    }
}

/// `global-hotkey` on Linux is an X11 grab. `DISPLAY` of `:N` or `unix:N`
/// listens on this socket; a TCP display has no socket here and is left for
/// the backend to try.
#[cfg(target_os = "linux")]
fn x11_socket_path(display: &str) -> Option<std::path::PathBuf> {
    let display = display.trim();
    if display.is_empty() {
        return None;
    }
    let rest = display.strip_prefix("unix:").unwrap_or(display);
    let number = if let Some((host, number)) = rest.rsplit_once(':') {
        if !host.is_empty() {
            return None;
        }
        number
    } else {
        rest
    };
    let screen = number.split('.').next().unwrap_or("");
    if screen.is_empty() || !screen.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(std::path::PathBuf::from(format!(
        "/tmp/.X11-unix/X{screen}"
    )))
}

#[cfg(target_os = "linux")]
fn ensure_x11_hotkeys() -> Result<(), String> {
    let display = std::env::var("DISPLAY").unwrap_or_default();
    x11_hotkeys_from(&display, |path| {
        std::os::unix::net::UnixStream::connect(path).is_ok()
    })
}

/// `open` reports whether the local socket accepts a connection. A TCP
/// `DISPLAY` has no socket in `/tmp/.X11-unix` and is left for the backend.
#[cfg(target_os = "linux")]
fn x11_hotkeys_from(display: &str, open: impl Fn(&std::path::Path) -> bool) -> Result<(), String> {
    if display.trim().is_empty() {
        return Err("global hotkeys need an X11 display".to_string());
    }
    let Some(path) = x11_socket_path(display) else {
        return Ok(());
    };
    if open(&path) {
        Ok(())
    } else {
        Err("global hotkeys need an X11 display".to_string())
    }
}

/// Renders a stored binding for display: the backends round-trip a canonical
/// form like `shift+control+KeyK`, which is accurate but not what anyone wants
/// to read in the UI.
pub fn pretty(binding: &str) -> String {
    let trimmed = binding.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed
        .split('+')
        .map(|part| match part.trim() {
            "shift" => "Shift",
            "control" => "Ctrl",
            "alt" => "Alt",
            "super" => "Super",
            "meta" => "Meta",
            other => other
                .strip_prefix("Key")
                .or_else(|| other.strip_prefix("Digit"))
                .unwrap_or(other),
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// `true` for the press transition only, so a held key fires once.
pub fn is_press(event: &GlobalHotKeyEvent) -> bool {
    event.state == HotKeyState::Pressed
}

/// Filters key events down to genuine presses.
///
/// X11 auto-repeat is the reason this exists: the server emits a *pair* of
/// events (KeyRelease immediately followed by KeyPress) on every repeat tick,
/// so a held hotkey would otherwise re-fire forever and a held toggle would
/// flap. Windows and macOS send one press. Each binding is tracked on its
/// own, because a release of the other key can land between the two halves
/// of that pair and must not turn the repeat press into a new tap.
pub struct Gate {
    /// Bindings whose press was accepted and whose release has not arrived.
    held: Vec<u32>,
    /// Latest release of each binding, so a repeat pair is still recognised
    /// when another key's events land between the two halves.
    released: Vec<(u32, Instant)>,
}

/// A release/press pair closer together than this is auto-repeat, not a tap.
/// X11 emits the pair back to back on each repeat tick; a human double-tap
/// leaves far more than this between release and press.
const REPEAT_PAIR_GAP: Duration = Duration::from_millis(25);

impl Gate {
    pub fn new() -> Self {
        Self {
            held: Vec::new(),
            released: Vec::new(),
        }
    }

    /// Feeds one event in; returns `true` when it should trigger its action.
    pub fn accept(&mut self, id: u32, pressed: bool, now: Instant) -> bool {
        self.released
            .retain(|(_, at)| now.saturating_duration_since(*at) < REPEAT_PAIR_GAP);
        if !pressed {
            self.held.retain(|held| *held != id);
            self.note_release(id, now);
            return false;
        }
        let is_repeat = self.released.iter().any(|(released_id, at)| {
            *released_id == id && now.saturating_duration_since(*at) < REPEAT_PAIR_GAP
        });
        if self.held.contains(&id) || is_repeat {
            self.hold(id);
            return false;
        }
        self.hold(id);
        true
    }

    fn hold(&mut self, id: u32) {
        if !self.held.contains(&id) {
            self.held.push(id);
        }
    }

    fn note_release(&mut self, id: u32, now: Instant) {
        if let Some(slot) = self.released.iter_mut().find(|(seen, _)| *seen == id) {
            slot.1 = now;
        } else {
            self.released.push((id, now));
        }
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

/// True when `id` is the binding stored in `text`, resolved from the config
/// string so the notifier thread needs no shared manager state.
pub fn matches(id: u32, text: &str) -> bool {
    parse(text).is_some_and(|hotkey| hotkey.id() == id)
}

pub use global_hotkey::hotkey::Code;

/// Builds a binding from a captured key combination.
pub fn binding(code: Code, ctrl: bool, shift: bool, alt: bool, sup: bool) -> HotKey {
    HotKey::new(modifiers(ctrl, shift, alt, sup), code)
}

/// Modifier set for a captured key combination.
pub fn modifiers(ctrl: bool, shift: bool, alt: bool, sup: bool) -> Option<Modifiers> {
    let mut mods = Modifiers::empty();
    mods.set(Modifiers::CONTROL, ctrl);
    mods.set(Modifiers::SHIFT, shift);
    mods.set(Modifiers::ALT, alt);
    mods.set(Modifiers::SUPER, sup);
    (!mods.is_empty()).then_some(mods)
}

/// Modifiers held when a capture event arrived.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeldMods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
}

impl HeldMods {
    fn any(self) -> bool {
        self.ctrl || self.shift || self.alt || self.super_key
    }

    fn binding(self, code: Code) -> String {
        binding(code, self.ctrl, self.shift, self.alt, self.super_key).to_string()
    }
}

/// One physical side of a modifier key. The two sides are tracked apart so
/// releasing one does not look like both are up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    CtrlL,
    CtrlR,
    ShiftL,
    ShiftR,
    AltL,
    AltR,
    SuperL,
    SuperR,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sides {
    pub ctrl_l: bool,
    pub ctrl_r: bool,
    pub shift_l: bool,
    pub shift_r: bool,
    pub alt_l: bool,
    pub alt_r: bool,
    pub super_l: bool,
    pub super_r: bool,
}

impl Sides {
    fn set(&mut self, side: Side, down: bool) {
        match side {
            Side::CtrlL => self.ctrl_l = down,
            Side::CtrlR => self.ctrl_r = down,
            Side::ShiftL => self.shift_l = down,
            Side::ShiftR => self.shift_r = down,
            Side::AltL => self.alt_l = down,
            Side::AltR => self.alt_r = down,
            Side::SuperL => self.super_l = down,
            Side::SuperR => self.super_r = down,
        }
    }

    fn held_mods(self) -> HeldMods {
        HeldMods {
            ctrl: self.ctrl_l || self.ctrl_r,
            shift: self.shift_l || self.shift_r,
            alt: self.alt_l || self.alt_r,
            super_key: self.super_l || self.super_r,
        }
    }
}

/// What the capture UI should do with one decided press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureResult {
    /// Bare Escape: keep the previous binding.
    Cancel,
    /// Store this canonical hotkey string.
    Binding(String),
    /// Modifier-only, auto-repeat, or a key the backends cannot bind.
    Ignore,
}

/// Clipboard shortcuts egui delivers instead of the key that produced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clipboard {
    Copy,
    Cut,
    Paste,
}

/// One input event, reduced to what capture needs. The UI translates egui.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawEvent {
    /// A modifier side went down or up.
    Side {
        side: Side,
        pressed: bool,
        repeat: bool,
    },
    /// A non-modifier key. `mods` is that event's own modifier snapshot.
    /// Super is not in it on Windows or Linux, so replay fills it in.
    Key {
        code: Option<Code>,
        escape: bool,
        pressed: bool,
        repeat: bool,
        mods: HeldMods,
    },
    Clipboard(Clipboard),
}

/// State of the modifier sides before `events` were applied.
pub fn sides_at_frame_start(end: Sides, events: &[RawEvent]) -> Sides {
    let mut sides = end;
    for event in events.iter().rev() {
        if let RawEvent::Side {
            side,
            pressed,
            repeat: false,
        } = *event
        {
            sides.set(side, !pressed);
        }
    }
    sides
}

/// Walks one frame and returns the first capture decision.
///
/// Super is taken from the side tracker, because a Win/Super hold is not
/// recorded on the key event off macOS — storing the bare key would grab it
/// globally. Clipboard events are the same press as Ctrl/Cmd+C, X, or V;
/// egui never sends that key, and the Ctrl key may already be up again by
/// the end of the frame, so the decision uses the sides at the event.
pub fn replay_capture(mut sides: Sides, events: &[RawEvent]) -> CaptureResult {
    for event in events {
        match *event {
            RawEvent::Side {
                side,
                pressed,
                repeat,
            } => {
                if !repeat {
                    sides.set(side, pressed);
                }
            }
            RawEvent::Key {
                code,
                escape,
                pressed,
                repeat,
                mods,
            } => {
                if !pressed || repeat {
                    continue;
                }
                let mut mods = mods;
                mods.super_key = mods.super_key || sides.held_mods().super_key;
                let decision = decide_key(code, escape, mods);
                if !matches!(decision, CaptureResult::Ignore) {
                    return decision;
                }
            }
            RawEvent::Clipboard(which) => {
                let decision = decide_clipboard(which, sides.held_mods());
                if !matches!(decision, CaptureResult::Ignore) {
                    return decision;
                }
            }
        }
    }
    CaptureResult::Ignore
}

pub fn decide_key(code: Option<Code>, escape: bool, mods: HeldMods) -> CaptureResult {
    if escape && !mods.any() {
        return CaptureResult::Cancel;
    }
    let Some(code) = code else {
        return CaptureResult::Ignore;
    };
    CaptureResult::Binding(mods.binding(code))
}

pub fn decide_clipboard(which: Clipboard, mods: HeldMods) -> CaptureResult {
    // A dedicated Copy key arrives with no Ctrl or Super. Binding it as C
    // would grab the letter. Those keys are not something the backends spell.
    if !mods.ctrl && !mods.super_key {
        return CaptureResult::Ignore;
    }
    let code = match which {
        Clipboard::Copy => Code::KeyC,
        Clipboard::Cut => Code::KeyX,
        Clipboard::Paste => Code::KeyV,
    };
    CaptureResult::Binding(mods.binding(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_strings_round_trip_through_the_hotkey_type() {
        for text in ["F6", "F7", "Ctrl+Shift+K", "Alt+Digit1"] {
            let parsed = parse(text).unwrap_or_else(|| panic!("{text} should parse"));
            let printed = parsed.to_string();
            assert_eq!(
                parse(&printed).map(|hotkey| hotkey.id()),
                Some(parsed.id()),
                "{text} printed as {printed} and did not survive a round trip"
            );
        }
    }

    #[test]
    fn empty_and_malformed_bindings_are_optional_not_fatal() {
        assert!(parse("").is_none());
        assert!(parse("   ").is_none());
        assert!(parse("NotAKey").is_none());
        assert!(!invalid(""), "clearing a binding is allowed");
        assert!(!invalid("F6"));
        assert!(invalid("NotAKey"));
        assert!(!invalid("  "), "whitespace is treated as cleared");
    }

    #[test]
    fn stored_bindings_display_readably() {
        assert_eq!(pretty("F6"), "F6");
        assert_eq!(pretty("shift+control+KeyK"), "Shift+Ctrl+K");
        assert_eq!(pretty("alt+Digit1"), "Alt+1");
        assert_eq!(pretty("super+Numpad0"), "Super+Numpad0");
        assert_eq!(pretty(""), "");
        // whatever the UI prints must still parse back into a working binding
        let stored = "shift+control+KeyK";
        assert_eq!(
            parse(&pretty(stored)).map(|hotkey| hotkey.id()),
            parse(stored).map(|h| h.id())
        );
    }

    #[test]
    fn ids_identify_the_right_binding() {
        let toggle = parse("F8").unwrap();
        let capture = parse("F9").unwrap();
        assert_ne!(toggle.id(), capture.id());
        assert_eq!(parse("F8").unwrap().id(), toggle.id());
    }

    #[test]
    fn gate_passes_distinct_taps_and_ignores_releases() {
        let mut gate = Gate::new();
        let start = Instant::now();
        // A tap is press-then-release; releases alone must never trigger.
        let tap = |gate: &mut Gate, at: Duration| -> bool {
            let triggered = gate.accept(7, true, start + at);
            assert!(
                !gate.accept(7, false, start + at + Duration::from_millis(5)),
                "releases never trigger"
            );
            triggered
        };
        assert!(
            tap(&mut gate, Duration::ZERO),
            "the first press must trigger"
        );
        assert!(
            tap(&mut gate, Duration::from_millis(400)),
            "a second tap must trigger"
        );
        assert!(
            tap(&mut gate, Duration::from_millis(800)),
            "a third tap must trigger"
        );
    }

    #[test]
    fn gate_fires_once_while_a_key_is_held_on_x11() {
        let mut gate = Gate::new();
        let start = Instant::now();
        assert!(
            gate.accept(3, true, start),
            "the initial press must trigger"
        );

        // X11 auto-repeat: after the repeat delay, a Release/Press pair every 33 ms.
        let mut triggers = 0;
        for tick in 0..30 {
            let at = start + Duration::from_millis(500) + Duration::from_millis(33 * tick);
            gate.accept(3, false, at);
            if gate.accept(3, true, at + Duration::from_millis(1)) {
                triggers += 1;
            }
        }
        assert_eq!(triggers, 0, "holding the key must not retrigger");

        // ...and once really released, the next tap works again.
        assert!(!gate.accept(3, false, start + Duration::from_millis(1600)));
        assert!(gate.accept(3, true, start + Duration::from_millis(2000)));
    }

    #[test]
    fn gate_does_not_rearm_a_held_key_when_another_key_is_tapped() {
        let mut gate = Gate::new();
        let start = Instant::now();
        assert!(gate.accept(7, true, start), "start/stop must trigger");
        // Further presses with no release: a repeat that is not an X11 pair.
        assert!(!gate.accept(7, true, start + Duration::from_millis(500)));

        // Tap pick-position while start/stop is still held.
        assert!(gate.accept(8, true, start + Duration::from_millis(600)));
        assert!(!gate.accept(8, false, start + Duration::from_millis(650)));

        assert!(
            !gate.accept(7, true, start + Duration::from_millis(700)),
            "the held toggle must not fire again after the other key's release"
        );
        assert!(!gate.accept(7, true, start + Duration::from_millis(733)));

        assert!(!gate.accept(7, false, start + Duration::from_millis(900)));
        assert!(
            gate.accept(7, true, start + Duration::from_millis(1200)),
            "a later tap must trigger"
        );
    }

    #[test]
    fn gate_keeps_an_x11_repeat_pair_when_another_release_lands_between() {
        let mut gate = Gate::new();
        let start = Instant::now();
        assert!(gate.accept(7, true, start));
        assert!(gate.accept(8, true, start + Duration::from_millis(100)));

        // Release of 7, then a release of 8, then the repeat press of 7.
        // The pair is still a repeat of 7 even though 8 was the latest release.
        let at = start + Duration::from_millis(200);
        assert!(!gate.accept(7, false, at));
        assert!(!gate.accept(8, false, at + Duration::from_millis(1)));
        assert!(
            !gate.accept(7, true, at + Duration::from_millis(2)),
            "the repeat press of the held key must stay suppressed"
        );
    }

    #[test]
    fn gate_keeps_single_press_platforms_working() {
        // Windows and macOS send one press and a late synthetic release.
        let mut gate = Gate::new();
        let start = Instant::now();
        assert!(gate.accept(5, true, start));
        assert!(!gate.accept(5, false, start + Duration::from_millis(50)));
        assert!(gate.accept(5, true, start + Duration::from_millis(600)));
    }

    #[test]
    fn published_edges_reach_the_single_receiver() {
        // The only test that reads the process-wide channel.
        publish(42, true);
        publish(42, false);
        assert_eq!(
            recv_event().unwrap(),
            HotkeyEvent {
                id: 42,
                pressed: true
            }
        );
        assert_eq!(
            recv_event().unwrap(),
            HotkeyEvent {
                id: 42,
                pressed: false
            }
        );
    }

    fn mods_of(text: &str) -> (Modifiers, Code) {
        let hotkey = parse(text).unwrap_or_else(|| panic!("{text} should parse"));
        (hotkey.mods, hotkey.key)
    }

    #[test]
    fn super_is_part_of_the_binding_even_when_the_key_event_omits_it() {
        // Win/Super is down before the letter, and the letter's own snapshot
        // does not mention it. That is what Windows and Linux report.
        let end = Sides {
            super_l: true,
            ..Sides::default()
        };
        let events = [
            RawEvent::Side {
                side: Side::SuperL,
                pressed: true,
                repeat: false,
            },
            RawEvent::Key {
                code: Some(Code::KeyK),
                escape: false,
                pressed: true,
                repeat: false,
                mods: HeldMods::default(),
            },
        ];
        let decision = replay_capture(sides_at_frame_start(end, &events), &events);
        let CaptureResult::Binding(text) = decision else {
            panic!("expected a binding, got {decision:?}");
        };
        assert_eq!(pretty(&text), "Super+K");
        let (mods, key) = mods_of(&text);
        assert_eq!(key, Code::KeyK);
        assert!(mods.contains(Modifiers::SUPER));
        assert!(!mods.contains(Modifiers::CONTROL));
    }

    #[test]
    fn a_key_pressed_before_super_does_not_pick_the_modifier_up() {
        let end = Sides {
            super_l: true,
            ..Sides::default()
        };
        let events = [
            RawEvent::Key {
                code: Some(Code::KeyK),
                escape: false,
                pressed: true,
                repeat: false,
                mods: HeldMods::default(),
            },
            RawEvent::Side {
                side: Side::SuperL,
                pressed: true,
                repeat: false,
            },
        ];
        let decision = replay_capture(sides_at_frame_start(end, &events), &events);
        let CaptureResult::Binding(text) = decision else {
            panic!("expected a binding, got {decision:?}");
        };
        let (mods, key) = mods_of(&text);
        assert_eq!(key, Code::KeyK);
        assert!(mods.is_empty(), "super went down after the letter: {text}");
    }

    #[test]
    fn ctrl_c_is_captured_from_the_clipboard_event_even_if_ctrl_is_already_up() {
        // A quick tap can press Ctrl, deliver Copy, and release Ctrl in one frame.
        let end = Sides::default();
        let events = [
            RawEvent::Side {
                side: Side::CtrlL,
                pressed: true,
                repeat: false,
            },
            RawEvent::Clipboard(Clipboard::Copy),
            RawEvent::Side {
                side: Side::CtrlL,
                pressed: false,
                repeat: false,
            },
        ];
        let decision = replay_capture(sides_at_frame_start(end, &events), &events);
        let CaptureResult::Binding(text) = decision else {
            panic!("expected a binding, got {decision:?}");
        };
        assert_eq!(pretty(&text), "Ctrl+C");
        let (mods, key) = mods_of(&text);
        assert_eq!(key, Code::KeyC);
        assert!(mods.contains(Modifiers::CONTROL));
    }

    #[test]
    fn shift_stays_on_a_clipboard_chord_and_a_bare_copy_key_is_not_the_letter_c() {
        let end = Sides {
            ctrl_l: true,
            shift_l: true,
            ..Sides::default()
        };
        let decision = replay_capture(end, &[RawEvent::Clipboard(Clipboard::Cut)]);
        let CaptureResult::Binding(text) = decision else {
            panic!("expected a binding, got {decision:?}");
        };
        assert_eq!(pretty(&text), "Shift+Ctrl+X");
        let (mods, key) = mods_of(&text);
        assert_eq!(key, Code::KeyX);
        assert!(mods.contains(Modifiers::SHIFT));
        assert!(mods.contains(Modifiers::CONTROL));

        assert_eq!(
            decide_clipboard(Clipboard::Copy, HeldMods::default()),
            CaptureResult::Ignore,
            "a modifier-less Copy event must not grab the letter C"
        );
        assert_eq!(
            decide_clipboard(
                Clipboard::Paste,
                HeldMods {
                    super_key: true,
                    ..HeldMods::default()
                }
            ),
            CaptureResult::Binding(binding(Code::KeyV, false, false, false, true).to_string())
        );
    }

    #[test]
    fn one_key_cannot_be_both_actions() {
        let key = parse("F6").unwrap();
        let (toggle, capture, problem) = separate_duplicate(Some(key), Some(key));
        assert_eq!(toggle.map(|hotkey| hotkey.id()), Some(key.id()));
        assert!(capture.is_none());
        assert_eq!(
            problem.as_deref(),
            Some("F6 is used by both start / stop and pick position")
        );

        let other = parse("Ctrl+Shift+K").unwrap();
        let (toggle, capture, problem) = separate_duplicate(Some(key), Some(other));
        assert!(toggle.is_some() && capture.is_some() && problem.is_none());
        assert_eq!(separate_duplicate(None, Some(key)), (None, Some(key), None));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn x11_socket_path_reads_a_local_display_number() {
        assert_eq!(
            x11_socket_path(":0").unwrap(),
            std::path::PathBuf::from("/tmp/.X11-unix/X0")
        );
        assert_eq!(
            x11_socket_path(":12.0").unwrap(),
            std::path::PathBuf::from("/tmp/.X11-unix/X12")
        );
        assert_eq!(
            x11_socket_path("unix:1").unwrap(),
            std::path::PathBuf::from("/tmp/.X11-unix/X1")
        );
        assert_eq!(
            x11_socket_path("  unix:3.0 ").unwrap(),
            std::path::PathBuf::from("/tmp/.X11-unix/X3")
        );
        assert!(x11_socket_path("").is_none());
        assert!(x11_socket_path("localhost:0").is_none());
        assert!(x11_socket_path("localhost:0.0").is_none());
        assert!(x11_socket_path(":").is_none());
        assert!(x11_socket_path(":abc").is_none());

        let err = x11_hotkeys_from("", |_| true).unwrap_err();
        assert_eq!(err, "global hotkeys need an X11 display");
        assert!(x11_hotkeys_from(":9", |_| false).is_err());
        assert!(x11_hotkeys_from(":9", |_| true).is_ok());
        // TCP displays are not probed through the unix socket.
        assert!(x11_hotkeys_from("localhost:0", |_| false).is_ok());
    }

    #[test]
    fn bare_escape_cancels_and_a_modified_escape_is_a_binding() {
        assert_eq!(
            decide_key(Some(Code::Escape), true, HeldMods::default()),
            CaptureResult::Cancel
        );
        let decision = decide_key(
            Some(Code::Escape),
            true,
            HeldMods {
                ctrl: true,
                ..HeldMods::default()
            },
        );
        let CaptureResult::Binding(text) = decision else {
            panic!("expected a binding, got {decision:?}");
        };
        let (mods, key) = mods_of(&text);
        assert_eq!(key, Code::Escape);
        assert!(mods.contains(Modifiers::CONTROL));
        assert_eq!(
            decide_key(None, false, HeldMods::default()),
            CaptureResult::Ignore
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod backend_tests {
    use super::*;
    #[test]
    fn portal_is_preferred_and_devices_are_only_a_fallback() {
        assert_eq!(
            wayland_backend(|| Ok("portal"), || panic!("must not open devices")),
            Ok("portal")
        );
        assert_eq!(
            wayland_backend(|| Err("refused".into()), || Ok("devices")),
            Ok("devices")
        );
        assert_eq!(
            wayland_backend::<()>(
                || Err("missing".into()),
                || Err("no keyboard access".into())
            ),
            Err("no keyboard access".into())
        );
    }
}
