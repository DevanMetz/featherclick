//! Global hotkey registration.
//!
//! Windows, macOS, and X11 grabs must be created on the thread that owns the
//! platform event loop, so `Hotkeys` is built in `App::new`. A Wayland session
//! reads the keyboard device instead. Both publish on one channel, which the
//! notifier thread reads.

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
            // Do not fall back to the X11 grab: it registers successfully and
            // then never fires, which looks like a working hotkey.
            let listener = evdev_keys::Listener::start(publish)?;
            return Ok(Self {
                backend: Backend::Devices(listener),
                toggle: None,
                capture: None,
            });
        }
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
        match &mut self.backend {
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
        }
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
/// flap. Windows and macOS send a single press, so the same gate covers both
/// without a timer.
pub struct Gate {
    /// Binding whose press was accepted and whose release has not arrived yet.
    held: Option<u32>,
    /// Last release seen, used to recognise the repeat signature.
    released: Option<(u32, Instant)>,
}

/// A release/press pair closer together than this is auto-repeat, not a tap.
/// X11 emits the pair back to back on each repeat tick; a human double-tap
/// leaves far more than this between release and press.
const REPEAT_PAIR_GAP: Duration = Duration::from_millis(25);

impl Gate {
    pub fn new() -> Self {
        Self {
            held: None,
            released: None,
        }
    }

    /// Feeds one event in; returns `true` when it should trigger its action.
    pub fn accept(&mut self, id: u32, pressed: bool, now: Instant) -> bool {
        if !pressed {
            self.held = None;
            self.released = Some((id, now));
            return false;
        }
        let is_repeat = self.released.is_some_and(|(released_id, at)| {
            released_id == id && now.duration_since(at) < REPEAT_PAIR_GAP
        });
        if self.held == Some(id) || is_repeat {
            // Track repeats as held so the whole burst stays suppressed.
            self.held = Some(id);
            return false;
        }
        self.held = Some(id);
        true
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
}
