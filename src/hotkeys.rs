//! Global hotkey registration.
//!
//! The manager must live on the thread that owns the platform event loop
//! (required by the Windows and macOS backends), so it is created in
//! `App::new`, which eframe runs on the main thread. Events themselves are
//! delivered on a process-wide channel that any thread may read.

use std::str::FromStr;
use std::time::{Duration, Instant};

pub use global_hotkey::GlobalHotKeyEvent;
use global_hotkey::hotkey::{HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyManager, HotKeyState};

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

pub struct Hotkeys {
    manager: GlobalHotKeyManager,
    toggle: Option<HotKey>,
    capture: Option<HotKey>,
}

impl Hotkeys {
    pub fn new() -> Result<Self, String> {
        GlobalHotKeyManager::new()
            .map(|manager| Self {
                manager,
                toggle: None,
                capture: None,
            })
            .map_err(|err| format!("global hotkeys unavailable: {err}"))
    }

    /// Replaces every binding. Returns a message when one could not be
    /// registered, which in practice means another application owns it.
    pub fn apply(&mut self, toggle: Option<HotKey>, capture: Option<HotKey>) -> Option<String> {
        for hotkey in [self.toggle, self.capture].into_iter().flatten() {
            let _ = self.manager.unregister(hotkey);
        }
        self.toggle = None;
        self.capture = None;

        let mut problem = None;
        for (action, hotkey) in [(Action::Toggle, toggle), (Action::Capture, capture)] {
            let Some(hotkey) = hotkey else { continue };
            match self.manager.register(hotkey) {
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

/// True when `event` is the toggle binding of `settings`, resolved from the
/// config text so the notifier thread needs no shared manager state.
pub fn matches(event: &GlobalHotKeyEvent, text: &str) -> bool {
    parse(text).is_some_and(|hotkey| hotkey.id() == event.id)
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
}
